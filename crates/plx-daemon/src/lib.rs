//! The daemon shell: the single-threaded mio reactor that turns sockets,
//! PTYs, pidfds and signals into core inputs and executes the core's
//! effects. Its loop follows the simulation's reference reactor
//! (`plx-sim`): client input first, budgeted PTY reads, exits, then a due
//! tick. `unsafe` stays denied; process spawning (`pre_exec`) and the
//! scheduler slice's two system calls (`sched`) opt in locally.

pub mod log;
pub mod paths;
pub mod pty;
pub mod reactor;
pub mod sched;
pub mod subscribe;
