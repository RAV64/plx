//! The wire protocol: handshake, framing, and both directions' codecs.

#![cfg(test)]
#![expect(
    clippy::disallowed_methods,
    reason = "tests copy values to compare them; proptest macros clone internally"
)]

plx_alloc_count::install!();

mod client;
mod framing;
mod server;
