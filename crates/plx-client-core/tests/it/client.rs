//! The client core against a real session: frames drawn and acknowledged,
//! input forwarded, the connection's end, and the daemon's mistakes.

use std::num::NonZeroUsize;

use plx_alloc_count::measure;
use plx_client_core::client::{
    ClientCore, ClientEffects, DaemonError, End, PasteTooLarge, Received,
};
use plx_client_core::compose::{Capabilities, ColorDepth, ProgressBars, UnderlineStyles};
use plx_core::command::{ClientCommand, Command, SessionCommand};
use plx_core::event::EndReason;
use plx_core::id::ClientId;
use plx_core::input::Input;
use plx_core::message::ClientMessage;
use plx_core::mode::Keymap;
use plx_core::rejection::Rejection;
use plx_core::session::Session;
use plx_core::size::SizePolicy;
use plx_core::time::Timestamp;
use plx_emulator::{Emulator, FakeFactory, Key, KeyChar, KeyEvent, Mods, Version};
use plx_ghostty::{GhosttyEmulator, GraphemeClustering, Options, Scrollback};
use plx_protocol::client::{ClientWire, Request, decode};
use plx_protocol::framing::{MAX_CLIENT_MESSAGE, split};
use proptest::prelude::*;

use crate::mirror::{Daemon, size};

const CAPABILITIES: Capabilities = Capabilities {
    color: ColorDepth::TrueColor,
    underlines: UnderlineStyles::Styled,
    progress: ProgressBars::Drawn,
};

/// The client shell's buffers.
#[derive(Default)]
struct Shell {
    daemon: Vec<u8>,
    terminal: Vec<u8>,
    rejections: Vec<Rejection>,
}

impl ClientEffects for Shell {
    fn daemon(&mut self) -> &mut Vec<u8> {
        &mut self.daemon
    }
    fn terminal(&mut self) -> &mut Vec<u8> {
        &mut self.terminal
    }
    fn rejected(&mut self, rejection: Rejection) {
        self.rejections.push(rejection);
    }
}

/// A session, its one client's id, and the clock.
struct World {
    daemon: Daemon,
    session: Session<FakeFactory>,
    id: ClientId,
    now: u64,
}

impl World {
    fn new() -> Self {
        let mut daemon = Daemon::default();
        let mut session = Session::new(
            FakeFactory,
            Keymap::unlock_first(),
            SizePolicy::Latest,
            &mut daemon,
        );
        let id = session.connect();
        Self {
            daemon,
            session,
            id,
            now: 0,
        }
    }

    fn step(&mut self, input: Input<'_>) {
        self.now += 20_000_000;
        self.session
            .step(Timestamp::from_nanos(self.now), input, &mut self.daemon);
    }

    /// Delivers everything the client sent to the session.
    fn deliver(&mut self, shell: &mut Shell) {
        let mut used = 0;
        while let Some((payload, length)) =
            split(&shell.daemon[used..], MAX_CLIENT_MESSAGE).unwrap()
        {
            let wire = decode(payload).unwrap();
            let id = self.id;
            let Request::Session(message) = wire.request() else {
                panic!("the client core never subscribes");
            };
            self.step(Input::Client {
                client: id,
                message,
            });
            used += length;
        }
        shell.daemon.clear();
    }

    /// What the session sent the client since the last call.
    fn take(&mut self) -> Vec<u8> {
        std::mem::take(self.daemon.outboxes.entry(self.id).or_default())
    }

    fn output(&mut self, bytes: &[u8]) {
        let pane = self.session.structure().panes()[0];
        self.step(Input::PtyOutput { pane, bytes });
        self.step(Input::Tick);
    }

    fn command(&mut self, command: &Command) {
        let id = self.id;
        self.step(Input::Client {
            client: id,
            message: ClientMessage::Command(command),
        });
    }
}

fn attached(received: Result<Received, DaemonError>) -> ClientCore {
    match received.unwrap() {
        Received::Attached(core) => core,
        Received::Ended(end) => panic!("ended: {end:?}"),
    }
}

/// The client's messages so far, decoded.
fn sent(shell: &Shell) -> Vec<String> {
    let mut used = 0;
    let mut out = Vec::new();
    while let Some((payload, length)) = split(&shell.daemon[used..], MAX_CLIENT_MESSAGE).unwrap() {
        out.push(format!("{:?}", decode(payload).unwrap()));
        used += length;
    }
    out
}

#[test]
fn frames_are_drawn_then_acknowledged() {
    let mut world = World::new();
    let mut shell = Shell::default();
    let mut core = ClientCore::attach(size(40, 10), None, CAPABILITIES, &mut shell);
    assert_eq!(
        sent(&shell),
        [format!(
            "{:?}",
            ClientWire::Attach {
                size: size(40, 10),
                cell_px: None
            }
        )]
    );
    world.deliver(&mut shell);
    let mut bytes = world.take();
    core = attached(core.receive(&mut bytes, &mut shell));
    assert!(!shell.terminal.is_empty(), "the first frame is drawn");
    let seq = core.mirror().seq_or_none().unwrap();
    assert_eq!(sent(&shell), [format!("{:?}", ClientWire::Ack(seq))]);
    world.deliver(&mut shell);
    // The ack let the next frame through.
    shell.terminal.clear();
    world.output(b"hello");
    let mut bytes = world.take();
    assert!(
        !bytes.is_empty(),
        "the acknowledged daemon sends the next frame"
    );
    core = attached(core.receive(&mut bytes, &mut shell));
    assert!(shell.terminal.windows(5).any(|w| w == b"hello"));
    assert!(core.mirror().seq_or_none().unwrap() > seq);
    assert_eq!(shell.rejections, []);
}

#[test]
fn keys_and_pastes_reach_the_daemon() {
    let mut shell = Shell::default();
    let mut core = ClientCore::attach(size(40, 10), None, CAPABILITIES, &mut shell);
    shell.daemon.clear();
    let key = KeyEvent::press(Key::Char(KeyChar::new('q').unwrap()), Mods::CTRL);
    core.key(key, &mut shell);
    core.paste("echo hi\n", &mut shell).unwrap();
    let (key_message, paste_message) = {
        let (payload, length) = split(&shell.daemon, MAX_CLIENT_MESSAGE).unwrap().unwrap();
        let key_message = format!("{:?}", decode(payload).unwrap());
        let (payload, _) = split(&shell.daemon[length..], MAX_CLIENT_MESSAGE)
            .unwrap()
            .unwrap();
        (key_message, format!("{:?}", decode(payload).unwrap()))
    };
    assert_eq!(key_message, format!("{:?}", ClientWire::Key(key)));
    assert!(paste_message.contains("echo hi\\n"), "{paste_message}");
}

#[test]
fn a_paste_too_large_for_a_message_sends_nothing() {
    let mut shell = Shell::default();
    let mut core = ClientCore::attach(size(40, 10), None, CAPABILITIES, &mut shell);
    let before = shell.daemon.len();
    let text = "x".repeat(MAX_CLIENT_MESSAGE);
    assert_eq!(
        core.paste(&text, &mut shell),
        Err(PasteTooLarge { len: text.len() })
    );
    assert_eq!(shell.daemon.len(), before);
}

#[test]
fn rejections_are_reported() {
    let mut world = World::new();
    let mut shell = Shell::default();
    let mut core = ClientCore::attach(size(40, 10), None, CAPABILITIES, &mut shell);
    world.deliver(&mut shell);
    let mut bytes = world.take();
    core = attached(core.receive(&mut bytes, &mut shell));
    world.deliver(&mut shell);
    world.command(&Command::Session(SessionCommand::FocusTab {
        tab: plx_core::tab::TabNumber::new(9.try_into().unwrap()),
    }));
    let mut bytes = world.take();
    let _core = attached(core.receive(&mut bytes, &mut shell));
    assert_eq!(shell.rejections.len(), 1, "{:?}", shell.rejections);
}

#[test]
fn the_daemon_ends_the_connection() {
    for (command, end) in [
        (Command::Client(ClientCommand::Detach), End::Detached),
        (
            Command::Session(SessionCommand::KillSession),
            End::SessionEnded(EndReason::Killed),
        ),
    ] {
        let mut world = World::new();
        let mut shell = Shell::default();
        let mut core = ClientCore::attach(size(40, 10), None, CAPABILITIES, &mut shell);
        world.deliver(&mut shell);
        let mut bytes = world.take();
        core = attached(core.receive(&mut bytes, &mut shell));
        world.deliver(&mut shell);
        world.command(&command);
        let mut bytes = world.take();
        match core.receive(&mut bytes, &mut shell).unwrap() {
            Received::Ended(got) => assert_eq!(got, end),
            Received::Attached(_) => panic!("still attached after {command:?}"),
        }
    }
}

/// An attached client and the session's bytes that detach it.
fn detaching() -> (ClientCore, Shell, Vec<u8>) {
    let mut world = World::new();
    let mut shell = Shell::default();
    let mut core = ClientCore::attach(size(40, 10), None, CAPABILITIES, &mut shell);
    world.deliver(&mut shell);
    let mut bytes = world.take();
    core = attached(core.receive(&mut bytes, &mut shell));
    world.deliver(&mut shell);
    world.command(&Command::Client(ClientCommand::Detach));
    let bytes = world.take();
    (core, shell, bytes)
}

#[test]
fn bytes_after_the_end_break_the_protocol() {
    // A whole message after the end, then part of one.
    let (_, _, end) = detaching();
    for extra in [end, vec![1_u8, 0, 0]] {
        let (core, mut shell, mut stream) = detaching();
        stream.extend_from_slice(&extra);
        assert!(matches!(
            core.receive(&mut stream, &mut shell),
            Err(DaemonError::AfterEnd(_))
        ));
    }
}

#[test]
fn garbage_from_the_daemon_breaks_the_protocol() {
    let mut shell = Shell::default();
    let core = ClientCore::attach(size(40, 10), None, CAPABILITIES, &mut shell);
    let mut garbage = vec![3_u8, 0, 0, 0, 0xFF, 0xFF, 0xFF];
    assert!(matches!(
        core.receive(&mut garbage, &mut shell),
        Err(DaemonError::Codec(_))
    ));
}

#[test]
fn resizing_and_resuming_redraw_everything() {
    let mut world = World::new();
    let mut shell = Shell::default();
    let mut core = ClientCore::attach(size(40, 10), None, CAPABILITIES, &mut shell);
    world.deliver(&mut shell);
    let mut bytes = world.take();
    core = attached(core.receive(&mut bytes, &mut shell));
    world.deliver(&mut shell);
    let clears = |terminal: &[u8]| terminal.windows(4).filter(|w| w == b"\x1b[2J").count();
    shell.terminal.clear();
    core.resized(size(50, 12), None, &mut shell);
    assert_eq!(clears(&shell.terminal), 1);
    assert_eq!(
        sent(&shell),
        [format!(
            "{:?}",
            ClientWire::Resize {
                size: size(50, 12),
                cell_px: None
            }
        )]
    );
    shell.terminal.clear();
    core.resumed(&mut shell);
    assert_eq!(clears(&shell.terminal), 1);
}

const OPTIONS: Options = Options {
    scrollback: Scrollback::Disabled,
    continuation_max_bytes: NonZeroUsize::new(4096).unwrap(),
    grapheme_clustering: GraphemeClustering::Enabled,
};

/// What a terminal shows after `bytes`.
fn screen(bytes: &[u8]) -> String {
    let mut terminal = GhosttyEmulator::new(size(40, 10), &OPTIONS).unwrap();
    terminal.feed(bytes);
    terminal.sync(Version::ZERO.next()).unwrap();
    let screen = terminal.screen();
    format!(
        "{:?}\n{:?}",
        screen
            .grid()
            .rows()
            .map(|row| row
                .iter()
                .map(|&cell| format!("{:?}", screen.resolve(cell)))
                .collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        screen.grid().cursor()
    )
}

proptest! {
    /// However the daemon's bytes are cut into reads, the terminal ends up
    /// showing the same.
    #[test]
    fn any_chunking_draws_the_same(
        outputs in proptest::collection::vec(proptest::collection::vec(prop_oneof![Just(b'a'), Just(b'\n'), Just(b'\r'), Just(b' ')], 0..40), 1..8),
        cuts in proptest::collection::vec(1_usize..64, 1..200),
    ) {
        // A session and a client fed whole messages; record the stream.
        let mut world = World::new();
        let mut shell = Shell::default();
        let mut core = ClientCore::attach(size(40, 10), None, CAPABILITIES, &mut shell);
        world.deliver(&mut shell);
        let mut inbox = world.take();
        let mut stream = inbox.clone();
        core = attached(core.receive(&mut inbox, &mut shell));
        world.deliver(&mut shell);
        for output in &outputs {
            world.output(output);
            let mut bytes = world.take();
            stream.extend_from_slice(&bytes);
            core = attached(core.receive(&mut bytes, &mut shell));
            world.deliver(&mut shell);
        }
        let whole = screen(&shell.terminal);
        // The same stream, cut anywhere.
        let mut chunked_shell = Shell::default();
        let mut chunked = ClientCore::attach(size(40, 10), None, CAPABILITIES, &mut chunked_shell);
        // Reads land in the shell's buffer; a partial message stays there.
        let mut inbox = Vec::new();
        let mut rest = &stream[..];
        for cut in cuts.iter().cycle() {
            if rest.is_empty() {
                break;
            }
            let (chunk, tail) = rest.split_at((*cut).min(rest.len()));
            inbox.extend_from_slice(chunk);
            chunked = attached(chunked.receive(&mut inbox, &mut chunked_shell));
            rest = tail;
        }
        prop_assert_eq!(inbox.len(), 0);
        prop_assert_eq!(screen(&chunked_shell.terminal), whole);
    }
}

#[test]
fn steady_state_receiving_allocates_nothing() {
    let mut world = World::new();
    let mut shell = Shell::default();
    let mut core = ClientCore::attach(size(80, 24), None, CAPABILITIES, &mut shell);
    world.deliver(&mut shell);
    let mut bytes = world.take();
    core = attached(core.receive(&mut bytes, &mut shell));
    world.deliver(&mut shell);
    // Different text each round, so every row changes.
    let screens: Vec<Vec<u8>> = (0..5)
        .map(|n| {
            format!("\x1b[Hround {n}: some output on every row\r\n")
                .repeat(23)
                .into_bytes()
        })
        .collect();
    let mut rounds = screens.iter();
    let mut round = |world: &mut World, shell: &mut Shell| {
        world.output(rounds.next().unwrap());
        shell.terminal.clear();
        world.take()
    };
    for _ in 0..3 {
        let mut bytes = round(&mut world, &mut shell);
        core = attached(core.receive(&mut bytes, &mut shell));
        world.deliver(&mut shell);
    }
    let mut bytes = round(&mut world, &mut shell);
    shell.daemon.reserve(64);
    let (counts, received) = measure(|| core.receive(&mut bytes, &mut shell));
    let _core = attached(received);
    assert_ne!(shell.terminal.len(), 0, "the frame is drawn");
    assert_eq!(counts.allocations, 0, "{counts:?}");
}

/// OSC 52 carries the text base64-encoded (RFC 4648's test vectors).
#[test]
fn clipboard_text_is_written_as_osc_52() {
    use plx_client_core::client::osc52;
    for (text, base64) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        let mut out = Vec::new();
        osc52(text.as_bytes(), &mut out);
        assert_eq!(
            out,
            format!("\x1b]52;c;{base64}\x1b\\").into_bytes(),
            "{text:?}"
        );
    }
}

/// Progress reports are written as OSC 9;4, from the typed report, and
/// removal is the shell's constant.
#[test]
fn progress_is_written_as_osc_9_4() {
    use plx_client_core::client::{PROGRESS_REMOVED, osc_progress};
    use plx_emulator::{Percent, Progress};
    let percent = |p| Some(Percent::new(p).unwrap());
    for (progress, osc) in [
        (Progress::Removed, "0"),
        (Progress::Set(percent(0)), "1;0"),
        (Progress::Set(percent(7)), "1;7"),
        (Progress::Set(percent(42)), "1;42"),
        (Progress::Set(None), "1"),
        (Progress::Error(percent(100)), "2;100"),
        (Progress::Indeterminate, "3"),
        (Progress::Paused(percent(64)), "4;64"),
    ] {
        let mut out = Vec::new();
        osc_progress(progress, &mut out);
        assert_eq!(
            out,
            format!("\x1b]9;4;{osc}\x1b\\").into_bytes(),
            "{progress:?}"
        );
    }
    let mut removed = Vec::new();
    osc_progress(Progress::Removed, &mut removed);
    assert_eq!(removed, PROGRESS_REMOVED);
}

/// A pane's progress report reaches the terminal once its frame is drawn;
/// a terminal that draws no progress bars is sent none. Resuming shows the
/// bar again (the shell removed it when it stopped).
#[test]
fn progress_reports_reach_a_terminal_that_draws_them() {
    for progress in [ProgressBars::Drawn, ProgressBars::Unsupported] {
        let capabilities = Capabilities {
            progress,
            ..CAPABILITIES
        };
        let mut world = World::new();
        let mut shell = Shell::default();
        let mut core = ClientCore::attach(size(40, 10), None, capabilities, &mut shell);
        world.deliver(&mut shell);
        let mut bytes = world.take();
        core = attached(core.receive(&mut bytes, &mut shell));
        world.deliver(&mut shell);
        shell.terminal.clear();
        world.output(b"\x1b]9;4;1;50\x07");
        let mut bytes = world.take();
        core = attached(core.receive(&mut bytes, &mut shell));
        let bar = |terminal: &[u8]| {
            terminal
                .windows(12)
                .filter(|w| w == b"\x1b]9;4;1;50\x1b\\")
                .count()
        };
        let shown = match progress {
            ProgressBars::Drawn => 1,
            ProgressBars::Unsupported => 0,
        };
        assert_eq!(bar(&shell.terminal), shown, "{progress:?}");
        shell.terminal.clear();
        core.resumed(&mut shell);
        assert_eq!(bar(&shell.terminal), shown, "{progress:?} resumed");
        // Nothing new: nothing written.
        world.deliver(&mut shell);
        shell.terminal.clear();
        world.output(b"x");
        let mut bytes = world.take();
        let _core = attached(core.receive(&mut bytes, &mut shell));
        assert!(
            !shell.terminal.windows(6).any(|w| w == b"\x1b]9;4;"),
            "{progress:?}"
        );
    }
}
