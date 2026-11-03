//! Syncs libghostty-vt's render state into the pane's [`Grid`]: only rows
//! libghostty reports dirty, each compacted to 8-byte cells with styles and
//! clusters interned, and written only if its content changed.
//!
//! Every value read here comes from a live terminal (or one restored from a
//! snapshot, whose decoder validates scalars and styles), so anything outside
//! the header's documented ranges breaks libghostty's contract.

use core::ffi::c_void;
use core::ptr::{self, NonNull};

use plx_bug::{OrBug as _, breach, contract};
use plx_emulator::{
    Attrs, Change, Color, Cursor, CursorBlink, CursorShape, CursorVisibility, Damage, GridSize,
    Position, Rgb, RowWriter, Screen, Style, Text, Underline, Version, Width,
};
use plx_ghostty_sys as sys;

use crate::error::{Error, adopt, check, check_contract, widen};
use crate::search::{Mark, MarkKind};

/// Search matches, in the client's cendre colours: dark text on its dim
/// foreground, the selected match on ember.
const MATCH_COLORS: (Color, Color) = (Color::Rgb(CENDRE_BG), Color::Rgb(CENDRE_FG_DIM));
const SELECTED_MATCH_COLORS: (Color, Color) = (Color::Rgb(CENDRE_BG), Color::Rgb(CENDRE_EMBER));
const CENDRE_BG: Rgb = Rgb {
    r: 0x20,
    g: 0x1b,
    b: 0x19,
};
const CENDRE_FG_DIM: Rgb = Rgb {
    r: 0xa0,
    g: 0x93,
    b: 0x84,
};
const CENDRE_EMBER: Rgb = Rgb {
    r: 0xea,
    g: 0x98,
    b: 0x75,
};

/// How much of the screen a sync reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyncScope {
    /// Only the rows libghostty reports dirty.
    Dirty,
    /// Every row: after creation, restore or resize.
    Full,
}

/// A render state, its row and cell iterators, and scratch buffers. One per
/// emulator, reused for every sync.
pub(crate) struct GridSync {
    state: RenderState,
    rows: RowIterator,
    cells: RowCells,
    /// A cluster's codepoints as libghostty reports them, then as `char`s.
    codepoints: Vec<u32>,
    chars: Vec<char>,
    /// The rows a dirty sync read, and rows above them to read as well.
    read: Vec<u16>,
    above: Vec<u16>,
}

impl GridSync {
    /// # Errors
    ///
    /// When libghostty-vt cannot create a render state or its iterators.
    pub(crate) fn new() -> Result<Self, Error> {
        // Each handle frees itself on drop, so an error on a later one cannot
        // leak an earlier one.
        Ok(Self {
            state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: RowCells::new()?,
            codepoints: Vec::new(),
            chars: Vec::new(),
            read: Vec::new(),
            above: Vec::new(),
        })
    }

    /// Brings `screen` up to date with `terminal`, reading the rows `scope`
    /// asks for (and every row if the size or libghostty says so), with the
    /// search matches `marks` (row by row, top to bottom) drawn in.
    ///
    /// # Errors
    ///
    /// When libghostty-vt cannot update its render state (an internal limit).
    pub(crate) fn sync(
        &mut self,
        terminal: NonNull<sys::GhosttyTerminalImpl>,
        screen: &mut Screen,
        scope: SyncScope,
        version: Version,
        marks: &[Mark],
    ) -> Result<Damage, Error> {
        // SAFETY: both handles are live; no other call on them is in progress.
        check("ghostty_render_state_update", unsafe {
            sys::ghostty_render_state_update(self.state.0.as_ptr(), terminal.as_ptr())
        })?;
        let cols = self.get_u16(sys::GhosttyRenderStateData::GHOSTTY_RENDER_STATE_DATA_COLS);
        let rows = self.get_u16(sys::GhosttyRenderStateData::GHOSTTY_RENDER_STATE_DATA_ROWS);
        let size = GridSize::new(
            cols.try_into().or_contract("a render state has columns"),
            rows.try_into().or_contract("a render state has rows"),
        )
        .or_bug("a terminal has the size plx created, resized or restored it with");

        let size_change = if screen.size() == size {
            Change::Unchanged
        } else {
            screen.resize(size, version);
            Change::Changed
        };
        let mut damage = Damage {
            size: size_change,
            ..Damage::default()
        };
        let mut scope = if size_change == Change::Changed {
            SyncScope::Full
        } else {
            scope
        };
        // Initial value of the out-parameter only; the query overwrites it.
        let mut dirty = sys::GhosttyRenderStateDirty::GHOSTTY_RENDER_STATE_DIRTY_FULL;
        self.get(
            "ghostty_render_state_get(DIRTY)",
            sys::GhosttyRenderStateData::GHOSTTY_RENDER_STATE_DATA_DIRTY,
            (&raw mut dirty).cast(),
        );
        if dirty == sys::GhosttyRenderStateDirty::GHOSTTY_RENDER_STATE_DIRTY_FULL {
            scope = SyncScope::Full;
        }

        if scope == SyncScope::Full
            || dirty != sys::GhosttyRenderStateDirty::GHOSTTY_RENDER_STATE_DIRTY_FALSE
        {
            let rows_out = self.rows.out();
            self.get(
                "ghostty_render_state_get(ROW_ITERATOR)",
                sys::GhosttyRenderStateData::GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                rows_out,
            );
            self.read.clear();
            let mut y = 0_u16;
            while self.next_row(scope, &mut y) {
                self.read_into(screen, y, version, &mut damage, marks);
                self.read.push(y);
                if scope == SyncScope::Full {
                    y += 1;
                }
            }
            contract!(
                scope == SyncScope::Dirty || self.read.len() == usize::from(rows),
                "a full render state yields every row: {} of {}",
                self.read.len(),
                rows,
            );
            if scope == SyncScope::Dirty {
                self.reread_spacers_above(terminal, screen, version, &mut damage, marks)?;
            }
        }

        damage.cursor = screen.set_cursor(self.read_cursor(), version);
        // SAFETY: the render state is live.
        check_contract("ghostty_render_state_clean", unsafe {
            sys::ghostty_render_state_clean(self.state.0.as_ptr())
        });
        Ok(damage)
    }

    /// Advances to the next row to read: every row for a full sync (the
    /// caller counts `y`), otherwise the next dirty row (whose `y` libghostty
    /// reports).
    /// Reads row `y` into the screen, counting it in `damage` if changed.
    fn read_into(
        &mut self,
        screen: &mut Screen,
        y: u16,
        version: Version,
        damage: &mut Damage,
        marks: &[Mark],
    ) {
        // The marks are ordered by row.
        let first = marks.partition_point(|mark| mark.y < y);
        let end = marks.partition_point(|mark| mark.y <= y);
        let in_row = marks
            .get(first..end)
            .or_bug("partition points of one ordering are in order");
        let mut row = screen.write_row(y, version);
        self.read_row(&mut row, in_row);
        if row.finish() == Change::Changed {
            damage.rows += 1;
        }
    }

    /// After a dirty sync: rereads each clean row above a dirty one whose
    /// last cell still holds a wide character's spacer. Overwriting a wide
    /// character that wrapped to the next row clears that spacer too, and
    /// libghostty 1.4 marks neither the row above dirty nor refreshes the
    /// render state's copy of it. A fresh render state copies every row on
    /// its first update, so those rows are read from one. This allocates,
    /// but only when a wrapped wide character was overwritten.
    ///
    /// # Errors
    ///
    /// When libghostty-vt cannot create or update the render state.
    fn reread_spacers_above(
        &mut self,
        terminal: NonNull<sys::GhosttyTerminalImpl>,
        screen: &mut Screen,
        version: Version,
        damage: &mut Damage,
        marks: &[Mark],
    ) -> Result<(), Error> {
        let mut above = core::mem::take(&mut self.above);
        above.clear();
        for &y in &self.read {
            if let Some(up) = y.checked_sub(1)
                && !self.read.contains(&up)
                && screen
                    .grid()
                    .row_or_none(up)
                    .and_then(<[_]>::last)
                    .is_some_and(|cell| cell.width == Width::SpacerHead)
            {
                above.push(up);
            }
        }
        if !above.is_empty() {
            self.state = RenderState::new()?;
            // SAFETY: both handles are live; no other call on them is in
            // progress.
            check("ghostty_render_state_update", unsafe {
                sys::ghostty_render_state_update(self.state.0.as_ptr(), terminal.as_ptr())
            })?;
            let rows_out = self.rows.out();
            self.get(
                "ghostty_render_state_get(ROW_ITERATOR)",
                sys::GhosttyRenderStateData::GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                rows_out,
            );
            let mut y = 0_u16;
            while self.next_row(SyncScope::Full, &mut y) {
                if above.contains(&y) {
                    self.read_into(screen, y, version, damage, marks);
                }
                y += 1;
            }
        }
        self.above = above;
        Ok(())
    }

    fn next_row(&mut self, scope: SyncScope, y: &mut u16) -> bool {
        if scope == SyncScope::Full {
            // SAFETY: the row iterator was filled from the live render state.
            unsafe { sys::ghostty_render_state_row_iterator_next(self.rows.0.as_ptr()) }
        } else {
            // SAFETY: as above; `y` is a valid out-pointer.
            unsafe { sys::ghostty_render_state_row_iterator_next_dirty(self.rows.0.as_ptr(), y) }
        }
    }

    /// Reads the iterator's current row into `row`, with the search matches
    /// `marks` in it.
    fn read_row(&mut self, row: &mut RowWriter<'_>, marks: &[Mark]) {
        // SAFETY: the row iterator is positioned on a row; `cells` is a live
        // handle that the call refills in place.
        check_contract("ghostty_render_state_row_get(CELLS)", unsafe {
            sys::ghostty_render_state_row_get(
                self.rows.0.as_ptr(),
                sys::GhosttyRenderStateRowData::GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                self.cells.out(),
            )
        });
        // The row's selected columns, drawn in inverse video.
        let mut selection = sys::GhosttyRenderStateRowSelection {
            size: size_of::<sys::GhosttyRenderStateRowSelection>(),
            start_x: 0,
            end_x: 0,
        };
        // SAFETY: the row iterator is positioned on a row; `DATA_SELECTION`
        // documents a sized `GhosttyRenderStateRowSelection` output.
        let result = unsafe {
            sys::ghostty_render_state_row_get(
                self.rows.0.as_ptr(),
                sys::GhosttyRenderStateRowData::GHOSTTY_RENDER_STATE_ROW_DATA_SELECTION,
                (&raw mut selection).cast(),
            )
        };
        let selected = if result == sys::GhosttyResult::GHOSTTY_SUCCESS {
            Some(selection.start_x..=selection.end_x)
        } else {
            // The row does not intersect the selection.
            check_contract(
                "ghostty_render_state_row_get(SELECTION)",
                match result {
                    sys::GhosttyResult::GHOSTTY_NO_VALUE => sys::GhosttyResult::GHOSTTY_SUCCESS,
                    other => other,
                },
            );
            None
        };
        let mut x = 0_u16;
        // SAFETY: `cells` was just filled for the current row.
        while unsafe { sys::ghostty_render_state_row_cells_next(self.cells.0.as_ptr()) } {
            let in_selection = match &selected {
                Some(columns) if columns.contains(&x) => Selected::Yes,
                Some(_) | None => Selected::No,
            };
            let mark = marks
                .iter()
                .find(|mark| (mark.start..=mark.end).contains(&x))
                .map(|mark| mark.kind);
            self.read_cell(row, in_selection, mark);
            x = x.saturating_add(1);
        }
    }

    /// Reads the iterator's current cell into `row`.
    fn read_cell(&mut self, row: &mut RowWriter<'_>, selected: Selected, mark: Option<MarkKind>) {
        let mut raw: sys::GhosttyCell = 0;
        let mut grapheme_len: u32 = 0;
        let keys = [
            sys::GhosttyRenderStateRowCellsData::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW,
            sys::GhosttyRenderStateRowCellsData::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN,
        ];
        let mut values: [*mut c_void; 2] = [(&raw mut raw).cast(), (&raw mut grapheme_len).cast()];
        // SAFETY: the cells iterator is positioned on a cell; each value
        // points to the type documented for its key.
        check_contract(
            "ghostty_render_state_row_cells_get_multi(RAW, GRAPHEMES_LEN)",
            unsafe {
                sys::ghostty_render_state_row_cells_get_multi(
                    self.cells.0.as_ptr(),
                    keys.len(),
                    keys.as_ptr(),
                    values.as_mut_ptr(),
                    ptr::null_mut(),
                )
            },
        );

        let mut codepoint: u32 = 0;
        // Initial value of the out-parameter only; the query overwrites it.
        let mut wide = sys::GhosttyCellWide::GHOSTTY_CELL_WIDE_NARROW;
        let mut styled = false;
        let mut content = sys::GhosttyCellContentTag::GHOSTTY_CELL_CONTENT_CODEPOINT;
        let keys = [
            sys::GhosttyCellData::GHOSTTY_CELL_DATA_CODEPOINT,
            sys::GhosttyCellData::GHOSTTY_CELL_DATA_WIDE,
            sys::GhosttyCellData::GHOSTTY_CELL_DATA_HAS_STYLING,
            sys::GhosttyCellData::GHOSTTY_CELL_DATA_CONTENT_TAG,
        ];
        let mut values: [*mut c_void; 4] = [
            (&raw mut codepoint).cast(),
            (&raw mut wide).cast(),
            (&raw mut styled).cast(),
            (&raw mut content).cast(),
        ];
        // SAFETY: `raw` is a cell value; each value points to the type
        // documented for its key.
        check_contract(
            "ghostty_cell_get_multi(CODEPOINT, WIDE, HAS_STYLING, CONTENT_TAG)",
            unsafe {
                sys::ghostty_cell_get_multi(
                    raw,
                    keys.len(),
                    keys.as_ptr(),
                    values.as_mut_ptr(),
                    ptr::null_mut(),
                )
            },
        );

        let mut style = if styled {
            self.read_style()
        } else {
            Style::DEFAULT
        };
        if let Some(bg) = content_background_or_none(raw, content) {
            style.bg = bg;
        }
        if let Some(kind) = mark {
            (style.fg, style.bg) = match kind {
                MarkKind::Match => MATCH_COLORS,
                MarkKind::Selected => SELECTED_MATCH_COLORS,
            };
        }
        if selected == Selected::Yes {
            style.attrs = style.attrs.toggled(Attrs::INVERSE);
        }
        let width = convert_width(wide);
        match grapheme_len {
            0 => row.push(Text::Empty, width, style),
            1 => row.push(Text::Char(scalar(codepoint)), width, style),
            _ => {
                self.read_cluster(grapheme_len);
                row.push(Text::Cluster(&self.chars), width, style);
            }
        }
    }

    /// Reads the current cell's cluster into `self.chars`.
    fn read_cluster(&mut self, len: u32) {
        self.codepoints.clear();
        self.codepoints.resize(widen(len), 0);
        // The buffer holds exactly `len` entries, as the call requires.
        let buffer = self.codepoints.as_mut_ptr().cast();
        self.cell_get(
            "ghostty_render_state_row_cells_get(GRAPHEMES_BUF)",
            sys::GhosttyRenderStateRowCellsData::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
            buffer,
        );
        self.chars.clear();
        self.chars
            .extend(self.codepoints.iter().map(|&codepoint| scalar(codepoint)));
    }

    fn read_style(&self) -> Style {
        let mut style = sys::GhosttyStyle {
            size: size_of::<sys::GhosttyStyle>(),
            ..sys::GhosttyStyle::default()
        };
        self.cell_get(
            "ghostty_render_state_row_cells_get(STYLE)",
            sys::GhosttyRenderStateRowCellsData::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
            (&raw mut style).cast(),
        );
        convert_style(&style)
    }

    fn read_cursor(&self) -> Cursor {
        let mut cursor = sys::GhosttyRenderStateCursor {
            size: size_of::<sys::GhosttyRenderStateCursor>(),
            ..sys::GhosttyRenderStateCursor::default()
        };
        self.get(
            "ghostty_render_state_get(CURSOR)",
            sys::GhosttyRenderStateData::GHOSTTY_RENDER_STATE_DATA_CURSOR,
            (&raw mut cursor).cast(),
        );
        let shape = match cursor.visual_style {
            sys::GhosttyRenderStateCursorVisualStyle::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK => {
                CursorShape::Block
            }
            // The default style every terminal is given (no escape sequence
            // selects a hollow block): the program asked for no shape.
            sys::GhosttyRenderStateCursorVisualStyle::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW => {
                CursorShape::Default
            }
            sys::GhosttyRenderStateCursorVisualStyle::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE => {
                CursorShape::Underline
            }
            sys::GhosttyRenderStateCursorVisualStyle::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR => CursorShape::Bar,
            other => breach!("the header lists every cursor style, not {}", other.0),
        };
        // The position fields are undefined unless `viewport_has_value`.
        let position = cursor.viewport_has_value.then_some(Position {
            x: cursor.viewport_x,
            y: cursor.viewport_y,
        });
        Cursor {
            position,
            visibility: if cursor.visible {
                CursorVisibility::Visible
            } else {
                CursorVisibility::Hidden
            },
            blink: if cursor.blinking {
                CursorBlink::Blinking
            } else {
                CursorBlink::Steady
            },
            shape,
        }
    }

    fn get_u16(&self, data: sys::GhosttyRenderStateData) -> u16 {
        let mut value: u16 = 0;
        self.get("ghostty_render_state_get", data, (&raw mut value).cast());
        value
    }

    #[track_caller]
    fn get(&self, call: &'static str, data: sys::GhosttyRenderStateData, out: *mut c_void) {
        // SAFETY: the render state is live, and every caller passes an `out`
        // pointer to a value of the type documented for `data`.
        check_contract(call, unsafe {
            sys::ghostty_render_state_get(self.state.0.as_ptr(), data, out)
        });
    }

    #[track_caller]
    fn cell_get(
        &self,
        call: &'static str,
        data: sys::GhosttyRenderStateRowCellsData,
        out: *mut c_void,
    ) {
        // SAFETY: the cells iterator is positioned on a cell, and every caller
        // passes an `out` pointer to a value of the type documented for `data`.
        check_contract(call, unsafe {
            sys::ghostty_render_state_row_cells_get(self.cells.0.as_ptr(), data, out)
        });
    }
}

/// An owned render state.
/// Whether a cell is in the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Selected {
    Yes,
    No,
}

struct RenderState(NonNull<sys::GhosttyRenderStateImpl>);

impl RenderState {
    fn new() -> Result<Self, Error> {
        const CALL: &str = "ghostty_render_state_new";
        let mut raw: sys::GhosttyRenderState = ptr::null_mut();
        // SAFETY: valid out-pointer and allocator.
        check(CALL, unsafe {
            sys::ghostty_render_state_new(crate::alloc::rust(), &raw mut raw)
        })?;
        Ok(Self(adopt(raw, CALL)))
    }
}

impl Drop for RenderState {
    fn drop(&mut self) {
        // SAFETY: owned by this value and freed exactly once.
        unsafe { sys::ghostty_render_state_free(self.0.as_ptr()) };
    }
}

/// An owned row iterator, refilled from the render state on every sync.
struct RowIterator(NonNull<sys::GhosttyRenderStateRowIteratorImpl>);

impl RowIterator {
    fn new() -> Result<Self, Error> {
        const CALL: &str = "ghostty_render_state_row_iterator_new";
        let mut raw: sys::GhosttyRenderStateRowIterator = ptr::null_mut();
        // SAFETY: valid out-pointer and allocator.
        check(CALL, unsafe {
            sys::ghostty_render_state_row_iterator_new(crate::alloc::rust(), &raw mut raw)
        })?;
        Ok(Self(adopt(raw, CALL)))
    }

    /// The out-pointer the C API refills the iterator through: a pointer to
    /// the handle (`NonNull` has the handle's layout).
    const fn out(&mut self) -> *mut c_void {
        (&raw mut self.0).cast()
    }
}

impl Drop for RowIterator {
    fn drop(&mut self) {
        // SAFETY: owned by this value and freed exactly once.
        unsafe { sys::ghostty_render_state_row_iterator_free(self.0.as_ptr()) };
    }
}

/// An owned row-cells iterator, refilled for each row.
struct RowCells(NonNull<sys::GhosttyRenderStateRowCellsImpl>);

impl RowCells {
    fn new() -> Result<Self, Error> {
        const CALL: &str = "ghostty_render_state_row_cells_new";
        let mut raw: sys::GhosttyRenderStateRowCells = ptr::null_mut();
        // SAFETY: valid out-pointer and allocator.
        check(CALL, unsafe {
            sys::ghostty_render_state_row_cells_new(crate::alloc::rust(), &raw mut raw)
        })?;
        Ok(Self(adopt(raw, CALL)))
    }

    /// As [`RowIterator::out`].
    const fn out(&mut self) -> *mut c_void {
        (&raw mut self.0).cast()
    }
}

impl Drop for RowCells {
    fn drop(&mut self) {
        // SAFETY: owned by this value and freed exactly once.
        unsafe { sys::ghostty_render_state_row_cells_free(self.0.as_ptr()) };
    }
}

/// The background colour of a cell that holds only a colour, which is what
/// erasing with a background colour set leaves behind. Such a cell has the
/// default style, so the colour is in its content, not its style. `None` for
/// cells that hold text (or nothing).
fn content_background_or_none(
    cell: sys::GhosttyCell,
    content: sys::GhosttyCellContentTag,
) -> Option<Color> {
    match content {
        sys::GhosttyCellContentTag::GHOSTTY_CELL_CONTENT_CODEPOINT
        | sys::GhosttyCellContentTag::GHOSTTY_CELL_CONTENT_CODEPOINT_GRAPHEME => None,
        sys::GhosttyCellContentTag::GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE => {
            let mut index: sys::GhosttyColorPaletteIndex = 0;
            // SAFETY: the tag says the cell holds a palette background, and
            // `index` has the documented output type.
            check_contract("ghostty_cell_get(COLOR_PALETTE)", unsafe {
                sys::ghostty_cell_get(
                    cell,
                    sys::GhosttyCellData::GHOSTTY_CELL_DATA_COLOR_PALETTE,
                    (&raw mut index).cast(),
                )
            });
            Some(Color::Palette(index))
        }
        sys::GhosttyCellContentTag::GHOSTTY_CELL_CONTENT_BG_COLOR_RGB => {
            let mut rgb = sys::GhosttyColorRgb::default();
            // SAFETY: the tag says the cell holds an RGB background, and
            // `rgb` has the documented output type.
            check_contract("ghostty_cell_get(COLOR_RGB)", unsafe {
                sys::ghostty_cell_get(
                    cell,
                    sys::GhosttyCellData::GHOSTTY_CELL_DATA_COLOR_RGB,
                    (&raw mut rgb).cast(),
                )
            });
            Some(Color::Rgb(Rgb {
                r: rgb.r,
                g: rgb.g,
                b: rgb.b,
            }))
        }
        other => breach!("the header lists every cell content tag, not {}", other.0),
    }
}

/// libghostty stores only Unicode scalar values: its parser replaces invalid
/// input with U+FFFD, and so does its snapshot decoder.
#[track_caller]
fn scalar(codepoint: u32) -> char {
    char::from_u32(codepoint).or_contract("libghostty stores only Unicode scalar values")
}

fn convert_width(wide: sys::GhosttyCellWide) -> Width {
    match wide {
        sys::GhosttyCellWide::GHOSTTY_CELL_WIDE_NARROW => Width::Narrow,
        sys::GhosttyCellWide::GHOSTTY_CELL_WIDE_WIDE => Width::Wide,
        sys::GhosttyCellWide::GHOSTTY_CELL_WIDE_SPACER_TAIL => Width::SpacerTail,
        sys::GhosttyCellWide::GHOSTTY_CELL_WIDE_SPACER_HEAD => Width::SpacerHead,
        other => breach!("the header lists every cell width, not {}", other.0),
    }
}

fn convert_style(style: &sys::GhosttyStyle) -> Style {
    let underline = match style.underline {
        u if u == sys::GhosttySgrUnderline::GHOSTTY_SGR_UNDERLINE_NONE.0 => Underline::None,
        u if u == sys::GhosttySgrUnderline::GHOSTTY_SGR_UNDERLINE_SINGLE.0 => Underline::Single,
        u if u == sys::GhosttySgrUnderline::GHOSTTY_SGR_UNDERLINE_DOUBLE.0 => Underline::Double,
        u if u == sys::GhosttySgrUnderline::GHOSTTY_SGR_UNDERLINE_CURLY.0 => Underline::Curly,
        u if u == sys::GhosttySgrUnderline::GHOSTTY_SGR_UNDERLINE_DOTTED.0 => Underline::Dotted,
        u if u == sys::GhosttySgrUnderline::GHOSTTY_SGR_UNDERLINE_DASHED.0 => Underline::Dashed,
        other => breach!("the header lists every underline style, not {}", other),
    };

    let flags = [
        (style.bold, Attrs::BOLD),
        (style.italic, Attrs::ITALIC),
        (style.faint, Attrs::FAINT),
        (style.blink, Attrs::BLINK),
        (style.inverse, Attrs::INVERSE),
        (style.invisible, Attrs::INVISIBLE),
        (style.strikethrough, Attrs::STRIKETHROUGH),
        (style.overline, Attrs::OVERLINE),
    ];
    let attrs = flags
        .into_iter()
        .filter(|(set, _)| *set)
        .fold(Attrs::empty(), |attrs, (_, attr)| attrs.with(attr));

    Style {
        fg: convert_color(&style.fg_color),
        bg: convert_color(&style.bg_color),
        underline_color: convert_color(&style.underline_color),
        underline,
        attrs,
    }
}

fn convert_color(color: &sys::GhosttyStyleColor) -> Color {
    match color.tag {
        sys::GhosttyStyleColorTag::GHOSTTY_STYLE_COLOR_NONE => Color::Default,
        // SAFETY: the tag says the union holds a palette index.
        sys::GhosttyStyleColorTag::GHOSTTY_STYLE_COLOR_PALETTE => {
            Color::Palette(unsafe { color.value.palette })
        }
        sys::GhosttyStyleColorTag::GHOSTTY_STYLE_COLOR_RGB => {
            // SAFETY: the tag says the union holds an RGB value.
            let rgb = unsafe { color.value.rgb };
            Color::Rgb(Rgb {
                r: rgb.r,
                g: rgb.g,
                b: rgb.b,
            })
        }
        other => breach!("the header lists every style colour tag, not {}", other.0),
    }
}
