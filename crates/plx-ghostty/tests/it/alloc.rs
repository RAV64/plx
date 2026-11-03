//! Zero-allocation and memory guarantees of the adapter.
//! libghostty-vt allocates through Rust's global allocator, so these counts
//! include its memory too.

use core::num::NonZeroUsize;

use plx_alloc_count::measure;
use plx_emulator::{Emulator, FKey, Key, KeyChar, KeyEvent, Mods, Version};
use plx_ghostty::{GhosttyEmulator, GraphemeClustering, Options, Scrollback};

/// A full-screen program repainting in place (like a TUI redrawing its
/// frame): once warm, feeding it and syncing the damage into the grid
/// allocates nothing, in Rust or in libghostty-vt.
#[test]
fn redraw_feed_and_sync_allocate_nothing() {
    let mut emulator = crate::emulator(crate::size(80, 24));
    let frames: [&[u8]; 2] = [
        b"\x1b[H\x1b[1;32mstatus: ok\x1b[0m   \x1b[5;10Hcounter 0001\x1b[24;1Hready",
        b"\x1b[H\x1b[1;31mstatus: busy\x1b[0m \x1b[5;10Hcounter 0002\x1b[24;1Hworking",
    ];
    // Warm-up: both styles interned, every buffer at its working size.
    for (version, frame) in (1_u64..)
        .map(Version::new)
        .zip(frames.iter().cycle().take(4))
    {
        emulator.feed(frame);
        emulator.sync(version).unwrap();
    }
    let (counts, rows_changed) = measure(|| {
        let mut rows_changed = 0_u32;
        for (version, frame) in (100_u64..)
            .map(Version::new)
            .zip(frames.iter().cycle().take(2_000))
        {
            assert!(emulator.feed(frame).is_empty());
            rows_changed += u32::from(emulator.sync(version).unwrap().rows);
        }
        rows_changed
    });
    assert!(rows_changed > 0, "the frames differ, so rows must change");
    assert_eq!(counts.allocations, 0, "{counts:?}");
}

/// Scrolling output with scrollback disabled reuses the screen's memory.
#[test]
fn scrolling_without_scrollback_allocates_nothing() {
    let options = Options {
        scrollback: Scrollback::Disabled,
        continuation_max_bytes: NonZeroUsize::new(4096).unwrap(),
        grapheme_clustering: GraphemeClustering::Enabled,
    };
    let mut emulator = GhosttyEmulator::new(crate::size(80, 24), &options).unwrap();
    let chunk = b"cargo build\r\n   Compiling plx v0.1.0\r\n\x1b[32mok\x1b[0m\r\n";
    for version in (1..=200_u64).map(Version::new) {
        emulator.feed(chunk);
        emulator.sync(version).unwrap();
    }
    let (counts, ()) = measure(|| {
        for version in (1_000..2_000_u64).map(Version::new) {
            emulator.feed(chunk);
            emulator.sync(version).unwrap();
        }
    });
    assert_eq!(counts.allocations, 0, "{counts:?}");
}

/// Encoding keystrokes and pastes into a reused buffer allocates nothing
/// once the buffers have grown.
#[test]
fn encoding_input_allocates_nothing() {
    let mut emulator = crate::emulator(crate::size(80, 24));
    emulator.feed(b"\x1b[?2004h"); // bracketed paste on
    let keys = [
        KeyEvent::press(Key::Char(KeyChar::new('a').unwrap()), Mods::empty()),
        KeyEvent::press(Key::Char(KeyChar::new('c').unwrap()), Mods::CTRL),
        KeyEvent::press(Key::Up, Mods::empty()),
        KeyEvent::press(Key::Char(KeyChar::new('x').unwrap()), Mods::ALT),
        KeyEvent::press(Key::F(FKey::new(5).unwrap()), Mods::SHIFT),
    ];
    let mut out = Vec::with_capacity(256);
    let encode_all = |emulator: &mut GhosttyEmulator, out: &mut Vec<u8>| {
        out.clear();
        for key in &keys {
            emulator.encode_key(key, out);
        }
        emulator.encode_paste("echo pasted\nline two", out);
    };
    encode_all(&mut emulator, &mut out); // warm-up: grows the paste scratch
    let (counts, ()) = measure(|| {
        for _ in 0..1_000 {
            encode_all(&mut emulator, &mut out);
        }
    });
    assert!(out.starts_with(b"a\x03"), "unexpected encoding {out:?}");
    assert_eq!(counts.allocations, 0, "{counts:?}");
}

/// The whole cost of an idle 80×24 pane: libghostty-vt's terminal, render
/// state and key encoder, plus plx's grid and tables.
#[test]
fn an_idle_pane_stays_within_its_memory_budget() {
    // Measured at 188 KiB in 48 allocations; the budget leaves room
    // for noise, not for growth.
    const BUDGET_BYTES: i128 = 256 * 1024;
    let (counts, mut emulator) = measure(|| {
        let mut emulator = crate::emulator(crate::size(80, 24));
        emulator.sync(Version::new(1)).unwrap();
        emulator
    });
    assert!(
        counts.net_bytes() <= BUDGET_BYTES,
        "an idle 80x24 pane holds {} bytes, over the {BUDGET_BYTES}-byte budget",
        counts.net_bytes()
    );
    // Proof that libghostty-vt really allocates through Rust's allocator: its
    // terminal pages alone are far larger than plx's 15 KiB grid.
    assert!(
        counts.net_bytes() >= 64 * 1024,
        "only {} bytes counted: libghostty-vt is not using the Rust allocator",
        counts.net_bytes()
    );
    // The grid itself is 8 bytes per cell plus a version per row.
    assert_eq!(
        size_of_val(emulator.screen().grid().row_or_none(0).unwrap()),
        80 * 8
    );
    assert!(emulator.feed(b"").is_empty());
}
