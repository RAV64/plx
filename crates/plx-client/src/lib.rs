//! The client shell: raw mode on the user's terminal, input parsed into key
//! events and pastes, the client core's output written out, and the
//! terminal restored however the client ends.

pub mod capabilities;
pub mod keys;
pub mod session;
pub mod terminal;
