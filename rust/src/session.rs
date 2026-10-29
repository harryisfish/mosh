use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use quinn::{Connection, VarInt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::diagnostics::{self, Context as DiagnosticContext, ProgressSnapshot, Stage};
use crate::model::*;
use crate::pty::Pty;
use crate::state;
use crate::terminal::{ModelChange, TerminalModel};
use crate::transport;

enum Event {
    Attach {
        request: Attach,
        connection: Connection,
        output: mpsc::Sender<ServerMessage>,
        flushed: oneshot::Receiver<()>,
    },
    Message {
        epoch: u64,
        message: ClientMessage,
    },
    Disconnected {
        epoch: u64,
    },
    DisplaySent {
        epoch: u64,
        revision: u64,
        success: bool,
    },
    ResourceSent {
        epoch: u64,
        digest: Digest,
        id: u64,
        success: bool,
    },
}

struct PendingWrite {
    input: Option<u64>,
    bytes: Vec<u8>,
    position: usize,
}

struct ResourceTask {
    id: u64,
    task: JoinHandle<()>,
}

struct AcceptTask(JoinHandle<()>);

impl Drop for AcceptTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct ListenerGuard(Option<transport::ServerListeners>);

impl ListenerGuard {
    fn close(mut self, code: VarInt, reason: &[u8]) {
        if let Some(listeners) = self.0.take() {
            listeners.close(code, reason);
        }
    }
}

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        if let Some(listeners) = &self.0 {
            listeners.close(0_u32.into(), b"session cancelled");
        }
    }
}

struct Attachment {
    epoch: u64,
    connection: Connection,
    output: mpsc::Sender<ServerMessage>,
    confirmed: Option<(u64, Arc<Screen>)>,
    sent: BTreeMap<u64, Arc<Screen>>,
    last_sent: u64,
    display: Option<JoinHandle<()>>,
    display_started: Instant,
    display_bytes: usize,
    checked_revision: u64,
    cancellations: u8,
    resources: BTreeMap<Digest, ResourceTask>,
    known_resources: BTreeSet<Digest>,
    effects_ack: u64,
    effects_sent: u64,
    floor_sent: u64,
    exit_sent: bool,
}

impl Attachment {
    fn new(
        epoch: u64,
        connection: Connection,
        output: mpsc::Sender<ServerMessage>,
        effects_ack: u64,
    ) -> Self {
        Self {
            epoch,
            connection,
            output,
            confirmed: None,
            sent: BTreeMap::new(),
            last_sent: 0,
            display: None,
            display_started: Instant::now(),
            display_bytes: 0,
            checked_revision: 0,
            cancellations: 0,
            resources: BTreeMap::new(),
            known_resources: BTreeSet::new(),
            effects_ack,
            effects_sent: effects_ack,
            floor_sent: 0,
            exit_sent: false,
        }
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        if let Some(task) = self.display.take() {
            task.abort();
        }
        for task in self.resources.values() {
            task.task.abort();
        }
        self.connection.close(0u32.into(), b"attachment ended");
    }
}

pub struct SessionActor {
    prepared: PreparedSession,
}

struct PreparedSession {
    command: Vec<String>,
    locale_candidates: Vec<String>,
    size: Size,
    terminal: TerminalModel,
    session: String,
    events: mpsc::Receiver<Event>,
    sender: mpsc::Sender<Event>,
    deadline: tokio::time::Instant,
    idle: Duration,
}

struct FirstAttachment {
    attempt: u64,
    connection: Connection,
    output: mpsc::Sender<ServerMessage>,
    flushed: oneshot::Receiver<()>,
}

struct RunningSession {
    pty: Pty,
    terminal: TerminalModel,
    session: String,
    events: mpsc::Receiver<Event>,
    sender: mpsc::Sender<Event>,
    attachment: Option<Attachment>,
    attempt: u64,
    epoch: u64,
    accepted: u64,
    committed: u64,
    notified_commit: u64,
    digests: BTreeMap<u64, Digest>,
    writes: VecDeque<PendingWrite>,
    queued_bytes: usize,
    resize: Option<(u64, u64, u64, Size)>,
    resize_generation: u64,
    revision: u64,
    echo_through: u64,
    latest: Option<Arc<Screen>>,
    dirty: bool,
    publish_allowed: bool,
    last_output: Instant,
    effects: VecDeque<Effect>,
    effect_bytes: usize,
    effect_floor: u64,
    effect_sequence: u64,
    queries: BTreeMap<u64, Instant>,
    disconnected_at: Instant,
    idle: Duration,
    exit: Option<(i32, Instant)>,
    eof: bool,
    fatal: bool,
    resource_sequence: u64,
    exit_acknowledged: bool,
    display_cancellations: u64,
}

impl SessionActor {
    pub fn new(request: &BootstrapRequest, session: String) -> Result<Self> {
        request.size.validate()?;
        ensure!(
            request.version == PROTOCOL_VERSION,
            "unsupported protocol version"
        );
        validate_locale_candidates(&request.locale_candidates)?;
        ensure!(
            (60..=604800).contains(&request.idle_seconds),
            "idle timeout must be 60..=604800 seconds"
        );
        let terminal = TerminalModel::new(request.size, request.clipboard_read)?;
        let (sender, events) = mpsc::channel(128);
        Ok(Self {
            prepared: PreparedSession {
                command: request.command.clone(),
                locale_candidates: request.locale_candidates.clone(),
                size: request.size,
                terminal,
                session,
                events,
                sender,
                deadline: tokio::time::Instant::now() + Duration::from_secs(60),
                idle: Duration::from_secs(request.idle_seconds),
            },
        })
    }

    pub fn run(
        self,
        listeners: transport::ServerListeners,
    ) -> impl std::future::Future<Output = Result<()>> {
        let accepting = listeners.clone();
        let listeners = ListenerGuard(Some(listeners));
        async move {
            let mut prepared = self.prepared;
            let sender = prepared.sender.clone();
            let connections = Arc::new(tokio::sync::Semaphore::new(8));
            let accept_task = AcceptTask(tokio::spawn(async move {
                while let Some(incoming) = accepting.accept().await {
                    let Ok(permit) = connections.clone().try_acquire_owned() else {
                        incoming.refuse();
                        continue;
                    };
                    let sender = sender.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        if let Ok(Ok(connection)) =
                            tokio::time::timeout(Duration::from_secs(10), incoming).await
                        {
                            let _ = serve_connection(connection, sender).await;
                        }
                    });
                }
            }));
            let result = async {
                let Some(first) = prepared.wait_first().await? else {
                    return Ok(());
                };
                let mut running = prepared.start(first).await?;
                let result = running.run_actor().await;
                running.pty.close();
                result
            }
            .await;
            drop(accept_task);
            if let Err(error) = &result {
                let reason: String = error.to_string().chars().take(240).collect();
                listeners.close(1u32.into(), reason.as_bytes());
            } else {
                listeners.close(0u32.into(), b"session ended");
            }
            result
        }
    }
}

fn valid_attachment(
    request: &Attach,
    session: &str,
    attempt: u64,
    committed: u64,
    effects: u64,
) -> bool {
    request.version == PROTOCOL_VERSION
        && request.session == session
        && request.attempt > attempt
        && request.committed <= committed
        && request.effects <= effects
}

fn reject_attachment(output: &mpsc::Sender<ServerMessage>, reason: &str) {
    let _ = output.try_send(ServerMessage::Error {
        message: reason.into(),
    });
}

impl PreparedSession {
    async fn wait_first(&mut self) -> Result<Option<FirstAttachment>> {
        loop {
            let event = tokio::select! {
                _ = tokio::time::sleep_until(self.deadline) => return Ok(None),
                event = self.events.recv() => event,
            };
            if tokio::time::Instant::now() >= self.deadline {
                return Ok(None);
            }
            let Some(event) = event else {
                return Ok(None);
            };
            if let Event::Attach {
                request,
                connection,
                output,
                flushed,
            } = event
            {
                if !valid_attachment(&request, &self.session, 0, 0, 0) {
                    reject_attachment(&output, "invalid or obsolete session attachment");
                    continue;
                }
                if connection.close_reason().is_some() || output.is_closed() {
                    continue;
                }
                return Ok(Some(FirstAttachment {
                    attempt: request.attempt,
                    connection,
                    output,
                    flushed,
                }));
            }
        }
    }

    async fn start(self, first: FirstAttachment) -> Result<RunningSession> {
        let pty = match Pty::spawn(&self.command, self.size, &self.locale_candidates).await {
            Ok(pty) => pty,
            Err(error) => {
                reject_attachment(
                    &first.output,
                    &format!("remote command startup failed: {error:#}"),
                );
                let _ = first.flushed.await;
                return Err(error);
            }
        };
        let mut running = RunningSession {
            pty,
            terminal: self.terminal,
            session: self.session,
            events: self.events,
            sender: self.sender,
            attachment: Some(Attachment::new(1, first.connection, first.output, 0)),
            attempt: first.attempt,
            epoch: 1,
            accepted: 0,
            committed: 0,
            notified_commit: 0,
            digests: BTreeMap::new(),
            writes: VecDeque::new(),
            queued_bytes: 0,
            resize: None,
            resize_generation: 0,
            revision: 0,
            echo_through: 0,
            latest: None,
            dirty: true,
            publish_allowed: true,
            last_output: Instant::now(),
            effects: VecDeque::new(),
            effect_bytes: 0,
            effect_floor: 0,
            effect_sequence: 0,
            queries: BTreeMap::new(),
            disconnected_at: Instant::now(),
            idle: self.idle,
            exit: None,
            eof: false,
            fatal: false,
            resource_sequence: 0,
            exit_acknowledged: false,
            display_cancellations: 0,
        };
        running.send(ServerMessage::Attached {
            epoch: 1,
            committed: 0,
            effect_floor: 0,
        });
        Ok(running)
    }
}

impl RunningSession {
    async fn run_actor(&mut self) -> Result<()> {
        let mut timer = tokio::time::interval(Duration::from_millis(16));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut buffer = vec![0; 32 * 1024];
        let mut pty_written_bytes = 0;
        loop {
            if diagnostics::enabled() {
                let attachment = self.attachment.as_ref();
                diagnostics::progress(ProgressSnapshot {
                    connected: attachment.is_some(),
                    foreground: true,
                    attempt: self.attempt,
                    epoch: self.epoch,
                    sequence: self.accepted,
                    committed: self.committed,
                    revision: self.revision,
                    pty_queued_bytes: self.queued_bytes as u64,
                    pty_written_bytes,
                    event_queue: self.events.len() as u64,
                    control_queue: attachment.map_or(0, |a| {
                        (a.output.max_capacity() - a.output.capacity()) as u64
                    }),
                    pending_frame: attachment
                        .filter(|a| a.display.is_some())
                        .map_or(0, |a| a.last_sent),
                    cancellations: self.display_cancellations,
                    ..Default::default()
                });
            }
            tokio::select! {
                event = self.events.recv() => {
                    if let Some(event) = event { self.event(event)?; }
                }
                _ = timer.tick() => {
                    if self.tick()? { return Ok(()); }
                }
                ready = self.pty.io.writable(), if !self.writes.is_empty() && self.exit.is_none() => {
                    let mut ready = ready?;
                    let write = self.writes.front_mut().expect("pending write");
                    let _operation = diagnostics::operation(Stage::PtyWrite, DiagnosticContext { attempt: self.attempt, epoch: self.epoch, sequence: write.input.unwrap_or(0), bytes: (write.bytes.len() - write.position) as u64, ..Default::default() });
                    let result = ready.try_io(|fd| Pty::write(fd.get_ref(), &write.bytes[write.position..]));
                    match result {
                        Ok(Ok(0)) => bail!("PTY accepted an empty write"),
                        Ok(Ok(count)) => {
                            write.position += count;
                            pty_written_bytes += count as u64;
                            self.queued_bytes -= count;
                            if write.position == write.bytes.len() { self.finish_write(); }
                        }
                        Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => (),
                        Ok(Err(error)) => return Err(error).context("write remote PTY"),
                        Err(_) => (),
                    }
                }
                ready = self.pty.io.readable(), if !self.eof => {
                    let mut ready = ready?;
                    match ready.try_io(|fd| Pty::read(fd.get_ref(), &mut buffer)) {
                        Ok(Ok(0)) => self.eof = true,
                        Ok(Ok(count)) => {
                            self.last_output = Instant::now();
                            self.echo_through = self.committed;
                            let _operation = diagnostics::operation(Stage::TerminalParse, DiagnosticContext { attempt: self.attempt, epoch: self.epoch, bytes: count as u64, ..Default::default() });
                            let change = self.terminal.advance(&buffer[..count])?;
                            self.change(change)?;
                            self.dirty = true;
                        }
                        Ok(Err(error)) if error.raw_os_error() == Some(libc::EIO) => self.eof = true,
                        Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => (),
                        Ok(Err(error)) => return Err(error).context("read remote PTY"),
                        Err(_) => (),
                    }
                }
            }
            tokio::task::yield_now().await;
        }
    }

    fn active(&self, epoch: u64) -> bool {
        self.attachment
            .as_ref()
            .is_some_and(|attachment| attachment.epoch == epoch)
    }

    fn event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Attach {
                request,
                connection,
                output,
                ..
            } => self.attach(request, connection, output)?,
            Event::Message { epoch, message } if self.active(epoch) => {
                if let Err(error) = self.message(epoch, message) {
                    eprintln!("mosh-rs-server: rejected session message: {error}");
                    if let Some(attachment) = &self.attachment {
                        let reason: String = error.to_string().chars().take(240).collect();
                        attachment.connection.close(1u32.into(), reason.as_bytes());
                    }
                    self.send(ServerMessage::Error {
                        message: error.to_string(),
                    });
                    if self.fatal {
                        return Err(error);
                    }
                    self.disconnect();
                }
            }
            Event::Disconnected { epoch } if self.active(epoch) => self.disconnect(),
            Event::DisplaySent {
                epoch,
                revision,
                success,
            } if self.active(epoch) => {
                let attachment = self.attachment.as_mut().expect("active attachment");
                if revision == attachment.last_sent {
                    attachment.display.take();
                    if success {
                        attachment.cancellations = 0;
                    } else {
                        self.disconnect();
                    }
                }
            }
            Event::ResourceSent {
                epoch,
                digest,
                id,
                success,
            } if self.active(epoch) => {
                let attachment = self.attachment.as_mut().expect("active attachment");
                if attachment
                    .resources
                    .get(&digest)
                    .is_none_or(|task| task.id != id)
                {
                    return Ok(());
                }
                attachment.resources.remove(&digest);
                if success {
                    attachment.known_resources.insert(digest);
                } else {
                    self.disconnect();
                }
            }
            _ => (),
        }
        Ok(())
    }

    fn attach(
        &mut self,
        request: Attach,
        connection: Connection,
        output: mpsc::Sender<ServerMessage>,
    ) -> Result<()> {
        if !valid_attachment(
            &request,
            &self.session,
            self.attempt,
            self.committed,
            self.effect_sequence,
        ) {
            reject_attachment(&output, "invalid or obsolete session attachment");
            return Ok(());
        }
        self.attachment.take();
        self.resize = None;
        self.attempt = request.attempt;
        self.epoch = self
            .epoch
            .checked_add(1)
            .context("attachment epoch exhausted")?;
        self.notified_commit = self.committed;
        let effects_ack = request.effects.max(self.effect_floor);
        self.attachment = Some(Attachment::new(self.epoch, connection, output, effects_ack));
        self.send(ServerMessage::Attached {
            epoch: self.epoch,
            committed: self.committed,
            effect_floor: self.effect_floor,
        });
        self.dirty = true;
        Ok(())
    }

    fn message(&mut self, epoch: u64, message: ClientMessage) -> Result<()> {
        match message {
            ClientMessage::Input {
                epoch: message_epoch,
                record,
            } => {
                ensure!(message_epoch == epoch, "input epoch mismatch");
                self.input(record)?;
            }
            ClientMessage::Rendered {
                epoch: message_epoch,
                revision,
            } => {
                ensure!(
                    message_epoch == epoch,
                    "display acknowledgement epoch mismatch"
                );
                let attachment = self.attachment.as_mut().expect("active attachment");
                if attachment
                    .confirmed
                    .as_ref()
                    .is_none_or(|(old, _)| revision > *old)
                    && let Some(screen) = attachment.sent.get(&revision).cloned()
                {
                    attachment.confirmed = Some((revision, screen));
                    attachment.known_resources = attachment
                        .confirmed
                        .as_ref()
                        .expect("confirmed screen")
                        .1
                        .resources
                        .keys()
                        .copied()
                        .collect();
                    attachment.sent.retain(|sent, _| *sent > revision);
                }
            }
            ClientMessage::Resync {
                epoch: message_epoch,
            } => {
                ensure!(message_epoch == epoch, "resync epoch mismatch");
                let attachment = self.attachment.as_mut().expect("active attachment");
                if let Some(task) = attachment.display.take() {
                    task.abort();
                }
                attachment.confirmed = None;
                attachment.last_sent = 0;
                attachment.known_resources.clear();
                self.dirty = true;
            }
            ClientMessage::MissingResources {
                epoch: message_epoch,
                digests,
            } => {
                ensure!(
                    message_epoch == epoch && digests.len() <= 128,
                    "invalid resource request"
                );
                let attachment = self.attachment.as_mut().expect("active attachment");
                for digest in digests {
                    attachment.known_resources.remove(&digest);
                }
            }
            ClientMessage::EffectsApplied {
                epoch: message_epoch,
                through,
            } => {
                ensure!(
                    message_epoch == epoch && through <= self.effect_sequence,
                    "invalid effect acknowledgement"
                );
                let attachment = self.attachment.as_mut().expect("active attachment");
                ensure!(
                    through <= attachment.effects_sent.max(self.effect_floor),
                    "acknowledgement for unsent effect"
                );
                attachment.effects_ack = attachment.effects_ack.max(through);
                let acknowledged = attachment.effects_ack;
                while self
                    .effects
                    .front()
                    .is_some_and(|effect| effect.id <= acknowledged)
                {
                    self.pop_effect();
                }
            }
            ClientMessage::Resize {
                epoch: message_epoch,
                generation,
                after,
                size,
            } => {
                ensure!(
                    message_epoch == epoch && after <= self.accepted,
                    "invalid resize barrier"
                );
                size.validate()?;
                if generation > self.resize_generation
                    && self.resize.is_none_or(|(_, old, _, _)| generation > old)
                {
                    self.resize = Some((epoch, generation, after, size));
                }
            }
            ClientMessage::Detach {
                epoch: message_epoch,
            } => {
                ensure!(message_epoch == epoch, "detach epoch mismatch");
                self.disconnect();
            }
            ClientMessage::Close {
                epoch: message_epoch,
            } => {
                ensure!(message_epoch == epoch, "close epoch mismatch");
                self.pty.close();
                self.exit = Some((129, Instant::now()));
                self.dirty = true;
            }
            ClientMessage::ExitAck {
                epoch: message_epoch,
            } => {
                ensure!(
                    message_epoch == epoch
                        && self
                            .attachment
                            .as_ref()
                            .is_some_and(|attachment| attachment.exit_sent),
                    "invalid exit acknowledgement"
                );
                self.exit_acknowledged = true;
            }
            ClientMessage::Attach(_) => bail!("attachment must be the first message"),
        }
        Ok(())
    }

    fn input(&mut self, record: InputRecord) -> Result<()> {
        let _operation = diagnostics::operation(
            Stage::InputEncode,
            DiagnosticContext {
                attempt: self.attempt,
                epoch: self.epoch,
                sequence: record.seq,
                ..Default::default()
            },
        );
        record.event.validate()?;
        if self.exit.is_some() {
            return Ok(());
        }
        ensure!(record.seq > 0, "input sequence must be positive");
        let bytes = serde_json::to_vec(&record.event)?;
        let digest = *blake3::hash(&bytes).as_bytes();
        if record.seq <= self.accepted {
            if let Some(previous) = self.digests.get(&record.seq) {
                ensure!(*previous == digest, "conflicting duplicate input");
            } else {
                ensure!(
                    record.seq <= self.committed,
                    "unknown pending duplicate input"
                );
            }
            self.notified_commit = self.notified_commit.min(self.committed.saturating_sub(1));
            return Ok(());
        }
        ensure!(record.seq == self.accepted + 1, "input sequence gap");
        ensure!(
            self.writes.len() < 4096 && self.queued_bytes + bytes.len() <= MAX_PENDING_INPUT,
            "pending input limit reached"
        );
        self.fatal = true;
        let encoded = self.terminal.encode_input(&record.event)?;
        self.accepted = record.seq;
        self.digests.insert(record.seq, digest);
        self.queue(Some(record.seq), encoded)?;
        self.fatal = false;
        if let InputEvent::QueryResult { query, .. } = &record.event {
            self.queries.remove(query);
        }
        self.dirty = true;
        Ok(())
    }

    fn queue(&mut self, input: Option<u64>, bytes: Vec<u8>) -> Result<()> {
        ensure!(
            self.queued_bytes + bytes.len() <= 2 * MAX_PENDING_INPUT && self.writes.len() < 8192,
            "PTY writer limit reached"
        );
        self.queued_bytes += bytes.len();
        self.writes.push_back(PendingWrite {
            input,
            bytes,
            position: 0,
        });
        while self
            .writes
            .front()
            .is_some_and(|write| write.bytes.is_empty())
        {
            self.finish_write();
        }
        Ok(())
    }

    fn finish_write(&mut self) {
        if let Some(write) = self.writes.pop_front()
            && let Some(seq) = write.input
        {
            self.committed = seq;
            let floor = self.committed.saturating_sub(4096);
            self.digests.retain(|seq, _| *seq >= floor);
        }
        while self
            .writes
            .front()
            .is_some_and(|write| write.bytes.is_empty())
        {
            let write = self.writes.pop_front().expect("empty pending write");
            if let Some(seq) = write.input {
                self.committed = seq;
            }
        }
    }

    fn change(&mut self, change: ModelChange) -> Result<()> {
        self.publish_allowed = change.publish_allowed;
        if !change.replies.is_empty() {
            self.queue(None, change.replies)?;
        }
        for kind in change.effects {
            let bytes = serde_json::to_vec(&kind)?.len();
            if let EffectKind::ClipboardRead { query, .. } = &kind {
                if self.queries.len() >= 32 || bytes > MAX_EFFECT_BYTES / 4 {
                    let response = self.terminal.encode_input(&InputEvent::QueryResult {
                        query: *query,
                        data: None,
                    })?;
                    self.queue(None, response)?;
                    continue;
                }
                self.queries.insert(*query, Instant::now());
            }
            if bytes > MAX_EFFECT_BYTES / 4 {
                continue;
            }
            self.effect_sequence = self
                .effect_sequence
                .checked_add(1)
                .context("effect sequence exhausted")?;
            self.effects.push_back(Effect {
                id: self.effect_sequence,
                after: self.revision + 1,
                kind,
            });
            self.effect_bytes += bytes;
            while self.effects.len() > MAX_EFFECTS || self.effect_bytes > MAX_EFFECT_BYTES {
                self.pop_effect();
            }
        }
        Ok(())
    }

    fn pop_effect(&mut self) {
        if let Some(effect) = self.effects.pop_front() {
            self.effect_bytes = self
                .effect_bytes
                .saturating_sub(serde_json::to_vec(&effect.kind).map_or(0, |bytes| bytes.len()));
            self.effect_floor = self.effect_floor.max(effect.id);
        }
    }

    fn send(&mut self, message: ServerMessage) {
        if self
            .attachment
            .as_ref()
            .is_some_and(|attachment| attachment.output.try_send(message).is_err())
        {
            diagnostics::queue_full();
            self.disconnect();
        }
    }

    fn disconnect(&mut self) {
        self.attachment.take();
        self.resize = None;
        self.disconnected_at = Instant::now();
    }

    fn tick(&mut self) -> Result<bool> {
        self.publish_allowed = self.terminal.publish_allowed();
        if let Some((epoch, generation, after, size)) = self.resize
            && after <= self.committed
        {
            ensure!(self.active(epoch), "obsolete pending resize");
            self.pty.resize(size)?;
            self.terminal.resize(size)?;
            self.resize_generation = generation;
            self.resize = None;
            self.dirty = true;
        }
        if self.exit.is_none()
            && let Some(code) = self.pty.poll_exit()?
        {
            self.exit = Some((code, Instant::now()));
            self.dirty = true;
        }
        let expired: Vec<_> = self
            .queries
            .iter()
            .filter(|(_, time)| time.elapsed() > Duration::from_secs(5))
            .map(|(id, _)| *id)
            .collect();
        for query in expired {
            self.queries.remove(&query);
            let response = self
                .terminal
                .encode_input(&InputEvent::QueryResult { query, data: None })?;
            self.queue(None, response)?;
        }
        if self.dirty
            && (self.publish_allowed || self.last_output.elapsed() > Duration::from_millis(150))
        {
            let _operation = diagnostics::operation(
                Stage::Snapshot,
                DiagnosticContext {
                    attempt: self.attempt,
                    epoch: self.epoch,
                    revision: self.revision + 1,
                    ..Default::default()
                },
            );
            let mut screen = self.terminal.snapshot()?;
            screen.prediction.echo_enabled = self.pty.echo_enabled();
            self.latest = Some(Arc::new(screen));
            self.revision = self
                .revision
                .checked_add(1)
                .context("display revision exhausted")?;
            self.dirty = false;
        }
        if self.committed != self.notified_commit {
            self.notified_commit = self.committed;
            self.send(ServerMessage::Committed {
                epoch: self.epoch,
                through: self.committed,
            });
        }
        self.publish()?;
        self.publish_effects()?;
        if let Some((code, exited)) = self.exit {
            let settled = self.eof || self.last_output.elapsed() > Duration::from_millis(100);
            if settled
                && self.attachment.as_ref().is_some_and(|attachment| {
                    !attachment.exit_sent
                        && attachment
                            .confirmed
                            .as_ref()
                            .is_some_and(|(revision, _)| *revision >= self.revision)
                        && attachment.effects_ack >= self.effect_sequence
                })
            {
                self.send(ServerMessage::Exit {
                    epoch: self.epoch,
                    code,
                    revision: self.revision,
                    effects: self.effect_sequence,
                });
                if let Some(attachment) = self.attachment.as_mut() {
                    attachment.exit_sent = true;
                }
            }
            if self.exit_acknowledged || exited.elapsed() > self.idle {
                return Ok(true);
            }
        }
        if self.attachment.is_none() && self.disconnected_at.elapsed() >= self.idle {
            return Ok(true);
        }
        Ok(false)
    }

    fn publish(&mut self) -> Result<()> {
        let Some(screen) = self.latest.as_ref() else {
            return Ok(());
        };
        let Some(attachment) = self.attachment.as_mut() else {
            return Ok(());
        };
        loop {
            let retained: BTreeMap<_, _> = attachment
                .sent
                .values()
                .chain(attachment.confirmed.iter().map(|(_, screen)| screen))
                .chain(std::iter::once(screen))
                .flat_map(|screen| {
                    screen
                        .resources
                        .iter()
                        .map(|(digest, resource)| (*digest, resource.rgba.len()))
                })
                .collect();
            if retained.values().sum::<usize>() <= 2 * MAX_RESOURCES {
                break;
            }
            if attachment.confirmed.take().is_none() && attachment.sent.pop_first().is_none() {
                break;
            }
        }
        let stable = self.last_output.elapsed() >= Duration::from_millis(75)
            && attachment.display_started.elapsed() >= Duration::from_millis(250)
            && attachment.cancellations == 0;
        let check_replacement = attachment.display.is_some()
            && (attachment.checked_revision < self.revision || stable)
            && attachment.display_started.elapsed() >= Duration::from_millis(75);
        if attachment.last_sent < self.revision
            && (attachment.display.is_none() || check_replacement)
        {
            if check_replacement {
                attachment.checked_revision = self.revision;
            }
            let frame = transport::EncodedFrame::new(&state::frame(
                attachment.epoch,
                self.revision,
                self.echo_through,
                screen,
                attachment
                    .confirmed
                    .as_ref()
                    .map(|(revision, screen)| (*revision, screen.as_ref())),
            )?)?;
            let bytes = frame.compressed_len();
            let smaller = attachment.display_bytes > 8192 && bytes < attachment.display_bytes / 2;
            if attachment.display.is_none() || smaller || stable {
                if let Some(task) = attachment.display.take() {
                    task.abort();
                    attachment.cancellations = attachment.cancellations.saturating_add(1);
                    self.display_cancellations += 1;
                }
                attachment.sent.insert(self.revision, screen.clone());
                while attachment.sent.len() > 2 {
                    attachment.sent.pop_first();
                }
                attachment.last_sent = self.revision;
                attachment.display_started = Instant::now();
                attachment.display_bytes = bytes;
                let connection = attachment.connection.clone();
                let epoch = attachment.epoch;
                let revision = self.revision;
                let sender = self.sender.clone();
                attachment.display = Some(tokio::spawn(async move {
                    let result = frame.send(&connection).await;
                    if let Err(error) = &result {
                        eprintln!("mosh-rs-server: display revision {revision} failed: {error:#}");
                    }
                    let success = result.is_ok();
                    let _ = sender
                        .send(Event::DisplaySent {
                            epoch,
                            revision,
                            success,
                        })
                        .await;
                }));
            }
        }
        let desired: BTreeSet<_> = attachment
            .sent
            .values()
            .chain(attachment.confirmed.iter().map(|(_, screen)| screen))
            .chain(std::iter::once(screen))
            .flat_map(|screen| screen.resources.keys().copied())
            .collect();
        attachment
            .known_resources
            .retain(|digest| desired.contains(digest));
        let obsolete: Vec<_> = attachment
            .resources
            .keys()
            .filter(|digest| !desired.contains(*digest))
            .copied()
            .collect();
        for digest in obsolete {
            if let Some(task) = attachment.resources.remove(&digest) {
                task.task.abort();
            }
        }
        for (digest, resource) in &screen.resources {
            if attachment.resources.len() >= 2 {
                break;
            }
            if attachment.known_resources.contains(digest)
                || attachment.resources.contains_key(digest)
            {
                continue;
            }
            let connection = attachment.connection.clone();
            let resource = resource.clone();
            let digest = *digest;
            let epoch = attachment.epoch;
            let sender = self.sender.clone();
            self.resource_sequence = self
                .resource_sequence
                .checked_add(1)
                .context("resource task sequence exhausted")?;
            let id = self.resource_sequence;
            attachment.resources.insert(
                digest,
                ResourceTask {
                    id,
                    task: tokio::spawn(async move {
                        let success = transport::send_resource(&connection, &resource)
                            .await
                            .is_ok();
                        let _ = sender
                            .send(Event::ResourceSent {
                                epoch,
                                digest,
                                id,
                                success,
                            })
                            .await;
                    }),
                },
            );
        }
        Ok(())
    }

    fn publish_effects(&mut self) -> Result<()> {
        let Some(attachment) = self.attachment.as_ref() else {
            return Ok(());
        };
        let mut bytes = 0;
        let items: Vec<_> = self
            .effects
            .iter()
            .filter(|effect| effect.id > attachment.effects_sent)
            .take_while(|effect| {
                bytes += serde_json::to_vec(effect).map_or(MAX_CONTROL, |data| data.len());
                bytes < MAX_CONTROL - 1024
            })
            .cloned()
            .collect();
        if items.is_empty() && attachment.floor_sent == self.effect_floor {
            return Ok(());
        }
        let through = items
            .last()
            .map_or(attachment.effects_sent, |effect| effect.id);
        self.send(ServerMessage::Effects {
            epoch: self.epoch,
            floor: self.effect_floor,
            items,
        });
        if let Some(attachment) = self.attachment.as_mut() {
            attachment.effects_sent = through;
            attachment.floor_sent = self.effect_floor;
        }
        Ok(())
    }
}

async fn serve_connection(connection: Connection, sender: mpsc::Sender<Event>) -> Result<()> {
    let (mut output, mut input) =
        tokio::time::timeout(Duration::from_secs(10), connection.accept_bi()).await??;
    let first = tokio::time::timeout(
        Duration::from_secs(10),
        transport::recv_message::<ClientMessage>(&mut input),
    )
    .await??;
    let Some(ClientMessage::Attach(request)) = first else {
        bail!("missing attachment request");
    };
    let (send, mut receive) = mpsc::channel(64);
    let (flushed, flushing) = oneshot::channel();
    sender
        .send(Event::Attach {
            request,
            connection: connection.clone(),
            output: send,
            flushed: flushing,
        })
        .await?;
    let epoch = match receive
        .recv()
        .await
        .context("session ended before attachment")?
    {
        message @ ServerMessage::Attached { epoch, .. } => {
            let sent = transport::send_message(&mut output, &message).await;
            let _ = flushed.send(());
            sent?;
            epoch
        }
        message => {
            let reason: String = match &message {
                ServerMessage::Error { message } => message.as_str(),
                _ => "session attachment rejected",
            }
            .chars()
            .take(240)
            .collect();
            let sent = tokio::time::timeout(Duration::from_secs(10), async {
                transport::send_message(&mut output, &message).await?;
                output.finish()?;
                output.stopped().await?;
                Result::<()>::Ok(())
            })
            .await;
            let _ = flushed.send(());
            connection.close(1u32.into(), reason.as_bytes());
            sent??;
            return Ok(());
        }
    };
    let writer_connection = connection.clone();
    let writer = tokio::spawn(async move {
        while let Some(message) = receive.recv().await {
            let _operation = diagnostics::operation(
                Stage::ControlSend,
                DiagnosticContext {
                    epoch,
                    ..Default::default()
                },
            );
            if !matches!(
                tokio::time::timeout(
                    Duration::from_secs(10),
                    transport::send_message(&mut output, &message)
                )
                .await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
        writer_connection.close(0u32.into(), b"control writer ended");
    });
    let result = async {
        while let Some(message) = transport::recv_message(&mut input).await? {
            let _operation = diagnostics::operation(
                Stage::EventQueueSend,
                DiagnosticContext {
                    epoch,
                    ..Default::default()
                },
            );
            sender.send(Event::Message { epoch, message }).await?;
        }
        Result::<()>::Ok(())
    }
    .await;
    writer.abort();
    let _ = sender.send(Event::Disconnected { epoch }).await;
    connection.close(0u32.into(), b"control reader ended");
    result
}
