//! The compositor against an oracle: its output, fed to a headless
//! libghostty terminal, must leave that terminal showing exactly what the
//! mirror holds, after every frame.

use std::num::NonZeroUsize;

use plx_alloc_count::measure;
use plx_client_core::compose::{
    Capabilities, ColorDepth, Compositor, ProgressBars, UnderlineStyles,
};
use plx_client_core::mirror::{Mirror, MirrorPane};
use plx_core::command::{Command, SessionCommand};
use plx_core::geometry::{Axis, Direction};
use plx_core::id::ClientId;
use plx_core::input::Input;
use plx_core::message::ClientMessage;
use plx_core::mode::Keymap;
use plx_core::session::Session;
use plx_core::size::{SizePolicy, pane_area};
use plx_core::tab::TabName;
use plx_core::time::Timestamp;
use plx_emulator::{
    CellText, Change, CursorShape, CursorVisibility, Emulator, GlyphKind, GridSize, Position,
    Style, Version, Width,
};
use plx_ghostty::{GhosttyEmulator, GhosttyFactory, GraphemeClustering, Options, Scrollback};
use plx_protocol::framing::{MAX_SERVER_MESSAGE, split};
use plx_protocol::server::{Decoder, ServerMessage};
use proptest::prelude::*;

use crate::mirror::{Daemon, size};

const OPTIONS: Options = Options {
    scrollback: Scrollback::Disabled,
    continuation_max_bytes: NonZeroUsize::new(4096).unwrap(),
    grapheme_clustering: GraphemeClustering::Enabled,
};

const FULL: Capabilities = Capabilities {
    color: ColorDepth::TrueColor,
    underlines: UnderlineStyles::Styled,
    progress: ProgressBars::Unsupported,
};

/// A client drawing onto a headless terminal.
struct Client {
    id: ClientId,
    mirror: Mirror,
    decoder: Decoder,
    compositor: Compositor,
    terminal: GridSize,
    oracle: GhosttyEmulator,
    version: Version,
    out: Vec<u8>,
}

impl Client {
    fn new(id: ClientId, terminal: GridSize) -> Self {
        Self {
            id,
            mirror: Mirror::new(),
            decoder: Decoder::new(),
            compositor: Compositor::new(FULL),
            terminal,
            oracle: GhosttyEmulator::new(terminal, &OPTIONS).unwrap(),
            version: Version::ZERO,
            out: Vec::new(),
        }
    }

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

    /// The client's terminal resized.
    fn resize_terminal(&mut self, terminal: GridSize) {
        self.terminal = terminal;
        self.oracle.resize(terminal, None).unwrap();
    }

    /// Draws onto the oracle and checks it.
    fn draw(&mut self) -> Result<(), TestCaseError> {
        self.out.clear();
        self.compositor
            .render(&mut self.mirror, self.terminal, &mut self.out);
        self.oracle.feed(&self.out);
        self.version = self.version.next();
        self.oracle
            .sync(self.version)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        check(&self.mirror, self.terminal, &self.oracle)
    }
}

/// A cell as it looks; a blank is the same whether erased or a space.
#[derive(Debug, PartialEq, Eq)]
struct Look {
    text: Text,
    width: Width,
    style: Style,
}

#[derive(Debug, PartialEq, Eq)]
enum Text {
    Blank,
    Char(char),
    Cluster(Vec<char>),
}

const BLANK: Look = Look {
    text: Text::Blank,
    width: Width::Narrow,
    style: Style::DEFAULT,
};

/// The style a separator cell at (`x`, `y`) must have: the focus colour
/// where it borders the focused pane `focus`.
fn border_style(axis: Axis, x: u16, y: u16, focus: Option<plx_core::geometry::Rect>) -> Style {
    let borders = focus.is_some_and(|f| {
        let (right, bottom) = (f.x() + f.cols(), f.y() + f.rows());
        match axis {
            Axis::Horizontal => (x == right || x + 1 == f.x()) && f.y() <= y && y < bottom,
            Axis::Vertical => (y == bottom || y + 1 == f.y()) && f.x() <= x && x < right,
        }
    });
    if borders {
        plx_client_core::compose::BORDER_FOCUSED
    } else {
        plx_client_core::compose::BORDER
    }
}

/// What the terminal must show for `mirror`: `None` where the test does not
/// say (the status line, checked by its text).
fn expected(mirror: &Mirror, terminal: GridSize) -> Vec<Vec<Option<Look>>> {
    let header = mirror.header().unwrap();
    let cols = terminal.cols().get().min(header.size.cols().get());
    let rows = terminal.rows().get().min(header.size.rows().get());
    let mut screen: Vec<Vec<Option<Look>>> = (0..terminal.rows().get())
        .map(|_| (0..terminal.cols().get()).map(|_| Some(BLANK)).collect())
        .collect();
    let status = pane_area(header.size).rows();
    if status < header.size.rows().get() && status < rows {
        for cell in screen[usize::from(status)]
            .iter_mut()
            .take(usize::from(cols))
        {
            *cell = None;
        }
    }
    let focus = mirror.pane_or_none(header.focus).map(MirrorPane::rect);
    for separator in mirror.separators() {
        let rect = separator.rect;
        let glyph = match separator.axis {
            Axis::Horizontal => '│',
            Axis::Vertical => '─',
        };
        for y in rect.y()..(rect.y() + rect.rows()).min(rows) {
            for x in rect.x()..(rect.x() + rect.cols()).min(cols) {
                screen[usize::from(y)][usize::from(x)] = Some(Look {
                    text: Text::Char(glyph),
                    width: Width::Narrow,
                    style: border_style(separator.axis, x, y, focus),
                });
            }
        }
    }
    join_lines(&mut screen);
    for (_, pane) in mirror.panes() {
        let rect = pane.rect();
        let right = (rect.x() + rect.cols()).min(cols);
        for y in 0..rect.rows() {
            let row = rect.y() + y;
            if row >= rows {
                break;
            }
            for (x, cell) in (rect.x()..right).zip(pane.row_or_none(y).unwrap()) {
                let style = pane.style(cell.style);
                let text = match cell.glyph.kind() {
                    GlyphKind::Empty | GlyphKind::Char(' ') => Text::Blank,
                    GlyphKind::Char(c) => Text::Char(c),
                    GlyphKind::Cluster(id) => Text::Cluster(pane.grapheme(id).to_vec()),
                };
                let look = match cell.width {
                    // Cut by the clip, or a wrap placeholder: a blank.
                    Width::Wide if x + 1 >= right => Look {
                        text: Text::Blank,
                        width: Width::Narrow,
                        style,
                    },
                    Width::SpacerHead => Look {
                        text: Text::Blank,
                        width: Width::Narrow,
                        style,
                    },
                    Width::Narrow | Width::Wide | Width::SpacerTail => Look {
                        text,
                        width: cell.width,
                        style,
                    },
                };
                screen[usize::from(row)][usize::from(x)] = Some(look);
            }
        }
    }
    screen
}

/// Where separator lines meet, joins them, judging by the cells drawn: a
/// `│` connects up and down to any line, left and right only to a `─`; a
/// `─` the other way round.
fn join_lines(screen: &mut [Vec<Option<Look>>]) {
    let line_at = |screen: &[Vec<Option<Look>>], x: Option<usize>, y: Option<usize>| {
        let look = screen.get(y?)?.get(x?)?.as_ref()?;
        if let Text::Char(c @ ('│' | '─')) = look.text {
            Some(c)
        } else {
            None
        }
    };
    let mut joined = Vec::new();
    for y in 0..screen.len() {
        for x in 0..screen[y].len() {
            let Some(line) = line_at(screen, Some(x), Some(y)) else {
                continue;
            };
            let near = |dx: isize, dy: isize| {
                line_at(screen, x.checked_add_signed(dx), y.checked_add_signed(dy))
            };
            let column = line == '│';
            let across = if column { '─' } else { '│' };
            let any = |c: Option<char>| c.is_some();
            let crossing = |c: Option<char>| c == Some(across);
            let (up, down, left, right) = if column {
                (
                    any(near(0, -1)),
                    any(near(0, 1)),
                    crossing(near(-1, 0)),
                    crossing(near(1, 0)),
                )
            } else {
                (
                    crossing(near(0, -1)),
                    crossing(near(0, 1)),
                    any(near(-1, 0)),
                    any(near(1, 0)),
                )
            };
            let glyph = match (up, down, left, right) {
                (true, true, true, true) => '┼',
                (true, true, false, true) => '├',
                (true, true, true, false) => '┤',
                (false, true, true, true) => '┬',
                (true, false, true, true) => '┴',
                (false, true, false, true) => '┌',
                (false, true, true, false) => '┐',
                (true, false, false, true) => '└',
                (true, false, true, false) => '┘',
                _ => line,
            };
            joined.push((x, y, glyph));
        }
    }
    for (x, y, glyph) in joined {
        if let Some(look) = &mut screen[y][x] {
            look.text = Text::Char(glyph);
        }
    }
}

fn check(
    mirror: &Mirror,
    terminal: GridSize,
    oracle: &GhosttyEmulator,
) -> Result<(), TestCaseError> {
    let screen = oracle.screen();
    let header = mirror.header().unwrap();
    for (y, (expected, row)) in expected(mirror, terminal)
        .iter()
        .zip(screen.grid().rows())
        .enumerate()
    {
        for (x, (expected, &cell)) in expected.iter().zip(row).enumerate() {
            let shown = screen.resolve(cell);
            let look = Look {
                text: match shown.text {
                    CellText::Empty | CellText::Char(' ') => Text::Blank,
                    CellText::Char(c) => Text::Char(c),
                    CellText::Cluster(cluster) => Text::Cluster(cluster.to_vec()),
                },
                width: shown.width,
                style: *shown.style,
            };
            if let Some(expected) = expected {
                prop_assert_eq!(&look, expected, "at column {}, row {}", x, y);
            }
        }
    }
    // The status line starts with the mode.
    let status = pane_area(header.size).rows();
    if status < header.size.rows().get() && status < terminal.rows().get() {
        let text: String = screen
            .grid()
            .row_or_none(status)
            .unwrap()
            .iter()
            .map(|&cell| match screen.resolve(cell).text {
                CellText::Char(c) => c,
                CellText::Empty | CellText::Cluster(_) => ' ',
            })
            .collect();
        let mode = format!(" {:?} ", header.mode).to_uppercase();
        let shown = mode.chars().count().min(usize::from(
            terminal.cols().get().min(header.size.cols().get()),
        ));
        prop_assert_eq!(
            text.chars().take(shown).collect::<String>(),
            mode.chars().take(shown).collect::<String>()
        );
    }
    // The focused pane's cursor, where it is drawn.
    let cursor = screen.grid().cursor();
    let focus = mirror
        .pane_or_none(header.focus)
        .map(|pane| (pane.rect(), pane.cursor()));
    let placed = focus.and_then(|(rect, wanted)| {
        let position = wanted.position?;
        let x = rect.x() + position.x;
        let y = rect.y() + position.y;
        let inside = position.x < rect.cols()
            && position.y < rect.rows()
            && x < terminal.cols().get().min(header.size.cols().get())
            && y < terminal.rows().get().min(header.size.rows().get());
        (inside && wanted.visibility == CursorVisibility::Visible)
            .then_some((Position { x, y }, wanted))
    });
    if let Some((position, wanted)) = placed {
        prop_assert_eq!(cursor.visibility, CursorVisibility::Visible);
        prop_assert_eq!(cursor.position, Some(position));
        prop_assert_eq!(cursor.shape, wanted.shape);
        // The terminal's own cursor blinks as the terminal says.
        if wanted.shape != CursorShape::Default {
            prop_assert_eq!(cursor.blink, wanted.blink);
        }
    } else {
        prop_assert_eq!(cursor.visibility, CursorVisibility::Hidden);
    }
    Ok(())
}

/// Program output: text of every width, styles of every kind, cursor
/// movement and erasure.
const PIECES: &[&str] = &[
    "a",
    "Z",
    "  ",
    "\r\n",
    "\n",
    "漢",
    "e\u{301}",
    "👍🏽",
    "👨\u{200d}👩\u{200d}👧",
    "\x1b[31m",
    "\x1b[95m",
    "\x1b[103m",
    "\x1b[1;4m",
    "\x1b[2m",
    "\x1b[3;9m",
    "\x1b[7m",
    "\x1b[8m",
    "\x1b[53m",
    "\x1b[5m",
    "\x1b[38;2;10;200;30m",
    "\x1b[48;5;200m",
    "\x1b[38;5;3m",
    "\x1b[4:3m",
    "\x1b[4:2m",
    "\x1b[21m",
    "\x1b[58;2;1;2;3m",
    "\x1b[58;5;99m",
    "\x1b[0m",
    "\x1b[2J",
    "\x1b[H",
    "\x1b[5;3H",
    "\x1b[K",
    "\x1b[44m\x1b[K",
    "\x1b[?25l",
    "\x1b[?25h",
    "\x1b[5 q",
    "\x1b[4 q",
    "\x1b[0 q",
    "\x1b[3D",
    "\x1b[2A",
];

const NAMES: &[&str] = &[
    "dev",
    "漢字",
    "a-rather-long-tab-name-for-a-narrow-status-line",
];

#[derive(Clone, Debug)]
enum Op {
    Output(usize, Vec<usize>),
    Split(bool),
    NewTab,
    NextTab,
    ClosePane,
    Rename(usize),
    /// The client's terminal and the session resize.
    Resize(u16, u16),
    /// Only the client's terminal resizes (the daemon has not heard yet).
    Terminal(u16, u16),
    Invalidate,
    Ack,
    Tick,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (any::<usize>(), proptest::collection::vec(0..PIECES.len(), 0..40))
            .prop_map(|(i, pieces)| Op::Output(i, pieces)),
        1 => any::<bool>().prop_map(Op::Split),
        1 => Just(Op::NewTab),
        1 => Just(Op::NextTab),
        1 => Just(Op::ClosePane),
        1 => (0..NAMES.len()).prop_map(Op::Rename),
        1 => (5_u16..90, 2_u16..30).prop_map(|(c, r)| Op::Resize(c, r)),
        // At least two columns: in a one-column terminal libghostty drops a
        // cluster's later codepoints (it attaches them left of the cursor,
        // where there is no cell), so it cannot serve as the oracle there.
        1 => (2_u16..100, 1_u16..35).prop_map(|(c, r)| Op::Terminal(c, r)),
        1 => Just(Op::Invalidate),
        4 => Just(Op::Ack),
        2 => Just(Op::Tick),
    ]
}

proptest! {
    /// After every frame, the terminal the compositor drew on shows the
    /// mirror: every visible pane cell with its text, width and style, the
    /// separators, the mode, blanks elsewhere, and the focused cursor.
    #[test]
    fn the_terminal_shows_the_mirror(ops in proptest::collection::vec(op(), 0..50)) {
        let mut daemon = Daemon::default();
        let mut session = Session::new(
            GhosttyFactory { options: OPTIONS },
            Keymap::unlock_first(),
            SizePolicy::Latest,
            &mut daemon,
        );
        let mut now = 0_u64;
        let mut step = |session: &mut Session<GhosttyFactory>, daemon: &mut Daemon, input: Input<'_>| {
            now += 20_000_000;
            session.step(Timestamp::from_nanos(now), input, daemon);
        };
        let mut a = Client::new(session.connect(), size(60, 20));
        step(&mut session, &mut daemon, Input::Client {
            client: a.id,
            message: ClientMessage::Attach { size: a.terminal, cell_px: None },
        });
        let mut unacked = a.receive(&mut daemon);
        a.draw()?;
        for op in ops {
            let panes = session.structure().panes();
            let mut run = |session: &mut Session<GhosttyFactory>, daemon: &mut Daemon, command: SessionCommand| {
                let command = Command::Session(command);
                step(session, daemon, Input::Client { client: a.id, message: ClientMessage::Command(&command) });
            };
            match op {
                Op::Output(i, pieces) => {
                    let bytes: String = pieces.iter().map(|&p| PIECES[p]).collect();
                    let pane = panes[i % panes.len()];
                    step(&mut session, &mut daemon, Input::PtyOutput { pane, bytes: bytes.as_bytes() });
                }
                Op::Split(right) => {
                    let direction = if right { Direction::Right } else { Direction::Down };
                    run(&mut session, &mut daemon, SessionCommand::Split { direction });
                }
                Op::NewTab => run(&mut session, &mut daemon, SessionCommand::NewTab),
                Op::NextTab => run(&mut session, &mut daemon, SessionCommand::NextTab),
                Op::ClosePane => {
                    // Never the last pane: that ends the session.
                    if panes.len() > 1 {
                        run(&mut session, &mut daemon, SessionCommand::ClosePane);
                    }
                }
                Op::Rename(n) => {
                    let name = TabName::new(NAMES[n]).unwrap();
                    run(&mut session, &mut daemon, SessionCommand::RenameTab { name });
                }
                Op::Resize(cols, rows) => {
                    a.resize_terminal(size(cols, rows));
                    step(&mut session, &mut daemon, Input::Client {
                        client: a.id,
                        message: ClientMessage::Resize { size: a.terminal, cell_px: None },
                    });
                }
                Op::Terminal(cols, rows) => a.resize_terminal(size(cols, rows)),
                Op::Invalidate => a.compositor.invalidate(),
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
            a.draw()?;
        }
    }
}

#[test]
fn a_drawn_frame_with_nothing_new_writes_nothing() {
    let mut daemon = Daemon::default();
    let mut session = Session::new(
        GhosttyFactory { options: OPTIONS },
        Keymap::unlock_first(),
        SizePolicy::Latest,
        &mut daemon,
    );
    let mut a = Client::new(session.connect(), size(40, 10));
    session.step(
        Timestamp::from_nanos(0),
        Input::Client {
            client: a.id,
            message: ClientMessage::Attach {
                size: a.terminal,
                cell_px: None,
            },
        },
        &mut daemon,
    );
    assert!(a.receive(&mut daemon));
    a.draw().unwrap();
    a.out.clear();
    let drawn = a.compositor.render(&mut a.mirror, a.terminal, &mut a.out);
    assert_eq!(drawn, Change::Unchanged);
    assert_eq!(a.out.len(), 0);
}

#[test]
fn steady_state_drawing_allocates_nothing() {
    let mut daemon = Daemon::default();
    let mut session = Session::new(
        GhosttyFactory { options: OPTIONS },
        Keymap::unlock_first(),
        SizePolicy::Latest,
        &mut daemon,
    );
    let mut a = Client::new(session.connect(), size(80, 24));
    let mut now = 0;
    let mut step =
        |session: &mut Session<GhosttyFactory>, daemon: &mut Daemon, input: Input<'_>| {
            now += 20_000_000;
            session.step(Timestamp::from_nanos(now), input, daemon);
        };
    step(
        &mut session,
        &mut daemon,
        Input::Client {
            client: a.id,
            message: ClientMessage::Attach {
                size: a.terminal,
                cell_px: None,
            },
        },
    );
    let pane = session.structure().panes()[0];
    let output = "\x1b[H\x1b[31mred \x1b[1;4:3mbold curly\x1b[0m 漢字 👍🏽 plain text\r\n".repeat(23);
    let mut round = |session: &mut Session<GhosttyFactory>, daemon: &mut Daemon, a: &mut Client| {
        step(
            session,
            daemon,
            Input::PtyOutput {
                pane,
                bytes: output.as_bytes(),
            },
        );
        step(session, daemon, Input::Tick);
        assert!(a.receive(daemon));
        let seq = a.mirror.seq_or_none().unwrap();
        step(
            session,
            daemon,
            Input::Client {
                client: a.id,
                message: ClientMessage::Ack(seq),
            },
        );
        // Every row redrawn, as after a full-screen update.
        a.mirror.damage_all();
        a.out.clear();
    };
    // Warm up: buffers grow to the frame's size.
    for _ in 0..3 {
        round(&mut session, &mut daemon, &mut a);
        a.compositor.render(&mut a.mirror, a.terminal, &mut a.out);
    }
    round(&mut session, &mut daemon, &mut a);
    let (counts, drawn) = measure(|| a.compositor.render(&mut a.mirror, a.terminal, &mut a.out));
    assert_eq!(drawn, Change::Changed);
    assert_eq!(counts.allocations, 0, "{counts:?}");
}

mod downgrade {
    use plx_client_core::compose::{ColorDepth, downgrade};
    use plx_emulator::{Color, Rgb};
    use proptest::prelude::*;

    const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color::Rgb(Rgb { r, g, b })
    }

    /// xterm's colour of a palette index from 16 up, by its formula.
    fn xterm(index: u8) -> Rgb {
        let level = |step: u8| if step == 0 { 0 } else { 55 + 40 * step };
        if index >= 232 {
            let grey = 8 + 10 * (index - 232);
            Rgb {
                r: grey,
                g: grey,
                b: grey,
            }
        } else {
            let cube = index - 16;
            Rgb {
                r: level(cube / 36),
                g: level(cube / 6 % 6),
                b: level(cube % 6),
            }
        }
    }

    #[test]
    fn known_colours() {
        let cases = [
            (rgb(255, 0, 0), ColorDepth::Palette256, Color::Palette(196)),
            (rgb(0, 0, 0), ColorDepth::Palette256, Color::Palette(16)),
            (
                rgb(255, 255, 255),
                ColorDepth::Palette256,
                Color::Palette(231),
            ),
            (
                rgb(128, 128, 128),
                ColorDepth::Palette256,
                Color::Palette(244),
            ),
            (rgb(100, 0, 0), ColorDepth::Palette256, Color::Palette(52)),
            (rgb(205, 0, 0), ColorDepth::Ansi16, Color::Palette(1)),
            (rgb(250, 250, 250), ColorDepth::Ansi16, Color::Palette(15)),
            (rgb(10, 10, 10), ColorDepth::Ansi16, Color::Palette(0)),
            (Color::Palette(196), ColorDepth::Ansi16, Color::Palette(9)),
            (Color::Palette(21), ColorDepth::Ansi16, Color::Palette(4)),
            (Color::Palette(5), ColorDepth::Ansi16, Color::Palette(5)),
            (
                Color::Palette(200),
                ColorDepth::Palette256,
                Color::Palette(200),
            ),
            (rgb(1, 2, 3), ColorDepth::TrueColor, rgb(1, 2, 3)),
        ];
        for (color, depth, want) in cases {
            assert_eq!(downgrade(color, depth), want, "{color:?} at {depth:?}");
        }
        for depth in [
            ColorDepth::TrueColor,
            ColorDepth::Palette256,
            ColorDepth::Ansi16,
        ] {
            assert_eq!(downgrade(Color::Default, depth), Color::Default);
        }
    }

    proptest! {
        /// Every colour of the cube and ramp maps back to its own index.
        #[test]
        fn palette_colours_are_their_own_nearest(index in 16_u8..=255) {
            let Rgb { r, g, b } = xterm(index);
            prop_assert_eq!(downgrade(rgb(r, g, b), ColorDepth::Palette256), Color::Palette(index));
        }

        /// What each depth leaves is a colour it shows, and no farther from
        /// the original than any other it shows.
        #[test]
        fn downgrades_stay_in_depth(r: u8, g: u8, b: u8, index: u8) {
            let distance = |a: Rgb, c: Rgb| {
                let d = |x: u8, y: u8| u32::from(x.abs_diff(y)).pow(2);
                d(a.r, c.r) + d(a.g, c.g) + d(a.b, c.b)
            };
            let original = Rgb { r, g, b };
            match downgrade(rgb(r, g, b), ColorDepth::Palette256) {
                Color::Palette(n) => {
                    prop_assert!(n >= 16);
                    let best = (16..=255).map(|m| distance(xterm(m), original)).min().unwrap();
                    prop_assert_eq!(distance(xterm(n), original), best);
                }
                other @ (Color::Default | Color::Rgb(_)) => prop_assert!(false, "{other:?}"),
            }
            for color in [rgb(r, g, b), Color::Palette(index)] {
                match downgrade(color, ColorDepth::Ansi16) {
                    Color::Palette(n) => prop_assert!(n < 16),
                    other @ (Color::Default | Color::Rgb(_)) => prop_assert!(false, "{other:?}"),
                }
            }
        }
    }
}

/// A pane can hold side by side what a terminal that clusters graphemes
/// would join if written left to right (a skin tone written first, a family
/// emoji to its left after): they are drawn so they stay apart.
#[test]
fn cells_a_terminal_would_join_are_drawn_apart() {
    let mut daemon = Daemon::default();
    let mut session = Session::new(
        GhosttyFactory { options: OPTIONS },
        Keymap::unlock_first(),
        SizePolicy::Latest,
        &mut daemon,
    );
    let mut now = 0_u64;
    let mut step =
        |session: &mut Session<GhosttyFactory>, daemon: &mut Daemon, input: Input<'_>| {
            now += 20_000_000;
            session.step(Timestamp::from_nanos(now), input, daemon);
        };
    let mut a = Client::new(session.connect(), size(20, 5));
    let attach = ClientMessage::Attach {
        size: a.terminal,
        cell_px: None,
    };
    step(
        &mut session,
        &mut daemon,
        Input::Client {
            client: a.id,
            message: attach,
        },
    );
    a.receive(&mut daemon);
    a.draw().unwrap();
    let seq = a.mirror.seq_or_none().unwrap();
    step(
        &mut session,
        &mut daemon,
        Input::Client {
            client: a.id,
            message: ClientMessage::Ack(seq),
        },
    );
    let pane = session.structure().panes()[0];
    let bytes = "\x1b[1;3H\u{1F3FD}\x1b[1;1H\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
    step(
        &mut session,
        &mut daemon,
        Input::PtyOutput {
            pane,
            bytes: bytes.as_bytes(),
        },
    );
    assert!(a.receive(&mut daemon));
    // The pane keeps them apart: the family, then the skin tone alone.
    let screen = session.pane_screen_or_none(pane).unwrap();
    let row: Vec<_> = screen
        .grid()
        .rows()
        .next()
        .unwrap()
        .iter()
        .map(|&cell| screen.resolve(cell).text)
        .collect();
    assert!(matches!(row[2], CellText::Char('\u{1F3FD}')), "{row:?}");
    a.draw().unwrap();
}
