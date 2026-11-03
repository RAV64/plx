//! Raw FFI bindings to libghostty-vt, generated from the Ghostty v1.4.0
//! headers.
//!
//! Do not use this crate directly; `plx-ghostty` is the only permitted
//! dependent (enforced by deny.toml). The bindings are committed, not
//! generated at build time; regenerate them after bumping the Ghostty pin:
//!
//! ```text
//! cargo run --manifest-path xtask/Cargo.toml -- ghostty-bindings
//! ```
//!
//! Kitty graphics functions are excluded from the bindings because plx
//! builds libghostty-vt without that feature. The glyph
//! protocol, also compiled out, has no functions; its one terminal option
//! stays, and plx-ghostty sets it off on every terminal. `plx_ghostty::Feature`
//! is the list of compiled-out features.

#![no_std]

mod bindings;

pub use bindings::*;

/// The version of the libghostty-vt this crate links: the pkg-config
/// `Version:` of the pinned flake build (e.g. `0.1.0-dev+83fb956-nix`). The
/// linked library reports the same string through `ghostty_build_info`.
pub const LINKED_VERSION: &str = env!("PLX_GHOSTTY_VT_VERSION");
