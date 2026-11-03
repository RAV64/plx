//! Wall-clock benchmarks of the core's hot paths, on the fake emulator so
//! they measure the core alone. Run with `cargo bench -p plx-core`.

#![expect(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "a benchmark sets up fixed, valid inputs; a failure aborts the measurement"
)]

use divan::Bencher;
use plx_core::effects::{Effects, PaneProgram};
use plx_core::event::{Event, EventSeq};
use plx_core::geometry::{Direction, Rect};
use plx_core::id::{ClientId, PaneId};
use plx_core::input::Input;
use plx_core::layout::{Arrangement, Layout};
use plx_core::message::{ClientMessage, FrameSeq, Outgoing};
use plx_core::mode::{Keymap, Mode};
use plx_core::session::Session;
use plx_core::size::SizePolicy;
use plx_core::time::Timestamp;
use plx_emulator::{Emulator, FakeFactory, GridSize, Key, KeyChar, KeyEvent, Mods};

fn main() {
    divan::main();
}

fn size(cols: u16, rows: u16) -> GridSize {
    GridSize::new(
        cols.try_into().expect("not zero"),
        rows.try_into().expect("not zero"),
    )
    .expect("within limits")
}

/// Eight panes in nested splits.
fn layout() -> Layout {
    let mut layout = Layout::single(PaneId::new(0));
    let directions = [
        Direction::Right,
        Direction::Down,
        Direction::Left,
        Direction::Up,
    ];
    for n in 1..8_u64 {
        let target = layout.panes()[usize::try_from(n).expect("small") / 2];
        layout.split(
            target,
            directions[usize::try_from(n % 4).expect("small")],
            PaneId::new(n),
        );
    }
    layout
}

#[divan::bench]
fn arrange_eight_panes(bencher: Bencher) {
    let layout = layout();
    let area = Rect::at_origin(size(200, 60));
    let mut arrangement = Arrangement::default();
    bencher.bench_local(|| {
        layout.arrange_into(area, &mut arrangement);
        arrangement.panes.len()
    });
}

#[divan::bench]
fn resolve_a_key(bencher: Bencher) {
    let keymap = Keymap::unlock_first();
    let key = KeyEvent::press(Key::Char(KeyChar::from_byte(b'x')), Mods::empty());
    bencher.bench_local(|| keymap.resolve(Mode::Locked, &key));
}

/// A sink that walks frames as an encoder would and discards everything.
struct Discard;

impl Effects for Discard {
    fn spawn_pty(&mut self, _: PaneId, _: GridSize, _: PaneProgram<'_>) {}
    fn write_pty(&mut self, _: PaneId, bytes: &[u8]) {
        divan::black_box(bytes);
    }
    fn resize_pty(&mut self, _: PaneId, _: GridSize) {}
    fn close_pty(&mut self, _: PaneId) {}
    fn send<E: Emulator>(&mut self, _: ClientId, message: Outgoing<'_, E>) {
        if let Outgoing::Frame(view) = message {
            for update in view.panes() {
                for (_, row) in update.rows() {
                    divan::black_box(row);
                }
                for style in update.styles() {
                    divan::black_box(style);
                }
            }
        }
    }
    fn close_client(&mut self, _: ClientId) {}
    fn event(&mut self, _: EventSeq, event: Event<'_>) {
        divan::black_box(event);
    }
    fn exit(&mut self) {}
}

/// One round of the steady state: a line of output, a keystroke, a frame
/// built and walked, its ack.
#[divan::bench]
fn output_frame_ack_round(bencher: Bencher) {
    let mut fx = Discard;
    let mut session = Session::new(
        FakeFactory,
        Keymap::unlock_first(),
        SizePolicy::Latest,
        &mut fx,
    );
    let client = session.connect();
    let message = ClientMessage::Attach {
        size: size(200, 60),
        cell_px: None,
    };
    session.step(Timestamp::ZERO, Input::Client { client, message }, &mut fx);
    let pane = PaneId::new(0);
    let key = KeyEvent::press(Key::Char(KeyChar::from_byte(b'x')), Mods::empty());
    let mut now = 0_u64;
    let mut seq = FrameSeq::FIRST;
    bencher.bench_local(|| {
        // Each round a frame interval apart, so every round sends a frame.
        now += 20_000_000;
        let at = Timestamp::from_nanos(now);
        let output = Input::PtyOutput {
            pane,
            bytes: b"line of output from a busy program\r\n",
        };
        session.step(at, output, &mut fx);
        let message = ClientMessage::Key(key);
        session.step(at, Input::Client { client, message }, &mut fx);
        let message = ClientMessage::Ack(seq);
        session.step(at, Input::Client { client, message }, &mut fx);
        seq = seq.next();
    });
}
