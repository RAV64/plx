//! The client shell's pure parts, against termina's real parser.

#![cfg(test)]
#![expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "proptest clones and uses Arc internally"
)]

mod attach;
mod capabilities;
mod keys;
