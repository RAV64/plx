//! The client's mirror of what the daemon shows.
//!
//! It holds the chrome (size, mode, tabs, arrangement) and, per visible
//! pane, its grid, styles, clusters and cursor. Frames apply to it in place
//! ([`FrameApply`]); it records what changed since the compositor last
//! drew, so drawing is proportional to damage.
//!
//! The daemon is trusted to be plx, not to be bug-free: every row and id in
//! a frame is checked against the mirror's state, and a frame that does not
//! fit is refused with a reason rather than drawn wrong. Rows are decoded
//! straight into the grid and checked there, so a mirror that refused a
//! frame is discarded (the client core is consumed with it). Nothing in a frame
//! can reach the client's terminal as a command: cells hold no control
//! characters.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use plx_bug::OrBug as _;
use plx_collections::IdMap;
use plx_core::geometry::Rect;
use plx_core::id::PaneId;
use plx_core::layout::{Placement, Separator};
use plx_core::message::FrameSeq;
use plx_core::search::{ControlCharacter, Needle};
use plx_core::tab::{TabName, Zoom};
use plx_emulator::{
    Cell, Change, Cursor, GlyphKind, GraphemeId, Progress, SearchStatus, Style, StyleId,
};
use plx_protocol::server::{FrameApply, FrameHeader, FrameSearch, Refused};

/// A tab as the daemon sent it.
#[derive(Debug, PartialEq, Eq)]
pub struct MirrorTab {
    pub label: TabLabel,
    /// Whether a pane fills it (the status line says so).
    pub zoom: Zoom,
}

/// A tab's label as the daemon sent it.
#[derive(Debug, PartialEq, Eq)]
pub enum TabLabel {
    /// Shown by its number.
    Unnamed,
    Named(TabName),
}

/// Whether the last frame showed a pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shown {
    Yes,
    No,
}

/// One visible pane's mirror.
#[derive(Debug)]
pub struct MirrorPane {
    rect: Rect,
    /// `rows × cols` of `rect`'s size, row-major.
    cells: Box<[Cell]>,
    /// By style id; `None` for an id the daemon has not defined. Id 0 is the
    /// default style.
    styles: Vec<Option<Style>>,
    /// By cluster id.
    graphemes: Vec<Option<Box<[char]>>>,
    cursor: Cursor,
    /// Rows changed since the compositor last drew this pane.
    damage: Box<[Change]>,
    cursor_damage: Change,
    shown: Shown,
}

impl MirrorPane {
    fn new(rect: Rect, cursor: Cursor) -> Self {
        let size = rect.size();
        Self {
            rect,
            cells: vec![Cell::BLANK; size.area()].into_boxed_slice(),
            styles: Vec::new(),
            graphemes: Vec::new(),
            cursor,
            damage: vec![Change::Changed; usize::from(size.rows().get())].into_boxed_slice(),
            cursor_damage: Change::Changed,
            shown: Shown::Yes,
        }
    }

    #[must_use]
    pub const fn rect(&self) -> Rect {
        self.rect
    }

    #[must_use]
    pub const fn cursor(&self) -> Cursor {
        self.cursor
    }

    fn cols(&self) -> usize {
        usize::from(self.rect.cols())
    }

    /// Row `y`; `None` past the last row.
    #[must_use]
    pub fn row_or_none(&self, y: u16) -> Option<&[Cell]> {
        let cols = self.cols();
        let start = usize::from(y) * cols;
        self.cells.get(start..start + cols)
    }

    /// The rows changed since the compositor last drew them, with their
    /// index.
    pub fn damaged_rows(&self) -> impl Iterator<Item = (u16, &[Cell])> + '_ {
        self.cells
            .chunks_exact(self.cols())
            .zip(self.damage.iter())
            .zip(0_u16..)
            .filter(|((_, damage), _)| **damage == Change::Changed)
            .map(|((row, _), y)| (y, row))
    }

    /// The style a cell of this pane refers to (checked when its row came).
    #[must_use]
    #[track_caller]
    pub fn style(&self, id: StyleId) -> Style {
        if id == StyleId::DEFAULT {
            return Style::DEFAULT;
        }
        *lookup(&self.styles, id.get()).or_bug("a mirrored cell's style was defined before its row")
    }

    /// The cluster a cell of this pane refers to (checked when its row came).
    #[must_use]
    #[track_caller]
    pub fn grapheme(&self, id: GraphemeId) -> &[char] {
        lookup(&self.graphemes, id.get())
            .or_bug("a mirrored cell's cluster was defined before its row")
    }

    #[must_use]
    pub const fn cursor_damage(&self) -> Change {
        self.cursor_damage
    }

    /// Forgets the damage: the compositor drew it.
    pub fn clear_damage(&mut self) {
        self.damage.fill(Change::Unchanged);
        self.cursor_damage = Change::Unchanged;
    }

    /// The most ids a table of this pane can need: one per cell plus a row
    /// in flight (the daemon reuses freed ids).
    fn id_limit(&self) -> u32 {
        let size = self.rect.size();
        u32::try_from(size.area() + usize::from(size.cols().get()))
            .or_bug("a grid's cells, plus a row, fit u32")
    }

    fn defines_style(&self, id: StyleId) -> bool {
        id == StyleId::DEFAULT || lookup(&self.styles, id.get()).is_some()
    }

    fn defines_grapheme(&self, id: GraphemeId) -> bool {
        lookup(&self.graphemes, id.get()).is_some()
    }
}

/// The entry for `id`, if it is defined.
fn lookup<T>(table: &[Option<T>], id: u32) -> Option<&T> {
    usize::try_from(id)
        .map_or(None, |index| table.get(index))
        .and_then(Option::as_ref)
}

/// Sets `table[id] = value`, growing the table as needed.
fn define<T>(table: &mut Vec<Option<T>>, id: u32, value: T) -> Result<(), Refused> {
    let index = usize::try_from(id).map_err(|_overflow| Refused("an id past the address space"))?;
    if table.len() <= index {
        table.resize_with(index + 1, || None);
    }
    let slot = table
        .get_mut(index)
        .ok_or(Refused("a table entry past its end"))?;
    *slot = Some(value);
    Ok(())
}

/// The client's mirror. Starts empty; the first frame fills it.
#[derive(Debug, Default)]
pub struct Mirror {
    header: Option<FrameHeader>,
    tabs: Vec<MirrorTab>,
    tab_count: usize,
    placements: Vec<Placement>,
    placement_count: usize,
    separators: Vec<Separator>,
    separator_count: usize,
    panes: IdMap<PaneId, MirrorPane>,
    /// The pane whose update is being applied.
    current: Option<PaneId>,
    /// Size, mode, tabs or arrangement changed since the compositor drew.
    chrome_damage: Change,
    /// Text the client selected (UTF-8), until the client puts it on the
    /// clipboard.
    clipboard: Vec<u8>,
    copied: Change,
    /// The search going on, if any, and its needle (kept while none is).
    search: Option<SearchStatus>,
    needle: Needle,
    /// The last progress report frames carried, until the client passes
    /// it on.
    reported: Option<Progress>,
}

impl Mirror {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            header: None,
            tabs: Vec::new(),
            tab_count: 0,
            placements: Vec::new(),
            placement_count: 0,
            separators: Vec::new(),
            separator_count: 0,
            panes: IdMap::new(),
            current: None,
            chrome_damage: Change::Unchanged,
            clipboard: Vec::new(),
            copied: Change::Unchanged,
            search: None,
            needle: Needle::new(),
            reported: None,
        }
    }

    /// The last frame's fixed part; `None` before the first frame.
    #[must_use]
    pub const fn header(&self) -> Option<&FrameHeader> {
        self.header.as_ref()
    }

    /// The last frame's number, to acknowledge.
    #[must_use]
    pub fn seq_or_none(&self) -> Option<FrameSeq> {
        self.header.map(|header| header.seq)
    }

    pub fn tabs(&self) -> impl Iterator<Item = &MirrorTab> {
        self.tabs.iter().take(self.tab_count)
    }

    #[must_use]
    #[track_caller]
    pub fn placements(&self) -> &[Placement] {
        self.placements
            .get(..self.placement_count)
            .or_bug("placements are stored as they are counted")
    }

    #[must_use]
    #[track_caller]
    pub fn separators(&self) -> &[Separator] {
        self.separators
            .get(..self.separator_count)
            .or_bug("separators are stored as they are counted")
    }

    /// The search going on, if any: its needle and status.
    #[must_use]
    pub fn search_or_none(&self) -> Option<(&Needle, SearchStatus)> {
        self.search.map(|status| (&self.needle, status))
    }

    /// Text copied since the last call, for the clipboard.
    pub fn copied_or_none(&mut self) -> Option<&[u8]> {
        match self.copied {
            Change::Changed => {
                self.copied = Change::Unchanged;
                Some(&self.clipboard)
            }
            Change::Unchanged => None,
        }
    }

    /// The progress report frames carried since the last call, the latest
    /// if several did, for the terminal.
    pub const fn reported_or_none(&mut self) -> Option<Progress> {
        self.reported.take()
    }

    /// The visible panes, in id order.
    pub fn panes(&self) -> impl Iterator<Item = (PaneId, &MirrorPane)> {
        self.panes.iter()
    }

    /// A visible pane; `None` for one the last frame did not show.
    #[must_use]
    pub fn pane_or_none(&self, pane: PaneId) -> Option<&MirrorPane> {
        self.panes.get_or_none(pane)
    }

    #[must_use]
    pub const fn chrome_damage(&self) -> Change {
        self.chrome_damage
    }

    /// Forgets all damage: the compositor drew it.
    pub fn clear_damage(&mut self) {
        self.chrome_damage = Change::Unchanged;
        for (_, pane) in self.panes.iter_mut() {
            pane.clear_damage();
        }
    }

    /// Marks everything damaged, for a full redraw (attach, the client's
    /// terminal resized or was resumed).
    pub fn damage_all(&mut self) {
        self.chrome_damage = Change::Changed;
        for (_, pane) in self.panes.iter_mut() {
            pane.damage.fill(Change::Changed);
            pane.cursor_damage = Change::Changed;
        }
    }

    fn current_pane(&mut self) -> Result<&mut MirrorPane, Refused> {
        let pane = self
            .current
            .ok_or(Refused("a pane part outside a pane update"))?;
        self.panes
            .get_mut_or_none(pane)
            .ok_or(Refused("a pane part for a pane not begun"))
    }

    /// Stores `value` at `index` of a reused list, marking chrome damage if
    /// it differs.
    fn store<T: PartialEq>(list: &mut Vec<T>, index: usize, value: T, damage: &mut Change) {
        match list.get_mut(index) {
            Some(existing) if *existing == value => {}
            Some(existing) => {
                *existing = value;
                *damage = Change::Changed;
            }
            None => {
                list.push(value);
                *damage = Change::Changed;
            }
        }
    }
}

impl FrameApply for Mirror {
    fn begin(
        &mut self,
        header: FrameHeader,
        search: Option<FrameSearch<'_>>,
    ) -> Result<(), Refused> {
        if let Some(previous) = self.header
            && previous.seq >= header.seq
        {
            return Err(Refused("frames arrive in order"));
        }
        let needle = match search {
            Some(search) => {
                self.needle
                    .set(search.needle.as_str())
                    .map_err(|ControlCharacter| {
                        Refused("a search needle holds no control characters")
                    })?
            }
            None => Change::Unchanged,
        };
        let status = search.map(|search| search.status);
        let changed = self.header.is_none_or(|previous| {
            previous.size != header.size
                || previous.mode != header.mode
                || previous.active != header.active
                || previous.focus != header.focus
                || previous.scroll != header.scroll
        }) || self.search != status
            || needle == Change::Changed;
        self.search = status;
        if changed {
            self.chrome_damage = Change::Changed;
        }
        if let Some(progress) = header.progress {
            self.reported = Some(progress);
        }
        self.header = Some(header);
        self.tab_count = 0;
        self.placement_count = 0;
        self.separator_count = 0;
        for (_, pane) in self.panes.iter_mut() {
            pane.shown = Shown::No;
        }
        Ok(())
    }

    fn tab(&mut self, name: Option<&str>, zoom: Zoom) -> Result<(), Refused> {
        let index = self.tab_count;
        self.tab_count += 1;
        let existing = self.tabs.get(index);
        let unchanged = existing.is_some_and(|tab| tab.zoom == zoom)
            && (matches!(
                (existing.map(|tab| &tab.label), name),
                (Some(TabLabel::Unnamed), None)
            ) || matches!(
                (existing.map(|tab| &tab.label), name),
                (Some(TabLabel::Named(existing)), Some(name)) if existing.as_str() == name
            ));
        if unchanged {
            return Ok(());
        }
        // A name is parsed like any other input: only when it changed.
        let label = match name {
            None => TabLabel::Unnamed,
            Some(name) => TabLabel::Named(
                TabName::new(name)
                    .map_err(|_invalid| Refused("a tab name the daemon could not have"))?,
            ),
        };
        let tab = MirrorTab { label, zoom };
        match self.tabs.get_mut(index) {
            Some(existing) => *existing = tab,
            None => self.tabs.push(tab),
        }
        self.chrome_damage = Change::Changed;
        Ok(())
    }

    fn placement(&mut self, placement: Placement) -> Result<(), Refused> {
        let index = self.placement_count;
        self.placement_count += 1;
        Self::store(
            &mut self.placements,
            index,
            placement,
            &mut self.chrome_damage,
        );
        Ok(())
    }

    fn separator(&mut self, separator: Separator) -> Result<(), Refused> {
        let index = self.separator_count;
        self.separator_count += 1;
        Self::store(
            &mut self.separators,
            index,
            separator,
            &mut self.chrome_damage,
        );
        Ok(())
    }

    fn pane(&mut self, pane: PaneId, rect: Rect, cursor: Cursor) -> Result<(), Refused> {
        let placed = self
            .placements()
            .iter()
            .any(|placement| placement.pane == pane && placement.rect == Some(rect));
        if !placed {
            return Err(Refused("a pane update for a pane not shown there"));
        }
        match self.panes.get_mut_or_none(pane) {
            // A pane that grew or shrank was redrawn whole by the daemon
            // (fresh tables, every row resent): start it afresh.
            Some(existing) if existing.rect.size() == rect.size() => {
                if existing.rect != rect {
                    existing.rect = rect;
                    existing.damage.fill(Change::Changed);
                }
                if existing.cursor != cursor {
                    existing.cursor = cursor;
                    existing.cursor_damage = Change::Changed;
                }
                existing.shown = Shown::Yes;
            }
            Some(existing) => *existing = MirrorPane::new(rect, cursor),
            None => self.panes.insert(pane, MirrorPane::new(rect, cursor)),
        }
        self.current = Some(pane);
        Ok(())
    }

    fn style(&mut self, id: StyleId, style: Style) -> Result<(), Refused> {
        let pane = self.current_pane()?;
        if id == StyleId::DEFAULT || id.get() > pane.id_limit() {
            return Err(Refused("a style id outside the pane's range"));
        }
        define(&mut pane.styles, id.get(), style)
    }

    fn grapheme(&mut self, id: GraphemeId, codepoints: &[char]) -> Result<(), Refused> {
        let pane = self.current_pane()?;
        if id.get() > pane.id_limit() {
            return Err(Refused("a cluster id outside the pane's range"));
        }
        if codepoints.len() < 2 {
            return Err(Refused("a cluster of fewer than two codepoints"));
        }
        if codepoints.iter().copied().any(char::is_control) {
            return Err(Refused("a control character in a cluster"));
        }
        match lookup(&pane.graphemes, id.get()) {
            Some(existing) if existing.as_ref() == codepoints => Ok(()),
            _ => define(&mut pane.graphemes, id.get(), codepoints.into()),
        }
    }

    fn row_slot(&mut self, y: u16) -> Result<&mut [Cell], Refused> {
        let pane = self.current_pane()?;
        let cols = pane.cols();
        let start = usize::from(y) * cols;
        pane.cells
            .get_mut(start..start + cols)
            .ok_or(Refused("a row within its pane"))
    }

    fn row_filled(&mut self, y: u16) -> Result<(), Refused> {
        let pane = self.current_pane()?;
        let row = pane
            .row_or_none(y)
            .ok_or(Refused("a row within its pane"))?;
        for cell in row {
            if !pane.defines_style(cell.style) {
                return Err(Refused("a cell's style is defined first"));
            }
            match cell.glyph.kind() {
                // Drawn as is: a control character would reach the client's
                // terminal as a command.
                GlyphKind::Char(c) if c.is_control() => {
                    return Err(Refused("a control character in a cell"));
                }
                GlyphKind::Cluster(id) if !pane.defines_grapheme(id) => {
                    return Err(Refused("a cell's cluster is defined first"));
                }
                GlyphKind::Empty | GlyphKind::Char(_) | GlyphKind::Cluster(_) => {}
            }
        }
        if let Some(damage) = pane.damage.get_mut(usize::from(y)) {
            *damage = Change::Changed;
        }
        Ok(())
    }

    fn end(&mut self) -> Result<(), Refused> {
        self.current = None;
        if self.tabs.len() > self.tab_count {
            self.tabs.truncate(self.tab_count);
            self.chrome_damage = Change::Changed;
        }
        if self.placements.len() > self.placement_count {
            self.placements.truncate(self.placement_count);
            self.chrome_damage = Change::Changed;
        }
        if self.separators.len() > self.separator_count {
            self.separators.truncate(self.separator_count);
            self.chrome_damage = Change::Changed;
        }
        // Panes the frame did not show are dropped; the daemon forgets them
        // too once this frame is acknowledged, and sends them whole when
        // they are shown again.
        self.panes.retain(|_, pane| pane.shown == Shown::Yes);
        Ok(())
    }

    fn clipboard(&mut self, text: &[u8]) -> Result<(), Refused> {
        self.clipboard.clear();
        self.clipboard.extend_from_slice(text);
        self.copied = Change::Changed;
        Ok(())
    }
}
