//! The daemon's reactor: one thread, one `mio::Poll`.
//!
//! The session is stepped in the order the simulation's reference reactor
//! proved (`plx-sim`): client input first, PTY output in budgets, exits,
//! then a due tick.
//!
//! Readiness is edge-triggered: sockets are read until they would block, and
//! a pane whose budget ran out stays marked, so the next poll does not wait.
//! The core's contracts with its shell (`Effects`) hold here: spawn outcomes
//! are reported right after the step that asked, a closed client's input is
//! dropped and never reported gone, and nothing steps the session after it
//! exits.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read as _, Write as _};
use std::num::{NonZeroI32, NonZeroU32};
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mio::net::{UnixListener, UnixStream};
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Registry, Token};
use plx_bug::OrBug as _;
use plx_core::effects::{Effects, History, PaneProgram, StartIn};
use plx_core::event::{Event, EventSeq, Pid, SpawnError as CoreSpawnError, SpawnStage};
use plx_core::id::{ClientId, PaneId};
use plx_core::input::Input;
use plx_core::message::Outgoing;
use plx_core::mode::Keymap;
use plx_core::session::Session;
use plx_core::size::SizePolicy;
use plx_core::time::Timestamp;
use plx_emulator::{Emulator, GridSize};
use plx_ghostty::{GhosttyFactory, GraphemeClustering, Options, Scrollback};
use plx_protocol::client::{Request, decode};
use plx_protocol::framing::{MAX_CLIENT_MESSAGE, split};
use plx_protocol::hello::{HELLO, check_hello};
use plx_protocol::server::encode_outgoing;
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
use signal_hook_mio::v1_0::Signals;

use crate::paths::{Paths, SessionName};
use crate::pty::{Program, Pty, Read, SpawnError};
use crate::subscribe::{self, Lag};

/// The most pane output read in one iteration, shared by the panes that
/// have output.
///
/// libghostty parses a flood at about 2.8 µs per KiB, so an iteration stays
/// near 100 µs however many panes flood, and client input and echo are
/// never far behind.
pub const ITERATION_BUDGET: usize = 32 * 1024;

/// The least a pane with output gets per iteration: one terminal read.
pub const MIN_SHARE: usize = 4096;

/// How long a hung-up program has before its process group is killed.
pub const HANG_UP_GRACE: Duration = Duration::from_secs(2);

const LISTENER: Token = Token(0);
const SIGNALS: Token = Token(1);
const KIND_SHIFT: u32 = 60;
const KIND_CONN: usize = 1;
const KIND_PTY: usize = 2;
const KIND_PIDFD: usize = 3;

/// How much history a pane keeps unless configured: 10 MiB (5.9k rows at
/// 200 columns, about 1.6 ms to reflow).
pub const DEFAULT_SCROLLBACK: Scrollback = Scrollback::Bytes(NonZeroUsizeConst::MIB_10);

/// libghostty's settings for panes keeping `scrollback`.
const fn options(scrollback: Scrollback) -> Options {
    Options {
        scrollback,
        continuation_max_bytes: NonZeroUsizeConst::KIB_4,
        grapheme_clustering: GraphemeClustering::Enabled,
    }
}

/// `NonZeroUsize` constants, built without panics.
struct NonZeroUsizeConst;

impl NonZeroUsizeConst {
    const KIB_4: std::num::NonZeroUsize = std::num::NonZeroUsize::MIN.saturating_add(4095);
    const MIB_10: std::num::NonZeroUsize =
        std::num::NonZeroUsize::MIN.saturating_add(10 * 1024 * 1024 - 1);
}

/// Why the daemon stopped with an error.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("{0}")]
    Paths(#[from] crate::paths::PathsError),
    #[error("a daemon for this session is already running")]
    AlreadyRunning,
    #[error("{what}: {source}")]
    Io {
        what: &'static str,
        source: io::Error,
    },
}

const fn io_error(what: &'static str) -> impl FnOnce(io::Error) -> DaemonError {
    move |source| DaemonError::Io { what, source }
}

/// How the daemon is started.
#[derive(Debug)]
pub struct Config {
    /// The session served; its programs find it in `PLX_SESSION`.
    pub session: SessionName,
    pub paths: Paths,
    pub program: Program,
    /// The editor a pane's history opens in: a command line run by
    /// `/bin/sh` with the file appended. `None`: the pane says there is
    /// none.
    pub editor: Option<OsString>,
    /// How much history each pane keeps.
    pub scrollback: Scrollback,
    /// Written to once the socket accepts connections (the client that
    /// started the daemon waits on it).
    pub ready: Option<OwnedFd>,
}

fn token(kind: usize, id: u64) -> Token {
    let id = usize::try_from(id).or_bug("ids fit usize on 64-bit targets");
    Token((kind << KIND_SHIFT) | id)
}

fn id_of(token: Token) -> (usize, u64) {
    let kind = token.0 >> KIND_SHIFT;
    let id = u64::try_from(token.0 & ((1 << KIND_SHIFT) - 1)).or_bug("a usize id fits u64");
    (kind, id)
}

/// Whether the session asked to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Over {
    No,
    Yes,
}

/// Whether a pane's output is left over from an earlier iteration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Backlog {
    No,
    Yes,
}

/// What a pane's terminal may still have to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Output {
    /// Readiness came after it drained: interactive output, such as the
    /// echo of a key. Read before backlogs.
    Ready,
    /// A backlog: more than its last share was waiting.
    Pending,
    /// Read until it would block; readiness comes when there is more.
    Drained,
    /// Closed: every process holding the terminal is gone.
    Closed,
}

/// A pane's program, as the reactor holds it.
#[derive(Debug)]
struct PaneIo {
    pty: Pty,
    /// Written to the program, not yet taken by its terminal.
    out: Vec<u8>,
    output: Output,
    /// How much its last read took: an echo is a few bytes, a flood fills
    /// its share. The smallest are read first.
    last_read: usize,
    /// When it was hung up; killed after [`HANG_UP_GRACE`].
    hung_up: Option<Instant>,
}

/// A client's connection, as effects see it.
#[derive(Debug)]
struct ConnIo {
    stream: UnixStream,
    outbox: Vec<u8>,
}

/// What the session asks of the world, done against real IO.
#[derive(Debug)]
struct Io {
    registry: Registry,
    session: SessionName,
    program: Program,
    /// The editor for histories, a command line for `/bin/sh`; `None` when
    /// none is configured.
    editor: Option<OsString>,
    /// Where histories to edit are written.
    runtime: PathBuf,
    /// The history file of each pane editing one.
    edits: BTreeMap<PaneId, PathBuf>,
    panes: BTreeMap<PaneId, PaneIo>,
    conns: BTreeMap<ClientId, ConnIo>,
    /// Ids by the raw value in their reactor token (ids are minted by the
    /// session, never rebuilt from a number).
    pane_ids: BTreeMap<u64, PaneId>,
    client_ids: BTreeMap<u64, ClientId>,
    /// Spawn outcomes, reported right after the step that asked.
    spawned: Vec<(PaneId, Result<Pid, CoreSpawnError>)>,
    /// Clients the session closed: input dropped, flushed, then closed.
    closed: BTreeSet<ClientId>,
    /// Connections that subscribed to events (the session forgot them),
    /// with the events each missed.
    subscribers: BTreeMap<ClientId, Lag>,
    over: Over,
}

/// The terminal every pane's program is told it runs in.
const PANE_TERM: [(&str, &str); 2] = [("TERM", "xterm-256color"), ("COLORTERM", "truecolor")];

/// The byte written to [`Config::ready`] once the daemon serves.
pub const READY: u8 = b'r';

/// Names the session to programs in its panes, so `plx` run there talks to
/// it.
pub const SESSION_VAR: &str = "PLX_SESSION";

fn core_spawn_error(error: &SpawnError) -> CoreSpawnError {
    let (stage, errno) = match error {
        SpawnError::Pty(errno) => (SpawnStage::OpenPty, errno.raw_os_error()),
        SpawnError::Pidfd(errno) => (SpawnStage::Fork, errno.raw_os_error()),
        // std reports exec failures with their errno; an error without one
        // (an argument with a NUL byte) is an invalid argument.
        SpawnError::Start(error) => (
            SpawnStage::Exec,
            error
                .raw_os_error()
                .unwrap_or_else(|| rustix::io::Errno::INVAL.raw_os_error()),
        ),
    };
    CoreSpawnError {
        stage,
        errno: NonZeroI32::new(errno).or_bug("a failed call reports a non-zero errno"),
    }
}

/// Starts `program` for `pane` on a new PTY of `size`, registered with the
/// reactor.
fn start(
    registry: &Registry,
    session: &SessionName,
    program: &Program,
    pane: PaneId,
    size: GridSize,
    cwd: Option<&Path>,
) -> Result<(Pty, Pid), CoreSpawnError> {
    let [term, colorterm] = PANE_TERM;
    let env = [term, colorterm, (SESSION_VAR, session.as_str())];
    let pty = match Pty::spawn(program, size, &env, cwd) {
        Ok(pty) => pty,
        Err(error) => {
            tracing::warn!(?pane, %error, "starting a pane's program");
            return Err(core_spawn_error(&error));
        }
    };
    let registered = registry
        .register(
            &mut SourceFd(&pty.master_fd()),
            token(KIND_PTY, pane.get()),
            Interest::READABLE | Interest::WRITABLE,
        )
        .and_then(|()| {
            registry.register(
                &mut SourceFd(&pty.pidfd()),
                token(KIND_PIDFD, pane.get()),
                Interest::READABLE,
            )
        });
    match registered {
        Ok(()) => {
            let pid = u32::try_from(pty.pid().as_raw_nonzero().get())
                .map_or(None, NonZeroU32::new)
                .or_bug("a child's pid is positive");
            Ok((pty, Pid::new(pid)))
        }
        Err(error) => {
            tracing::error!(?pane, %error, "registering a pane with the reactor");
            // The program must not outlive its pane.
            if let Err(error) = pty.kill() {
                tracing::warn!(?pane, %error, "killing an unregistered pane");
            }
            Err(CoreSpawnError {
                stage: SpawnStage::Fork,
                errno: error
                    .raw_os_error()
                    .and_then(NonZeroI32::new)
                    .or_bug("epoll errors carry an errno"),
            })
        }
    }
}

/// What tells the user, in the editor's pane, that there is no editor.
const NO_EDITOR: &str = "plx: no editor for the scrollback: set scrollback_editor in \
    config.toml, or VISUAL or EDITOR. Press Enter to close.";

/// The program that edits `file`: `editor`, a command line run by
/// `/bin/sh` with the file's path appended; without one, a message saying
/// so, until Enter.
fn editor_program(editor: Option<&OsStr>, file: &Path) -> Program {
    let args = editor.map_or_else(
        || {
            Vec::from([
                OsString::from("-c"),
                OsString::from("printf '%s\\n' \"$1\"; read -r _"),
                OsString::from("plx-editor"),
                OsString::from(NO_EDITOR),
            ])
        },
        |editor| {
            let mut script = OsString::from("exec ");
            script.push(editor);
            script.push(" \"$1\"");
            Vec::from([
                OsString::from("-c"),
                script,
                OsString::from("plx-editor"),
                OsString::from(file.as_os_str()),
            ])
        },
    );
    Program {
        path: OsString::from("/bin/sh"),
        args,
    }
}

/// Writes `text` to `path`, readable by the user alone; a file left there
/// by a daemon that died is replaced.
fn write_history(path: &Path, text: &[u8]) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(text)
}

impl Io {
    /// Starts the editor on `history` for `pane`: the history goes to a file
    /// in the runtime directory, removed once the pane is gone.
    fn start_editor(
        &mut self,
        pane: PaneId,
        size: GridSize,
        history: History<'_>,
    ) -> Result<(Pty, Pid), CoreSpawnError> {
        let file = self.runtime.join(format!(
            "scrollback-{}-{}.txt",
            self.session.as_str(),
            pane.get()
        ));
        if let Err(error) = write_history(&file, history.bytes()) {
            tracing::warn!(?pane, %error, file = %file.display(), "writing a history to edit");
            // A write that stopped short has no errno: an I/O error.
            let errno = error
                .raw_os_error()
                .unwrap_or_else(|| rustix::io::Errno::IO.raw_os_error());
            return Err(CoreSpawnError {
                stage: SpawnStage::WriteHistory,
                errno: NonZeroI32::new(errno).or_bug("a failed call reports a non-zero errno"),
            });
        }
        let program = editor_program(self.editor.as_deref(), &file);
        self.edits.insert(pane, file);
        start(&self.registry, &self.session, &program, pane, size, None)
    }

    /// Where a shell starting `start_in` starts; `None`: where the daemon
    /// does. A pane that just exited, or a directory that is gone, cannot
    /// be started in: the default then, and the log says so.
    fn working_directory_or_none(&self, start_in: StartIn) -> Option<PathBuf> {
        let StartIn::PaneOf(pane) = start_in else {
            return None;
        };
        let dir = self
            .panes
            .get(&pane)
            .and_then(|io| io.pty.working_directory_or_none());
        match dir {
            Some(dir) if dir.is_dir() => Some(dir),
            Some(dir) => {
                tracing::warn!(?pane, dir = %dir.display(), "a split starts where the daemon does: its pane's directory is gone");
                None
            }
            None => {
                tracing::warn!(
                    ?pane,
                    "a split starts where the daemon does: its pane's directory cannot be read"
                );
                None
            }
        }
    }

    /// Removes the history file of `pane`, if it edited one.
    fn remove_history(&mut self, pane: PaneId) {
        if let Some(file) = self.edits.remove(&pane)
            && let Err(error) = std::fs::remove_file(&file)
        {
            tracing::warn!(?pane, %error, file = %file.display(), "removing an edited history");
        }
    }
}

impl Drop for Io {
    /// Histories still being edited go with the session.
    fn drop(&mut self) {
        let panes: Vec<PaneId> = self.edits.keys().copied().collect();
        for pane in panes {
            self.remove_history(pane);
        }
    }
}

impl Effects for Io {
    fn spawn_pty(&mut self, pane: PaneId, size: GridSize, program: PaneProgram<'_>) {
        let started = match program {
            PaneProgram::Shell { start_in } => {
                let cwd = self.working_directory_or_none(start_in);
                start(
                    &self.registry,
                    &self.session,
                    &self.program,
                    pane,
                    size,
                    cwd.as_deref(),
                )
            }
            PaneProgram::Editor(history) => self.start_editor(pane, size, history),
        };
        let outcome = started.map(|(pty, pid)| {
            self.panes.insert(
                pane,
                PaneIo {
                    pty,
                    out: Vec::new(),
                    output: Output::Pending,
                    last_read: 0,
                    hung_up: None,
                },
            );
            self.pane_ids.insert(pane.get(), pane);
            pid
        });
        if outcome.is_err() {
            self.remove_history(pane);
        }
        self.spawned.push((pane, outcome));
    }

    fn write_pty(&mut self, pane: PaneId, bytes: &[u8]) {
        if let Some(io) = self.panes.get_mut(&pane) {
            io.out.extend_from_slice(bytes);
            flush_pane(pane, io);
        }
    }

    fn resize_pty(&mut self, pane: PaneId, size: GridSize) {
        if let Some(io) = self.panes.get(&pane)
            && let Err(error) = io.pty.resize(size)
        {
            tracing::warn!(?pane, %error, "resizing a pane's terminal");
        }
    }

    fn close_pty(&mut self, pane: PaneId) {
        if let Some(io) = self.panes.get_mut(&pane) {
            if let Err(error) = io.pty.hang_up() {
                tracing::debug!(?pane, %error, "hanging up a pane (already gone?)");
            }
            io.hung_up = Some(Instant::now());
        }
    }

    fn send<E: Emulator>(&mut self, client: ClientId, message: Outgoing<'_, E>) {
        let Some(conn) = self.conns.get_mut(&client) else {
            return;
        };
        if let Err(error) = encode_outgoing(&message, &mut conn.outbox) {
            tracing::error!(?client, %error, "encoding a message");
        }
        flush_conn(client, conn);
    }

    fn close_client(&mut self, client: ClientId) {
        self.closed.insert(client);
    }

    fn event(&mut self, seq: EventSeq, event: Event<'_>) {
        tracing::info!(seq = seq.get(), ?event, "event");
        for (&client, lag) in &mut self.subscribers {
            if let Some(conn) = self.conns.get_mut(&client) {
                subscribe::write(&mut conn.outbox, lag, seq, &event);
                flush_conn(client, conn);
            }
        }
    }

    fn exit(&mut self) {
        self.over = Over::Yes;
    }
}

/// Writes what a pane's terminal takes now.
fn flush_pane(pane: PaneId, io: &mut PaneIo) {
    while !io.out.is_empty() {
        match io.pty.write(&io.out) {
            Ok(0) => break,
            Ok(n) => {
                io.out.drain(..n);
            }
            Err(error) => {
                tracing::debug!(?pane, %error, "writing to a pane (it is going)");
                io.out.clear();
            }
        }
    }
}

/// Writes what a client's socket takes now.
fn flush_conn(client: ClientId, conn: &mut ConnIo) {
    while !conn.outbox.is_empty() {
        match conn.stream.write(&conn.outbox) {
            Ok(0) => break,
            Ok(n) => {
                conn.outbox.drain(..n);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                tracing::debug!(?client, %error, "writing to a client (it is going)");
                conn.outbox.clear();
            }
        }
    }
}

/// Steps the session, then reports at once every program the step started:
/// spawning is synchronous, so its outcome comes before anything else about
/// the pane (the `Effects::spawn_pty` contract).
fn step(session: &mut Session<GhosttyFactory>, io: &mut Io, now: Timestamp, input: Input<'_>) {
    if io.over == Over::Yes {
        return;
    }
    session.step(now, input, io);
    while let Some((pane, outcome)) = io.spawned.pop() {
        if io.over == Over::Yes {
            return;
        }
        let input = match outcome {
            Ok(pid) => Input::PtySpawned { pane, pid },
            Err(error) => Input::PtySpawnFailed { pane, error },
        };
        session.step(now, input, io);
    }
}

/// Whether a client's hello arrived.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Greeting {
    #[default]
    Awaited,
    Done,
}

/// Whether a client closed its side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum End {
    #[default]
    Open,
    Eof,
}

/// Whom a connection talks to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Role {
    /// The session: a client, attached or not.
    #[default]
    Session,
    /// The daemon's event stream; it sends nothing more.
    Subscriber,
}

/// A connection's receive side.
#[derive(Debug, Default)]
struct Inbox {
    bytes: Vec<u8>,
    greeting: Greeting,
    end: End,
    role: Role,
}

/// The daemon.
struct Reactor {
    poll: Poll,
    listener: UnixListener,
    signals: Signals,
    session: Session<GhosttyFactory>,
    io: Io,
    inboxes: BTreeMap<ClientId, Inbox>,
    started: Instant,
    read: Vec<u8>,
    /// The panes to read this iteration, in order (reused).
    order: Vec<PaneId>,
    /// A terminating signal arrived.
    signalled: Over,
}

/// Takes the session's lock: the truth of who serves the session, held for
/// the daemon's life and released by the kernel if it dies.
///
/// # Errors
///
/// When another daemon holds it, or it cannot be taken.
fn lock_session(paths: &Paths) -> Result<std::fs::File, DaemonError> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&paths.lock)
        .map_err(io_error("opening the session lock"))?;
    match rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(rustix::io::Errno::WOULDBLOCK) => return Err(DaemonError::AlreadyRunning),
        Err(errno) => {
            return Err(DaemonError::Io {
                what: "locking the session",
                source: errno.into(),
            });
        }
    }
    Ok(lock)
}

/// Runs a session's daemon until the session ends or a terminating signal.
///
/// # Errors
///
/// When the session's files cannot be set up, another daemon holds the
/// session, or the reactor cannot be made.
pub fn run(config: Config) -> Result<(), DaemonError> {
    let Config {
        session: name,
        paths,
        program,
        editor,
        scrollback,
        ready,
    } = config;
    // Echo comes before a busy program's slice runs out (crate::sched).
    match crate::sched::shorten() {
        Ok(crate::sched::Slice::Short) => {
            tracing::debug!(ns = crate::sched::SLICE_NS, "a short scheduler slice");
        }
        Ok(crate::sched::Slice::Kept) => {
            tracing::info!("not the normal scheduling policy: the slice stays as it is");
        }
        Err(error) => tracing::warn!(%error, "asking for a short scheduler slice"),
    }
    paths.prepare()?;
    let lock = lock_session(&paths)?;
    // Holding the lock, any socket there is a dead daemon's.
    match std::fs::remove_file(&paths.socket) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(DaemonError::Io {
                what: "removing a stale socket",
                source: error,
            });
        }
    }
    let poll = Poll::new().map_err(io_error("creating the reactor"))?;
    let mut listener =
        UnixListener::bind(&paths.socket).map_err(io_error("binding the session socket"))?;
    poll.registry()
        .register(&mut listener, LISTENER, Interest::READABLE)
        .map_err(io_error("registering the socket"))?;
    let mut signals =
        Signals::new([SIGTERM, SIGINT, SIGHUP]).map_err(io_error("installing signal handlers"))?;
    poll.registry()
        .register(&mut signals, SIGNALS, Interest::READABLE)
        .map_err(io_error("registering signals"))?;
    let mut io = Io {
        registry: poll
            .registry()
            .try_clone()
            .map_err(io_error("cloning the registry"))?,
        session: name,
        program,
        editor,
        runtime: PathBuf::from(paths.runtime.as_os_str()),
        edits: BTreeMap::new(),
        panes: BTreeMap::new(),
        conns: BTreeMap::new(),
        pane_ids: BTreeMap::new(),
        client_ids: BTreeMap::new(),
        spawned: Vec::new(),
        closed: BTreeSet::new(),
        subscribers: BTreeMap::new(),
        over: Over::No,
    };
    let mut session = Session::new(
        GhosttyFactory {
            options: options(scrollback),
        },
        Keymap::classic(),
        SizePolicy::Latest,
        &mut io,
    );
    // The first pane was started by `new`, before any step.
    let started = Instant::now();
    while let Some((pane, outcome)) = io.spawned.pop() {
        let input = match outcome {
            Ok(pid) => Input::PtySpawned { pane, pid },
            Err(error) => Input::PtySpawnFailed { pane, error },
        };
        session.step(Timestamp::ZERO, input, &mut io);
    }
    if let Some(ready) = ready {
        // The client that started us waits for this byte.
        if let Err(error) = rustix::io::write(&ready, &[READY]) {
            tracing::warn!(%error, "signalling readiness to the client that started the daemon");
        }
    }
    tracing::info!(socket = %paths.socket.display(), "serving");
    let mut reactor = Reactor {
        poll,
        listener,
        signals,
        session,
        io,
        inboxes: BTreeMap::new(),
        started,
        read: vec![0; ITERATION_BUDGET],
        order: Vec::new(),
        signalled: Over::No,
    };
    let result = reactor.serve();
    reactor.shut_down();
    // Socket first: a client that finds no socket starts a new daemon,
    // which then waits for our lock.
    if let Err(error) = std::fs::remove_file(&paths.socket) {
        tracing::warn!(%error, "removing the session socket");
    }
    drop(lock);
    result
}

impl Reactor {
    fn now(&self) -> Timestamp {
        Timestamp::from_nanos(
            u64::try_from(self.started.elapsed().as_nanos())
                .or_bug("a daemon runs under 584 years"),
        )
    }

    /// How long the next poll may wait.
    fn timeout(&self) -> Option<Duration> {
        if self
            .io
            .panes
            .values()
            .any(|pane| matches!(pane.output, Output::Ready | Output::Pending))
        {
            return Some(Duration::ZERO);
        }
        let now = Instant::now();
        let tick = self.session.poll_timeout().map(|deadline| {
            let due = Duration::from_nanos(deadline.as_nanos());
            due.saturating_sub(self.started.elapsed())
        });
        let kill = self
            .io
            .panes
            .values()
            .filter_map(|pane| pane.hung_up)
            .map(|at| (at + HANG_UP_GRACE).saturating_duration_since(now))
            .min();
        match (tick, kill) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }

    fn serve(&mut self) -> Result<(), DaemonError> {
        let mut events = Events::with_capacity(256);
        let mut readable_conns = Vec::new();
        let mut exited = Vec::new();
        while self.signalled == Over::No && self.io.over == Over::No {
            match self.poll.poll(&mut events, self.timeout()) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(DaemonError::Io {
                        what: "polling",
                        source: error,
                    });
                }
            }
            let mut accept = false;
            readable_conns.clear();
            exited.clear();
            for event in &events {
                match event.token() {
                    LISTENER => accept = true,
                    SIGNALS => {
                        for signal in self.signals.pending() {
                            tracing::info!(signal, "terminating signal");
                            self.signalled = Over::Yes;
                        }
                    }
                    token => match id_of(token) {
                        (KIND_CONN, id) => {
                            let Some(&client) = self.io.client_ids.get(&id) else {
                                continue;
                            };
                            if event.is_readable() || event.is_read_closed() {
                                readable_conns.push(client);
                            }
                            if event.is_writable()
                                && let Some(conn) = self.io.conns.get_mut(&client)
                            {
                                flush_conn(client, conn);
                            }
                        }
                        (KIND_PTY, id) => {
                            let Some(&pane) = self.io.pane_ids.get(&id) else {
                                continue;
                            };
                            if let Some(io) = self.io.panes.get_mut(&pane) {
                                if event.is_readable() || event.is_read_closed() {
                                    io.output = match io.output {
                                        Output::Drained => Output::Ready,
                                        unchanged @ (Output::Ready
                                        | Output::Pending
                                        | Output::Closed) => unchanged,
                                    };
                                }
                                if event.is_writable() {
                                    flush_pane(pane, io);
                                }
                            }
                        }
                        (KIND_PIDFD, id) => {
                            if let Some(&pane) = self.io.pane_ids.get(&id) {
                                exited.push(pane);
                            }
                        }
                        (kind, id) => plx_bug::bug!(
                            "tokens are minted by the reactor: kind {}, id {}",
                            kind,
                            id
                        ),
                    },
                }
            }
            if accept {
                self.accept()?;
            }
            // Client input first.
            for &client in &readable_conns {
                self.receive(client);
            }
            self.serve_clients();
            self.read_panes();
            self.reap(&exited);
            self.kill_overdue();
            if self
                .session
                .poll_timeout()
                .is_some_and(|deadline| deadline <= self.now())
            {
                let now = self.now();
                step(&mut self.session, &mut self.io, now, Input::Tick);
            }
            self.close_finished();
        }
        Ok(())
    }

    fn accept(&mut self) -> Result<(), DaemonError> {
        loop {
            let (mut stream, _) = match self.listener.accept() {
                Ok(accepted) => accepted,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(DaemonError::Io {
                        what: "accepting a client",
                        source: error,
                    });
                }
            };
            // Only this user may talk to the session.
            match rustix::net::sockopt::socket_peercred(&stream) {
                Ok(cred) if cred.uid == rustix::process::getuid() => {}
                Ok(cred) => {
                    tracing::warn!(uid = cred.uid.as_raw(), "refused a client of another user");
                    continue;
                }
                Err(error) => {
                    tracing::warn!(%error, "reading a client's credentials");
                    continue;
                }
            }
            let client = self.session.connect();
            if let Err(error) = self.poll.registry().register(
                &mut stream,
                token(KIND_CONN, client.get()),
                Interest::READABLE | Interest::WRITABLE,
            ) {
                tracing::warn!(%error, "registering a client");
                let now = self.now();
                step(
                    &mut self.session,
                    &mut self.io,
                    now,
                    Input::ClientGone { client },
                );
                continue;
            }
            let mut conn = ConnIo {
                stream,
                outbox: Vec::from(HELLO),
            };
            flush_conn(client, &mut conn);
            self.io.conns.insert(client, conn);
            self.io.client_ids.insert(client.get(), client);
            self.inboxes.insert(client, Inbox::default());
        }
    }

    /// Reads what a client sent, until its socket would block.
    fn receive(&mut self, client: ClientId) {
        let (Some(conn), Some(inbox)) = (
            self.io.conns.get_mut(&client),
            self.inboxes.get_mut(&client),
        ) else {
            return;
        };
        let mut chunk = [0_u8; 16 * 1024];
        loop {
            match conn.stream.read(&mut chunk) {
                Ok(0) => {
                    inbox.end = End::Eof;
                    return;
                }
                Ok(n) => inbox
                    .bytes
                    .extend_from_slice(chunk.get(..n).or_bug("read returns at most the buffer")),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    tracing::debug!(?client, %error, "reading from a client");
                    inbox.end = End::Eof;
                    return;
                }
            }
        }
    }

    /// Steps every whole message clients sent, then the clients that left.
    fn serve_clients(&mut self) {
        let now = self.now();
        let mut gone = Vec::new();
        for (&client, inbox) in &mut self.inboxes {
            if self.io.closed.contains(&client) {
                inbox.bytes.clear();
                continue;
            }
            let mut used = match inbox.greeting {
                Greeting::Done => 0,
                Greeting::Awaited => {
                    let Some(hello) = inbox.bytes.first_chunk::<8>() else {
                        if inbox.end == End::Eof {
                            gone.push(client);
                        }
                        continue;
                    };
                    if let Err(error) = check_hello(*hello) {
                        tracing::warn!(?client, %error, "refused a client");
                        gone.push(client);
                        continue;
                    }
                    inbox.greeting = Greeting::Done;
                    HELLO.len()
                }
            };
            loop {
                if self.io.over == Over::Yes || self.io.closed.contains(&client) {
                    break;
                }
                let rest = inbox
                    .bytes
                    .get(used..)
                    .or_bug("used stays within the inbox");
                let message = match split(rest, MAX_CLIENT_MESSAGE) {
                    Ok(Some((payload, length))) => {
                        used += length;
                        decode(payload)
                    }
                    Ok(None) => break,
                    Err(error) => Err(error),
                };
                match message.map(|wire| (inbox.role, wire)) {
                    Ok((Role::Session, wire)) => match wire.request() {
                        Request::Session(message) => step(
                            &mut self.session,
                            &mut self.io,
                            now,
                            Input::Client { client, message },
                        ),
                        Request::Subscribe => {
                            // The session forgets it; the daemon writes it
                            // events.
                            step(
                                &mut self.session,
                                &mut self.io,
                                now,
                                Input::ClientGone { client },
                            );
                            inbox.role = Role::Subscriber;
                            self.io.subscribers.insert(client, Lag::None);
                        }
                    },
                    Ok((Role::Subscriber, _)) => {
                        tracing::warn!(?client, "a subscriber sent a message");
                        gone.push(client);
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(?client, %error, "a client broke the protocol");
                        gone.push(client);
                        break;
                    }
                }
            }
            if self.io.closed.contains(&client) {
                inbox.bytes.clear();
            } else {
                inbox.bytes.drain(..used);
            }
            if inbox.end == End::Eof && !gone.contains(&client) {
                gone.push(client);
            }
        }
        for client in gone {
            // A client the session closed, or forgot when it subscribed,
            // must not be reported gone.
            if !self.io.closed.contains(&client) && !self.io.subscribers.contains_key(&client) {
                step(
                    &mut self.session,
                    &mut self.io,
                    now,
                    Input::ClientGone { client },
                );
            }
            self.drop_conn(client);
        }
    }

    /// Reads the panes' output, each up to its share of the iteration's
    /// budget: interactive panes first (readiness after draining, then the
    /// least output last time), so an echo is not queued behind floods.
    fn read_panes(&mut self) {
        let now = self.now();
        let mut order = std::mem::take(&mut self.order);
        order.clear();
        order.extend(
            self.io
                .panes
                .iter()
                .filter(|(_, pane)| matches!(pane.output, Output::Ready | Output::Pending))
                .map(|(&pane, _)| pane),
        );
        let panes = &self.io.panes;
        order.sort_unstable_by_key(|pane| {
            panes.get(pane).map(|io| {
                let backlog = match io.output {
                    Output::Ready => Backlog::No,
                    Output::Pending | Output::Drained | Output::Closed => Backlog::Yes,
                };
                (backlog, io.last_read)
            })
        });
        let share = match order.len() {
            0 => ITERATION_BUDGET,
            panes => (ITERATION_BUDGET / panes).max(MIN_SHARE),
        };
        for &pane in &order {
            let Some(io) = self.io.panes.get_mut(&pane) else {
                continue;
            };
            let mut filled = 0;
            let read = self
                .read
                .get_mut(..share.min(ITERATION_BUDGET))
                .or_bug("a share is at most the budget");
            // Read until the share is full or the terminal is empty.
            let state = loop {
                let Some(space) = read.get_mut(filled..) else {
                    break Read::Data(filled);
                };
                if space.is_empty() {
                    break Read::Data(filled);
                }
                match io.pty.read(space) {
                    Ok(Read::Data(n)) => filled += n,
                    Ok(other) => break other,
                    Err(error) => {
                        tracing::warn!(?pane, %error, "reading a pane");
                        break Read::Closed;
                    }
                }
            };
            io.last_read = filled;
            match state {
                // The share filled: more may be waiting.
                Read::Data(_) => io.output = Output::Pending,
                Read::Empty => io.output = Output::Drained,
                Read::Closed => io.output = Output::Closed,
            }
            if filled > 0 {
                let bytes = read.get(..filled).or_bug("filled stays within the buffer");
                step(
                    &mut self.session,
                    &mut self.io,
                    now,
                    Input::PtyOutput { pane, bytes },
                );
            }
        }
        self.order = order;
    }

    /// Collects exits of programs whose pidfd became readable.
    fn reap(&mut self, exited: &[PaneId]) {
        let now = self.now();
        for &pane in exited {
            let status = match self
                .io
                .panes
                .get(&pane)
                .map(|io| io.pty.exit_status_or_none())
            {
                Some(Ok(Some(status))) => status,
                Some(Ok(None)) | None => continue,
                Some(Err(error)) => {
                    tracing::warn!(?pane, %error, "collecting a pane's exit");
                    continue;
                }
            };
            self.io.remove_history(pane);
            if let Some(io) = self.io.panes.remove(&pane) {
                self.io.pane_ids.remove(&pane.get());
                for fd in [io.pty.master_fd(), io.pty.pidfd()] {
                    if let Err(error) = self.poll.registry().deregister(&mut SourceFd(&fd)) {
                        tracing::warn!(?pane, %error, "deregistering a pane");
                    }
                }
            }
            step(
                &mut self.session,
                &mut self.io,
                now,
                Input::PtyExited { pane, status },
            );
        }
    }

    /// Kills programs that ignored their hang-up.
    fn kill_overdue(&mut self) {
        let now = Instant::now();
        for (pane, io) in &mut self.io.panes {
            if let Some(at) = io.hung_up
                && now >= at + HANG_UP_GRACE
            {
                if let Err(error) = io.pty.kill() {
                    tracing::debug!(?pane, %error, "killing a pane (already gone?)");
                }
                io.hung_up = None;
            }
        }
    }

    /// Closes connections the session closed, once their outbox is sent.
    fn close_finished(&mut self) {
        let finished: Vec<ClientId> = self
            .io
            .closed
            .iter()
            .filter(|client| {
                self.io
                    .conns
                    .get(client)
                    .is_none_or(|conn| conn.outbox.is_empty())
            })
            .copied()
            .collect();
        for client in finished {
            self.drop_conn(client);
        }
    }

    fn drop_conn(&mut self, client: ClientId) {
        if let Some(mut conn) = self.io.conns.remove(&client)
            && let Err(error) = self.poll.registry().deregister(&mut conn.stream)
        {
            tracing::warn!(?client, %error, "deregistering a client");
        }
        self.io.client_ids.remove(&client.get());
        self.inboxes.remove(&client);
        self.io.closed.remove(&client);
        self.io.subscribers.remove(&client);
    }

    /// The daemon stops: clients get what was sent to them (their goodbye),
    /// programs are hung up.
    fn shut_down(&mut self) {
        for (&client, conn) in &mut self.io.conns {
            flush_conn(client, conn);
        }
        for (pane, io) in &self.io.panes {
            if let Err(error) = io.pty.hang_up() {
                tracing::debug!(?pane, %error, "hanging up a pane at shutdown");
            }
        }
    }
}
