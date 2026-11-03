//! Time as data. The core never reads a clock; every step receives `now`.

use core::time::Duration;

use serde::{Deserialize, Serialize};

/// A point in time: nanoseconds since the daemon started. Constructible at
/// any value, so tests and recordings control time completely.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Timestamp(u64);

impl Timestamp {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    #[must_use]
    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    /// `self + duration`, saturating at the end of time (584 years after
    /// start), which only ever makes a deadline "never".
    #[must_use]
    pub fn saturating_add(self, duration: Duration) -> Self {
        let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        Self(self.0.saturating_add(nanos))
    }
}
