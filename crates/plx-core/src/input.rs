//! The core's driving port: everything that can happen to a session, parsed
//! and lent to [`crate::session::Session::step`].

use crate::event::{ExitStatus, Pid, SpawnError};
use crate::id::{ClientId, PaneId};
use crate::message::ClientMessage;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input<'a> {
    /// A message from a connected client.
    Client {
        client: ClientId,
        message: ClientMessage<'a>,
    },
    /// The client's connection closed.
    ClientGone {
        client: ClientId,
    },
    PtySpawned {
        pane: PaneId,
        pid: Pid,
    },
    PtySpawnFailed {
        pane: PaneId,
        error: SpawnError,
    },
    PtyOutput {
        pane: PaneId,
        bytes: &'a [u8],
    },
    PtyExited {
        pane: PaneId,
        status: ExitStatus,
    },
    /// Time passed: a deadline from [`crate::session::Session::poll_timeout`]
    /// is due.
    Tick,
}
