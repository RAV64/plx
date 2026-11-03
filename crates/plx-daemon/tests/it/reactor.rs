//! The real daemon, in-process: real sockets, real PTYs, the real client
//! core on the other end.

use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use plx_client_core::client::{ClientCore, ClientEffects, Received};
use plx_client_core::compose::{Capabilities, ColorDepth, ProgressBars, UnderlineStyles};
use plx_core::command::{Command, SessionCommand};
use plx_core::geometry::Direction;
use plx_core::rejection::Rejection;
use plx_daemon::paths::{Env, Paths, SessionName};
use plx_daemon::pty::Program;
use plx_daemon::reactor::{Config, DaemonError, run};
use plx_emulator::{GlyphKind, GridSize, Key, KeyChar, KeyEvent, Mods};
use plx_protocol::client::{ClientWire, encode};
use plx_protocol::framing::{MAX_SERVER_MESSAGE, split};
use plx_protocol::hello::{HELLO, check_hello};
use plx_protocol::server::{Decoder, ServerMessage, Unattached};

fn size(cols: u16, rows: u16) -> GridSize {
    GridSize::new(cols.try_into().unwrap(), rows.try_into().unwrap()).unwrap()
}

/// A fresh directory for a test's session.
fn root(test: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("plx-reactor-{test}-{}", std::process::id()));
    if root.exists() {
        std::fs::remove_dir_all(&root).unwrap();
    }
    root
}

/// The session's paths under `root` (pure: computed again wherever needed).
fn session(root: &Path) -> Paths {
    let env = Env {
        runtime_dir: Some(OsString::from(root.join("run"))),
        state_home: Some(OsString::from(root.join("state"))),
        home: None,
    };
    Paths::of(&SessionName::new("test").unwrap(), &env).unwrap()
}

fn config(root: &Path) -> Config {
    Config {
        session: SessionName::new("test").unwrap(),
        paths: session(root),
        program: Program {
            path: "/bin/sh".into(),
            args: Vec::new(),
        },
        // An "editor" that shows the file until Enter.
        editor: Some(std::ffi::OsString::from("sh -c 'cat \"$0\"; read -r _'")),
        scrollback: plx_daemon::reactor::DEFAULT_SCROLLBACK,
        ready: None,
    }
}

fn start(root: &Path) -> JoinHandle<Result<(), DaemonError>> {
    let daemon = std::thread::spawn({
        let config = config(root);
        move || run(config)
    });
    let socket = session(root).socket;
    let deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(&socket).is_err() {
        assert!(Instant::now() < deadline, "the daemon did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    daemon
}

/// A connection with the hello exchanged.
fn connect(paths: &Paths) -> UnixStream {
    let mut stream = UnixStream::connect(&paths.socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    stream.write_all(&HELLO).unwrap();
    let mut hello = [0_u8; 8];
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got = 0;
    while got < 8 {
        assert!(Instant::now() < deadline, "no hello");
        match stream.read(&mut hello[got..]) {
            Ok(n) => got += n,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("{error}"),
        }
    }
    check_hello(hello).unwrap();
    stream
}

#[derive(Default)]
struct Shell {
    daemon: Vec<u8>,
    terminal: Vec<u8>,
    rejections: Vec<Rejection>,
}

impl ClientEffects for Shell {
    fn daemon(&mut self) -> &mut Vec<u8> {
        &mut self.daemon
    }
    fn terminal(&mut self) -> &mut Vec<u8> {
        &mut self.terminal
    }
    fn rejected(&mut self, rejection: Rejection) {
        self.rejections.push(rejection);
    }
}

/// An attached client.
struct Client {
    stream: UnixStream,
    core: Option<ClientCore>,
    shell: Shell,
    inbox: Vec<u8>,
}

impl Client {
    fn attach(paths: &Paths) -> Self {
        let stream = connect(paths);
        let mut shell = Shell::default();
        let core = ClientCore::attach(size(80, 24), None, CAPABILITIES, &mut shell);
        let mut client = Self {
            stream,
            core: Some(core),
            shell,
            inbox: Vec::new(),
        };
        client.flush();
        client
    }

    fn flush(&mut self) {
        self.stream.write_all(&self.shell.daemon).unwrap();
        self.shell.daemon.clear();
    }

    /// Receives for a while, acknowledging frames.
    fn pump(&mut self, until: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut chunk = vec![0_u8; 65536];
        while !until(self) {
            assert!(
                Instant::now() < deadline,
                "timed out; screen:\n{}",
                self.screen()
            );
            match self.stream.read(&mut chunk) {
                Ok(0) => panic!("the daemon closed the connection"),
                Ok(n) => self.inbox.extend_from_slice(&chunk[..n]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(error) => panic!("{error}"),
            }
            let core = self.core.take().unwrap();
            match core.receive(&mut self.inbox, &mut self.shell).unwrap() {
                Received::Attached(core) => self.core = Some(core),
                Received::Ended(end) => panic!("ended: {end:?}"),
            }
            self.flush();
        }
    }

    /// Receives whatever arrives for `wait`.
    fn pump_for(&mut self, wait: Duration) {
        let until = Instant::now() + wait;
        self.pump(|_| Instant::now() >= until);
    }

    fn mouse(&mut self, mouse: plx_emulator::MouseEvent) {
        self.core.as_mut().unwrap().mouse(mouse, &mut self.shell);
        self.flush();
    }

    fn key(&mut self, key: KeyEvent) {
        self.core.as_mut().unwrap().key(key, &mut self.shell);
        self.flush();
    }

    /// The text of every shown pane, row by row.
    fn screen(&self) -> String {
        let mut text = String::new();
        if let Some(core) = &self.core {
            for (_, pane) in core.mirror().panes() {
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
        }
        text
    }

    /// Everything the client has drawn on its terminal, as text.
    fn drawn(&self) -> String {
        String::from_utf8_lossy(&self.shell.terminal).into_owned()
    }

    fn focus(&self) -> Option<plx_core::id::PaneId> {
        self.core
            .as_ref()
            .and_then(|core| core.mirror().header())
            .map(|header| header.focus)
    }

    /// The active tab's position, as the last frame said.
    fn active(&self) -> Option<u16> {
        self.core
            .as_ref()
            .and_then(|core| core.mirror().header())
            .map(|header| header.active)
    }

    fn tabs(&self) -> usize {
        self.core
            .as_ref()
            .map_or(0, |core| core.mirror().tabs().count())
    }

    fn panes(&self) -> usize {
        self.core
            .as_ref()
            .map_or(0, |core| core.mirror().panes().count())
    }
}

const CAPABILITIES: Capabilities = Capabilities {
    color: ColorDepth::TrueColor,
    underlines: UnderlineStyles::Styled,
    progress: ProgressBars::Drawn,
};

/// Sends one command on its own connection, as the CLI does, and returns
/// the daemon's answer.
fn command(paths: &Paths, command: Command) -> ServerMessage {
    let mut stream = connect(paths);
    let mut out = Vec::new();
    encode(&ClientWire::Command(command), &mut out).unwrap();
    stream.write_all(&out).unwrap();
    let mut inbox = Vec::new();
    let mut chunk = [0_u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "no answer");
        if let Some((payload, _)) = split(&inbox, MAX_SERVER_MESSAGE).unwrap() {
            return Decoder::new().decode(payload, &mut Unattached).unwrap();
        }
        match stream.read(&mut chunk) {
            Ok(0) => panic!("closed without an answer"),
            Ok(n) => inbox.extend_from_slice(&chunk[..n]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("{error}"),
        }
    }
}

fn letter(c: char) -> KeyEvent {
    KeyEvent::press(Key::Char(KeyChar::new(c).unwrap()), Mods::empty())
}

#[test]
fn a_client_attaches_types_and_sees_the_echo() {
    let root = root("echo");
    let daemon = start(&root);
    let paths = session(&root);
    let mut client = Client::attach(&paths);
    client.pump(|c| c.panes() == 1);
    for c in "echo plx-$((6*7))".chars() {
        client.key(letter(c));
    }
    client.key(KeyEvent::press(Key::Enter, Mods::empty()));
    client.pump(|c| c.screen().contains("plx-42"));
    // A second daemon for the same session is refused.
    assert!(matches!(
        run(config(&root)),
        Err(DaemonError::AlreadyRunning)
    ));
    // The CLI's path: a command on its own connection.
    let split = Command::Session(SessionCommand::Split {
        direction: Direction::Right,
    });
    assert_eq!(command(&paths, split), ServerMessage::Applied);
    client.pump(|c| c.panes() == 2);
    let kill = Command::Session(SessionCommand::KillSession);
    assert!(matches!(
        command(&paths, kill),
        ServerMessage::SessionEnded(_)
    ));
    daemon.join().unwrap().unwrap();
    assert!(!paths.socket.exists(), "the socket is removed");
    std::fs::remove_dir_all(root).unwrap();
}

/// Reads what arrives on `stream` for up to `wait`, or until it closes.
fn read_for(stream: &mut UnixStream, wait: Duration, into: &mut Vec<u8>) -> End {
    let deadline = Instant::now() + wait;
    let mut chunk = [0_u8; 4096];
    while Instant::now() < deadline {
        match stream.read(&mut chunk) {
            Ok(0) => return End::Closed,
            Ok(n) => into.extend_from_slice(&chunk[..n]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("{error}"),
        }
    }
    End::Open
}

#[derive(Debug, PartialEq)]
enum End {
    Open,
    Closed,
}

#[test]
fn a_subscriber_gets_every_event_as_json_until_the_session_ends() {
    let root = root("events");
    let daemon = start(&root);
    let paths = session(&root);
    let mut events = connect(&paths);
    let mut out = Vec::new();
    encode(&ClientWire::Subscribe, &mut out).unwrap();
    events.write_all(&out).unwrap();
    // Nothing acknowledges a subscription: split until an event shows.
    let mut lines = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !String::from_utf8_lossy(&lines).contains("PaneOpened") {
        assert!(Instant::now() < deadline, "no events");
        let split = Command::Session(SessionCommand::Split {
            direction: Direction::Right,
        });
        assert_eq!(command(&paths, split), ServerMessage::Applied);
        read_for(&mut events, Duration::from_millis(100), &mut lines);
    }
    let kill = Command::Session(SessionCommand::KillSession);
    assert!(matches!(
        command(&paths, kill),
        ServerMessage::SessionEnded(_)
    ));
    let mut end = End::Open;
    while end == End::Open {
        assert!(Instant::now() < deadline, "the stream did not end");
        end = read_for(&mut events, Duration::from_millis(100), &mut lines);
    }
    // One JSON object per line, numbered in order; the last says why.
    let text = String::from_utf8(lines).unwrap();
    let mut previous = None;
    for line in text.lines() {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        let seq = value["seq"].as_u64().unwrap();
        assert!(previous.is_none_or(|p| seq == p + 1), "{text}");
        previous = Some(seq);
    }
    assert!(
        text.lines().last().unwrap().contains("SessionEnded"),
        "{text}"
    );
    daemon.join().unwrap().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

/// Whether a row of the client's screen is exactly `line`.
fn shows(client: &Client, line: &str) -> bool {
    client.screen().lines().any(|row| row.trim_end() == line)
}

#[test]
fn scroll_mode_shows_the_history_and_leaving_returns_to_the_live_screen() {
    let root = root("scroll");
    let daemon = start(&root);
    let paths = session(&root);
    let mut client = Client::attach(&paths);
    client.pump(|c| c.panes() == 1);
    for c in "seq 1 200".chars() {
        client.key(letter(c));
    }
    client.key(KeyEvent::press(Key::Enter, Mods::empty()));
    client.pump(|c| shows(c, "200"));
    // Ctrl-s: scroll mode; `h`: a page back.
    client.key(KeyEvent::press(
        Key::Char(KeyChar::new('s').unwrap()),
        Mods::CTRL,
    ));
    client.key(letter('h'));
    client.pump(|c| !shows(c, "200") && shows(c, "170"));
    // Leaving scroll mode returns to the live screen.
    client.key(KeyEvent::press(Key::Escape, Mods::empty()));
    client.pump(|c| shows(c, "200"));
    let kill = Command::Session(SessionCommand::KillSession);
    assert!(matches!(
        command(&paths, kill),
        ServerMessage::SessionEnded(_)
    ));
    daemon.join().unwrap().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

/// Zellij's search through the daemon: `/` in scroll mode, a needle,
/// `Enter` to the newest match (scrolled to), `N` to an older one; the
/// status line shows the needle and which match of how many.
#[test]
fn searching_the_history_scrolls_to_the_matches() {
    let root = root("search");
    let daemon = start(&root);
    let paths = session(&root);
    let mut client = Client::attach(&paths);
    client.pump(|c| c.panes() == 1);
    for c in "seq 1 200".chars() {
        client.key(letter(c));
    }
    client.key(KeyEvent::press(Key::Enter, Mods::empty()));
    client.pump(|c| shows(c, "200"));
    client.key(KeyEvent::press(
        Key::Char(KeyChar::new('s').unwrap()),
        Mods::CTRL,
    ));
    for c in "/42".chars() {
        client.key(letter(c));
    }
    client.pump(|c| c.drawn().contains(" /42_ "));
    // 142, then 42: both far back in the history.
    client.key(KeyEvent::press(Key::Enter, Mods::empty()));
    client.pump(|c| shows(c, "142") && !shows(c, "200") && c.drawn().contains("1/2 "));
    client.key(KeyEvent::press(
        Key::Char(KeyChar::new('N').unwrap()),
        Mods::SHIFT,
    ));
    client.pump(|c| shows(c, "42") && c.drawn().contains("2/2 "));
    // Leaving returns to the live screen.
    client.key(KeyEvent::press(Key::Escape, Mods::empty()));
    client.pump(|c| shows(c, "200"));
    let kill = Command::Session(SessionCommand::KillSession);
    assert!(matches!(
        command(&paths, kill),
        ServerMessage::SessionEnded(_)
    ));
    daemon.join().unwrap().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

/// Alt-e opens the focused pane's history in the editor, in the pane's
/// place, from a private file that is gone once the editor exits; the pane
/// is back, its shell still running.
#[test]
fn alt_e_edits_the_history_in_the_panes_place() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = root("edit");
    let daemon = start(&root);
    let paths = session(&root);
    let mut client = Client::attach(&paths);
    client.pump(|c| c.panes() == 1);
    for c in "echo plx-$((6*7))".chars() {
        client.key(letter(c));
    }
    client.key(KeyEvent::press(Key::Enter, Mods::empty()));
    client.pump(|c| c.screen().contains("plx-42"));
    let shell = client.focus();
    client.key(KeyEvent::press(
        Key::Char(KeyChar::new('e').unwrap()),
        Mods::ALT,
    ));
    // The test's editor shows the file until Enter, in the same tab.
    client.pump(|c| c.focus() != shell && c.screen().contains("plx-42"));
    let files: Vec<_> = std::fs::read_dir(&paths.runtime)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().contains("scrollback-"))
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let mode = std::fs::metadata(&files[0]).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let history = std::fs::read_to_string(&files[0]).unwrap();
    // The shell's prompt may land before or after the echoed command.
    assert!(history.contains("plx-42\n"), "{history:?}");
    assert_eq!(client.tabs(), 1);
    client.key(KeyEvent::press(Key::Enter, Mods::empty()));
    client.pump(|c| c.focus() == shell && c.screen().contains("plx-42"));
    // The shell ran on: it still answers.
    for c in "echo again-$((1+1))".chars() {
        client.key(letter(c));
    }
    client.key(KeyEvent::press(Key::Enter, Mods::empty()));
    client.pump(|c| c.screen().contains("again-2"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while files[0].exists() {
        assert!(Instant::now() < deadline, "the history file is removed");
        std::thread::sleep(Duration::from_millis(10));
    }
    let kill = Command::Session(SessionCommand::KillSession);
    assert!(matches!(
        command(&paths, kill),
        ServerMessage::SessionEnded(_)
    ));
    daemon.join().unwrap().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

/// A split's shell starts where the split pane's shell is: after a `cd`,
/// the new pane's `pwd` names that directory.
#[test]
fn a_split_starts_in_the_split_panes_directory() {
    let root = root("split-cwd");
    let daemon = start(&root);
    let paths = session(&root);
    let mut client = Client::attach(&paths);
    client.pump(|c| c.panes() == 1);
    let dir = root.join("work dir");
    std::fs::create_dir_all(&dir).unwrap();
    for c in format!("cd '{}' && echo moved-$((1+1))", dir.display()).chars() {
        client.key(letter(c));
    }
    client.key(KeyEvent::press(Key::Enter, Mods::empty()));
    client.pump(|c| c.screen().contains("moved-2"));
    let split = Command::Session(SessionCommand::Split {
        direction: Direction::Down,
    });
    assert_eq!(command(&paths, split), ServerMessage::Applied);
    client.pump(|c| c.panes() == 2);
    for c in "pwd".chars() {
        client.key(letter(c));
    }
    client.key(KeyEvent::press(Key::Enter, Mods::empty()));
    let shown = dir.display().to_string();
    // The prompt may land on the same line; the `cd` line names it too.
    client.pump(|c| {
        c.screen()
            .lines()
            .any(|row| row.trim_end().ends_with(&shown) && !row.contains("cd "))
    });
    let kill = Command::Session(SessionCommand::KillSession);
    assert!(matches!(
        command(&paths, kill),
        ServerMessage::SessionEnded(_)
    ));
    daemon.join().unwrap().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

/// A click on a tab's label in the status line picks that tab.
#[test]
fn clicking_a_tab_label_picks_the_tab() {
    use plx_emulator::{MouseAction, MouseButton, MouseEvent, Position};
    let root = root("tab-click");
    let daemon = start(&root);
    let paths = session(&root);
    let mut client = Client::attach(&paths);
    client.pump(|c| c.panes() == 1);
    let new_tab = Command::Session(SessionCommand::NewTab);
    assert_eq!(command(&paths, new_tab), ServerMessage::Applied);
    client.pump(|c| c.tabs() == 2 && c.active() == Some(1));
    // 80×24: the status line is the last row; " NORMAL " then " Tab #1 ".
    let press = |action| MouseEvent {
        action,
        button: MouseButton::Left,
        position: Position { x: 10, y: 23 },
        mods: Mods::empty(),
    };
    client.mouse(press(MouseAction::Press));
    client.mouse(press(MouseAction::Release));
    client.pump(|c| c.active() == Some(0));
    let kill = Command::Session(SessionCommand::KillSession);
    assert!(matches!(
        command(&paths, kill),
        ServerMessage::SessionEnded(_)
    ));
    daemon.join().unwrap().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

/// A program's progress report (OSC 9;4, as cargo or systemd send it)
/// reaches the client's terminal, rebuilt by the client (ST-ended).
#[test]
fn a_programs_progress_report_reaches_the_clients_terminal() {
    let root = root("osc94");
    let daemon = start(&root);
    let paths = session(&root);
    let mut client = Client::attach(&paths);
    client.pump(|c| c.panes() == 1);
    for c in r"printf '\033]9;4;1;42\007'".chars() {
        client.key(letter(c));
    }
    client.key(KeyEvent::press(Key::Enter, Mods::empty()));
    client.pump(|c| c.drawn().contains("\x1b]9;4;1;42\x1b\\"));
    let kill = Command::Session(SessionCommand::KillSession);
    assert!(matches!(
        command(&paths, kill),
        ServerMessage::SessionEnded(_)
    ));
    daemon.join().unwrap().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

/// A program's own clipboard write (OSC 52, as Claude Code or Neovim
/// copies) reaches the client's terminal as OSC 52.
#[test]
fn a_programs_clipboard_write_reaches_the_clients_terminal() {
    let root = root("osc52");
    let daemon = start(&root);
    let paths = session(&root);
    let mut client = Client::attach(&paths);
    client.pump(|c| c.panes() == 1);
    // "hello from plx", base64.
    for c in r"printf '\033]52;c;aGVsbG8gZnJvbSBwbHg=\007'".chars() {
        client.key(letter(c));
    }
    client.key(KeyEvent::press(Key::Enter, Mods::empty()));
    client.pump(|c| c.drawn().contains("\x1b]52;c;aGVsbG8gZnJvbSBwbHg=\x1b\\"));
    let kill = Command::Session(SessionCommand::KillSession);
    assert!(matches!(
        command(&paths, kill),
        ServerMessage::SessionEnded(_)
    ));
    daemon.join().unwrap().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

/// Hovering (motion with no button held) reaches a program that asked for
/// every movement (mode 1003), as an SGR report relative to its pane.
#[test]
fn hovering_reaches_a_program_that_asks_for_it() {
    use plx_emulator::{MouseAction, MouseButton, MouseEvent, Position};
    let root = root("hover");
    let daemon = start(&root);
    let paths = session(&root);
    let mut client = Client::attach(&paths);
    client.pump(|c| c.panes() == 1);
    // Raw mode: `cat` gets the report at once, not at a newline.
    for c in r"printf '\033[?1003h\033[?1006h'; stty raw -echo; cat -v".chars() {
        client.key(letter(c));
    }
    client.key(KeyEvent::press(Key::Enter, Mods::empty()));
    // Hover until the program, once its modes are on, shows the report.
    // Button 35: motion with none held; 1-based column 5, row 7.
    let hover = MouseEvent {
        action: MouseAction::Motion,
        button: MouseButton::None,
        position: Position { x: 4, y: 6 },
        mods: Mods::empty(),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !client.screen().contains("^[[<35;5;7M") {
        assert!(
            Instant::now() < deadline,
            "no hover report:\n{}",
            client.screen()
        );
        client.mouse(hover);
        std::thread::sleep(Duration::from_millis(50));
        client.pump_for(Duration::from_millis(50));
    }
    let kill = Command::Session(SessionCommand::KillSession);
    assert!(matches!(
        command(&paths, kill),
        ServerMessage::SessionEnded(_)
    ));
    daemon.join().unwrap().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

/// Decodes standard base64, as OSC 52 carries the clipboard.
fn base64(text: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let (mut bits, mut held) = (0_u32, 0_u32);
    for byte in text.bytes().filter(|&byte| byte != b'=') {
        let value = ALPHABET.iter().position(|&a| a == byte).unwrap();
        bits = (bits << 6) | u32::try_from(value).unwrap();
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push(u8::try_from(bits >> held).unwrap());
            bits &= (1 << held) - 1;
        }
    }
    out
}

/// The text the client last put on its terminal's clipboard, if any.
fn clipboard_or_none(client: &Client) -> Option<String> {
    let terminal = String::from_utf8_lossy(&client.shell.terminal);
    let (_, last) = terminal.rsplit_once("\x1b]52;c;")?;
    let (encoded, _) = last.split_once("\x1b\\")?;
    Some(String::from_utf8(base64(encoded)).unwrap())
}

/// Dragging a selection up to the top edge and holding it there scrolls
/// the history into view and selects it, so more than the screen showed is
/// copied: here all of it, from the command line to the last number.
#[test]
fn a_selection_held_at_the_top_edge_scrolls_and_copies_the_history() {
    use plx_emulator::{MouseAction, MouseButton, MouseEvent, Position};
    let root = root("autoscroll");
    let daemon = start(&root);
    let paths = session(&root);
    let mut client = Client::attach(&paths);
    // Typed before the prompt, the line is in the history twice: echoed as
    // typed ahead, then again after the prompt.
    client.pump(|c| c.panes() == 1 && c.screen().contains('$'));
    for c in "seq 1 100".chars() {
        client.key(letter(c));
    }
    client.key(KeyEvent::press(Key::Enter, Mods::empty()));
    client.pump(|c| shows(c, "100"));
    let y = u16::try_from(
        client
            .screen()
            .lines()
            .position(|row| row.trim_end() == "100")
            .unwrap(),
    )
    .unwrap();
    let left = |action, x, y| MouseEvent {
        action,
        button: MouseButton::Left,
        position: Position { x, y },
        mods: Mods::empty(),
    };
    // Press after "100", drag to the pane's top row (the screen's: a
    // pointer above the window is reported there), and hold it still.
    client.mouse(left(MouseAction::Press, 5, y));
    client.mouse(left(MouseAction::Motion, 0, y / 2));
    client.mouse(left(MouseAction::Motion, 0, 0));
    client.pump(|c| {
        c.screen()
            .lines()
            .next()
            .is_some_and(|top| top.contains("seq 1 100"))
    });
    client.mouse(left(MouseAction::Release, 0, 0));
    client.pump(|c| clipboard_or_none(c).is_some());
    let copied = clipboard_or_none(&client).unwrap();
    let numbers: Vec<String> = (1..=100).map(|n| format!("{n}")).collect();
    let (first, rest) = copied.split_once('\n').unwrap();
    assert!(first.ends_with("seq 1 100"), "{copied:?}");
    assert_eq!(rest, numbers.join("\n"), "{copied:?}");
    let kill = Command::Session(SessionCommand::KillSession);
    assert!(matches!(
        command(&paths, kill),
        ServerMessage::SessionEnded(_)
    ));
    daemon.join().unwrap().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_wheel_scrolls_a_shells_history() {
    use plx_emulator::{MouseAction, MouseButton, MouseEvent, Position};
    let root = root("wheel");
    let daemon = start(&root);
    let paths = session(&root);
    let mut client = Client::attach(&paths);
    client.pump(|c| c.panes() == 1);
    for c in "seq 1 200".chars() {
        client.key(letter(c));
    }
    client.key(KeyEvent::press(Key::Enter, Mods::empty()));
    client.pump(|c| shows(c, "200"));
    let notch = |button| MouseEvent {
        action: MouseAction::Press,
        button,
        position: Position { x: 5, y: 5 },
        mods: Mods::empty(),
    };
    // Ten notches up: 30 rows back.
    for _ in 0..10 {
        client.mouse(notch(MouseButton::WheelUp));
    }
    client.pump(|c| !shows(c, "200") && shows(c, "170"));
    for _ in 0..10 {
        client.mouse(notch(MouseButton::WheelDown));
    }
    client.pump(|c| shows(c, "200"));
    let kill = Command::Session(SessionCommand::KillSession);
    assert!(matches!(
        command(&paths, kill),
        ServerMessage::SessionEnded(_)
    ));
    daemon.join().unwrap().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn dragging_over_text_copies_it_to_the_clients_clipboard() {
    use plx_emulator::{MouseAction, MouseButton, MouseEvent, Position};
    let root = root("select");
    let daemon = start(&root);
    let paths = session(&root);
    let mut client = Client::attach(&paths);
    client.pump(|c| c.panes() == 1);
    for c in "echo hel$((1))lo".chars() {
        client.key(letter(c));
    }
    client.key(KeyEvent::press(Key::Enter, Mods::empty()));
    client.pump(|c| shows(c, "hel1lo"));
    let y = u16::try_from(
        client
            .screen()
            .lines()
            .position(|row| row.trim_end() == "hel1lo")
            .unwrap(),
    )
    .unwrap();
    let event = |action, x| MouseEvent {
        action,
        button: MouseButton::Left,
        position: Position { x, y },
        mods: Mods::empty(),
    };
    client.mouse(event(MouseAction::Press, 0));
    client.mouse(event(MouseAction::Motion, 5));
    client.mouse(event(MouseAction::Release, 5));
    // "hel1lo" in base64.
    client
        .pump(|c| String::from_utf8_lossy(&c.shell.terminal).contains("\x1b]52;c;aGVsMWxv\x1b\\"));
    let kill = Command::Session(SessionCommand::KillSession);
    assert!(matches!(
        command(&paths, kill),
        ServerMessage::SessionEnded(_)
    ));
    daemon.join().unwrap().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
