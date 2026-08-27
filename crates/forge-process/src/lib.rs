//! Process lifecycle management over PTYs.
//!
//! This crate knows nothing about AI agents, terminals, or user interfaces. It
//! owns processes and publishes what they do. Both the terminal pane and the
//! agent are ordinary subscribers with no privileged access, which is what
//! makes it structurally impossible for the agent to run something the user
//! cannot see.
//!
//! Two invariants are load-bearing and are documented where they are enforced:
//! signals go to the process *group* (never the bare pid), and an interrupt
//! that originates from a human keystroke uses a different mechanism than one
//! that originates from a button or a tool call. See [`ProcessManager::interrupt`]
//! and [`ProcessManager::write_stdin`].

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::broadcast;

/// Opaque, Forge-issued process handle.
///
/// Deliberately *not* the OS pid: pids are recycled by the kernel, and a record
/// must remain queryable after the process exits so the agent can still ask how
/// a build finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProcessId(pub u64);

impl std::fmt::Display for ProcessId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "proc-{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Starting,
    Running,
    Stopping,
    Exited,
    /// Forge could not run the command at all (exec failure, missing binary).
    ///
    /// Distinct from `Exited` with a non-zero code. A failing build is a normal,
    /// informative result the agent should reason about; a failed *spawn* is a
    /// Forge-level problem. Collapsing the two teaches the agent to treat build
    /// failures as tooling errors.
    Failed,
    Interrupted,
}

impl ProcessState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Exited | Self::Failed | Self::Interrupted)
    }
}

#[derive(Debug, Clone)]
pub struct ProcessSpec {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<std::path::PathBuf>,
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
    pub label: Option<String>,
}

impl ProcessSpec {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            // Never leave this at 0x0: an unset winsize makes the child read
            // `stty size` as "0 0" and full-screen programs render wrongly.
            rows: 24,
            cols: 80,
            label: None,
        }
    }

    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn cwd(mut self, p: impl Into<std::path::PathBuf>) -> Self {
        self.cwd = Some(p.into());
        self
    }

    pub fn size(mut self, rows: u16, cols: u16) -> Self {
        self.rows = rows;
        self.cols = cols;
        self
    }

    pub fn label(mut self, l: impl Into<String>) -> Self {
        self.label = Some(l.into());
        self
    }
}

#[derive(Debug, Clone)]
pub struct ProcessRecord {
    pub id: ProcessId,
    pub pid: Option<u32>,
    pub command: String,
    pub args: Vec<String>,
    pub label: Option<String>,
    pub cwd: Option<std::path::PathBuf>,
    pub state: ProcessState,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub started_at: Option<Instant>,
    pub ended_at: Option<Instant>,
    pub rows: u16,
    pub cols: u16,
    pub bytes_out: u64,
}

impl ProcessRecord {
    pub fn runtime(&self) -> Option<Duration> {
        let start = self.started_at?;
        Some(self.ended_at.unwrap_or_else(Instant::now).duration_since(start))
    }
}

#[derive(Debug, Clone)]
pub enum ProcessEvent {
    /// Raw bytes exactly as the child wrote them. No interpretation happens
    /// here; VT emulation is the terminal layer's job.
    Output(Arc<Vec<u8>>),
    StateChanged(ProcessState),
    Exited { code: Option<i32>, signal: Option<i32> },
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("no such process: {0}")]
    NoSuchProcess(ProcessId),
    #[error("process {0} has already terminated")]
    AlreadyTerminated(ProcessId),
    #[error("pty error: {0}")]
    Pty(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

type Result<T> = std::result::Result<T, ProcessError>;

/// Everything the manager holds for one live process.
struct ProcessEntry {
    record: ProcessRecord,
    /// Bounded retained output.
    ///
    /// Two jobs: it closes the race where a subscriber attaching after `start`
    /// would miss the first bytes, and it backs the agent's ability to read a
    /// *window* of a long build's output. Bounded on purpose — a kernel build
    /// emits gigabytes, and the oldest bytes are dropped rather than growing
    /// without limit. `dropped_bytes` makes that loss visible instead of silent.
    buffer: std::collections::VecDeque<u8>,
    dropped_bytes: u64,
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    events: broadcast::Sender<ProcessEvent>,
}

/// Owns every process Forge has started.
#[derive(Clone)]
pub struct ProcessManager {
    inner: Arc<Mutex<HashMap<ProcessId, ProcessEntry>>>,
    next_id: Arc<AtomicU64>,
    event_capacity: usize,
    buffer_capacity: usize,
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            // Bounded on purpose. A subscriber that cannot keep up gets a
            // visible `Lagged` error rather than silently unbounded memory.
            // The PTY reader must never be slowed by a slow consumer.
            event_capacity: 2048,
            // 1 MiB of retained bytes per process. Enough to reconstruct a
            // screen and give the agent recent context; small enough that a
            // hundred concurrent builds cannot exhaust memory.
            buffer_capacity: 1024 * 1024,
        }
    }

    /// Start a process and return immediately.
    ///
    /// This returns a handle, **not** output. Output is obtained by
    /// [`subscribe`](Self::subscribe), and the terminal pane and the agent
    /// subscribe independently. This asymmetry is the whole design.
    pub fn start(&self, spec: ProcessSpec) -> Result<ProcessId> {
        let id = ProcessId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, _rx) = broadcast::channel(self.event_capacity);

        let mut record = ProcessRecord {
            id,
            pid: None,
            command: spec.command.clone(),
            args: spec.args.clone(),
            label: spec.label.clone(),
            cwd: spec.cwd.clone(),
            state: ProcessState::Starting,
            exit_code: None,
            signal: None,
            started_at: Some(Instant::now()),
            ended_at: None,
            rows: spec.rows,
            cols: spec.cols,
            bytes_out: 0,
        };

        let pty_system = native_pty_system();
        let size = PtySize {
            rows: spec.rows,
            cols: spec.cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        // portable-pty performs setsid() and TIOCSCTTY in the child on unix, so
        // the child becomes its own session and process-group leader with the
        // slave as its controlling terminal. That is what makes group signalling
        // safe: we can never accidentally signal Forge itself.
        let pair = pty_system
            .openpty(size)
            .map_err(|e| ProcessError::Pty(e.to_string()))?;

        let mut cmd = CommandBuilder::new(&spec.command);
        for a in &spec.args {
            cmd.arg(a);
        }
        if let Some(cwd) = &spec.cwd {
            cmd.cwd(cwd);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        let child = match pair.slave.spawn_command(cmd) {
            Ok(c) => c,
            Err(e) => {
                // Could not run it at all -> Failed, never Exited.
                record.state = ProcessState::Failed;
                record.ended_at = Some(Instant::now());
                let _ = tx.send(ProcessEvent::StateChanged(ProcessState::Failed));
                self.inner.lock().unwrap().insert(
                    id,
                    ProcessEntry {
                        record,
                        buffer: Default::default(),
                        dropped_bytes: 0,
                        master: None,
                        writer: None,
                        child: None,
                        events: tx,
                    },
                );
                return Err(ProcessError::Pty(e.to_string()));
            }
        };

        // Drop the slave in the parent. If we hold it, the master never sees
        // EOF when the child exits and the reader thread hangs forever.
        drop(pair.slave);

        record.pid = child.process_id();
        record.state = ProcessState::Running;

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| ProcessError::Pty(e.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| ProcessError::Pty(e.to_string()))?;

        self.inner.lock().unwrap().insert(
            id,
            ProcessEntry {
                record,
                buffer: std::collections::VecDeque::with_capacity(8192),
                dropped_bytes: 0,
                master: Some(pair.master),
                writer: Some(writer),
                child: Some(child),
                events: tx.clone(),
            },
        );
        let _ = tx.send(ProcessEvent::StateChanged(ProcessState::Running));

        self.spawn_reader(id, reader, tx.clone());
        self.spawn_waiter(id, tx);

        Ok(id)
    }

    /// Drain the PTY master as fast as the kernel delivers.
    ///
    /// This thread must never be throttled by the UI. A PTY applies backpressure
    /// to the writer, so a slow reader does not drop output — it makes the child
    /// block in `write(2)`, which would slow the build itself down. Rendering is
    /// decoupled downstream, on a fixed cadence.
    fn spawn_reader(
        &self,
        id: ProcessId,
        mut reader: Box<dyn Read + Send>,
        tx: broadcast::Sender<ProcessEvent>,
    ) {
        let inner = Arc::clone(&self.inner);
        let cap = self.buffer_capacity;
        std::thread::Builder::new()
            .name(format!("forge-pty-read-{}", id.0))
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Ok(mut g) = inner.lock() {
                                if let Some(e) = g.get_mut(&id) {
                                    e.record.bytes_out += n as u64;
                                    e.buffer.extend(&buf[..n]);
                                    if e.buffer.len() > cap {
                                        let excess = e.buffer.len() - cap;
                                        e.buffer.drain(..excess);
                                        e.dropped_bytes += excess as u64;
                                    }
                                }
                            }
                            // A send error only means nobody is listening yet.
                            // Keep draining regardless.
                            let _ = tx.send(ProcessEvent::Output(Arc::new(buf[..n].to_vec())));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            })
            .expect("spawn pty reader thread");
    }

    /// Reap the child and publish its exit exactly once.
    fn spawn_waiter(&self, id: ProcessId, tx: broadcast::Sender<ProcessEvent>) {
        let inner = Arc::clone(&self.inner);
        std::thread::Builder::new()
            .name(format!("forge-pty-wait-{}", id.0))
            .spawn(move || {
                let mut child = match inner.lock().unwrap().get_mut(&id).and_then(|e| e.child.take())
                {
                    Some(c) => c,
                    None => return,
                };
                let status = child.wait();

                let mut g = inner.lock().unwrap();
                let Some(entry) = g.get_mut(&id) else { return };

                let code = status.as_ref().ok().map(|s| s.exit_code() as i32);
                entry.record.exit_code = code;
                entry.record.ended_at = Some(Instant::now());

                // A process we were deliberately stopping stays INTERRUPTED, so
                // the agent can tell "the user stopped this build" apart from
                // "this build failed on its own".
                entry.record.state = match entry.record.state {
                    ProcessState::Stopping | ProcessState::Interrupted => ProcessState::Interrupted,
                    _ => ProcessState::Exited,
                };
                let final_state = entry.record.state;
                let signal = entry.record.signal;

                // Release the master last: closing the final fd referring to it
                // is what SIGHUPs the foreground group.
                entry.master = None;
                entry.writer = None;
                drop(g);

                let _ = tx.send(ProcessEvent::StateChanged(final_state));
                let _ = tx.send(ProcessEvent::Exited { code, signal });
            })
            .expect("spawn pty waiter thread");
    }

    /// Subscribe to future events only.
    ///
    /// Prefer [`attach`](Self::attach) for anything that renders output: this
    /// races with the reader thread and can miss bytes already delivered.
    pub fn subscribe(&self, id: ProcessId) -> Result<broadcast::Receiver<ProcessEvent>> {
        let g = self.inner.lock().unwrap();
        g.get(&id)
            .map(|e| e.events.subscribe())
            .ok_or(ProcessError::NoSuchProcess(id))
    }

    /// Atomically take a snapshot of retained output *and* a receiver for
    /// everything after it.
    ///
    /// Both are taken under one lock, so no byte is lost between them and none
    /// is delivered twice. This is how a terminal pane attaches to a process
    /// that is already running, and how the agent catches up on a build that
    /// started before it asked.
    pub fn attach(&self, id: ProcessId) -> Result<(Vec<u8>, broadcast::Receiver<ProcessEvent>)> {
        let g = self.inner.lock().unwrap();
        let e = g.get(&id).ok_or(ProcessError::NoSuchProcess(id))?;
        Ok((e.buffer.iter().copied().collect(), e.events.subscribe()))
    }

    /// Retained output, plus how many bytes were dropped from the front.
    ///
    /// A non-zero drop count must be surfaced to the user rather than hidden;
    /// silently truncated build output is worse than none.
    pub fn output_snapshot(&self, id: ProcessId) -> Result<(Vec<u8>, u64)> {
        let g = self.inner.lock().unwrap();
        let e = g.get(&id).ok_or(ProcessError::NoSuchProcess(id))?;
        Ok((e.buffer.iter().copied().collect(), e.dropped_bytes))
    }

    pub fn get(&self, id: ProcessId) -> Result<ProcessRecord> {
        let g = self.inner.lock().unwrap();
        g.get(&id).map(|e| e.record.clone()).ok_or(ProcessError::NoSuchProcess(id))
    }

    pub fn list(&self) -> Vec<ProcessRecord> {
        let g = self.inner.lock().unwrap();
        let mut v: Vec<_> = g.values().map(|e| e.record.clone()).collect();
        v.sort_by_key(|r| r.id);
        v
    }

    /// Write bytes to the PTY master, exactly as a terminal would.
    ///
    /// This is the path for **human keystrokes**, including Ctrl-C as `0x03`.
    /// Writing `0x03` here lets the line discipline decide what it means, which
    /// is correct whether the child is in cooked or raw mode — the same as a
    /// real terminal. To interrupt *programmatically*, use
    /// [`interrupt`](Self::interrupt) instead; a raw-mode child such as `vim`
    /// receives `0x03` as ordinary data and ignores it entirely.
    pub fn write_stdin(&self, id: ProcessId, data: &[u8]) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        let entry = g.get_mut(&id).ok_or(ProcessError::NoSuchProcess(id))?;
        if entry.record.state.is_terminal() {
            return Err(ProcessError::AlreadyTerminated(id));
        }
        let w = entry.writer.as_mut().ok_or(ProcessError::AlreadyTerminated(id))?;
        w.write_all(data)?;
        w.flush()?;
        Ok(())
    }

    pub fn resize(&self, id: ProcessId, rows: u16, cols: u16) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        let entry = g.get_mut(&id).ok_or(ProcessError::NoSuchProcess(id))?;
        entry.record.rows = rows;
        entry.record.cols = cols;
        if let Some(m) = entry.master.as_ref() {
            // TIOCSWINSZ, which immediately delivers SIGWINCH to the child's
            // foreground process group.
            m.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
                .map_err(|e| ProcessError::Pty(e.to_string()))?;
        }
        Ok(())
    }

    /// Send a signal to the child's **process group**.
    ///
    /// Always the group, never the bare pid: a build is typically
    /// `sh -c` → `bitbake` → compilers, and signalling only the direct child
    /// leaves the actual work running. The child is a session leader (see
    /// [`start`](Self::start)), so its group contains its descendants and
    /// cannot contain Forge.
    #[cfg(unix)]
    pub fn signal(&self, id: ProcessId, sig: nix::sys::signal::Signal) -> Result<()> {
        use nix::sys::signal::killpg;
        use nix::unistd::Pid;

        let mut g = self.inner.lock().unwrap();
        let entry = g.get_mut(&id).ok_or(ProcessError::NoSuchProcess(id))?;
        if entry.record.state.is_terminal() {
            return Err(ProcessError::AlreadyTerminated(id));
        }
        let pid = entry.record.pid.ok_or(ProcessError::AlreadyTerminated(id))?;
        entry.record.signal = Some(sig as i32);

        killpg(Pid::from_raw(pid as i32), sig)
            .map_err(|e| ProcessError::Pty(format!("killpg({pid}, {sig:?}): {e}")))?;
        Ok(())
    }

    /// Programmatic interrupt: `killpg(SIGINT)`.
    ///
    /// Used by the Stop control and by the agent's `proc_signal` tool. Unlike
    /// writing `0x03`, this works regardless of the child's terminal mode. A
    /// Stop button implemented by writing `0x03` would appear to work in `bash`
    /// and silently fail in `vim`, `less`, and every full-screen program.
    #[cfg(unix)]
    pub fn interrupt(&self, id: ProcessId) -> Result<()> {
        self.signal(id, nix::sys::signal::Signal::SIGINT)
    }

    /// Stop a process, escalating `SIGINT` → `SIGTERM` → `SIGKILL`.
    ///
    /// Non-blocking: escalation runs on its own thread so the UI never stalls
    /// waiting for a stubborn build to die.
    #[cfg(unix)]
    pub fn terminate(&self, id: ProcessId, grace: Duration) -> Result<()> {
        use nix::sys::signal::Signal;

        {
            let mut g = self.inner.lock().unwrap();
            let entry = g.get_mut(&id).ok_or(ProcessError::NoSuchProcess(id))?;
            if entry.record.state.is_terminal() {
                return Err(ProcessError::AlreadyTerminated(id));
            }
            entry.record.state = ProcessState::Stopping;
            let _ = entry.events.send(ProcessEvent::StateChanged(ProcessState::Stopping));
        }

        self.signal(id, Signal::SIGINT)?;

        let this = self.clone();
        std::thread::Builder::new()
            .name(format!("forge-pty-term-{}", id.0))
            .spawn(move || {
                for sig in [Signal::SIGTERM, Signal::SIGKILL] {
                    std::thread::sleep(grace);
                    match this.get(id) {
                        Ok(r) if r.state.is_terminal() => return,
                        Ok(_) => {
                            let _ = this.signal(id, sig);
                        }
                        Err(_) => return,
                    }
                }
            })
            .expect("spawn terminate thread");
        Ok(())
    }
}
