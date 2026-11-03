//! The user's terminal: raw mode, the modes plx turns on, and their undoing
//! however the client ends.
//!
//! The client talks to its own open description of the terminal (the CLI
//! opens `/dev/tty`), so it can be non-blocking without touching the
//! description the user's shell shares. Termios is a property of the
//! terminal itself: it is saved once, restored on every exit path, and by
//! the panic hook.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::OnceLock;

use plx_bug::OrBug as _;
use plx_client_core::client::PROGRESS_REMOVED;
use plx_client_core::compose::ProgressBars;
use plx_emulator::GridSize;
use rustix::io::Errno;
use rustix::termios::{OptionalActions, Termios};

/// Turned on at attach.
///
/// The alternate screen, bracketed paste, the kitty keyboard protocol's
/// unambiguous keys (flag 1, pushed so it pops), and mouse reports (every
/// movement, so a program that asks for hovering gets it; SGR coordinates).
/// Shift-drag still selects in the terminal itself.
pub const SETUP: &[u8] = b"\x1b[?1049h\x1b[?2004h\x1b[>1u\x1b[?1003h\x1b[?1006h";

/// Turned off on every exit: synchronized output, the pen, a hidden cursor
/// and its shape (back to the user's own), mouse reports, kitty keys,
/// bracketed paste, and the alternate screen.
///
/// A terminal that draws progress bars also has its bar removed
/// ([`PROGRESS_REMOVED`]).
pub const RESTORE: &[u8] =
    b"\x1b[?2026l\x1b[0m\x1b[?25h\x1b[0 q\x1b[?1006l\x1b[?1003l\x1b[<u\x1b[?2004l\x1b[?1049l";

/// The terminal settings found at startup, and whether it draws progress
/// bars, for the panic hook.
static SAVED: OnceLock<(OwnedFd, Termios, ProgressBars)> = OnceLock::new();

/// Turns plx's modes off ([`RESTORE`]), and removes the progress bar on a
/// terminal that draws them.
fn write_restore(tty: impl AsFd, progress: ProgressBars) -> Result<(), Errno> {
    write_all(&tty, RESTORE)?;
    match progress {
        ProgressBars::Drawn => write_all(&tty, PROGRESS_REMOVED),
        ProgressBars::Unsupported => Ok(()),
    }
}

/// Raw mode. [`Raw::leave`] undoes it and reports how that went; dropping
/// it without leaving (unwinding) undoes it as well as it can.
#[derive(Debug)]
#[must_use = "raw mode is left with `leave`"]
pub struct Raw<'a> {
    tty: BorrowedFd<'a>,
    saved: Termios,
    progress: ProgressBars,
    state: State,
}

/// Whether raw mode was already left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Raw,
    Left,
}

/// The terminal's settings switched to raw, read fresh from the terminal
/// (settings are not copied in memory).
fn raw_settings(tty: BorrowedFd<'_>) -> Result<Termios, Errno> {
    let mut raw = rustix::termios::tcgetattr(tty)?;
    raw.make_raw();
    Ok(raw)
}

impl<'a> Raw<'a> {
    /// Saves the terminal's settings and switches it to raw mode;
    /// `progress` says whether leaving removes a progress bar.
    ///
    /// # Errors
    ///
    /// When `tty` is not a terminal or cannot be configured.
    pub fn enter(tty: BorrowedFd<'a>, progress: ProgressBars) -> Result<Self, Errno> {
        let saved = rustix::termios::tcgetattr(tty)?;
        if SAVED.get().is_none() {
            // The hook gets its own descriptor and copy: it may run after
            // `tty` is gone.
            let fd = rustix::io::fcntl_dupfd_cloexec(tty, 3)?;
            let for_hook = rustix::termios::tcgetattr(tty)?;
            if SAVED.set((fd, for_hook, progress)).is_err() {
                // Another attach in this process saved them first; those
                // are the settings to restore.
            }
        }
        rustix::termios::tcsetattr(tty, OptionalActions::Flush, &raw_settings(tty)?)?;
        Ok(Self {
            tty,
            saved,
            progress,
            state: State::Raw,
        })
    }

    /// Turns plx's modes off, staying raw (before a suspend).
    ///
    /// # Errors
    ///
    /// When the terminal cannot be written to.
    pub fn modes_off(&self) -> Result<(), Errno> {
        write_restore(self.tty, self.progress)
    }

    /// Leaves raw mode for a while (suspend); [`Raw::resume`] returns.
    ///
    /// # Errors
    ///
    /// When the settings cannot be restored.
    pub fn pause(&self) -> Result<(), Errno> {
        rustix::termios::tcsetattr(self.tty, OptionalActions::Now, &self.saved)
    }

    /// Back to raw mode after [`Raw::pause`].
    ///
    /// # Errors
    ///
    /// When the terminal cannot be configured.
    pub fn resume(&self) -> Result<(), Errno> {
        rustix::termios::tcsetattr(self.tty, OptionalActions::Flush, &raw_settings(self.tty)?)
    }

    /// Turns plx's modes off and restores the saved settings.
    ///
    /// # Errors
    ///
    /// When the terminal cannot be written to or configured.
    pub fn leave(mut self) -> Result<(), Errno> {
        self.state = State::Left;
        write_restore(self.tty, self.progress)?;
        rustix::termios::tcsetattr(self.tty, OptionalActions::Now, &self.saved)
    }
}

impl Drop for Raw<'_> {
    fn drop(&mut self) {
        if self.state == State::Raw {
            // Unwinding past an attach: undo what can be undone. There is
            // no one to report a failure to (the terminal is what would show
            // it), so this path alone does not report.
            let written = write_restore(self.tty, self.progress);
            let restored = rustix::termios::tcsetattr(self.tty, OptionalActions::Now, &self.saved);
            if written.is_err() || restored.is_err() {}
        }
    }
}

/// Writes all of `bytes`, waiting out a full terminal.
///
/// # Errors
///
/// When the write fails other than by the terminal being full.
pub fn write_all(tty: impl AsFd, bytes: &[u8]) -> Result<(), Errno> {
    let mut written = 0;
    while let Some(rest) = bytes.get(written..).filter(|rest| !rest.is_empty()) {
        match rustix::io::write(&tty, rest) {
            Ok(n) => written += n,
            Err(Errno::AGAIN | Errno::INTR) => std::thread::yield_now(),
            Err(errno) => return Err(errno),
        }
    }
    Ok(())
}

/// The terminal's size, bounded to what plx can show.
///
/// # Errors
///
/// When it cannot be read, or the terminal reports no size.
pub fn size(tty: impl AsFd) -> Result<GridSize, Errno> {
    let size = rustix::termios::tcgetwinsize(tty)?;
    // A terminal larger than plx's largest grid is drawn in its top-left
    // corner of that size.
    let cols = std::num::NonZeroU16::new(size.ws_col.min(GridSize::MAX_COLS));
    let rows = std::num::NonZeroU16::new(size.ws_row.min(GridSize::MAX_ROWS));
    let (Some(cols), Some(rows)) = (cols, rows) else {
        return Err(Errno::INVAL);
    };
    Ok(GridSize::new(cols, rows).or_bug("a size within the maxima is a grid size"))
}

/// Installs the client's panic hook: the terminal is
/// restored first, so the report shows on the user's own screen, then the
/// default report, then an abort.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let restored = restore_after_panic();
        default(info);
        if let Err(errno) = restored {
            let report = format!("plx: restoring the terminal failed too: {errno}\n");
            // A failed write ends it: in a panic hook there is nowhere left
            // to report that to.
            if std::io::Write::write_all(&mut std::io::stderr(), report.as_bytes()).is_err() {}
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "the client's panic hook: the terminal is restored, the report printed"
        )]
        std::process::abort();
    }));
}

/// For the panic hook: restores the saved settings and the modes, on the
/// descriptor saved for it. Raw writes only.
///
/// # Errors
///
/// When restoring failed; the hook prints that along with the panic.
pub fn restore_after_panic() -> Result<(), Errno> {
    match SAVED.get() {
        Some((tty, saved, progress)) => {
            let written = write_restore(tty, *progress);
            rustix::termios::tcsetattr(tty, OptionalActions::Now, saved)?;
            written
        }
        // Raw mode was never entered: nothing to restore.
        None => Ok(()),
    }
}
