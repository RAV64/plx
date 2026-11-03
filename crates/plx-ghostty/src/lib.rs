//! The libghostty-vt adapter.
//!
//! [`GhosttyEmulator`] implements [`plx_emulator::Emulator`] on top of
//! libghostty-vt from Ghostty v1.4.0. Nothing from the C API escapes this
//! crate: callers see only plx value types and [`Error`].
//!
//! A `GhosttyEmulator` is neither `Send` nor `Sync`. The C API allows moving
//! a terminal between threads with exclusive access, but plx gives each
//! emulator a single owner on the daemon's one thread, so there is no reason
//! to promise more.

#![expect(
    unsafe_code,
    reason = "FFI adapter over libghostty-vt; every unsafe block states its invariant"
)]

mod alloc;
mod build;
mod error;
mod input;
mod search;
mod selection;
mod sync;
mod terminal;

pub use build::{BuildInfo, Feature, Optimize, Presence, build_info, verify_build};
pub use error::{BuildError, Code, Error};
pub use terminal::{
    GhosttyEmulator, GhosttyFactory, GraphemeClustering, MemoryUsage, Options, Scrollback,
};
