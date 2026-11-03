//! Wall-clock benchmarks of the client's hot path: a frame decoded into the
//! mirror and drawn. Panes run on libghostty so frames carry real styles,
//! wide characters and clusters. Run with `cargo bench -p plx-client-core`.

#![expect(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "a benchmark sets up fixed, valid inputs; a failure aborts the measurement"
)]

use std::num::NonZeroUsize;

use divan::Bencher;
use plx_client_core::compose::{
    Capabilities, ColorDepth, Compositor, ProgressBars, UnderlineStyles,
};
use plx_client_core::mirror::Mirror;
use plx_core::effects::{Effects, PaneProgram};
use plx_core::event::{Event, EventSeq};
use plx_core::id::{ClientId, PaneId};
use plx_core::input::Input;
use plx_core::message::{ClientMessage, Outgoing};
use plx_core::mode::Keymap;
use plx_core::session::Session;
use plx_core::size::SizePolicy;
use plx_core::time::Timestamp;
use plx_emulator::{Emulator, GridSize};
use plx_ghostty::{GhosttyFactory, GraphemeClustering, Options, Scrollback};
use plx_protocol::framing::{MAX_SERVER_MESSAGE, split};
use plx_protocol::server::{Decoder, encode_outgoing};

fn main() {
    divan::main();
}

/// The one client's send buffer.
#[derive(Default)]
struct Outbox(Vec<u8>);

impl Effects for Outbox {
    fn spawn_pty(&mut self, _: PaneId, _: GridSize, _: PaneProgram<'_>) {}
    fn write_pty(&mut self, _: PaneId, _: &[u8]) {}
    fn resize_pty(&mut self, _: PaneId, _: GridSize) {}
    fn close_pty(&mut self, _: PaneId) {}
    fn send<E: Emulator>(&mut self, _: ClientId, message: Outgoing<'_, E>) {
        encode_outgoing(&message, &mut self.0).expect("a frame fits");
    }
    fn close_client(&mut self, _: ClientId) {}
    fn event(&mut self, _: EventSeq, _: Event<'_>) {}
    fn exit(&mut self) {}
}

const STYLED: &str = "\x1b[31mred \x1b[1;4:3mbold curly\x1b[0m 漢字 👍🏽 \x1b[38;2;10;200;30mtrue colour\x1b[0m plain text that fills the rest of the line ";
const PLAIN: &str = "plain text, every cell one column wide and unstyled, as in a log ";

/// A 200×50 session whose one pane is filled with `line`, decoded into a
/// mirror.
fn mirror(line: &str) -> (Mirror, GridSize) {
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
    let mut outbox = Outbox::default();
    let mut session = Session::new(
        factory,
        Keymap::unlock_first(),
        SizePolicy::Latest,
        &mut outbox,
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
        &mut outbox,
    );
    let mut mirror = Mirror::new();
    let mut decoder = Decoder::new();
    receive(&mut outbox, &mut decoder, &mut mirror);
    let seq = mirror.seq_or_none().expect("attached");
    session.step(
        Timestamp::from_nanos(1),
        Input::Client {
            client,
            message: ClientMessage::Ack(seq),
        },
        &mut outbox,
    );
    let pane = session.structure().panes()[0];
    let mut screen = String::from("\x1b[H");
    for _ in 0..49 {
        screen.push_str(&line.repeat(4).chars().take(190).collect::<String>());
        screen.push_str("\r\n");
    }
    session.step(
        Timestamp::from_nanos(2),
        Input::PtyOutput {
            pane,
            bytes: screen.as_bytes(),
        },
        &mut outbox,
    );
    session.step(Timestamp::from_nanos(100_000_000), Input::Tick, &mut outbox);
    receive(&mut outbox, &mut decoder, &mut mirror);
    assert_ne!(mirror.seq_or_none(), Some(seq), "the filled screen came");
    (mirror, size)
}

/// Applies everything sent so far.
fn receive(outbox: &mut Outbox, decoder: &mut Decoder, mirror: &mut Mirror) {
    while let Some((payload, length)) = split(&outbox.0, MAX_SERVER_MESSAGE).expect("framed") {
        decoder.decode(payload, mirror).expect("valid");
        outbox.0.drain(..length);
    }
}

fn full_redraw(bencher: Bencher, line: &str, color: ColorDepth) {
    let (mut mirror, size) = mirror(line);
    let mut compositor = Compositor::new(Capabilities {
        color,
        underlines: UnderlineStyles::Styled,
        progress: ProgressBars::Unsupported,
    });
    let mut out = Vec::new();
    compositor.render(&mut mirror, size, &mut out);
    bencher
        .counter(divan::counter::BytesCount::new(out.len()))
        .bench_local(|| {
            mirror.damage_all();
            out.clear();
            compositor.render(&mut mirror, size, &mut out)
        });
}

/// Every cell of a 200×50 screen, styled, in true colour.
#[divan::bench]
fn full_redraw_styled(bencher: Bencher) {
    full_redraw(bencher, STYLED, ColorDepth::TrueColor);
}

/// The same, downgraded to 256 colours.
#[divan::bench]
fn full_redraw_styled_256(bencher: Bencher) {
    full_redraw(bencher, STYLED, ColorDepth::Palette256);
}

/// Every cell of a 200×50 screen, plain.
#[divan::bench]
fn full_redraw_plain(bencher: Bencher) {
    full_redraw(bencher, PLAIN, ColorDepth::TrueColor);
}
