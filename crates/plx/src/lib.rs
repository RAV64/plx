//! The `plx` binary's parts: the command line, the CPU check, starting a
//! daemon, and `plx daemon` itself. `main` puts them together.

pub mod cli;
pub mod config;
pub mod cpu;
pub mod daemon;
pub mod start;
