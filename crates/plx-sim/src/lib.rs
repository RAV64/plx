//! The deterministic whole-system simulation harness: the real session
//! and client cores, the real codec over in-memory pipes, fake programs,
//! virtual time, and headless libghostty "outer terminals" as the oracle.
//!
//! - [`world::World`] runs a schedule of [`world::Choice`]s.
//! - [`daemon::Daemon`] is the reference reactor plx-daemon's follows.
//! - [`client::Client`] checks everything a client draws as it draws it.
//! - [`oracle`] holds the checks.

#![forbid(unsafe_code)]

pub mod client;
pub mod daemon;
pub mod oracle;
pub mod program;
pub mod trace;
pub mod world;
