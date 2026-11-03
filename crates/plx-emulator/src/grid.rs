//! A pane's visible screen: one flat allocation of compact cells, with a
//! version per row so frames send only what a client has not seen.

use alloc::boxed::Box;
use alloc::vec;
use core::num::NonZeroU16;

use plx_bug::invariant;
use serde::{Deserialize, Serialize};

use crate::cell::{Cell, Width};
use crate::port::Change;
use crate::style::Style;

/// A terminal grid size in cells: at least 1×1 and at most
/// [`GridSize::MAX_COLS`] × [`GridSize::MAX_ROWS`].
///
/// The bound keeps every grid, and every table of what its cells refer to,
/// a bounded size: a client cannot make the daemon allocate gigabytes by
/// claiming a huge terminal. It holds per dimension, so the smaller or larger
/// of two sizes, and any part of one, is in bounds too. [`GridSize::new`] is
/// the parse boundary for sizes from clients and snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawGridSize")]
pub struct GridSize {
    cols: NonZeroU16,
    rows: NonZeroU16,
}

/// A size as it arrives from outside, before its bound is checked.
#[derive(Deserialize)]
struct RawGridSize {
    cols: NonZeroU16,
    rows: NonZeroU16,
}

/// A size past [`GridSize::MAX_COLS`] × [`GridSize::MAX_ROWS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "a {cols}x{rows} grid is larger than the {}x{} plx supports",
    GridSize::MAX_COLS,
    GridSize::MAX_ROWS
)]
pub struct GridTooLarge {
    pub cols: NonZeroU16,
    pub rows: NonZeroU16,
}

impl GridSize {
    /// The widest grid: wider than any real terminal.
    pub const MAX_COLS: u16 = 4096;
    /// The tallest grid: taller than any real terminal.
    pub const MAX_ROWS: u16 = 2048;
    /// The most cells a grid has: `MAX_COLS × MAX_ROWS`, 2^23.
    pub const MAX_CELLS: usize = 1 << 23;

    /// # Errors
    ///
    /// When either dimension is past its maximum.
    pub const fn new(cols: NonZeroU16, rows: NonZeroU16) -> Result<Self, GridTooLarge> {
        if cols.get() <= Self::MAX_COLS && rows.get() <= Self::MAX_ROWS {
            Ok(Self { cols, rows })
        } else {
            Err(GridTooLarge { cols, rows })
        }
    }

    #[must_use]
    pub const fn cols(self) -> NonZeroU16 {
        self.cols
    }

    #[must_use]
    pub const fn rows(self) -> NonZeroU16 {
        self.rows
    }

    /// Number of cells, at most [`GridSize::MAX_CELLS`].
    #[must_use]
    pub fn area(self) -> usize {
        usize::from(self.cols.get()) * usize::from(self.rows.get())
    }

    /// A size no larger than this one in either dimension, such as a part of
    /// its area.
    #[must_use]
    #[track_caller]
    pub fn within(self, cols: NonZeroU16, rows: NonZeroU16) -> Self {
        invariant!(
            cols <= self.cols && rows <= self.rows,
            "a part of a grid is no larger than the grid: {}x{} in {}x{}",
            cols,
            rows,
            self.cols,
            self.rows,
        );
        Self { cols, rows }
    }

    /// The smaller of each dimension.
    #[must_use]
    pub fn smallest(self, other: Self) -> Self {
        Self {
            cols: self.cols.min(other.cols),
            rows: self.rows.min(other.rows),
        }
    }

    /// The larger of each dimension; in bounds because both sizes are.
    #[must_use]
    pub fn largest(self, other: Self) -> Self {
        Self {
            cols: self.cols.max(other.cols),
            rows: self.rows.max(other.rows),
        }
    }
}

impl TryFrom<RawGridSize> for GridSize {
    type Error = GridTooLarge;

    fn try_from(raw: RawGridSize) -> Result<Self, GridTooLarge> {
        Self::new(raw.cols, raw.rows)
    }
}

/// The shape the cursor is drawn with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CursorShape {
    /// The terminal's own cursor, as its user configured it: the program
    /// asked for no shape, or reset it (`CSI 0 q`).
    Default,
    Block,
    Underline,
    Bar,
}

/// A cell position in the visible screen, zero-based from the top left.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Position {
    pub x: u16,
    pub y: u16,
}

/// Whether the program wants the cursor shown (DECTCEM).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CursorVisibility {
    Visible,
    Hidden,
}

/// Whether the cursor blinks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CursorBlink {
    Steady,
    Blinking,
}

/// The cursor as the program left it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Cursor {
    /// Where the cursor sits, or `None` when it is outside the visible
    /// screen (for example while the viewport is scrolled back).
    pub position: Option<Position>,
    pub visibility: CursorVisibility,
    pub blink: CursorBlink,
    pub shape: CursorShape,
}

impl Cursor {
    /// A fresh terminal's cursor.
    pub const INITIAL: Self = Self {
        position: Some(Position { x: 0, y: 0 }),
        visibility: CursorVisibility::Visible,
        blink: CursorBlink::Steady,
        shape: CursorShape::Default,
    };
}

/// When a row or the cursor last changed: the version of the sync that
/// changed it. Clients acknowledge versions, so a frame carries only what
/// changed since. Versions only grow; the core mints them.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Version(u64);

impl Version {
    /// A fresh grid's version: before every sync.
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The version after this one.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// The visible screen's cells, row versions and cursor. Part of a
/// [`crate::Screen`], which alone changes it; everyone else borrows it.
#[derive(Debug, PartialEq, Eq)]
pub struct Grid {
    size: GridSize,
    /// `rows × cols` cells, row-major.
    cells: Box<[Cell]>,
    /// The version that last changed each row.
    versions: Box<[Version]>,
    cursor: Cursor,
    cursor_version: Version,
}

impl Grid {
    /// A blank grid; every row has version 0.
    #[must_use]
    pub fn new(size: GridSize) -> Self {
        Self {
            size,
            cells: vec![Cell::BLANK; size.area()].into_boxed_slice(),
            versions: vec![Version::ZERO; usize::from(size.rows().get())].into_boxed_slice(),
            cursor: Cursor::INITIAL,
            cursor_version: Version::ZERO,
        }
    }

    #[must_use]
    pub const fn size(&self) -> GridSize {
        self.size
    }

    /// Reallocates for `size`, blank, with every row and the cursor at
    /// `version` (their content is about to be rewritten).
    pub(crate) fn resize(&mut self, size: GridSize, version: Version) {
        self.size = size;
        self.cells = vec![Cell::BLANK; size.area()].into_boxed_slice();
        self.versions = vec![version; usize::from(size.rows().get())].into_boxed_slice();
        self.cursor_version = version;
    }

    fn cols(&self) -> usize {
        usize::from(self.size.cols().get())
    }

    /// Row `y`; `None` past the last row.
    #[must_use]
    pub fn row_or_none(&self, y: u16) -> Option<&[Cell]> {
        let start = usize::from(y) * self.cols();
        self.cells.get(start..start + self.cols())
    }

    /// Every row, top to bottom.
    pub fn rows(&self) -> impl Iterator<Item = &[Cell]> + '_ {
        self.cells.chunks_exact(self.cols())
    }

    /// Row `y`'s version; `None` past the last row.
    #[must_use]
    pub fn row_version_or_none(&self, y: u16) -> Option<Version> {
        self.versions.get(usize::from(y)).copied()
    }

    /// The rows changed after `acked`, with their index: what a client that
    /// has seen version `acked` is missing.
    pub fn rows_newer_than(&self, acked: Version) -> impl Iterator<Item = (u16, &[Cell])> + '_ {
        self.rows()
            .zip(self.versions.iter())
            .zip(0_u16..)
            .filter(move |((_, version), _)| **version > acked)
            .map(|((row, _), y)| (y, row))
    }

    #[must_use]
    pub const fn cursor(&self) -> Cursor {
        self.cursor
    }

    #[must_use]
    pub const fn cursor_version(&self) -> Version {
        self.cursor_version
    }

    /// Row `y`'s cells and version, for rewriting it; `None` past the last
    /// row.
    pub(crate) fn row_mut_or_none(&mut self, y: u16) -> Option<(&mut [Cell], &mut Version)> {
        let cols = self.cols();
        let start = usize::from(y) * cols;
        let cells = self.cells.get_mut(start..start + cols)?;
        let version = self.versions.get_mut(usize::from(y))?;
        Some((cells, version))
    }

    /// Sets the cursor if it differs, stamping it with `version`.
    pub(crate) fn set_cursor(&mut self, cursor: Cursor, version: Version) -> Change {
        if self.cursor == cursor {
            return Change::Unchanged;
        }
        self.cursor = cursor;
        self.cursor_version = version;
        Change::Changed
    }
}

/// A cell with its style and text looked up: what it looks like, for
/// renderers and for comparing grids whose tables were filled in different
/// orders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedCell<'a> {
    pub text: CellText<'a>,
    pub width: Width,
    pub style: &'a Style,
}

/// A cell's text, borrowed from the tables for clusters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellText<'a> {
    Empty,
    Char(char),
    Cluster(&'a [char]),
}
