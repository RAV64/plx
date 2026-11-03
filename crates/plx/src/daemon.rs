//! `plx daemon`: one session's daemon process around the reactor
//! (`plx_daemon::reactor`): its own session away from the terminal, its log
//! as stderr, and readiness on the stdout pipe its starter waits on.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd as _, OwnedFd};
use std::process::ExitCode;

use plx_daemon::log::{self, LogError};
use plx_daemon::paths::{Env, Paths, PathsError, SessionName};
use plx_daemon::pty::Program;
use plx_daemon::reactor::{self, Config, DaemonError};
use plx_ghostty::Scrollback;

use crate::cli::Readiness;
use crate::start::EXIT_ALREADY_RUNNING;

/// Why the daemon could not serve.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("leaving the terminal's session: {0}")]
    Setsid(rustix::io::Errno),
    #[error(transparent)]
    Paths(#[from] PathsError),
    #[error("the log: {0}")]
    Log(#[from] LogError),
    #[error("{what}: {source}")]
    Io {
        what: &'static str,
        source: io::Error,
    },
    #[error(transparent)]
    Daemon(DaemonError),
}

const fn io_error(what: &'static str) -> impl FnOnce(io::Error) -> ServeError {
    move |source| ServeError::Io { what, source }
}

/// Serves `session` until it ends.
///
/// # Errors
///
/// When it cannot start serving, or the reactor fails.
pub fn serve(
    session: SessionName,
    shell: Program,
    editor: Option<OsString>,
    scrollback: Scrollback,
    ready: Readiness,
) -> Result<ExitCode, ServeError> {
    // Away from the starter's terminal: its hang-up and job control are
    // not the daemon's. The starter did not make it a group leader, so this
    // succeeds.
    rustix::process::setsid().map_err(ServeError::Setsid)?;
    let paths = Paths::of(&session, &Env::current())?;
    paths.prepare()?;
    let _logging = log::install(&paths)?;
    let ready = match ready {
        Readiness::Stdout => Some(
            io::stdout()
                .as_fd()
                .try_clone_to_owned()
                .map_err(io_error("keeping the readiness pipe"))?,
        ),
        Readiness::Nobody => None,
    };
    // The readiness pipe closes once the reactor has written to it, so
    // nothing else may hold it: stdout becomes /dev/null.
    let null = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .map_err(io_error("opening /dev/null"))?;
    rustix::stdio::dup2_stdout(&null).map_err(|errno| ServeError::Io {
        what: "replacing stdout",
        source: errno.into(),
    })?;
    // Reports that bypass tracing (the default panic message) go to the
    // log too, not to a terminal the daemon has left.
    let log: OwnedFd = File::options()
        .append(true)
        .open(&paths.log)
        .map_err(io_error("opening the log as stderr"))?
        .into();
    rustix::stdio::dup2_stderr(&log).map_err(|errno| ServeError::Io {
        what: "replacing stderr",
        source: errno.into(),
    })?;
    tracing::info!(session = session.as_str(), shell = %shell.path.display(), "starting");
    match reactor::run(Config {
        session,
        paths,
        program: shell,
        editor,
        scrollback,
        ready,
    }) {
        Ok(()) => {
            tracing::info!("the session ended");
            Ok(ExitCode::SUCCESS)
        }
        Err(DaemonError::AlreadyRunning) => {
            tracing::info!("another daemon serves the session");
            Ok(ExitCode::from(EXIT_ALREADY_RUNNING))
        }
        Err(error) => {
            tracing::error!(%error, "the daemon failed");
            Err(ServeError::Daemon(error))
        }
    }
}
