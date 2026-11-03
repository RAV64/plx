//! Event subscribers (`plx events`): every event as a line of JSON, and an
//! explicit gap when a subscriber falls behind, never a silent drop.
//!
//! ```text
//! {"seq":7,"event":{"Structure":{"TabOpened":{"tab":1,"pane":2}}}}
//! {"lagged":{"from":8,"to":120}}
//! ```

use plx_bug::OrBug as _;
use plx_core::event::{Event, EventSeq};
use serde::Serialize;

/// Unwritten output past which a subscriber's events are dropped, and the
/// gap reported once it has room again.
pub const BACKLOG: usize = 1 << 20;

/// Events a subscriber missed and has not been told about.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lag {
    #[default]
    None,
    Missed {
        from: EventSeq,
        to: EventSeq,
    },
}

#[derive(Serialize)]
struct EventLine<'e, 'a> {
    seq: u64,
    event: &'e Event<'a>,
}

#[derive(Serialize)]
struct LaggedLine {
    lagged: Range,
}

#[derive(Serialize)]
struct Range {
    from: u64,
    to: u64,
}

/// Appends event `seq` to a subscriber's unwritten output `out`: first the
/// gap it missed, if any. When `out` is past [`BACKLOG`] the event is
/// dropped instead, and the gap grows.
pub fn write(out: &mut Vec<u8>, lag: &mut Lag, seq: EventSeq, event: &Event<'_>) {
    if out.len() >= BACKLOG {
        *lag = match *lag {
            Lag::None => Lag::Missed { from: seq, to: seq },
            Lag::Missed { from, .. } => Lag::Missed { from, to: seq },
        };
        return;
    }
    if let Lag::Missed { from, to } = *lag {
        let lagged = Range {
            from: from.get(),
            to: to.get(),
        };
        line(out, &LaggedLine { lagged });
        *lag = Lag::None;
    }
    line(
        out,
        &EventLine {
            seq: seq.get(),
            event,
        },
    );
}

fn line(out: &mut Vec<u8>, value: &impl Serialize) {
    serde_json::to_writer(&mut *out, value)
        .or_bug("events are plain data: they always serialise to JSON");
    out.push(b'\n');
}
