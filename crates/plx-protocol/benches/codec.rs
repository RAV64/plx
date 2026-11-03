//! Wall-clock benchmarks of the frame codec on a real full frame (a 200×50
//! libghostty screen of styled text, wide characters and clusters), against
//! the floor of copying the same bytes. Run with
//! `cargo bench -p plx-protocol`.

#![expect(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "a benchmark sets up fixed, valid inputs; a failure aborts the measurement"
)]

use std::hint::black_box;
use std::num::NonZeroUsize;

use divan::Bencher;
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
use plx_emulator::{Cell, Cursor, Emulator, GraphemeId, GridSize, Style, StyleId};
use plx_ghostty::{GhosttyFactory, GraphemeClustering, Options, Scrollback};
use plx_protocol::framing::{MAX_SERVER_MESSAGE, split};
use plx_protocol::server::{
    Decoder, FrameApply, FrameHeader, FrameSearch, Refused, ServerMessage, encode_outgoing,
};

fn main() {
    divan::main();
}

const LINE: &str = "\x1b[31mred \x1b[1;4:3mbold curly\x1b[0m 漢字 👍🏽 \x1b[38;2;10;200;30mtrue colour\x1b[0m plain text that fills the rest of the line ";

/// The daemon side: encodes what the session sends, or hands the full
/// frame to a benchmark.
struct Daemon<'a, 'b> {
    out: Vec<u8>,
    bench: Option<Bencher<'a, 'b>>,
}

impl Effects for Daemon<'_, '_> {
    fn spawn_pty(&mut self, _: PaneId, _: GridSize, _: PaneProgram<'_>) {}
    fn write_pty(&mut self, _: PaneId, _: &[u8]) {}
    fn resize_pty(&mut self, _: PaneId, _: GridSize) {}
    fn close_pty(&mut self, _: PaneId) {}
    fn send<E: Emulator>(&mut self, _: ClientId, message: Outgoing<'_, E>) {
        if let Some(bencher) = self.bench.take() {
            let mut buf = Vec::new();
            encode_outgoing(&message, &mut buf).expect("fits");
            bencher
                .counter(divan::counter::BytesCount::new(buf.len()))
                .bench_local(|| {
                    buf.clear();
                    encode_outgoing(&message, &mut buf)
                });
        }
        encode_outgoing(&message, &mut self.out).expect("fits");
    }
    fn close_client(&mut self, _: ClientId) {}
    fn event(&mut self, _: EventSeq, _: Event<'_>) {}
    fn exit(&mut self) {}
}

/// Runs a 200×50 session to its full frame; `bench` measures encoding it.
/// Returns the full frame's payload.
fn full_frame(bench: Option<Bencher<'_, '_>>) -> Vec<u8> {
    let size = GridSize::new(
        200.try_into().expect("not zero"),
        50.try_into().expect("not zero"),
    )
    .expect("within limits");
    let factory = GhosttyFactory {
        options: Options {
            scrollback: Scrollback::Disabled,
            continuation_max_bytes: NonZeroUsize::new(4096).expect("not zero"),
            grapheme_clustering: GraphemeClustering::Enabled,
        },
    };
    let mut daemon = Daemon {
        out: Vec::new(),
        bench: None,
    };
    let mut session = Session::new(
        factory,
        Keymap::unlock_first(),
        SizePolicy::Latest,
        &mut daemon,
    );
    let client = session.connect();
    session.step(
        Timestamp::from_nanos(0),
        Input::Client {
            client,
            message: ClientMessage::Attach {
                size,
                cell_px: None,
            },
        },
        &mut daemon,
    );
    session.step(
        Timestamp::from_nanos(1),
        Input::Client {
            client,
            message: ClientMessage::Ack(FrameSeq::FIRST),
        },
        &mut daemon,
    );
    daemon.out.clear();
    let pane = session.structure().panes()[0];
    let mut screen = String::from("\x1b[H");
    for _ in 0..49 {
        screen.push_str(&LINE.repeat(4).chars().take(190).collect::<String>());
        screen.push_str("\r\n");
    }
    session.step(
        Timestamp::from_nanos(2),
        Input::PtyOutput {
            pane,
            bytes: screen.as_bytes(),
        },
        &mut daemon,
    );
    daemon.bench = bench;
    session.step(Timestamp::from_nanos(100_000_000), Input::Tick, &mut daemon);
    let (payload, used) = split(&daemon.out, MAX_SERVER_MESSAGE)
        .expect("framed")
        .expect("a frame came");
    assert_eq!(used, daemon.out.len(), "one frame");
    let prefix = used - payload.len();
    let mut frame = daemon.out;
    frame.drain(..prefix);
    frame
}

/// Takes a frame the way the mirror does: rows decoded into a grid.
struct Grid {
    cells: Vec<Cell>,
    cols: usize,
}

impl FrameApply for Grid {
    fn begin(
        &mut self,
        header: FrameHeader,
        search: Option<FrameSearch<'_>>,
    ) -> Result<(), Refused> {
        black_box((header, search));
        Ok(())
    }
    fn tab(&mut self, name: Option<&str>, zoom: Zoom) -> Result<(), Refused> {
        black_box((name, zoom));
        Ok(())
    }
    fn placement(&mut self, placement: Placement) -> Result<(), Refused> {
        black_box(placement);
        Ok(())
    }
    fn separator(&mut self, separator: Separator) -> Result<(), Refused> {
        black_box(separator);
        Ok(())
    }
    fn pane(&mut self, _: PaneId, rect: Rect, cursor: Cursor) -> Result<(), Refused> {
        self.cols = usize::from(rect.cols());
        self.cells.resize(rect.size().area(), Cell::BLANK);
        black_box(cursor);
        Ok(())
    }
    fn style(&mut self, id: StyleId, style: Style) -> Result<(), Refused> {
        black_box((id, style));
        Ok(())
    }
    fn grapheme(&mut self, id: GraphemeId, codepoints: &[char]) -> Result<(), Refused> {
        black_box((id, codepoints));
        Ok(())
    }
    fn row_slot(&mut self, y: u16) -> Result<&mut [Cell], Refused> {
        let start = usize::from(y) * self.cols;
        Ok(&mut self.cells[start..start + self.cols])
    }
    fn row_filled(&mut self, y: u16) -> Result<(), Refused> {
        black_box(y);
        Ok(())
    }
    fn end(&mut self) -> Result<(), Refused> {
        Ok(())
    }
    fn clipboard(&mut self, _: &[u8]) -> Result<(), Refused> {
        Ok(())
    }
}

/// Encoding the full frame from the session's borrowed view.
#[divan::bench]
fn encode_full_frame(bencher: Bencher) {
    full_frame(Some(bencher));
}

/// Decoding it into a grid, every cell and value checked.
#[divan::bench]
fn decode_full_frame(bencher: Bencher) {
    let payload = full_frame(None);
    let mut decoder = Decoder::new();
    let mut grid = Grid {
        cells: Vec::new(),
        cols: 0,
    };
    bencher
        .counter(divan::counter::BytesCount::new(payload.len()))
        .bench_local(|| {
            let message = decoder.decode(&payload, &mut grid).expect("valid");
            assert!(matches!(message, ServerMessage::Frame(_)));
        });
}

/// The floor: copying the frame's bytes.
#[divan::bench]
fn copy_full_frame(bencher: Bencher) {
    let payload = full_frame(None);
    let mut copy = vec![0; payload.len()];
    bencher
        .counter(divan::counter::BytesCount::new(payload.len()))
        .bench_local(|| {
            copy.copy_from_slice(black_box(&payload));
            black_box(&copy);
        });
}
