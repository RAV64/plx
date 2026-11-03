//! Finding a session's daemon, and starting one.
//!
//! The daemon is this binary's `plx daemon --ready`: a child that leaves
//! the terminal's session itself (`setsid`, no double fork) and says it
//! serves with one byte on the stdout pipe. A serving daemon outlives its
//! starter, and init reaps it.

use std::io::{self, Read as _};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use plx_bug::OrBug as _;
use plx_daemon::paths::{Paths, SessionName};
use plx_daemon::reactor::READY;
use rustix::event::{PollFd, PollFlags, Timespec};

/// How long a new daemon has to start serving.
pub const START_TIMEOUT: Duration = Duration::from_secs(5);

/// `plx daemon`'s exit status when another daemon serves the session
/// already (two clients raced to start it).
pub const EXIT_ALREADY_RUNNING: u8 = 3;

/// Why there is no connection to the session.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("{what}: {source}")]
    Io {
        what: &'static str,
        source: io::Error,
    },
    #[error("the plx daemon did not start ({status}); its log is {}", log.display())]
    Failed { status: ExitStatus, log: PathBuf },
    #[error(
        "the plx daemon did not start within {}s; its log is {}",
        START_TIMEOUT.as_secs(),
        log.display()
    )]
    Timeout { log: PathBuf },
}

const fn io_error(what: &'static str) -> impl FnOnce(io::Error) -> StartError {
    move |source| StartError::Io { what, source }
}

/// A connection to the session's daemon, or `None` when none serves it.
///
/// # Errors
///
/// When connecting fails other than by there being no daemon.
pub fn connect_or_none(paths: &Paths) -> Result<Option<UnixStream>, StartError> {
    match UnixStream::connect(&paths.socket) {
        Ok(stream) => Ok(Some(stream)),
        // No socket, or a dead daemon's: nobody serves the session.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(StartError::Io {
            what: "connecting to the session",
            source: error,
        }),
    }
}

/// How [`start`] ended when it did not fail.
#[derive(Debug)]
pub enum Started {
    /// The new daemon serves; connected to it.
    Serving(UnixStream),
    /// Another daemon took the session first (a race between clients).
    Raced,
}

/// Starts the session's daemon and connects once it serves.
///
/// # Errors
///
/// When it cannot be started, fails, or does not serve in time.
pub fn start(session: &SessionName, paths: &Paths) -> Result<Started, StartError> {
    let exe = std::env::current_exe().map_err(io_error("finding the plx binary"))?;
    let mut child = Command::new(exe)
        .args(["daemon", "--ready", "--session", session.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Until it logs, the daemon reports on the user's terminal.
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(io_error("starting the plx daemon"))?;
    let mut ready = child
        .stdout
        .take()
        .or_bug("the daemon's stdout is a pipe: it was asked for");
    let deadline = Instant::now() + START_TIMEOUT;
    let mut byte = [0_u8; 1];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let timeout = Timespec::try_from(left).or_bug("five seconds is a timespec");
        let pipe = ready.as_fd();
        let mut fds = [PollFd::new(&pipe, PollFlags::IN)];
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) => {
                return Err(StartError::Timeout {
                    log: PathBuf::from(paths.log.as_os_str()),
                });
            }
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(errno) => {
                return Err(StartError::Io {
                    what: "waiting for the plx daemon",
                    source: errno.into(),
                });
            }
        }
        match ready.read(&mut byte) {
            Ok(1) if byte == [READY] => break,
            // Ended without serving: why is in its exit status.
            Ok(_) => {
                let status = child
                    .wait()
                    .map_err(io_error("waiting for the plx daemon"))?;
                if status.code() == Some(i32::from(EXIT_ALREADY_RUNNING)) {
                    return Ok(Started::Raced);
                }
                return Err(StartError::Failed {
                    status,
                    log: PathBuf::from(paths.log.as_os_str()),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(StartError::Io {
                    what: "waiting for the plx daemon",
                    source: error,
                });
            }
        }
    }
    let stream =
        UnixStream::connect(&paths.socket).map_err(io_error("connecting to the new plx daemon"))?;
    Ok(Started::Serving(stream))
}

/// Connects to a daemon that another client is starting: it holds the
/// session's lock and binds its socket shortly.
///
/// # Errors
///
/// When it does not serve within [`START_TIMEOUT`].
pub fn await_other(paths: &Paths) -> Result<UnixStream, StartError> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(stream) = connect_or_none(paths)? {
            return Ok(stream);
        }
        if Instant::now() >= deadline {
            return Err(StartError::Timeout {
                log: PathBuf::from(paths.log.as_os_str()),
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
