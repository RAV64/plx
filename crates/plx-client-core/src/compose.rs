//! The compositor: turns a [`Mirror`]'s damage into terminal output.
//!
//! A frame writes only what changed since the last one: the damaged rows of
//! each pane, the chrome (separators and the status line) when it changed,
//! and the cursor. Everything is drawn at absolute positions, clipped to the
//! client's terminal, inside one synchronized update, so the terminal never
//! shows half a frame. Styles are emitted only when they change, downgraded
//! to what the terminal can show.
//!
//! The output buffer is the caller's and is reused: a frame allocates
//! nothing once it has grown to the largest frame so far.

use alloc::vec::Vec;
use core::num::NonZeroU16;
use core::ops::Range;

use plx_bug::OrBug as _;
use plx_core::geometry::{Axis, Rect};
use plx_core::layout::Separator;
use plx_core::mode::{Mode, Scrolling, Searching};
use plx_core::search::Needle;
use plx_core::size::pane_area;
use plx_core::tab::{TabNumber, Zoom};
use plx_emulator::{
    Attrs, Case, Cell, Change, Color, Count, CursorBlink, CursorShape, CursorVisibility, GlyphKind,
    GridSize, Position, Rgb, SearchStatus, Style, StyleId, Underline, Width, Words,
};
use plx_protocol::server::FrameHeader;
use unicode_segmentation::GraphemeCursor;
use unicode_width::UnicodeWidthChar as _;

use crate::mirror::{Mirror, MirrorPane, TabLabel};

/// How many colours the terminal shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorDepth {
    /// 24-bit colour (`38;2;r;g;b`).
    TrueColor,
    /// The xterm 256-colour palette.
    Palette256,
    /// The 16 ANSI colours.
    Ansi16,
}

/// Whether the terminal knows styled and coloured underlines (`4:3`, `58`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnderlineStyles {
    Styled,
    /// Every underline is drawn single, in the text colour.
    Plain,
}

/// Whether the terminal draws progress reports (`OSC 9;4`) as a progress
/// bar. One that does not is sent none: some show any `OSC 9` as a desktop
/// notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressBars {
    Drawn,
    Unsupported,
}

/// What the client's terminal can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub color: ColorDepth,
    pub underlines: UnderlineStyles,
    pub progress: ProgressBars,
}

/// What the screen was drawn for. When it changes, the screen is cleared and
/// drawn whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Canvas {
    session: GridSize,
    terminal: GridSize,
}

/// Draws a mirror onto the client's terminal.
#[derive(Debug)]
pub struct Compositor {
    capabilities: Capabilities,
    /// `None` until the first frame is drawn, and after [`Self::invalidate`].
    drawn: Option<Canvas>,
    /// The cursor style last set; `None` when unknown.
    cursor_style: Option<(CursorShape, CursorBlink)>,
    /// Where the status line's tab labels are, as last drawn.
    tab_labels: TabLabels,
}

/// Where the tab labels were drawn: the status line's row (`None` when
/// there is none) and each label's columns, in tab order. A click there
/// picks the tab.
#[derive(Debug)]
struct TabLabels {
    row: Option<u32>,
    spans: Vec<Range<u32>>,
}

impl Compositor {
    #[must_use]
    pub const fn new(capabilities: Capabilities) -> Self {
        Self {
            capabilities,
            drawn: None,
            cursor_style: None,
            tab_labels: TabLabels {
                row: None,
                spans: Vec::new(),
            },
        }
    }

    /// The tab whose label is drawn at `at`, as last drawn; `None`
    /// anywhere else.
    #[must_use]
    pub fn tab_at_or_none(&self, at: Position) -> Option<TabNumber> {
        if self.tab_labels.row != Some(u32::from(at.y)) {
            return None;
        }
        let x = u32::from(at.x);
        let index = self
            .tab_labels
            .spans
            .iter()
            .position(|span| span.contains(&x))?;
        let number = u16::try_from(index + 1)
            .map_or(None, NonZeroU16::new)
            .or_bug("tab positions are bounded by MAX_TABS");
        Some(TabNumber::new(number))
    }

    /// Forgets what is on the terminal (the client resumed, or something
    /// else wrote to it): the next frame draws everything.
    pub const fn invalidate(&mut self) {
        self.drawn = None;
        self.cursor_style = None;
    }

    /// Appends to `out` what brings the terminal up to date with `mirror`,
    /// and clears the mirror's damage. Returns `Unchanged`, writing nothing,
    /// when there is nothing to draw (including before the first frame).
    pub fn render(&mut self, mirror: &mut Mirror, terminal: GridSize, out: &mut Vec<u8>) -> Change {
        let Some(header) = mirror.header().copied() else {
            return Change::Unchanged;
        };
        let canvas = Canvas {
            session: header.size,
            terminal,
        };
        let whole = self.drawn != Some(canvas);
        if whole {
            mirror.damage_all();
        } else if !damaged(mirror) {
            return Change::Unchanged;
        }
        let clip = Clip::new(canvas);
        out.extend_from_slice(b"\x1b[?2026h\x1b[?25l\x1b[?7l\x1b[0m");
        if whole {
            out.extend_from_slice(b"\x1b[2J");
        }
        let mut pen = Pen {
            capabilities: self.capabilities,
            style: Style::DEFAULT,
        };
        if mirror.chrome_damage() == Change::Changed {
            let focus = mirror.pane_or_none(header.focus).map(MirrorPane::rect);
            let separators = mirror.separators();
            for separator in separators {
                draw_separator(out, &mut pen, clip, separator, separators, focus);
            }
            draw_status(out, &mut pen, clip, mirror, &header, &mut self.tab_labels);
        }
        for (_, pane) in mirror.panes() {
            let rect = pane.rect();
            for (y, cells) in pane.damaged_rows() {
                draw_row(out, &mut pen, clip, pane, rect, y, cells);
            }
        }
        if pen.style != Style::DEFAULT {
            out.extend_from_slice(b"\x1b[0m");
        }
        if let Some(pane) = mirror.pane_or_none(header.focus) {
            self.draw_cursor(out, clip, pane);
        }
        out.extend_from_slice(b"\x1b[?7h\x1b[?2026l");
        mirror.clear_damage();
        self.drawn = Some(canvas);
        Change::Changed
    }

    /// Places and shows the focused pane's cursor, unless it is hidden or
    /// outside what is drawn (it stays hidden from the frame's start).
    fn draw_cursor(&mut self, out: &mut Vec<u8>, clip: Clip, pane: &MirrorPane) {
        let cursor = pane.cursor();
        let Some(position) = cursor.position else {
            return;
        };
        if cursor.visibility == CursorVisibility::Hidden {
            return;
        }
        let rect = pane.rect();
        let x = u32::from(rect.x()) + u32::from(position.x);
        let y = u32::from(rect.y()) + u32::from(position.y);
        let inside = u32::from(position.x) < u32::from(rect.cols())
            && u32::from(position.y) < u32::from(rect.rows());
        if !inside || x >= clip.cols || y >= clip.rows {
            return;
        }
        move_to(out, x, y);
        // The terminal's own cursor blinks as its user configured it.
        let style = match cursor.shape {
            CursorShape::Default => (CursorShape::Default, CursorBlink::Steady),
            shape @ (CursorShape::Block | CursorShape::Underline | CursorShape::Bar) => {
                (shape, cursor.blink)
            }
        };
        if self.cursor_style != Some(style) {
            let code = match style {
                (CursorShape::Default, _) => b'0',
                (CursorShape::Block, CursorBlink::Blinking) => b'1',
                (CursorShape::Block, CursorBlink::Steady) => b'2',
                (CursorShape::Underline, CursorBlink::Blinking) => b'3',
                (CursorShape::Underline, CursorBlink::Steady) => b'4',
                (CursorShape::Bar, CursorBlink::Blinking) => b'5',
                (CursorShape::Bar, CursorBlink::Steady) => b'6',
            };
            out.extend_from_slice(&[0x1b, b'[', code, b' ', b'q']);
            self.cursor_style = Some(style);
        }
        out.extend_from_slice(b"\x1b[?25h");
    }
}

/// Whether anything in `mirror` needs drawing.
fn damaged(mirror: &Mirror) -> bool {
    mirror.chrome_damage() == Change::Changed
        || mirror.panes().any(|(_, pane)| {
            pane.cursor_damage() == Change::Changed || pane.damaged_rows().next().is_some()
        })
}

/// The drawable area: the session's screen, cut to the client's terminal.
#[derive(Clone, Copy, Debug)]
struct Clip {
    cols: u32,
    rows: u32,
    /// The terminal's width: one past its last column.
    margin: u32,
}

impl Clip {
    fn new(canvas: Canvas) -> Self {
        Self {
            margin: u32::from(canvas.terminal.cols().get()),
            cols: u32::from(
                canvas
                    .session
                    .cols()
                    .get()
                    .min(canvas.terminal.cols().get()),
            ),
            rows: u32::from(
                canvas
                    .session
                    .rows()
                    .get()
                    .min(canvas.terminal.rows().get()),
            ),
        }
    }
}

/// One pane row, from its left edge to its right edge or the clip.
fn draw_row(
    out: &mut Vec<u8>,
    pen: &mut Pen,
    clip: Clip,
    pane: &MirrorPane,
    rect: Rect,
    y: u16,
    cells: &[Cell],
) {
    let row = u32::from(rect.y()) + u32::from(y);
    if row >= clip.rows {
        return;
    }
    let limit = rect.right().min(clip.cols);
    // Where the terminal's cursor is known to be; `None` after text whose
    // width the terminal may measure differently (wide characters,
    // clusters), so the next cell is placed absolutely.
    let mut at: Option<u32> = None;
    // The previous cell drew a wide character over this one.
    let mut covered = Covered::No;
    // The style id the pen holds, once this row set it.
    let mut drawn_style: Option<StyleId> = None;
    let start = u32::from(rect.x());
    let shown = usize::try_from(limit.saturating_sub(start))
        .or_bug("a row's width fits usize")
        .min(cells.len());
    let cells = cells
        .get(..shown)
        .or_bug("at most the row's cells are shown");
    let mut i = 0;
    while let Some(&cell) = cells.get(i) {
        let x = start + u32::try_from(i).or_bug("a column fits u32");
        // The common run: narrow ASCII or blank cells in the pen's style,
        // starting where the cursor already is. Written with one reserve
        // and no per-byte bookkeeping (measured: a per-cell push left the
        // loop's state to the register allocator's luck).
        if at == Some(x) && drawn_style == Some(cell.style) {
            let ahead = cells.get(i..).or_bug("i indexes a cell");
            let run = Cell::plain_run(ahead, cell.style);
            if run > 0 {
                let plain = ahead.get(..run).or_bug("the run was counted in the row");
                out.extend(plain.iter().map(|next| next.plain_byte()));
                i += run;
                at = Some(x + u32::try_from(run).or_bug("a run fits u32"));
                covered = Covered::No;
                continue;
            }
        }
        i += 1;
        if cell.width == Width::SpacerTail && covered == Covered::Yes {
            covered = Covered::No;
            continue;
        }
        let shown = Shown::of(cell, x, limit);
        // A terminal that clusters graphemes joins a codepoint onto the cell
        // drawn to its left when the two form one grapheme, wherever its
        // cursor came from (Ghostty decides from the cells). Cells the pane
        // keeps apart must not be drawn left to right then: the cells that
        // would join are drawn right to left, over blanks.
        let after = i + usize::from(shown == Shown::Wide);
        let (chain_end, end_shown) =
            joining_chain_end(pane, cells, (i - 1, cell, shown), after, start, limit);
        if chain_end > i - 1 {
            // The cells the chain covers, as indices.
            let past = chain_end + 1 + usize::from(end_shown == Shown::Wide);
            move_to(out, x, row);
            for _ in i - 1..past {
                out.push(b' ');
            }
            for (k, &kcell) in cells.iter().enumerate().take(past).skip(i - 1).rev() {
                // A wide character's right half is drawn with it.
                if kcell.width != Width::SpacerTail || k == i - 1 {
                    let kx = column(start, k);
                    draw_cell(
                        out,
                        pen,
                        clip,
                        pane,
                        kcell,
                        kx,
                        row,
                        limit,
                        None,
                        &mut drawn_style,
                    );
                }
            }
            i = past;
            at = None;
            covered = Covered::No;
            continue;
        }
        at = draw_cell(
            out,
            pen,
            clip,
            pane,
            cell,
            x,
            row,
            limit,
            at,
            &mut drawn_style,
        );
        covered = if shown == Shown::Wide {
            Covered::Yes
        } else {
            Covered::No
        };
    }
}

/// How a cell is drawn at column `x` before `limit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shown {
    /// Text one column wide.
    Narrow,
    /// A wide character: it covers the next column too.
    Wide,
    /// Nothing to draw but a blank (an empty cell, a spacer, a wide
    /// character the clip cuts).
    Blank,
}

impl Shown {
    fn of(cell: Cell, x: u32, limit: u32) -> Self {
        match (cell.width, cell.glyph.kind()) {
            (_, GlyphKind::Empty) | (Width::SpacerTail | Width::SpacerHead, _) => Self::Blank,
            (Width::Narrow, _) => Self::Narrow,
            (Width::Wide, _) if x + 1 < limit => Self::Wide,
            (Width::Wide, _) => Self::Blank,
        }
    }
}

/// The last of the cells from `after` on that would each join onto the cell
/// before them (starting with `first`: its index, cell and how it shows) if
/// drawn after it, and how it shows; `first` when the next one would not.
fn joining_chain_end(
    pane: &MirrorPane,
    cells: &[Cell],
    first: (usize, Cell, Shown),
    mut after: usize,
    start: u32,
    limit: u32,
) -> (usize, Shown) {
    let (mut left, mut left_cell, mut left_shown) = first;
    while let Some(&next) = cells.get(after) {
        let shown = Shown::of(next, column(start, after), limit);
        if shown == Shown::Blank || left_shown == Shown::Blank || !joins(pane, left_cell, next) {
            break;
        }
        (left, left_cell, left_shown) = (after, next, shown);
        after += if shown == Shown::Wide { 2 } else { 1 };
    }
    (left, left_shown)
}

/// The terminal column of a row's cell `index`, the row starting at `start`.
fn column(start: u32, index: usize) -> u32 {
    start + u32::try_from(index).or_bug("a column fits u32")
}

/// Whether `right`'s first codepoint would join `left`'s grapheme if drawn
/// right after it (UAX #29, extended grapheme clusters, as terminals that
/// cluster apply it). Two ASCII codepoints never join.
fn joins(pane: &MirrorPane, left: Cell, right: Cell) -> bool {
    let left_char: char;
    let left_chars: &[char] = match left.glyph.kind() {
        GlyphKind::Char(c) => {
            left_char = c;
            core::slice::from_ref(&left_char)
        }
        GlyphKind::Cluster(id) => pane.grapheme(id),
        GlyphKind::Empty => return false,
    };
    let first = match right.glyph.kind() {
        GlyphKind::Char(c) => c,
        GlyphKind::Cluster(id) => match pane.grapheme(id).first() {
            Some(&c) => c,
            None => return false,
        },
        GlyphKind::Empty => return false,
    };
    let Some(&last) = left_chars.last() else {
        return false;
    };
    if last.is_ascii() && first.is_ascii() {
        return false;
    }
    // The left grapheme's codepoints (its tail, if long: the rules look
    // back only through extenders and joiners), then the right's first.
    let mut text = [0_u8; 64];
    let mut len = 0;
    let tail = left_chars.len().saturating_sub(12);
    for &c in left_chars.iter().skip(tail) {
        if len + c.len_utf8() + first.len_utf8() > text.len() {
            break;
        }
        len += c
            .encode_utf8(text.get_mut(len..).or_bug("room was checked"))
            .len();
    }
    let boundary = len;
    len += first
        .encode_utf8(text.get_mut(len..).or_bug("room was kept for it"))
        .len();
    let text = core::str::from_utf8(text.get(..len).or_bug("len bytes were written"))
        .or_bug("whole chars were encoded");
    let mut cursor = GraphemeCursor::new(boundary, len, true);
    !cursor
        .is_boundary(text, 0)
        .or_bug("the whole text is one chunk, so no context is missing")
}

/// Draws `cell` at column `x` of terminal row `row`, from where the cursor
/// is (`at`, `None` when unknown); returns where the cursor is known to be
/// afterwards.
#[expect(
    clippy::too_many_arguments,
    reason = "the row's drawing state, passed down"
)]
fn draw_cell(
    out: &mut Vec<u8>,
    pen: &mut Pen,
    clip: Clip,
    pane: &MirrorPane,
    cell: Cell,
    x: u32,
    row: u32,
    limit: u32,
    at: Option<u32>,
    drawn_style: &mut Option<StyleId>,
) -> Option<u32> {
    let glyph = match Shown::of(cell, x, limit) {
        Shown::Narrow | Shown::Wide => Some(cell.glyph.kind()),
        Shown::Blank => None,
    };
    if at != Some(x) {
        move_to(out, x, row);
    }
    // Within a pane an id is one style: resolve only on a change.
    if *drawn_style != Some(cell.style) {
        pen.set(out, pane.style(cell.style));
        *drawn_style = Some(cell.style);
    }
    match glyph {
        None | Some(GlyphKind::Empty) => {
            out.push(b' ');
            Some(x + 1)
        }
        Some(GlyphKind::Char(c)) => {
            push_char(out, c);
            (cell.width == Width::Narrow).then_some(x + 1)
        }
        Some(GlyphKind::Cluster(id)) => {
            // With autowrap off, libghostty attaches a cluster's later
            // codepoints to the cell under the cursor when that is the last
            // column and holds text (a separator, another pane); with
            // autowrap on, always to the cell just written (measured). So
            // when the base leaves the cursor on the last column, autowrap
            // is on for the cluster. It cannot wrap: the column after the
            // base is free, and no terminal draws a cluster wider than two
            // columns.
            let base = if cell.width == Width::Wide { 2 } else { 1 };
            let onto_last = x + base + 1 == clip.margin;
            if onto_last {
                out.extend_from_slice(b"\x1b[?7h");
            }
            for &c in pane.grapheme(id) {
                push_char(out, c);
            }
            if onto_last {
                out.extend_from_slice(b"\x1b[?7l");
            }
            None
        }
    }
}

/// Whether a cell is the right half of a wide character just drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Covered {
    Yes,
    No,
}

/// The chrome's colours: the cendre palette (the user's Zellij, Neovim
/// and Ghostty theme, hard depth). Terminals with fewer colours get the
/// nearest ones.
mod cendre {
    use plx_emulator::Rgb;

    const fn rgb(hex: u32) -> Rgb {
        let [_, r, g, b] = hex.to_be_bytes();
        Rgb { r, g, b }
    }

    pub(super) const BG1: Rgb = rgb(0x0020_1b19);
    pub(super) const FG: Rgb = rgb(0x00e6_d5c2);
    pub(super) const FG_DIM: Rgb = rgb(0x00a0_9384);
    pub(super) const EMBER: Rgb = rgb(0x00ea_9875);
    pub(super) const SAP: Rgb = rgb(0x0099_af6b);
    pub(super) const POTASSIUM: Rgb = rgb(0x0094_80ba);
}

/// Text in `fg` on `bg`, bold or not.
const fn chrome(fg: Rgb, bg: Option<Rgb>, attrs: Attrs) -> Style {
    Style {
        fg: Color::Rgb(fg),
        bg: match bg {
            Some(bg) => Color::Rgb(bg),
            None => Color::Default,
        },
        attrs,
        ..Style::DEFAULT
    }
}

// The status line draws like Zellij's compact bar with `simplified_ui`
// (default-plugins/bars/src/compact_bar): the mode as bold coloured text,
// then the tabs as bold ribbons side by side, no separators. The colours
// are the user's theme's components, named after them.

/// The bar (`text_unselected`: base on background).
const BAR: Style = chrome(cendre::FG_DIM, Some(cendre::BG1), Attrs::empty());
/// The mode: `emphasis_2` when normal, `emphasis_3` when locked,
/// `emphasis_0` in every other mode.
const MODE_NORMAL: Style = chrome(cendre::SAP, Some(cendre::BG1), Attrs::BOLD);
const MODE_LOCKED: Style = chrome(cendre::POTASSIUM, Some(cendre::BG1), Attrs::BOLD);
const MODE_OTHER: Style = chrome(cendre::EMBER, Some(cendre::BG1), Attrs::BOLD);
/// The active tab (`ribbon_selected`).
const TAB_ACTIVE: Style = chrome(cendre::BG1, Some(cendre::SAP), Attrs::BOLD);
/// The other tabs (`ribbon_unselected`), every second one on `emphasis_1`
/// so neighbours stay apart without separators.
const TAB: Style = chrome(cendre::BG1, Some(cendre::FG), Attrs::BOLD);
const TAB_ALTERNATE: Style = chrome(cendre::BG1, Some(cendre::FG_DIM), Attrs::BOLD);
/// The search needle, then its count and options in the bar's colours.
const SEARCH_NEEDLE: Style = chrome(cendre::FG, Some(cendre::BG1), Attrs::BOLD);
/// A separator.
pub const BORDER: Style = chrome(cendre::FG_DIM, None, Attrs::empty());
/// The stretches of a separator that border the focused pane.
pub const BORDER_FOCUSED: Style = chrome(cendre::SAP, None, Attrs::empty());

/// A separator line: `│` between panes side by side, `─` between stacked
/// ones, joined where lines meet (`├`, `┬`, `┼`, ...); `all` is every
/// separator. Where it borders the focused pane it is drawn in the focus
/// colour.
fn draw_separator(
    out: &mut Vec<u8>,
    pen: &mut Pen,
    clip: Clip,
    separator: &Separator,
    all: &[Separator],
    focus: Option<Rect>,
) {
    let rect = separator.rect;
    let left = u32::from(rect.x());
    let right = rect.right().min(clip.cols);
    let bottom = rect.bottom().min(clip.rows);
    if left >= right {
        return;
    }
    let borders_focus = |x: u32, y: u32| {
        focus.is_some_and(|focus| {
            let (fx, fy) = (u32::from(focus.x()), u32::from(focus.y()));
            match separator.axis {
                // A column between panes side by side.
                Axis::Horizontal => {
                    (x == focus.right() || x + 1 == fx) && (fy..focus.bottom()).contains(&y)
                }
                // A row between stacked panes.
                Axis::Vertical => {
                    (y == focus.bottom() || y + 1 == fy) && (fx..focus.right()).contains(&x)
                }
            }
        })
    };
    for y in u32::from(rect.y())..bottom {
        move_to(out, left, y);
        for x in left..right {
            let style = if borders_focus(x, y) {
                BORDER_FOCUSED
            } else {
                BORDER
            };
            pen.set(out, style);
            out.extend_from_slice(separator_glyph(all, clip, separator.axis, x, y).as_bytes());
        }
    }
}

/// The character for the cell at (`x`, `y`) of a separator along `axis`:
/// its line, joined with the lines of `separators` that meet it there
/// (within `clip`: what is cut off joins nothing).
fn separator_glyph(
    separators: &[Separator],
    clip: Clip,
    axis: Axis,
    x: u32,
    y: u32,
) -> &'static str {
    let at = |x: u32, y: u32| {
        if x >= clip.cols || y >= clip.rows {
            return None;
        }
        separators
            .iter()
            .find(|separator| separator.rect.contains(x, y))
            .map(|separator| separator.axis)
    };
    // `│` lines run between panes side by side, `─` between stacked ones.
    let column = |found: Option<Axis>| found == Some(Axis::Horizontal);
    let row = |found: Option<Axis>| found == Some(Axis::Vertical);
    let above = y.checked_sub(1).map(|up| at(x, up));
    let below = at(x, y + 1);
    let before = x.checked_sub(1).map(|left| at(left, y));
    let after = at(x + 1, y);
    // A line continues along itself into any separator; across it, only
    // into a line of the other kind.
    let (up, down, left, right) = match axis {
        Axis::Horizontal => (
            above.flatten().is_some(),
            below.is_some(),
            before.is_some_and(row),
            row(after),
        ),
        Axis::Vertical => (
            above.is_some_and(column),
            column(below),
            before.flatten().is_some(),
            after.is_some(),
        ),
    };
    match (up, down, left, right) {
        (true, true, true, true) => "┼",
        (true, true, false, true) => "├",
        (true, true, true, false) => "┤",
        (false, true, true, true) => "┬",
        (true, false, true, true) => "┴",
        (false, true, false, true) => "┌",
        (false, true, true, false) => "┐",
        (true, false, false, true) => "└",
        (true, false, true, false) => "┘",
        // A line meeting nothing across it.
        _ => match axis {
            Axis::Horizontal => "│",
            Axis::Vertical => "─",
        },
    }
}

/// The status line below the panes: the mode, then the tabs, the active one
/// highlighted. A session of one row has none.
fn draw_status(
    out: &mut Vec<u8>,
    pen: &mut Pen,
    clip: Clip,
    mirror: &Mirror,
    header: &FrameHeader,
    labels: &mut TabLabels,
) {
    labels.row = None;
    labels.spans.clear();
    let size = header.size;
    let row = u32::from(pane_area(size).rows());
    if row >= u32::from(size.rows().get()) || row >= clip.rows {
        return;
    }
    labels.row = Some(row);
    let mut status = Status {
        out,
        pen,
        row,
        x: 0,
        limit: clip.cols,
    };
    let mode = match header.mode {
        Mode::Normal => MODE_NORMAL,
        Mode::Locked => MODE_LOCKED,
        Mode::Pane
        | Mode::Tab
        | Mode::Resize
        | Mode::Move
        | Mode::Scroll
        | Mode::Session
        | Mode::EnterSearch
        | Mode::Search => MODE_OTHER,
    };
    status.segment(mode, &[" ", mode_name(header.mode), " "]);
    // How far back the focused pane's view is, as Zellij shows it.
    let scroll = header.scroll;
    if scroll.back > 0 || header.mode.scrolling() == Scrolling::Yes {
        let back = Decimal::new(scroll.back);
        let history = Decimal::new(scroll.history);
        status.segment(mode, &[back.as_str(), "/", history.as_str(), " "]);
    }
    if header.mode.searching() == Searching::Yes
        && let Some((needle, search)) = mirror.search_or_none()
    {
        draw_search(&mut status, header.mode, needle, search);
    }
    for (position, tab) in (1_u32..).zip(mirror.tabs()) {
        let style = if position == u32::from(header.active) + 1 {
            TAB_ACTIVE
        } else if position % 2 == 0 {
            TAB_ALTERNATE
        } else {
            TAB
        };
        // An unnamed tab is called as Zellij calls it, and a tab a pane
        // fills is marked as Zellij marks it.
        let number = Decimal::new(position);
        let zoomed = match tab.zoom {
            Zoom::Tiled => "",
            Zoom::Zoomed => " (FULLSCREEN)",
        };
        let start = status.x;
        match &tab.label {
            TabLabel::Unnamed => {
                status.segment(style, &[" Tab #", number.as_str(), zoomed, " "]);
            }
            TabLabel::Named(name) => status.segment(style, &[" ", name.as_str(), zoomed, " "]),
        }
        labels.spans.push(start..status.x);
    }
    status.fill(BAR);
}

/// The find bar: the needle (with a cursor while it is typed), which match
/// of how many is selected, and the options that are on.
fn draw_search(status: &mut Status<'_>, mode: Mode, needle: &Needle, search: SearchStatus) {
    let cursor = if mode == Mode::EnterSearch { "_" } else { "" };
    status.segment(SEARCH_NEEDLE, &[" /", needle.as_str(), cursor, " "]);
    let total = Decimal::new(search.total);
    // A count waiting for the next search key, while output streams in.
    let about = match search.count {
        Count::Current => "",
        Count::Stale => "~",
    };
    match (search.total, search.selected) {
        (0, _) if needle.as_str().is_empty() => {}
        (0, _) => status.segment(BAR, &[about, "no matches "]),
        (_, Some(selected)) => {
            let selected = Decimal::new(selected.get());
            status.segment(BAR, &[about, selected.as_str(), "/", total.as_str(), " "]);
        }
        (_, None) => status.segment(BAR, &[about, total.as_str(), " found "]),
    }
    if search.options.case == Case::Matched {
        status.segment(BAR, &["[case] "]);
    }
    if search.options.words == Words::Whole {
        status.segment(BAR, &["[word] "]);
    }
}

/// The status line being drawn, left to right.
struct Status<'a> {
    out: &'a mut Vec<u8>,
    pen: &'a mut Pen,
    row: u32,
    /// The next column.
    x: u32,
    limit: u32,
}

impl Status<'_> {
    /// Draws `parts` in `style` from the next column, placed absolutely so a
    /// width the terminal measures differently does not shift the rest;
    /// cut at the limit.
    #[track_caller]
    fn segment(&mut self, style: Style, parts: &[&str]) {
        if self.x >= self.limit {
            return;
        }
        move_to(self.out, self.x, self.row);
        self.pen.set(self.out, style);
        for c in parts.iter().flat_map(|part| part.chars()) {
            let width = c.width().or_bug("status text holds no control characters");
            let width = u32::try_from(width).or_bug("a character is at most two columns");
            if self.x + width > self.limit {
                // Whatever is cut is blank, so the fill covers it.
                return;
            }
            push_char(self.out, c);
            self.x += width;
        }
    }

    /// Fills the rest of the line with blanks in `style`.
    fn fill(&mut self, style: Style) {
        if self.x >= self.limit {
            return;
        }
        move_to(self.out, self.x, self.row);
        self.pen.set(self.out, style);
        for _ in self.x..self.limit {
            self.out.push(b' ');
        }
        self.x = self.limit;
    }
}

const fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Locked => "LOCKED",
        Mode::Normal => "NORMAL",
        Mode::Pane => "PANE",
        Mode::Tab => "TAB",
        Mode::Resize => "RESIZE",
        Mode::Move => "MOVE",
        Mode::Scroll => "SCROLL",
        Mode::Session => "SESSION",
        Mode::EnterSearch | Mode::Search => "SEARCH",
    }
}

/// The style the terminal is drawing with; a change emits one SGR.
#[derive(Debug)]
struct Pen {
    capabilities: Capabilities,
    style: Style,
}

impl Pen {
    fn set(&mut self, out: &mut Vec<u8>, style: Style) {
        if style == self.style {
            return;
        }
        self.style = style;
        // Reset, then set everything: one sequence, no state carried over.
        out.extend_from_slice(b"\x1b[0");
        let attrs = [
            (Attrs::BOLD, 1),
            (Attrs::FAINT, 2),
            (Attrs::ITALIC, 3),
            (Attrs::BLINK, 5),
            (Attrs::INVERSE, 7),
            (Attrs::INVISIBLE, 8),
            (Attrs::STRIKETHROUGH, 9),
            (Attrs::OVERLINE, 53),
        ];
        for (attr, code) in attrs {
            if style.attrs.contains(attr) {
                param(out, code);
            }
        }
        let styled = self.capabilities.underlines == UnderlineStyles::Styled;
        match (style.underline, styled) {
            (Underline::None, _) => {}
            (Underline::Single, _)
            | (
                Underline::Double | Underline::Curly | Underline::Dotted | Underline::Dashed,
                false,
            ) => param(out, 4),
            (Underline::Double, true) => out.extend_from_slice(b";4:2"),
            (Underline::Curly, true) => out.extend_from_slice(b";4:3"),
            (Underline::Dotted, true) => out.extend_from_slice(b";4:4"),
            (Underline::Dashed, true) => out.extend_from_slice(b";4:5"),
        }
        let depth = self.capabilities.color;
        color(out, depth, style.fg, Layer::Foreground);
        color(out, depth, style.bg, Layer::Background);
        // Set even without an underline: the terminal then holds exactly
        // the style the program set.
        if styled {
            color(out, depth, style.underline_color, Layer::Underline);
        }
        out.push(b'm');
    }
}

/// Which colour an SGR colour parameter sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layer {
    Foreground,
    Background,
    Underline,
}

/// The nearest colour to `color` that a terminal of `depth` shows.
///
/// RGB becomes the nearest of the 256-colour cube and grey ramp, and either
/// becomes the nearest of the 16 ANSI colours (by xterm's default palette).
#[must_use]
pub fn downgrade(color: Color, depth: ColorDepth) -> Color {
    match (color, depth) {
        (Color::Default, _)
        | (Color::Palette(_), ColorDepth::TrueColor | ColorDepth::Palette256)
        | (Color::Rgb(_), ColorDepth::TrueColor) => color,
        (Color::Rgb(rgb), ColorDepth::Palette256) => Color::Palette(nearest_256(rgb)),
        (Color::Palette(index), ColorDepth::Ansi16) if index < 16 => color,
        (Color::Palette(index), ColorDepth::Ansi16) => {
            Color::Palette(nearest_16(palette_rgb(index)))
        }
        (Color::Rgb(rgb), ColorDepth::Ansi16) => Color::Palette(nearest_16(rgb)),
    }
}

/// Appends the parameters that set `layer` to `color`, as near as `depth`
/// allows. The default colour needs none: the pen was reset.
fn color(out: &mut Vec<u8>, depth: ColorDepth, color: Color, layer: Layer) {
    let (base, bright, extended) = match layer {
        Layer::Foreground => (30, 90, 38),
        Layer::Background => (40, 100, 48),
        Layer::Underline => (0, 0, 58),
    };
    match downgrade(color, depth) {
        Color::Default => {}
        Color::Palette(index) if index < 8 && layer != Layer::Underline => {
            param(out, base + u32::from(index));
        }
        Color::Palette(index) if index < 16 && layer != Layer::Underline => {
            param(out, bright + u32::from(index - 8));
        }
        Color::Palette(index) => {
            param(out, extended);
            param(out, 5);
            param(out, u32::from(index));
        }
        Color::Rgb(Rgb { r, g, b }) => {
            param(out, extended);
            param(out, 2);
            param(out, u32::from(r));
            param(out, u32::from(g));
            param(out, u32::from(b));
        }
    }
}

/// xterm's default 16 colours.
const ANSI: [Rgb; 16] = [
    Rgb { r: 0, g: 0, b: 0 },
    Rgb { r: 205, g: 0, b: 0 },
    Rgb { r: 0, g: 205, b: 0 },
    Rgb {
        r: 205,
        g: 205,
        b: 0,
    },
    Rgb { r: 0, g: 0, b: 238 },
    Rgb {
        r: 205,
        g: 0,
        b: 205,
    },
    Rgb {
        r: 0,
        g: 205,
        b: 205,
    },
    Rgb {
        r: 229,
        g: 229,
        b: 229,
    },
    Rgb {
        r: 127,
        g: 127,
        b: 127,
    },
    Rgb { r: 255, g: 0, b: 0 },
    Rgb { r: 0, g: 255, b: 0 },
    Rgb {
        r: 255,
        g: 255,
        b: 0,
    },
    Rgb {
        r: 92,
        g: 92,
        b: 255,
    },
    Rgb {
        r: 255,
        g: 0,
        b: 255,
    },
    Rgb {
        r: 0,
        g: 255,
        b: 255,
    },
    Rgb {
        r: 255,
        g: 255,
        b: 255,
    },
];

/// The six channel levels of the 6×6×6 colour cube (indices 16 to 231).
const fn cube_level(step: u8) -> u8 {
    if step == 0 { 0 } else { 55 + 40 * step }
}

/// The grey of the 24-step ramp (indices 232 to 255).
const fn grey_level(step: u8) -> u8 {
    8 + 10 * step
}

/// The xterm default colour of a palette index.
#[track_caller]
fn palette_rgb(index: u8) -> Rgb {
    match index {
        0..16 => *ANSI
            .get(usize::from(index))
            .or_bug("the 16 ANSI colours are tabled"),
        16..232 => {
            let cube = index - 16;
            Rgb {
                r: cube_level(cube / 36),
                g: cube_level(cube / 6 % 6),
                b: cube_level(cube % 6),
            }
        }
        232..=255 => {
            let grey = grey_level(index - 232);
            Rgb {
                r: grey,
                g: grey,
                b: grey,
            }
        }
    }
}

/// Squared distance between two colours.
fn distance(a: Rgb, b: Rgb) -> u32 {
    let channel = |a: u8, b: u8| u32::from(a.abs_diff(b)).pow(2);
    channel(a.r, b.r) + channel(a.g, b.g) + channel(a.b, b.b)
}

/// The cube step nearest to a channel value.
#[track_caller]
fn nearest_step(value: u8) -> u8 {
    (0..6_u8)
        .min_by_key(|&step| cube_level(step).abs_diff(value))
        .or_bug("the cube has six steps")
}

/// The nearest colour of the cube and grey ramp (the 16 ANSI colours are
/// left out: terminals theme them).
#[track_caller]
fn nearest_256(rgb: Rgb) -> u8 {
    let (r, g, b) = (
        nearest_step(rgb.r),
        nearest_step(rgb.g),
        nearest_step(rgb.b),
    );
    let cube = 16 + 36 * r + 6 * g + b;
    let grey = (0..24_u8)
        .map(|step| 232 + step)
        .min_by_key(|&index| distance(palette_rgb(index), rgb))
        .or_bug("the ramp has 24 greys");
    if distance(palette_rgb(grey), rgb) < distance(palette_rgb(cube), rgb) {
        grey
    } else {
        cube
    }
}

/// The nearest of the 16 ANSI colours.
#[track_caller]
fn nearest_16(rgb: Rgb) -> u8 {
    (0..16_u8)
        .min_by_key(|&index| distance(palette_rgb(index), rgb))
        .or_bug("there are 16 ANSI colours")
}

/// Appends `;n`.
fn param(out: &mut Vec<u8>, n: u32) {
    out.push(b';');
    out.extend_from_slice(Decimal::new(n).as_bytes());
}

/// Moves the cursor to zero-based column `x`, row `y` (CUP).
fn move_to(out: &mut Vec<u8>, x: u32, y: u32) {
    out.extend_from_slice(b"\x1b[");
    out.extend_from_slice(Decimal::new(y + 1).as_bytes());
    out.push(b';');
    out.extend_from_slice(Decimal::new(x + 1).as_bytes());
    out.push(b'H');
}

fn push_char(out: &mut Vec<u8>, c: char) {
    // Most text is ASCII: one push, not a copy of an encoded slice.
    match u8::try_from(c) {
        Ok(byte) if byte.is_ascii() => out.push(byte),
        Ok(_) | Err(_) => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
    }
}

/// A number's decimal digits, formatted without `core::fmt`.
struct Decimal {
    digits: [u8; 20],
    start: usize,
}

impl Decimal {
    fn new(n: impl Into<u64>) -> Self {
        let mut n: u64 = n.into();
        let mut digits = [b'0'; 20];
        let mut start = digits.len();
        loop {
            start -= 1;
            let digit = u8::try_from(n % 10).or_bug("a decimal digit fits u8");
            if let Some(slot) = digits.get_mut(start) {
                *slot = b'0' + digit;
            }
            n /= 10;
            if n == 0 {
                break;
            }
        }
        Self { digits, start }
    }

    /// The digits as bytes, for output that is bytes anyway: no UTF-8
    /// check per number written.
    #[track_caller]
    fn as_bytes(&self) -> &[u8] {
        self.digits
            .get(self.start..)
            .or_bug("digits are written from start")
    }

    #[track_caller]
    fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).or_bug("decimal digits are ASCII")
    }
}
