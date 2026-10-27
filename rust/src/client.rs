use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use quinn::Connection;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::display::{Display, Graphics, PredictionPolicy};
use crate::input::{
    EscapeAction, EscapeSequence, InputDecoder, LocalEvent, LocalTerminal, TtyOutput,
    foreground_lost,
};
use crate::model::*;
use crate::transport::{self, ClientEndpoints, Incoming};

enum NetworkEvent {
    Opened {
        attempt: u64,
        result: Result<Link>,
    },
    Message {
        attempt: u64,
        message: ServerMessage,
    },
    Display {
        attempt: u64,
        incoming: Incoming,
    },
    Lost {
        attempt: u64,
        reason: String,
        fatal: bool,
    },
}

struct Link {
    connection: Connection,
    epoch: u64,
    committed: u64,
    effect_floor: u64,
    output: mpsc::Sender<ClientMessage>,
    tasks: Vec<JoinHandle<()>>,
    sent_through: u64,
    rendered_ack: u64,
    effects_ack: u64,
    resize_sent: u64,
    start: watch::Sender<bool>,
}

struct Connecting(JoinHandle<()>);

impl Drop for Connecting {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        self.connection.close(0u32.into(), b"client detached");
    }
}

async fn open_link(
    endpoint: ClientEndpoints,
    grant: Grant,
    attach: Attach,
    events: mpsc::Sender<NetworkEvent>,
) -> Result<Link> {
    let attempt = attach.attempt;
    let connection = transport::connect(&endpoint, &grant).await?;
    let (mut output, mut input) = connection.open_bi().await?;
    transport::send_message(&mut output, &ClientMessage::Attach(attach)).await?;
    let first = tokio::time::timeout(Duration::from_secs(10), transport::recv_message(&mut input))
        .await??;
    let Some(ServerMessage::Attached {
        epoch,
        committed,
        effect_floor,
    }) = first
    else {
        anyhow::bail!("server rejected session attachment: {first:?}");
    };
    let (send, mut receive) = mpsc::channel(64);
    let (start, waiting) = watch::channel(false);
    let mut tasks = Vec::new();
    let notify = events.clone();
    let sending_connection = connection.clone();
    let mut activated = waiting.clone();
    tasks.push(tokio::spawn(async move {
        if activated.wait_for(|ready| *ready).await.is_err() {
            return;
        }
        let result = async {
            while let Some(message) = receive.recv().await {
                transport::send_message(&mut output, &message).await?;
            }
            Result::<()>::Ok(())
        }
        .await;
        let fatal = fatal_close(&sending_connection);
        let _ = notify
            .send(NetworkEvent::Lost {
                attempt,
                reason: result
                    .err()
                    .map_or_else(|| "control writer ended".into(), |error| error.to_string()),
                fatal,
            })
            .await;
    }));
    let notify = events.clone();
    let reading_connection = connection.clone();
    let mut activated = waiting.clone();
    tasks.push(tokio::spawn(async move {
        if activated.wait_for(|ready| *ready).await.is_err() {
            return;
        }
        let result = async {
            while let Some(message) = transport::recv_message(&mut input).await? {
                notify
                    .send(NetworkEvent::Message { attempt, message })
                    .await?;
            }
            Result::<()>::Ok(())
        }
        .await;
        let fatal = fatal_close(&reading_connection);
        let _ = notify
            .send(NetworkEvent::Lost {
                attempt,
                reason: result.err().map_or_else(
                    || "server connection ended".into(),
                    |error| error.to_string(),
                ),
                fatal,
            })
            .await;
    }));
    for _ in 0..3 {
        let notify = events.clone();
        let receiving = connection.clone();
        let mut activated = waiting.clone();
        tasks.push(tokio::spawn(async move {
            if activated.wait_for(|ready| *ready).await.is_err() {
                return;
            }
            loop {
                match transport::receive_uni(&receiving).await {
                    Ok(incoming) => {
                        if notify
                            .send(NetworkEvent::Display { attempt, incoming })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        let fatal = fatal_close(&receiving);
                        let _ = notify
                            .send(NetworkEvent::Lost {
                                attempt,
                                reason: error.to_string(),
                                fatal,
                            })
                            .await;
                        break;
                    }
                }
            }
        }));
    }
    Ok(Link {
        connection,
        epoch,
        committed,
        effect_floor,
        output: send,
        tasks,
        sent_through: committed,
        rendered_ack: 0,
        effects_ack: 0,
        resize_sent: 0,
        start,
    })
}

fn fatal_close(connection: &Connection) -> bool {
    matches!(connection.close_reason(), Some(quinn::ConnectionError::ApplicationClosed(reason)) if reason.error_code == 1u32.into())
}

pub struct ClientOptions {
    pub graphics: Graphics,
    pub prediction: PredictionPolicy,
    pub clipboard_read: bool,
}

fn input_terminal() -> Result<AsyncFd<File>> {
    let mut name = [0 as libc::c_char; 1024];
    let error = unsafe { libc::ttyname_r(0, name.as_mut_ptr(), name.len()) };
    ensure!(
        error == 0,
        "resolve local terminal device: {}",
        io::Error::from_raw_os_error(error)
    );
    let path = unsafe { CStr::from_ptr(name.as_ptr()) };
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(std::ffi::OsStr::from_bytes(path.to_bytes()))
        .context("open local terminal input")?;
    unsafe { AsyncFd::register_with_interest(file, Interest::READABLE) }
        .context("watch local terminal input")
}

enum LocalUi {
    Foreground {
        display: Display<TtyOutput>,
        input: AsyncFd<File>,
        terminal: LocalTerminal,
    },
    Parked {
        display: Display<()>,
        interrupted: Option<LocalTerminal>,
    },
}

impl LocalUi {
    fn park(self) -> Self {
        match self {
            Self::Foreground {
                display,
                input,
                terminal,
            } => {
                let (display, writer) = display.into_cached();
                drop(writer);
                drop(input);
                Self::Parked {
                    display,
                    interrupted: Some(terminal),
                }
            }
            parked => parked,
        }
    }

    fn try_foreground(self) -> Result<Self> {
        if !LocalTerminal::foreground()? {
            return Ok(self.park());
        }
        let Self::Parked {
            display,
            mut interrupted,
        } = self
        else {
            return Ok(self);
        };
        let result = match interrupted.as_mut() {
            Some(terminal) => terminal.reassert_owned_modes(),
            None => LocalTerminal::enter().map(|terminal| interrupted = Some(terminal)),
        };
        if let Err(error) = result {
            if foreground_lost(&error) {
                return Ok(Self::Parked {
                    display,
                    interrupted,
                });
            }
            return Err(error);
        }
        Ok(Self::Foreground {
            display: display.bind(TtyOutput::new()?),
            input: input_terminal()?,
            terminal: interrupted.expect("foreground terminal"),
        })
    }

    fn suspend(self) -> Result<Self> {
        let Self::Foreground {
            mut display,
            input,
            mut terminal,
        } = self
        else {
            return Ok(self);
        };
        if let Err(error) = display.flush() {
            if foreground_lost(&error) {
                return Ok(Self::Foreground {
                    display,
                    input,
                    terminal,
                }
                .park());
            }
            return Err(error);
        }
        let (display, writer) = display.into_cached();
        drop(writer);
        drop(input);
        if let Err(error) = terminal.release() {
            if foreground_lost(&error) {
                return Ok(Self::Parked {
                    display,
                    interrupted: Some(terminal),
                });
            }
            return Err(error);
        }
        let parked = Self::Parked {
            display,
            interrupted: None,
        };
        ensure!(
            unsafe { libc::kill(libc::getpid(), libc::SIGSTOP) } == 0,
            "suspend local client: {}",
            io::Error::last_os_error()
        );
        Ok(parked)
    }
}

enum SessionAction {
    Continue,
    Suspend,
    Detach,
    Exit(i32),
}

fn read_size(size: &mut Size, generation: &mut u64) -> Result<()> {
    let current = LocalTerminal::size()?;
    if *size != current {
        *size = current;
        *generation = generation
            .checked_add(1)
            .context("resize sequence exhausted")?;
    }
    Ok(())
}

pub async fn run(endpoint: ClientEndpoints, grant: Grant, options: ClientOptions) -> Result<i32> {
    let mut resize_signal = signal(SignalKind::window_change())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut continued = signal(SignalKind::from_raw(libc::SIGCONT))?;
    let mut display = Display::new(io::sink(), options.graphics, options.prediction);
    display.write_status("Connecting.")?;
    let mut ui = LocalUi::Parked {
        display: display.into_cached().0,
        interrupted: None,
    };
    let mut decoder = InputDecoder::new();
    let (send, mut receive) = mpsc::channel(4);
    let mut timer = tokio::time::interval(Duration::from_millis(16));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut link: Option<Link> = None;
    let mut connecting: Option<Connecting> = None;
    let mut attempt = 0;
    let mut retry_at = Instant::now();
    let mut failures = 0u32;
    let mut sequence = 0;
    let mut committed = 0;
    let mut pending = BTreeMap::<u64, (InputRecord, usize)>::new();
    let mut pending_bytes = 0usize;
    let mut captured = VecDeque::<LocalEvent>::new();
    let mut ready_input = VecDeque::<InputEvent>::new();
    let mut escape = EscapeSequence::new();
    let mut help_visible = false;
    let mut resources = BTreeMap::<Digest, Resource>::new();
    let mut pending_frame: Option<Frame> = None;
    let mut effects = BTreeMap::<u64, Effect>::new();
    let mut clipboard_queries = VecDeque::<(u64, String, Instant)>::new();
    let mut resize_generation = 1u64;
    let mut size = LocalTerminal::size()?;
    let mut last_escape = Instant::now();
    let mut remote_exit: Option<(i32, u64, u64)> = None;
    let mut resync_pending = false;
    let mut requested_resources = Instant::now() - Duration::from_secs(1);
    let mut buffer = [0u8; 256];
    loop {
        let was_foreground = matches!(ui, LocalUi::Foreground { .. });
        ui = ui.try_foreground()?;
        let LocalUi::Foreground {
            display,
            input: tty,
            terminal,
        } = &mut ui
        else {
            if was_foreground {
                resync_pending = true;
            }
            tokio::select! {
                biased;
                _ = terminate.recv() => break,
                _ = interrupt.recv() => break,
                _ = hangup.recv() => break,
                _ = continued.recv() => (),
                _ = resize_signal.recv() => (),
                _ = timer.tick() => (),
            }
            continue;
        };
        let result: Result<SessionAction> = async {
            if !was_foreground {
                read_size(&mut size, &mut resize_generation)?;
                display.redraw_cached(size)?;
            }
            tokio::select! {
                _ = terminate.recv() => return Ok(SessionAction::Detach),
                _ = interrupt.recv() => return Ok(SessionAction::Detach),
                _ = hangup.recv() => return Ok(SessionAction::Detach),
                _ = resize_signal.recv() => {
                    read_size(&mut size, &mut resize_generation)?;
                }
                _ = continued.recv() => {
                    terminal.reassert_owned_modes()?;
                    read_size(&mut size, &mut resize_generation)?;
                    display.redraw_cached(size)?;
                }
                event = receive.recv() => {
                    match event.context("network tasks ended")? {
                        NetworkEvent::Opened { attempt: incoming_attempt, result } if incoming_attempt == attempt => {
                            connecting.take();
                            match result {
                                Ok(new_link) => {
                                    ensure!(new_link.committed >= committed && new_link.committed <= sequence, "server input commit is inconsistent");
                                    committed = new_link.committed;
                                    pending.retain(|seq, (_, bytes)| {
                                        if *seq <= committed { pending_bytes -= *bytes; false } else { true }
                                    });
                                    display.set_epoch(new_link.epoch);
                                    display.set_effect_floor(new_link.effect_floor);
                                    effects.retain(|id, _| *id > new_link.effect_floor);
                                    let _ = new_link.start.send(true);
                                    link = Some(new_link);
                                    resync_pending = false;
                                    failures = 0;
                                    display.write_status("")?;
                                }
                                Err(error) => {
                                    failures = failures.saturating_add(1);
                                    let delay = Duration::from_millis((250u64 << failures.min(4)).min(5000));
                                    retry_at = Instant::now() + delay;
                                    display.write_status(&format!("Ctrl-^ . to detach. Connection unavailable, retrying. {error:#}"))?;
                                }
                            }
                        }
                        NetworkEvent::Message { attempt: incoming_attempt, message } if incoming_attempt == attempt && link.is_some() => {
                            let active = link.as_ref().expect("active link");
                            match message {
                                ServerMessage::Committed { epoch, through } if epoch == active.epoch => {
                                    ensure!(through >= committed && through <= sequence, "invalid server input commit");
                                    committed = through;
                                    pending.retain(|seq, (_, bytes)| {
                                        if *seq <= committed { pending_bytes -= *bytes; false } else { true }
                                    });
                                }
                                ServerMessage::Effects { epoch, floor, items } if epoch == active.epoch => {
                                    display.set_effect_floor(floor);
                                    effects.retain(|id, _| *id > floor);
                                    for effect in items {
                                        if effect.id > display.effects_applied() { effects.entry(effect.id).or_insert(effect); }
                                    }
                                    ensure!(effects.len() <= MAX_EFFECTS, "pending effects exceed limit");
                                }
                                ServerMessage::Exit { epoch, code, revision, effects: through } if epoch == active.epoch => {
                                    remote_exit = Some((code, revision, through));
                                }
                                ServerMessage::Error { message } => anyhow::bail!("server: {message}"),
                                _ => (),
                            }
                        }
                        NetworkEvent::Display { attempt: incoming_attempt, incoming } if incoming_attempt == attempt && link.is_some() => {
                            match incoming {
                                Incoming::Frame(frame) => {
                                    let active = link.as_ref().expect("active link");
                                    if frame.epoch == active.epoch && frame.revision > display.revision()
                                        && pending_frame.as_ref().is_none_or(|pending| frame.revision > pending.revision) {
                                        if frame.base.is_some_and(|base| base != display.revision()) {
                                            resync_pending = true;
                                        } else { pending_frame = Some(frame); }
                                    }
                                }
                                Incoming::Resource(resource) => {
                                    resource.validate()?;
                                    let desired: BTreeSet<_> = pending_frame.as_ref().into_iter().flat_map(|frame| frame.resources.iter().map(|meta| meta.digest)).collect();
                                    let total = resources.values().map(|resource| resource.rgba.len()).sum::<usize>();
                                    if total + resource.rgba.len() > MAX_RESOURCES {
                                        resources.retain(|digest, _| desired.contains(digest));
                                    }
                                    let total = resources.values().map(|resource| resource.rgba.len()).sum::<usize>();
                                    if resources.contains_key(&resource.meta.digest) || total + resource.rgba.len() <= MAX_RESOURCES {
                                        resources.insert(resource.meta.digest, resource);
                                    }
                                }
                            }
                        }
                        NetworkEvent::Lost { attempt: incoming_attempt, reason, fatal } if incoming_attempt == attempt && link.is_some() => {
                            if fatal { anyhow::bail!("remote session ended: {reason}"); }
                            link.take();
                            pending_frame = None;
                            resources.clear();
                            retry_at = Instant::now();
                            display.write_status(&format!("Ctrl-^ . to detach. Connection interrupted, reconnecting. {reason}"))?;
                        }
                        _ => (),
                    }
                }
                ready = tty.readable(), if pending_bytes < MAX_PENDING_INPUT - 64 * 1024 && captured.is_empty() && ready_input.is_empty() => {
                    let mut ready = ready?;
                    let result = ready.try_io(|fd| {
                        use std::io::Read;
                        fd.get_ref().read(&mut buffer)
                    });
                    match result {
                        Ok(Ok(0)) => return Ok(SessionAction::Detach),
                        Ok(Ok(count)) => {
                            captured.extend(decoder.feed(&buffer[..count])?);
                            last_escape = Instant::now();
                        }
                        Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => (),
                        Ok(Err(error)) => return Err(error).context("read local terminal"),
                        Err(_) => (),
                    }
                }
                _ = timer.tick() => {
                    display.tick()?;
                    if decoder.has_pending_escape() && last_escape.elapsed() >= Duration::from_millis(30) {
                        captured.extend(decoder.flush_escape());
                    }
                }
            }
            loop {
                if let Some(event) = ready_input.front() {
                    let bytes = serde_json::to_vec(event)?.len();
                    ensure!(
                        bytes <= MAX_PENDING_INPUT,
                        "input event exceeds pending input limit"
                    );
                    if pending_bytes + bytes > MAX_PENDING_INPUT {
                        break;
                    }
                    let event = ready_input.pop_front().expect("ready input");
                    sequence = sequence
                        .checked_add(1)
                        .context("input sequence exhausted")?;
                    pending.insert(
                        sequence,
                        (
                            InputRecord {
                                seq: sequence,
                                event,
                            },
                            bytes,
                        ),
                    );
                    pending_bytes += bytes;
                    display.predict(sequence, &pending[&sequence].0.event)?;
                    continue;
                }
                let Some(event) = captured.pop_front() else {
                    break;
                };
                match event {
                    LocalEvent::Input(event) => match escape.feed(event) {
                        EscapeAction::Send(events) => {
                            let clear_help = help_visible && !events.is_empty();
                            ready_input.extend(events);
                            if clear_help {
                                help_visible = false;
                                display.write_status("")?;
                            }
                        }
                        EscapeAction::Detach => return Ok(SessionAction::Detach),
                        EscapeAction::Suspend => return Ok(SessionAction::Suspend),
                        EscapeAction::Close => {
                            if let Some(active) = &link {
                                let _ = tokio::time::timeout(
                                    Duration::from_secs(1),
                                    active.output.send(ClientMessage::Close {
                                        epoch: active.epoch,
                                    }),
                                )
                                .await;
                            }
                        }
                        EscapeAction::Help => {
                            display.write_status(
                                "Ctrl-^ . detach; Ctrl-^ Ctrl-Z suspend; Ctrl-^ q end session; Ctrl-^ ^ send Ctrl-^.",
                            )?;
                            help_visible = true;
                        }
                    },
                    LocalEvent::Clipboard { selection, data } => {
                        let response = clipboard_queries
                            .iter()
                            .position(|(_, wanted, _)| wanted == &selection)
                            .and_then(|index| clipboard_queries.remove(index))
                            .map(|(query, _, _)| InputEvent::QueryResult { query, data });
                        if let Some(response) = response {
                            ready_input.push_back(response);
                        }
                    }
                }
            }
            if let Some(frame) = pending_frame.as_ref()
                && frame.resources.iter().all(|meta| {
                    resources
                        .get(&meta.digest)
                        .is_some_and(|resource| resource.meta == *meta)
                })
            {
                let desired: BTreeSet<_> = frame.resources.iter().map(|meta| meta.digest).collect();
                if !display.install(frame, &resources)? && display.needs_checkpoint() {
                    resync_pending = true;
                }
                resources.retain(|digest, _| desired.contains(digest));
                pending_frame = None;
            }
            let ready_effects: Vec<_> = effects
                .values()
                .take_while(|effect| effect.after <= display.revision())
                .cloned()
                .collect();
            for effect in ready_effects {
                let response = display.apply_effect(&effect, options.clipboard_read)?;
                if let Some(event) = response {
                    captured.push_back(LocalEvent::Input(event));
                } else if let EffectKind::ClipboardRead { query, selection } = &effect.kind {
                    clipboard_queries.push_back((*query, selection.clone(), Instant::now()));
                }
                effects.remove(&effect.id);
            }
            while clipboard_queries
                .front()
                .is_some_and(|(_, _, time)| time.elapsed() > Duration::from_secs(2))
            {
                let (query, _, _) = clipboard_queries
                    .pop_front()
                    .expect("expired clipboard query");
                captured.push_back(LocalEvent::Input(InputEvent::QueryResult {
                    query,
                    data: None,
                }));
            }
            if let Some((code, revision, effects)) = remote_exit
                && display.revision() >= revision
                && display.effects_applied() >= effects
            {
                if let Some(active) = &link {
                    active
                        .output
                        .send(ClientMessage::ExitAck {
                            epoch: active.epoch,
                        })
                        .await?;
                    let _ =
                        tokio::time::timeout(Duration::from_secs(1), active.connection.closed()).await;
                }
                return Ok(SessionAction::Exit(code));
            }
            if let Some(active) = link.as_mut() {
                if requested_resources.elapsed() >= Duration::from_millis(500)
                    && let Some(frame) = &pending_frame
                {
                    let missing: Vec<_> = frame
                        .resources
                        .iter()
                        .filter(|meta| !resources.contains_key(&meta.digest))
                        .take(128)
                        .map(|meta| meta.digest)
                        .collect();
                    if !missing.is_empty()
                        && active
                            .output
                            .try_send(ClientMessage::MissingResources {
                                epoch: active.epoch,
                                digests: missing,
                            })
                            .is_ok()
                    {
                        requested_resources = Instant::now();
                    }
                }
                if resync_pending
                    && active
                        .output
                        .try_send(ClientMessage::Resync {
                            epoch: active.epoch,
                        })
                        .is_ok()
                {
                    resync_pending = false;
                }
                if active.rendered_ack < display.revision()
                    && active
                        .output
                        .try_send(ClientMessage::Rendered {
                            epoch: active.epoch,
                            revision: display.revision(),
                        })
                        .is_ok()
                {
                    active.rendered_ack = display.revision();
                }
                if active.effects_ack < display.effects_applied()
                    && active
                        .output
                        .try_send(ClientMessage::EffectsApplied {
                            epoch: active.epoch,
                            through: display.effects_applied(),
                        })
                        .is_ok()
                {
                    active.effects_ack = display.effects_applied();
                }
                for (_, (record, _)) in pending.range((active.sent_through + 1)..).take(8) {
                    if active
                        .output
                        .try_send(ClientMessage::Input {
                            epoch: active.epoch,
                            record: record.clone(),
                        })
                        .is_err()
                    {
                        break;
                    }
                    active.sent_through = record.seq;
                }
                if active.resize_sent < resize_generation
                    && active.sent_through >= sequence
                    && active
                        .output
                        .try_send(ClientMessage::Resize {
                            epoch: active.epoch,
                            generation: resize_generation,
                            after: sequence,
                            size,
                        })
                        .is_ok()
                {
                    active.resize_sent = resize_generation;
                }
            } else if connecting.is_none() && Instant::now() >= retry_at {
                attempt = attempt
                    .checked_add(1)
                    .context("attachment attempts exhausted")?;
                let attach = Attach {
                    version: PROTOCOL_VERSION,
                    session: grant.session.clone(),
                    attempt,
                    committed,
                    rendered: display.revision(),
                    effects: display.effects_applied(),
                };
                let endpoint = endpoint.clone();
                let grant = grant.clone();
                let notify = send.clone();
                connecting = Some(Connecting(tokio::spawn(async move {
                    let result = open_link(endpoint, grant, attach, notify.clone()).await;
                    let _ = notify.send(NetworkEvent::Opened { attempt, result }).await;
                })));
            }
            Ok(SessionAction::Continue)
        }.await;
        match result {
            Ok(SessionAction::Continue) => (),
            Ok(SessionAction::Suspend) => ui = ui.suspend()?,
            Ok(SessionAction::Detach) => break,
            Ok(SessionAction::Exit(code)) => return Ok(code),
            Err(error) if foreground_lost(&error) => {
                ui = ui.park();
                resync_pending = true;
            }
            Err(error) => return Err(error),
        }
    }

    if let Some(active) = &link {
        let _ = active.output.try_send(ClientMessage::Detach {
            epoch: active.epoch,
        });
    }
    drop(connecting);
    endpoint.close(0u32.into(), b"client detached");
    Ok(0)
}
