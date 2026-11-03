//! Commands: what a user or script asks for.
//!
//! A command comes from a keybinding, the CLI, or a script, always as the
//! same type. It is an intent and may be rejected; when accepted it produces
//! events (facts). Its values are already parsed (a [`TabName`] is valid, a
//! [`TabNumber`] is at least 1), so deciding it never re-checks them.

use plx_emulator::SearchDirection;
use serde::{Deserialize, Serialize};

use crate::geometry::Direction;
use crate::id::PaneId;
use crate::layout::{Ratio, SplitIndex};
use crate::tab::{TabName, TabNumber};

/// Any command a binding can run. Not `Clone`: a command is lent (from a
/// keymap, or decoded from a client's buffer) to whoever executes it.
#[derive(Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Command {
    /// Changes the session: decided by its structure.
    Session(SessionCommand),
    /// Concerns only the client that asked.
    Client(ClientCommand),
    /// Moves the focused pane's view of its history. Not part of the
    /// structure, so it produces no event.
    Scroll(ScrollBy),
    /// Acts on the search going on (search mode); none either.
    Search(SearchCommand),
    /// Opens the focused pane's history in the user's editor, in the
    /// pane's place; the pane runs on hidden and is back when the editor
    /// exits or its pane is closed.
    EditScrollback,
}

/// What to do with the search going on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SearchCommand {
    /// Select the next match that way, scrolling to it.
    Select(SearchDirection),
    ToggleCase,
    ToggleWholeWord,
}

/// How far to move a pane's view of its history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScrollBy {
    LineUp,
    LineDown,
    HalfPageUp,
    HalfPageDown,
    PageUp,
    PageDown,
    /// The oldest output.
    Top,
    /// The live screen.
    Bottom,
    /// The previous command the shell ran (its prompt, by the marks the
    /// shell writes).
    PreviousPrompt,
    NextPrompt,
}

/// Which way along the row of tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TabSide {
    Previous,
    Next,
}

/// A command the session's structure decides. Acts on the active tab and
/// its focused pane.
#[derive(Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionCommand {
    /// Split the focused pane and start a new pane on the `direction` side.
    Split {
        direction: Direction,
    },
    /// Close the focused pane (its process group is hung up).
    ClosePane,
    /// Move focus to the neighbouring pane in `direction`.
    Focus {
        direction: Direction,
    },
    /// Resize the focused pane towards `direction`, as Zellij's `Resize
    /// Increase`: its border on that side moves that way; at the edge, its
    /// other border does, shrinking it.
    Grow {
        direction: Direction,
    },
    /// Move every pane of the active tab one place on, the last to the
    /// first; the focus stays with its pane.
    RotatePanes,
    /// Move the focused pane towards `direction`: it swaps places with its
    /// neighbour that way; with none, left and right take it into the tab
    /// before or after (a new one past the end of the row of tabs).
    MovePane {
        direction: Direction,
    },
    /// Move the focused pane into a new tab of its own, after the active
    /// one.
    BreakPane,
    /// Move the focused pane into the tab on `side` (a new one past the
    /// end of the row of tabs).
    MovePaneToTab {
        side: TabSide,
    },
    /// Move the active tab one place towards `side`.
    MoveTab {
        side: TabSide,
    },
    /// Swap the focused pane with its neighbour in `direction`.
    Swap {
        direction: Direction,
    },
    /// Show the focused pane alone, filling the tab, or undo that.
    ToggleZoom,
    /// Open a tab with one new pane after the active tab, and focus it.
    NewTab,
    /// Close the active tab and every pane in it.
    CloseTab,
    NextTab,
    PreviousTab,
    FocusTab {
        tab: TabNumber,
    },
    RenameTab {
        name: TabName,
    },
    /// End the session: close every pane and stop the daemon.
    KillSession,
    /// Focus `pane` in the active tab (a click on it).
    FocusPane {
        pane: PaneId,
    },
    /// Move the active tab's split at `split` to `ratio` (its separator
    /// dragged).
    MoveSplit {
        split: SplitIndex,
        ratio: Ratio,
    },
}

/// A command for the requesting client alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClientCommand {
    /// Detach the client; the session keeps running.
    Detach,
}
