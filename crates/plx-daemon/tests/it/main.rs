//! The daemon shell against the real kernel: PTYs, processes, sockets.

#![cfg(test)]
#![expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "proptest clones and uses Arc internally"
)]

mod paths;
mod pty;
mod reactor;
mod sched;
mod subscribe;
