//! Mouse input, in cells: what the client's terminal reported, and what a
//! pane's program receives when it tracks the mouse.

use serde::{Deserialize, Serialize};

use crate::grid::Position;
use crate::key::Mods;

/// One mouse report. `position` is a cell: on the client's terminal as it
/// arrives, relative to a pane once the session places it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MouseEvent {
    pub action: MouseAction,
    pub button: MouseButton,
    pub position: Position,
    pub mods: Mods,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseAction {
    Press,
    Release,
    /// Movement; `button` is the one held, if any.
    Motion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
    /// Motion with no button held.
    None,
}

impl MouseButton {
    /// Whether this is a wheel notch rather than a button.
    #[must_use]
    pub const fn wheel(self) -> Wheel {
        match self {
            Self::WheelUp => Wheel::Up,
            Self::WheelDown => Wheel::Down,
            Self::WheelLeft | Self::WheelRight => Wheel::Sideways,
            Self::Left | Self::Middle | Self::Right | Self::None => Wheel::No,
        }
    }
}

/// What a wheel notch asks for, if the button is one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wheel {
    No,
    Up,
    Down,
    Sideways,
}

/// Whether a program asked to receive the mouse (any tracking mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseTracking {
    Off,
    On,
}

/// A step of selecting text with the mouse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectGesture {
    /// Starts a selection; a second or third press soon after selects the
    /// word or the line.
    Press,
    /// Extends it to the pointer.
    Drag,
    Release,
}

/// Where a pointer selecting text is: the visible cell nearest it, and
/// whether it is past the pane's top or bottom edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectAt {
    pub cell: Position,
    pub past: Past,
}

/// Whether a selecting pointer is past the pane's top or bottom edge, where
/// a drag scrolls the view to select beyond what it shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Past {
    Nothing,
    Top,
    Bottom,
}

/// Whether a drag past the pane's edge scrolls its view: while it does,
/// [`crate::Emulator::autoscroll_selection`] is to be called every
/// [`AUTOSCROLL_INTERVAL`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Autoscroll {
    Stopped,
    Scrolling,
}

/// How often a selection that autoscrolls moves one row: libghostty's
/// suggestion, as Ghostty itself scrolls.
pub const AUTOSCROLL_INTERVAL: core::time::Duration = core::time::Duration::from_millis(15);

/// Whether a terminal has a selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    None,
    Some,
}

/// Which screen a terminal shows: full-screen programs use the alternate
/// one, which keeps no history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveScreen {
    Primary,
    Alternate,
}
