//! Identifiers.
//!
//! Pane and tab ids are minted only by the session's allocator, inside
//! [`crate::structure::Structure`]: they are monotonic and never reused, so an
//! id names at most one thing for the lifetime of a session, and replaying
//! the event log rebuilds the allocator with the structure. Nothing else can
//! make one (outside tests): an id from a client arrives as a claim in the
//! protocol and is resolved against the session.

use serde::{Deserialize, Serialize};

macro_rules! id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name(u64);

        impl $name {
            /// An id from its raw value: for the allocator, and for tests.
            #[cfg(any(test, feature = "testkit"))]
            #[must_use]
            pub const fn new(raw: u64) -> Self {
                Self(raw)
            }

            #[cfg(not(any(test, feature = "testkit")))]
            #[must_use]
            pub(crate) const fn new(raw: u64) -> Self {
                Self(raw)
            }

            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

id!(
    /// A pane: one PTY, one program, one emulator.
    PaneId
);
id!(
    /// A tab: one layout of panes.
    TabId
);
/// A connected client. Minted by the session when a client connects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ClientId(u64);

impl ClientId {
    /// The first client's id.
    pub(crate) const FIRST: Self = Self(0);

    /// The id after this one.
    pub(crate) const fn next(self) -> Self {
        Self(self.0 + 1)
    }

    /// An id from its raw value, for tests.
    #[cfg(any(test, feature = "testkit"))]
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The next pane and tab ids. Ids are *peeked* while a command is decided
/// and *committed* when the event that introduces them is folded, so the
/// allocator advances only with the event log. Not `Clone`: a copy would
/// hand out the same ids twice.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct IdAllocator {
    next_pane: u64,
    next_tab: u64,
}

impl IdAllocator {
    pub(crate) const fn new() -> Self {
        Self {
            next_pane: 0,
            next_tab: 0,
        }
    }

    /// The id the next new pane gets.
    pub(crate) const fn peek_pane(&self) -> PaneId {
        PaneId::new(self.next_pane)
    }

    /// The id the next new tab gets.
    pub(crate) const fn peek_tab(&self) -> TabId {
        TabId::new(self.next_tab)
    }

    /// Records that `pane` was opened. Panes open in allocation order, so it
    /// is the peeked id.
    #[track_caller]
    pub(crate) fn commit_pane(&mut self, pane: PaneId) {
        plx_bug::invariant!(
            pane.0 == self.next_pane,
            "panes open with the next pane id: {} instead of {}",
            pane.0,
            self.next_pane,
        );
        self.next_pane += 1;
    }

    /// Records that `tab` was opened, as [`IdAllocator::commit_pane`].
    #[track_caller]
    pub(crate) fn commit_tab(&mut self, tab: TabId) {
        plx_bug::invariant!(
            tab.0 == self.next_tab,
            "tabs open with the next tab id: {} instead of {}",
            tab.0,
            self.next_tab,
        );
        self.next_tab += 1;
    }
}
