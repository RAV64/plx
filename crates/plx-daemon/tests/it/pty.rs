use std::time::{Duration, Instant};

use plx_core::event::ExitStatus;
use plx_daemon::pty::{Program, Pty, Read};
use plx_emulator::GridSize;

fn size(cols: u16, rows: u16) -> GridSize {
    GridSize::new(cols.try_into().unwrap(), rows.try_into().unwrap()).unwrap()
}

fn sh(script: &str) -> Program {
    Program {
        path: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
    }
}

/// Everything the program writes until its terminal closes.
fn output(pty: &Pty) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut all = Vec::new();
    let mut buf = [0_u8; 4096];
    loop {
        assert!(
            Instant::now() < deadline,
            "no hang-up: {:?}",
            String::from_utf8_lossy(&all)
        );
        match pty.read(&mut buf).unwrap() {
            Read::Data(n) => all.extend_from_slice(&buf[..n]),
            Read::Empty => std::thread::sleep(Duration::from_millis(5)),
            Read::Closed => return String::from_utf8(all).unwrap(),
        }
    }
}

fn exit(pty: &Pty) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = pty.exit_status_or_none().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "the program did not exit");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_program_writes_to_its_terminal_and_exits() {
    let pty = Pty::spawn(&sh("echo hello; exit 3"), size(80, 24), &[], None).unwrap();
    assert!(output(&pty).contains("hello"));
    assert_eq!(exit(&pty), ExitStatus::Code(3));
}

#[test]
fn the_terminal_is_the_programs_controlling_terminal() {
    // `tty` names stdin's terminal; reading /dev/tty works only with a
    // controlling terminal.
    let pty = Pty::spawn(
        &sh("tty; : < /dev/tty && echo controlling"),
        size(80, 24),
        &[],
        None,
    )
    .unwrap();
    let out = output(&pty);
    assert!(out.contains("/dev/pts/"), "{out:?}");
    assert!(out.contains("controlling"), "{out:?}");
}

#[test]
fn the_program_starts_with_no_blocked_or_ignored_signals() {
    let pty = Pty::spawn(
        &sh("grep -E '^Sig(Blk|Ign)' /proc/self/status"),
        size(80, 24),
        &[],
        None,
    )
    .unwrap();
    let out = output(&pty);
    for line in out.lines() {
        let (name, mask) = line.split_once(':').unwrap();
        assert_eq!(
            u64::from_str_radix(mask.trim(), 16).unwrap(),
            0,
            "{name} in {out:?}"
        );
    }
    assert_eq!(out.lines().count(), 2, "{out:?}");
}

#[test]
fn a_resize_reaches_the_program() {
    let pty = Pty::spawn(&sh("sleep 0.3; stty size"), size(80, 24), &[], None).unwrap();
    pty.resize(size(100, 30)).unwrap();
    assert!(output(&pty).contains("30 100"));
}

#[test]
fn a_hang_up_ends_the_program_by_sighup() {
    let pty = Pty::spawn(&sh("exec sleep 30"), size(80, 24), &[], None).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    pty.hang_up().unwrap();
    assert_eq!(exit(&pty), ExitStatus::Signal(1.try_into().unwrap()));
}

#[test]
fn the_environment_is_passed() {
    let pty = Pty::spawn(
        &sh("echo $TERM"),
        size(80, 24),
        &[("TERM", "xterm-256color")],
        None,
    )
    .unwrap();
    assert!(output(&pty).contains("xterm-256color"));
}

/// A program can start in a given directory, and the terminal tells the
/// directory of what runs in its foreground (here the shell, after a `cd`).
#[test]
fn the_working_directory_is_given_and_followed() {
    let pty = Pty::spawn(
        &sh("pwd; cd /proc && sleep 0.5"),
        size(80, 24),
        &[],
        Some(std::path::Path::new("/tmp")),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    // `sleep` is in the foreground, started from /proc.
    assert_eq!(
        pty.working_directory_or_none(),
        Some(std::path::PathBuf::from("/proc"))
    );
    assert!(output(&pty).starts_with("/tmp"));
}
