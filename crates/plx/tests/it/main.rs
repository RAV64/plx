//! The `plx` binary: its command line as properties, and the real binary
//! end to end in a pseudoterminal.

#![cfg(test)]
#![expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "proptest clones and uses Arc internally"
)]

mod cli;
mod config;
mod smoke;
