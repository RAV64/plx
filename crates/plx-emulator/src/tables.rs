//! Per-pane tables of what cells refer to: styles and multi-codepoint
//! grapheme clusters.
//!
//! An entry lives exactly while some cell of the screen uses it; the
//! [`crate::Screen`] counts uses as it writes rows, and a slot nobody uses is
//! freed for reuse. Each slot records the version that (re)defined it, so a
//! client catching up from an acknowledged version receives the slots defined
//! since, exactly like rows. Consistent because a slot is freed only when no
//! cell uses it: every row that used its old meaning was rewritten, and is
//! sent in the same frame.
//!
//! Bounded: at most one entry per cell, plus one row being written, which is
//! far below what the ids can name. Neither table can fill, whatever a
//! program prints.

use alloc::vec::Vec;

use plx_bug::{OrBug as _, invariant};
use plx_collections::{Interned, Interner, SliceInterner};

use crate::cell::{Cell, GlyphKind, GraphemeId, StyleId};
use crate::grid::{GridSize, Version};
use crate::style::Style;

/// Styles a pane can hold besides the default: one per non-zero [`StyleId`].
const STYLE_LIMIT: u32 = StyleId::MAX.get();

/// Clusters a pane can hold: one per [`GraphemeId`].
const GRAPHEME_LIMIT: u32 = GraphemeId::MAX + 1;

/// The most entries a table holds: one per cell of the largest grid, plus a
/// row being written (interned before the row it replaces is released).
const MOST_LIVE: usize = GridSize::MAX_CELLS + widen16(GridSize::MAX_COLS);

const _: () = assert!(
    MOST_LIVE < widen(STYLE_LIMIT) && MOST_LIVE < widen(GRAPHEME_LIMIT),
    "a screen's tables can never fill"
);

/// A table entry's bookkeeping.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Slot {
    /// How many cells use the entry.
    users: u32,
    /// The version that last (re)defined the entry.
    defined: Version,
}

/// The styles and grapheme clusters a pane's cells refer to.
#[derive(Debug)]
pub struct Tables {
    /// Every live style but the default, which is implicit as id 0; so the
    /// style at interner index `i` has [`StyleId`] `i + 1`.
    styles: Interner<Style, STYLE_LIMIT>,
    /// By style index.
    style_slots: Vec<Slot>,
    graphemes: SliceInterner<char, GRAPHEME_LIMIT>,
    /// By grapheme id.
    grapheme_slots: Vec<Slot>,
}

/// The default style, lent for [`StyleId::DEFAULT`].
static DEFAULT_STYLE: Style = Style::DEFAULT;

impl Tables {
    /// Empty tables; [`StyleId::DEFAULT`] already means [`Style::DEFAULT`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            styles: Interner::new(),
            style_slots: Vec::new(),
            graphemes: SliceInterner::new(),
            grapheme_slots: Vec::new(),
        }
    }

    /// Forgets every entry, for a screen whose every cell was blanked.
    pub(crate) fn clear(&mut self) {
        *self = Self::new();
    }

    /// The id of `style` for a cell about to be written at `version`. The
    /// entry stays only if the cell is: the screen counts its uses.
    pub(crate) fn intern_style(&mut self, style: Style, version: Version) -> StyleId {
        if style == Style::DEFAULT {
            return StyleId::DEFAULT;
        }
        let (index, interned) = self
            .styles
            .intern(style)
            .or_bug("a screen's styles fit their ids: at most one per cell");
        if interned == Interned::New {
            define(&mut self.style_slots, index, version);
        }
        StyleId::new(index + 1).or_bug("style indices are below the id limit")
    }

    /// The id of a multi-codepoint cluster for a cell about to be written at
    /// `version`, as [`Tables::intern_style`].
    pub(crate) fn intern_grapheme(&mut self, codepoints: &[char], version: Version) -> GraphemeId {
        let (id, interned) = self
            .graphemes
            .intern(codepoints)
            .or_bug("a screen's clusters fit their ids: at most one per cell");
        if interned == Interned::New {
            define(&mut self.grapheme_slots, id, version);
        }
        GraphemeId::new(id).or_bug("cluster ids are below the id limit")
    }

    /// Counts a cell's uses of its entries.
    pub(crate) fn acquire(&mut self, cell: Cell) {
        if let Some(index) = style_index_or_none(cell.style) {
            slot_mut(&mut self.style_slots, index).users += 1;
        }
        if let GlyphKind::Cluster(id) = cell.glyph.kind() {
            slot_mut(&mut self.grapheme_slots, id.get()).users += 1;
        }
    }

    /// Uncounts a cell's uses, freeing entries no cell uses any more.
    pub(crate) fn release(&mut self, cell: Cell) {
        if let Some(index) = style_index_or_none(cell.style)
            && unuse(slot_mut(&mut self.style_slots, index))
        {
            self.styles.remove(index);
        }
        if let GlyphKind::Cluster(id) = cell.glyph.kind()
            && unuse(slot_mut(&mut self.grapheme_slots, id.get()))
        {
            self.graphemes.remove(id.get());
        }
    }

    /// The style a cell of this screen refers to.
    #[must_use]
    #[track_caller]
    #[expect(
        clippy::option_if_let_else,
        reason = "map_or's closure would lose the caller's location"
    )]
    pub fn style(&self, id: StyleId) -> &Style {
        match style_index_or_none(id) {
            None => &DEFAULT_STYLE,
            Some(index) => self
                .styles
                .get_or_none(index)
                .or_bug("a cell's style is live in its screen's tables"),
        }
    }

    /// The cluster a cell of this screen refers to.
    #[must_use]
    #[track_caller]
    pub fn grapheme(&self, id: GraphemeId) -> &[char] {
        self.graphemes
            .get_or_none(id.get())
            .or_bug("a cell's cluster is live in its screen's tables")
    }

    /// How many styles are live, the default included.
    #[must_use]
    pub const fn style_count(&self) -> usize {
        self.styles.len() + 1
    }

    /// How many clusters are live.
    #[must_use]
    pub const fn grapheme_count(&self) -> usize {
        self.graphemes.len()
    }

    /// Characters the cluster arena holds, live and freed: what clusters
    /// cost. Never more than twice the live clusters' characters.
    #[must_use]
    pub const fn grapheme_arena_len(&self) -> usize {
        self.graphemes.arena_len()
    }

    /// The live styles (re)defined after `acked`: what a client that has
    /// seen version `acked` is missing.
    pub fn styles_changed_since(
        &self,
        acked: Version,
    ) -> impl Iterator<Item = (StyleId, &Style)> + '_ {
        changed_since(&self.style_slots, acked).filter_map(move |index| {
            let style = self.styles.get_or_none(index)?;
            Some((
                StyleId::new(index + 1).or_bug("style indices are below the id limit"),
                style,
            ))
        })
    }

    /// The live clusters (re)defined after `acked`, as
    /// [`Tables::styles_changed_since`].
    pub fn graphemes_changed_since(
        &self,
        acked: Version,
    ) -> impl Iterator<Item = (GraphemeId, &[char])> + '_ {
        changed_since(&self.grapheme_slots, acked).filter_map(move |id| {
            let codepoints = self.graphemes.get_or_none(id)?;
            Some((
                GraphemeId::new(id).or_bug("cluster ids are below the id limit"),
                codepoints,
            ))
        })
    }
}

impl Default for Tables {
    fn default() -> Self {
        Self::new()
    }
}

/// The interner index of a non-default style.
const fn style_index_or_none(id: StyleId) -> Option<u32> {
    id.get().checked_sub(1)
}

/// Records that `index` was (re)defined at `version`, with no users yet.
fn define(slots: &mut Vec<Slot>, index: u32, version: Version) {
    let index = widen(index);
    if slots.len() <= index {
        slots.resize(index + 1, Slot::default());
    }
    *slots
        .get_mut(index)
        .or_bug("slots were just grown past the index") = Slot {
        users: 0,
        defined: version,
    };
}

#[track_caller]
fn slot_mut(slots: &mut [Slot], index: u32) -> &mut Slot {
    slots
        .get_mut(widen(index))
        .or_bug("a cell's entry has a slot")
}

/// Drops one use; whether that was the last.
#[track_caller]
fn unuse(slot: &mut Slot) -> bool {
    invariant!(
        slot.users > 0,
        "an entry is released no more often than acquired"
    );
    slot.users -= 1;
    slot.users == 0
}

/// Indices of slots defined after `acked`.
fn changed_since(slots: &[Slot], acked: Version) -> impl Iterator<Item = u32> + '_ {
    slots
        .iter()
        .zip(0_u32..)
        .filter(move |(slot, _)| slot.defined > acked)
        .map(|(_, index)| index)
}

/// `u32` to `usize`.
#[expect(
    clippy::as_conversions,
    reason = "usize holds every u32 on the 64-bit targets plx builds for"
)]
const fn widen(value: u32) -> usize {
    value as usize
}

/// `u16` to `usize`.
#[expect(clippy::as_conversions, reason = "lossless; usize::from is not const")]
const fn widen16(value: u16) -> usize {
    value as usize
}
