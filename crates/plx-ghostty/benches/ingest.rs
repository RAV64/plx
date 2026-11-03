//! Wall-clock benchmarks of the libghostty adapter's hot path: feeding
//! program output and syncing the dirty rows into the pane's screen.
//! Run with `cargo bench -p plx-ghostty`.

#![expect(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::print_stderr,
    reason = "a benchmark sets up fixed, valid inputs and reports on stderr; a failure aborts the measurement"
)]

use core::fmt::Write as _;
use core::num::{NonZeroU16, NonZeroUsize};

use divan::Bencher;
use divan::counter::BytesCount;
use plx_emulator::{Emulator, GridSize, Key, KeyChar, KeyEvent, Mods, Version};
use plx_ghostty::{GhosttyEmulator, GraphemeClustering, Options, Scrollback};

fn main() {
    divan::main();
}

fn emulator(scrollback: Scrollback) -> GhosttyEmulator {
    let size = GridSize::new(
        NonZeroU16::new(80).expect("80 is not zero"),
        NonZeroU16::new(24).expect("24 is not zero"),
    )
    .expect("80x24 fits");
    let options = Options {
        scrollback,
        continuation_max_bytes: NonZeroUsize::new(4096).expect("4096 is not zero"),
        grapheme_clustering: GraphemeClustering::Enabled,
    };
    GhosttyEmulator::new(size, &options).expect("a terminal is created")
}

/// A full-screen program redrawing a status line and a counter: a few
/// dirty rows per frame, the common case.
#[divan::bench]
fn redraw_feed_and_sync(bencher: Bencher) {
    let frames: [&[u8]; 2] = [
        b"\x1b[H\x1b[1;32mstatus: ok\x1b[0m   \x1b[5;10Hcounter 0001\x1b[24;1Hready",
        b"\x1b[H\x1b[1;31mstatus: busy\x1b[0m \x1b[5;10Hcounter 0002\x1b[24;1Hworking",
    ];
    let mut term = emulator(Scrollback::Disabled);
    let mut version = Version::ZERO;
    let mut i = 0_usize;
    bencher
        .counter(BytesCount::new(frames[0].len()))
        .bench_local(|| {
            i += 1;
            version = version.next();
            term.feed(frames[i % 2]);
            term.sync(version).expect("sync")
        });
}

/// A program flooding output (a build log): 64 KiB of scrolling lines, fed
/// and synced as one read.
#[divan::bench]
fn flood_feed_and_sync(bencher: Bencher) {
    let line = b"   Compiling plx-core v0.1.0 (/home/user/plx/crates/plx-core)\r\n";
    let chunk: Vec<u8> = line.iter().copied().cycle().take(64 * 1024).collect();
    let mut term = emulator(Scrollback::Bytes(
        NonZeroUsize::new(1 << 20).expect("not zero"),
    ));
    let mut version = Version::ZERO;
    bencher
        .counter(BytesCount::new(chunk.len()))
        .bench_local(|| {
            version = version.next();
            term.feed(&chunk);
            term.sync(version).expect("sync")
        });
}

/// Encoding a keystroke for the focused pane.
#[divan::bench]
fn encode_key(bencher: Bencher) {
    let mut term = emulator(Scrollback::Disabled);
    let key = KeyEvent::press(Key::Char(KeyChar::from_byte(b'x')), Mods::empty());
    let mut out = Vec::with_capacity(64);
    bencher.bench_local(|| {
        out.clear();
        term.encode_key(&key, &mut out);
        out.len()
    });
}

/// Lines of mixed length (short, a screen wide, longer than any width)
/// that rewrap at every width: a long-running shell's history.
fn history(lines: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..lines {
        let len = [12, 70, 150, 260][i % 4] + i % 7;
        out.extend(format!("{i:08} ").bytes());
        out.extend((0..len).map(|j| b'a' + u8::try_from((i + j) % 26).expect("< 26")));
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Resizing a terminal `cols` wide and 50 rows tall, holding `lines` of
/// history, between `cols` and three fifths of it, which reflows all of
/// it, then syncing the screen.
fn resize_reflow(bencher: Bencher, cols: u16, lines: usize, scrollback: Scrollback) {
    let mut term = emulator(scrollback);
    let size = |cols: u16| {
        GridSize::new(
            NonZeroU16::new(cols).expect("not zero"),
            NonZeroU16::new(50).expect("not zero"),
        )
        .expect("fits")
    };
    term.resize(size(cols), None).expect("resized");
    term.feed(&history(lines));
    // Wrapped lines take several rows, and a byte limit prunes the oldest.
    eprintln!(
        "{lines} lines fed: {} rows of scrollback at {cols} columns",
        term.scrollback_rows()
    );
    let mut version = Version::ZERO;
    let mut narrow = false;
    bencher.bench_local(|| {
        narrow = !narrow;
        let width = if narrow { cols / 5 * 3 } else { cols };
        term.resize(size(width), None).expect("resized");
        version = version.next();
        term.sync(version).expect("sync")
    });
}

#[divan::bench(args = [10_000, 100_000])]
fn resize_reflow_unlimited(bencher: Bencher, lines: usize) {
    resize_reflow(bencher, 200, lines, Scrollback::Unlimited);
}

/// The daemon's limit (10 MiB per pane), full: what a pane really holds,
/// at a narrow and a wide width.
#[divan::bench(args = [80, 200])]
fn resize_reflow_daemon_limit(bencher: Bencher, cols: u16) {
    resize_reflow(
        bencher,
        cols,
        100_000,
        Scrollback::Bytes(NonZeroUsize::new(10 << 20).expect("not zero")),
    );
}

/// A 200×50 pane holding the daemon's limit of history (10 MiB), full.
fn full_pane() -> GhosttyEmulator {
    let mut term = emulator(Scrollback::Bytes(
        NonZeroUsize::new(10 << 20).expect("not zero"),
    ));
    let size = GridSize::new(
        NonZeroU16::new(200).expect("not zero"),
        NonZeroU16::new(50).expect("not zero"),
    )
    .expect("fits");
    term.resize(size, None).expect("resized");
    term.feed(&history(100_000));
    term.sync(Version::new(1)).expect("sync");
    term
}

/// Editing the scrollback: all of a full pane as text.
#[divan::bench]
fn history_text_full_pane(bencher: Bencher) {
    let mut term = full_pane();
    let mut out = Vec::new();
    bencher.bench_local(|| {
        out.clear();
        term.history_text(&mut out);
        out.len()
    });
}

/// A sync with nothing new and no search: the floor every frame pays.
#[divan::bench]
fn sync_unchanged(bencher: Bencher) {
    let mut term = full_pane();
    let mut version = Version::new(1);
    bencher.bench_local(|| {
        version = version.next();
        term.sync(version).expect("sync")
    });
}

/// While a search is on in a full pane, output keeps coming: one new line,
/// then the sync (which catches the search up, counts and marks). `0`: any
/// case; `1`: letter case; `2`: whole words.
#[divan::bench(args = [0, 1, 2])]
fn search_while_output_streams(bencher: Bencher, options: u8) {
    use plx_emulator::{Case, SearchOptions, Words};
    let mut term = full_pane();
    let options = match options {
        0 => SearchOptions::default(),
        1 => SearchOptions {
            case: Case::Matched,
            words: Words::Within,
        },
        _ => SearchOptions {
            case: Case::Ignored,
            words: Words::Whole,
        },
    };
    // "xyz" is in about one line in four of the history.
    term.search("xyz", options);
    let mut version = Version::new(1);
    let mut n = 0_u32;
    // Reused, so the only allocations counted are the emulator's.
    let mut line = String::with_capacity(32);
    bencher.bench_local(|| {
        n += 1;
        line.clear();
        write!(line, "line {n} xyz\r\n").expect("a String takes any text");
        term.feed(line.as_bytes());
        version = version.next();
        term.sync(version).expect("sync")
    });
    eprintln!("search {options:?}: {:?}", term.search_status());
}

/// One `n`: the next match, in a full pane.
#[divan::bench]
fn search_select_next(bencher: Bencher) {
    use plx_emulator::{SearchDirection, SearchOptions};
    let mut term = full_pane();
    term.search("xyz", SearchOptions::default());
    bencher.bench_local(|| term.search_select(SearchDirection::Older));
}

/// A search key with a filter on (here: setting the options again): the
/// exact count over every match of a full pane. `1`: letter case; `2`:
/// whole words.
#[divan::bench(args = [1, 2])]
fn search_exact_filtered_count(bencher: Bencher, options: u8) {
    use plx_emulator::{Case, SearchOptions, Words};
    let mut term = full_pane();
    let filtered = if options == 1 {
        SearchOptions {
            case: Case::Matched,
            words: Words::Within,
        }
    } else {
        SearchOptions {
            case: Case::Ignored,
            words: Words::Whole,
        }
    };
    // The search of the whole history happens once, here: libghostty keeps
    // its matches while the needle stays, so each sample counts afresh only.
    term.search("xyz", SearchOptions::default());
    let mut flip = false;
    bencher.bench_local(|| {
        // Alternating with no filter, so each call counts afresh.
        flip = !flip;
        let options = if flip {
            filtered
        } else {
            SearchOptions::default()
        };
        term.search("xyz", options);
    });
}

/// Random hex lines, as `od` prints them: the hard case for compression.
fn hex_history(lines: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    for _ in 0..lines {
        for _ in 0..16 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend(format!("{:02x} ", state & 0xff).bytes());
        }
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// A 200×50 pane with the daemon's 10 MiB of history, of `content`:
/// `0` log-like text, `1` random hex.
fn full_pane_of(content: u8) -> GhosttyEmulator {
    let mut term = emulator(Scrollback::Bytes(
        NonZeroUsize::new(10 << 20).expect("not zero"),
    ));
    let size = GridSize::new(
        NonZeroU16::new(200).expect("not zero"),
        NonZeroU16::new(50).expect("not zero"),
    )
    .expect("fits");
    term.resize(size, None).expect("resized");
    if content == 0 {
        term.feed(&history(100_000));
    } else {
        term.feed(&hex_history(300_000));
    }
    term.sync(Version::new(1)).expect("sync");
    term
}

const fn kib(bytes: u64) -> u64 {
    bytes / 1024
}

/// Compressing a full pane in one go; the memory before and after is
/// printed once. `0`: log-like text; `1`: random hex.
#[divan::bench(args = [0, 1], sample_count = 10)]
fn compress_full_pane(bencher: Bencher, content: u8) {
    let mut term = full_pane_of(content);
    let before = term.memory_usage();
    term.compress_fully();
    let after = term.memory_usage();
    eprintln!(
        "content {content}: resident {} KiB -> {} KiB ({} of {} pages compressed), reserved {} KiB",
        kib(before.resident_bytes),
        kib(after.resident_bytes),
        after.compressed_pages,
        after.pages,
        kib(after.reserved_bytes),
    );
    bencher
        .with_inputs(|| full_pane_of(content))
        .bench_local_values(|mut term| term.compress_fully());
}

/// One incremental step on a full pane: the work the session does at a
/// time while the pane is idle.
#[divan::bench(args = [0, 1], sample_count = 10)]
fn compress_one_step(bencher: Bencher, content: u8) {
    bencher
        .with_inputs(|| full_pane_of(content))
        .bench_local_values(|mut term| term.compress());
}

/// A resize of a compressed full pane, and what it leaves compressed.
#[divan::bench(args = [0, 1], sample_count = 10)]
fn resize_compressed_pane(bencher: Bencher, content: u8) {
    let narrow = GridSize::new(
        NonZeroU16::new(120).expect("not zero"),
        NonZeroU16::new(50).expect("not zero"),
    )
    .expect("fits");
    let mut term = full_pane_of(content);
    term.compress_fully();
    term.resize(narrow, None).expect("resized");
    term.sync(Version::new(2)).expect("sync");
    let after = term.memory_usage();
    eprintln!(
        "content {content}: after a resize of the compressed pane, resident {} KiB ({} of {} pages compressed)",
        kib(after.resident_bytes),
        after.compressed_pages,
        after.pages,
    );
    bencher
        .with_inputs(|| {
            let mut term = full_pane_of(content);
            term.compress_fully();
            term
        })
        .bench_local_values(|mut term| {
            term.resize(narrow, None).expect("resized");
            term.sync(Version::new(2)).expect("sync")
        });
}

/// A search over a compressed full pane (the needle set, matches counted).
#[divan::bench(args = [0, 1], sample_count = 10)]
fn search_compressed_pane(bencher: Bencher, content: u8) {
    use plx_emulator::SearchOptions;
    bencher
        .with_inputs(|| {
            let mut term = full_pane_of(content);
            term.compress_fully();
            term
        })
        .bench_local_values(|mut term| term.search("e3", SearchOptions::default()));
}

/// The same search over the full pane uncompressed, to compare.
#[divan::bench(args = [0, 1], sample_count = 10)]
fn search_uncompressed_pane(bencher: Bencher, content: u8) {
    use plx_emulator::SearchOptions;
    bencher
        .with_inputs(|| full_pane_of(content))
        .bench_local_values(|mut term| term.search("e3", SearchOptions::default()));
}

/// The same resize of the full pane uncompressed, to compare.
#[divan::bench(args = [0, 1], sample_count = 10)]
fn resize_uncompressed_pane(bencher: Bencher, content: u8) {
    let narrow = GridSize::new(
        NonZeroU16::new(120).expect("not zero"),
        NonZeroU16::new(50).expect("not zero"),
    )
    .expect("fits");
    bencher
        .with_inputs(|| full_pane_of(content))
        .bench_local_values(|mut term| {
            term.resize(narrow, None).expect("resized");
            term.sync(Version::new(2)).expect("sync")
        });
}
