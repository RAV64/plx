use plx_alloc_count::measure;
use plx_core::effects::{Effects, PaneProgram};
use plx_core::event::{Event, EventSeq};
use plx_core::geometry::Rect;
use plx_core::id::{ClientId, PaneId};
use plx_core::input::Input;
use plx_core::layout::{Placement, Separator};
use plx_core::message::{ClientMessage, FrameSeq, Outgoing};
use plx_core::mode::Keymap;
use plx_core::session::Session;
use plx_core::size::SizePolicy;
use plx_core::tab::Zoom;
use plx_core::time::Timestamp;
use plx_emulator::{
    Cell, Cursor, Emulator, FakeFactory, GlyphKind, GraphemeId, GridSize, Key, KeyChar, KeyEvent,
    Mods, Percent, Progress, SearchStatus, Style, StyleId,
};
use plx_protocol::framing::{MAX_SERVER_MESSAGE, split};
use plx_protocol::server::{
    Decoder, FrameApply, FrameHeader, FrameSearch, Refused, ServerMessage, encode_outgoing,
};
use proptest::prelude::*;

/// Encodes everything the session sends, as the daemon's sink would.
#[derive(Default)]
struct Wire {
    bytes: Vec<u8>,
    /// The rows each frame carried, read from the borrowed view.
    sent_rows: Vec<Vec<(PaneId, u16, String)>>,
}

fn text(cells: &[Cell]) -> String {
    let s: String = cells
        .iter()
        .map(|cell| match cell.glyph.kind() {
            GlyphKind::Char(c) => c,
            GlyphKind::Empty | GlyphKind::Cluster(_) => ' ',
        })
        .collect();
    s.trim_end().to_owned()
}

impl Effects for Wire {
    fn spawn_pty(&mut self, _: PaneId, _: GridSize, _: PaneProgram<'_>) {}
    fn write_pty(&mut self, _: PaneId, _: &[u8]) {}
    fn resize_pty(&mut self, _: PaneId, _: GridSize) {}
    fn close_pty(&mut self, _: PaneId) {}
    fn send<E: Emulator>(&mut self, _: ClientId, message: Outgoing<'_, E>) {
        if let Outgoing::Frame(view) = &message {
            self.sent_rows.push(
                view.panes()
                    .flat_map(|update| {
                        update
                            .rows()
                            .map(move |(y, cells)| (update.pane, y, text(cells)))
                    })
                    .collect(),
            );
        }
        encode_outgoing(&message, &mut self.bytes).unwrap();
    }
    fn close_client(&mut self, _: ClientId) {}
    fn event(&mut self, _: EventSeq, _: Event<'_>) {}
    fn exit(&mut self) {}
}

/// Records what a frame decodes to.
#[derive(Default, Debug, PartialEq, Eq)]
struct Received {
    headers: Vec<FrameHeader>,
    searches: Vec<Option<(String, SearchStatus)>>,
    tabs: Vec<Option<String>>,
    placements: Vec<Placement>,
    separators: Vec<Separator>,
    panes: Vec<(PaneId, Rect, Cursor)>,
    styles: Vec<(StyleId, Style)>,
    graphemes: Vec<(GraphemeId, Vec<char>)>,
    rows: Vec<(PaneId, u16, String)>,
    ends: usize,
    clipboard: Vec<Vec<u8>>,
    /// The row being decoded.
    slot: Vec<Cell>,
}

impl FrameApply for Received {
    fn begin(
        &mut self,
        header: FrameHeader,
        search: Option<FrameSearch<'_>>,
    ) -> Result<(), Refused> {
        self.headers.push(header);
        self.searches
            .push(search.map(|search| (String::from(search.needle.as_str()), search.status)));
        Ok(())
    }
    fn tab(&mut self, name: Option<&str>, _: Zoom) -> Result<(), Refused> {
        self.tabs.push(name.map(str::to_owned));
        Ok(())
    }
    fn placement(&mut self, placement: Placement) -> Result<(), Refused> {
        self.placements.push(placement);
        Ok(())
    }
    fn separator(&mut self, separator: Separator) -> Result<(), Refused> {
        self.separators.push(separator);
        Ok(())
    }
    fn pane(&mut self, pane: PaneId, rect: Rect, cursor: Cursor) -> Result<(), Refused> {
        self.panes.push((pane, rect, cursor));
        Ok(())
    }
    fn style(&mut self, id: StyleId, style: Style) -> Result<(), Refused> {
        self.styles.push((id, style));
        Ok(())
    }
    fn grapheme(&mut self, id: GraphemeId, codepoints: &[char]) -> Result<(), Refused> {
        self.graphemes.push((id, codepoints.to_vec()));
        Ok(())
    }
    fn row_slot(&mut self, _: u16) -> Result<&mut [Cell], Refused> {
        let (_, rect, _) = self.panes.last().ok_or(Refused("a row before any pane"))?;
        self.slot.clear();
        self.slot.resize(usize::from(rect.cols()), Cell::BLANK);
        Ok(&mut self.slot)
    }
    fn row_filled(&mut self, y: u16) -> Result<(), Refused> {
        let (pane, _, _) = self.panes.last().ok_or(Refused("a row before any pane"))?;
        self.rows.push((*pane, y, text(&self.slot)));
        Ok(())
    }
    fn end(&mut self) -> Result<(), Refused> {
        self.ends += 1;
        Ok(())
    }
    fn clipboard(&mut self, text: &[u8]) -> Result<(), Refused> {
        self.clipboard.push(Vec::from(text));
        Ok(())
    }
}

fn size(cols: u16, rows: u16) -> GridSize {
    GridSize::new(cols.try_into().unwrap(), rows.try_into().unwrap()).unwrap()
}

/// A session with an attached client, after `outputs` and acks.
fn run(outputs: &[&[u8]]) -> Wire {
    run_sized(size(40, 10), outputs)
}

/// The same with a terminal of `terminal`.
fn run_sized(terminal: GridSize, outputs: &[&[u8]]) -> Wire {
    let mut wire = Wire::default();
    let mut session = Session::new(
        FakeFactory,
        Keymap::unlock_first(),
        SizePolicy::Latest,
        &mut wire,
    );
    let client = session.connect();
    let message = ClientMessage::Attach {
        size: terminal,
        cell_px: None,
    };
    session.step(
        Timestamp::ZERO,
        Input::Client { client, message },
        &mut wire,
    );
    let mut seq = FrameSeq::FIRST;
    for (i, bytes) in outputs.iter().enumerate() {
        let at = Timestamp::from_nanos((u64::try_from(i).unwrap() + 1) * 20_000_000);
        session.step(
            at,
            Input::Client {
                client,
                message: ClientMessage::Ack(seq),
            },
            &mut wire,
        );
        seq = seq.next();
        session.step(
            at,
            Input::PtyOutput {
                pane: PaneId::new(0),
                bytes,
            },
            &mut wire,
        );
    }
    wire
}

fn decode_all(bytes: &[u8]) -> (Vec<ServerMessage>, Received) {
    let mut decoder = Decoder::new();
    let mut received = Received::default();
    let mut messages = Vec::new();
    let mut rest = bytes;
    while let Some((payload, used)) = split(rest, MAX_SERVER_MESSAGE).unwrap() {
        messages.push(decoder.decode(payload, &mut received).unwrap());
        rest = &rest[used..];
    }
    assert_eq!(rest, b"");
    (messages, received)
}

#[test]
fn frames_decode_to_exactly_what_the_view_held() {
    let wire = run(&[b"hello", b"\r\nworld"]);
    let (messages, received) = decode_all(&wire.bytes);
    assert!(
        messages
            .iter()
            .all(|m| matches!(m, ServerMessage::Frame(_)))
    );
    assert_eq!(received.headers.len(), messages.len());
    assert_eq!(received.ends, messages.len());
    let sent: Vec<(PaneId, u16, String)> = wire.sent_rows.into_iter().flatten().collect();
    assert_eq!(received.rows, sent);
    assert!(
        received
            .rows
            .contains(&(PaneId::new(0), 1, String::from("world")))
    );
    assert_eq!(received.headers[0].size, size(40, 10));
}

/// The focused pane's progress report travels in the frame's header.
#[test]
fn progress_reports_travel_in_the_header() {
    let wire = run(&[b"\x1b]9;4;4;64\x07", b"x", b"\x1b]9;4;0\x07"]);
    let (_, received) = decode_all(&wire.bytes);
    let progress: Vec<Option<Progress>> = received
        .headers
        .iter()
        .map(|header| header.progress)
        .collect();
    let paused = Progress::Paused(Some(Percent::new(64).unwrap()));
    assert_eq!(
        progress,
        [None, Some(paused), None, Some(Progress::Removed)]
    );
}

#[test]
fn wide_rows_travel_whole() {
    // 300 columns: 2400 bytes in one row.
    let line: Vec<u8> = (0..300_u16)
        .map(|x| b'a' + u8::try_from(x % 26).unwrap())
        .collect();
    let wire = run_sized(size(300, 3), &[&line]);
    let (_, received) = decode_all(&wire.bytes);
    let sent: Vec<(PaneId, u16, String)> = wire.sent_rows.into_iter().flatten().collect();
    assert_eq!(received.rows, sent);
    let expected = String::from_utf8(line).unwrap();
    assert!(received.rows.iter().any(|(_, _, text)| *text == expected));
}

/// A 3×2 session's frame after "ab": the wire format, pinned.
#[rustfmt::skip]
const GOLDEN_FRAME: &[u8] = &[
    63, 0, 0, 0, // the payload's length
    // Header (the focused pane's scroll position: 0 back of 0; then no
    // progress report and no search), tabs (one: unnamed, tiled),
    // arrangement, then the pane's id, rect, cursor, styles and clusters.
    0, 1, 3, 2, 0, 0, 0, 0, 0, 0, 0,
    1, 0, 0,
    1, 0, 1, 0, 0, 3, 1, 0, 1, 0, 0, 0, 3, 1, 1, 2, 0, 0, 0, 0, 0, 0,
    // Its rows: one, y 0, 24 bytes holding `a`, `b` and a blank (glyph
    // LE, style id, width).
    1, 0, 24,
    97, 0, 0, 0, 0, 0, 0, 0,
    98, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0,
];

#[test]
fn frame_golden_bytes() {
    let wire = run_sized(size(3, 2), &[b"ab"]);
    let (_, first) = split(&wire.bytes, MAX_SERVER_MESSAGE).unwrap().unwrap();
    let second = &wire.bytes[first..];
    let (_, used) = split(second, MAX_SERVER_MESSAGE).unwrap().unwrap();
    assert_eq!(&second[..used], GOLDEN_FRAME);
}

#[test]
fn a_row_of_partial_cells_is_refused() {
    let wire = run_sized(size(3, 2), &[b"ab"]);
    let (_, first) = split(&wire.bytes, MAX_SERVER_MESSAGE).unwrap().unwrap();
    let (payload, _) = split(&wire.bytes[first..], MAX_SERVER_MESSAGE)
        .unwrap()
        .unwrap();
    // The row's bytes are the frame's last 1 + 3×8; shortening their
    // length by one byte leaves a partial cell.
    let block_len = 3 * 8;
    let at = payload
        .windows(1 + block_len)
        .rposition(|w| usize::from(w[0]) == block_len)
        .unwrap();
    let mut broken = payload.to_vec();
    broken[at] -= 1;
    broken.remove(at + block_len);
    let mut decoder = Decoder::new();
    let mut received = Received::default();
    assert!(decoder.decode(&broken, &mut received).is_err());
}

#[test]
fn a_mirror_can_refuse_a_frame_and_says_why() {
    struct Refuser;
    impl FrameApply for Refuser {
        fn begin(&mut self, _: FrameHeader, _: Option<FrameSearch<'_>>) -> Result<(), Refused> {
            Err(Refused("not attached here"))
        }
        fn tab(&mut self, _: Option<&str>, _: Zoom) -> Result<(), Refused> {
            Ok(())
        }
        fn placement(&mut self, _: Placement) -> Result<(), Refused> {
            Ok(())
        }
        fn separator(&mut self, _: Separator) -> Result<(), Refused> {
            Ok(())
        }
        fn pane(&mut self, _: PaneId, _: Rect, _: Cursor) -> Result<(), Refused> {
            Ok(())
        }
        fn style(&mut self, _: StyleId, _: Style) -> Result<(), Refused> {
            Ok(())
        }
        fn grapheme(&mut self, _: GraphemeId, _: &[char]) -> Result<(), Refused> {
            Ok(())
        }
        fn row_slot(&mut self, _: u16) -> Result<&mut [Cell], Refused> {
            Err(Refused("not attached here"))
        }
        fn row_filled(&mut self, _: u16) -> Result<(), Refused> {
            Ok(())
        }
        fn end(&mut self) -> Result<(), Refused> {
            Ok(())
        }
        fn clipboard(&mut self, _: &[u8]) -> Result<(), Refused> {
            Ok(())
        }
    }
    let wire = run(&[]);
    let (payload, _) = split(&wire.bytes, MAX_SERVER_MESSAGE).unwrap().unwrap();
    let error = Decoder::new().decode(payload, &mut Refuser).unwrap_err();
    assert_eq!(error.to_string(), "invalid message: not attached here");
}

proptest! {
    /// Garbage never panics the frame decoder.
    #[test]
    fn garbage_frames_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..128)) {
        let _result = Decoder::new().decode(&bytes, &mut Received::default());
    }

    /// Every prefix of a real frame is an error, never a panic or a
    /// partial success.
    #[test]
    fn truncated_frames_are_errors(cut in any::<usize>()) {
        let wire = run(&[b"some text"]);
        let (payload, _) = split(&wire.bytes, MAX_SERVER_MESSAGE).unwrap().unwrap();
        let cut = cut % payload.len();
        prop_assert!(Decoder::new().decode(&payload[..cut], &mut Received::default()).is_err());
    }
}

/// Keeps only the row being decoded.
struct Discard(Vec<Cell>);
impl FrameApply for Discard {
    fn begin(&mut self, _: FrameHeader, _: Option<FrameSearch<'_>>) -> Result<(), Refused> {
        Ok(())
    }
    fn tab(&mut self, _: Option<&str>, _: Zoom) -> Result<(), Refused> {
        Ok(())
    }
    fn placement(&mut self, _: Placement) -> Result<(), Refused> {
        Ok(())
    }
    fn separator(&mut self, _: Separator) -> Result<(), Refused> {
        Ok(())
    }
    fn pane(&mut self, _: PaneId, rect: Rect, _: Cursor) -> Result<(), Refused> {
        self.0.resize(usize::from(rect.cols()), Cell::BLANK);
        Ok(())
    }
    fn style(&mut self, _: StyleId, _: Style) -> Result<(), Refused> {
        Ok(())
    }
    fn grapheme(&mut self, _: GraphemeId, _: &[char]) -> Result<(), Refused> {
        Ok(())
    }
    fn row_slot(&mut self, _: u16) -> Result<&mut [Cell], Refused> {
        Ok(&mut self.0)
    }
    fn row_filled(&mut self, _: u16) -> Result<(), Refused> {
        std::hint::black_box(&self.0);
        Ok(())
    }
    fn end(&mut self) -> Result<(), Refused> {
        Ok(())
    }
    fn clipboard(&mut self, _: &[u8]) -> Result<(), Refused> {
        Ok(())
    }
}

#[test]
fn steady_state_encoding_and_decoding_allocate_nothing() {
    struct Sink {
        bytes: Vec<u8>,
    }
    impl Effects for Sink {
        fn spawn_pty(&mut self, _: PaneId, _: GridSize, _: PaneProgram<'_>) {}
        fn write_pty(&mut self, _: PaneId, _: &[u8]) {}
        fn resize_pty(&mut self, _: PaneId, _: GridSize) {}
        fn close_pty(&mut self, _: PaneId) {}
        fn send<E: Emulator>(&mut self, _: ClientId, message: Outgoing<'_, E>) {
            self.bytes.clear();
            encode_outgoing(&message, &mut self.bytes).unwrap();
        }
        fn close_client(&mut self, _: ClientId) {}
        fn event(&mut self, _: EventSeq, _: Event<'_>) {}
        fn exit(&mut self) {}
    }
    let mut sink = Sink { bytes: Vec::new() };
    let mut session = Session::new(
        FakeFactory,
        Keymap::unlock_first(),
        SizePolicy::Latest,
        &mut sink,
    );
    let client = session.connect();
    let message = ClientMessage::Attach {
        size: size(80, 24),
        cell_px: None,
    };
    session.step(
        Timestamp::ZERO,
        Input::Client { client, message },
        &mut sink,
    );
    let mut decoder = Decoder::new();
    let mut seq = FrameSeq::FIRST;
    let mut now = 0_u64;
    let mut discard = Discard(Vec::new());
    let mut round = |session: &mut Session<FakeFactory>, sink: &mut Sink, decoder: &mut Decoder| {
        now += 20_000_000;
        let at = Timestamp::from_nanos(now);
        session.step(
            at,
            Input::Client {
                client,
                message: ClientMessage::Ack(seq),
            },
            sink,
        );
        seq = seq.next();
        session.step(
            at,
            Input::PtyOutput {
                pane: PaneId::new(0),
                bytes: b"output line\r\n",
            },
            sink,
        );
        let (payload, _) = split(&sink.bytes, MAX_SERVER_MESSAGE).unwrap().unwrap();
        decoder.decode(payload, &mut discard).unwrap();
    };
    for _ in 0..50 {
        round(&mut session, &mut sink, &mut decoder);
    }
    let (counts, ()) = measure(|| {
        for _ in 0..200 {
            round(&mut session, &mut sink, &mut decoder);
        }
    });
    assert_eq!(counts.allocations, 0, "{counts:?}");
}

/// Clipboard text travels whole, any text, and reaches the receiver.
#[test]
fn clipboard_text_reaches_the_receiver() {
    use plx_emulator::FakeEmulator;
    for text in ["hello", "", "multi\nline \u{1F468}\u{200D}\u{1F469} text"] {
        let mut out = Vec::new();
        plx_protocol::server::encode_outgoing::<FakeEmulator>(
            &plx_core::message::Outgoing::Clipboard(plx_core::message::Copied::new(
                text.as_bytes(),
            )),
            &mut out,
        )
        .unwrap();
        let (payload, _) = split(&out, MAX_SERVER_MESSAGE).unwrap().unwrap();
        let mut received = Received::default();
        let message = Decoder::new().decode(payload, &mut received).unwrap();
        assert_eq!(message, ServerMessage::Clipboard);
        assert_eq!(received.clipboard, [text.as_bytes()]);
    }
}

/// A client typing a needle in search mode sees it in its frames, any text
/// included; before the search, frames carry none.
#[test]
fn a_search_reaches_the_receiver_with_its_needle() {
    let mut wire = Wire::default();
    let mut session = Session::new(
        FakeFactory,
        Keymap::unlock_first(),
        SizePolicy::Latest,
        &mut wire,
    );
    let client = session.connect();
    let message = ClientMessage::Attach {
        size: size(40, 10),
        cell_px: None,
    };
    session.step(
        Timestamp::ZERO,
        Input::Client { client, message },
        &mut wire,
    );
    let char_key = |c| KeyEvent::press(Key::Char(KeyChar::new(c).unwrap()), Mods::empty());
    let keys = [
        KeyEvent::press(Key::Char(KeyChar::new('g').unwrap()), Mods::CTRL),
        char_key('s'),
        char_key('/'),
        char_key('h'),
        char_key('\u{e9}'),
    ];
    let mut seq = FrameSeq::FIRST;
    for (i, key) in keys.into_iter().enumerate() {
        let at = Timestamp::from_nanos((u64::try_from(i).unwrap() + 1) * 20_000_000);
        for message in [ClientMessage::Ack(seq), ClientMessage::Key(key)] {
            session.step(at, Input::Client { client, message }, &mut wire);
        }
        seq = seq.next();
    }
    let (messages, received) = decode_all(&wire.bytes);
    assert!(
        messages
            .iter()
            .all(|message| matches!(message, ServerMessage::Frame(_))),
        "{messages:?}"
    );
    assert_eq!(received.searches.first(), Some(&None));
    assert_eq!(
        received.searches.last(),
        Some(&Some((String::from("h\u{e9}"), SearchStatus::default())))
    );
}
