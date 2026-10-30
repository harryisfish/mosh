use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, CString};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use mosh_rs::display::{Display, Graphics, PredictionPolicy};
use mosh_rs::model::*;
use mosh_rs::session::SessionActor;
use mosh_rs::transport::{self, ClientEndpoints, ClientIdentity, Incoming, ServerListeners};
use quinn::{Connection, RecvStream, SendStream, VarInt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const BULK_BYTES: usize = 120_000;
const WAIT: Duration = Duration::from_secs(12);
const CLIPBOARD_REPLY: &[u8] = b"\x1b]52;c;bG9jYWwtY2xpcGJvYXJk\x1b\\";

enum Wire {
    Control(ServerMessage),
    Display(Incoming),
    Lost(String),
}

struct Peer {
    connection: Connection,
    send: SendStream,
    receive: mpsc::Receiver<Wire>,
    notify: mpsc::Sender<Wire>,
    readers: Vec<JoinHandle<()>>,
    epoch: u64,
}

impl Peer {
    async fn attach(
        endpoint: &ClientEndpoints,
        grant: &Grant,
        view: &mut View,
        attempt: u64,
        reported_effects: Option<u64>,
        uni_readers: usize,
    ) -> Result<Self> {
        let connection = transport::connect(endpoint, grant).await?;
        let (mut send, mut receive) = connection.open_bi().await?;
        transport::send_message(
            &mut send,
            &ClientMessage::Attach(Attach {
                version: PROTOCOL_VERSION,
                session: grant.session.clone(),
                attempt,
                committed: view.committed,
                rendered: view.display.revision(),
                effects: reported_effects.unwrap_or(view.display.effects_applied()),
            }),
        )
        .await?;
        let first =
            tokio::time::timeout(WAIT, transport::recv_message::<ServerMessage>(&mut receive))
                .await??
                .context("attachment ended before reply")?;
        let ServerMessage::Attached {
            epoch,
            committed,
            effect_floor,
        } = first
        else {
            connection.close(0_u32.into(), b"rejected fixture attachment");
            bail!("attachment rejected: {first:?}");
        };
        ensure!(
            committed >= view.committed && committed <= view.sequence,
            "invalid attachment commit"
        );
        view.committed = committed;
        view.display.set_epoch(epoch)?;
        view.display.set_effect_floor(effect_floor);
        view.pending = None;
        view.resources.clear();
        let (notify, input) = mpsc::channel(64);
        let sender = notify.clone();
        let reader = tokio::spawn(async move {
            loop {
                match transport::recv_message(&mut receive).await {
                    Ok(Some(message)) => {
                        if sender.send(Wire::Control(message)).await.is_err() {
                            return;
                        }
                    }
                    result => {
                        let _ = sender
                            .send(Wire::Lost(format!("control ended: {result:?}")))
                            .await;
                        return;
                    }
                }
            }
        });
        let mut peer = Self {
            connection,
            send,
            receive: input,
            notify,
            readers: vec![reader],
            epoch,
        };
        peer.start_uni_readers(uni_readers);
        Ok(peer)
    }

    fn start_uni_readers(&mut self, count: usize) {
        for _ in 0..count {
            let connection = self.connection.clone();
            let sender = self.notify.clone();
            self.readers.push(tokio::spawn(async move {
                loop {
                    match transport::receive_uni(&connection).await {
                        Ok(incoming) => {
                            if sender.send(Wire::Display(incoming)).await.is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            let _ = sender
                                .send(Wire::Lost(format!("display ended: {error:#}")))
                                .await;
                            return;
                        }
                    }
                }
            }));
        }
    }

    async fn send(&mut self, message: ClientMessage) -> Result<()> {
        tokio::time::timeout(WAIT, transport::send_message(&mut self.send, &message))
            .await
            .context("control write exceeded deadline")??;
        Ok(())
    }

    async fn receive(&mut self) -> Result<Wire> {
        tokio::time::timeout(WAIT, self.receive.recv())
            .await
            .context("network event read exceeded deadline")?
            .context("fixture network readers ended")
    }

    async fn wait_for_rejection(&mut self) -> Result<()> {
        loop {
            match self.receive().await? {
                Wire::Control(ServerMessage::Error { .. }) | Wire::Lost(_) => return Ok(()),
                _ => (),
            }
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.connection.close(0_u32.into(), b"fixture reattachment");
        for reader in &self.readers {
            reader.abort();
        }
    }
}

struct View {
    display: Display<Vec<u8>>,
    resources: BTreeMap<Digest, Resource>,
    pending: Option<Frame>,
    effects: BTreeMap<u64, Effect>,
    applied: BTreeMap<u64, usize>,
    raw_labels: BTreeSet<String>,
    replayed: BTreeSet<u64>,
    sequence: u64,
    committed: u64,
    ack_effects: bool,
    exit: Option<(i32, u64, u64)>,
    frames: usize,
}

impl View {
    fn new() -> Self {
        Self {
            display: Display::new(Vec::new(), Graphics::Kitty, PredictionPolicy::Never),
            resources: BTreeMap::new(),
            pending: None,
            effects: BTreeMap::new(),
            applied: BTreeMap::new(),
            raw_labels: BTreeSet::new(),
            replayed: BTreeSet::new(),
            sequence: 0,
            committed: 0,
            ack_effects: false,
            exit: None,
            frames: 0,
        }
    }

    async fn input(&mut self, peer: &mut Peer, event: InputEvent) -> Result<InputRecord> {
        event.validate()?;
        self.sequence += 1;
        let record = InputRecord {
            seq: self.sequence,
            event,
        };
        peer.send(ClientMessage::Input {
            epoch: peer.epoch,
            record: record.clone(),
        })
        .await?;
        Ok(record)
    }

    async fn key(&mut self, peer: &mut Peer, character: char) -> Result<InputRecord> {
        self.input(peer, key(KeyCode::Char(character))).await
    }

    fn text(&self) -> String {
        self.display
            .screen()
            .map(|screen| {
                screen
                    .rows
                    .iter()
                    .map(|row| {
                        row.cells
                            .iter()
                            .map(|cell| cell.text.as_str())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    async fn wait_text(&mut self, peer: &mut Peer, expected: &str) -> Result<()> {
        tokio::time::timeout(WAIT, async {
            while self.display.needs_checkpoint() || !self.text().contains(expected) {
                let event = peer.receive().await?;
                self.process(peer, event).await?;
            }
            Result::<()>::Ok(())
        })
        .await
        .with_context(|| {
            format!(
                "did not receive screen marker {expected:?}; current screen: {:?}",
                self.text()
            )
        })??;
        Ok(())
    }

    async fn wait_committed(&mut self, peer: &mut Peer, sequence: u64) -> Result<()> {
        tokio::time::timeout(WAIT, async {
            while self.committed < sequence {
                let event = peer.receive().await?;
                self.process(peer, event).await?;
            }
            Result::<()>::Ok(())
        })
        .await??;
        Ok(())
    }

    async fn process(&mut self, peer: &mut Peer, event: Wire) -> Result<()> {
        match event {
            Wire::Lost(reason) => bail!(
                "unexpected fixture connection loss: {reason}; screen={:?}",
                self.text()
            ),
            Wire::Control(message) => match message {
                ServerMessage::Committed { epoch, through } => {
                    ensure!(
                        epoch == peer.epoch
                            && through >= self.committed
                            && through <= self.sequence,
                        "invalid input commit"
                    );
                    self.committed = through;
                }
                ServerMessage::Effects {
                    epoch,
                    floor,
                    items,
                } => {
                    ensure!(epoch == peer.epoch, "stale effects epoch");
                    self.display.set_effect_floor(floor);
                    self.effects.retain(|id, _| *id > floor);
                    for effect in items {
                        if let EffectKind::ClipboardWrite { data, .. } = &effect.kind {
                            self.raw_labels.insert(data.clone());
                        }
                        if effect.id <= self.display.effects_applied() {
                            self.replayed.insert(effect.id);
                        } else {
                            self.effects.entry(effect.id).or_insert(effect);
                        }
                    }
                }
                ServerMessage::Exit {
                    epoch,
                    code,
                    revision,
                    effects,
                } => {
                    ensure!(
                        epoch == peer.epoch
                            && self.display.revision() >= revision
                            && self.display.effects_applied() >= effects,
                        "exit preceded final display/effect barrier"
                    );
                    self.exit = Some((code, revision, effects));
                }
                ServerMessage::Error { message } => bail!("session actor reported {message}"),
                ServerMessage::Attached { .. } => bail!("duplicate attachment response"),
            },
            Wire::Display(Incoming::Resource(resource)) => {
                resource.validate()?;
                self.resources.insert(resource.meta.digest, resource);
            }
            Wire::Display(Incoming::Frame(frame)) => {
                ensure!(frame.epoch == peer.epoch, "stale frame epoch");
                if frame.revision > self.display.revision()
                    && self
                        .pending
                        .as_ref()
                        .is_none_or(|pending| frame.revision > pending.revision)
                {
                    self.pending = Some(frame);
                }
            }
        }
        self.install_ready(peer).await
    }

    async fn install_ready(&mut self, peer: &mut Peer) -> Result<()> {
        if let Some(frame) = &self.pending {
            let missing: Vec<_> = frame
                .resources
                .iter()
                .filter(|meta| !self.resources.contains_key(&meta.digest))
                .map(|meta| meta.digest)
                .take(128)
                .collect();
            if !missing.is_empty() {
                peer.send(ClientMessage::MissingResources {
                    epoch: peer.epoch,
                    digests: missing,
                })
                .await?;
            } else {
                let frame = self.pending.take().expect("complete frame");
                let desired: BTreeSet<_> = frame.resources.iter().map(|meta| meta.digest).collect();
                if self.display.install(&frame, &self.resources)? {
                    self.frames += 1;
                    peer.send(ClientMessage::Rendered {
                        epoch: peer.epoch,
                        revision: self.display.revision(),
                    })
                    .await?;
                    self.resources.retain(|digest, _| desired.contains(digest));
                } else if self.display.needs_checkpoint() {
                    peer.send(ClientMessage::Resync { epoch: peer.epoch })
                        .await?;
                }
            }
        }
        while let Some((&id, effect)) = self.effects.first_key_value() {
            if effect.after > self.display.revision() {
                break;
            }
            let effect = self.effects.remove(&id).expect("ready effect");
            self.display.apply_effect(&effect, true)?;
            *self.applied.entry(effect.id).or_default() += 1;
            if let EffectKind::ClipboardRead { query, .. } = effect.kind {
                self.input(
                    peer,
                    InputEvent::QueryResult {
                        query,
                        data: Some("local-clipboard".into()),
                    },
                )
                .await?;
            }
        }
        if self.ack_effects {
            peer.send(ClientMessage::EffectsApplied {
                epoch: peer.epoch,
                through: self.display.effects_applied(),
            })
            .await?;
        }
        Ok(())
    }
}

struct Running {
    endpoint: ServerListeners,
    task: Option<JoinHandle<Result<()>>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.endpoint.close(0_u32.into(), b"fixture stopped");
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn key(code: KeyCode) -> InputEvent {
    InputEvent::Key(Key {
        code,
        modifiers: 0,
        phase: KeyPhase::Press,
        shifted: None,
        base: None,
        text: None,
    })
}

fn size(cols: u16, rows: u16) -> Size {
    Size {
        cols,
        rows,
        pixel_width: u32::from(cols) * 10,
        pixel_height: u32::from(rows) * 20,
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mode = std::env::args().nth(1);
    match mode.as_deref() {
        Some("--fixture") => return fixture(),
        Some("--close-fixture") => return close_fixture(),
        Some("--locale-fixture") => return locale_fixture(),
        _ => (),
    }
    let _diagnostics = if mode.as_deref() == Some("--diagnostics") {
        let path = std::env::args()
            .nth(2)
            .context("--diagnostics requires an output path")?;
        Some(mosh_rs::diagnostics::start(
            mosh_rs::diagnostics::Role::Server,
            std::path::Path::new(&path),
        )?)
    } else {
        None
    };
    tokio::task::LocalSet::new()
        .run_until(async {
            tokio::time::timeout(Duration::from_secs(180), async {
                if mode.as_deref() == Some("--verify-close") {
                    verify_close().await
                } else if mode.as_deref() == Some("--verify-prepared") {
                    verify_prepared().await
                } else if mode.as_deref() == Some("--verify-locale") {
                    verify_locale().await
                } else {
                    verify().await
                }
            })
            .await
            .context("entire session fixture exceeded its deadline")?
        })
        .await
}

async fn verify() -> Result<()> {
    verify_prepared().await?;
    verify_locale().await?;
    let identity = ClientIdentity::generate()?;
    let request = BootstrapRequest {
        version: PROTOCOL_VERSION,
        client_cert: identity.certificate(),
        size: size(80, 12),
        command: vec![
            std::env::current_exe()?.to_string_lossy().into_owned(),
            "--fixture".into(),
        ],
        bind: Some("127.0.0.1".into()),
        port: None,
        idle_seconds: 60,
        clipboard_read: true,
        locale_candidates: Vec::new(),
    };
    let (endpoint, grant) = transport::listen(&request)?;
    let actor = SessionActor::new(&request, grant.session.clone())?;
    let task = tokio::task::spawn_local(actor.run(endpoint.clone()));
    let mut running = Running {
        endpoint,
        task: Some(task),
    };
    let client = identity.endpoint(&grant)?;
    let mut view = View::new();
    let mut attempt = 1;
    let mut peer = Peer::attach(&client, &grant, &mut view, attempt, None, 3).await?;
    view.wait_text(&mut peer, "LEGACY_READY").await?;
    let initial = view.display.screen().context("initial screen missing")?;
    ensure!(
        initial.title == "mosh-session-fixture",
        "terminal title lost"
    );
    ensure!(
        initial.directory.as_deref() == Some("file://mosh-fixture/tmp/session"),
        "OSC7 directory lost"
    );
    ensure!(
        initial.markers.iter().any(|marker| marker.kind == "A"),
        "shell prompt marker lost"
    );
    let linked = initial
        .rows
        .iter()
        .flat_map(|row| &row.cells)
        .find(|cell| cell.style.hyperlink.is_some())
        .context("hyperlink missing")?;
    ensure!(
        linked
            .style
            .hyperlink
            .as_ref()
            .is_some_and(|link| link.params == "id=session-link"
                && link.url == "https://example.com/session"),
        "hyperlink params or URL lost"
    );
    ensure!(
        initial
            .rows
            .iter()
            .flat_map(|row| &row.cells)
            .any(|cell| cell.style.fg == Color::Rgb([12, 34, 56])),
        "RGB state lost"
    );
    ensure!(
        view.text().contains("现代 e\u{301}"),
        "Unicode grapheme state lost"
    );
    let pid = extract_pid(&view.text())?;
    println!(
        "verified modern text, hyperlink parameters, directory, and shell markers through real PTY frames"
    );

    view.input(&mut peer, key(KeyCode::Named("Up".into())))
        .await?;
    view.input(&mut peer, InputEvent::Paste("paste\n✓".into()))
        .await?;
    view.input(
        &mut peer,
        InputEvent::Mouse(Mouse {
            x: 2,
            y: 1,
            pixel_x: 0,
            pixel_y: 0,
            modifiers: 0,
            kind: MouseKind::Press(0),
        }),
    )
    .await?;
    view.input(
        &mut peer,
        InputEvent::Mouse(Mouse {
            x: 2,
            y: 1,
            pixel_x: 0,
            pixel_y: 0,
            modifiers: 0,
            kind: MouseKind::Release(0),
        }),
    )
    .await?;
    view.input(&mut peer, InputEvent::Focus(false)).await?;
    view.input(&mut peer, InputEvent::Focus(true)).await?;
    view.wait_text(&mut peer, "KITTY_KEYS_READY").await?;
    for phase in [KeyPhase::Press, KeyPhase::Repeat, KeyPhase::Release] {
        view.input(
            &mut peer,
            InputEvent::Key(Key {
                code: KeyCode::Char('a'),
                modifiers: 1,
                phase,
                shifted: Some('A'),
                base: Some('q'),
                text: Some("A".into()),
            }),
        )
        .await?;
    }
    view.wait_text(&mut peer, "BULK_READY").await?;
    println!(
        "verified application cursor, paste, mouse, focus, and kitty phases against exact helper bytes"
    );
    println!("starting bulk input");
    let bulk = view
        .input(&mut peer, InputEvent::Paste("B".repeat(BULK_BYTES)))
        .await?;
    view.wait_text(&mut peer, "BULK_STARTED").await?;
    println!(
        "observed partial bulk: committed={} bulk={} epoch={}",
        view.committed, bulk.seq, peer.epoch
    );
    ensure!(
        view.committed < bulk.seq,
        "bulk event committed before blocked PTY consumed it"
    );
    peer.send(ClientMessage::Resize {
        epoch: peer.epoch,
        generation: 17,
        after: bulk.seq,
        size: size(37, 9),
    })
    .await?;
    tokio::time::sleep(Duration::from_millis(20)).await;
    drop(peer);
    attempt += 1;
    let mut peer = Peer::attach(&client, &grant, &mut view, attempt, None, 3).await?;
    println!(
        "reattached during bulk: committed={} bulk={} epoch={}",
        view.committed, bulk.seq, peer.epoch
    );
    peer.send(ClientMessage::Input {
        epoch: peer.epoch,
        record: bulk.clone(),
    })
    .await?;
    let sentinel = view
        .input(&mut peer, InputEvent::Paste("END!".into()))
        .await?;
    println!("sent bulk retry and sentinel seq={}", sentinel.seq);
    view.wait_text(&mut peer, "OLD_RESIZE_CLEARED:80x12")
        .await?;
    ensure!(
        view.text().contains(&format!("DEDUP_OK pid={pid}")),
        "helper restarted or bulk duplicated"
    );
    println!("received complete bulk and sentinel; waiting commit");
    view.wait_committed(&mut peer, sentinel.seq).await?;
    println!("committed sentinel; requesting valid resize");
    peer.send(ClientMessage::Resize {
        epoch: peer.epoch,
        generation: 1,
        after: sentinel.seq,
        size: size(100, 20),
    })
    .await?;
    tokio::time::timeout(WAIT, async {
        while view
            .display
            .screen()
            .is_none_or(|screen| screen.size != size(100, 20))
        {
            let event = peer.receive().await?;
            view.process(&mut peer, event).await?;
        }
        Result::<()>::Ok(())
    })
    .await??;
    view.key(&mut peer, 'R').await?;
    view.wait_text(&mut peer, "RESIZE_OK:100x20").await?;
    println!("verified partial PTY write reconnect, retry deduplication, and fenced resize");
    let rejected = Peer::attach(&client, &grant, &mut view, 1, None, 3).await;
    ensure!(
        rejected.is_err(),
        "stale attachment stole the active worker"
    );

    for stale in 0..3 {
        let old_epoch = peer.epoch - 1;
        let message = match stale {
            0 => ClientMessage::Resize {
                epoch: old_epoch,
                generation: 99,
                after: view.sequence,
                size: size(7, 7),
            },
            1 => ClientMessage::Rendered {
                epoch: old_epoch,
                revision: u64::MAX,
            },
            _ => ClientMessage::Close { epoch: old_epoch },
        };
        peer.send(message).await?;
        peer.wait_for_rejection().await?;
        drop(peer);
        attempt += 1;
        peer = Peer::attach(&client, &grant, &mut view, attempt, None, 3).await?;
        view.wait_text(&mut peer, "RESIZE_OK:100x20").await?;
        ensure!(
            view.display
                .screen()
                .is_some_and(|screen| screen.size == size(100, 20)),
            "stale resize changed the worker"
        );
    }

    println!("verified stale attachment, close, resize, and render acknowledgement rejection");
    view.key(&mut peer, 'G').await?;
    view.wait_text(&mut peer, "IMAGES_READY").await?;
    let images = view.display.screen().context("image screen missing")?;
    ensure!(
        images
            .resources
            .values()
            .any(|resource| resource.meta.width == 1
                && resource.meta.height == 1
                && resource.rgba.as_ref() == [255, 0, 0, 255]),
        "Kitty image bytes lost"
    );
    ensure!(
        images
            .resources
            .values()
            .any(|resource| resource.meta.width == 1
                && resource.meta.height == 6
                && resource.rgba[..4] == [0, 255, 0, 255]),
        "SIXEL image bytes lost"
    );
    let digest = *images
        .resources
        .keys()
        .next()
        .context("image resource absent")?;
    peer.send(ClientMessage::MissingResources {
        epoch: peer.epoch,
        digests: vec![digest],
    })
    .await?;
    let late = loop {
        let event = peer.receive().await?;
        if let Wire::Display(Incoming::Resource(resource)) = event {
            if resource.meta.digest == digest {
                break resource;
            }
            view.process(&mut peer, Wire::Display(Incoming::Resource(resource)))
                .await?;
        } else {
            view.process(&mut peer, event).await?;
        }
    };
    view.key(&mut peer, 'D').await?;
    view.wait_text(&mut peer, "IMAGES_DELETED").await?;
    view.process(&mut peer, Wire::Display(Incoming::Resource(late)))
        .await?;
    ensure!(
        view.display
            .screen()
            .is_some_and(|screen| screen.resources.is_empty()
                && screen
                    .rows
                    .iter()
                    .flat_map(|row| &row.cells)
                    .all(|cell| cell.images.is_empty())),
        "late resource resurrected a placement"
    );

    println!("verified Kitty and SIXEL bytes, deletion, and delayed resource delivery");
    view.key(&mut peer, 'C').await?;
    view.wait_text(&mut peer, "CLIPBOARD_OK").await?;
    ensure!(
        view.raw_labels.contains("clipboard-write"),
        "clipboard write did not traverse effect stream"
    );
    let applied_before: BTreeSet<_> = view.applied.keys().copied().collect();
    ensure!(
        applied_before.len() >= 3,
        "clipboard query/write/bell effects missing"
    );
    drop(peer);
    attempt += 1;
    let mut peer = Peer::attach(&client, &grant, &mut view, attempt, Some(0), 3).await?;
    tokio::time::timeout(WAIT, async {
        while !applied_before.is_subset(&view.replayed) {
            let event = peer.receive().await?;
            view.process(&mut peer, event).await?;
        }
        Result::<()>::Ok(())
    })
    .await??;
    ensure!(
        view.applied.values().all(|count| *count == 1),
        "reconnection repeated an external effect"
    );
    view.ack_effects = true;
    view.install_ready(&mut peer).await?;
    println!("verified clipboard replies through PTY and effect replay deduplication");

    view.key(&mut peer, 'F').await?;
    view.wait_text(&mut peer, "FLOOD_READY").await?;
    println!("flood helper ready; waiting actual flood frame");
    view.wait_text(&mut peer, "FLOOD_LINE").await?;
    println!("actual flood frame observed; sending Ctrl-C");
    let interrupted = Instant::now();
    view.input(
        &mut peer,
        InputEvent::Key(Key {
            code: KeyCode::Char('c'),
            modifiers: 4,
            phase: KeyPhase::Press,
            shifted: None,
            base: None,
            text: None,
        }),
    )
    .await?;
    println!("Ctrl-C sent; waiting latest prompt");
    view.wait_text(&mut peer, "LATEST_PROMPT")
        .await
        .with_context(|| {
            format!(
                "flood final prompt missing; committed={} expected={} screen={:?}",
                view.committed,
                view.sequence,
                view.text()
            )
        })?;
    ensure!(
        interrupted.elapsed() < Duration::from_secs(5),
        "output flood prevented prompt input progress"
    );
    ensure!(
        view.text().contains(&format!("pid={pid}")),
        "worker restarted during flood"
    );
    ensure!(
        !view.text().contains("FLOOD_LINE"),
        "obsolete flood screen did not converge"
    );

    println!("verified Ctrl-C input progress and latest screen after output flood");
    drop(peer);
    attempt += 1;
    let mut peer = Peer::attach(&client, &grant, &mut view, attempt, None, 0).await?;
    view.key(&mut peer, 'S').await?;
    let mut held = tokio::time::timeout(
        Duration::from_secs(90),
        hold_large_frame(&mut peer, &mut view),
    )
    .await
    .context("did not receive a large backpressured display")??;
    println!("holding large display; waiting helper completion effect");
    tokio::time::timeout(WAIT, async {
        while !view.raw_labels.contains("BIG_DISPLAY_READY") {
            let event = peer.receive().await?;
            view.process(&mut peer, event).await?;
        }
        Result::<()>::Ok(())
    })
    .await
    .context("large display helper did not finish its output")??;
    let hold = Duration::from_millis(if mosh_rs::diagnostics::enabled() {
        1200
    } else {
        350
    });
    loop {
        match tokio::time::timeout(hold, held.received_reset()).await {
            Err(_) => break,
            Ok(Ok(Some(code))) if code == VarInt::from_u32(0x10) => {
                held = tokio::time::timeout(WAIT, hold_large_frame(&mut peer, &mut view)).await??;
            }
            reset => bail!("large display did not remain backpressured: {reset:?}"),
        }
    }
    view.key(&mut peer, 'T').await?;
    let cancelled = tokio::time::timeout(WAIT, held.received_reset())
        .await
        .context("obsolete display was not cancelled")??;
    ensure!(
        cancelled == Some(VarInt::from_u32(0x10)),
        "obsolete backpressured display was transmitted instead of superseded"
    );
    peer.start_uni_readers(3);
    view.wait_text(&mut peer, "SMALL_NEW_PROMPT").await?;
    ensure!(
        view.text().contains(&format!("pid={pid}")),
        "worker restarted under display backpressure"
    );
    println!("verified obsolete display RESET under real QUIC receive-window backpressure");

    view.key(&mut peer, 'X').await?;
    tokio::time::timeout(WAIT, async {
        while view.exit.is_none() {
            let event = peer.receive().await?;
            view.process(&mut peer, event).await?;
        }
        Result::<()>::Ok(())
    })
    .await??;
    ensure!(
        view.exit.is_some_and(|(code, ..)| code == 23),
        "remote exit status lost"
    );
    ensure!(
        view.text().contains("FINAL_SCREEN"),
        "exit preceded final screen"
    );
    let task = running.task.as_ref().context("actor task missing")?;
    ensure!(!task.is_finished(), "actor ended before ExitAck");
    peer.send(ClientMessage::ExitAck { epoch: peer.epoch })
        .await?;
    tokio::time::timeout(WAIT, running.task.take().context("actor task missing")?).await???;
    let output = view.display.into_inner();
    for expected in [
        b"38;2;12;34;56".as_slice(),
        "现代 e\u{301}".as_bytes(),
        b"\x1b]8;id=session-link;https://example.com/session",
        b"\x1b]7;file://mosh-fixture/tmp/session",
        b"\x1b]133;A",
        b"\x1b]133;D;23",
        b"\x1b_Ga=t",
        b"\x1b_Ga=d",
        b"\x1b]52;c;Y2xpcGJvYXJkLXdyaXRl",
        b"\x1b]52;c;?",
    ] {
        ensure!(
            output
                .windows(expected.len())
                .any(|window| window == expected),
            "renderer omitted expected bytes: {:?}",
            String::from_utf8_lossy(expected)
        );
    }
    verify_close().await?;
    println!(
        "session verification passed: {} installed frames; raw PTY input modes, partial writes, reconnect deduplication, fencing, resize, graphics, late resources, effects, queries, flood convergence, final exit barriers, explicit close and child reaping",
        view.frames
    );
    Ok(())
}

async fn verify_prepared() -> Result<()> {
    let identity = ClientIdentity::generate()?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let marker =
        std::env::temp_dir().join(format!("mosh-rs-prepared-{}-{stamp}", std::process::id()));
    let request = BootstrapRequest {
        version: PROTOCOL_VERSION,
        client_cert: identity.certificate(),
        size: size(80, 12),
        command: vec![
            std::env::current_exe()?.to_string_lossy().into_owned(),
            "--close-fixture".into(),
            marker.to_string_lossy().into_owned(),
        ],
        bind: None,
        port: None,
        idle_seconds: 60,
        clipboard_read: false,
        locale_candidates: Vec::new(),
    };
    let (endpoint, grant) = transport::listen(&request)?;
    let mut ipv4_grant = grant.clone();
    ipv4_grant.address = "127.0.0.1".into();
    let mut ipv6_grant = grant.clone();
    ipv6_grant.address = "::1".into();
    let actor = SessionActor::new(&request, grant.session.clone())?;
    let mut grant_bytes = Vec::new();
    transport::write_grant(&mut grant_bytes, &grant)?;
    let mut running = Running {
        task: Some(tokio::task::spawn_local(actor.run(endpoint.clone()))),
        endpoint,
    };
    let client = identity.endpoint(&grant)?;
    let handshake = transport::connect(&client, &ipv4_grant).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    ensure!(
        !marker.exists(),
        "grant or TLS handshake started the user command before attach"
    );
    handshake.close(0_u32.into(), b"no attachment");

    for invalid in 0..6 {
        let destination = if invalid % 2 == 0 {
            &ipv4_grant
        } else {
            &ipv6_grant
        };
        let connection = transport::connect(&client, destination).await?;
        let (mut send, mut receive) = connection.open_bi().await?;
        let mut attach = Attach {
            version: PROTOCOL_VERSION,
            session: grant.session.clone(),
            attempt: 1,
            committed: 0,
            rendered: 0,
            effects: 0,
        };
        match invalid {
            0 => attach.version -= 1,
            1 => attach.session = "invalid-session".into(),
            2 => attach.attempt = 0,
            3 => attach.committed = 1,
            4 => attach.effects = 1,
            _ => (),
        }
        let message = if invalid == 5 {
            ClientMessage::Close { epoch: 0 }
        } else {
            ClientMessage::Attach(attach)
        };
        transport::send_message(&mut send, &message).await?;
        let reply =
            tokio::time::timeout(WAIT, transport::recv_message::<ServerMessage>(&mut receive))
                .await
                .context("invalid first attachment was not rejected")?;
        ensure!(
            matches!(
                reply,
                Err(_) | Ok(None) | Ok(Some(ServerMessage::Error { .. }))
            ),
            "invalid first attachment was accepted"
        );
        ensure!(!marker.exists(), "invalid attach started the user command");
        connection.close(0_u32.into(), b"invalid fixture complete");
    }

    let mut first_view = View::new();
    let mut view = View::new();
    view.ack_effects = true;
    let (first, second) = tokio::join!(
        Peer::attach(&client, &ipv4_grant, &mut first_view, 1, None, 3),
        Peer::attach(&client, &ipv6_grant, &mut view, 2, None, 3),
    );
    let (mut peer, mut view) = match (first, second) {
        (Ok(first), Ok(second)) if first.epoch > second.epoch => (first, first_view),
        (Ok(_), Ok(second)) | (Err(_), Ok(second)) => (second, view),
        (Ok(first), Err(_)) => (first, first_view),
        (Err(first), Err(second)) => {
            bail!("both first attachments failed: {first:#}; {second:#}")
        }
    };
    view.wait_text(&mut peer, "CLOSE_READY").await?;
    let pid = extract_pid(&view.text())?;
    ensure!(
        std::fs::read_to_string(&marker)? == format!("{pid}\n"),
        "competing first attachments started more than one command"
    );
    ensure!(
        Peer::attach(&client, &grant, &mut View::new(), 1, None, 3)
            .await
            .is_err(),
        "obsolete attachment was accepted after command startup"
    );
    let epoch = peer.epoch;
    let destination = if peer.connection.remote_address().is_ipv4() {
        &ipv6_grant
    } else {
        &ipv4_grant
    };
    drop(peer);
    let mut peer = Peer::attach(&client, destination, &mut view, 3, None, 3).await?;
    view.wait_text(&mut peer, "CLOSE_READY").await?;
    ensure!(
        peer.epoch > epoch
            && extract_pid(&view.text())? == pid
            && std::fs::read_to_string(&marker)? == format!("{pid}\n"),
        "reattachment restarted the user command"
    );
    peer.send(ClientMessage::Close { epoch: peer.epoch })
        .await?;
    while view.exit.is_none() {
        let event = peer.receive().await?;
        view.process(&mut peer, event).await?;
    }
    peer.send(ClientMessage::ExitAck { epoch: peer.epoch })
        .await?;
    tokio::time::timeout(
        WAIT,
        running.task.take().context("prepared actor task missing")?,
    )
    .await???;
    std::fs::remove_file(&marker)?;

    let (unpolled, unpolled_grant) = transport::listen(&request)?;
    let actor = SessionActor::new(&request, unpolled_grant.session.clone())?;
    drop(actor.run(unpolled.clone()));
    ensure!(
        tokio::time::timeout(WAIT, unpolled.accept())
            .await?
            .is_none(),
        "unpolled actor future left a listener accepting"
    );

    let (cancelled, cancellation_grant) = transport::listen(&request)?;
    drop(cancelled.clone());
    let actor = SessionActor::new(&request, cancellation_grant.session.clone())?;
    let cancellation_task = tokio::task::spawn_local(actor.run(cancelled.clone()));
    let cancellation_client = identity.endpoint(&cancellation_grant)?;
    let handshake = transport::connect(&cancellation_client, &cancellation_grant).await?;
    handshake.close(0_u32.into(), b"cancellation fixture");
    cancellation_task.abort();
    ensure!(
        cancellation_task
            .await
            .is_err_and(|error| error.is_cancelled()),
        "actor was not cancelled"
    );
    ensure!(
        tokio::time::timeout(WAIT, cancelled.accept())
            .await?
            .is_none(),
        "cancelled actor left a listener accepting"
    );
    cancelled.wait_idle().await;
    ensure!(
        !marker.exists(),
        "cancelled prepared actor started a command"
    );

    let mut missing = request;
    missing.command = vec![
        marker
            .join("missing-program")
            .to_string_lossy()
            .into_owned(),
    ];
    let (endpoint, grant) = transport::listen(&missing)?;
    let actor = SessionActor::new(&missing, grant.session.clone())?;
    let mut running = Running {
        task: Some(tokio::task::spawn_local(actor.run(endpoint.clone()))),
        endpoint,
    };
    let client = identity.endpoint(&grant)?;
    let failure = Peer::attach(&client, &grant, &mut View::new(), 1, None, 3).await;
    let Err(error) = failure else {
        bail!("missing executable produced a successful attachment");
    };
    ensure!(
        format!("{error:#}").contains("remote command startup failed"),
        "spawn failure did not reach the client: {error:#}"
    );
    let result = tokio::time::timeout(
        WAIT,
        running
            .task
            .take()
            .context("failed startup actor task missing")?,
    )
    .await??;
    ensure!(
        result.is_err(),
        "spawn failure returned to Prepared instead of ending"
    );
    println!(
        "verified grant/TLS/invalid attaches have no command side effects, competing IPv4/IPv6 first attaches spawn once, cross-family reconnect advances the epoch and keeps the PID, unpolled and running cancellation close all listeners, and spawn failure ends the actor"
    );
    Ok(())
}

async fn verify_locale() -> Result<()> {
    let original_environment = ["LC_ALL", "LC_CTYPE", "LANG"].map(std::env::var_os);
    let identity = ClientIdentity::generate()?;
    let request = BootstrapRequest {
        version: PROTOCOL_VERSION,
        client_cert: identity.certificate(),
        size: size(80, 12),
        command: vec![
            std::env::current_exe()?.to_string_lossy().into_owned(),
            "--locale-fixture".into(),
        ],
        bind: Some("127.0.0.1".into()),
        port: None,
        idle_seconds: 60,
        clipboard_read: false,
        locale_candidates: vec!["mosh_missing.UTF-8".into(), "C".into()],
    };
    let (endpoint, grant) = transport::listen(&request)?;
    let actor = SessionActor::new(&request, grant.session.clone())?;
    let task = tokio::task::spawn_local(actor.run(endpoint.clone()));
    let client = identity.endpoint(&grant)?;
    let mut view = View::new();
    let mut peer = Peer::attach(&client, &grant, &mut view, 1, None, 1).await?;
    view.wait_text(&mut peer, "LOCALE_READY").await?;
    let text = view.text();
    ensure!(
        text.contains("LC_ALL=<unset>"),
        "child inherited LC_ALL: {text:?}"
    );
    ensure!(
        text.contains("CODESET=UTF-8") || text.contains("CODESET=utf-8"),
        "child did not use an actual UTF-8 codeset: {text:?}"
    );
    ensure!(
        text.contains("中文é"),
        "child did not emit UTF-8 fixture text: {text:?}"
    );
    let ctype = text
        .lines()
        .find_map(|line| line.strip_prefix("LC_CTYPE="))
        .context("child LC_CTYPE missing")?
        .trim();
    let lang = text
        .lines()
        .find_map(|line| line.strip_prefix("LANG="))
        .context("child LANG missing")?
        .trim();
    ensure!(
        valid_locale_name(ctype) && ctype == lang,
        "child locale variables disagree: {text:?}"
    );
    view.input(&mut peer, InputEvent::Paste("中文\u{7f}\n".into()))
        .await?;
    view.wait_text(&mut peer, "UTF8_ERASE_OK").await?;
    peer.send(ClientMessage::Close { epoch: peer.epoch })
        .await?;
    while view.exit.is_none() {
        let event = peer.receive().await?;
        view.process(&mut peer, event).await?;
    }
    peer.send(ClientMessage::ExitAck { epoch: peer.epoch })
        .await?;
    let result = tokio::time::timeout(WAIT, task).await??;
    ensure!(result.is_ok(), "locale fixture actor failed: {result:?}");
    endpoint.close(0_u32.into(), b"locale fixture complete");
    ensure!(
        original_environment == ["LC_ALL", "LC_CTYPE", "LANG"].map(std::env::var_os),
        "PTY startup changed parent locale environment"
    );
    println!(
        "verified locale candidate fallback, child UTF-8 codeset, isolated parent environment, IUTF8 and canonical multi-byte erase"
    );
    Ok(())
}

async fn verify_close() -> Result<()> {
    let identity = ClientIdentity::generate()?;
    let request = BootstrapRequest {
        version: PROTOCOL_VERSION,
        client_cert: identity.certificate(),
        size: size(80, 12),
        command: vec![
            std::env::current_exe()?.to_string_lossy().into_owned(),
            "--close-fixture".into(),
        ],
        bind: Some("127.0.0.1".into()),
        port: None,
        idle_seconds: 60,
        clipboard_read: false,
        locale_candidates: Vec::new(),
    };
    let (endpoint, grant) = transport::listen(&request)?;
    let actor = SessionActor::new(&request, grant.session.clone())?;
    let mut running = Running {
        task: Some(tokio::task::spawn_local(actor.run(endpoint.clone()))),
        endpoint,
    };
    let client = identity.endpoint(&grant)?;
    let mut view = View::new();
    view.ack_effects = true;
    let mut peer = Peer::attach(&client, &grant, &mut view, 1, None, 3).await?;
    view.wait_text(&mut peer, "CLOSE_READY").await?;
    let pid = extract_pid(&view.text())?;
    ensure!(
        unsafe { libc::kill(pid as libc::pid_t, 0) } == 0,
        "close fixture child ended before the close request"
    );
    let initial_revision = view.display.revision();
    for _ in 0..2 {
        peer.send(ClientMessage::Close { epoch: peer.epoch })
            .await?;
    }
    peer.send(ClientMessage::Resync { epoch: peer.epoch })
        .await?;
    tokio::time::timeout(WAIT, async {
        while view.exit.is_none() {
            let event = peer.receive().await?;
            view.process(&mut peer, event).await?;
        }
        Result::<()>::Ok(())
    })
    .await
    .context("explicit close stopped network progress before the exit barrier")??;
    ensure!(
        view.exit.is_some_and(|(code, ..)| code == 129),
        "explicit close lost its exit status"
    );
    ensure!(
        view.display.revision() > initial_revision,
        "explicit close did not publish its final display"
    );
    ensure!(
        !running
            .task
            .as_ref()
            .context("close actor task missing")?
            .is_finished(),
        "closed actor ended before ExitAck"
    );
    peer.send(ClientMessage::ExitAck { epoch: peer.epoch })
        .await?;
    tokio::time::timeout(
        WAIT,
        running.task.take().context("close actor task missing")?,
    )
    .await
    .context("closed actor did not finish after ExitAck")???;
    tokio::time::timeout(WAIT, async {
        let mut retry = tokio::time::interval(Duration::from_millis(10));
        loop {
            if unsafe { libc::kill(pid as libc::pid_t, 0) } == -1 {
                let error = std::io::Error::last_os_error();
                ensure!(
                    error.raw_os_error() == Some(libc::ESRCH),
                    "cannot inspect closed PTY child: {error}"
                );
                return Result::<()>::Ok(());
            }
            retry.tick().await;
        }
    })
    .await
    .context("closed PTY child was not reaped")??;
    println!(
        "verified repeated explicit close, final exit barriers, actor completion and child reaping"
    );
    Ok(())
}

async fn hold_large_frame(peer: &mut Peer, view: &mut View) -> Result<RecvStream> {
    let connection = peer.connection.clone();
    let mut largest = 0;
    loop {
        let mut stream = tokio::select! {
            stream = connection.accept_uni() => stream?,
            event = peer.receive() => {
                view.process(peer, event?).await?;
                continue;
            }
        };
        let mut kind = [0; 1];
        match stream.read_exact(&mut kind).await {
            Ok(()) => (),
            Err(quinn::ReadExactError::ReadError(quinn::ReadError::Reset(_))) => continue,
            Err(error) => return Err(error.into()),
        }
        ensure!(
            kind == [1],
            "unexpected graphics stream during display backpressure fixture"
        );
        let mut bytes = Vec::new();
        let mut received = 0usize;
        loop {
            let chunk = tokio::select! {
                chunk = stream.read_chunk(32 * 1024, true) => chunk,
                event = peer.receive() => {
                    view.process(peer, event?).await?;
                    continue;
                }
            };
            match chunk {
                Ok(Some(chunk)) => {
                    received += chunk.bytes.len();
                    ensure!(
                        received <= MAX_FRAME,
                        "backpressure fixture frame exceeds limit"
                    );
                    if bytes.len() <= 128 * 1024 {
                        bytes.extend_from_slice(&chunk.bytes);
                    }
                    if let Ok(Some(size)) = zstd::zstd_safe::get_frame_content_size(&bytes)
                        && size > largest + 500_000
                    {
                        largest = size;
                        println!("display fixture reached {size} uncompressed bytes");
                    }
                    if received > 128 * 1024
                        && zstd::zstd_safe::get_frame_content_size(&bytes)
                            .ok()
                            .flatten()
                            .is_some_and(|size| size > 3_000_000)
                    {
                        return Ok(stream);
                    }
                }
                Ok(None) => break,
                Err(quinn::ReadError::Reset(_)) => break,
                Err(error) => return Err(error.into()),
            }
        }
    }
}

fn extract_pid(text: &str) -> Result<u32> {
    let pid: String = text
        .split("pid=")
        .nth(1)
        .context("helper PID missing")?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    Ok(pid.parse()?)
}

fn close_fixture() -> Result<()> {
    if let Some(marker) = std::env::args().nth(2) {
        let mut marker = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(marker)?;
        writeln!(marker, "{}", std::process::id())?;
    }
    ensure!(
        unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) } != libc::SIG_ERR,
        "cannot ignore SIGHUP in close fixture"
    );
    emit(format!("\x1b[2J\x1b[HCLOSE_READY pid={}\r\n", std::process::id()).as_bytes())?;
    loop {
        std::thread::park();
    }
}

fn locale_fixture() -> Result<()> {
    let empty = CString::new("")?;
    ensure!(
        !unsafe { libc::setlocale(libc::LC_CTYPE, empty.as_ptr()) }.is_null(),
        "cannot initialize child locale"
    );
    let codeset = unsafe { libc::nl_langinfo(libc::CODESET) };
    ensure!(!codeset.is_null(), "child codeset query returned null");
    let codeset = unsafe { CStr::from_ptr(codeset) }.to_string_lossy();
    let mut attrs = std::mem::MaybeUninit::<libc::termios>::uninit();
    ensure!(
        unsafe { libc::tcgetattr(0, attrs.as_mut_ptr()) } == 0,
        "cannot read child PTY modes"
    );
    let attrs = unsafe { attrs.assume_init() };
    ensure!(
        attrs.c_lflag & libc::ICANON != 0,
        "PTY startup disabled canonical input"
    );
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    ensure!(
        attrs.c_iflag & libc::IUTF8 != 0,
        "PTY startup did not enable IUTF8"
    );
    ensure!(
        attrs.c_cc[libc::VERASE] == 0x7f,
        "unexpected default erase byte"
    );
    let value = |key: &str| std::env::var(key).unwrap_or_else(|_| "<unset>".into());
    emit(
        format!(
            "\x1b[2J\x1b[HLC_ALL={}\r\nLC_CTYPE={}\r\nLANG={}\r\nCODESET={}\r\n中文é\r\nLOCALE_READY\r\n",
            value("LC_ALL"),
            value("LC_CTYPE"),
            value("LANG"),
            codeset
        )
        .as_bytes(),
    )?;
    expect(&mut std::io::stdin().lock(), "中\n".as_bytes())?;
    emit(b"\r\nUTF8_ERASE_OK\r\n")?;
    loop {
        std::thread::park();
    }
}

fn fixture() -> Result<()> {
    let mut attrs = std::mem::MaybeUninit::<libc::termios>::uninit();
    ensure!(
        unsafe { libc::tcgetattr(0, attrs.as_mut_ptr()) } == 0,
        "fixture stdin is not a PTY"
    );
    let mut attrs = unsafe { attrs.assume_init() };
    unsafe { libc::cfmakeraw(&mut attrs) };
    ensure!(
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &attrs) } == 0,
        "cannot set raw PTY mode"
    );
    let mut input = std::io::stdin().lock();
    let pid = std::process::id();
    emit(format!("\x1b[2J\x1b[H\x1b]2;mosh-session-fixture\x1b\\\x1b]7;file://mosh-fixture/tmp/session\x1b\\\x1b]133;A\x1b\\\x1b[38;2;12;34;56m现代 é\x1b[0m \x1b]8;id=session-link;https://example.com/session\x1b\\link\x1b]8;;\x1b\\\r\nLEGACY_READY pid={pid}\r\n\x1b[?1h\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?1004h").as_bytes())?;
    expect(
        &mut input,
        "\x1bOA\x1b[200~paste\n✓\x1b[201~\x1b[<0;3;2M\x1b[<0;3;2m\x1b[O\x1b[I".as_bytes(),
    )?;
    emit(b"\x1b[>31u\r\nKITTY_KEYS_READY\r\n")?;
    expect(
        &mut input,
        b"\x1b[97:65:113;2:1;65u\x1b[97:65:113;2:2;65u\x1b[97:65:113;2:3u",
    )?;
    emit(b"\x1b[<u\x1b[?2004l\r\nBULK_READY\r\n")?;
    expect(&mut input, b"B")?;
    emit(b"\r\nBULK_STARTED\r\n")?;
    std::thread::sleep(Duration::from_millis(300));
    expect(&mut input, &vec![b'B'; BULK_BYTES - 1])?;
    expect(&mut input, b"END!")?;
    emit(
        format!(
            "\r\nDEDUP_OK pid={pid}\r\nOLD_RESIZE_CLEARED:{}x{}\r\n",
            dimensions()?.0,
            dimensions()?.1
        )
        .as_bytes(),
    )?;
    expect(&mut input, b"R")?;
    ensure!(
        dimensions()? == (100, 20),
        "valid resize did not reach PTY ioctl"
    );
    emit(b"\r\nRESIZE_OK:100x20\r\n")?;
    expect(&mut input, b"G")?;
    emit(b"\x1b[5;1H\x1b_Ga=T,f=24,s=1,v=1,i=7,q=2;/wAA\x1b\\\x1b[6;1H\x1bPq\"1;1;1;6#1;2;0;100;0#1~\x1b\\\x1b[8;1HIMAGES_READY\r\n")?;
    expect(&mut input, b"D")?;
    emit(b"\x1b_Ga=d,d=I,i=7,q=2\x1b\\\x1b[2J\x1b[HIMAGES_DELETED\r\n")?;
    expect(&mut input, b"C")?;
    emit(b"\x1b]52;c;Y2xpcGJvYXJkLXdyaXRl\x1b\\\x1b]52;c;?\x1b\\\x07\r\nCLIPBOARD_READY\r\n")?;
    expect(&mut input, CLIPBOARD_REPLY)?;
    emit(b"\r\nCLIPBOARD_OK\r\n")?;
    expect(&mut input, b"F")?;
    emit(format!("\x1b[2J\x1b[HFLOOD_READY pid={pid}\r\n").as_bytes())?;
    std::thread::sleep(Duration::from_millis(100));
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let flood = std::thread::spawn(move || {
        let block = "FLOOD_LINE obsolete output \x1b[38;2;31;91;191m蓝色\x1b[0m\r\n".repeat(256);
        while !flag.load(Ordering::Relaxed) {
            if emit(block.as_bytes()).is_err() {
                return;
            }
        }
    });
    expect(&mut input, &[3])?;
    stop.store(true, Ordering::Relaxed);
    flood
        .join()
        .map_err(|_| anyhow::anyhow!("fixture output thread panicked"))?;
    emit(format!("\x1b[2J\x1b[HLATEST_PROMPT pid={pid}\r\nCTRL_C_OK\r\n").as_bytes())?;
    expect(&mut input, b"S")?;
    emit(b"\x1b[2J\x1b[H")?;
    let mut random = 0x1234_5678_9abc_def0_u64;
    let mut large = Vec::new();
    for cell in 0..1500 {
        large.extend_from_slice(format!("\x1b]8;id=large-{cell};https://example.com/").as_bytes());
        for _ in 0..2048 {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            large.push(
                b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
                    [(random % 62) as usize],
            );
        }
        large.extend_from_slice(b"\x1b\\x");
    }
    large.extend_from_slice(b"\x1b]8;;\x1b\\\x1b]52;c;QklHX0RJU1BMQVlfUkVBRFk=\x1b\\");
    emit(&large)?;
    expect(&mut input, b"T")?;
    emit(format!("\x1b[2J\x1b[HSMALL_NEW_PROMPT pid={pid}\r\n").as_bytes())?;
    expect(&mut input, b"X")?;
    emit(b"\x1b[2J\x1b[HFINAL_SCREEN\r\n\x1b]133;D;23\x1b\\\x07")?;
    std::process::exit(23)
}

fn expect(input: &mut impl Read, expected: &[u8]) -> Result<()> {
    let mut received = vec![0; expected.len()];
    input.read_exact(&mut received)?;
    ensure!(
        received == expected,
        "PTY input mismatch: expected {} bytes beginning {:?}, received {:?}",
        expected.len(),
        &expected[..expected.len().min(80)],
        &received[..received.len().min(80)]
    );
    Ok(())
}

fn dimensions() -> Result<(u16, u16)> {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::uninit();
    ensure!(
        unsafe { libc::ioctl(0, libc::TIOCGWINSZ, size.as_mut_ptr()) } == 0,
        "cannot read PTY size"
    );
    let size = unsafe { size.assume_init() };
    Ok((size.ws_col, size.ws_row))
}

fn emit(bytes: &[u8]) -> Result<()> {
    let mut output = std::io::stdout().lock();
    output.write_all(bytes)?;
    output.flush()?;
    Ok(())
}
