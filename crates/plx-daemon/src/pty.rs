//! A pane's program on a pseudoterminal.
//!
//! The master is non-blocking and read in budgets by the reactor. The child
//! runs in its own session with the slave as its controlling terminal, and
//! is watched through a pidfd: its exit makes the pidfd readable and is
//! collected with `waitid`, so there is no SIGCHLD and no pid reuse race.

use std::ffi::OsString;
use std::io;
use std::num::NonZeroU8;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use plx_bug::OrBug as _;
use plx_core::event::ExitStatus;
use plx_emulator::GridSize;
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, Signal, WaitId, WaitIdOptions};
use rustix::pty::OpenptFlags;
use rustix::termios::Winsize;

/// What a pane runs.
#[derive(Debug, PartialEq, Eq)]
pub struct Program {
    pub path: OsString,
    pub args: Vec<OsString>,
}

/// Why a program could not be started.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("opening a pseudoterminal: {0}")]
    Pty(Errno),
    #[error("starting the program: {0}")]
    Start(io::Error),
    #[error("watching the program: {0}")]
    Pidfd(Errno),
}

/// What a read of the master found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    Data(usize),
    /// Nothing now; readiness comes later.
    Empty,
    /// The slave side closed: every process holding it is gone.
    Closed,
}

/// A running program and its terminal.
#[derive(Debug)]
pub struct Pty {
    master: OwnedFd,
    pid: Pid,
    pidfd: OwnedFd,
}

const fn winsize(size: GridSize) -> Winsize {
    Winsize {
        ws_row: size.rows().get(),
        ws_col: size.cols().get(),
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

impl Pty {
    /// Starts `program` on a new pseudoterminal of `size`.
    ///
    /// # Errors
    ///
    /// When the terminal cannot be made or the program cannot start.
    pub fn spawn(
        program: &Program,
        size: GridSize,
        env: &[(&str, &str)],
        cwd: Option<&Path>,
    ) -> Result<Self, SpawnError> {
        let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
        let master = rustix::pty::openpt(flags).map_err(SpawnError::Pty)?;
        rustix::pty::grantpt(&master).map_err(SpawnError::Pty)?;
        rustix::pty::unlockpt(&master).map_err(SpawnError::Pty)?;
        rustix::termios::tcsetwinsize(&master, winsize(size)).map_err(SpawnError::Pty)?;
        // The slave straight from the master: no path, no race.
        let slave = rustix::pty::ioctl_tiocgptpeer(&master, flags).map_err(SpawnError::Pty)?;
        let stdio = |fd: &OwnedFd| fd.try_clone().map(Stdio::from).map_err(SpawnError::Start);
        let mut command = Command::new(&program.path);
        command
            .args(&program.args)
            .envs(env.iter().copied())
            .stdin(stdio(&slave)?)
            .stdout(stdio(&slave)?)
            .stderr(stdio(&slave)?);
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        // SAFETY: the closure runs in the child between fork and exec and
        // only makes system calls (setsid, ioctl, sched_getattr and
        // sched_setattr), which are async-signal-safe; it allocates nothing
        // (an `io::Error` from an errno is a plain value) and touches no
        // locks.
        #[expect(
            unsafe_code,
            reason = "pre_exec: the child's session and controlling terminal"
        )]
        unsafe {
            command.pre_exec(|| {
                rustix::process::setsid()?;
                rustix::process::ioctl_tiocsctty(rustix::stdio::stdin())?;
                // The daemon's short slice is for its own latency, not the
                // program's (crate::sched).
                match crate::sched::reset() {
                    // Not worth failing the program over: it runs with the
                    // daemon's slice then.
                    Ok(()) | Err(_) => {}
                }
                Ok(())
            });
        }
        let child = command.spawn().map_err(SpawnError::Start)?;
        drop(slave);
        let pid = Pid::from_child(&child);
        // The child is reaped through the pidfd; `Child` has nothing left
        // to do (dropping it neither waits nor kills).
        drop(child);
        let pidfd =
            rustix::process::pidfd_open(pid, PidfdFlags::NONBLOCK).map_err(SpawnError::Pidfd)?;
        rustix::io::ioctl_fionbio(&master, true).map_err(SpawnError::Pty)?;
        Ok(Self { master, pid, pidfd })
    }

    #[must_use]
    pub const fn pid(&self) -> Pid {
        self.pid
    }

    /// The master's fd, to register with the reactor.
    #[must_use]
    pub fn master_fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    /// The directory the program in the foreground of the terminal works in
    /// (a command the shell runs, or the shell itself), else the program
    /// this terminal started with; `None` when neither can be read (gone,
    /// or not ours to read), which a program that just exited can be.
    #[must_use]
    pub fn working_directory_or_none(&self) -> Option<PathBuf> {
        // The foreground group's leader first, then the program; either may
        // be gone (or have no group, when the terminal is hung up), which is
        // what the `None` is for.
        [rustix::termios::tcgetpgrp(&self.master), Ok(self.pid)]
            .into_iter()
            .flatten()
            .flat_map(|pid| std::fs::read_link(format!("/proc/{}/cwd", pid.as_raw_nonzero())))
            .next()
    }

    /// The master, to wait on outside a reactor (benchmarks).
    #[must_use]
    pub fn master(&self) -> BorrowedFd<'_> {
        self.master.as_fd()
    }

    /// The pidfd, readable once the program exits.
    #[must_use]
    pub fn pidfd(&self) -> RawFd {
        self.pidfd.as_raw_fd()
    }

    /// Reads what the program wrote, up to `buf.len()` bytes.
    ///
    /// # Errors
    ///
    /// When the read fails other than by having nothing or by hang-up.
    pub fn read(&self, buf: &mut [u8]) -> Result<Read, Errno> {
        match rustix::io::read(&self.master, buf) {
            // Linux reports the slave's last close as EIO.
            Ok(0) | Err(Errno::IO) => Ok(Read::Closed),
            Ok(n) => Ok(Read::Data(n)),
            Err(Errno::AGAIN | Errno::INTR) => Ok(Read::Empty),
            Err(errno) => Err(errno),
        }
    }

    /// Writes as much of `bytes` as the terminal takes now.
    ///
    /// # Errors
    ///
    /// When the write fails other than by the terminal being full.
    pub fn write(&self, bytes: &[u8]) -> Result<usize, Errno> {
        match rustix::io::write(&self.master, bytes) {
            Ok(n) => Ok(n),
            Err(Errno::AGAIN | Errno::INTR) => Ok(0),
            Err(errno) => Err(errno),
        }
    }

    /// Resizes the terminal; the kernel signals the program (SIGWINCH).
    ///
    /// # Errors
    ///
    /// When the ioctl fails.
    pub fn resize(&self, size: GridSize) -> Result<(), Errno> {
        rustix::termios::tcsetwinsize(&self.master, winsize(size))
    }

    /// Hangs up the program's process group (it leads its own session).
    ///
    /// # Errors
    ///
    /// When the signal cannot be sent (it may be gone already: ESRCH).
    pub fn hang_up(&self) -> Result<(), Errno> {
        rustix::process::kill_process_group(self.pid, Signal::HUP)
    }

    /// Kills the program's process group, after a hang-up was ignored.
    ///
    /// # Errors
    ///
    /// As [`Pty::hang_up`].
    pub fn kill(&self) -> Result<(), Errno> {
        rustix::process::kill_process_group(self.pid, Signal::KILL)
    }

    /// The program's exit, once its pidfd is readable; `None` while it runs.
    ///
    /// # Errors
    ///
    /// When `waitid` fails.
    pub fn exit_status_or_none(&self) -> Result<Option<ExitStatus>, Errno> {
        let status = rustix::process::waitid(
            WaitId::PidFd(self.pidfd.as_fd()),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
        )?;
        Ok(status.map(
            |status| match (status.exit_status(), status.terminating_signal()) {
                (Some(code), _) => ExitStatus::Code(
                    u8::try_from(code & 0xFF).or_bug("an exit code is masked to a byte"),
                ),
                (None, Some(signal)) => ExitStatus::Signal(
                    u8::try_from(signal)
                        .map_or(None, NonZeroU8::new)
                        .or_bug("the kernel reports signals 1 to 64"),
                ),
                (None, None) => plx_bug::bug!("waitid with EXITED reports an exit or a signal"),
            },
        ))
    }
}
