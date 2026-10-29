use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};

const SLOTS: usize = 64;
const STAGES: usize = ALL_STAGES.len();
const STALL_MS: u64 = 1000;
const REPEAT_MS: u64 = 5000;
const LOG_LIMIT: usize = 16 * 1024 * 1024;
static GLOBAL: OnceLock<Arc<State>> = OnceLock::new();

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Client,
    Server,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(usize)]
pub enum Stage {
    Bootstrap,
    Resolve,
    Connect,
    Attach,
    ControlSend,
    ControlWrite,
    ControlPayloadRead,
    EventQueueSend,
    PtyWrite,
    InputEncode,
    TerminalParse,
    Snapshot,
    FrameDiff,
    FrameEncode,
    FrameSend,
    ResourceSend,
    DisplayPayloadRead,
    FrameDecode,
    InstallFrame,
    TtyWrite,
    TtyFlush,
}

const ALL_STAGES: &[Stage] = &[
    Stage::Bootstrap,
    Stage::Resolve,
    Stage::Connect,
    Stage::Attach,
    Stage::ControlSend,
    Stage::ControlWrite,
    Stage::ControlPayloadRead,
    Stage::EventQueueSend,
    Stage::PtyWrite,
    Stage::InputEncode,
    Stage::TerminalParse,
    Stage::Snapshot,
    Stage::FrameDiff,
    Stage::FrameEncode,
    Stage::FrameSend,
    Stage::ResourceSend,
    Stage::DisplayPayloadRead,
    Stage::FrameDecode,
    Stage::InstallFrame,
    Stage::TtyWrite,
    Stage::TtyFlush,
];

#[derive(Clone, Copy, Default, Debug, Serialize)]
pub struct Context {
    pub attempt: u64,
    pub epoch: u64,
    pub sequence: u64,
    pub revision: u64,
    pub bytes: u64,
}

#[derive(Clone, Copy, Default, Serialize)]
pub struct ProgressSnapshot {
    pub connected: bool,
    pub foreground: bool,
    pub attempt: u64,
    pub epoch: u64,
    pub sequence: u64,
    pub committed: u64,
    pub revision: u64,
    pub pending_input_bytes: u64,
    pub pty_queued_bytes: u64,
    pub control_queue: u64,
    pub event_queue: u64,
    pub missing_resources: u64,
    pub pending_frame: u64,
    pub pty_written_bytes: u64,
    pub cancellations: u64,
}

impl ProgressSnapshot {
    fn fields(self) -> [u64; 15] {
        [
            u64::from(self.connected),
            u64::from(self.foreground),
            self.attempt,
            self.epoch,
            self.sequence,
            self.committed,
            self.revision,
            self.pending_input_bytes,
            self.pty_queued_bytes,
            self.control_queue,
            self.event_queue,
            self.missing_resources,
            self.pending_frame,
            self.pty_written_bytes,
            self.cancellations,
        ]
    }

    fn from_fields(v: [u64; 15]) -> Self {
        Self {
            connected: v[0] != 0,
            foreground: v[1] != 0,
            attempt: v[2],
            epoch: v[3],
            sequence: v[4],
            committed: v[5],
            revision: v[6],
            pending_input_bytes: v[7],
            pty_queued_bytes: v[8],
            control_queue: v[9],
            event_queue: v[10],
            missing_resources: v[11],
            pending_frame: v[12],
            pty_written_bytes: v[13],
            cancellations: v[14],
        }
    }
}

struct Slot<const N: usize> {
    generation: AtomicU64,
    fields: [AtomicU64; N],
}

impl<const N: usize> Slot<N> {
    fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            fields: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    fn publish(&self, generation: u64, fields: [u64; N]) {
        for (target, value) in self.fields.iter().zip(fields) {
            target.store(value, Ordering::SeqCst);
        }
        self.generation.store(generation, Ordering::SeqCst);
    }

    fn read(&self) -> Option<(u64, [u64; N])> {
        let before = self.generation.load(Ordering::SeqCst);
        if before < 2 || before & 1 != 0 {
            return None;
        }
        let fields = std::array::from_fn(|i| self.fields[i].load(Ordering::SeqCst));
        (self.generation.load(Ordering::SeqCst) == before).then_some((before, fields))
    }
}

struct StageStats {
    count: AtomicU64,
    total_us: AtomicU64,
    max_us: AtomicU64,
    bytes: AtomicU64,
}

impl StageStats {
    fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            total_us: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }
}

struct State {
    start: Instant,
    role: Role,
    running: AtomicBool,
    stop: AtomicBool,
    serial: AtomicU64,
    slots: [Slot<7>; SLOTS],
    progress: Slot<15>,
    high_water: [AtomicU64; 5],
    stats: [StageStats; STAGES],
    lost: AtomicU64,
    queue_full: AtomicU64,
    sender: mpsc::SyncSender<Event>,
}

#[derive(Clone, Copy)]
struct Active {
    id: u64,
    stage: Stage,
    started_us: u64,
    context: Context,
}

enum Event {
    Ended(Active, u64),
    Wake,
}

pub struct Diagnostics {
    state: Arc<State>,
    done: mpsc::Receiver<()>,
}

pub struct Operation {
    active: Option<(&'static State, usize, Active)>,
}

pub fn enabled() -> bool {
    state().is_some()
}

fn state() -> Option<&'static State> {
    GLOBAL
        .get()
        .map(Arc::as_ref)
        .filter(|state| state.running.load(Ordering::Relaxed))
}

pub fn create_path(role: Role) -> Result<PathBuf> {
    let label = match role {
        Role::Client => "client",
        Role::Server => "server",
    };
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory =
        std::env::temp_dir().join(format!("mosh-rs-{label}-{}-{stamp}", std::process::id()));
    std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
    Ok(directory.join("diagnostics.jsonl"))
}

impl State {
    fn new(role: Role) -> (Self, mpsc::Receiver<Event>) {
        let (sender, receiver) = mpsc::sync_channel(128);
        let state = State {
            start: Instant::now(),
            role,
            running: AtomicBool::new(true),
            stop: AtomicBool::new(false),
            serial: AtomicU64::new(2),
            slots: std::array::from_fn(|_| Slot::new()),
            progress: Slot::new(),
            high_water: std::array::from_fn(|_| AtomicU64::new(0)),
            stats: std::array::from_fn(|_| StageStats::new()),
            lost: AtomicU64::new(0),
            queue_full: AtomicU64::new(0),
            sender,
        };
        (state, receiver)
    }
}

pub fn start(role: Role, path: &Path) -> Result<Diagnostics> {
    ensure!(GLOBAL.get().is_none(), "diagnostics already started");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .context("create diagnostics file")?;
    ensure!(
        file.metadata()?.is_file(),
        "diagnostics destination is not a file"
    );
    let (state, receiver) = State::new(role);
    let state = Arc::new(state);
    let (finished, done) = mpsc::channel();
    let monitor = state.clone();
    GLOBAL
        .set(state.clone())
        .map_err(|_| anyhow::anyhow!("diagnostics already started"))?;
    if let Err(error) = std::thread::Builder::new()
        .name("mosh-diagnostics".into())
        .spawn(move || {
            run_monitor(&monitor, receiver, file);
            let _ = finished.send(());
        })
    {
        state.running.store(false, Ordering::Relaxed);
        return Err(error).context("start diagnostics monitor");
    }
    Ok(Diagnostics { state, done })
}

impl Drop for Diagnostics {
    fn drop(&mut self) {
        self.state.running.store(false, Ordering::Relaxed);
        self.state.stop.store(true, Ordering::Relaxed);
        let _ = self.state.sender.try_send(Event::Wake);
        let _ = self.done.recv_timeout(Duration::from_millis(250));
    }
}

pub fn operation(stage: Stage, context: Context) -> Operation {
    let Some(state) = state() else {
        return Operation { active: None };
    };
    for (index, slot) in state.slots.iter().enumerate() {
        if slot
            .generation
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let id = state.serial.fetch_add(2, Ordering::Relaxed);
            let started_us = elapsed_us(state);
            slot.publish(
                id,
                [
                    stage as u64,
                    started_us,
                    context.attempt,
                    context.epoch,
                    context.sequence,
                    context.revision,
                    context.bytes,
                ],
            );
            return Operation {
                active: Some((
                    state,
                    index,
                    Active {
                        id,
                        stage,
                        started_us,
                        context,
                    },
                )),
            };
        }
    }
    state.lost.fetch_add(1, Ordering::Relaxed);
    Operation { active: None }
}

impl Drop for Operation {
    fn drop(&mut self) {
        let Some((state, index, active)) = self.active.take() else {
            return;
        };
        let micros = elapsed_us(state).saturating_sub(active.started_us);
        let elapsed = micros / 1000;
        let stats = &state.stats[active.stage as usize];
        stats.count.fetch_add(1, Ordering::Relaxed);
        stats.total_us.fetch_add(micros, Ordering::Relaxed);
        stats.max_us.fetch_max(micros, Ordering::Relaxed);
        stats
            .bytes
            .fetch_add(active.context.bytes, Ordering::Relaxed);
        state.slots[index].generation.store(0, Ordering::SeqCst);
        if elapsed >= 100
            && state
                .sender
                .try_send(Event::Ended(active, elapsed))
                .is_err()
        {
            state.lost.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub fn progress(snapshot: ProgressSnapshot) {
    let Some(state) = state() else {
        return;
    };
    let previous = state.progress.generation.load(Ordering::SeqCst);
    if previous == 1
        || state
            .progress
            .generation
            .compare_exchange(previous, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
    {
        state.lost.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let generation = state.serial.fetch_add(2, Ordering::Relaxed);
    state.progress.publish(generation, snapshot.fields());
    for (peak, value) in state.high_water.iter().zip([
        snapshot.pending_input_bytes,
        snapshot.pty_queued_bytes,
        snapshot.control_queue,
        snapshot.event_queue,
        snapshot.missing_resources,
    ]) {
        peak.fetch_max(value, Ordering::Relaxed);
    }
}

pub fn queue_full() {
    if let Some(state) = state() {
        state.queue_full.fetch_add(1, Ordering::Relaxed);
    }
}

fn elapsed_ms(state: &State) -> u64 {
    elapsed_us(state) / 1000
}

fn elapsed_us(state: &State) -> u64 {
    state.start.elapsed().as_micros().min(u128::from(u64::MAX)) as u64
}

struct Log<W> {
    writer: W,
    remaining: usize,
}

impl<W: Write> Log<W> {
    fn write(&mut self, state: &State, mut value: Value) -> io::Result<()> {
        if self.remaining == 0 {
            return Ok(());
        }
        value["schema"] = json!(1);
        value["role"] = json!(state.role);
        value["pid"] = json!(std::process::id());
        value["elapsed_ms"] = json!(elapsed_ms(state));
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        if bytes.len() + 256 > self.remaining {
            let marker = format!(
                "{{\"schema\":1,\"role\":{},\"pid\":{},\"elapsed_ms\":{},\"event\":\"budget_exhausted\"}}\n",
                serde_json::to_string(&state.role)?,
                std::process::id(),
                elapsed_ms(state)
            );
            if marker.len() <= self.remaining {
                self.writer.write_all(marker.as_bytes())?;
            }
            self.remaining = 0;
        } else {
            self.writer.write_all(&bytes)?;
            self.remaining -= bytes.len();
        }
        Ok(())
    }
}

#[derive(Default)]
struct Pending {
    key: (u64, u64, u64),
    since: u64,
    reported: Option<u64>,
    active: bool,
}

enum PendingKind {
    Input,
    Pty,
    Resources,
}

impl Pending {
    fn observe<W: Write>(
        &mut self,
        state: &State,
        log: &mut Log<W>,
        kind: PendingKind,
        snapshot: ProgressSnapshot,
        now: u64,
    ) -> io::Result<u64> {
        let (stage, key, pending) = match kind {
            PendingKind::Input => (
                "input_commit",
                (snapshot.epoch, snapshot.committed, 0),
                snapshot.foreground && snapshot.sequence > snapshot.committed,
            ),
            PendingKind::Pty => (
                "pty_write",
                (snapshot.epoch, snapshot.pty_written_bytes, 0),
                snapshot.pty_queued_bytes > 0,
            ),
            PendingKind::Resources => (
                "missing_resources",
                (
                    snapshot.epoch,
                    snapshot.pending_frame,
                    snapshot.missing_resources,
                ),
                snapshot.foreground && snapshot.missing_resources > 0,
            ),
        };
        if !pending || !self.active || key != self.key {
            if self.reported.take().is_some() {
                log.write(
                    state,
                    json!({"event":"resumed", "stage":stage,
                    "duration_ms":now.saturating_sub(self.since), "progress":snapshot}),
                )?;
            }
            self.key = key;
            self.since = now;
        }
        self.active = pending;
        let age = if pending {
            now.saturating_sub(self.since)
        } else {
            0
        };
        if age >= STALL_MS && self.reported.is_none_or(|last| now - last >= REPEAT_MS) {
            log.write(
                state,
                json!({"event":"waiting", "stage":stage, "duration_ms":age, "progress":snapshot}),
            )?;
            self.reported = Some(now);
        }
        Ok(age)
    }
}

fn run_monitor(state: &State, receiver: mpsc::Receiver<Event>, writer: impl Write) {
    let _ = monitor_loop(state, receiver, writer);
    state.running.store(false, Ordering::Relaxed);
}

fn monitor_loop(
    state: &State,
    receiver: mpsc::Receiver<Event>,
    writer: impl Write,
) -> io::Result<()> {
    let mut log = Log {
        writer,
        remaining: LOG_LIMIT,
    };
    let mut observed: [Option<(Active, u64)>; SLOTS] = [None; SLOTS];
    let mut pending = [Pending::default(), Pending::default(), Pending::default()];
    let mut last_summary = 0;
    let mut last_progress = ProgressSnapshot::default();
    let mut last_slow = [0; STAGES];
    let mut queue_full = 0;
    let mut ages = [0; 3];
    log.write(
        state,
        json!({"event":"start", "limit_bytes":LOG_LIMIT, "stall_ms":STALL_MS}),
    )?;
    loop {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Ended(active, duration)) => {
                if duration >= STALL_MS
                    || elapsed_ms(state) - last_slow[active.stage as usize] >= 1000
                {
                    log.write(
                        state,
                        json!({"event":"slow", "stage":active.stage,
                        "duration_ms":duration, "context":active.context}),
                    )?;
                    last_slow[active.stage as usize] = elapsed_ms(state);
                }
            }
            Ok(Event::Wake) | Err(mpsc::RecvTimeoutError::Timeout) => (),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        let now = elapsed_ms(state);
        let fullness = state.queue_full.load(Ordering::Relaxed);
        if fullness != queue_full {
            log.write(
                state,
                json!({"event":"queue_full", "count":fullness, "progress":last_progress}),
            )?;
            queue_full = fullness;
        }
        for (index, slot) in state.slots.iter().enumerate() {
            let active = slot.read().map(|(id, v)| Active {
                id,
                stage: ALL_STAGES[v[0] as usize],
                started_us: v[1],
                context: Context {
                    attempt: v[2],
                    epoch: v[3],
                    sequence: v[4],
                    revision: v[5],
                    bytes: v[6],
                },
            });
            if let Some((old, _)) = observed[index]
                && active.is_none_or(|current| current.id != old.id)
            {
                log.write(state, json!({"event":"resumed", "stage":old.stage,
                    "duration_ms":now.saturating_sub(old.started_us / 1000), "context":old.context}))?;
                observed[index] = None;
            }
            if let Some(active) = active {
                let age = now.saturating_sub(active.started_us / 1000);
                if age >= STALL_MS
                    && observed[index].is_none_or(|(_, last)| now - last >= REPEAT_MS)
                {
                    log.write(
                        state,
                        json!({"event":"waiting", "stage":active.stage,
                        "duration_ms":age, "context":active.context, "progress":last_progress}),
                    )?;
                    observed[index] = Some((active, now));
                }
            }
        }
        if let Some((_, fields)) = state.progress.read() {
            let snapshot = ProgressSnapshot::from_fields(fields);
            if snapshot.connected != last_progress.connected
                || snapshot.foreground != last_progress.foreground
                || snapshot.attempt != last_progress.attempt
                || snapshot.epoch != last_progress.epoch
            {
                log.write(state, json!({"event":"transition", "progress":snapshot}))?;
            }
            ages = [
                pending[0].observe(state, &mut log, PendingKind::Input, snapshot, now)?,
                pending[1].observe(state, &mut log, PendingKind::Pty, snapshot, now)?,
                pending[2].observe(state, &mut log, PendingKind::Resources, snapshot, now)?,
            ];
            last_progress = snapshot;
        }
        if now - last_summary >= REPEAT_MS || state.stop.load(Ordering::Relaxed) {
            let stats: Vec<_> = ALL_STAGES.iter().enumerate().filter_map(|(index, stage)| {
                    let s = &state.stats[index];
                    let count = s.count.load(Ordering::Relaxed);
                    (count > 0).then(|| json!({"stage":stage, "count":count,
                        "total_us":s.total_us.load(Ordering::Relaxed), "max_us":s.max_us.load(Ordering::Relaxed),
                        "bytes":s.bytes.load(Ordering::Relaxed)}))
                }).collect();
            log.write(state, json!({"event":"summary", "progress":last_progress, "pending_age_ms":ages,
                    "high_water":state.high_water.iter().map(|v|v.load(Ordering::Relaxed)).collect::<Vec<_>>(),
                    "dropped":state.lost.load(Ordering::Relaxed), "stages":stats}))?;
            last_summary = now;
        }
        if state.stop.load(Ordering::Relaxed) || log.remaining == 0 {
            log.write(
                state,
                json!({"event":"stop", "dropped":state.lost.load(Ordering::Relaxed)}),
            )?;
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(bytes: &[u8]) -> Vec<Value> {
        std::str::from_utf8(bytes)
            .unwrap()
            .split_inclusive('\n')
            .filter(|line| line.ends_with('\n'))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn live_waits_capacity_and_recovery() {
        let ready = Arc::new(std::sync::Barrier::new(2));
        let attempts: Vec<_> = (0..2)
            .map(|_| {
                let ready = ready.clone();
                std::thread::spawn(move || {
                    let path = create_path(Role::Client).unwrap();
                    ready.wait();
                    let result = start(Role::Client, &path);
                    (path, result)
                })
            })
            .collect();
        let mut results: Vec<_> = attempts
            .into_iter()
            .map(|task| task.join().unwrap())
            .collect();
        assert_eq!(
            results.iter().filter(|(_, result)| result.is_ok()).count(),
            1
        );
        let winner = results
            .iter()
            .position(|(_, result)| result.is_ok())
            .unwrap();
        let (path, monitor) = results.swap_remove(winner);
        let monitor = monitor.unwrap();
        let context = Context {
            attempt: 3,
            epoch: 4,
            sequence: 5,
            revision: 6,
            bytes: 7,
        };
        let held = operation(Stage::TtyFlush, context);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let entries = records(&std::fs::read(&path).unwrap());
            if entries
                .iter()
                .any(|row| row["event"] == "waiting" && row["stage"] == "tty_flush")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the monitor must report a live synchronous wait"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let occupied: Vec<_> = (1..SLOTS)
            .map(|_| operation(Stage::FrameEncode, context))
            .collect();
        let exhausted = operation(Stage::FrameEncode, context);
        assert!(exhausted.active.is_none());
        drop(occupied);
        drop(held);
        for _ in 0..1000 {
            drop(operation(Stage::FrameEncode, context));
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let _held = operation(Stage::ControlWrite, context);
            tokio::time::sleep(Duration::from_millis(1200)).await;
            let entries = records(&std::fs::read(&path).unwrap());
            assert!(
                entries
                    .iter()
                    .any(|row| row["event"] == "waiting" && row["stage"] == "control_write")
            );
        });
        drop(monitor);
        assert!(!enabled());
        assert!(operation(Stage::Connect, context).active.is_none());
        let entries = records(&std::fs::read(&path).unwrap());
        assert!(entries.iter().any(|row| row["event"] == "resumed"
            && row["stage"] == "tty_flush"
            && row["context"]["epoch"] == 4));
        assert!(
            entries
                .iter()
                .any(|row| row["event"] == "summary" && row["dropped"].as_u64().unwrap_or(0) >= 1)
        );
        assert!(entries.iter().any(|row| row["event"] == "stop"));
        for (unused, _) in results {
            if unused.exists() {
                assert_eq!(std::fs::metadata(&unused).unwrap().len(), 0);
                std::fs::remove_file(&unused).unwrap();
            }
            std::fs::remove_dir(unused.parent().unwrap()).unwrap();
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn slot_reuse_never_mixes_generations() {
        let slot = Arc::new(Slot::<7>::new());
        let writing = slot.clone();
        let task = std::thread::spawn(move || {
            for value in 1..20_000 {
                writing.generation.store(1, Ordering::SeqCst);
                writing.publish(value * 2, [value; 7]);
            }
        });
        while !task.is_finished() {
            if let Some((generation, fields)) = slot.read() {
                assert_eq!(fields, [generation / 2; 7]);
            }
        }
        task.join().unwrap();
        assert_eq!(slot.read().unwrap().1, [19_999; 7]);
    }

    #[test]
    fn budget_preserves_a_final_marker_and_stops() {
        let (state, _) = State::new(Role::Server);
        let mut log = Log {
            writer: Vec::new(),
            remaining: 600,
        };
        for _ in 0..30 {
            log.write(&state, json!({"event":"summary", "count":123}))
                .unwrap();
        }
        assert!(log.writer.len() <= 600);
        let entries = records(&log.writer);
        assert_eq!(entries.last().unwrap()["event"], "budget_exhausted");
        assert_eq!(
            entries
                .iter()
                .filter(|row| row["event"] == "budget_exhausted")
                .count(),
            1
        );
        assert_eq!(log.remaining, 0);
    }

    #[test]
    fn write_failure_disables_only_diagnostics() {
        struct FailedWriter;
        impl Write for FailedWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::Other.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (state, receiver) = State::new(Role::Client);
        run_monitor(&state, receiver, FailedWriter);
        assert!(!state.running.load(Ordering::Relaxed));
        assert!(!state.stop.load(Ordering::Relaxed));
    }

    #[test]
    fn pending_age_restarts_on_actual_progress() {
        let (state, _) = State::new(Role::Client);
        let mut log = Log {
            writer: Vec::new(),
            remaining: LOG_LIMIT,
        };
        let mut pending = Pending::default();
        let mut snapshot = ProgressSnapshot {
            foreground: true,
            epoch: 1,
            sequence: 10,
            ..Default::default()
        };
        assert_eq!(
            pending
                .observe(&state, &mut log, PendingKind::Input, snapshot, 10)
                .unwrap(),
            0
        );
        assert_eq!(
            pending
                .observe(&state, &mut log, PendingKind::Input, snapshot, 1010)
                .unwrap(),
            1000
        );
        snapshot.committed = 3;
        assert_eq!(
            pending
                .observe(&state, &mut log, PendingKind::Input, snapshot, 1011)
                .unwrap(),
            0
        );
        snapshot.committed = 10;
        assert_eq!(
            pending
                .observe(&state, &mut log, PendingKind::Input, snapshot, 9011)
                .unwrap(),
            0
        );
        let entries = records(&log.writer);
        assert_eq!(
            entries
                .iter()
                .filter(|row| row["event"] == "waiting")
                .count(),
            1
        );
        assert_eq!(
            entries
                .iter()
                .filter(|row| row["event"] == "resumed")
                .count(),
            1
        );
    }
}
