//! The search going on in a session: the pane searched, the needle typed
//! so far, and how it matches. The emulator does the searching.

use alloc::string::String;

use plx_emulator::{Change, Key, KeyAction, KeyEvent, Mods, SearchOptions, SearchStatus};

use crate::id::PaneId;

/// The text searched for, as typed: no control characters.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Needle(String);

impl Needle {
    #[must_use]
    pub const fn new() -> Self {
        Self(String::new())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Types `event` into the needle: a character is added, `Backspace`
    /// deletes the last one. `Changed` when it did either.
    #[must_use]
    pub fn type_key(&mut self, event: &KeyEvent) -> Typed {
        if event.action == KeyAction::Release {
            return Typed::Ignored;
        }
        // Shift is part of what a character is (`A`, `!`); any other
        // modifier makes a chord, not text.
        let chord = event.chord();
        if chord.mods().without(Mods::SHIFT) != Mods::empty() {
            return Typed::Ignored;
        }
        match chord.key() {
            Key::Char(c) if !c.get().is_control() => {
                self.0.push(c.get());
                Typed::Changed
            }
            Key::Backspace => match self.0.pop() {
                Some(_) => Typed::Changed,
                None => Typed::Ignored,
            },
            Key::Char(_)
            | Key::Enter
            | Key::Tab
            | Key::Escape
            | Key::Up
            | Key::Down
            | Key::Left
            | Key::Right
            | Key::Home
            | Key::End
            | Key::PageUp
            | Key::PageDown
            | Key::Insert
            | Key::Delete
            | Key::F(_) => Typed::Ignored,
        }
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Replaces the needle with `text` (a client mirroring the session's).
    ///
    /// # Errors
    ///
    /// When `text` holds a control character, which no needle does.
    pub fn set(&mut self, text: &str) -> Result<Change, ControlCharacter> {
        if text.chars().any(char::is_control) {
            return Err(ControlCharacter);
        }
        if self.0 == text {
            return Ok(Change::Unchanged);
        }
        self.0.clear();
        self.0.push_str(text);
        Ok(Change::Changed)
    }
}

/// Text with a control character, where a needle was expected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a search needle holds no control characters")]
pub struct ControlCharacter;

/// Whether a key changed the needle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Typed {
    Ignored,
    Changed,
}

/// The session's search: at most one at a time, while a client is in a
/// search mode.
#[derive(Debug, Default)]
pub(crate) struct Search {
    /// The pane searched; `None` while nobody searches.
    pub(crate) pane: Option<PaneId>,
    pub(crate) needle: Needle,
    /// Kept from one search to the next, as Zellij keeps them.
    pub(crate) options: SearchOptions,
}

/// A search as a client's find bar shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchView<'a> {
    pub needle: &'a Needle,
    pub status: SearchStatus,
}
