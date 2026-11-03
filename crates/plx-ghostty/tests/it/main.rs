//! Integration tests for the libghostty-vt adapter, against the real library.
//!
//! One test binary for the crate (modules below) keeps links to one per
//! change.

// Marks everything here as test code, so clippy's allow-*-in-tests settings
// (clippy.toml) also cover the helpers outside #[test] functions.
#![cfg(test)]
// Tests copy byte streams to feed them two ways, and proptest's macros clone
// internally; production code gets no such allowance.
#![expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "tests copy values to compare them; proptest macros use Arc and clone internally"
)]

plx_alloc_count::install!();

mod alloc;
mod build;
mod feed;
mod input;
mod search;
mod snapshot;
mod vt;

use core::num::{NonZeroU16, NonZeroUsize};

use plx_emulator::{Bell, CellText, Cursor, Emulator, GridSize, Progress, Style, Version, Width};
use plx_ghostty::{GhosttyEmulator, GraphemeClustering, Options, Scrollback};

pub(crate) const CONTINUATION_MAX_BYTES: NonZeroUsize = NonZeroUsize::new(4096).unwrap();
pub(crate) const SCROLLBACK: Scrollback = Scrollback::Bytes(NonZeroUsize::new(1 << 20).unwrap());

pub(crate) fn size(cols: u16, rows: u16) -> GridSize {
    GridSize::new(
        NonZeroU16::new(cols).unwrap(),
        NonZeroU16::new(rows).unwrap(),
    )
    .unwrap()
}

pub(crate) fn options(grapheme_clustering: GraphemeClustering) -> Options {
    Options {
        scrollback: SCROLLBACK,
        continuation_max_bytes: CONTINUATION_MAX_BYTES,
        grapheme_clustering,
    }
}

/// A terminal with grapheme clustering on, plx's intended pane default.
pub(crate) fn emulator(size: GridSize) -> GhosttyEmulator {
    GhosttyEmulator::new(size, &options(GraphemeClustering::Enabled)).unwrap()
}

/// What a terminal shows, resolved through its tables into owned values, so
/// terminals whose tables were filled in different orders compare equal
/// exactly when they look the same.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct View {
    pub(crate) rows: Vec<Vec<(String, Width, Style)>>,
    pub(crate) cursor: Cursor,
}

impl View {
    /// Each row's text, without trailing blanks.
    pub(crate) fn text(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|row| {
                let text: String = row
                    .iter()
                    .map(|(text, ..)| if text.is_empty() { " " } else { text.as_str() })
                    .collect();
                String::from(text.trim_end())
            })
            .collect()
    }
}

/// Syncs `emulator` and returns what it shows.
pub(crate) fn view(emulator: &mut impl Emulator) -> View {
    emulator.sync(Version::new(1)).unwrap();
    let screen = emulator.screen();
    let grid = screen.grid();
    let rows = grid
        .rows()
        .map(|row| {
            row.iter()
                .map(|cell| {
                    let resolved = screen.resolve(*cell);
                    let text = match resolved.text {
                        CellText::Empty => String::new(),
                        CellText::Char(c) => c.to_string(),
                        CellText::Cluster(chars) => chars.iter().collect(),
                    };
                    (text, resolved.width, *resolved.style)
                })
                .collect()
        })
        .collect();
    View {
        rows,
        cursor: grid.cursor(),
    }
}

/// Replies, bells and the last progress report accumulated over several
/// feeds (each feed lends its events only until the next one).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Effects {
    pub(crate) replies: Vec<u8>,
    pub(crate) bell: Bell,
    pub(crate) progress: Option<Progress>,
}

impl Effects {
    pub(crate) fn feed(&mut self, emulator: &mut impl Emulator, bytes: &[u8]) {
        let events = emulator.feed(bytes);
        self.replies.extend_from_slice(events.replies());
        if events.bell() == Bell::Rang {
            self.bell = Bell::Rang;
        }
        if let Some(progress) = events.progress_or_none() {
            self.progress = Some(progress);
        }
    }
}
