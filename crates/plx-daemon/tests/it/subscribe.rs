use plx_core::event::{EndReason, Event, EventSeq};
use plx_daemon::subscribe::{BACKLOG, Lag, write};
use proptest::prelude::*;

/// What the subscriber does before an event is written.
#[derive(Clone, Copy, Debug)]
enum Reader {
    /// Takes everything written so far.
    Reads,
    /// Takes nothing; its unwritten output stands past the backlog.
    Stalled,
}

/// The lines a subscriber received: each event's seq, or a reported gap.
#[derive(Debug, PartialEq)]
enum Line {
    Event(u64),
    Lagged(u64, u64),
}

fn parse(bytes: &[u8]) -> Vec<Line> {
    std::str::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            match (&value["seq"], &value["lagged"]) {
                (serde_json::Value::Number(seq), _) => {
                    assert!(value["event"].is_object(), "{line}");
                    Line::Event(seq.as_u64().unwrap())
                }
                (_, serde_json::Value::Object(gap)) => {
                    Line::Lagged(gap["from"].as_u64().unwrap(), gap["to"].as_u64().unwrap())
                }
                _ => panic!("not an event line: {line}"),
            }
        })
        .collect()
}

proptest! {
    /// However the subscriber keeps up, every event reaches it exactly
    /// once: delivered, or inside one gap reported before the next event
    /// it gets. Gaps and events come in order.
    #[test]
    fn every_event_is_delivered_or_reported_missed(
        readers in proptest::collection::vec(
            prop_oneof![Just(Reader::Reads), Just(Reader::Stalled)],
            1..60,
        ),
    ) {
        let event = Event::SessionEnded {
            reason: EndReason::Killed,
        };
        let mut out = Vec::new();
        let mut lag = Lag::None;
        let mut received = Vec::new();
        // Where filler stands in `out`: skipped when the reader takes it.
        let mut fillers: Vec<std::ops::Range<usize>> = Vec::new();
        let take = |out: &mut Vec<u8>, fillers: &mut Vec<std::ops::Range<usize>>, into: &mut Vec<u8>| {
            let mut at = 0;
            for filler in fillers.drain(..) {
                into.extend_from_slice(&out[at..filler.start]);
                at = filler.end;
            }
            into.extend_from_slice(&out[at..]);
            out.clear();
        };
        let mut seq = EventSeq::FIRST;
        // A last read and event, so a gap at the end is reported too.
        for reader in readers.iter().copied().chain([Reader::Reads]) {
            match reader {
                Reader::Reads => take(&mut out, &mut fillers, &mut received),
                // Filler standing for unread output.
                Reader::Stalled => {
                    let start = out.len();
                    out.resize(start.max(BACKLOG), b' ');
                    fillers.push(start..out.len());
                }
            }
            write(&mut out, &mut lag, seq, &event);
            seq = seq.next();
        }
        take(&mut out, &mut fillers, &mut received);
        let mut next = 0;
        for line in parse(&received) {
            match line {
                Line::Event(seq) => {
                    prop_assert_eq!(seq, next);
                    next += 1;
                }
                Line::Lagged(from, to) => {
                    prop_assert_eq!(from, next);
                    prop_assert!(to >= from);
                    next = to + 1;
                }
            }
        }
        prop_assert_eq!(next, seq.get());
    }
}
