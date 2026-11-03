//! The terminal-emulator port.
//!
//! The core drives one [`Emulator`] per pane. This crate holds only the trait
//! and the plx-owned value types that cross it, so no adapter type
//! (libghostty or otherwise) ever leaks into the core. It changes rarely by
//! design: the libghostty adapter depends on this crate alone, so editing the
//! core never rebuilds it.
//!
//! The screen model is compact: 8-byte `Copy` cells in
//! one flat [`Grid`] per pane, styles and grapheme clusters in per-pane
//! [`Tables`] bounded by use, owned together as a [`Screen`].

#![no_std]
// Audited sites, all in `cell.rs`, each with a SAFETY proof and a property
// test against the safe definition: `Cell::slice_as_wire` lends cells as
// their wire bytes; `Cell::copy_from_wire` checks wire forms and copies them
// into cells; `Cell::plain_run` counts plain cells. The checks run in AVX2
// (an `unsafe` call only because AVX2 functions are `target_feature`
// functions). Everything else stays safe.
#![deny(unsafe_code)]

// plx requires x86-64-v3; the workspace's
// .cargo/config.toml builds for it. Fail here rather than run the portable
// fallbacks unmeasured.
#[cfg(all(target_arch = "x86_64", not(target_feature = "avx2")))]
compile_error!("plx requires x86-64-v3 (AVX2): build with the workspace's .cargo/config.toml");

extern crate alloc;

mod cell;
mod events;
#[cfg(feature = "testkit")]
mod fake;
mod grid;
mod key;
mod mouse;
mod port;
mod progress;
mod screen;
mod search;
mod style;
mod tables;

pub use cell::{
    Cell, Glyph, GlyphKind, GraphemeId, GraphemeIdTooLarge, InvalidCell, InvalidGlyph, StyleId,
    Width,
};
pub use events::{Bell, Clipboard, EmulatorEvents, MAX_CLIPBOARD_BYTES};
#[cfg(feature = "testkit")]
pub use fake::{FakeEmulator, FakeFactory};
pub use grid::{
    CellText, Cursor, CursorBlink, CursorShape, CursorVisibility, Grid, GridSize, GridTooLarge,
    Position, ResolvedCell, Version,
};
pub use key::{
    FKey, FunctionKeyCode, Key, KeyAction, KeyChar, KeyChord, KeyEvent, Mods, NoSuchFunctionKey,
    UnknownMods,
};
pub use mouse::{
    AUTOSCROLL_INTERVAL, ActiveScreen, Autoscroll, MouseAction, MouseButton, MouseEvent,
    MouseTracking, Past, SelectAt, SelectGesture, Selection, Wheel,
};
pub use port::{
    CellPx, Change, Compression, Damage, Emulator, EmulatorFactory, Scroll, ScrollPosition,
};
pub use progress::{Percent, PercentTooLarge, Progress};
pub use screen::{RowWriter, Screen, Text};
pub use search::{Case, Count, SearchDirection, SearchOptions, SearchStatus, Words};
pub use style::{Attrs, Color, Rgb, Style, Underline, UnknownAttrs};
pub use tables::Tables;
