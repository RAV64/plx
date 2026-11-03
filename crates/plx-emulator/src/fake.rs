//! A deterministic stand-in for a real terminal emulator, for tests and the
//! simulation (feature `testkit`).
//!
//! It understands just enough to drive the core: printable ASCII is written
//! at the cursor (wrapping, scrolling at the bottom), `\r` and `\n` move the
//! cursor, BEL rings the bell, and ENQ (0x05) is answered with ACK (0x06) so
//! tests can see replies. A progress report, `ESC ] 9;4;state[;percent]`
//! ended by BEL or `ESC \`, is reported, even across feeds; any other
//! OSC is dropped. Every other byte is ignored. After its buffers are
//! sized, feeding and syncing allocate nothing.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;

use plx_bug::OrBug as _;

use crate::cell::Width;
use crate::events::EmulatorEvents;
use crate::grid::{
    Cursor, CursorBlink, CursorShape, CursorVisibility, GridSize, Position, Version,
};
use crate::key::{Key, KeyAction, KeyEvent, Mods};
use crate::mouse::Autoscroll;
use crate::port::{CellPx, Change, Compression, Damage, Emulator, EmulatorFactory};
use crate::progress::{Percent, Progress};
use crate::screen::{Screen, Text};
use crate::search::{SearchDirection, SearchOptions, SearchStatus};
use crate::style::Style;

/// See the module docs.
#[derive(Debug)]
pub struct FakeEmulator {
    size: GridSize,
    /// `rows × cols` characters; `None` is an empty cell.
    text: Box<[Option<char>]>,
    /// Rows changed since the last sync.
    dirty: Box<[Change]>,
    x: u16,
    y: u16,
    events: EmulatorEvents,
    screen: Screen,
    /// It finds nothing, but keeps how it was asked to search.
    search: SearchStatus,
    autoscroll: Autoscroll,
    escape: Escape,
    /// The start of the OSC being read; `osc_len` counts all of it.
    osc: [u8; OSC_KEPT],
    osc_len: usize,
}

/// The most of an OSC the fake keeps: a longer one is no progress report.
const OSC_KEPT: usize = 16;

/// Where the fake is in an escape sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Escape {
    Ground,
    /// After ESC.
    Esc,
    /// Inside an OSC.
    Osc,
    /// After ESC inside an OSC: `\` ends it.
    OscEsc,
}

/// The progress report an OSC's text makes (`9;4;state[;percent]`), if it
/// is one.
fn osc_progress_or_none(text: &[u8]) -> Option<Progress> {
    let decimal = |digits: &[u8]| -> Option<u8> {
        if digits.is_empty() {
            return None;
        }
        digits.iter().try_fold(0_u8, |n, &digit| match digit {
            b'0'..=b'9' => n.checked_mul(10)?.checked_add(digit - b'0'),
            _ => None,
        })
    };
    let rest = text.strip_prefix(b"9;4;")?;
    let (state, percent) = match rest.iter().position(|&byte| byte == b';') {
        Some(at) => (rest.get(..at)?, Some(rest.get(at + 1..)?)),
        None => (rest, None),
    };
    let percent = match percent {
        Some(digits) => match Percent::new(decimal(digits)?) {
            Ok(percent) => Some(percent),
            // Past 100: not a report.
            Err(_too_large) => return None,
        },
        None => None,
    };
    Progress::from_state_or_none(decimal(state)?, percent)
}

impl FakeEmulator {
    /// A blank terminal of `size`.
    #[must_use]
    pub fn new(size: GridSize) -> Self {
        Self {
            size,
            text: vec![None; size.area()].into_boxed_slice(),
            dirty: vec![Change::Changed; usize::from(size.rows().get())].into_boxed_slice(),
            x: 0,
            y: 0,
            events: EmulatorEvents::new(),
            screen: Screen::new(size),
            search: SearchStatus::default(),
            autoscroll: Autoscroll::Stopped,
            escape: Escape::Ground,
            osc: [0; OSC_KEPT],
            osc_len: 0,
        }
    }

    /// One byte outside any escape sequence.
    fn ground(&mut self, byte: u8) {
        match byte {
            0x1b => self.escape = Escape::Esc,
            b'\r' => self.x = 0,
            b'\n' => self.newline(),
            0x07 => self.events.ring_bell(),
            0x05 => self.events.push_reply(&[0x06]),
            0x20..=0x7e => self.put(char::from(byte)),
            _ => {}
        }
    }

    /// The OSC being read has ended.
    fn osc_ended(&mut self) {
        self.escape = Escape::Ground;
        if let Some(text) = self.osc.get(..self.osc_len)
            && let Some(progress) = osc_progress_or_none(text)
        {
            self.events.report_progress(progress);
        }
    }

    fn cols(&self) -> usize {
        usize::from(self.size.cols().get())
    }

    fn put(&mut self, c: char) {
        let cols = self.cols();
        if usize::from(self.x) == cols {
            self.newline();
            self.x = 0;
        }
        if let Some(cell) = self
            .text
            .get_mut(usize::from(self.y) * cols + usize::from(self.x))
        {
            *cell = Some(c);
        }
        if let Some(row) = self.dirty.get_mut(usize::from(self.y)) {
            *row = Change::Changed;
        }
        self.x += 1;
    }

    fn newline(&mut self) {
        if self.y + 1 < self.size.rows().get() {
            self.y += 1;
            return;
        }
        // Scroll: every row moves up one, the last row empties.
        let cols = self.cols();
        self.text.copy_within(cols.., 0);
        let len = self.text.len();
        if let Some(last) = self.text.get_mut(len - cols..) {
            last.fill(None);
        }
        self.dirty.fill(Change::Changed);
    }
}

impl Emulator for FakeEmulator {
    type Error = Infallible;

    fn feed(&mut self, bytes: &[u8]) -> &EmulatorEvents {
        self.events.clear();
        for &byte in bytes {
            match (self.escape, byte) {
                (Escape::Ground, _) => self.ground(byte),
                (Escape::Osc, 0x07) | (Escape::OscEsc, b'\\') => self.osc_ended(),
                (Escape::Osc, 0x1b) => self.escape = Escape::OscEsc,
                (Escape::Osc, _) => {
                    if let Some(slot) = self.osc.get_mut(self.osc_len) {
                        *slot = byte;
                    }
                    self.osc_len = self.osc_len.saturating_add(1);
                }
                // ESC inside an OSC cancels it and starts an escape.
                (Escape::Esc | Escape::OscEsc, b']') => {
                    self.escape = Escape::Osc;
                    self.osc_len = 0;
                }
                // Any other escape is ignored, as ESC alone always was.
                (Escape::Esc | Escape::OscEsc, _) => {
                    self.escape = Escape::Ground;
                    self.ground(byte);
                }
            }
        }
        &self.events
    }

    fn resize(
        &mut self,
        size: GridSize,
        _cell_px: Option<CellPx>,
    ) -> Result<&EmulatorEvents, Infallible> {
        self.events.clear();
        // No reflow: the new screen starts blank, the cursor clamped.
        self.size = size;
        self.text = vec![None; size.area()].into_boxed_slice();
        self.dirty = vec![Change::Changed; usize::from(size.rows().get())].into_boxed_slice();
        self.x = self.x.min(size.cols().get());
        self.y = self.y.min(size.rows().get() - 1);
        Ok(&self.events)
    }

    fn sync(&mut self, version: Version) -> Result<Damage, Infallible> {
        let mut damage = Damage::default();
        if self.screen.size() != self.size {
            self.screen.resize(self.size, version);
            damage.size = Change::Changed;
        }
        let cols = self.cols();
        for (y, (dirty, row)) in self
            .dirty
            .iter_mut()
            .zip(self.text.chunks_exact(cols))
            .enumerate()
        {
            if *dirty == Change::Unchanged {
                continue;
            }
            *dirty = Change::Unchanged;
            let y = u16::try_from(y).or_bug("a grid's rows are numbered in u16");
            let mut writer = self.screen.write_row(y, version);
            for cell in row {
                let text = cell.map_or(Text::Empty, Text::Char);
                writer.push(text, Width::Narrow, Style::DEFAULT);
            }
            if writer.finish() == Change::Changed {
                damage.rows += 1;
            }
        }
        let cursor = Cursor {
            position: Some(Position {
                x: self.x.min(self.size.cols().get() - 1),
                y: self.y,
            }),
            visibility: CursorVisibility::Visible,
            blink: CursorBlink::Steady,
            shape: CursorShape::Default,
        };
        damage.cursor = self.screen.set_cursor(cursor, version);
        Ok(damage)
    }

    fn screen(&self) -> &Screen {
        &self.screen
    }

    fn snapshot(&mut self) -> Result<Vec<u8>, Infallible> {
        let mut bytes = Vec::new();
        for c in self.text.iter().flatten() {
            let mut buf = [0_u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
        Ok(bytes)
    }

    fn encode_key(&mut self, key: &KeyEvent, out: &mut Vec<u8>) {
        if key.action == KeyAction::Release {
            return;
        }
        match key.key {
            Key::Char(c) => {
                let c = c.get();
                if key.mods.contains(Mods::CTRL) && c.is_ascii_alphabetic() {
                    let byte =
                        u8::try_from(c.to_ascii_lowercase()).or_bug("an ASCII letter is a byte");
                    out.push(byte & 0x1f);
                } else {
                    let mut buf = [0_u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            }
            Key::Enter => out.push(b'\r'),
            Key::Tab => out.push(b'\t'),
            Key::Backspace => out.push(0x7f),
            Key::Escape => out.push(0x1b),
            Key::Up
            | Key::Down
            | Key::Left
            | Key::Right
            | Key::Home
            | Key::End
            | Key::PageUp
            | Key::PageDown
            | Key::Insert
            | Key::Delete
            | Key::F(_) => {}
        }
    }

    fn encode_paste(&mut self, text: &str, out: &mut Vec<u8>) {
        out.extend_from_slice(text.as_bytes());
    }

    /// The fake keeps no history: its view is always the live screen.
    fn scroll(&mut self, _scroll: crate::port::Scroll) {}

    fn scroll_position(&self) -> crate::port::ScrollPosition {
        crate::port::ScrollPosition::default()
    }

    /// The fake's programs never ask for the mouse.
    fn mouse_tracking(&self) -> crate::mouse::MouseTracking {
        crate::mouse::MouseTracking::Off
    }

    fn active_screen(&self) -> crate::mouse::ActiveScreen {
        crate::mouse::ActiveScreen::Primary
    }

    fn encode_mouse(&mut self, _mouse: &crate::mouse::MouseEvent, _out: &mut Vec<u8>) {}

    /// The fake selects nothing, but a drag past the edge autoscrolls
    /// until the next step, as a real terminal's does.
    fn select(
        &mut self,
        gesture: crate::mouse::SelectGesture,
        at: crate::mouse::SelectAt,
        _time_ns: u64,
    ) -> Autoscroll {
        use crate::mouse::{Past, SelectGesture};
        self.autoscroll = match (gesture, at.past) {
            (SelectGesture::Drag, Past::Top | Past::Bottom) => Autoscroll::Scrolling,
            (SelectGesture::Drag, Past::Nothing)
            | (SelectGesture::Press | SelectGesture::Release, _) => Autoscroll::Stopped,
        };
        self.autoscroll
    }

    fn autoscroll_selection(&mut self) -> Autoscroll {
        self.autoscroll
    }

    fn selection_text(&mut self, _out: &mut Vec<u8>) -> crate::mouse::Selection {
        crate::mouse::Selection::None
    }

    fn clear_selection(&mut self) {}

    fn search(&mut self, _needle: &str, options: SearchOptions) {
        self.search = SearchStatus {
            options,
            ..SearchStatus::default()
        };
    }

    fn search_select(&mut self, _direction: SearchDirection) {}

    fn search_status(&self) -> SearchStatus {
        self.search
    }

    /// It keeps no history: nothing to compress.
    fn compress(&mut self) -> Compression {
        Compression::Done
    }

    /// It keeps no history: the screen's rows, one per line.
    fn history_text(&mut self, out: &mut Vec<u8>) {
        let mut line = alloc::string::String::new();
        for row in self.text.chunks(self.cols()) {
            line.clear();
            line.extend(row.iter().map(|cell| cell.unwrap_or(' ')));
            out.extend_from_slice(line.trim_end().as_bytes());
            out.push(b'\n');
        }
    }
}

/// Creates [`FakeEmulator`]s.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FakeFactory;

impl EmulatorFactory for FakeFactory {
    type Emulator = FakeEmulator;

    fn create(&mut self, size: GridSize) -> Result<FakeEmulator, Infallible> {
        Ok(FakeEmulator::new(size))
    }
}
