//! What a program says about its progress: the `OSC 9;4` report (from
//! `ConEmu`), which Ghostty, kitty and `WezTerm` draw as a progress bar.
//! plx carries it typed, never as the program's bytes.

use serde::{Deserialize, Serialize};

/// A percentage: 0 through 100.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8")]
pub struct Percent(u8);

/// A percentage past 100.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0} is more than 100 percent")]
pub struct PercentTooLarge(pub u8);

impl Percent {
    pub const MAX: Self = Self(100);

    /// # Errors
    ///
    /// When `percent` is past 100.
    pub const fn new(percent: u8) -> Result<Self, PercentTooLarge> {
        if percent > Self::MAX.0 {
            Err(PercentTooLarge(percent))
        } else {
            Ok(Self(percent))
        }
    }

    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for Percent {
    type Error = PercentTooLarge;

    fn try_from(percent: u8) -> Result<Self, PercentTooLarge> {
        Self::new(percent)
    }
}

/// A program's progress, as it last reported it. A percentage is kept
/// only where the protocol gives it a meaning, and only if the program
/// sent one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Progress {
    /// Nothing to show: the program removed its report, or never made one.
    #[default]
    Removed,
    /// Under way.
    Set(Option<Percent>),
    /// Failed.
    Error(Option<Percent>),
    /// Under way, with no idea how far.
    Indeterminate,
    Paused(Option<Percent>),
}

impl Progress {
    /// A report from its protocol state (`0` remove, `1` set, `2` error,
    /// `3` indeterminate, `4` pause), or `None` for a state the protocol
    /// does not have.
    #[must_use]
    pub const fn from_state_or_none(state: u8, percent: Option<Percent>) -> Option<Self> {
        match state {
            0 => Some(Self::Removed),
            1 => Some(Self::Set(percent)),
            2 => Some(Self::Error(percent)),
            3 => Some(Self::Indeterminate),
            4 => Some(Self::Paused(percent)),
            _ => None,
        }
    }

    /// The protocol state: the inverse of [`Progress::from_state_or_none`].
    #[must_use]
    pub const fn state(self) -> u8 {
        match self {
            Self::Removed => 0,
            Self::Set(_) => 1,
            Self::Error(_) => 2,
            Self::Indeterminate => 3,
            Self::Paused(_) => 4,
        }
    }

    /// The percentage, where the report has one.
    #[must_use]
    pub const fn percent_or_none(self) -> Option<Percent> {
        match self {
            Self::Set(percent) | Self::Error(percent) | Self::Paused(percent) => percent,
            Self::Removed | Self::Indeterminate => None,
        }
    }
}
