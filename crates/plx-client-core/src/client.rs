//! The attached client: daemon bytes in, terminal bytes and acks out.
//!
//! [`ClientCore`] is the client's whole logic between its two IO ends. The
//! shell hands it what the socket and the terminal produced; it decodes
//! frames into the mirror, draws them, acknowledges them, and encodes the
//! user's keys and pastes. Output goes straight into the shell's send
//! buffers, lent through [`ClientEffects`].
//!
//! Only the daemon can end the connection, so only [`ClientCore::receive`]
//! consumes the core: a core that was detached, saw its session end or
//! received a broken frame cannot be used again.

use alloc::vec::Vec;

use plx_bug::OrBug as _;
use plx_core::command::{Command, SessionCommand};
use plx_core::event::EndReason;
use plx_core::message::Paste;
use plx_core::rejection::Rejection;
use plx_emulator::{
    CellPx, Change, GridSize, KeyEvent, MouseAction, MouseButton, MouseEvent, Progress,
};
use plx_protocol::CodecError;
use plx_protocol::client::{ClientWire, encode};
use plx_protocol::framing::{MAX_SERVER_MESSAGE, split};
use plx_protocol::server::{Decoder, Refused, ServerMessage};

use crate::compose::{Capabilities, Compositor, ProgressBars};
use crate::mirror::Mirror;

/// Where a client's output goes: the shell's buffers, written out when the
/// socket and the terminal can take them.
pub trait ClientEffects {
    /// The daemon socket's send buffer; the core appends whole messages.
    fn daemon(&mut self) -> &mut Vec<u8>;
    /// The terminal's write buffer; the core appends whole frames.
    fn terminal(&mut self) -> &mut Vec<u8>;
    /// The daemon refused a request; the shell may tell the user.
    fn rejected(&mut self, rejection: Rejection);
}

/// Why the daemon ended the connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// This client was detached.
    Detached,
    SessionEnded(EndReason),
}

/// What became of the client after the daemon's bytes.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "returned and matched at once; boxing the core would allocate on every receive"
)]
pub enum Received {
    /// Still attached.
    Attached(ClientCore),
    Ended(End),
}

/// The daemon broke the protocol; the connection is over.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("the daemon sent an undecodable or unfitting message: {0}")]
    Codec(#[from] CodecError),
    #[error("the daemon sent {0} bytes after ending the connection")]
    AfterEnd(usize),
}

/// Appends the sequence that puts `text` (UTF-8) on the terminal's
/// clipboard: OSC 52 (`c`, the clipboard), base64.
pub fn osc52(text: &[u8], out: &mut Vec<u8>) {
    let digit = |n: u32| {
        let n = u8::try_from(n & 63).or_bug("six bits fit u8");
        match n {
            0..=25 => b'A' + n,
            26..=51 => b'a' + (n - 26),
            52..=61 => b'0' + (n - 52),
            62 => b'+',
            _ => b'/',
        }
    };
    out.extend_from_slice(b"\x1b]52;c;");
    for chunk in text.chunks(3) {
        let [a, b, c] = match *chunk {
            [a] => [a, 0, 0],
            [a, b] => [a, b, 0],
            [a, b, c, ..] => [a, b, c],
            [] => break,
        };
        let n = u32::from(a) << 16 | u32::from(b) << 8 | u32::from(c);
        out.push(digit(n >> 18));
        out.push(digit(n >> 12));
        out.push(if chunk.len() > 1 { digit(n >> 6) } else { b'=' });
        out.push(if chunk.len() > 2 { digit(n) } else { b'=' });
    }
    out.extend_from_slice(b"\x1b\\");
}

/// Removes the terminal's progress bar: what [`osc_progress`] writes for
/// [`Progress::Removed`], as a constant for the shell's exits (a panic hook
/// does not allocate).
pub const PROGRESS_REMOVED: &[u8] = b"\x1b]9;4;0\x1b\\";

/// Appends the sequence that shows `progress` in the terminal's progress
/// bar: `OSC 9;4;state[;percent]`, rebuilt from the typed report (never the
/// program's bytes).
pub fn osc_progress(progress: Progress, out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b]9;4;");
    decimal(progress.state(), out);
    if let Some(percent) = progress.percent_or_none() {
        out.push(b';');
        decimal(percent.get(), out);
    }
    out.extend_from_slice(b"\x1b\\");
}

/// Appends `n` in decimal.
fn decimal(n: u8, out: &mut Vec<u8>) {
    let (hundreds, tens, ones) = (n / 100, n / 10 % 10, n % 10);
    if hundreds > 0 {
        out.push(b'0' + hundreds);
    }
    if n >= 10 {
        out.push(b'0' + tens);
    }
    out.push(b'0' + ones);
}

/// A paste larger than one client message can carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a paste of {len} bytes exceeds what one message carries")]
pub struct PasteTooLarge {
    pub len: usize,
}

/// An attached client.
#[derive(Debug)]
pub struct ClientCore {
    mirror: Mirror,
    decoder: Decoder,
    compositor: Compositor,
    terminal: GridSize,
    progress_bars: ProgressBars,
    /// The progress the terminal was last told to show.
    progress: Progress,
}

impl ClientCore {
    /// Attaches with the terminal's size (after the shell's handshake).
    pub fn attach(
        terminal: GridSize,
        cell_px: Option<CellPx>,
        capabilities: Capabilities,
        fx: &mut impl ClientEffects,
    ) -> Self {
        send(
            fx,
            &ClientWire::Attach {
                size: terminal,
                cell_px,
            },
        );
        Self {
            mirror: Mirror::new(),
            decoder: Decoder::new(),
            compositor: Compositor::new(capabilities),
            terminal,
            progress_bars: capabilities.progress,
            progress: Progress::Removed,
        }
    }

    /// Takes the shell's receive buffer, holding everything read from the
    /// daemon so far: applies every whole message in place, removes those
    /// bytes from the buffer (a partial message stays for the next read),
    /// draws what changed, and acknowledges the last frame once drawn.
    ///
    /// # Errors
    ///
    /// When the daemon breaks the protocol: the connection is over.
    pub fn receive(
        mut self,
        inbox: &mut Vec<u8>,
        fx: &mut impl ClientEffects,
    ) -> Result<Received, DaemonError> {
        let mut used = 0;
        let mut framed = None;
        let mut end = None;
        while let Some((payload, length)) = split(
            inbox.get(used..).or_bug("used stays within the inbox"),
            MAX_SERVER_MESSAGE,
        )? {
            if end.is_some() {
                return Err(DaemonError::AfterEnd(inbox.len() - used));
            }
            match self.decoder.decode(payload, &mut self.mirror)? {
                ServerMessage::Frame(seq) => framed = Some(seq),
                ServerMessage::Rejected(rejection) => fx.rejected(rejection),
                ServerMessage::Detached => end = Some(End::Detached),
                ServerMessage::SessionEnded(reason) => end = Some(End::SessionEnded(reason)),
                ServerMessage::Clipboard => {
                    if let Some(text) = self.mirror.copied_or_none() {
                        osc52(text, fx.terminal());
                    }
                }
                // Only an unattached client's command is acknowledged so.
                ServerMessage::Applied => {
                    return Err(DaemonError::Codec(CodecError::Invalid(Refused(
                        "an acknowledgment of a command an attached client never sends",
                    ))));
                }
            }
            used += length;
        }
        if let Some(end) = end {
            if used < inbox.len() {
                return Err(DaemonError::AfterEnd(inbox.len() - used));
            }
            return Ok(Received::Ended(end));
        }
        inbox.drain(..used);
        if let Some(seq) = framed {
            self.draw(fx);
            if let Some(progress) = self.mirror.reported_or_none() {
                self.progress = progress;
                self.show_progress(fx);
            }
            // Acknowledged once drawn: the daemon sends the next frame only
            // then, so a slow terminal slows frames instead of queueing them.
            send(fx, &ClientWire::Ack(seq));
        }
        Ok(Received::Attached(self))
    }

    /// The user pressed a key; the daemon resolves it against the mode.
    pub fn key(&mut self, key: KeyEvent, fx: &mut impl ClientEffects) {
        send(fx, &ClientWire::Key(key));
    }

    /// The user used the mouse; the session decides what it means.
    /// A click on a tab's label in the status line picks the tab; anything
    /// else goes to the daemon.
    pub fn mouse(&mut self, mouse: MouseEvent, fx: &mut impl ClientEffects) {
        if mouse.action == MouseAction::Press
            && mouse.button == MouseButton::Left
            && let Some(tab) = self.compositor.tab_at_or_none(mouse.position)
        {
            let pick = Command::Session(SessionCommand::FocusTab { tab });
            send(fx, &ClientWire::Command(pick));
            return;
        }
        send(fx, &ClientWire::Mouse(mouse));
    }

    /// The user pasted `text`.
    ///
    /// # Errors
    ///
    /// When it is too large for one message; nothing is sent.
    pub fn paste(&mut self, text: &str, fx: &mut impl ClientEffects) -> Result<(), PasteTooLarge> {
        let out = fx.daemon();
        let start = out.len();
        match encode(&ClientWire::Paste(Paste::new(text)), out) {
            Ok(()) => Ok(()),
            Err(CodecError::TooLarge { .. }) => {
                out.truncate(start);
                Err(PasteTooLarge { len: text.len() })
            }
            Err(CodecError::Postcard(_) | CodecError::Trailing(_) | CodecError::Invalid(_)) => {
                plx_bug::bug!("encoding a paste fails only by size")
            }
        }
    }

    /// The terminal resized: redraws for the new size at once (clipped or
    /// blank-bordered) and tells the daemon, whose next frame fits it.
    pub fn resized(
        &mut self,
        terminal: GridSize,
        cell_px: Option<CellPx>,
        fx: &mut impl ClientEffects,
    ) {
        self.terminal = terminal;
        self.draw(fx);
        send(
            fx,
            &ClientWire::Resize {
                size: terminal,
                cell_px,
            },
        );
    }

    /// The terminal may have been changed behind the client's back (it was
    /// suspended and resumed): redraws everything.
    pub fn resumed(&mut self, fx: &mut impl ClientEffects) {
        self.compositor.invalidate();
        self.draw(fx);
        // The shell removed the progress bar when it stopped.
        if self.progress != Progress::Removed {
            self.show_progress(fx);
        }
    }

    /// The mirror, for the shell's own use (a scrollback view, tests).
    #[must_use]
    pub const fn mirror(&self) -> &Mirror {
        &self.mirror
    }

    /// Tells the terminal the progress to show, if it draws progress bars.
    fn show_progress(&self, fx: &mut impl ClientEffects) {
        match self.progress_bars {
            ProgressBars::Drawn => osc_progress(self.progress, fx.terminal()),
            ProgressBars::Unsupported => {}
        }
    }

    fn draw(&mut self, fx: &mut impl ClientEffects) {
        // Nothing drawn (nothing changed) needs nothing done.
        match self
            .compositor
            .render(&mut self.mirror, self.terminal, fx.terminal())
        {
            Change::Changed | Change::Unchanged => {}
        }
    }
}

/// Appends a message that always fits (everything but a paste).
#[track_caller]
fn send(fx: &mut impl ClientEffects, message: &ClientWire<'_>) {
    encode(message, fx.daemon()).or_bug("a message without a paste fits the limit");
}
