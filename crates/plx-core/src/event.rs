//! Events: facts about the session, in the past tense.
//!
//! [`StructureEvent`]s change the session's structure (tabs, panes, layout,
//! focus) and carry everything needed to replay them: folding them over a
//! new structure reproduces it exactly. The other [`Event`]s report what
//! happened to the session, its processes and clients.

use core::num::{NonZeroI32, NonZeroU8, NonZeroU32};

use plx_emulator::GridSize;
use serde::{Deserialize, Serialize};

use crate::command::TabSide;
use crate::geometry::Direction;
use crate::id::{ClientId, PaneId, TabId};
use crate::layout::{Ratio, SplitIndex};
use crate::mode::Mode;
use crate::tab::{TabName, Zoom};

/// A change to the structure. Not `Clone`: an event is lent to its consumers
/// and then moved into the structure.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StructureEvent {
    /// A tab with one pane opened after the active tab and became active.
    TabOpened {
        tab: TabId,
        pane: PaneId,
    },
    /// A tab and all its panes closed; `active` is the tab now active.
    TabClosed {
        tab: TabId,
        active: TabId,
    },
    TabRenamed {
        tab: TabId,
        name: TabName,
    },
    TabActivated {
        tab: TabId,
    },
    /// `pane` opened on the `direction` side of `beside` and took focus.
    PaneOpened {
        pane: PaneId,
        tab: TabId,
        beside: PaneId,
        direction: Direction,
    },
    /// `pane` closed and its tab still has panes; `focus` is the tab's
    /// focused pane afterwards.
    PaneClosed {
        pane: PaneId,
        tab: TabId,
        focus: PaneId,
    },
    FocusChanged {
        tab: TabId,
        pane: PaneId,
    },
    /// `pane` grew towards `direction` by `step` thousandths of the nearest
    /// split that could move (replaying runs the same deterministic grow).
    PaneGrown {
        tab: TabId,
        pane: PaneId,
        direction: Direction,
        step: u16,
    },
    /// Panes `a` and `b` exchanged places in their tab's layout.
    /// `cover` took the place of `pane` (an editor on its history); `pane`
    /// runs on hidden, and the focus, if it was there, is on `cover`.
    PaneCovered {
        tab: TabId,
        pane: PaneId,
        cover: PaneId,
    },
    /// `cover` went and `pane`, hidden behind it, is back in its place
    /// (with the focus, if `cover` had it).
    PaneUncovered {
        tab: TabId,
        cover: PaneId,
        pane: PaneId,
    },
    /// `pane`, hidden behind a cover, exited; what it hid, if anything,
    /// is now behind that cover.
    CoveredPaneExited {
        tab: TabId,
        pane: PaneId,
    },
    /// `pane` left tab `from` for tab `to`, as `into` says; `from` keeps
    /// `left_focus`, or closes when `None` (it was its last pane). `to` is
    /// then active, focused on `pane`, which takes along what it covers.
    PaneMovedToTab {
        pane: PaneId,
        from: TabId,
        left_focus: Option<PaneId>,
        to: TabId,
        into: PaneDestination,
    },
    /// The active tab `tab` moved one place towards `side`.
    TabMoved {
        tab: TabId,
        side: TabSide,
    },
    /// Every pane of the tab moved one place on (`Layout::rotate`).
    PanesRotated {
        tab: TabId,
    },
    PanesSwapped {
        tab: TabId,
        a: PaneId,
        b: PaneId,
    },
    ZoomChanged {
        tab: TabId,
        zoom: Zoom,
    },
    /// The split at `split` in `tab`'s layout moved to `ratio`.
    SplitMoved {
        tab: TabId,
        split: SplitIndex,
        ratio: Ratio,
    },
}

/// Where a pane moved to another tab goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneDestination {
    /// Into an open tab, split in on the `direction` side of its focused
    /// `pane`.
    Beside { pane: PaneId, direction: Direction },
    /// Into a new tab of its own, opened on `side` of the tab it left.
    NewTab { side: TabSide },
}

/// Anything that happened, as reported on the session's event stream; lent
/// to the sink, which encodes it for subscribers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Event<'a> {
    Structure(&'a StructureEvent),
    /// The session is over; no event follows.
    SessionEnded {
        reason: EndReason,
    },
    PaneStarted {
        pane: PaneId,
        pid: Pid,
    },
    PaneExited {
        pane: PaneId,
        status: ExitStatus,
    },
    PaneSpawnFailed {
        pane: PaneId,
        error: SpawnError,
    },
    Bell {
        pane: PaneId,
    },
    ClientAttached {
        client: ClientId,
    },
    ClientDetached {
        client: ClientId,
    },
    ModeChanged {
        client: ClientId,
        mode: Mode,
    },
    SessionResized {
        size: GridSize,
    },
}

/// A position in the session's event stream. Sequence numbers only grow.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct EventSeq(u64);

impl EventSeq {
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

/// Why a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EndReason {
    /// The last pane closed or its program exited.
    LastPaneClosed,
    /// The last tab was closed.
    LastTabClosed,
    /// A client asked for it.
    Killed,
}

/// A process id, as the kernel hands them out: positive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Pid(NonZeroU32);

impl Pid {
    #[must_use]
    pub const fn new(pid: NonZeroU32) -> Self {
        Self(pid)
    }

    #[must_use]
    pub const fn get(self) -> NonZeroU32 {
        self.0
    }
}

/// How a pane's program ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExitStatus {
    /// It exited with this status (0 to 255).
    Code(u8),
    /// A signal killed it.
    Signal(NonZeroU8),
}

/// Why a pane's program could not be started.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SpawnError {
    pub stage: SpawnStage,
    /// The `errno` the failing call reported.
    pub errno: NonZeroI32,
}

/// The step of starting a program that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SpawnStage {
    OpenPty,
    Fork,
    Exec,
    /// Writing the history an editor opens.
    WriteHistory,
}
