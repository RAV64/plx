//! Keystroke to echo through the real daemon while four panes flood output
//! (target: under 1 ms). The path: the client sends a key, the
//! daemon writes it to the focused pane's terminal, the kernel echoes it,
//! the daemon reads it, feeds libghostty and sends a frame, and the client
//! decodes the character at its cursor.
//!
//! Run with `cargo bench -p plx-daemon --bench echo`, or the built binary
//! with `[N|ingest] [od|cat] [CPUS] [zoom]` (see [`Args`]). On a hybrid
//! machine, pin the binary to the performance cores and the floods away
//! from them: `taskset -c 0-3 echo-… 4 cat 4-11`. Compare variants
//! interleaved in one session: a laptop's clock drifts by half between
//! sessions (the per-key decoding time shows it).

#![expect(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::as_conversions,
    reason = "a benchmark sets up fixed, valid inputs and reports on stdout; a failure aborts it"
)]

use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use plx_client_core::client::{ClientCore, ClientEffects, Received};
use plx_client_core::compose::{Capabilities, ColorDepth, ProgressBars, UnderlineStyles};
use plx_core::command::{Command, SessionCommand};
use plx_core::geometry::Direction;
use plx_core::rejection::Rejection;
use plx_daemon::paths::{Env, Paths, SessionName};
use plx_daemon::pty::Program;
use plx_daemon::reactor::{Config, run};
use plx_emulator::{GlyphKind, GridSize, Key, KeyChar, KeyEvent, Mods};
use plx_protocol::client::{ClientWire, encode};
use plx_protocol::framing::{MAX_SERVER_MESSAGE, split};
use plx_protocol::hello::{HELLO, check_hello};
use plx_protocol::server::{Decoder, ServerMessage, Unattached};

/// Timed keystrokes, after the warm-up ones.
const KEYS: usize = 2000;
const WARM_UP: usize = 200;
/// Flooding panes unless the first argument says otherwise.
const FLOODING: usize = 4;

/// What floods: `od` of `/dev/urandom` (a CPU-heavy producer), or `cat` of
/// a file of random hex in a loop (a cheap one, limited by the daemon).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flood {
    Od,
    Cat,
}

/// The command line: `[N] [od|cat] [CPUS] [zoom]` for echo, `ingest`, or
/// `resize [N] [tabs|split]`.
struct Args {
    task: Task,
    /// For `resize`: panes with full scrollback, and where they are.
    panes: usize,
    layout: Layout,
    flooding: usize,
    flood: Flood,
    /// `taskset -c CPUS ` before each flood, to keep the flooding programs
    /// off the daemon's cores on hybrid machines.
    pin: String,
    /// `zoom`: the quiet pane shown alone, so the floods are ingested but
    /// not drawn, which isolates the cost of drawing them.
    zoom: Mode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Off,
    On,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Task {
    /// Keystroke to echo while panes flood.
    Echo,
    /// How fast one pane takes a flood.
    Ingest,
    /// A client resize to its frame, with every pane's scrollback full.
    Resize,
    /// The ingest's flood read straight from a terminal opened as the
    /// daemon opens one, read as it reads, without the daemon: the ceiling
    /// any change to it could reach. `On`: fed to libghostty too.
    Raw(Mode),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// One pane per tab: one shown, the others hidden.
    Tabs,
    /// All in one tab, all shown.
    Split,
}

impl Args {
    fn parse() -> Self {
        let args: Vec<String> = std::env::args()
            .skip(1)
            .filter(|arg| arg != "--bench")
            .collect();
        let arg = |i: usize| args.get(i).map(String::as_str);
        let task = match arg(0) {
            Some("ingest") => Task::Ingest,
            Some("resize") => Task::Resize,
            Some("raw") => Task::Raw(if arg(1) == Some("feed") {
                Mode::On
            } else {
                Mode::Off
            }),
            None | Some(_) => Task::Echo,
        };
        Self {
            task,
            panes: match (task, arg(1)) {
                (Task::Resize, Some(n)) => n.parse().expect("a number of panes"),
                (Task::Echo | Task::Ingest | Task::Resize | Task::Raw(_), _) => 8,
            },
            layout: match (task, arg(2)) {
                (Task::Resize, Some("split")) => Layout::Split,
                (Task::Echo | Task::Ingest | Task::Resize | Task::Raw(_), _) => Layout::Tabs,
            },
            flooding: match (task, arg(0)) {
                (Task::Echo, Some(n)) => n.parse().expect("a number of flooding panes"),
                (Task::Echo | Task::Ingest | Task::Resize | Task::Raw(_), _) => FLOODING,
            },
            flood: match (task, arg(1)) {
                (Task::Echo, None | Some("od"))
                | (Task::Ingest | Task::Resize | Task::Raw(_), _) => Flood::Od,
                (Task::Echo, Some("cat")) => Flood::Cat,
                (Task::Echo, Some(other)) => panic!("floods are od or cat, not {other}"),
            },
            pin: arg(2).map_or_else(String::new, |cpus| format!("taskset -c {cpus} ")),
            zoom: if arg(3) == Some("zoom") {
                Mode::On
            } else {
                Mode::Off
            },
        }
    }
}

fn main() {
    let args = Args::parse();
    if let Task::Raw(feed) = args.task {
        raw(feed);
        return;
    }
    let root = std::env::temp_dir().join(format!("plx-echo-bench-{}", std::process::id()));
    if root.exists() {
        std::fs::remove_dir_all(&root).expect("a stale bench directory is removed");
    }
    let paths = session(&root);
    let daemon = std::thread::Builder::new()
        .name(String::from("plx-daemon"))
        .spawn({
            let config = Config {
                session: SessionName::new("bench").expect("a valid name"),
                paths: session(&root),
                program: Program {
                    path: "/bin/sh".into(),
                    args: Vec::new(),
                },
                editor: None,
                scrollback: plx_daemon::reactor::DEFAULT_SCROLLBACK,
                ready: None,
            };
            move || run(config)
        })
        .expect("the daemon thread starts");
    let deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(&paths.socket).is_err() {
        assert!(Instant::now() < deadline, "the daemon did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    let hex = root.join("flood.hex");
    if args.task != Task::Echo || args.flood == Flood::Cat {
        std::fs::write(&hex, random_hex(16 << 20)).expect("the flood file is written");
    }
    // As the plx client asks (the daemon thread asks for its own).
    let _slice = plx_daemon::sched::shorten().expect("a scheduler slice is asked for");
    let mut client = Client::attach(&paths);
    client.pump_until(|c| c.panes() == 1);
    match args.task {
        Task::Ingest => ingest(&mut client, &hex),
        Task::Echo => echo(&args, &paths, &mut client, &hex),
        Task::Resize => resize(&args, &paths, &mut client, &hex),
        Task::Raw(_) => panic!("the raw read runs without the daemon"),
    }
    assert!(matches!(
        command(&paths, SessionCommand::KillSession),
        ServerMessage::SessionEnded(_)
    ));
    daemon
        .join()
        .expect("the daemon thread ends")
        .expect("the daemon ends cleanly");
    std::fs::remove_dir_all(&root).expect("the bench directory is removed");
}

/// The ingest's 64 MiB read from a terminal as the daemon reads one: a
/// share of [`ITERATION_BUDGET`] at a time until it is empty, then waiting
/// until it is readable; `feed` hands each read to libghostty (and syncs,
/// as a frame would).
fn raw(feed: Mode) {
    use plx_daemon::pty::{Pty, Read};
    use plx_daemon::reactor::ITERATION_BUDGET;
    use plx_emulator::{Emulator as _, Version};
    use plx_ghostty::{GhosttyEmulator, GraphemeClustering, Options};
    use rustix::event::{PollFd, PollFlags};
    let root = std::env::temp_dir().join(format!("plx-raw-bench-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("the bench directory is made");
    let hex = root.join("flood.hex");
    std::fs::write(&hex, random_hex(16 << 20)).expect("the flood file is written");
    let size = GridSize::new(
        80.try_into().expect("not zero"),
        24.try_into().expect("not zero"),
    )
    .expect("fits");
    // `PLX_BENCH_FILE`: that file once instead.
    let cat = std::env::var_os("PLX_BENCH_FILE").map_or_else(
        || format!("cat {0} {0} {0} {0}", hex.display()),
        |file| format!("cat '{}'", Path::new(&file).display()),
    );
    let program = Program {
        path: "/bin/sh".into(),
        args: Vec::from([OsString::from("-c"), OsString::from(cat)]),
    };
    let mut emulator = (feed == Mode::On).then(|| {
        let options = Options {
            scrollback: plx_daemon::reactor::DEFAULT_SCROLLBACK,
            continuation_max_bytes: std::num::NonZeroUsize::new(4096).expect("not zero"),
            grapheme_clustering: GraphemeClustering::Enabled,
        };
        GhosttyEmulator::new(size, &options).expect("a terminal is created")
    });
    let started = Instant::now();
    let pty = Pty::spawn(&program, size, &[("TERM", "xterm-256color")], None).expect("cat starts");
    let mut buffer = vec![0_u8; ITERATION_BUDGET];
    let (mut total, mut reads, mut wakes) = (0_usize, 0_usize, 0_usize);
    let mut version = Version::ZERO;
    let master = pty.master();
    'flood: loop {
        let mut fds = [PollFd::new(&master, PollFlags::IN)];
        rustix::event::poll(&mut fds, None).expect("poll");
        wakes += 1;
        loop {
            match pty.read(&mut buffer).expect("read") {
                Read::Data(n) => {
                    total += n;
                    reads += 1;
                    if let Some(emulator) = &mut emulator {
                        emulator.feed(&buffer[..n]);
                    }
                }
                Read::Empty => break,
                Read::Closed => break 'flood,
            }
        }
        if let Some(emulator) = &mut emulator {
            version = version.next();
            emulator.sync(version).expect("sync");
        }
    }
    let elapsed = started.elapsed();
    println!(
        "raw{}: {:.1} MiB in {elapsed:?}: {:.0} MB/s, {reads} reads of {:.0} B, {wakes} wake-ups",
        if feed == Mode::On {
            " + libghostty"
        } else {
            ""
        },
        total as f64 / f64::from(1 << 20),
        total as f64 / elapsed.as_secs_f64() / 1e6,
        total as f64 / reads as f64,
    );
    std::fs::remove_dir_all(&root).expect("the bench directory is removed");
}

/// 64 MiB through one pane, timed until the shell's next command shows.
/// With `PLX_BENCH_FILE` set, that file once instead (such as the output
/// of `scripts/stress.py`).
fn ingest(client: &mut Client, hex: &Path) {
    client.pump_until(|c| c.cursor_or_none().is_some_and(|(x, _)| x > 0));
    let (command, bytes) = std::env::var_os("PLX_BENCH_FILE").map_or_else(
        || (format!("cat {0} {0} {0} {0}", hex.display()), 64 << 20),
        |file| {
            let path = Path::new(&file);
            let len = std::fs::metadata(path)
                .expect("the bench file is there")
                .len();
            (format!("cat '{}'", path.display()), len)
        },
    );
    let started = Instant::now();
    client.type_text(&format!("{command}; echo ingest-$((6*7))\r"));
    client.pump_until(|c| c.focused_text().contains("ingest-42"));
    let elapsed = started.elapsed();
    println!(
        "ingest: {:.1} MiB through one pane in {elapsed:?}: {:.0} MB/s",
        bytes as f64 / f64::from(1 << 20),
        bytes as f64 / elapsed.as_secs_f64() / 1e6
    );
}

/// Resizes the client back and forth, timing each until the daemon's
/// frame has the new size, with every pane's scrollback filled to the
/// daemon's limit.
fn resize(args: &Args, paths: &Paths, client: &mut Client, hex: &Path) {
    for pane in 0..args.panes {
        if pane > 0 {
            let command_ = match (args.layout, pane % 2) {
                (Layout::Tabs, _) => SessionCommand::NewTab,
                (Layout::Split, 0) => SessionCommand::Split {
                    direction: Direction::Right,
                },
                (Layout::Split, _) => SessionCommand::Split {
                    direction: Direction::Down,
                },
            };
            assert_eq!(command(paths, command_), ServerMessage::Applied);
        }
        client.pump_until(|c| c.cursor_or_none().is_some_and(|(x, _)| x > 0));
        // 16 MiB of history: more than the 10 MiB a pane keeps.
        let marker = 1000 + pane;
        client.type_text(&format!(
            "cat {}; echo filled-$((1000+{pane}))\r",
            hex.display()
        ));
        let marker = format!("filled-{marker}");
        client.pump_until(|c| c.focused_text().contains(&marker));
    }
    let sizes = [(160, 50), (200, 60)];
    let mut samples = Vec::new();
    for round in 0..40 {
        let (cols, rows) = sizes[round % 2];
        let size = GridSize::new(
            cols.try_into().expect("not zero"),
            rows.try_into().expect("not zero"),
        )
        .expect("fits");
        // A frame interval apart, so pacing does not hold the frame back
        // and what is timed is the daemon's work.
        std::thread::sleep(Duration::from_millis(20));
        let sent = Instant::now();
        client.resize(size);
        client.pump_until(|c| c.frame_size_or_none() == Some(size));
        samples.push(sent.elapsed());
    }
    samples.sort_unstable();
    println!(
        "resize to frame, {} panes with full scrollback ({:?}): min {:?}  p50 {:?}  max {:?}",
        args.panes,
        args.layout,
        samples[0],
        samples[samples.len() / 2],
        samples[samples.len() - 1],
    );
}

/// Starts the floods, then times keystrokes in the quiet pane.
fn echo(args: &Args, paths: &Paths, client: &mut Client, hex: &Path) {
    // Each flooding pane prints random hex (every row differs, so every
    // frame carries them), then a split opens the next pane, which takes
    // focus; the last one stays quiet for typing.
    for pane in 0..args.flooding {
        let pin = &args.pin;
        match args.flood {
            Flood::Od => client.type_text(&format!("{pin}od -An -tx8 /dev/urandom\r")),
            Flood::Cat => client.type_text(&format!(
                "{pin}sh -c 'while :; do cat {}; done'\r",
                hex.display()
            )),
        }
        let direction = if pane % 2 == 0 {
            Direction::Right
        } else {
            Direction::Down
        };
        assert_eq!(
            command(paths, SessionCommand::Split { direction }),
            ServerMessage::Applied
        );
        client.pump_until(|c| c.panes() == pane + 2);
    }
    if args.zoom == Mode::On {
        assert_eq!(
            command(paths, SessionCommand::ToggleZoom),
            ServerMessage::Applied
        );
        client.pump_until(|c| c.panes() == 1);
    }
    // The quiet pane's shell has drawn its prompt.
    client.pump_until(|c| c.cursor_or_none().is_some_and(|(x, _)| x > 0));
    let start_x = client.cursor_or_none().expect("a cursor").0;
    let cols = client.focused_cols();
    let mut samples = Vec::with_capacity(KEYS);
    let started = Instant::now();
    let (received, frames, decoding) = (client.received, client.frames, client.decoding);
    for i in 0..WARM_UP + KEYS {
        let (x, y) = client.cursor_or_none().expect("a cursor");
        let c = if i % 2 == 0 { 'a' } else { 'b' };
        let sent = Instant::now();
        client.key(KeyEvent::press(
            Key::Char(KeyChar::new(c).expect("a letter")),
            Mods::empty(),
        ));
        client.pump_until(|client| {
            client.cursor_or_none() == Some((x + 1, y)) && client.focused_char(x, y) == Some(c)
        });
        if i >= WARM_UP {
            samples.push(sent.elapsed());
        }
        // Erase the line (the terminal's kill character) before it wraps.
        if x + 2 >= cols {
            client.key(KeyEvent::press(
                Key::Char(KeyChar::new('u').expect("a letter")),
                Mods::CTRL,
            ));
            client.pump_until(|client| client.cursor_or_none().is_some_and(|(x, _)| x == start_x));
        }
    }
    let totals = Totals {
        elapsed: started.elapsed(),
        received: client.received - received,
        frames: client.frames - frames,
        decoding: client.decoding.saturating_sub(decoding),
    };
    report(args, samples, &totals);
}

/// What the client took in while keys were timed.
struct Totals {
    elapsed: Duration,
    received: usize,
    frames: usize,
    decoding: Duration,
}

fn report(args: &Args, mut samples: Vec<Duration>, totals: &Totals) {
    samples.sort_unstable();
    let at = |permille: usize| samples[(samples.len() - 1) * permille / 1000];
    let mean = samples.iter().sum::<Duration>() / u32::try_from(samples.len()).expect("fits");
    println!(
        "keystroke to echo, {KEYS} keys, {} panes flooding ({:?}):",
        args.flooding, args.flood
    );
    println!(
        "  min {:?}  p50 {:?}  p90 {:?}  p99 {:?}  p99.9 {:?}  max {:?}  mean {:?}",
        samples[0],
        at(500),
        at(900),
        at(990),
        at(999),
        samples[samples.len() - 1],
        mean,
    );
    let seconds = totals.elapsed.as_secs_f64();
    println!(
        "  the client received {:.1} MiB/s meanwhile: {:.0} frames/s of {:.1} KiB",
        totals.received as f64 / seconds / f64::from(1 << 20),
        totals.frames as f64 / seconds,
        totals.received as f64 / totals.frames.max(1) as f64 / 1024.0,
    );
    let keys = u32::try_from(WARM_UP + KEYS).expect("fits");
    println!(
        "  per key: {:.2} frames, {:?} decoding and composing them",
        totals.frames as f64 / f64::from(keys),
        totals.decoding / keys,
    );
}

/// `len` bytes of lines of random hex, from a fixed-seed xorshift.
fn random_hex(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        for _ in 0..4 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(format!("{state:016x} ").as_bytes());
        }
        out.push(b'\n');
    }
    out
}

fn session(root: &Path) -> Paths {
    let env = Env {
        runtime_dir: Some(OsString::from(root.join("run"))),
        state_home: Some(OsString::from(root.join("state"))),
        home: None,
    };
    Paths::of(&SessionName::new("bench").expect("a valid name"), &env).expect("paths")
}

fn connect(paths: &Paths) -> UnixStream {
    let mut stream = UnixStream::connect(&paths.socket).expect("connected");
    stream.write_all(&HELLO).expect("hello sent");
    let mut hello = [0_u8; 8];
    stream.read_exact(&mut hello).expect("hello read");
    check_hello(hello).expect("the same protocol");
    stream
}

/// One command on its own connection, as the CLI sends it.
fn command(paths: &Paths, command: SessionCommand) -> ServerMessage {
    let mut stream = connect(paths);
    let mut out = Vec::new();
    encode(&ClientWire::Command(Command::Session(command)), &mut out).expect("encoded");
    stream.write_all(&out).expect("sent");
    let mut inbox = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        if let Some((payload, _)) = split(&inbox, MAX_SERVER_MESSAGE).expect("framed") {
            return Decoder::new()
                .decode(payload, &mut Unattached)
                .expect("decoded");
        }
        let n = stream.read(&mut chunk).expect("read");
        assert!(n > 0, "closed without an answer");
        inbox.extend_from_slice(&chunk[..n]);
    }
}

#[derive(Default)]
struct Shell {
    daemon: Vec<u8>,
    terminal: Vec<u8>,
}

impl ClientEffects for Shell {
    fn daemon(&mut self) -> &mut Vec<u8> {
        &mut self.daemon
    }
    fn terminal(&mut self) -> &mut Vec<u8> {
        &mut self.terminal
    }
    fn rejected(&mut self, rejection: Rejection) {
        panic!("rejected: {rejection}");
    }
}

/// An attached client that composes as the real one does, but discards
/// the terminal output.
struct Client {
    stream: UnixStream,
    core: Option<ClientCore>,
    shell: Shell,
    inbox: Vec<u8>,
    chunk: Box<[u8]>,
    received: usize,
    frames: usize,
    /// Time spent decoding and composing frames.
    decoding: Duration,
}

impl Client {
    fn attach(paths: &Paths) -> Self {
        let stream = connect(paths);
        stream
            .set_read_timeout(Some(Duration::from_millis(20)))
            .expect("a timeout");
        let mut shell = Shell::default();
        let size = GridSize::new(
            200.try_into().expect("not zero"),
            60.try_into().expect("not zero"),
        )
        .expect("fits");
        let capabilities = Capabilities {
            color: ColorDepth::TrueColor,
            underlines: UnderlineStyles::Styled,
            progress: ProgressBars::Unsupported,
        };
        let core = ClientCore::attach(size, None, capabilities, &mut shell);
        let mut client = Self {
            stream,
            core: Some(core),
            shell,
            inbox: Vec::new(),
            chunk: vec![0; 1 << 16].into_boxed_slice(),
            received: 0,
            frames: 0,
            decoding: Duration::ZERO,
        };
        client.flush();
        client
    }

    fn flush(&mut self) {
        self.stream.write_all(&self.shell.daemon).expect("sent");
        self.shell.daemon.clear();
        self.shell.terminal.clear();
    }

    fn key(&mut self, key: KeyEvent) {
        self.core
            .as_mut()
            .expect("attached")
            .key(key, &mut self.shell);
        self.flush();
    }

    fn resize(&mut self, size: GridSize) {
        self.core
            .as_mut()
            .expect("attached")
            .resized(size, None, &mut self.shell);
        self.flush();
    }

    /// The session size of the last frame.
    fn frame_size_or_none(&self) -> Option<GridSize> {
        Some(self.core.as_ref()?.mirror().header()?.size)
    }

    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            let key = match c {
                '\r' => Key::Enter,
                c => Key::Char(KeyChar::new(c).expect("a printable character")),
            };
            self.key(KeyEvent::press(key, Mods::empty()));
        }
    }

    /// Receives and acknowledges frames until `done`.
    fn pump_until(&mut self, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(self) {
            assert!(Instant::now() < deadline, "timed out");
            match self.stream.read(&mut self.chunk) {
                Ok(0) => panic!("the daemon closed the connection"),
                Ok(n) => {
                    self.received += n;
                    self.inbox.extend_from_slice(&self.chunk[..n]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(error) => panic!("{error}"),
            }
            let core = self.core.take().expect("attached");
            let before = core.mirror().seq_or_none();
            let decoding = Instant::now();
            let received = core
                .receive(&mut self.inbox, &mut self.shell)
                .expect("valid frames");
            self.decoding += decoding.elapsed();
            match received {
                Received::Attached(core) => {
                    if core.mirror().seq_or_none() != before {
                        self.frames += 1;
                    }
                    self.core = Some(core);
                }
                Received::Ended(end) => panic!("ended: {end:?}"),
            }
            self.flush();
        }
    }

    fn panes(&self) -> usize {
        self.core
            .as_ref()
            .map_or(0, |core| core.mirror().panes().count())
    }

    fn focused(&self) -> Option<&plx_client_core::mirror::MirrorPane> {
        let mirror = self.core.as_ref()?.mirror();
        mirror.pane_or_none(mirror.header()?.focus)
    }

    fn cursor_or_none(&self) -> Option<(u16, u16)> {
        let position = self.focused()?.cursor().position?;
        Some((position.x, position.y))
    }

    fn focused_cols(&self) -> u16 {
        self.focused().expect("a focused pane").rect().cols()
    }

    /// The focused pane's text, row by row.
    fn focused_text(&self) -> String {
        let mut text = String::new();
        if let Some(pane) = self.focused() {
            let mut y = 0;
            while let Some(row) = pane.row_or_none(y) {
                text.extend(row.iter().map(|cell| match cell.glyph.kind() {
                    GlyphKind::Char(c) => c,
                    GlyphKind::Empty | GlyphKind::Cluster(_) => ' ',
                }));
                text.push('\n');
                y += 1;
            }
        }
        text
    }

    fn focused_char(&self, x: u16, y: u16) -> Option<char> {
        let cell = self.focused()?.row_or_none(y)?.get(usize::from(x))?;
        match cell.glyph.kind() {
            GlyphKind::Char(c) => Some(c),
            GlyphKind::Empty | GlyphKind::Cluster(_) => None,
        }
    }
}
