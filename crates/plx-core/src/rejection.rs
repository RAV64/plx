//! Rejections: why a command or client message was refused. They are data,
//! sent back to whoever asked; the session is unchanged.

use serde::{Deserialize, Serialize};

use crate::geometry::Direction;
use crate::message::FrameSeq;
use crate::structure::MAX_TABS;
use crate::tab::TabNumber;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, thiserror::Error)]
pub enum Rejection {
    #[error("a session holds at most {} tabs", MAX_TABS)]
    TooManyTabs,
    #[error("there is no tab {}", tab.get())]
    NoSuchTab { tab: TabNumber },
    #[error("there is no pane {direction:?} of the focused pane")]
    NoPaneInDirection { direction: Direction },
    #[error("the focused pane cannot grow {direction:?}")]
    CannotGrow { direction: Direction },
    #[error("the client must attach first")]
    NotAttached,
    #[error("the client is already attached")]
    AlreadyAttached,
    #[error("acknowledged frame {} but frame {:?} is in flight", got.get(), expected.map(FrameSeq::get))]
    UnexpectedAck {
        got: FrameSeq,
        expected: Option<FrameSeq>,
    },
    #[error("the session is ending")]
    SessionEnding,
    #[error("the active tab has no such pane")]
    NoSuchPane,
    #[error("the active tab has no such split")]
    NoSuchSplit,
    #[error(
        "no room to split the pane {direction:?}: each side needs {} columns and {} rows",
        crate::structure::MIN_PANE_COLS,
        crate::structure::MIN_PANE_ROWS
    )]
    NoRoomToSplit { direction: Direction },
}
