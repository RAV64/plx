//! Searching the history through libghostty-vt's search, which finds a
//! needle with ASCII letters in either case. plx filters what it finds for
//! the options libghostty lacks (letter case, whole words), counts the
//! matches it accepts, and marks those in view for the sync to draw.

use core::num::NonZeroU32;
use core::ptr::{self, NonNull};

use plx_bug::{OrBug as _, contract};
use plx_emulator::{Case, Change, Count, SearchDirection, SearchOptions, SearchStatus, Words};
use plx_ghostty_sys as sys;

use crate::error::{adopt, check_contract, widen};

/// A match's cells in one row of the view, `start..=end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mark {
    pub(crate) y: u16,
    pub(crate) start: u16,
    pub(crate) end: u16,
    pub(crate) kind: MarkKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MarkKind {
    Match,
    /// The selected match.
    Selected,
}

/// The most matches a count walks while output streams: about a
/// millisecond of filtering. Past it, the count waits for the next search
/// key and is shown as stale until then.
const STREAMING_RECOUNT: usize = 4096;

/// Whether a refresh must take an exact count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Recount {
    /// The user acted (a needle, an option, a selection): count exactly.
    Now,
    /// New output: count again only while that is cheap.
    IfCheap,
}

/// One emulator's search: idle until given a needle.
pub(crate) struct Searcher {
    /// `None` while not searching.
    handle: Option<SearchHandle>,
    needle: Vec<u8>,
    status: SearchStatus,
    /// What the count was taken from: libghostty's own count and selected
    /// match. A filtered count walks every match, so only a change of
    /// either (or of the options) takes it again.
    counted: Option<(usize, Option<RefKey>, SearchOptions)>,
    /// The matches in view, row by row, top to bottom.
    marks: Vec<Mark>,
    /// Scratch, reused: the marks before a refresh, matches, a match's text.
    previous: Vec<Mark>,
    matches: Vec<sys::GhosttySelection>,
    text: Vec<u8>,
    /// The width of the view, for finding the character after a match at
    /// the end of a row.
    cols: u16,
}

impl Searcher {
    pub(crate) const fn new() -> Self {
        Self {
            handle: None,
            needle: Vec::new(),
            status: SearchStatus {
                options: SearchOptions {
                    case: Case::Ignored,
                    words: Words::Within,
                },
                total: 0,
                selected: None,
                count: Count::Current,
            },
            counted: None,
            marks: Vec::new(),
            previous: Vec::new(),
            matches: Vec::new(),
            text: Vec::new(),
            cols: 0,
        }
    }

    pub(crate) const fn status(&self) -> SearchStatus {
        self.status
    }

    /// The matches in view, row by row, top to bottom.
    pub(crate) fn marks(&self) -> &[Mark] {
        &self.marks
    }

    /// Searches for `needle` as `options` say; an empty one stops.
    pub(crate) fn set(
        &mut self,
        terminal: sys::GhosttyTerminal,
        needle: &str,
        options: SearchOptions,
        view: View,
    ) {
        self.status.options = options;
        if needle.is_empty() {
            self.handle = None;
            self.needle.clear();
            self.status = SearchStatus {
                options,
                ..SearchStatus::default()
            };
            self.counted = None;
            self.marks.clear();
            return;
        }
        let handle = self
            .handle
            .get_or_insert_with(|| SearchHandle::new(terminal));
        let value = sys::GhosttyString {
            ptr: needle.as_ptr(),
            len: needle.len(),
        };
        // SAFETY: the search is live and bound to the live `terminal`; the
        // needle is a `GhosttyString` over bytes valid across the call,
        // which copies them. An unchanged needle keeps its results.
        check_contract("ghostty_search_set(NEEDLE)", unsafe {
            sys::ghostty_search_set(
                handle.0.as_ptr(),
                sys::GhosttySearchOption::GHOSTTY_SEARCH_OPT_NEEDLE,
                (&raw const value).cast(),
            )
        });
        self.needle.clear();
        self.needle.extend_from_slice(needle.as_bytes());
        self.counted = None;
        let _marks: Change = self.refresh(terminal, view, Recount::Now);
    }

    /// Selects the next match `direction` that the options accept and
    /// scrolls the view to it, if it is out of view.
    pub(crate) fn select(
        &mut self,
        terminal: sys::GhosttyTerminal,
        direction: SearchDirection,
        view: View,
    ) {
        let Some(handle) = &self.handle else {
            return;
        };
        let search = handle.0.as_ptr();
        let option = match direction {
            SearchDirection::Older => sys::GhosttySearchOption::GHOSTTY_SEARCH_OPT_SELECT_NEXT,
            SearchDirection::Newer => sys::GhosttySearchOption::GHOSTTY_SEARCH_OPT_SELECT_PREV,
        };
        // At most once round every match: the options may accept none.
        let mut steps = 1_usize;
        let mut step = 0_usize;
        while step < steps {
            // SAFETY: the search is live and bound to the live terminal; the
            // select options take a null value. It catches up with the
            // terminal first.
            let result = unsafe { sys::ghostty_search_set(search, option, ptr::null()) };
            if result == sys::GhosttyResult::GHOSTTY_NO_VALUE {
                // No matches at all.
                break;
            }
            check_contract("ghostty_search_set(SELECT)", result);
            if step == 0 {
                steps = get_usize(
                    search,
                    sys::GhosttySearchData::GHOSTTY_SEARCH_DATA_TOTAL_MATCHES,
                );
            }
            let selected = selected_or_none(search)
                .or_contract("a search that just selected a match has one selected");
            if self.accepts(terminal, &selected) {
                scroll_into_view(terminal, &selected, view);
                break;
            }
            step += 1;
        }
        let _marks: Change = self.refresh(terminal, view, Recount::Now);
    }

    /// Catches up with the terminal: the count, and the marks in `view`.
    /// `Changed` when the marks changed, so every row must be read again.
    pub(crate) fn refresh(
        &mut self,
        terminal: sys::GhosttyTerminal,
        view: View,
        recount: Recount,
    ) -> Change {
        self.cols = view.cols;
        core::mem::swap(&mut self.marks, &mut self.previous);
        self.marks.clear();
        if let Some(search) = self.handle.as_ref().map(|handle| handle.0.as_ptr()) {
            // SAFETY: the search is live and bound to the live terminal.
            check_contract("ghostty_search_run", unsafe {
                sys::ghostty_search_run(search)
            });
            let selected = selected_or_none(search);
            self.count(terminal, search, selected, recount);
            self.mark(terminal, search, selected, view);
        }
        if self.marks == self.previous {
            Change::Unchanged
        } else {
            Change::Changed
        }
    }

    /// Counts the matches the options accept and places the selected one.
    /// Without options libghostty's own count is exact and free; a
    /// filtered count walks every match, so new output only takes one
    /// while there are few (`recount`), and marks the count stale
    /// otherwise.
    fn count(
        &mut self,
        terminal: sys::GhosttyTerminal,
        search: sys::GhosttySearch,
        selected: Option<sys::GhosttySelection>,
        recount: Recount,
    ) {
        let options = self.status.options;
        let found = get_usize(
            search,
            sys::GhosttySearchData::GHOSTTY_SEARCH_DATA_TOTAL_MATCHES,
        );
        let key = (found, selected.map(|s| RefKey::of(&s.start)), options);
        if self.counted == Some(key) {
            return;
        }
        if options != SearchOptions::default()
            && recount == Recount::IfCheap
            && found > STREAMING_RECOUNT
        {
            self.status.count = Count::Stale;
            return;
        }
        self.counted = Some(key);
        self.status.count = Count::Current;
        let (total, position) = if options == SearchOptions::default() {
            let position = selected.map(|_| {
                get_usize(
                    search,
                    sys::GhosttySearchData::GHOSTTY_SEARCH_DATA_SELECTED_INDEX,
                ) + 1
            });
            (found, position)
        } else {
            self.collect(search, sys::GhosttySearchData::GHOSTTY_SEARCH_DATA_MATCHES);
            let mut matches = core::mem::take(&mut self.matches);
            let mut total = 0_usize;
            let mut position = None;
            for found in &matches {
                if self.accepts(terminal, found) {
                    total += 1;
                    if selected.is_some_and(|s| same_start(&s, found)) {
                        position = Some(total);
                    }
                }
            }
            matches.clear();
            self.matches = matches;
            (total, position)
        };
        self.status.total = saturate(total);
        self.status.selected = position.and_then(|p| NonZeroU32::new(saturate(p)));
    }

    /// Marks the accepted matches in `view`.
    fn mark(
        &mut self,
        terminal: sys::GhosttyTerminal,
        search: sys::GhosttySearch,
        selected: Option<sys::GhosttySelection>,
        view: View,
    ) {
        self.collect(
            search,
            sys::GhosttySearchData::GHOSTTY_SEARCH_DATA_VIEWPORT_MATCHES,
        );
        let matches = core::mem::take(&mut self.matches);
        let top = viewport_top(terminal);
        for found in &matches {
            if !self.accepts(terminal, found) {
                continue;
            }
            let kind = if selected.is_some_and(|s| same_start(&s, found)) {
                MarkKind::Selected
            } else {
                MarkKind::Match
            };
            let (start, end) = (
                screen_point(terminal, &found.start),
                screen_point(terminal, &found.end),
            );
            for y in start.y..=end.y {
                let Some(row) = y
                    .checked_sub(top)
                    .filter(|&row| row < usize::from(view.rows))
                else {
                    continue;
                };
                self.marks.push(Mark {
                    y: u16::try_from(row).or_bug("a row in view is below its u16 row count"),
                    start: if y == start.y { start.x } else { 0 },
                    end: if y == end.y {
                        end.x
                    } else {
                        view.cols.saturating_sub(1)
                    },
                    kind,
                });
            }
        }
        self.matches = matches;
        self.marks.sort_unstable_by_key(|mark| (mark.y, mark.start));
    }

    /// Fills `self.matches` with the selection list `data`.
    fn collect(&mut self, search: sys::GhosttySearch, data: sys::GhosttySearchData) {
        self.matches.clear();
        loop {
            let mut buffer = sys::GhosttySelectionBuffer {
                ptr: self.matches.as_mut_ptr(),
                cap: self.matches.capacity(),
                len: 0,
            };
            // SAFETY: the search is live; the buffer describes the vector's
            // spare capacity, which the call writes at most `cap` entries to.
            let result = unsafe { sys::ghostty_search_get(search, data, (&raw mut buffer).cast()) };
            if result == sys::GhosttyResult::GHOSTTY_OUT_OF_SPACE {
                self.matches.reserve(buffer.len);
                continue;
            }
            check_contract("ghostty_search_get(MATCHES)", result);
            contract!(
                buffer.len <= self.matches.capacity(),
                "a selection list fits the buffer it was written to"
            );
            // SAFETY: the call initialised `len` entries, within capacity.
            unsafe { self.matches.set_len(buffer.len) };
            return;
        }
    }

    /// Whether the options accept `found`, a match libghostty found.
    fn accepts(&mut self, terminal: sys::GhosttyTerminal, found: &sys::GhosttySelection) -> bool {
        let options = self.status.options;
        if options.case == Case::Matched {
            format(terminal, found, &mut self.text);
            if self.text != self.needle {
                return false;
            }
        }
        if options.words == Words::Whole {
            // No word character right before or right after it.
            let found = ordered(terminal, found);
            return !word_character_beside(terminal, &found.start, Side::Before, self.cols)
                && !word_character_beside(terminal, &found.end, Side::After, self.cols);
        }
        true
    }
}

/// The view a search marks matches in.
#[derive(Clone, Copy, Debug)]
pub(crate) struct View {
    pub(crate) cols: u16,
    pub(crate) rows: u16,
}

/// A grid reference as a value to compare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RefKey {
    node: usize,
    x: u16,
    y: u16,
}

impl RefKey {
    fn of(grid_ref: &sys::GhosttyGridRef) -> Self {
        Self {
            node: grid_ref.node.addr(),
            x: grid_ref.x,
            y: grid_ref.y,
        }
    }
}

fn same_start(a: &sys::GhosttySelection, b: &sys::GhosttySelection) -> bool {
    RefKey::of(&a.start) == RefKey::of(&b.start)
}

fn saturate(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

const fn empty_selection() -> sys::GhosttySelection {
    let empty = sys::GhosttyGridRef {
        size: size_of::<sys::GhosttyGridRef>(),
        node: ptr::null_mut(),
        x: 0,
        y: 0,
    };
    sys::GhosttySelection {
        size: size_of::<sys::GhosttySelection>(),
        start: empty,
        end: empty,
        rectangle: false,
    }
}

fn get_usize(search: sys::GhosttySearch, data: sys::GhosttySearchData) -> usize {
    let mut value = 0_usize;
    // SAFETY: the search is live; `data` documents a `size_t` output.
    check_contract("ghostty_search_get(size_t)", unsafe {
        sys::ghostty_search_get(search, data, (&raw mut value).cast())
    });
    value
}

/// The selected match, ordered top-left first; `None` when none is.
fn selected_or_none(search: sys::GhosttySearch) -> Option<sys::GhosttySelection> {
    let mut selected = empty_selection();
    // SAFETY: the search is live; `SELECTED_MATCH` documents a sized
    // `GhosttySelection` output.
    let result = unsafe {
        sys::ghostty_search_get(
            search,
            sys::GhosttySearchData::GHOSTTY_SEARCH_DATA_SELECTED_MATCH,
            (&raw mut selected).cast(),
        )
    };
    if result == sys::GhosttyResult::GHOSTTY_NO_VALUE {
        return None;
    }
    check_contract("ghostty_search_get(SELECTED_MATCH)", result);
    Some(selected)
}

/// Which side of a match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Before,
    After,
}

/// Whether the character right `side` of the cell `at` (a match's first
/// or last) is a word character: a letter, a digit or `_`. Across a soft
/// wrap the line goes on in the next row; a blank, a hard line end or the
/// screen's edge is no word character.
fn word_character_beside(
    terminal: sys::GhosttyTerminal,
    at: &sys::GhosttyGridRef,
    side: Side,
    cols: u16,
) -> bool {
    let point = screen_point(terminal, at);
    let neighbour = match side {
        Side::Before => match point.x.checked_sub(1) {
            Some(x) => Some((x, point.y)),
            // The row goes on from the one above, which wrapped.
            None if row_flag(at, sys::GhosttyRowData::GHOSTTY_ROW_DATA_WRAP_CONTINUATION) => {
                point.y.checked_sub(1).map(|y| (cols.saturating_sub(1), y))
            }
            None => None,
        },
        Side::After => {
            // A wide character's second cell is part of it.
            let width = if cell_of(at).wide == sys::GhosttyCellWide::GHOSTTY_CELL_WIDE_WIDE {
                2
            } else {
                1
            };
            let x = point.x.saturating_add(width);
            if x < cols {
                Some((x, point.y))
            } else if row_flag(at, sys::GhosttyRowData::GHOSTTY_ROW_DATA_WRAP) {
                Some((0, point.y + 1))
            } else {
                None
            }
        }
    };
    let Some((x, y)) = neighbour else {
        return false;
    };
    let mut neighbour = cell_of(&grid_ref_at(terminal, x, y));
    // Before a match, a wide character's second cell stands for it.
    if side == Side::Before
        && neighbour.wide == sys::GhosttyCellWide::GHOSTTY_CELL_WIDE_SPACER_TAIL
        && let Some(x) = x.checked_sub(1)
    {
        neighbour = cell_of(&grid_ref_at(terminal, x, y));
    }
    char::from_u32(neighbour.codepoint).is_some_and(|c| c.is_alphanumeric() || c == '_')
}

/// What a cell holds, as far as word boundaries go.
#[derive(Clone, Copy, Debug)]
struct CellInfo {
    codepoint: u32,
    wide: sys::GhosttyCellWide,
}

fn cell_of(at: &sys::GhosttyGridRef) -> CellInfo {
    let mut cell: sys::GhosttyCell = 0;
    // SAFETY: `at` is a fresh reference into the live terminal.
    check_contract("ghostty_grid_ref_cell", unsafe {
        sys::ghostty_grid_ref_cell(at, &raw mut cell)
    });
    let mut info = CellInfo {
        codepoint: 0,
        wide: sys::GhosttyCellWide::GHOSTTY_CELL_WIDE_NARROW,
    };
    // SAFETY: `cell` is a cell value; each output has its documented type.
    check_contract("ghostty_cell_get(CODEPOINT)", unsafe {
        sys::ghostty_cell_get(
            cell,
            sys::GhosttyCellData::GHOSTTY_CELL_DATA_CODEPOINT,
            (&raw mut info.codepoint).cast(),
        )
    });
    // SAFETY: as above.
    check_contract("ghostty_cell_get(WIDE)", unsafe {
        sys::ghostty_cell_get(
            cell,
            sys::GhosttyCellData::GHOSTTY_CELL_DATA_WIDE,
            (&raw mut info.wide).cast(),
        )
    });
    info
}

/// A flag of the row holding `at`: whether it wraps into the next
/// (`WRAP`), or goes on from the one above (`WRAP_CONTINUATION`).
fn row_flag(at: &sys::GhosttyGridRef, flag: sys::GhosttyRowData) -> bool {
    let mut row: sys::GhosttyRow = 0;
    // SAFETY: `at` is a fresh reference into the live terminal.
    check_contract("ghostty_grid_ref_row", unsafe {
        sys::ghostty_grid_ref_row(at, &raw mut row)
    });
    let mut set = false;
    // SAFETY: `row` is a row value; both flags documented here are `bool`.
    check_contract("ghostty_row_get(WRAP)", unsafe {
        sys::ghostty_row_get(row, flag, (&raw mut set).cast())
    });
    set
}

/// The grid reference of the cell at (`x`, `y`) among all rows.
fn grid_ref_at(terminal: sys::GhosttyTerminal, x: u16, y: usize) -> sys::GhosttyGridRef {
    let mut grid_ref = empty_selection().start;
    let point = sys::GhosttyPoint {
        tag: sys::GhosttyPointTag::GHOSTTY_POINT_TAG_SCREEN,
        value: sys::GhosttyPointValue {
            coordinate: sys::GhosttyPointCoordinate {
                x,
                y: u32::try_from(y).or_bug("screen rows fit u32"),
            },
        },
    };
    // SAFETY: the terminal is live; the point is a cell next to one of its
    // cells (in the same row, or the row a wrap leads to or from);
    // `coordinate` is the field every tag reads.
    check_contract("ghostty_terminal_grid_ref(SCREEN)", unsafe {
        sys::ghostty_terminal_grid_ref(terminal, point, &raw mut grid_ref)
    });
    grid_ref
}

/// `selection` with its top-left end first.
fn ordered(
    terminal: sys::GhosttyTerminal,
    selection: &sys::GhosttySelection,
) -> sys::GhosttySelection {
    let mut ordered = empty_selection();
    // SAFETY: the terminal is live; `selection` was just derived from its
    // active screen.
    check_contract("ghostty_terminal_selection_ordered", unsafe {
        sys::ghostty_terminal_selection_ordered(
            terminal,
            selection,
            sys::GhosttySelectionOrder::GHOSTTY_SELECTION_ORDER_FORWARD,
            &raw mut ordered,
        )
    });
    ordered
}

/// Replaces `out` with the plain text of `found`, soft wraps joined.
fn format(terminal: sys::GhosttyTerminal, found: &sys::GhosttySelection, out: &mut Vec<u8>) {
    let options = sys::GhosttyTerminalSelectionFormatOptions {
        size: size_of::<sys::GhosttyTerminalSelectionFormatOptions>(),
        emit: sys::GhosttyFormatterFormat::GHOSTTY_FORMATTER_FORMAT_PLAIN,
        unwrap: true,
        trim: false,
        selection: found,
    };
    out.clear();
    loop {
        let mut written = 0_usize;
        // SAFETY: the terminal is live; `found` is a fresh match on its
        // active screen; the buffer is the vector's capacity.
        let result = unsafe {
            sys::ghostty_terminal_selection_format_buf(
                terminal,
                options,
                out.as_mut_ptr(),
                out.capacity(),
                &raw mut written,
            )
        };
        if result == sys::GhosttyResult::GHOSTTY_OUT_OF_SPACE {
            out.reserve(written);
            continue;
        }
        check_contract("ghostty_terminal_selection_format_buf", result);
        contract!(
            written <= out.capacity(),
            "formatted text fits the buffer it was written to"
        );
        // SAFETY: the call initialised `written` bytes, within capacity.
        unsafe { out.set_len(written) };
        return;
    }
}

/// Where `at` is among all rows, the oldest history row first.
fn screen_point(terminal: sys::GhosttyTerminal, at: &sys::GhosttyGridRef) -> Point {
    let mut point = sys::GhosttyPointCoordinate::default();
    // SAFETY: the terminal is live; `at` is a fresh reference into it,
    // which every row has a screen coordinate for.
    check_contract("ghostty_terminal_point_from_grid_ref(SCREEN)", unsafe {
        sys::ghostty_terminal_point_from_grid_ref(
            terminal,
            at,
            sys::GhosttyPointTag::GHOSTTY_POINT_TAG_SCREEN,
            &raw mut point,
        )
    });
    Point {
        x: point.x,
        y: widen(point.y),
    }
}

#[derive(Clone, Copy, Debug)]
struct Point {
    x: u16,
    y: usize,
}

/// The screen row at the top of the view.
fn viewport_top(terminal: sys::GhosttyTerminal) -> usize {
    let mut bar = sys::GhosttyTerminalScrollbar::default();
    // SAFETY: the terminal is live; `DATA_SCROLLBAR` documents a
    // `GhosttyTerminalScrollbar` output.
    check_contract("ghostty_terminal_get(SCROLLBAR)", unsafe {
        sys::ghostty_terminal_get(
            terminal,
            sys::GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_SCROLLBAR,
            (&raw mut bar).cast(),
        )
    });
    usize::try_from(bar.offset).or_bug("a scrollback offset fits usize on 64-bit targets")
}

/// Scrolls the view so `found` is in its middle, unless it is in view.
fn scroll_into_view(terminal: sys::GhosttyTerminal, found: &sys::GhosttySelection, view: View) {
    let top = viewport_top(terminal);
    let y = screen_point(terminal, &found.start).y;
    let rows = usize::from(view.rows);
    if (top..top + rows).contains(&y) {
        return;
    }
    let target = y.saturating_sub(rows / 2);
    let signed = |n: usize| isize::try_from(n).or_bug("a row count fits isize");
    let behavior = sys::GhosttyTerminalScrollViewport {
        tag: sys::GhosttyTerminalScrollViewportTag::GHOSTTY_SCROLL_VIEWPORT_DELTA,
        value: sys::GhosttyTerminalScrollViewportValue {
            delta: signed(target) - signed(top),
        },
    };
    // SAFETY: the terminal is live; DELTA reads the `delta` field written.
    unsafe { sys::ghostty_terminal_scroll_viewport(terminal, behavior) };
}

/// An owned search, bound to its emulator's terminal.
struct SearchHandle(NonNull<sys::GhosttySearchImpl>);

impl SearchHandle {
    fn new(terminal: sys::GhosttyTerminal) -> Self {
        const CALL: &str = "ghostty_search_new";
        let mut raw: sys::GhosttySearch = ptr::null_mut();
        // SAFETY: valid out-pointer, allocator and live terminal. It fails
        // only on invalid arguments or out-of-memory, which plx's allocator
        // turns into an abort.
        check_contract(CALL, unsafe {
            sys::ghostty_search_new(crate::alloc::rust(), &raw mut raw, terminal)
        });
        let handle = Self(adopt(raw, CALL));
        let scroll = sys::GhosttySearchScroll::GHOSTTY_SEARCH_SCROLL_NONE;
        // SAFETY: the search is live; `SELECT_SCROLL` takes a
        // `GhosttySearchScroll`. plx scrolls to the matches it accepts.
        check_contract("ghostty_search_set(SELECT_SCROLL)", unsafe {
            sys::ghostty_search_set(
                handle.0.as_ptr(),
                sys::GhosttySearchOption::GHOSTTY_SEARCH_OPT_SELECT_SCROLL,
                (&raw const scroll).cast(),
            )
        });
        handle
    }
}

impl Drop for SearchHandle {
    fn drop(&mut self) {
        // SAFETY: owned by this value and freed exactly once. It may outlive
        // its terminal (the search is then detached), or not.
        unsafe { sys::ghostty_search_free(self.0.as_ptr()) };
    }
}
