//! Tests for the emulator port's value types.
//!
//! One test binary for the crate keeps links to one per change.

#![cfg(test)]
#![expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "test models keep their own copies; proptest uses Arc and clone internally"
)]

mod cell;
mod grid_size;
mod keys;
mod progress;
mod screen;
