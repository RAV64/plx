//! One tab: its name, its layout of panes, and which pane is focused.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU16;

use serde::{Deserialize, Serialize};

use crate::geometry::Rect;
use crate::id::{PaneId, TabId};
use crate::layout::{Arrangement, Layout, Placement};

/// A tab's name: non-empty, at most [`TabName::MAX_CHARS`] characters, no
/// control characters. Parsed once where it enters (a command from a client
/// or the config); everything after holds a valid name.
#[derive(Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct TabName(Box<str>);

/// Why a string is not a [`TabName`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, thiserror::Error)]
pub enum TabNameError {
    #[error("a tab name cannot be empty")]
    Empty,
    #[error("a tab name has at most {} characters", TabName::MAX_CHARS)]
    TooLong,
    #[error("a tab name cannot contain control characters")]
    ControlCharacter,
}

impl TabName {
    /// The longest name, in characters.
    pub const MAX_CHARS: usize = 64;

    /// # Errors
    ///
    /// When `name` is empty, too long, or contains a control character.
    pub fn new(name: &str) -> Result<Self, TabNameError> {
        Self::check(name)?;
        Ok(Self(name.into()))
    }

    fn check(name: &str) -> Result<(), TabNameError> {
        if name.is_empty() {
            Err(TabNameError::Empty)
        } else if name.chars().count() > Self::MAX_CHARS {
            Err(TabNameError::TooLong)
        } else if name.chars().any(char::is_control) {
            Err(TabNameError::ControlCharacter)
        } else {
            Ok(())
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A copy, for the one place a name must be owned twice: a renaming
    /// command lent from a keymap becomes an event that owns its name.
    #[must_use]
    pub fn duplicate(&self) -> Self {
        Self(self.0.as_ref().into())
    }
}

impl TryFrom<String> for TabName {
    type Error = TabNameError;

    fn try_from(name: String) -> Result<Self, TabNameError> {
        Self::check(&name)?;
        Ok(Self(name.into_boxed_str()))
    }
}

/// A tab as users count them: 1 for the first. The number of tabs is
/// bounded, so a number either names a tab or is a rejection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TabNumber(NonZeroU16);

impl TabNumber {
    #[must_use]
    pub const fn new(number: NonZeroU16) -> Self {
        Self(number)
    }

    #[must_use]
    pub const fn get(self) -> NonZeroU16 {
        self.0
    }

    /// The zero-based position this number names.
    pub(crate) fn position(self) -> usize {
        usize::from(self.0.get()) - 1
    }
}

/// Whether a tab shows its focused pane alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Zoom {
    /// Every pane in its place.
    Tiled,
    /// The focused pane alone, filling the tab.
    Zoomed,
}

impl Zoom {
    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Tiled => Self::Zoomed,
            Self::Zoomed => Self::Tiled,
        }
    }
}

/// One tab.
#[derive(Debug, PartialEq, Eq)]
pub struct Tab {
    pub(crate) id: TabId,
    /// `None` until renamed; clients then show the tab's number.
    pub(crate) name: Option<TabName>,
    pub(crate) layout: Layout,
    /// Always a pane of `layout`.
    pub(crate) focus: PaneId,
    pub(crate) zoom: Zoom,
    /// Panes in the layout that stand in for others, which run on hidden.
    pub(crate) covers: Vec<Cover>,
}

/// A pane standing in the place of another (an editor on its history),
/// which runs on hidden and comes back when the cover goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cover {
    /// In the layout.
    pub cover: PaneId,
    /// Hidden behind it.
    pub covered: PaneId,
}

impl Tab {
    /// A tab holding one pane.
    pub(crate) const fn new(id: TabId, pane: PaneId) -> Self {
        Self {
            id,
            name: None,
            layout: Layout::single(pane),
            focus: pane,
            zoom: Zoom::Tiled,
            covers: Vec::new(),
        }
    }

    #[must_use]
    pub const fn id(&self) -> TabId {
        self.id
    }

    #[must_use]
    pub const fn name(&self) -> Option<&TabName> {
        self.name.as_ref()
    }

    #[must_use]
    pub const fn layout(&self) -> &Layout {
        &self.layout
    }

    #[must_use]
    pub const fn focus(&self) -> PaneId {
        self.focus
    }

    #[must_use]
    pub const fn zoom(&self) -> Zoom {
        self.zoom
    }

    /// The panes standing in for hidden ones.
    #[must_use]
    pub fn covers(&self) -> &[Cover] {
        &self.covers
    }

    /// Whether `pane` is in this tab: in its layout, or hidden behind a
    /// cover.
    #[must_use]
    pub fn holds(&self, pane: PaneId) -> bool {
        self.layout.contains(pane) || self.covers.iter().any(|cover| cover.covered == pane)
    }

    /// The cover record of `cover`, if it stands in for a pane.
    pub(crate) fn cover_of_or_none(&self, cover: PaneId) -> Option<Cover> {
        self.covers
            .iter()
            .copied()
            .find(|record| record.cover == cover)
    }

    /// Places the tab's panes into `area`. A zoomed tab shows only its
    /// focused pane, filling the area.
    #[must_use]
    pub fn arrange(&self, area: Rect) -> Arrangement {
        let mut arrangement = Arrangement::default();
        self.arrange_into(area, &mut arrangement);
        arrangement
    }

    /// As [`Tab::arrange`], reusing `out`'s buffers (cleared first).
    pub fn arrange_into(&self, area: Rect, out: &mut Arrangement) {
        match self.zoom {
            Zoom::Tiled => self.layout.arrange_into(area, out),
            Zoom::Zoomed => {
                out.panes.clear();
                out.separators.clear();
                let focus = self.focus;
                self.layout.for_each_pane(&mut |pane| {
                    out.panes.push(Placement {
                        pane,
                        rect: (pane == focus).then_some(area),
                    });
                });
            }
        }
    }
}
