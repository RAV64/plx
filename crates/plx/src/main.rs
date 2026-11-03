//! The `plx` binary: the CPU checked, the command line parsed once, then a
//! client, one command, or the daemon. Only this file writes the user's
//! messages and decides the exit status.

use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;
use std::process::ExitCode;

use plx::cli::{self, CliEnv, CliError, Invocation};
use plx::config::{self, ConfigFileError, Created};
use plx::cpu;
use plx::daemon::{self, ServeError};
use plx::start::{self, StartError, Started};
use plx_client::capabilities::{self, TerminalEnv};
use plx_client::session::{self, ClientError, Followed, Outcome, Reply};
use plx_client::terminal;
use plx_core::command::SessionCommand;
use plx_core::rejection::Rejection;
use plx_daemon::paths::{Env, Paths, PathsError, SessionName};
use plx_daemon::reactor::SESSION_VAR;

fn main() -> ExitCode {
    if let Some(missing) = cpu::first_missing_or_none() {
        cpu::report(missing);
        return ExitCode::FAILURE;
    }
    run()
}

/// Everything after the CPU check, kept out of `main` so that nothing
/// before the check may use x86-64-v3.
#[inline(never)]
fn run() -> ExitCode {
    let session_var = std::env::var_os(SESSION_VAR);
    let env = CliEnv {
        session: session_var.as_deref(),
    };
    let result = match cli::parse(std::env::args_os().skip(1), &env) {
        Ok(invocation) => invoke(invocation),
        Err(error) => Err(MainError::Cli(error)),
    };
    match result {
        Ok(code) => code,
        Err(error @ MainError::Cli(_)) => {
            say(format_args!(
                "plx: {error}\nRun `plx help` for the commands.\n"
            ));
            ExitCode::FAILURE
        }
        Err(error) => {
            say(format_args!("plx: {error}\n"));
            ExitCode::FAILURE
        }
    }
}

/// Everything `main` can fail with.
#[derive(Debug, thiserror::Error)]
enum MainError {
    #[error(transparent)]
    Cli(CliError),
    #[error(transparent)]
    Paths(#[from] PathsError),
    #[error(transparent)]
    Start(#[from] StartError),
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error(transparent)]
    Serve(#[from] ServeError),
    #[error(transparent)]
    Ghostty(#[from] plx_ghostty::BuildError),
    #[error(transparent)]
    Config(#[from] ConfigFileError),
    #[error("no plx session {} is running", .0.as_str())]
    NotRunning(SessionName),
    #[error("plx session {} is running already", .0.as_str())]
    Running(SessionName),
    #[error("plx needs a terminal: opening /dev/tty: {0}")]
    NoTerminal(io::Error),
    #[error("listing the sessions in {}: {source}", dir.display())]
    List { dir: PathBuf, source: io::Error },
    #[error("the session refused: {0}")]
    Rejected(Rejection),
    #[error("plx daemon hit a bug; see its log at {}", .0.display())]
    DaemonLost(PathBuf),
}

/// How the user asked to reach a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reach {
    /// Attach, starting the session if needed.
    Open,
    /// Start the session; it must not be running.
    New,
    /// Attach to the running session.
    Attach,
}

fn invoke(invocation: Invocation) -> Result<ExitCode, MainError> {
    match invocation {
        Invocation::Open(session) => attach(session, Reach::Open),
        Invocation::New(session) => attach(session, Reach::New),
        Invocation::Attach(session) => attach(session, Reach::Attach),
        Invocation::List => list(),
        Invocation::Events(session) => events(session),
        Invocation::Command(session, command) => run_command(session, command),
        Invocation::Daemon { session, ready } => {
            plx_ghostty::verify_build()?;
            // Before the daemon leaves the terminal: a mistake in the
            // configuration shows where `plx` was started.
            let config_home = std::env::var_os("XDG_CONFIG_HOME");
            let home = std::env::var_os("HOME");
            let path = config::path(config_home.as_deref(), home.as_deref())?;
            let loaded = config::load(&path)?;
            if loaded.created == Created::Yes {
                say(format_args!(
                    "plx: wrote the default configuration to {}\n",
                    path.display()
                ));
            }
            let shell_var = std::env::var_os("SHELL");
            let shell = config::shell(&loaded.config, shell_var.as_deref(), &path)?;
            let scrollback = config::scrollback(&loaded.config);
            let editor = config::editor_or_none(
                &loaded.config,
                std::env::var_os("VISUAL").as_deref(),
                std::env::var_os("EDITOR").as_deref(),
            );
            Ok(daemon::serve(session, shell, editor, scrollback, ready)?)
        }
        Invocation::Version => {
            let info = plx_ghostty::verify_build()?;
            show(format_args!(
                "plx {}\nlibghostty-vt {} ({:?}, kitty graphics: {:?}, tmux control mode: {:?})\n",
                env!("CARGO_PKG_VERSION"),
                info.version,
                info.optimize,
                info.kitty_graphics,
                info.tmux_control_mode,
            ))?;
            Ok(ExitCode::SUCCESS)
        }
        Invocation::Help => {
            show(format_args!("{}", cli::USAGE))?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// `plx`, `plx new`, `plx attach`.
fn attach(name: SessionName, reach: Reach) -> Result<ExitCode, MainError> {
    let paths = Paths::of(&name, &Env::current())?;
    let mut stream = match (start::connect_or_none(&paths)?, reach) {
        (Some(stream), Reach::Open | Reach::Attach) => stream,
        // Dropped before the hello: the daemon forgets it quietly.
        (Some(_), Reach::New) => return Err(MainError::Running(name)),
        (None, Reach::Attach) => return Err(MainError::NotRunning(name)),
        (None, Reach::Open | Reach::New) => match (start::start(&name, &paths)?, reach) {
            (Started::Serving(stream), _) => stream,
            (Started::Raced, Reach::New) => return Err(MainError::Running(name)),
            (Started::Raced, Reach::Open | Reach::Attach) => start::await_other(&paths)?,
        },
    };
    session::greet(&mut stream)?;
    // The client's own description of the terminal, so it can be made
    // non-blocking without touching the one the shell shares.
    let tty: OwnedFd = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc_flags(
            rustix::fs::OFlags::NOCTTY | rustix::fs::OFlags::CLOEXEC,
        ))
        .open("/dev/tty")
        .map_err(MainError::NoTerminal)?
        .into();
    terminal::install_panic_hook();
    let term = std::env::var_os("TERM");
    let colorterm = std::env::var_os("COLORTERM");
    let term_program = std::env::var_os("TERM_PROGRAM");
    let capabilities = capabilities::detect(&TerminalEnv {
        term: term.as_deref(),
        colorterm: colorterm.as_deref(),
        term_program: term_program.as_deref(),
    });
    // Keys reach the daemon before a busy program's slice runs out
    // (plx_daemon::sched). The daemon was started before this, so it asks
    // for its own.
    match plx_daemon::sched::shorten() {
        // Without it plx works as before, only with a longer tail under
        // load: nothing to tell the user in the terminal it draws.
        Ok(plx_daemon::sched::Slice::Short | plx_daemon::sched::Slice::Kept) | Err(_) => {}
    }
    let (outcome, notices) = session::attach(stream, &tty, capabilities)?;
    if notices.pastes_too_large > 0 {
        say(format_args!(
            "plx: {} paste(s) were too large to send\n",
            notices.pastes_too_large
        ));
    }
    match outcome {
        Outcome::Detached => {
            say(format_args!(
                "[detached from plx session {}]\n",
                name.as_str()
            ));
            Ok(ExitCode::SUCCESS)
        }
        Outcome::SessionEnded(_) => {
            say(format_args!("[plx session {} ended]\n", name.as_str()));
            Ok(ExitCode::SUCCESS)
        }
        // The signal said what happened; the shell reports it.
        Outcome::Signalled => Ok(ExitCode::FAILURE),
        Outcome::DaemonLost => Err(MainError::DaemonLost(paths.log)),
    }
}

/// `OpenOptionsExt::custom_flags` takes the C flags as an `i32`.
const fn libc_flags(flags: rustix::fs::OFlags) -> i32 {
    flags.bits().cast_signed()
}

/// `plx <command>`.
fn run_command(name: SessionName, command: SessionCommand) -> Result<ExitCode, MainError> {
    let paths = Paths::of(&name, &Env::current())?;
    let Some(mut stream) = start::connect_or_none(&paths)? else {
        return Err(MainError::NotRunning(name));
    };
    session::greet(&mut stream)?;
    match session::command(&mut stream, command)? {
        Reply::Applied | Reply::SessionEnded(_) => Ok(ExitCode::SUCCESS),
        Reply::Rejected(rejection) => Err(MainError::Rejected(rejection)),
    }
}

/// `plx events`: the session's events on stdout, as they happen.
fn events(name: SessionName) -> Result<ExitCode, MainError> {
    let paths = Paths::of(&name, &Env::current())?;
    let Some(mut stream) = start::connect_or_none(&paths)? else {
        return Err(MainError::NotRunning(name));
    };
    session::greet(&mut stream)?;
    // Both ends are normal: the session ended, or the reader left.
    match session::follow_events(stream, &mut io::stdout().lock())? {
        Followed::Closed | Followed::ReaderGone => Ok(ExitCode::SUCCESS),
    }
}

/// `plx ls`: the sessions a daemon serves, by name.
fn list() -> Result<ExitCode, MainError> {
    let dir = Paths::runtime_dir(&Env::current())?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // No session ever ran here.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(ExitCode::SUCCESS),
        Err(source) => return Err(MainError::List { dir, source }),
    };
    let mut names = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|source| MainError::List {
                dir: PathBuf::from(dir.as_os_str()),
                source,
            })?
            .path();
        // Sockets are `{session}.sock`; anything else there is not one.
        if path.extension().is_none_or(|extension| extension != "sock") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(std::ffi::OsStr::to_str) else {
            continue;
        };
        let name = match SessionName::new(stem) {
            Ok(name) => name,
            // Not a name plx gives a socket: not a session's.
            Err(_not_a_session) => continue,
        };
        let paths = Paths::of(&name, &Env::current())?;
        // A socket nobody accepts on is a dead daemon's.
        if start::connect_or_none(&paths)?.is_some() {
            names.push(name);
        }
    }
    names.sort();
    for name in &names {
        show(format_args!("{}\n", name.as_str()))?;
    }
    Ok(ExitCode::SUCCESS)
}

/// Writes to stdout: what the user asked to see.
fn show(text: fmt::Arguments<'_>) -> Result<(), MainError> {
    match io::stdout().lock().write_fmt(text) {
        Ok(()) => Ok(()),
        // A closed pipe (`plx ls | head -0`) is the reader's choice.
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(MainError::Client(ClientError::Io {
            what: "writing to stdout",
            source: error,
        })),
    }
}

/// Writes to stderr: what happened. A failure here has no one left to be
/// reported to; the exit status still says how it went.
fn say(text: fmt::Arguments<'_>) {
    if io::stderr().lock().write_fmt(text).is_err() {}
}
