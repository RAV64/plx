//! Property tests for the pure core. One test binary for the crate.

// Marks everything here as test code, so clippy's allow-*-in-tests settings
// (clippy.toml) also cover the helpers outside #[test] functions.
#![cfg(test)]
// Tests compare states by value and proptest's macros copy and share values
// internally; production code gets no such allowance.
#![expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "tests copy values to compare them; proptest macros use Arc and clone internally"
)]

plx_alloc_count::install!();

mod alloc;
mod layout;
mod mode;
mod session;
mod size;
mod strategy;
mod structure;
