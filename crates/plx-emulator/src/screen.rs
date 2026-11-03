//! A pane's screen: its [`Grid`] and the [`Tables`] its cells refer to, owned
//! together so that every cell's style and cluster is always live in its own
//! tables. Rows change only through a [`RowWriter`],
//! which interns as the row is built and counts uses when it is committed.

use alloc::vec::Vec;

use plx_bug::{OrBug as _, invariant};

use crate::cell::{Cell, Glyph, GlyphKind, Width};
use crate::grid::{CellText, Cursor, Grid, GridSize, ResolvedCell, Version};
use crate::port::Change;
use crate::style::Style;
use crate::tables::Tables;

/// What a cell shows, before interning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Text<'a> {
    Empty,
    Char(char),
    /// Several codepoints that form one grapheme cluster.
    Cluster(&'a [char]),
}

/// A pane's visible screen.
#[derive(Debug)]
pub struct Screen {
    grid: Grid,
    tables: Tables,
    /// The row a [`RowWriter`] is building, reused.
    row: Vec<Cell>,
}

impl Screen {
    /// A blank screen.
    #[must_use]
    pub fn new(size: GridSize) -> Self {
        Self {
            grid: Grid::new(size),
            tables: Tables::new(),
            row: Vec::new(),
        }
    }

    /// The cells, row versions and cursor.
    #[must_use]
    pub const fn grid(&self) -> &Grid {
        &self.grid
    }

    /// What the cells refer to.
    #[must_use]
    pub const fn tables(&self) -> &Tables {
        &self.tables
    }

    #[must_use]
    pub const fn size(&self) -> GridSize {
        self.grid.size()
    }

    /// Reallocates for `size`, blank (no entries in use), with every row and
    /// the cursor at `version`: their content is about to be rewritten.
    pub fn resize(&mut self, size: GridSize, version: Version) {
        self.grid.resize(size, version);
        self.tables.clear();
    }

    /// Sets the cursor if it differs, stamping it with `version`.
    pub fn set_cursor(&mut self, cursor: Cursor, version: Version) -> Change {
        self.grid.set_cursor(cursor, version)
    }

    /// Starts rewriting row `y` (which must exist) at `version`.
    #[track_caller]
    pub fn write_row(&mut self, y: u16, version: Version) -> RowWriter<'_> {
        invariant!(
            y < self.grid.size().rows().get(),
            "a row written to a screen lies within it: row {} of {}",
            y,
            self.grid.size().rows(),
        );
        self.row.clear();
        RowWriter {
            screen: self,
            y,
            version,
        }
    }

    /// A cell of this screen with its style and text looked up.
    #[must_use]
    #[track_caller]
    pub fn resolve(&self, cell: Cell) -> ResolvedCell<'_> {
        let text = match cell.glyph.kind() {
            GlyphKind::Empty => CellText::Empty,
            GlyphKind::Char(c) => CellText::Char(c),
            GlyphKind::Cluster(id) => CellText::Cluster(self.tables.grapheme(id)),
        };
        ResolvedCell {
            text,
            width: cell.width,
            style: self.tables.style(cell.style),
        }
    }
}

/// Builds one row of a [`Screen`], cell by cell, then commits it.
#[derive(Debug)]
#[must_use = "a row is written only when finished"]
pub struct RowWriter<'a> {
    screen: &'a mut Screen,
    y: u16,
    version: Version,
}

impl RowWriter<'_> {
    /// Appends a cell.
    ///
    /// A control character is never a cell's text: terminals execute or
    /// drop them, and a cell holding one would reach clients' terminals as
    /// a command. Such a cell (or a cluster with one in it) is empty,
    /// keeping its column and style. libghostty 1.4 stores DEL (0x7F) as a
    /// glyph (found by the simulation, verified against libghostty).
    pub fn push(&mut self, text: Text<'_>, width: Width, style: Style) {
        let tables = &mut self.screen.tables;
        let glyph = match text {
            Text::Empty => Glyph::EMPTY,
            Text::Char(c) if c.is_control() => Glyph::EMPTY,
            Text::Char(c) => Glyph::from_char(c),
            Text::Cluster(codepoints) if codepoints.iter().copied().any(char::is_control) => {
                Glyph::EMPTY
            }
            Text::Cluster(codepoints) => {
                Glyph::from_cluster(tables.intern_grapheme(codepoints, self.version))
            }
        };
        let style = tables.intern_style(style, self.version);
        self.screen.row.push(Cell {
            glyph,
            style,
            width,
        });
    }

    /// Replaces the row with the cells pushed, if they differ, stamping it
    /// with the writer's version. The row must be complete.
    #[must_use = "a changed row is damage to report"]
    #[track_caller]
    pub fn finish(self) -> Change {
        let Screen { grid, tables, row } = self.screen;
        let cols = grid.size().cols();
        invariant!(
            row.len() == usize::from(cols.get()),
            "a row written to a screen has as many cells as it has columns: {} of {}",
            row.len(),
            cols,
        );
        let (cells, stamp) = grid
            .row_mut_or_none(self.y)
            .or_bug("the writer's row was checked to exist");
        if cells == row.as_slice() {
            return Change::Unchanged;
        }
        // New uses first: a cell may keep an entry the old row also used.
        for &cell in row.iter() {
            tables.acquire(cell);
        }
        for &cell in cells.iter() {
            tables.release(cell);
        }
        cells.copy_from_slice(row);
        *stamp = self.version;
        Change::Changed
    }
}
