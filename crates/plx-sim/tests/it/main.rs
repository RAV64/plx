//! The L3 gate: whole-system schedules on the real cores, checked from
//! outside.

#![cfg(test)]
#![expect(clippy::disallowed_methods, reason = "proptest clones internally")]

mod vocabulary;
mod world;
