//! The real client against the real daemon, in-process, with a PTY as the
//! user's terminal: the test types into the PTY's master and reads what the
//! client draws through a headless libghostty.

use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use plx_client::session::{Outcome, attach, greet};
use plx_client_core::compose::{Capabilities, ColorDepth, ProgressBars, UnderlineStyles};
use plx_daemon::paths::{Env, Paths, SessionName};
use plx_daemon::pty::Program;
use plx_daemon::reactor::{Config, run};
use plx_emulator::{CellText, Emulator, GridSize, Version};
use plx_ghostty::{GhosttyEmulator, GraphemeClustering, Options, Scrollback};
use rustix::pty::OpenptFlags;
use rustix::termios::Winsize;

fn root(test: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("plx-attach-{test}-{}", std::process::id()));
    if root.exists() {
        std::fs::remove_dir_all(&root).unwrap();
    }
    root
}

fn session(root: &Path) -> Paths {
    let env = Env {
        runtime_dir: Some(OsString::from(root.join("run"))),
        state_home: Some(OsString::from(root.join("state"))),
        home: None,
    };
    Paths::of(&SessionName::new("test").unwrap(), &env).unwrap()
}

/// A pseudoterminal standing in for the user's terminal: (master, slave).
fn terminal(cols: u16, rows: u16) -> (OwnedFd, OwnedFd) {
    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
    let master = rustix::pty::openpt(flags).unwrap();
    rustix::pty::grantpt(&master).unwrap();
    rustix::pty::unlockpt(&master).unwrap();
    rustix::termios::tcsetwinsize(
        &master,
        Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
    let slave = rustix::pty::ioctl_tiocgptpeer(&master, flags).unwrap();
    rustix::io::ioctl_fionbio(&master, true).unwrap();
    (master, slave)
}

/// The user's terminal: what the client drew, interpreted.
struct Screen {
    master: OwnedFd,
    terminal: GhosttyEmulator,
    version: Version,
}

impl Screen {
    fn new(master: OwnedFd, cols: u16, rows: u16) -> Self {
        let options = Options {
            scrollback: Scrollback::Disabled,
            continuation_max_bytes: NonZeroUsize::new(4096).unwrap(),
            grapheme_clustering: GraphemeClustering::Enabled,
        };
        let size = GridSize::new(cols.try_into().unwrap(), rows.try_into().unwrap()).unwrap();
        Self {
            master,
            terminal: GhosttyEmulator::new(size, &options).unwrap(),
            version: Version::ZERO,
        }
    }

    fn type_text(&self, text: &[u8]) {
        let mut written = 0;
        while written < text.len() {
            match rustix::io::write(&self.master, &text[written..]) {
                Ok(n) => written += n,
                Err(rustix::io::Errno::AGAIN) => std::thread::sleep(Duration::from_millis(1)),
                Err(error) => panic!("{error}"),
            }
        }
    }

    /// Reads what the client drew until `text` shows.
    fn wait_for(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut buf = [0_u8; 16384];
        loop {
            match rustix::io::read(&self.master, &mut buf) {
                Ok(n) if n > 0 => {
                    self.terminal.feed(&buf[..n]);
                    self.version = self.version.next();
                    self.terminal.sync(self.version).unwrap();
                }
                Ok(_) | Err(rustix::io::Errno::AGAIN) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
            if self.text().contains(text) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{text:?} never showed:\n{}",
                self.text()
            );
        }
    }

    fn text(&self) -> String {
        let screen = self.terminal.screen();
        let mut out = String::new();
        for row in screen.grid().rows() {
            out.extend(row.iter().map(|&cell| match screen.resolve(cell).text {
                CellText::Char(c) => c,
                CellText::Empty | CellText::Cluster(_) => ' ',
            }));
            out.push('\n');
        }
        out
    }
}

#[test]
fn a_user_attaches_works_and_detaches() {
    let root = root("detach");
    let daemon = std::thread::spawn({
        let config = Config {
            session: SessionName::new("test").unwrap(),
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
    });
    let socket = session(&root).socket;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        if let Ok(stream) = UnixStream::connect(&socket) {
            break stream;
        }
        assert!(Instant::now() < deadline, "the daemon did not start");
        std::thread::sleep(Duration::from_millis(10));
    };
    greet(&mut stream).unwrap();
    let (master, slave) = terminal(80, 24);
    let client = std::thread::spawn(move || {
        let capabilities = Capabilities {
            color: ColorDepth::TrueColor,
            underlines: UnderlineStyles::Styled,
            progress: ProgressBars::Unsupported,
        };
        attach(stream, &slave, capabilities)
    });
    let mut screen = Screen::new(master, 80, 24);
    // The status line shows the mode once the first frame is drawn.
    screen.wait_for("NORMAL");
    screen.type_text(b"echo plx-$((6*7))\r");
    screen.wait_for("plx-42");
    // Detach: Ctrl-m (session mode; sent as the kitty keyboard protocol
    // does, since legacy Ctrl-m is Enter), then `d`.
    screen.type_text(b"\x1b[109;5ud");
    let (outcome, notices) = client.join().unwrap().unwrap();
    assert_eq!(outcome, Outcome::Detached);
    assert_eq!(notices.pastes_too_large, 0);
    // The session lives on; end it through a fresh connection.
    let mut cli = UnixStream::connect(&socket).unwrap();
    greet(&mut cli).unwrap();
    let mut message = Vec::new();
    plx_protocol::client::encode(
        &plx_protocol::client::ClientWire::Command(plx_core::command::Command::Session(
            plx_core::command::SessionCommand::KillSession,
        )),
        &mut message,
    )
    .unwrap();
    std::io::Write::write_all(&mut cli, &message).unwrap();
    daemon.join().unwrap().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
