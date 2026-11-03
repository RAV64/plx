//! Searching a terminal's history, as a find bar shows it: a needle, how it
//! matches, and which of how many matches is selected.

use core::num::NonZeroU32;

use serde::{Deserialize, Serialize};

/// Whether a match must have the needle's letter case.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Case {
    #[default]
    Ignored,
    Matched,
}

/// Whether a match may be part of a longer word.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Words {
    #[default]
    Within,
    /// Only whole words: no letter, digit or `_` right before or after
    /// (across a soft wrap, the line goes on), as `grep -w` matches.
    Whole,
}

/// How a needle matches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SearchOptions {
    pub case: Case,
    pub words: Words,
}

impl SearchOptions {
    #[must_use]
    pub const fn toggled_case(self) -> Self {
        Self {
            case: match self.case {
                Case::Ignored => Case::Matched,
                Case::Matched => Case::Ignored,
            },
            ..self
        }
    }

    #[must_use]
    pub const fn toggled_words(self) -> Self {
        Self {
            words: match self.words {
                Words::Within => Words::Whole,
                Words::Whole => Words::Within,
            },
            ..self
        }
    }
}

/// Which way to the next match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SearchDirection {
    /// Up, into the history; past the oldest match it wraps to the newest.
    Older,
    /// Down, towards the live screen; past the newest it wraps to the oldest.
    Newer,
}

/// A search as a find bar shows it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SearchStatus {
    pub options: SearchOptions,
    /// The matches the options accept.
    pub total: u32,
    /// The selected match, counted from the newest: 1 is the newest. `None`
    /// before any is selected.
    pub selected: Option<NonZeroU32>,
    /// Whether `total` and `selected` are up to date.
    pub count: Count,
}

/// Whether a search's count is up to date. A filtered count walks every
/// match; with many matches and output streaming in, it is taken again at
/// the next search key, and is stale until then.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Count {
    #[default]
    Current,
    Stale,
}
