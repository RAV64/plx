use std::collections::BTreeMap;
use std::fmt::Write as _;

use plx_client_core::mirror::{Mirror, TabLabel};
use plx_core::command::{Command, SessionCommand};
use plx_core::effects::{Effects, PaneProgram};
use plx_core::event::{Event, EventSeq};
use plx_core::geometry::Direction;
use plx_core::id::{ClientId, PaneId};
use plx_core::input::Input;
use plx_core::message::{ClientMessage, Outgoing};
use plx_core::mode::Keymap;
use plx_core::session::Session;
use plx_core::size::SizePolicy;
use plx_core::tab::Zoom;
use plx_core::time::Timestamp;
use plx_emulator::{Emulator, FakeFactory, GlyphKind, GridSize, Style};
use plx_protocol::framing::{MAX_SERVER_MESSAGE, split};
use plx_protocol::server::{Decoder, ServerMessage, encode_outgoing};
use proptest::prelude::*;

/// Per-client send buffers, as the daemon keeps them.
#[derive(Default)]
pub(crate) struct Daemon {
    pub(crate) outboxes: BTreeMap<ClientId, Vec<u8>>,
}

impl Effects for Daemon {
    fn spawn_pty(&mut self, _: PaneId, _: GridSize, _: PaneProgram<'_>) {}
    fn write_pty(&mut self, _: PaneId, _: &[u8]) {}
    fn resize_pty(&mut self, _: PaneId, _: GridSize) {}
    fn close_pty(&mut self, _: PaneId) {}
    fn send<E: Emulator>(&mut self, client: ClientId, message: Outgoing<'_, E>) {
        encode_outgoing(&message, self.outboxes.entry(client).or_default()).unwrap();
    }
    fn close_client(&mut self, _: ClientId) {}
    fn event(&mut self, _: EventSeq, _: Event<'_>) {}
    fn exit(&mut self) {}
}

/// A client: its mirror and decoder.
struct Client {
    id: ClientId,
    mirror: Mirror,
    decoder: Decoder,
}

impl Client {
    /// Applies everything the daemon sent; returns whether a frame came.
    fn receive(&mut self, daemon: &mut Daemon) -> bool {
        let outbox = daemon.outboxes.entry(self.id).or_default();
        let mut framed = false;
        while let Some((payload, used)) = split(outbox, MAX_SERVER_MESSAGE).unwrap() {
            let message = self.decoder.decode(payload, &mut self.mirror).unwrap();
            framed |= matches!(message, ServerMessage::Frame(_));
            outbox.drain(..used);
        }
        framed
    }
}

/// What a mirror shows: chrome and every visible cell, resolved.
fn shows(mirror: &Mirror) -> String {
    let mut out = String::new();
    let header = mirror.header().unwrap();
    writeln!(
        out,
        "{:?} {:?} active {} focus {:?}",
        header.size, header.mode, header.active, header.focus
    )
    .unwrap();
    for tab in mirror.tabs() {
        match &tab.label {
            TabLabel::Unnamed => out.push_str("[#"),
            TabLabel::Named(name) => write!(out, "[{}", name.as_str()).unwrap(),
        }
        out.push_str(match tab.zoom {
            Zoom::Tiled => "]",
            Zoom::Zoomed => " zoomed]",
        });
    }
    writeln!(
        out,
        "\n{:?}\n{:?}",
        mirror.placements(),
        mirror.separators()
    )
    .unwrap();
    for (id, pane) in mirror.panes() {
        writeln!(out, "pane {id:?} {:?} {:?}", pane.rect(), pane.cursor()).unwrap();
        let mut y = 0;
        while let Some(row) = pane.row_or_none(y) {
            for cell in row {
                let text: String = match cell.glyph.kind() {
                    GlyphKind::Empty => String::from(" "),
                    GlyphKind::Char(c) => c.to_string(),
                    GlyphKind::Cluster(id) => pane.grapheme(id).iter().collect(),
                };
                let style = pane.style(cell.style);
                if style == Style::DEFAULT {
                    out.push_str(&text);
                } else {
                    write!(out, "<{text}{style:?}>").unwrap();
                }
            }
            out.push('\n');
            y += 1;
        }
    }
    out
}

#[derive(Clone, Debug)]
enum Op {
    Output(usize, Vec<u8>),
    Split(bool),
    NewTab,
    NextTab,
    ClosePane,
    Zoom,
    Resize(u16, u16),
    Ack,
    Tick,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => (any::<usize>(), proptest::collection::vec(prop_oneof![
            Just(b'a'), Just(b'Z'), Just(b' '), Just(b'\n'), Just(b'\r')
        ], 0..60)).prop_map(|(i, b)| Op::Output(i, b)),
        1 => any::<bool>().prop_map(Op::Split),
        1 => Just(Op::NewTab),
        1 => Just(Op::NextTab),
        1 => Just(Op::ClosePane),
        1 => Just(Op::Zoom),
        1 => (5_u16..90, 3_u16..30).prop_map(|(c, r)| Op::Resize(c, r)),
        4 => Just(Op::Ack),
        2 => Just(Op::Tick),
    ]
}

pub(crate) fn size(cols: u16, rows: u16) -> GridSize {
    GridSize::new(cols.try_into().unwrap(), rows.try_into().unwrap()).unwrap()
}

proptest! {
    /// However frames were coalesced and whatever happened in between, a
    /// client fed incrementally shows exactly what a freshly attached
    /// client's full frame shows.
    #[test]
    fn incremental_frames_equal_one_full_redraw(ops in proptest::collection::vec(op(), 0..60)) {
        let mut daemon = Daemon::default();
        let mut session = Session::new(FakeFactory, Keymap::unlock_first(), SizePolicy::Latest, &mut daemon);
        let mut now = 0_u64;
        let mut step = |session: &mut Session<FakeFactory>, daemon: &mut Daemon, input: Input<'_>| {
            now += 20_000_000;
            session.step(Timestamp::from_nanos(now), input, daemon);
        };
        let mut a = Client { id: session.connect(), mirror: Mirror::new(), decoder: Decoder::new() };
        step(&mut session, &mut daemon, Input::Client {
            client: a.id,
            message: ClientMessage::Attach { size: size(60, 20), cell_px: None },
        });
        let mut unacked = a.receive(&mut daemon);
        for op in ops {
            let panes = session.structure().panes();
            let command = |c| Command::Session(c);
            match op {
                Op::Output(i, bytes) => {
                    let pane = panes[i % panes.len()];
                    step(&mut session, &mut daemon, Input::PtyOutput { pane, bytes: &bytes });
                }
                Op::Split(right) => {
                    let direction = if right { Direction::Right } else { Direction::Down };
                    let split = command(SessionCommand::Split { direction });
                    step(&mut session, &mut daemon, Input::Client { client: a.id, message: ClientMessage::Command(&split) });
                }
                Op::NewTab => {
                    let new = command(SessionCommand::NewTab);
                    step(&mut session, &mut daemon, Input::Client { client: a.id, message: ClientMessage::Command(&new) });
                }
                Op::NextTab => {
                    let next = command(SessionCommand::NextTab);
                    step(&mut session, &mut daemon, Input::Client { client: a.id, message: ClientMessage::Command(&next) });
                }
                Op::ClosePane => {
                    // Never the last pane: that ends the session.
                    if panes.len() > 1 {
                        let close = command(SessionCommand::ClosePane);
                        step(&mut session, &mut daemon, Input::Client { client: a.id, message: ClientMessage::Command(&close) });
                    }
                }
                Op::Zoom => {
                    let zoom = command(SessionCommand::ToggleZoom);
                    step(&mut session, &mut daemon, Input::Client { client: a.id, message: ClientMessage::Command(&zoom) });
                }
                Op::Resize(cols, rows) => {
                    step(&mut session, &mut daemon, Input::Client {
                        client: a.id,
                        message: ClientMessage::Resize { size: size(cols, rows), cell_px: None },
                    });
                }
                Op::Ack => {
                    if unacked {
                        let seq = a.mirror.seq_or_none().unwrap();
                        step(&mut session, &mut daemon, Input::Client { client: a.id, message: ClientMessage::Ack(seq) });
                        unacked = false;
                    }
                }
                Op::Tick => step(&mut session, &mut daemon, Input::Tick),
            }
            unacked |= a.receive(&mut daemon);
        }
        // Let A catch up completely.
        for _ in 0..3 {
            if unacked {
                let seq = a.mirror.seq_or_none().unwrap();
                step(&mut session, &mut daemon, Input::Client { client: a.id, message: ClientMessage::Ack(seq) });
                unacked = false;
            }
            step(&mut session, &mut daemon, Input::Tick);
            unacked |= a.receive(&mut daemon);
        }
        // B attaches with A's size (so nothing resizes) and gets one full frame.
        let size = a.mirror.header().unwrap().size;
        let mut b = Client { id: session.connect(), mirror: Mirror::new(), decoder: Decoder::new() };
        step(&mut session, &mut daemon, Input::Client {
            client: b.id,
            message: ClientMessage::Attach { size, cell_px: None },
        });
        prop_assert!(b.receive(&mut daemon));
        // A sees B's attach as no change; its view must equal B's.
        let shown_a = shows(&a.mirror);
        let shown_b = shows(&b.mirror);
        // The headers differ only in the frame number.
        prop_assert_eq!(shown_a, shown_b);
    }
}
