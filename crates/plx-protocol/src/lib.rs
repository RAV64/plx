//! The client/daemon wire protocol.
//!
//! - A connection opens with an 8-byte [`hello::HELLO`] each way: magic and
//!   protocol version, which must match exactly (client and daemon are the
//!   same binary).
//! - Then messages, each a frame: a `u32` little-endian length and a
//!   postcard payload ([`framing`]). The length is checked against a maximum
//!   before anything is buffered.
//! - Client to daemon: [`client::ClientWire`], decoded borrowed from the
//!   receive buffer; its values are parsed types (sizes bounded, keys valid,
//!   names checked), so this is the parse boundary for client input.
//! - Daemon to client: [`server::encode_outgoing`] writes a core
//!   `Outgoing` (frames straight from the borrowed `FrameView`, rows as
//!   8-byte cells) into the connection's buffer; [`server::Decoder`] applies
//!   frames to a client mirror through [`server::FrameApply`], in place.
//!
//! The encoding lives here alone, so swapping it touches nothing else.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod client;
pub mod framing;
pub mod hello;
pub mod server;

/// Why a message could not be encoded or decoded.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("malformed message: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("{0} bytes follow the message")]
    Trailing(usize),
    #[error("a frame of {len} bytes exceeds the {max}-byte maximum")]
    TooLarge { len: usize, max: usize },
    /// The message decodes but does not fit the receiver's state.
    #[error("invalid message: {}", .0.0)]
    Invalid(server::Refused),
}
