//! What clients and the session say to each other, as the core sees it.
//! `plx-protocol` encodes these for the wire; the core owns the vocabulary.

use plx_emulator::{
    CellPx, Cursor, GraphemeId, GridSize, KeyEvent, Progress, Screen, Style, StyleId, Version,
};
use serde::{Deserialize, Serialize};

use crate::command::Command;
use crate::event::EndReason;
use crate::geometry::Rect;
use crate::id::PaneId;
use crate::layout::Arrangement;
use crate::mode::Mode;
use crate::structure::Structure;
use crate::tab::Tab;

/// A frame's position in its client's stream. Frames are acknowledged by
/// number, and only one is in flight per client at a time.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct FrameSeq(u64);

impl FrameSeq {
    pub const FIRST: Self = Self(0);

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// A message from a client, parsed (by `plx-protocol`) and lent to the core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientMessage<'a> {
    /// Join the session with this terminal size.
    Attach {
        size: GridSize,
        cell_px: Option<CellPx>,
    },
    Resize {
        size: GridSize,
        cell_px: Option<CellPx>,
    },
    Key(KeyEvent),
    Paste(Paste<'a>),
    /// A command from the CLI or a script.
    Command(&'a Command),
    /// The client has drawn this frame.
    Ack(FrameSeq),
    /// A mouse report, in the client terminal's cells.
    Mouse(plx_emulator::MouseEvent),
}

/// Text a client pasted, lent from its message (on the wire, borrowed from
/// the receive buffer). Any text can be pasted: the pane's emulator
/// neutralises control characters and brackets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paste<'a>(&'a str);

impl<'a> Paste<'a> {
    #[must_use]
    pub const fn new(text: &'a str) -> Self {
        Self(text)
    }

    #[must_use]
    pub const fn text(self) -> &'a str {
        self.0
    }
}

/// A message to a client, lent to the shell's sink, which encodes it.
#[derive(Debug)]
pub enum Outgoing<'a, E> {
    Frame(FrameView<'a, E>),
    Rejected(crate::rejection::Rejection),
    /// The client asked to detach; the connection closes next.
    Detached,
    SessionEnded(EndReason),
    /// A command from a client that is not attached (the CLI) was applied.
    /// An attached client sees its commands' effects in its frames instead.
    Applied,
    /// Text the client selected, for its terminal's clipboard.
    Clipboard(Copied<'a>),
}

/// Selected text on its way to a client's clipboard: UTF-8, lent from the
/// session's buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Copied<'a>(&'a [u8]);

impl<'a> Copied<'a> {
    #[must_use]
    pub const fn new(utf8: &'a [u8]) -> Self {
        Self(utf8)
    }

    #[must_use]
    pub const fn bytes(self) -> &'a [u8] {
        self.0
    }
}

/// What a client is missing, borrowed from the session: nothing is copied
/// per client. The sink walks it while encoding.
#[derive(Debug)]
pub struct FrameView<'a, E> {
    pub(crate) seq: FrameSeq,
    pub(crate) size: GridSize,
    pub(crate) mode: Mode,
    pub(crate) structure: &'a Structure,
    /// The active tab, arranged into the session's pane area.
    pub(crate) arrangement: &'a Arrangement,
    pub(crate) panes: &'a plx_collections::IdMap<PaneId, crate::session::Pane<E>>,
    /// The version of each pane the client has drawn and still holds (the
    /// panes its last acknowledged frame showed); any other pane is sent
    /// whole.
    pub(crate) acked: &'a plx_collections::IdMap<PaneId, Version>,
    pub(crate) search: &'a crate::search::Search,
    /// The focused pane's progress report, if the client has not had it.
    pub(crate) progress: Option<Progress>,
}

impl<'a, E: plx_emulator::Emulator> FrameView<'a, E> {
    #[must_use]
    pub const fn seq(&self) -> FrameSeq {
        self.seq
    }

    /// The session's size; the client draws within it.
    #[must_use]
    pub const fn size(&self) -> GridSize {
        self.size
    }

    /// The client's mode, for the status line.
    #[must_use]
    pub const fn mode(&self) -> Mode {
        self.mode
    }

    /// The tabs, for the tab bar.
    pub fn tabs(&self) -> impl Iterator<Item = &'a Tab> {
        self.structure.tabs()
    }

    #[must_use]
    pub const fn active_position(&self) -> usize {
        self.structure.active_position()
    }

    /// The focused pane of the active tab (its cursor is the one shown).
    #[must_use]
    pub const fn focus(&self) -> PaneId {
        self.structure.active().focus()
    }

    /// Where the focused pane's view is in its history.
    #[must_use]
    pub fn scroll(&self) -> plx_emulator::ScrollPosition {
        self.panes.get(self.focus()).emulator.scroll_position()
    }

    /// The focused pane's progress report, when the client's terminal has
    /// not been told it: a new or repeated report, or the focus moved. A
    /// removal comes only where something was shown.
    #[must_use]
    pub const fn progress_or_none(&self) -> Option<Progress> {
        self.progress
    }

    /// The search going on, if any (a client in a search mode shows it).
    #[must_use]
    pub fn search(&self) -> Option<crate::search::SearchView<'a>> {
        let pane = self.search.pane?;
        let entry = self.panes.get_or_none(pane)?;
        Some(crate::search::SearchView {
            needle: &self.search.needle,
            status: entry.emulator.search_status(),
        })
    }

    /// Where the active tab's panes and separators are.
    #[must_use]
    pub const fn arrangement(&self) -> &'a Arrangement {
        self.arrangement
    }

    /// Each visible pane, with what changed since the client drew it.
    pub fn panes(&self) -> impl Iterator<Item = PaneUpdate<'a>> + '_ {
        self.arrangement.panes.iter().filter_map(|placement| {
            let rect = placement.rect?;
            let pane = self.panes.get(placement.pane);
            // A pane the client has not drawn yet goes whole.
            let since = self
                .acked
                .get_or_none(placement.pane)
                .map_or(Version::ZERO, |version| *version);
            Some(PaneUpdate {
                pane: placement.pane,
                rect,
                since,
                screen: pane.emulator.screen(),
            })
        })
    }
}

/// One visible pane in a frame: what the client must update to draw it.
#[derive(Clone, Copy, Debug)]
pub struct PaneUpdate<'a> {
    pub pane: PaneId,
    pub rect: Rect,
    /// The version the client last drew; [`Version::ZERO`] for a pane it has
    /// never drawn, which sends it whole.
    pub since: Version,
    screen: &'a Screen,
}

impl<'a> PaneUpdate<'a> {
    /// The rows changed since the client drew the pane, with their index.
    pub fn rows(&self) -> impl Iterator<Item = (u16, &'a [plx_emulator::Cell])> + 'a {
        self.screen.grid().rows_newer_than(self.since)
    }

    /// The styles (re)defined since: sent before the rows that use them.
    pub fn styles(&self) -> impl Iterator<Item = (StyleId, &'a Style)> + 'a {
        self.screen.tables().styles_changed_since(self.since)
    }

    /// The grapheme clusters (re)defined since.
    pub fn graphemes(&self) -> impl Iterator<Item = (GraphemeId, &'a [char])> + 'a {
        self.screen.tables().graphemes_changed_since(self.since)
    }

    #[must_use]
    pub const fn cursor(&self) -> Cursor {
        self.screen.grid().cursor()
    }
}
