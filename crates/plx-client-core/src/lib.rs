//! The pure client core: frames from the daemon apply to a [`mirror::Mirror`]
//! of each visible pane, and the compositor turns its damage into terminal
//! output. No IO: the client shell feeds it bytes
//! and writes what it produces.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod client;
pub mod compose;
pub mod mirror;
