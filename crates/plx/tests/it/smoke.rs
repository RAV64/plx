//! The real `plx` binary in a pseudoterminal, as a user runs it: `plx`
//! starts the daemon, attaches, and the CLI talks to the same session.

use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use plx_core::event::ExitStatus;
use plx_daemon::pty::{Program, Pty, Read};
use plx_emulator::{CellText, Emulator, GridSize, Version};
use plx_ghostty::{GhosttyEmulator, GraphemeClustering, Options, Scrollback};

const PLX: &str = env!("CARGO_BIN_EXE_plx");

/// A private home for one test's sessions, ended however the test ends.
struct Home {
    root: PathBuf,
}

impl Home {
    fn new(test: &str) -> Self {
        let root = std::env::temp_dir().join(format!("plx-smoke-{test}-{}", std::process::id()));
        if root.exists() {
            std::fs::remove_dir_all(&root).unwrap();
        }
        std::fs::create_dir_all(root.join("run")).unwrap();
        // The configured shell wins over SHELL, which names no program.
        std::fs::create_dir_all(root.join("config/plx")).unwrap();
        std::fs::write(
            root.join("config/plx/config.toml"),
            "default_shell = \"/bin/sh\"\n",
        )
        .unwrap();
        Self { root }
    }

    fn env(&self) -> [(&'static str, OsString); 4] {
        [
            ("XDG_RUNTIME_DIR", self.root.join("run").into_os_string()),
            ("XDG_STATE_HOME", self.root.join("state").into_os_string()),
            ("XDG_CONFIG_HOME", self.root.join("config").into_os_string()),
            ("SHELL", OsString::from("/nonexistent/shell")),
        ]
    }

    /// `plx <args>` outside any terminal, as a script runs it.
    fn plx(&self, args: &[&str]) -> Output {
        Command::new(PLX)
            .args(args)
            .envs(self.env())
            .env_remove("PLX_SESSION")
            .output()
            .unwrap()
    }

    fn log(&self) -> PathBuf {
        self.root.join("state/plx/main.log")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        // A daemon left by a failed test would outlive it.
        let killed = self.plx(&["kill-session"]);
        if killed.status.success() {
            wait_until(|| !self.root.join("run/plx/v1/main.sock").exists());
        }
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn wait_until(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn text(output: &[u8]) -> &str {
    std::str::from_utf8(output).unwrap()
}

/// The user's terminal: `plx` running in it, and what it shows.
struct Screen {
    pty: Pty,
    terminal: GhosttyEmulator,
    version: Version,
}

impl Screen {
    fn run(home: &Home, args: &[&str]) -> Self {
        let env = home.env();
        let pairs: Vec<(&str, &str)> = env
            .iter()
            .map(|(key, value)| (*key, value.to_str().unwrap()))
            .chain([("TERM", "xterm-256color")])
            .collect();
        // Not inside whatever plx session runs the tests: plx would refuse
        // to nest.
        let program = Program {
            path: "env".into(),
            args: ["-u", "PLX_SESSION", PLX]
                .iter()
                .chain(args)
                .map(OsString::from)
                .collect(),
        };
        let size = GridSize::new(100.try_into().unwrap(), 30.try_into().unwrap()).unwrap();
        let options = Options {
            scrollback: Scrollback::Disabled,
            continuation_max_bytes: NonZeroUsize::new(4096).unwrap(),
            grapheme_clustering: GraphemeClustering::Enabled,
        };
        Self {
            pty: Pty::spawn(&program, size, &pairs, None).unwrap(),
            terminal: GhosttyEmulator::new(size, &options).unwrap(),
            version: Version::ZERO,
        }
    }

    fn type_text(&self, text: &[u8]) {
        let mut written = 0;
        while written < text.len() {
            written += self.pty.write(&text[written..]).unwrap();
        }
    }

    /// Reads what plx drew; whether its terminal closed.
    fn pump(&mut self) -> Read {
        let mut buf = [0_u8; 16384];
        let read = self.pty.read(&mut buf).unwrap();
        if let Read::Data(n) = read {
            self.terminal.feed(&buf[..n]);
            self.version = self.version.next();
            self.terminal.sync(self.version).unwrap();
        }
        read
    }

    fn wait_for(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if matches!(self.pump(), Read::Empty) {
                std::thread::sleep(Duration::from_millis(5));
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

    /// Waits for plx to exit, reading what it draws until then.
    fn exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if self.pump() == Read::Empty {
                std::thread::sleep(Duration::from_millis(5));
            }
            if let Some(status) = self.pty.exit_status_or_none().unwrap() {
                while let Read::Data(_) = self.pump() {}
                return status;
            }
            assert!(Instant::now() < deadline, "plx did not exit");
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
fn plx_starts_a_session_the_cli_drives_it_and_it_outlives_the_client() {
    let home = Home::new("session");
    assert_eq!(text(&home.plx(&["ls"]).stdout), "");
    let refused = home.plx(&["zoom"]);
    assert!(!refused.status.success());
    assert!(text(&refused.stderr).contains("no plx session main is running"));

    // `plx` starts the daemon and attaches.
    let mut screen = Screen::run(&home, &[]);
    screen.wait_for("NORMAL");
    assert_eq!(text(&home.plx(&["ls"]).stdout), "main\n");

    // The pane knows its session; the CLI reaches it from outside.
    screen.type_text(b"echo in-$PLX_SESSION\r");
    screen.wait_for("in-main");
    let split = home.plx(&["split", "right"]);
    assert!(split.status.success(), "{}", text(&split.stderr));
    let missing = home.plx(&["tab", "9"]);
    assert!(!missing.status.success());
    assert!(text(&missing.stderr).contains("there is no tab 9"));
    assert!(!home.plx(&["new"]).status.success());

    // Detach: the client leaves, the session stays.
    // Detach: Ctrl-m (session mode; sent as the kitty keyboard protocol
    // does, since legacy Ctrl-m is Enter), then `d`.
    screen.type_text(b"\x1b[109;5ud");
    let status = screen.exit();
    assert_eq!(status, ExitStatus::Code(0), "{}", screen.text());
    assert!(screen.text().contains("[detached from plx session main]"));
    assert_eq!(text(&home.plx(&["ls"]).stdout), "main\n");

    // `plx events` follows the session; nothing acknowledges the
    // subscription, so commands repeat until their events show.
    let mut events = Command::new(PLX)
        .arg("events")
        .envs(home.env())
        .env_remove("PLX_SESSION")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let lines = {
        let stdout = events.stdout.take().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufRead::lines(std::io::BufReader::new(stdout)) {
                if send.send(line.unwrap()).is_err() {
                    return;
                }
            }
        });
        receive
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    'subscribed: loop {
        assert!(Instant::now() < deadline, "no events");
        assert!(home.plx(&["zoom"]).status.success());
        while let Ok(line) = lines.recv_timeout(Duration::from_millis(100)) {
            if line.contains("ZoomChanged") {
                break 'subscribed;
            }
        }
    }

    // Reattach, then end the session from the CLI: the client hears it.
    let mut again = Screen::run(&home, &["attach"]);
    again.wait_for("NORMAL");
    assert!(home.plx(&["kill-session"]).status.success());
    assert!(events.wait().unwrap().success());
    let rest: Vec<String> = lines.iter().collect();
    assert!(
        rest.iter().any(|line| line.contains("ClientAttached")),
        "{rest:?}"
    );
    assert!(rest.last().unwrap().contains("SessionEnded"), "{rest:?}");
    assert_eq!(again.exit(), ExitStatus::Code(0));
    assert!(again.text().contains("[plx session main ended]"));
    wait_until(|| text(&home.plx(&["ls"]).stdout).is_empty());
    let log = std::fs::read_to_string(home.log()).unwrap();
    assert!(log.contains("serving"), "{log}");
}

#[test]
fn a_session_that_cannot_start_says_why() {
    let home = Home::new("broken");
    // A runtime directory others can enter is refused.
    let run = home.root.join("run/plx/v1");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut screen = Screen::run(&home, &[]);
    assert_eq!(screen.exit(), ExitStatus::Code(1));
    let shown = screen.text();
    assert!(
        shown.contains("must be owned by you and private"),
        "{shown}"
    );
    assert!(shown.contains("the plx daemon did not start"), "{shown}");
}
