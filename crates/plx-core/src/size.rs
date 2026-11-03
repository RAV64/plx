//! Session size: which grid size the session uses when several clients of
//! different sizes are attached, and the area panes get within it.

use core::num::NonZeroU16;

use plx_emulator::GridSize;
use serde::{Deserialize, Serialize};

use crate::geometry::Rect;
use crate::id::ClientId;
use crate::time::Timestamp;

/// How the session size follows the attached clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SizePolicy {
    /// The size of the most recently active client (tmux's default). Other
    /// clients clip or pad.
    Latest,
    /// The smallest width and the smallest height among the clients; every
    /// client sees everything.
    Smallest,
    /// The largest width and the largest height; smaller clients clip.
    Largest,
}

/// One attached client, as the policy sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientSize {
    pub client: ClientId,
    pub size: GridSize,
    /// When the client last attached, resized or sent input.
    pub last_active: Timestamp,
}

impl SizePolicy {
    /// The session size for these clients, or `None` when there are none (the
    /// session then keeps its last size).
    #[must_use]
    pub fn effective(self, clients: &[ClientSize]) -> Option<GridSize> {
        match self {
            Self::Latest => clients
                .iter()
                // Ties on activity go to the newest client, deterministically.
                .max_by_key(|c| (c.last_active, c.client))
                .map(|c| c.size),
            Self::Smallest => clients.iter().map(|c| c.size).reduce(GridSize::smallest),
            Self::Largest => clients.iter().map(|c| c.size).reduce(GridSize::largest),
        }
    }
}

/// Rows the client keeps for its status line at the bottom of its screen.
pub const STATUS_ROWS: u16 = 1;

/// The area panes are arranged in for a session of `size`: everything but
/// the status line. A one-row session has no room for one and uses its only
/// row for panes.
#[must_use]
pub fn pane_area(size: GridSize) -> Rect {
    let rows = size.rows();
    let rows = NonZeroU16::new(rows.get().saturating_sub(STATUS_ROWS)).unwrap_or(rows);
    Rect::at_origin(size.within(size.cols(), rows))
}
