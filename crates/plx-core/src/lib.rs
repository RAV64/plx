//! The plx session core.
//!
//! One pure, deterministic state machine per session:
//! `Core::step(now, input, effects)`. It does no IO, reads no clock, uses no
//! threads and no randomness; the shells feed it inputs and execute the
//! effects it emits into their sink. A bug panics.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod command;
pub mod effects;
pub mod event;
pub mod geometry;
pub mod id;
pub mod input;
pub mod layout;
pub mod message;
pub mod mode;
pub mod rejection;
pub mod search;
pub mod session;
pub mod size;
pub mod structure;
pub mod tab;
pub mod time;
