//! The core's driven port: everything it asks the outside world to do.
//!
//! The shell (daemon), the simulation and tests implement [`Effects`]; the
//! core calls it during [`crate::session::Session::step`], lending borrowed
//! data (bytes, frame views, events) that the sink encodes or copies as it
//! needs. Static dispatch: the session is generic over its sink.

use plx_emulator::{Emulator, GridSize};

use crate::event::{Event, EventSeq};
use crate::id::{ClientId, PaneId};
use crate::message::Outgoing;

pub trait Effects {
    /// Start `program` for the pane on a new PTY of `size`; report the
    /// outcome with `PtySpawned` or `PtySpawnFailed` right after the
    /// current step, before any other input about the pane, and its end
    /// with `PtyExited`.
    fn spawn_pty(&mut self, pane: PaneId, size: GridSize, program: PaneProgram<'_>);

    /// Write to the pane's PTY (keys, pastes, terminal replies).
    fn write_pty(&mut self, pane: PaneId, bytes: &[u8]);

    fn resize_pty(&mut self, pane: PaneId, size: GridSize);

    /// Hang up the pane's process group; report its end with `PtyExited`.
    fn close_pty(&mut self, pane: PaneId);

    fn send<E: Emulator>(&mut self, client: ClientId, message: Outgoing<'_, E>);

    /// Close the client's connection once what was sent to it is flushed.
    /// From this call on, the shell passes no more input from it (whatever
    /// is still queued is dropped) and never reports it as `ClientGone`:
    /// the session has already forgotten it.
    fn close_client(&mut self, client: ClientId);

    /// Publish an event on the session's stream.
    fn event(&mut self, seq: EventSeq, event: Event<'_>);

    /// The session is over: stop the daemon.
    fn exit(&mut self);
}

/// What a new pane runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneProgram<'a> {
    /// The session's shell, started in `start_in`.
    Shell { start_in: StartIn },
    /// The user's editor, on a file holding `History` (editing the
    /// scrollback); the shell removes the file once the editor is gone.
    Editor(History<'a>),
}

/// Where a new pane's shell starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartIn {
    /// Where the session's programs start.
    Default,
    /// Where the program in `PaneId`'s foreground works: a split starts
    /// where the pane it splits is. If that cannot be read (it just
    /// exited), the default.
    PaneOf(PaneId),
}

/// A pane's history as text (UTF-8), lent to the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct History<'a>(&'a [u8]);

impl<'a> History<'a> {
    #[must_use]
    pub const fn new(text: &'a [u8]) -> Self {
        Self(text)
    }

    #[must_use]
    pub const fn bytes(&self) -> &'a [u8] {
        self.0
    }
}
