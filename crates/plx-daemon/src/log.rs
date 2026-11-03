//! The daemon's log and panic hook.
//!
//! Logs go through `tracing` to `$XDG_STATE_HOME/plx/{session}.log`, written
//! by a background thread that blocks rather than drops lines. A panic is
//! written to the same file through a pre-opened descriptor, the socket is
//! removed so clients start afresh, and the process aborts: a half-applied
//! step is never continued.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;

use tracing_appender::non_blocking::{NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::EnvFilter;

use crate::paths::Paths;

/// The filter when `PLX_LOG` is not set.
pub const DEFAULT_FILTER: &str = "info";

/// Why logging could not start.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("opening the log {path}: {source}")]
    Open { path: PathBuf, source: io::Error },
    #[error("PLX_LOG is not valid UTF-8")]
    FilterNotUnicode,
    #[error("PLX_LOG is not a valid filter: {0}")]
    Filter(#[from] tracing_subscriber::filter::ParseError),
    #[error("a logger is already installed")]
    AlreadyInstalled,
}

/// Keeps the log writer alive; dropping it flushes the log.
#[derive(Debug)]
#[must_use = "dropping the guard stops logging"]
pub struct Logging {
    _writer: WorkerGuard,
}

/// The filter from `PLX_LOG`.
///
/// # Errors
///
/// When `PLX_LOG` is set but not a valid filter.
pub fn filter() -> Result<EnvFilter, LogError> {
    match std::env::var("PLX_LOG") {
        Ok(directives) => Ok(EnvFilter::try_new(directives)?),
        // Unset: the named default, not a guess.
        Err(std::env::VarError::NotPresent) => Ok(EnvFilter::try_new(DEFAULT_FILTER)?),
        Err(std::env::VarError::NotUnicode(_)) => Err(LogError::FilterNotUnicode),
    }
}

/// Starts logging to the session's log and installs the panic hook. Call
/// once, at the start of the daemon process.
///
/// # Errors
///
/// When the log cannot be opened, the filter is invalid, or a logger is
/// already installed.
pub fn install(paths: &Paths) -> Result<Logging, LogError> {
    let filter = filter()?;
    let open = |path: &PathBuf| -> Result<File, LogError> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .map_err(|source| LogError::Open {
                path: PathBuf::from(path.as_os_str()),
                source,
            })
    };
    let file = open(&paths.log)?;
    // A second descriptor for the panic hook: raw writes, no logger thread.
    let crash_log: OwnedFd = open(&paths.log)?.into();
    let (writer, guard) = NonBlockingBuilder::default()
        .lossy(false)
        .thread_name("plx-log")
        .finish(file);
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(filter)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|_already| LogError::AlreadyInstalled)?;
    install_panic_hook(crash_log, PathBuf::from(paths.socket.as_os_str()));
    Ok(Logging { _writer: guard })
}

/// On a panic: the default report, then the message, location and backtrace
/// into the log, the socket removed, and an abort.
fn install_panic_hook(log: OwnedFd, socket: PathBuf) {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        let report = format!(
            "plx daemon panicked: {info}\n{}\n",
            std::backtrace::Backtrace::force_capture()
        );
        write_all(&log, report.as_bytes());
        if let Err(error) = std::fs::remove_file(&socket) {
            write_all(
                &log,
                format!("removing the socket {}: {error}\n", socket.display()).as_bytes(),
            );
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "the daemon's panic hook: a half-applied step is never continued"
        )]
        std::process::abort();
    }));
}

/// Writes all of `bytes` to the crash log. A failed write ends it: in a
/// panic hook there is nowhere left to report that to.
fn write_all(log: &OwnedFd, bytes: &[u8]) {
    let mut written = 0;
    while let Some(rest) = bytes.get(written..).filter(|rest| !rest.is_empty()) {
        match rustix::io::write(log, rest) {
            Ok(0) | Err(_) => return,
            Ok(n) => written += n,
        }
    }
}
