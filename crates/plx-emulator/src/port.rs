//! The emulator port itself.

use alloc::vec::Vec;
use core::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use crate::events::EmulatorEvents;
use crate::grid::{GridSize, Version};
use crate::key::KeyEvent;
use crate::mouse::{
    ActiveScreen, Autoscroll, MouseEvent, MouseTracking, SelectAt, SelectGesture, Selection,
};
use crate::screen::Screen;
use crate::search::{SearchDirection, SearchOptions, SearchStatus};

/// The pixel size of one cell, when the client knows it.
///
/// Programs use it for image protocols and pixel size reports
/// (`CSI 14 t`); it is passed as `Option<CellPx>` because a client may not
/// know its cell size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CellPx {
    pub width: NonZeroU32,
    pub height: NonZeroU32,
}

/// Whether something changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Change {
    #[default]
    Unchanged,
    Changed,
}

/// What a [`Emulator::sync`] changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Damage {
    /// Rows whose content changed.
    pub rows: u16,
    pub cursor: Change,
    /// `Changed` when the grid was reallocated for a new size (every row
    /// changed).
    pub size: Change,
}

impl Damage {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows == 0 && self.cursor == Change::Unchanged && self.size == Change::Unchanged
    }
}

/// A terminal emulator: the state machine that turns a program's output into
/// a screen. It owns the pane's [`Screen`] and lends it.
///
/// Implementations are deterministic: the same sequence of calls produces the
/// same grids and events, however the output bytes are split across
/// [`Emulator::feed`] calls.
pub trait Emulator {
    type Error: core::error::Error;

    /// Feeds program output and lends what it triggered (replies to write
    /// back, bells). This cannot fail: malformed input is part of the stream
    /// and is handled by the emulator.
    fn feed(&mut self, bytes: &[u8]) -> &EmulatorEvents;

    /// Resizes the terminal, reflowing the primary screen. A resize can
    /// itself trigger replies (an in-band size report under mode 2048).
    ///
    /// # Errors
    ///
    /// When the emulator cannot apply the new size.
    fn resize(
        &mut self,
        size: GridSize,
        cell_px: Option<CellPx>,
    ) -> Result<&EmulatorEvents, Self::Error>;

    /// Brings [`Emulator::screen`] up to date with the terminal, touching
    /// only what changed since the last sync; rows whose content changed get
    /// `version`.
    ///
    /// # Errors
    ///
    /// When the terminal cannot update its render state (an internal limit).
    fn sync(&mut self, version: Version) -> Result<Damage, Self::Error>;

    /// The visible screen as of the last [`Emulator::sync`].
    fn screen(&self) -> &Screen;

    /// Serialises the complete terminal state. The format belongs to the
    /// implementation; it is only guaranteed to load into the same build.
    ///
    /// # Errors
    ///
    /// When the state cannot be serialised.
    fn snapshot(&mut self) -> Result<Vec<u8>, Self::Error>;

    /// Appends to `out` the bytes the program should receive for `key`,
    /// encoded for the terminal modes it has set (application cursor keys,
    /// the kitty keyboard protocol, ...). Appends nothing when the key
    /// produces no input. `out` is the caller's reused buffer. Every
    /// [`KeyEvent`] can be encoded: its type excludes what an encoder cannot
    /// take.
    fn encode_key(&mut self, key: &KeyEvent, out: &mut Vec<u8>);

    /// Appends to `out` the bytes the program should receive for pasted
    /// `text`: control characters neutralised, and bracketed if the program
    /// enabled bracketed paste. Any text can be pasted.
    fn encode_paste(&mut self, text: &str, out: &mut Vec<u8>);

    /// Moves the view of the history; the next [`Emulator::sync`] shows
    /// it. New output does not move a view that is back in the history.
    fn scroll(&mut self, scroll: Scroll);

    /// Where the view is in the history.
    fn scroll_position(&self) -> ScrollPosition;

    /// Whether the program asked for the mouse.
    fn mouse_tracking(&self) -> MouseTracking;

    /// Which screen the program uses.
    fn active_screen(&self) -> ActiveScreen;

    /// Appends to `out` the bytes the program should receive for `mouse`
    /// (its position relative to the pane), encoded for the mouse modes it
    /// set. Appends nothing when those modes ignore the event.
    fn encode_mouse(&mut self, mouse: &MouseEvent, out: &mut Vec<u8>);

    /// A step of selecting text with the pointer `at`, at `time_ns` (a
    /// double or triple press selects the word or the line). The next
    /// [`Emulator::sync`] shows the selection: its cells in inverse video.
    /// A drag past the top or bottom edge starts the view scrolling, as
    /// the result says; any other step stops it.
    fn select(&mut self, gesture: SelectGesture, at: SelectAt, time_ns: u64) -> Autoscroll;

    /// One step of a drag past the edge: the view scrolls a row towards
    /// it, and the selection extends to the row now under the pointer.
    /// `Stopped` once the drag is over or no longer past the edge.
    fn autoscroll_selection(&mut self) -> Autoscroll;

    /// Appends the selected text to `out` as UTF-8: plain, soft wraps
    /// joined, trailing blanks trimmed.
    fn selection_text(&mut self, out: &mut Vec<u8>) -> Selection;

    fn clear_selection(&mut self);

    /// Searches the active screen and its history for `needle` as
    /// `options` say; an empty needle stops searching. The next
    /// [`Emulator::sync`] marks the matches in view, the selected one
    /// apart.
    fn search(&mut self, needle: &str, options: SearchOptions);

    /// Selects the next match `direction` (from the live screen when none
    /// is selected yet), scrolling the view to it.
    fn search_select(&mut self, direction: SearchDirection);

    /// The search as of the last [`Emulator::sync`] or search call.
    fn search_status(&self) -> SearchStatus;

    /// Appends everything the terminal holds, its history then its screen,
    /// to `out` as UTF-8: plain, soft wraps joined, trailing blanks trimmed.
    fn history_text(&mut self, out: &mut Vec<u8>);

    /// Does a bounded step of compressing the history, which only changes
    /// how it is stored: reading it later restores it. `Pending` while more
    /// is left to do.
    fn compress(&mut self) -> Compression;
}

/// Whether compressing a terminal's history has more to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    Pending,
    /// Nothing left until the history changes (or compression is not
    /// supported here).
    Done,
}

/// Where to move a terminal's view of its history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    /// This many rows back, towards older output.
    Up(u16),
    /// This many rows forward, towards the live screen.
    Down(u16),
    /// The oldest row.
    Top,
    /// The live screen: the view follows new output again.
    Bottom,
    /// To the previous (negative) or next shell prompt, by the semantic
    /// prompt marks the shell writes (OSC 133). Without marks it stays.
    Prompts(i16),
}

/// Where a terminal's view is in its history, in rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollPosition {
    /// How far back the view is: 0 when it shows the live screen.
    pub back: u64,
    /// Rows of history above the live screen.
    pub history: u64,
}

/// Creates emulators, one per pane. Configuration (scrollback, limits, ...)
/// belongs to the factory, so the core only says how big.
pub trait EmulatorFactory {
    type Emulator: Emulator;

    /// A blank emulator of `size`.
    ///
    /// # Errors
    ///
    /// When the emulator cannot be created.
    fn create(
        &mut self,
        size: GridSize,
    ) -> Result<Self::Emulator, <Self::Emulator as Emulator>::Error>;
}
