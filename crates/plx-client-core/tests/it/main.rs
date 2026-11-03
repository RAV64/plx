//! The client core against real daemon frames (a session on the fake
//! emulator, encoded and decoded by plx-protocol).

#![cfg(test)]
#![expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "tests copy values to compare them; proptest macros use Arc and clone internally"
)]

plx_alloc_count::install!();

mod client;
mod compose;
mod mirror;
