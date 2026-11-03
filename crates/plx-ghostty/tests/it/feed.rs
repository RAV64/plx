use std::fmt::Write as _;

use plx_emulator::{Bell, Color, Emulator, Percent, Progress, Rgb, Style, Version, Width};
use plx_ghostty::{GhosttyEmulator, GraphemeClustering};
use proptest::prelude::*;

use crate::vt;

use crate::{Effects, view};

/// Feeds `chunks` in order and returns everything they triggered.
fn feed_all(emulator: &mut impl Emulator, chunks: &[Vec<u8>]) -> Effects {
    let mut effects = Effects::default();
    for chunk in chunks {
        effects.feed(emulator, chunk);
    }
    effects
}

proptest! {
    /// No cell ever holds a control character, whatever a program sends:
    /// terminals execute or drop them, and clients refuse frames carrying
    /// them. (libghostty 1.4 itself stores DEL as a glyph; the screen
    /// writer blanks it.)
    #[test]
    fn cells_never_hold_control_characters(
        size in vt::grid_size(),
        options in vt::options(),
        bytes in proptest::collection::vec(
            prop_oneof![any::<u8>(), Just(0x7F_u8), 0_u8..0x20, Just(0xC2_u8), 0x80_u8..0xA0],
            0..400,
        ),
    ) {
        use plx_emulator::GlyphKind;
        let mut emulator = GhosttyEmulator::new(size, &options).unwrap();
        emulator.feed(&bytes);
        emulator.sync(Version::new(1)).unwrap();
        let screen = emulator.screen();
        for row in screen.grid().rows() {
            for cell in row {
                match cell.glyph.kind() {
                    GlyphKind::Empty => {}
                    GlyphKind::Char(c) => prop_assert!(!c.is_control(), "{:#x}", u32::from(c)),
                    GlyphKind::Cluster(id) => {
                        prop_assert!(!screen.tables().grapheme(id).iter().any(|c| c.is_control()));
                    }
                }
            }
        }
    }

    /// The screen and the events depend only on the bytes, never on how they
    /// were split across `feed` calls (escape sequences and UTF-8 may be cut
    /// anywhere).
    #[test]
    fn split_anywhere_gives_the_same_screen_and_events(
        size in vt::grid_size(),
        options in vt::options(),
        (bytes, chunks) in vt::stream().prop_flat_map(|bytes| (Just(bytes.clone()), vt::chunked(bytes))),
    ) {
        let mut whole = GhosttyEmulator::new(size, &options).unwrap();
        let mut split = GhosttyEmulator::new(size, &options).unwrap();

        let whole_events = feed_all(&mut whole, &[bytes]);
        let split_events = feed_all(&mut split, &chunks);

        prop_assert_eq!(view(&mut whole), view(&mut split));
        prop_assert_eq!(whole_events, split_events);
    }

    /// The synced grid always has the terminal's exact shape, through any
    /// interleaving of output and resizes, and syncing again without new
    /// output changes nothing.
    #[test]
    fn grids_always_match_the_terminal_size(
        initial in vt::grid_size(),
        options in vt::options(),
        steps in proptest::collection::vec((vt::stream(), vt::grid_size()), 1..6),
    ) {
        let mut emulator = GhosttyEmulator::new(initial, &options).unwrap();
        for (version, (bytes, size)) in (1_u64..).map(Version::new).zip(steps) {
            emulator.feed(&bytes);
            emulator.resize(size, None).unwrap();

            emulator.sync(version).unwrap();
            let grid = emulator.screen().grid();
            prop_assert_eq!(grid.size(), size);
            prop_assert_eq!(grid.rows().count(), usize::from(size.rows().get()));
            for row in grid.rows() {
                prop_assert_eq!(row.len(), usize::from(size.cols().get()));
            }
            if let Some(position) = grid.cursor().position {
                prop_assert!(position.x < size.cols().get() && position.y < size.rows().get());
            }
            prop_assert!(emulator.sync(Version::new(version.get() + 1000)).unwrap().is_empty());
        }
    }

    /// Syncing only dirty rows produces the same grid as reading everything:
    /// an emulator synced after every chunk matches one synced once.
    #[test]
    fn incremental_sync_matches_a_full_read(
        size in vt::grid_size(),
        options in vt::options(),
        chunks in proptest::collection::vec(vt::stream(), 1..6),
    ) {
        let mut incremental = GhosttyEmulator::new(size, &options).unwrap();
        let mut once = GhosttyEmulator::new(size, &options).unwrap();
        for (version, chunk) in (1_u64..).map(Version::new).zip(&chunks) {
            incremental.feed(chunk);
            incremental.sync(version).unwrap();
            once.feed(chunk);
        }
        prop_assert_eq!(view(&mut incremental), view(&mut once));
    }
}

#[test]
fn primary_device_attributes_query_is_answered() {
    let mut emulator = crate::emulator(crate::size(80, 24));
    let events = emulator.feed(b"\x1b[c");
    assert!(
        events.replies().starts_with(b"\x1b[?"),
        "unexpected DA1 reply {:?}",
        events.replies()
    );
    assert_eq!(events.bell(), Bell::Quiet);
}

#[test]
fn replies_keep_stream_order_and_bells_are_reported() {
    let mut emulator = crate::emulator(crate::size(80, 24));
    let events = emulator.feed(b"\x07\x1b[5n\x07\x1b[6n");
    assert_eq!(events.replies(), b"\x1b[0n\x1b[1;1R");
    assert_eq!(events.bell(), Bell::Rang);
}

#[test]
fn each_feed_lends_only_its_own_events() {
    let mut emulator = crate::emulator(crate::size(80, 24));
    assert_eq!(emulator.feed(b"\x07").bell(), Bell::Rang);
    assert!(emulator.feed(b"plain text").is_empty());
}

#[test]
fn wide_characters_occupy_two_cells() {
    let mut emulator = crate::emulator(crate::size(10, 2));
    emulator.feed("漢x".as_bytes());
    let cells = &view(&mut emulator).rows[0];
    assert_eq!((cells[0].0.as_str(), cells[0].1), ("漢", Width::Wide));
    assert_eq!((cells[1].0.as_str(), cells[1].1), ("", Width::SpacerTail));
    assert_eq!((cells[2].0.as_str(), cells[2].1), ("x", Width::Narrow));
}

#[test]
fn grapheme_clustering_keeps_zwj_sequences_in_one_cell() {
    let mut emulator = GhosttyEmulator::new(
        crate::size(10, 2),
        &crate::options(GraphemeClustering::Enabled),
    )
    .unwrap();
    emulator.feed("👩‍💻e\u{301}".as_bytes());
    let cells = &view(&mut emulator).rows[0];
    assert_eq!((cells[0].0.as_str(), cells[0].1), ("👩‍💻", Width::Wide));
    assert_eq!(cells[2].0, "e\u{301}");
}

#[test]
fn without_grapheme_clustering_zwj_sequences_split() {
    let mut emulator = GhosttyEmulator::new(
        crate::size(10, 2),
        &crate::options(GraphemeClustering::Disabled),
    )
    .unwrap();
    emulator.feed("👩‍💻".as_bytes());
    let cells = &view(&mut emulator).rows[0];
    // Legacy per-codepoint widths: the ZWJ joins the first emoji's cell and
    // the second emoji starts its own.
    assert_eq!(cells[0].0, "👩\u{200d}");
    assert_eq!(cells[2].0, "💻");
}

#[test]
fn styles_are_interned_once_per_pane() {
    let mut emulator = crate::emulator(crate::size(20, 2));
    emulator.feed(b"\x1b[1;31mred\x1b[0m plain \x1b[1;31mred\x1b[0m");
    let damage = emulator.sync(Version::new(1)).unwrap();
    assert!(damage.rows >= 1);
    // Default plus one bold red style, shared by both runs.
    let screen = emulator.screen();
    assert_eq!(screen.tables().style_count(), 2);
    let row = screen.grid().row_or_none(0).unwrap();
    assert_eq!(row[0].style, row[11].style);
    assert_ne!(row[0].style, row[4].style);
}

#[test]
fn erasing_keeps_the_background_colour() {
    // Erase-with-background makes cells that hold no text, only a colour
    // (libghostty's bg-colour content tags), and carry the default style.
    let mut emulator = crate::emulator(crate::size(4, 2));
    emulator.feed(b"\x1b[44m\x1b[2J\x1b[2;1H\x1b[48;2;1;2;3m\x1b[K");
    let screen = view(&mut emulator);
    let palette = Style {
        bg: Color::Palette(4),
        ..Style::DEFAULT
    };
    let rgb = Style {
        bg: Color::Rgb(Rgb { r: 1, g: 2, b: 3 }),
        ..Style::DEFAULT
    };
    for (row, style) in screen.rows.iter().zip([palette, rgb]) {
        for cell in row {
            assert_eq!(cell, &(String::new(), Width::Narrow, style));
        }
    }
}

#[test]
fn compiled_out_protocols_never_answer() {
    let mut emulator = crate::emulator(crate::size(20, 4));
    let mut effects = Effects::default();
    // Glyph protocol support query, and a kitty graphics query.
    effects.feed(&mut emulator, b"\x1b_25a1;s\x1b\\");
    effects.feed(&mut emulator, b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    assert_eq!(effects, Effects::default());
}

/// A program printing endless unique truecolor styles and unique grapheme
/// clusters keeps the pane's tables bounded by what is on screen: entries
/// die with the last cell that uses them. (Append-only tables grew without
/// limit, then failed every sync once full.)
#[test]
fn endless_unique_styles_and_clusters_stay_bounded() {
    let cells = 20 * 5;
    let mut emulator = crate::emulator(crate::size(20, 5));
    let mut version = Version::ZERO;
    let mut chunk = String::new();
    for n in 0..30_000_u32 {
        let [_, r, g, b] = n.to_be_bytes();
        // Two combining marks from U+0300..U+036F make 12 544 distinct clusters.
        let marks = |i: u32| char::from_u32(0x300 + i % 112).unwrap();
        write!(
            chunk,
            "\x1b[38;2;{r};{g};{b}ma{}{}",
            marks(n),
            marks(n / 112)
        )
        .unwrap();
        if n % 50 == 49 {
            emulator.feed(chunk.as_bytes());
            chunk.clear();
            version = version.next();
            emulator.sync(version).unwrap();
            let tables = emulator.screen().tables();
            assert!(
                tables.style_count() <= cells + 1,
                "{} styles",
                tables.style_count()
            );
            assert!(
                tables.grapheme_count() <= cells,
                "{} clusters",
                tables.grapheme_count()
            );
            // Each cluster here is 3 characters; freed space is reclaimed.
            assert!(tables.grapheme_arena_len() <= 2 * 3 * cells);
        }
    }
    // The run really filled the screen with distinct entries.
    let tables = emulator.screen().tables();
    assert!(tables.style_count() > cells / 2 && tables.grapheme_count() > cells / 2);
}

#[test]
fn the_scrollback_holds_the_rows_above_the_screen() {
    use core::num::NonZeroUsize;
    use plx_emulator::GridSize;
    use plx_ghostty::{Options, Scrollback};
    let size = GridSize::new(80.try_into().unwrap(), 24.try_into().unwrap()).unwrap();
    let options = |scrollback| Options {
        scrollback,
        continuation_max_bytes: NonZeroUsize::new(4096).unwrap(),
        grapheme_clustering: GraphemeClustering::Enabled,
    };
    let lines: Vec<u8> = (0..100)
        .flat_map(|i| format!("line {i}\r\n").into_bytes())
        .collect();
    // 100 lines and the cursor's empty one: 101 rows, 24 on the screen.
    let mut unlimited = GhosttyEmulator::new(size, &options(Scrollback::Unlimited)).unwrap();
    unlimited.feed(&lines);
    assert_eq!(unlimited.scrollback_rows(), 77);
    let mut none = GhosttyEmulator::new(size, &options(Scrollback::Disabled)).unwrap();
    none.feed(&lines);
    assert_eq!(none.scrollback_rows(), 0);
}

/// A wide character that does not fit at a row's end leaves a spacer there
/// and wraps; overwriting half of it on the next row clears the spacer on
/// the row above too, which libghostty does not mark dirty. The sync must
/// still see it (found by `incremental_sync_matches_a_full_read`).
#[test]
fn clearing_a_wrapped_wide_character_clears_the_spacer_above() {
    let options = crate::options(GraphemeClustering::Enabled);
    let size = crate::size(4, 2);
    let mut incremental = GhosttyEmulator::new(size, &options).unwrap();
    let mut once = GhosttyEmulator::new(size, &options).unwrap();
    for (version, chunk) in [(1, &b"abc\xe5\xad\x97"[..]), (2, b"\x08 ")] {
        incremental.feed(chunk);
        incremental.sync(Version::new(version)).unwrap();
        once.feed(chunk);
    }
    assert_eq!(view(&mut incremental), view(&mut once));
}

/// The view moves back through the history and returns; new output does
/// not move a view that is back in the history.
#[test]
fn the_view_scrolls_through_the_history() {
    use core::num::NonZeroUsize;
    use plx_emulator::{Scroll, ScrollPosition};
    use plx_ghostty::{Options, Scrollback};
    let options = Options {
        scrollback: Scrollback::Unlimited,
        continuation_max_bytes: NonZeroUsize::new(4096).unwrap(),
        grapheme_clustering: GraphemeClustering::Enabled,
    };
    let mut emulator = GhosttyEmulator::new(crate::size(10, 3), &options).unwrap();
    let lines: Vec<u8> = (0..10)
        .flat_map(|i| format!("line {i}\r\n").into_bytes())
        .collect();
    emulator.feed(&lines);
    let text = |emulator: &mut GhosttyEmulator, version| {
        emulator.sync(Version::new(version)).unwrap();
        view(emulator).text()
    };
    // 10 lines and the cursor's row: 11 rows, 3 on the screen.
    assert_eq!(
        emulator.scroll_position(),
        ScrollPosition {
            back: 0,
            history: 8
        }
    );
    assert_eq!(text(&mut emulator, 1), ["line 8", "line 9", ""]);
    emulator.scroll(Scroll::Up(2));
    assert_eq!(emulator.scroll_position().back, 2);
    assert_eq!(text(&mut emulator, 2), ["line 6", "line 7", "line 8"]);
    // Output while back in the history leaves the view where it is.
    emulator.feed(b"line 10\r\n");
    assert_eq!(text(&mut emulator, 3), ["line 6", "line 7", "line 8"]);
    emulator.scroll(Scroll::Top);
    assert_eq!(text(&mut emulator, 4), ["line 0", "line 1", "line 2"]);
    assert_eq!(emulator.scroll_position().back, 9);
    emulator.scroll(Scroll::Down(1));
    assert_eq!(text(&mut emulator, 5), ["line 1", "line 2", "line 3"]);
    emulator.scroll(Scroll::Bottom);
    assert_eq!(emulator.scroll_position().back, 0);
    assert_eq!(text(&mut emulator, 6), ["line 9", "line 10", ""]);
    // Scrolling past either end stops there.
    emulator.scroll(Scroll::Down(5));
    assert_eq!(emulator.scroll_position().back, 0);
    emulator.scroll(Scroll::Up(500));
    assert_eq!(emulator.scroll_position().back, 9);
}

/// Jumping by prompts follows the shell's semantic prompt marks (OSC 133):
/// the prompt becomes the view's first row.
#[test]
fn the_view_jumps_between_prompts() {
    use core::num::NonZeroUsize;
    use plx_emulator::Scroll;
    use plx_ghostty::{Options, Scrollback};
    let options = Options {
        scrollback: Scrollback::Unlimited,
        continuation_max_bytes: NonZeroUsize::new(4096).unwrap(),
        grapheme_clustering: GraphemeClustering::Enabled,
    };
    let mut emulator = GhosttyEmulator::new(crate::size(20, 3), &options).unwrap();
    emulator.feed(b"\x1b]133;A\x07$ one\r\na\r\nb\r\nc\r\n");
    emulator.feed(b"\x1b]133;A\x07$ two\r\nd\r\ne\r\nf\r\n");
    emulator.feed(b"\x1b]133;A\x07$ ");
    let text = |emulator: &mut GhosttyEmulator| view(emulator).text();
    assert_eq!(text(&mut emulator), ["e", "f", "$"]);
    emulator.scroll(Scroll::Prompts(-1));
    assert_eq!(text(&mut emulator), ["$ two", "d", "e"]);
    emulator.scroll(Scroll::Prompts(-1));
    assert_eq!(text(&mut emulator), ["$ one", "a", "b"]);
    emulator.scroll(Scroll::Prompts(1));
    assert_eq!(text(&mut emulator), ["$ two", "d", "e"]);
}

/// A line limit keeps at least that many lines, and prunes the rest in
/// whole pages.
#[test]
fn a_line_limit_bounds_the_history() {
    use core::num::NonZeroUsize;
    use plx_ghostty::{Options, Scrollback};
    let options = Options {
        scrollback: Scrollback::Lines(NonZeroUsize::new(100).unwrap()),
        continuation_max_bytes: NonZeroUsize::new(4096).unwrap(),
        grapheme_clustering: GraphemeClustering::Enabled,
    };
    let mut emulator = GhosttyEmulator::new(crate::size(80, 24), &options).unwrap();
    let lines: Vec<u8> = (0..2000)
        .flat_map(|i| format!("line {i}\r\n").into_bytes())
        .collect();
    emulator.feed(&lines);
    let kept = emulator.scrollback_rows();
    assert!((100..1000).contains(&kept), "{kept} rows of history");
}

/// Compressing the history step by step ends, frees most of its memory,
/// and changes nothing it shows.
#[test]
fn compressing_the_history_frees_memory_and_keeps_its_text() {
    use plx_emulator::Compression;
    // Many pages: the live one is never compressed.
    let options = plx_ghostty::Options {
        scrollback: plx_ghostty::Scrollback::Bytes((8 << 20).try_into().unwrap()),
        ..crate::options(plx_ghostty::GraphemeClustering::Enabled)
    };
    let mut emulator = plx_ghostty::GhosttyEmulator::new(crate::size(80, 24), &options).unwrap();
    for n in 0..100_000 {
        emulator.feed(format!("line {n} of some ordinary output\r\n").as_bytes());
    }
    let mut before_text = Vec::new();
    emulator.history_text(&mut before_text);
    let before = emulator.memory_usage();
    let mut steps = 0;
    while emulator.compress() == Compression::Pending {
        steps += 1;
        assert!(steps < 10_000, "compression ends");
    }
    let after = emulator.memory_usage();
    assert!(after.compressed_pages > 0, "{after:?}");
    assert!(
        after.resident_bytes < before.resident_bytes / 2,
        "{before:?} -> {after:?}"
    );
    // Nothing changes but how it is stored.
    let mut after_text = Vec::new();
    emulator.history_text(&mut after_text);
    assert_eq!(after_text, before_text);
}

/// A program's OSC 52 clipboard write comes out with its output's events,
/// decoded; one for the primary selection does not (the client sets the
/// clipboard).
#[test]
fn a_programs_clipboard_write_comes_out_decoded() {
    let mut emulator = crate::emulator(crate::size(20, 4));
    let events = emulator.feed(b"before\x1b]52;c;aGVsbG8gd29ybGQ=\x07after");
    assert_eq!(events.clipboard_or_none(), Some(&b"hello world"[..]));
    let events = emulator.feed(b"\x1b]52;p;aGVsbG8=\x07");
    assert_eq!(events.clipboard_or_none(), None);
    // The text around it is still printed.
    assert_eq!(crate::view(&mut emulator).text()[0], "beforeafter");
}

/// A program's progress report (OSC 9;4) comes out with its output's
/// events, typed, in each of the protocol's states; the last one in a feed
/// wins, and nothing of it is printed.
#[test]
fn a_programs_progress_report_comes_out_typed() {
    let percent = |p| Some(Percent::new(p).unwrap());
    let mut emulator = crate::emulator(crate::size(20, 4));
    for (report, progress) in [
        (&b"\x1b]9;4;1;42\x07"[..], Progress::Set(percent(42))),
        (b"\x1b]9;4;2;7\x1b\\", Progress::Error(percent(7))),
        (b"\x1b]9;4;3\x07", Progress::Indeterminate),
        (b"\x1b]9;4;4;100\x07", Progress::Paused(percent(100))),
        (b"\x1b]9;4;0\x07", Progress::Removed),
    ] {
        let events = emulator.feed(report);
        assert_eq!(events.progress_or_none(), Some(progress), "{report:?}");
    }
    let events = emulator.feed(b"a\x1b]9;4;1;10\x07b\x1b]9;4;1;20\x07c");
    assert_eq!(events.progress_or_none(), Some(Progress::Set(percent(20))));
    assert_eq!(emulator.feed(b"d").progress_or_none(), None);
    assert_eq!(crate::view(&mut emulator).text()[0], "abcd");
}

/// A full reset (RIS) removes the program's report, and says so.
#[test]
fn a_full_reset_removes_the_progress_report() {
    let mut emulator = crate::emulator(crate::size(20, 4));
    emulator.feed(b"\x1b]9;4;3\x07");
    let events = emulator.feed(b"\x1bc");
    assert_eq!(events.progress_or_none(), Some(Progress::Removed));
}

/// What libghostty makes of reports outside the protocol: it clamps a
/// percentage past 100, reads a missing one as 0, and drops a state the
/// protocol does not have.
#[test]
fn reports_outside_the_protocol_stay_within_it() {
    let percent = |p| Some(Percent::new(p).unwrap());
    let mut emulator = crate::emulator(crate::size(20, 4));
    for (report, progress) in [
        (
            &b"\x1b]9;4;1;250\x07"[..],
            Some(Progress::Set(percent(100))),
        ),
        (b"\x1b]9;4;1\x07", Some(Progress::Set(percent(0)))),
        (b"\x1b]9;4;9;50\x07", None),
        (b"\x1b]9;4;\x07", None),
    ] {
        let events = emulator.feed(report);
        assert_eq!(events.progress_or_none(), progress, "{report:?}");
    }
}
