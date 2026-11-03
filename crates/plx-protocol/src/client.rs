//! Client to daemon: what a client says, as it travels.

use alloc::vec::Vec;

use plx_core::command::Command;
use plx_core::message::{ClientMessage, FrameSeq, Paste};
use plx_emulator::{CellPx, GridSize, KeyEvent, MouseEvent};
use serde::{Deserialize, Serialize};

use crate::CodecError;
use crate::framing::{self, MAX_CLIENT_MESSAGE};

/// A client message on the wire. Decoding it parses every value (bounded
/// sizes, valid keys, checked names), and a paste borrows the receive
/// buffer.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientWire<'a> {
    Attach {
        size: GridSize,
        cell_px: Option<CellPx>,
    },
    Resize {
        size: GridSize,
        cell_px: Option<CellPx>,
    },
    Key(KeyEvent),
    #[serde(borrow)]
    Paste(Paste<'a>),
    Command(Command),
    Ack(FrameSeq),
    /// Asks for the session's events instead of the session: the daemon
    /// forgets the connection as a client and writes every event to it as a
    /// line of JSON (`plx events`).
    Subscribe,
    Mouse(MouseEvent),
}

/// Whom a client message is for.
#[derive(Debug, PartialEq, Eq)]
pub enum Request<'a> {
    /// The session (the core).
    Session(ClientMessage<'a>),
    /// The daemon: the connection becomes an event subscriber.
    Subscribe,
}

impl ClientWire<'_> {
    /// The request, a session message borrowing this one.
    #[must_use]
    pub const fn request(&self) -> Request<'_> {
        Request::Session(match self {
            Self::Attach { size, cell_px } => ClientMessage::Attach {
                size: *size,
                cell_px: *cell_px,
            },
            Self::Resize { size, cell_px } => ClientMessage::Resize {
                size: *size,
                cell_px: *cell_px,
            },
            Self::Key(key) => ClientMessage::Key(*key),
            Self::Paste(paste) => ClientMessage::Paste(*paste),
            Self::Command(command) => ClientMessage::Command(command),
            Self::Ack(seq) => ClientMessage::Ack(*seq),
            Self::Mouse(mouse) => ClientMessage::Mouse(*mouse),
            Self::Subscribe => return Request::Subscribe,
        })
    }
}

/// Appends `message` to `out` as one frame (the client's send buffer).
///
/// # Errors
///
/// When it exceeds [`MAX_CLIENT_MESSAGE`] (a paste too large to send).
pub fn encode(message: &ClientWire<'_>, out: &mut Vec<u8>) -> Result<(), CodecError> {
    framing::write(message, MAX_CLIENT_MESSAGE, out)
}

/// Decodes one frame's payload (from [`framing::split`]).
///
/// # Errors
///
/// When it is malformed, carries an invalid value, or has bytes after the
/// message.
pub fn decode(payload: &[u8]) -> Result<ClientWire<'_>, CodecError> {
    let (message, rest) = postcard::take_from_bytes(payload)?;
    if rest.is_empty() {
        Ok(message)
    } else {
        Err(CodecError::Trailing(rest.len()))
    }
}
