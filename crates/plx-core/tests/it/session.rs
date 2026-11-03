use core::num::NonZeroU32;
use core::time::Duration;

use plx_alloc_count::measure;
use plx_core::command::{ClientCommand, Command, SessionCommand};
use plx_core::effects::{Effects, PaneProgram, StartIn};
use plx_core::event::{EndReason, Event, EventSeq, ExitStatus, Pid};
use plx_core::id::{ClientId, PaneId};
use plx_core::input::Input;
use plx_core::message::{ClientMessage, FrameSeq, Outgoing};
use plx_core::mode::{Keymap, Mode};
use plx_core::rejection::Rejection;
use plx_core::session::{ECHO_WINDOW, FRAME_INTERVAL, Session};
use plx_core::size::SizePolicy;
use plx_core::time::Timestamp;
use plx_emulator::{
    Case, Emulator, FakeFactory, GlyphKind, GridSize, Key, KeyChar, KeyEvent, Mods, Percent,
    Progress, SearchOptions, Words,
};

/// What the session asked for, owned, for assertions.
#[derive(Debug, PartialEq, Eq)]
enum Effect {
    Spawn(PaneId, GridSize),
    /// A pane started on the editor, with the history it edits.
    Editor(PaneId, String),
    Write(PaneId, Vec<u8>),
    Resize(PaneId, GridSize),
    Close(PaneId),
    Frame {
        client: ClientId,
        seq: FrameSeq,
        mode: Mode,
        /// Each visible pane with its sent rows, as text.
        panes: Vec<(PaneId, Vec<(u16, String)>)>,
    },
    Rejected(ClientId, Rejection),
    Detached(ClientId),
    Ended(ClientId, EndReason),
    Applied(ClientId),
    Clipboard(ClientId, String),
    CloseClient(ClientId),
    Event(String),
    Exit,
}

#[derive(Default)]
struct Recorder {
    effects: Vec<Effect>,
    /// Each frame's search: needle and options.
    searches: Vec<Option<(String, SearchOptions)>>,
    /// Where each shell was asked to start.
    starts: Vec<(PaneId, StartIn)>,
    /// Each frame's progress report, by client.
    progress: Vec<(ClientId, Option<Progress>)>,
}

impl Recorder {
    fn take(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }
}

impl Effects for Recorder {
    fn spawn_pty(&mut self, pane: PaneId, size: GridSize, program: PaneProgram<'_>) {
        self.effects.push(match program {
            PaneProgram::Shell { start_in } => {
                self.starts.push((pane, start_in));
                Effect::Spawn(pane, size)
            }
            PaneProgram::Editor(history) => {
                Effect::Editor(pane, String::from_utf8(history.bytes().to_vec()).unwrap())
            }
        });
    }

    fn write_pty(&mut self, pane: PaneId, bytes: &[u8]) {
        self.effects.push(Effect::Write(pane, bytes.to_vec()));
    }

    fn resize_pty(&mut self, pane: PaneId, size: GridSize) {
        self.effects.push(Effect::Resize(pane, size));
    }

    fn close_pty(&mut self, pane: PaneId) {
        self.effects.push(Effect::Close(pane));
    }

    fn send<E: Emulator>(&mut self, client: ClientId, message: Outgoing<'_, E>) {
        if let Outgoing::Frame(view) = &message {
            self.searches.push(
                view.search()
                    .map(|search| (search.needle.as_str().to_owned(), search.status.options)),
            );
            self.progress.push((client, view.progress_or_none()));
        }
        self.effects.push(match message {
            Outgoing::Frame(view) => Effect::Frame {
                client,
                seq: view.seq(),
                mode: view.mode(),
                panes: view
                    .panes()
                    .map(|update| {
                        let rows = update
                            .rows()
                            .map(|(y, cells)| {
                                let text: String = cells
                                    .iter()
                                    .map(|cell| match cell.glyph.kind() {
                                        GlyphKind::Char(c) => c,
                                        GlyphKind::Empty | GlyphKind::Cluster(_) => ' ',
                                    })
                                    .collect();
                                (y, text.trim_end().to_owned())
                            })
                            .collect();
                        (update.pane, rows)
                    })
                    .collect(),
            },
            Outgoing::Rejected(rejection) => Effect::Rejected(client, rejection),
            Outgoing::Detached => Effect::Detached(client),
            Outgoing::SessionEnded(reason) => Effect::Ended(client, reason),
            Outgoing::Applied => Effect::Applied(client),
            Outgoing::Clipboard(copied) => {
                Effect::Clipboard(client, String::from_utf8(copied.bytes().to_vec()).unwrap())
            }
        });
    }

    fn close_client(&mut self, client: ClientId) {
        self.effects.push(Effect::CloseClient(client));
    }

    fn event(&mut self, _seq: EventSeq, event: Event<'_>) {
        self.effects.push(Effect::Event(format!("{event:?}")));
    }

    fn exit(&mut self) {
        self.effects.push(Effect::Exit);
    }
}

fn size(cols: u16, rows: u16) -> GridSize {
    GridSize::new(cols.try_into().unwrap(), rows.try_into().unwrap()).unwrap()
}

fn at(ms: u64) -> Timestamp {
    Timestamp::from_nanos(ms * 1_000_000)
}

fn key(c: char, mods: Mods) -> KeyEvent {
    KeyEvent::press(Key::Char(KeyChar::new(c).unwrap()), mods)
}

/// A session with one attached 80x24 client, its first frame acknowledged.
struct Fixture {
    session: Session<FakeFactory>,
    fx: Recorder,
    client: ClientId,
    now: u64,
    next_ack: FrameSeq,
}

impl Fixture {
    fn new() -> Self {
        let mut fx = Recorder::default();
        let session = Session::new(
            FakeFactory,
            Keymap::unlock_first(),
            SizePolicy::Latest,
            &mut fx,
        );
        let mut fixture = Self {
            session,
            fx,
            client: ClientId::new(0),
            now: 0,
            next_ack: FrameSeq::FIRST,
        };
        fixture.client = fixture.session.connect();
        fixture.message(ClientMessage::Attach {
            size: size(80, 24),
            cell_px: None,
        });
        fixture.ack();
        fixture.fx.take();
        fixture
    }

    fn step(&mut self, input: Input<'_>) {
        // Every step happens a frame interval after the last, so pacing never
        // holds a frame back unless a test says so.
        self.now += 20;
        self.session.step(at(self.now), input, &mut self.fx);
    }

    fn message(&mut self, message: ClientMessage<'_>) {
        let client = self.client;
        self.step(Input::Client { client, message });
    }

    fn ack(&mut self) {
        let seq = self.next_ack;
        self.next_ack = seq.next();
        self.message(ClientMessage::Ack(seq));
    }

    fn frames(effects: &[Effect]) -> Vec<&Effect> {
        effects
            .iter()
            .filter(|e| matches!(e, Effect::Frame { .. }))
            .collect()
    }
}

#[test]
fn a_new_session_starts_its_first_pane_in_the_pane_area() {
    let mut fx = Recorder::default();
    let _session = Session::new(
        FakeFactory,
        Keymap::unlock_first(),
        SizePolicy::Latest,
        &mut fx,
    );
    // 80x24 less the status row.
    assert_eq!(fx.take(), [Effect::Spawn(PaneId::new(0), size(80, 23))]);
}

#[test]
fn attaching_resizes_the_session_and_sends_a_first_frame() {
    let mut fx = Recorder::default();
    let mut session = Session::new(
        FakeFactory,
        Keymap::unlock_first(),
        SizePolicy::Latest,
        &mut fx,
    );
    fx.take();
    let client = session.connect();
    session.step(
        at(0),
        Input::Client {
            client,
            message: ClientMessage::Attach {
                size: size(100, 30),
                cell_px: None,
            },
        },
        &mut fx,
    );
    let effects = fx.take();
    assert!(effects.contains(&Effect::Resize(PaneId::new(0), size(100, 29))));
    assert_eq!(session.size(), size(100, 30));
    assert!(matches!(
        Fixture::frames(&effects)[..],
        [Effect::Frame { seq, .. }] if *seq == FrameSeq::FIRST
    ));
}

#[test]
fn output_reaches_the_client_one_acknowledged_frame_at_a_time() {
    let mut f = Fixture::new();
    let pane = PaneId::new(0);
    f.step(Input::PtyOutput {
        pane,
        bytes: b"hello",
    });
    let frames = f.fx.take();
    assert_eq!(
        Fixture::frames(&frames),
        [&Effect::Frame {
            client: f.client,
            seq: FrameSeq::FIRST.next(),
            mode: Mode::Locked,
            panes: vec![(pane, vec![(0, String::from("hello"))])],
        }]
    );
    // More output while that frame is unacknowledged: nothing is sent...
    f.step(Input::PtyOutput {
        pane,
        bytes: b" world",
    });
    f.step(Input::Tick);
    assert_eq!(Fixture::frames(&f.fx.take()), Vec::<&Effect>::new());
    // ...until the ack, and then only what changed since.
    f.ack();
    assert_eq!(
        Fixture::frames(&f.fx.take()),
        [&Effect::Frame {
            client: f.client,
            seq: FrameSeq::FIRST.next().next(),
            mode: Mode::Locked,
            panes: vec![(pane, vec![(0, String::from("hello world"))])],
        }]
    );
}

#[test]
fn frames_wait_for_the_frame_interval() {
    let mut f = Fixture::new();
    let pane = PaneId::new(0);
    let client = f.client;
    // A frame, its ack and more output, all at the same instant: the second
    // frame waits for the interval.
    f.now += 20;
    let now = at(f.now);
    f.session
        .step(now, Input::PtyOutput { pane, bytes: b"a" }, &mut f.fx);
    assert_eq!(Fixture::frames(&f.fx.take()).len(), 1);
    let ack = ClientMessage::Ack(f.next_ack);
    f.session.step(
        now,
        Input::Client {
            client,
            message: ack,
        },
        &mut f.fx,
    );
    f.session
        .step(now, Input::PtyOutput { pane, bytes: b"b" }, &mut f.fx);
    assert_eq!(Fixture::frames(&f.fx.take()), Vec::<&Effect>::new());
    let due = f.session.poll_timeout().unwrap();
    assert_eq!(due, now.saturating_add(FRAME_INTERVAL));
    f.session.step(due, Input::Tick, &mut f.fx);
    assert_eq!(Fixture::frames(&f.fx.take()).len(), 1);
}

#[test]
fn echo_does_not_wait_for_the_frame_interval() {
    let mut f = Fixture::new();
    let pane = PaneId::new(0);
    let client = f.client;
    f.now += 20;
    let now = at(f.now);
    f.session
        .step(now, Input::PtyOutput { pane, bytes: b"$ " }, &mut f.fx);
    assert_eq!(Fixture::frames(&f.fx.take()).len(), 1);
    // A key typed while that frame is in flight: its echo is sent as soon
    // as the frame is acknowledged, not a frame interval later.
    let typed = ClientMessage::Key(key('x', Mods::empty()));
    f.session.step(
        now,
        Input::Client {
            client,
            message: typed,
        },
        &mut f.fx,
    );
    f.session
        .step(now, Input::PtyOutput { pane, bytes: b"x" }, &mut f.fx);
    // A frame is in flight: no frame waits, only the history's compression.
    assert_eq!(
        f.session.poll_timeout(),
        Some(now.saturating_add(plx_core::session::COMPRESS_AFTER))
    );
    let ack = ClientMessage::Ack(f.next_ack);
    f.session.step(
        now,
        Input::Client {
            client,
            message: ack,
        },
        &mut f.fx,
    );
    assert_eq!(Fixture::frames(&f.fx.take()).len(), 1);
    f.next_ack = f.next_ack.next();
    // Only the first output after the key counts: more waits as usual.
    let ack = ClientMessage::Ack(f.next_ack);
    f.session.step(
        now,
        Input::Client {
            client,
            message: ack,
        },
        &mut f.fx,
    );
    f.session
        .step(now, Input::PtyOutput { pane, bytes: b"y" }, &mut f.fx);
    assert_eq!(Fixture::frames(&f.fx.take()), Vec::<&Effect>::new());
    assert_eq!(
        f.session.poll_timeout(),
        Some(now.saturating_add(FRAME_INTERVAL))
    );
}

#[test]
fn an_echo_frame_carries_only_the_echo() {
    let mut f = Fixture::new();
    let client = f.client;
    let split = Command::Session(SessionCommand::Split {
        direction: plx_core::geometry::Direction::Right,
    });
    f.message(ClientMessage::Command(&split));
    f.ack();
    let (flooding, typing) = (PaneId::new(0), PaneId::new(1));
    f.step(Input::PtyOutput {
        pane: flooding,
        bytes: b"flood 1",
    });
    f.fx.take();
    // At the instant of that frame (within one frame interval): its ack,
    // more flood, a key in the other pane, and its echo.
    let now = at(f.now);
    let step = |f: &mut Fixture, input| f.session.step(now, input, &mut f.fx);
    let seq = f.next_ack;
    step(
        &mut f,
        Input::Client {
            client,
            message: ClientMessage::Ack(seq),
        },
    );
    f.next_ack = f.next_ack.next();
    step(
        &mut f,
        Input::PtyOutput {
            pane: flooding,
            bytes: b" 2",
        },
    );
    step(
        &mut f,
        Input::Client {
            client,
            message: ClientMessage::Key(key('x', Mods::empty())),
        },
    );
    step(
        &mut f,
        Input::PtyOutput {
            pane: typing,
            bytes: b"x",
        },
    );
    let effects = f.fx.take();
    let [Effect::Frame { panes, .. }] = Fixture::frames(&effects)[..] else {
        panic!("one echo frame: {effects:?}");
    };
    assert_eq!(
        panes,
        &[(flooding, vec![]), (typing, vec![(0, String::from("x"))])]
    );
    // The flood follows in the next paced frame.
    f.ack();
    let effects = f.fx.take();
    let [Effect::Frame { panes, .. }] = Fixture::frames(&effects)[..] else {
        panic!("the paced frame: {effects:?}");
    };
    assert_eq!(
        panes,
        &[
            (flooding, vec![(0, String::from("flood 1 2"))]),
            (typing, vec![])
        ]
    );
}

#[test]
fn typing_does_not_starve_the_other_panes() {
    let mut f = Fixture::new();
    let client = f.client;
    let split = Command::Session(SessionCommand::Split {
        direction: plx_core::geometry::Direction::Right,
    });
    f.message(ClientMessage::Command(&split));
    f.ack();
    let (flooding, typing) = (PaneId::new(0), PaneId::new(1));
    let start = f.now;
    let mut inflight = None;
    let mut flood_drawn = 0;
    // A key and its echo every 2 ms while the other pane floods: echo
    // frames go out at once, and the flood still gets its paced frames.
    for ms in (0..=40).step_by(2) {
        let now = at(start + ms);
        if let Some(seq) = inflight.take() {
            let message = ClientMessage::Ack(seq);
            f.session
                .step(now, Input::Client { client, message }, &mut f.fx);
        }
        let message = ClientMessage::Key(key('x', Mods::empty()));
        f.session
            .step(now, Input::Client { client, message }, &mut f.fx);
        for (pane, bytes) in [(typing, &b"x"[..]), (flooding, &b"flood"[..])] {
            f.session
                .step(now, Input::PtyOutput { pane, bytes }, &mut f.fx);
        }
        for effect in f.fx.take() {
            if let Effect::Frame { seq, panes, .. } = effect {
                inflight = Some(seq);
                flood_drawn += usize::from(
                    panes
                        .iter()
                        .any(|(pane, rows)| *pane == flooding && !rows.is_empty()),
                );
            }
        }
    }
    // 40 ms: at least two paced frames.
    assert!(flood_drawn >= 2, "the flood was drawn {flood_drawn} times");
}

#[test]
fn output_long_after_a_key_is_not_its_echo() {
    let mut f = Fixture::new();
    let pane = PaneId::new(0);
    let client = f.client;
    let step = |f: &mut Fixture, now: Timestamp, message: ClientMessage<'_>| {
        f.session
            .step(now, Input::Client { client, message }, &mut f.fx);
    };
    let typed_at = at(f.now + 20);
    step(
        &mut f,
        typed_at,
        ClientMessage::Key(key('x', Mods::empty())),
    );
    // Just before the echo window closes, a mode change (Ctrl-g, no
    // output) is drawn and acknowledged...
    let echo_ends = typed_at.saturating_add(ECHO_WINDOW);
    let drawn = typed_at.saturating_add(ECHO_WINDOW.checked_sub(Duration::from_millis(4)).unwrap());
    step(&mut f, drawn, ClientMessage::Key(key('g', Mods::CTRL)));
    assert_eq!(Fixture::frames(&f.fx.take()).len(), 1);
    let ack = ClientMessage::Ack(f.next_ack);
    step(&mut f, drawn, ack);
    // ...then the pane's output comes after the window: not echo, so it
    // waits for the interval.
    let late = echo_ends.saturating_add(Duration::from_millis(1));
    f.session
        .step(late, Input::PtyOutput { pane, bytes: b"x" }, &mut f.fx);
    assert_eq!(Fixture::frames(&f.fx.take()), Vec::<&Effect>::new());
    assert_eq!(
        f.session.poll_timeout(),
        Some(drawn.saturating_add(FRAME_INTERVAL))
    );
}

#[test]
fn a_resize_reflows_a_pane_only_when_it_prints_or_shows() {
    let mut f = Fixture::new();
    let new_tab = Command::Session(SessionCommand::NewTab);
    f.message(ClientMessage::Command(&new_tab));
    f.ack();
    let (hidden, shown) = (PaneId::new(0), PaneId::new(1));
    f.message(ClientMessage::Resize {
        size: size(100, 30),
        cell_px: None,
    });
    // Every program learns the size at once...
    let effects = f.fx.take();
    for pane in [hidden, shown] {
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Resize(p, _) if *p == pane)),
            "{pane:?}: {effects:?}"
        );
    }
    // ...the shown pane is reflowed for its frame, the hidden one waits.
    assert_eq!(f.session.pane_reflow_pending_or_none(shown), Some(false));
    assert_eq!(f.session.pane_reflow_pending_or_none(hidden), Some(true));
    // Its program's output is parsed at the new size.
    f.ack();
    f.step(Input::PtyOutput {
        pane: hidden,
        bytes: b"redrawn",
    });
    assert_eq!(f.session.pane_reflow_pending_or_none(hidden), Some(false));
    // A quiet hidden pane is reflowed when its tab is shown.
    f.message(ClientMessage::Resize {
        size: size(90, 30),
        cell_px: None,
    });
    assert_eq!(f.session.pane_reflow_pending_or_none(hidden), Some(true));
    f.ack();
    let previous = Command::Session(SessionCommand::PreviousTab);
    f.message(ClientMessage::Command(&previous));
    assert_eq!(f.session.pane_reflow_pending_or_none(hidden), Some(false));
}

#[test]
fn an_idle_session_never_needs_a_tick() {
    let f = Fixture::new();
    assert_eq!(f.session.poll_timeout(), None);
}

#[test]
fn unbound_keys_go_to_the_focused_pane_encoded_by_its_emulator() {
    let mut f = Fixture::new();
    f.message(ClientMessage::Key(key('a', Mods::empty())));
    assert_eq!(f.fx.take(), [Effect::Write(PaneId::new(0), b"a".to_vec())]);
}

#[test]
fn replies_go_back_to_the_program() {
    let mut f = Fixture::new();
    // ENQ: the fake emulator answers with ACK.
    f.step(Input::PtyOutput {
        pane: PaneId::new(0),
        bytes: b"\x05",
    });
    assert!(
        f.fx.take()
            .contains(&Effect::Write(PaneId::new(0), vec![0x06]))
    );
}

#[test]
fn a_binding_splits_the_pane_and_starts_the_new_program() {
    let mut f = Fixture::new();
    // unlock_first: Ctrl-g unlocks, p picks pane mode, n splits right.
    f.message(ClientMessage::Key(key('g', Mods::CTRL)));
    f.ack();
    f.message(ClientMessage::Key(key('p', Mods::empty())));
    f.ack();
    f.fx.take();
    f.message(ClientMessage::Key(key('n', Mods::empty())));
    let effects = f.fx.take();
    // 80 columns: 39 for each pane plus the separator.
    assert!(effects.contains(&Effect::Resize(PaneId::new(0), size(39, 23))));
    assert!(effects.contains(&Effect::Spawn(PaneId::new(1), size(40, 23))));
    // Back in the base mode, with a frame showing both panes.
    let frames = Fixture::frames(&effects);
    assert!(matches!(
        frames[..],
        [Effect::Frame { mode: Mode::Locked, panes, .. }] if panes.len() == 2
    ));
}

#[test]
fn a_late_exit_of_a_closed_pane_changes_nothing() {
    let mut f = Fixture::new();
    let split = Command::Session(plx_core::command::SessionCommand::Split {
        direction: plx_core::geometry::Direction::Right,
    });
    f.message(ClientMessage::Command(&split));
    f.ack();
    let close = Command::Session(plx_core::command::SessionCommand::ClosePane);
    f.message(ClientMessage::Command(&close));
    // The new pane (focused after the split) is hung up and leaves the layout.
    assert!(f.fx.take().contains(&Effect::Close(PaneId::new(1))));
    assert_eq!(f.session.structure().panes(), [PaneId::new(0)]);
    f.ack();
    f.step(Input::PtyOutput {
        pane: PaneId::new(1),
        bytes: b"late output is dropped",
    });
    f.step(Input::PtyExited {
        pane: PaneId::new(1),
        status: ExitStatus::Code(0),
    });
    assert_eq!(f.session.structure().panes(), [PaneId::new(0)]);
    assert!(!f.fx.take().contains(&Effect::Exit));
}

#[test]
fn the_last_program_exiting_ends_the_session() {
    let mut f = Fixture::new();
    let pane = PaneId::new(0);
    f.step(Input::PtySpawned {
        pane,
        pid: Pid::new(NonZeroU32::new(42).unwrap()),
    });
    f.fx.take();
    f.step(Input::PtyExited {
        pane,
        status: ExitStatus::Code(0),
    });
    let effects = f.fx.take();
    assert!(effects.contains(&Effect::Ended(f.client, EndReason::LastPaneClosed)));
    assert!(effects.contains(&Effect::CloseClient(f.client)));
    assert_eq!(effects.last(), Some(&Effect::Exit));
}

#[test]
fn clients_must_attach_and_acknowledge_in_order() {
    let mut f = Fixture::new();
    let other = f.session.connect();
    f.step(Input::Client {
        client: other,
        message: ClientMessage::Key(key('a', Mods::empty())),
    });
    assert_eq!(
        f.fx.take(),
        [Effect::Rejected(other, Rejection::NotAttached)]
    );
    f.message(ClientMessage::Ack(FrameSeq::FIRST.next().next()));
    assert_eq!(
        f.fx.take(),
        [Effect::Rejected(
            f.client,
            Rejection::UnexpectedAck {
                got: FrameSeq::FIRST.next().next(),
                expected: None,
            }
        )]
    );
}

#[test]
fn detaching_closes_the_connection_and_keeps_the_session() {
    let mut f = Fixture::new();
    let detach = Command::Client(ClientCommand::Detach);
    f.message(ClientMessage::Command(&detach));
    let effects = f.fx.take();
    assert_eq!(
        &effects[..2],
        [Effect::Detached(f.client), Effect::CloseClient(f.client)]
    );
    assert!(!effects.contains(&Effect::Exit));
}

/// The steady state allocates nothing: output, frames and acks, keys.
#[test]
fn steady_state_steps_allocate_nothing() {
    struct Discard {
        frames: usize,
    }
    impl Effects for Discard {
        fn spawn_pty(&mut self, _: PaneId, _: GridSize, _: PaneProgram<'_>) {}
        fn write_pty(&mut self, _: PaneId, _: &[u8]) {}
        fn resize_pty(&mut self, _: PaneId, _: GridSize) {}
        fn close_pty(&mut self, _: PaneId) {}
        fn send<E: Emulator>(&mut self, _: ClientId, message: Outgoing<'_, E>) {
            if let Outgoing::Frame(view) = message {
                // Walk the frame as an encoder would.
                for update in view.panes() {
                    for (_, row) in update.rows() {
                        std::hint::black_box(row);
                    }
                    for style in update.styles() {
                        std::hint::black_box(style);
                    }
                }
                self.frames += 1;
            }
        }
        fn close_client(&mut self, _: ClientId) {}
        fn event(&mut self, _: EventSeq, _: Event<'_>) {}
        fn exit(&mut self) {}
    }
    let mut fx = Discard { frames: 0 };
    let mut session = Session::new(
        FakeFactory,
        Keymap::unlock_first(),
        SizePolicy::Latest,
        &mut fx,
    );
    let client = session.connect();
    let pane = PaneId::new(0);
    let mut now = 0;
    let mut seq = FrameSeq::FIRST;
    let mut round = |session: &mut Session<FakeFactory>, fx: &mut Discard, seq: &mut FrameSeq| {
        now += 20;
        session.step(
            at(now),
            Input::PtyOutput {
                pane,
                bytes: b"line of output\r\n",
            },
            fx,
        );
        session.step(
            at(now),
            Input::Client {
                client,
                message: ClientMessage::Key(key('x', Mods::empty())),
            },
            fx,
        );
        session.step(
            at(now),
            Input::Client {
                client,
                message: ClientMessage::Ack(*seq),
            },
            fx,
        );
        *seq = seq.next();
    };
    session.step(
        at(0),
        Input::Client {
            client,
            message: ClientMessage::Attach {
                size: size(80, 24),
                cell_px: None,
            },
        },
        &mut fx,
    );
    // Warm up: every buffer reaches its working size.
    for _ in 0..50 {
        round(&mut session, &mut fx, &mut seq);
    }
    let frames_before = fx.frames;
    let (counts, ()) = measure(|| {
        for _ in 0..200 {
            round(&mut session, &mut fx, &mut seq);
        }
    });
    assert!(fx.frames > frames_before + 100, "frames keep flowing");
    assert_eq!(counts.allocations, 0, "{counts:?}");
}

/// A step a well-behaved shell might take: pane and frame references are
/// indices into what exists, so every generated run is legal.
#[derive(Clone, Debug)]
enum ShellStep {
    Output(usize, Vec<u8>),
    Key(char, bool),
    Command(usize),
    AckLast,
    Spawned(usize),
    Exit(usize),
    Resize(u16, u16),
    Connect,
    Gone(usize),
    Tick,
}

fn shell_step() -> impl proptest::strategy::Strategy<Value = ShellStep> {
    use proptest::prelude::*;
    prop_oneof![
        4 => (any::<usize>(), proptest::collection::vec(prop_oneof![Just(b'a'), Just(b'\n'), Just(0x05_u8), Just(0x07_u8)], 0..40))
            .prop_map(|(i, bytes)| ShellStep::Output(i, bytes)),
        3 => (prop::sample::select(vec!['g', 'p', 't', 'n', 'd', 'x', 'h', 'l', 'f', 'r', 'q', 'a']), any::<bool>())
            .prop_map(|(c, ctrl)| ShellStep::Key(c, ctrl)),
        1 => any::<usize>().prop_map(ShellStep::Command),
        3 => Just(ShellStep::AckLast),
        1 => any::<usize>().prop_map(ShellStep::Spawned),
        1 => any::<usize>().prop_map(ShellStep::Exit),
        1 => (10_u16..120, 3_u16..50).prop_map(|(c, r)| ShellStep::Resize(c, r)),
        1 => Just(ShellStep::Connect),
        1 => any::<usize>().prop_map(ShellStep::Gone),
        1 => Just(ShellStep::Tick),
    ]
}

/// Tracks what a shell knows: live programs, clients, and the last frame
/// each client was sent.
#[derive(Default)]
struct Shell {
    effects: Vec<Effect>,
    /// Panes whose program was asked for and has not exited.
    programs: Vec<(PaneId, bool)>,
    clients: Vec<ClientId>,
    last_frame: Vec<(ClientId, FrameSeq)>,
    state: ShellState,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ShellState {
    #[default]
    Running,
    Exited,
}

impl Shell {
    fn absorb(&mut self, recorder: &mut Recorder) {
        for effect in recorder.take() {
            match &effect {
                Effect::Spawn(pane, _) | Effect::Editor(pane, _) => {
                    self.programs.push((*pane, false));
                }
                Effect::Frame { client, seq, .. } => {
                    self.last_frame.retain(|(c, _)| c != client);
                    self.last_frame.push((*client, *seq));
                }
                Effect::CloseClient(client) => {
                    self.clients.retain(|c| c != client);
                    self.last_frame.retain(|(c, _)| c != client);
                }
                Effect::Exit => self.state = ShellState::Exited,
                Effect::Write(..)
                | Effect::Resize(..)
                | Effect::Close(_)
                | Effect::Rejected(..)
                | Effect::Detached(_)
                | Effect::Ended(..)
                | Effect::Applied(_)
                | Effect::Clipboard(..)
                | Effect::Event(_) => {}
            }
            self.effects.push(effect);
        }
    }
}

proptest::proptest! {
    #[test]
    fn any_legal_shell_run_keeps_the_session_sound(steps in proptest::collection::vec(shell_step(), 0..120)) {
        use plx_core::command::SessionCommand;
        use plx_core::geometry::Direction;
        let commands = [
            Command::Session(SessionCommand::Split { direction: Direction::Right }),
            Command::Session(SessionCommand::Split { direction: Direction::Down }),
            Command::Session(SessionCommand::ClosePane),
            Command::Session(SessionCommand::NewTab),
            Command::Session(SessionCommand::NextTab),
            Command::Session(SessionCommand::ToggleZoom),
            Command::Session(SessionCommand::CloseTab),
            Command::Client(ClientCommand::Detach),
        ];
        let mut recorder = Recorder::default();
        let mut session = Session::new(FakeFactory, Keymap::unlock_first(), SizePolicy::Latest, &mut recorder);
        let mut shell = Shell::default();
        shell.absorb(&mut recorder);
        let mut now = 0_u64;
        for step in steps {
            if shell.state == ShellState::Exited {
                break;
            }
            now += 7;
            let t = at(now);
            match step {
                ShellStep::Output(i, bytes) if !shell.programs.is_empty() => {
                    let pane = shell.programs[i % shell.programs.len()].0;
                    session.step(t, Input::PtyOutput { pane, bytes: &bytes }, &mut recorder);
                }
                ShellStep::Spawned(i) if !shell.programs.is_empty() => {
                    let n = shell.programs.len();
                    let (pane, started) = &mut shell.programs[i % n];
                    if !*started {
                        *started = true;
                        let pid = Pid::new(NonZeroU32::new(100 + u32::try_from(pane.get()).unwrap()).unwrap());
                        session.step(t, Input::PtySpawned { pane: *pane, pid }, &mut recorder);
                    }
                }
                ShellStep::Exit(i) if !shell.programs.is_empty() => {
                    let n = shell.programs.len();
                    let (pane, _) = shell.programs.remove(i % n);
                    session.step(t, Input::PtyExited { pane, status: ExitStatus::Code(0) }, &mut recorder);
                }
                ShellStep::Key(c, ctrl) => {
                    if let Some(&client) = shell.clients.first() {
                        let mods = if ctrl { Mods::CTRL } else { Mods::empty() };
                        let message = ClientMessage::Key(key(c, mods));
                        session.step(t, Input::Client { client, message }, &mut recorder);
                    }
                }
                ShellStep::Command(i) => {
                    if let Some(&client) = shell.clients.first() {
                        let message = ClientMessage::Command(&commands[i % commands.len()]);
                        session.step(t, Input::Client { client, message }, &mut recorder);
                    }
                }
                ShellStep::AckLast => {
                    if let Some(&(client, seq)) = shell.last_frame.first() {
                        shell.last_frame.remove(0);
                        session.step(t, Input::Client { client, message: ClientMessage::Ack(seq) }, &mut recorder);
                    }
                }
                ShellStep::Resize(cols, rows) => {
                    if let Some(&client) = shell.clients.first() {
                        let message = ClientMessage::Resize { size: size(cols, rows), cell_px: None };
                        session.step(t, Input::Client { client, message }, &mut recorder);
                    }
                }
                ShellStep::Connect => {
                    let client = session.connect();
                    shell.clients.push(client);
                    let message = ClientMessage::Attach { size: size(80, 24), cell_px: None };
                    session.step(t, Input::Client { client, message }, &mut recorder);
                }
                ShellStep::Gone(i) if !shell.clients.is_empty() => {
                    let client = shell.clients.remove(i % shell.clients.len());
                    shell.last_frame.retain(|(c, _)| *c != client);
                    session.step(t, Input::ClientGone { client }, &mut recorder);
                }
                ShellStep::Output(..) | ShellStep::Spawned(_) | ShellStep::Exit(_) | ShellStep::Gone(_) | ShellStep::Tick => {
                    session.step(t, Input::Tick, &mut recorder);
                }
            }
            shell.absorb(&mut recorder);
            if shell.state == ShellState::Running {
                session.structure().assert_invariants();
                // Every pane in the structure has a program the shell knows.
                for pane in session.structure().panes() {
                    proptest::prop_assert!(shell.programs.iter().any(|(p, _)| *p == pane));
                }
            }
        }
        // Nothing was ever rejected but by a client's own mistake: this shell
        // makes none, so only command rejections (no neighbour, ...) appear.
        for effect in &shell.effects {
            if let Effect::Rejected(_, rejection) = effect {
                proptest::prop_assert!(!matches!(rejection, Rejection::NotAttached | Rejection::AlreadyAttached | Rejection::UnexpectedAck { .. }), "{rejection:?}");
            }
        }
    }
}

/// A connected client that never attaches (the CLI) runs session commands
/// and hears that they applied; it is sent no frames.
#[test]
fn an_unattached_client_runs_session_commands() {
    use plx_core::geometry::Direction;
    let mut fixture = Fixture::new();
    let cli = fixture.session.connect();
    let split = Command::Session(SessionCommand::Split {
        direction: Direction::Right,
    });
    fixture.step(Input::Client {
        client: cli,
        message: ClientMessage::Command(&split),
    });
    let effects = fixture.fx.take();
    assert!(
        effects.iter().any(|e| matches!(e, Effect::Spawn(..))),
        "{effects:?}"
    );
    assert!(effects.contains(&Effect::Applied(cli)), "{effects:?}");
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::Frame { client, .. } if *client == cli))
    );
    // A command that cannot apply is rejected, not acknowledged.
    let missing = Command::Session(SessionCommand::FocusTab {
        tab: plx_core::tab::TabNumber::new(9.try_into().unwrap()),
    });
    fixture.step(Input::Client {
        client: cli,
        message: ClientMessage::Command(&missing),
    });
    let effects = fixture.fx.take();
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Rejected(c, _) if *c == cli)),
        "{effects:?}"
    );
    assert!(!effects.contains(&Effect::Applied(cli)));
    // Detaching needs attaching first.
    let detach = Command::Client(ClientCommand::Detach);
    fixture.step(Input::Client {
        client: cli,
        message: ClientMessage::Command(&detach),
    });
    assert_eq!(
        fixture.fx.take(),
        [Effect::Rejected(cli, Rejection::NotAttached)]
    );
}

/// Ending the session tells every client, attached or not, and the command
/// that ended it is not acknowledged after the end.
#[test]
fn ending_the_session_tells_every_client() {
    let mut fixture = Fixture::new();
    let cli = fixture.session.connect();
    let kill = Command::Session(SessionCommand::KillSession);
    fixture.step(Input::Client {
        client: cli,
        message: ClientMessage::Command(&kill),
    });
    let effects = fixture.fx.take();
    let attached = fixture.client;
    assert!(
        effects.contains(&Effect::Ended(attached, EndReason::Killed)),
        "{effects:?}"
    );
    assert!(
        effects.contains(&Effect::Ended(cli, EndReason::Killed)),
        "{effects:?}"
    );
    assert!(effects.contains(&Effect::CloseClient(cli)));
    assert!(!effects.contains(&Effect::Applied(cli)));
    assert_eq!(effects.last(), Some(&Effect::Exit));
}

fn mouse(
    action: plx_emulator::MouseAction,
    button: plx_emulator::MouseButton,
    x: u16,
    y: u16,
) -> ClientMessage<'static> {
    ClientMessage::Mouse(plx_emulator::MouseEvent {
        action,
        button,
        position: plx_emulator::Position { x, y },
        mods: Mods::empty(),
    })
}

#[test]
fn a_click_focuses_a_pane_and_a_drag_moves_its_separator() {
    use plx_emulator::{MouseAction, MouseButton};
    let mut f = Fixture::new();
    let split = Command::Session(SessionCommand::Split {
        direction: plx_core::geometry::Direction::Right,
    });
    f.message(ClientMessage::Command(&split));
    f.ack();
    f.fx.take();
    let separator_x = |f: &Fixture| {
        let area = plx_core::size::pane_area(f.session.size());
        let arrangement = f.session.structure().active().layout().arrange(area);
        arrangement.separators[0].rect.x()
    };
    let x = separator_x(&f);
    // The new pane (right) has focus; a click on the left one focuses it.
    assert_eq!(f.session.structure().active().focus(), PaneId::new(1));
    f.message(mouse(MouseAction::Press, MouseButton::Left, 2, 2));
    f.message(mouse(MouseAction::Release, MouseButton::Left, 2, 2));
    assert_eq!(f.session.structure().active().focus(), PaneId::new(0));
    assert!(
        f.fx.take()
            .iter()
            .any(|e| matches!(e, Effect::Event(event) if event.contains("FocusChanged")))
    );
    // Pressing the separator and dragging moves it to the pointer.
    f.message(mouse(MouseAction::Press, MouseButton::Left, x, 5));
    f.message(mouse(MouseAction::Motion, MouseButton::Left, 60, 5));
    assert_eq!(separator_x(&f), 60);
    f.message(mouse(MouseAction::Motion, MouseButton::Left, 20, 9));
    assert_eq!(separator_x(&f), 20);
    assert!(
        f.fx.take()
            .iter()
            .any(|e| matches!(e, Effect::Event(event) if event.contains("SplitMoved")))
    );
    // After the release, motion moves nothing.
    f.message(mouse(MouseAction::Release, MouseButton::Left, 20, 9));
    f.message(mouse(MouseAction::Motion, MouseButton::Left, 70, 9));
    assert_eq!(separator_x(&f), 20);
}

/// Zellij's search: `/` in scroll mode starts typing a needle (letters are
/// text there, `Backspace` deletes), `Enter` goes on to search mode, where
/// letters are commands again; leaving the search modes ends the search,
/// and `Esc` while typing gives up, back to scroll mode.
#[test]
fn a_search_lives_while_its_client_is_in_a_search_mode() {
    let mut f = Fixture::new();
    let press = |key| KeyEvent::press(key, Mods::empty());
    let last = |f: &Fixture| {
        let mode =
            f.fx.effects
                .iter()
                .rev()
                .find_map(|effect| {
                    if let Effect::Frame { mode, .. } = effect {
                        Some(*mode)
                    } else {
                        None
                    }
                })
                .unwrap();
        (mode, f.fx.searches.last().unwrap().clone())
    };
    let typed = |needle: &str, options| Some((needle.to_owned(), options));
    let any = SearchOptions::default();
    for event in [key('g', Mods::CTRL), key('s', Mods::empty())] {
        f.message(ClientMessage::Key(event));
        f.ack();
    }
    assert_eq!(last(&f), (Mode::Scroll, None));
    f.message(ClientMessage::Key(key('/', Mods::empty())));
    f.ack();
    assert_eq!(last(&f), (Mode::EnterSearch, typed("", any)));
    for event in [
        key('c', Mods::empty()),
        key('O', Mods::SHIFT),
        key('x', Mods::empty()),
        press(Key::Backspace),
    ] {
        f.message(ClientMessage::Key(event));
        f.ack();
    }
    assert_eq!(last(&f), (Mode::EnterSearch, typed("cO", any)));
    // A chord is not text.
    f.message(ClientMessage::Key(key('x', Mods::ALT)));
    f.message(ClientMessage::Key(press(Key::Enter)));
    f.ack();
    assert_eq!(last(&f), (Mode::Search, typed("cO", any)));
    // In search mode `c` and `o` toggle letter case and whole words.
    f.message(ClientMessage::Key(key('c', Mods::empty())));
    f.ack();
    let case = SearchOptions {
        case: Case::Matched,
        words: Words::Within,
    };
    assert_eq!(last(&f), (Mode::Search, typed("cO", case)));
    // Back to typing keeps the needle; giving up ends the search.
    f.message(ClientMessage::Key(key('/', Mods::empty())));
    f.ack();
    assert_eq!(last(&f), (Mode::EnterSearch, typed("cO", case)));
    f.message(ClientMessage::Key(press(Key::Escape)));
    f.ack();
    assert_eq!(last(&f), (Mode::Scroll, None));
    // The options stay for the next search.
    f.message(ClientMessage::Key(key('/', Mods::empty())));
    f.ack();
    assert_eq!(last(&f), (Mode::EnterSearch, typed("", case)));
    f.message(ClientMessage::Key(press(Key::Enter)));
    f.ack();
    f.message(ClientMessage::Key(press(Key::Escape)));
    f.ack();
    assert_eq!(last(&f), (Mode::Locked, None));
}

/// `e` in scroll mode opens the focused pane's history in the editor, in
/// the pane's place (the same size, the same tab); the pane runs on hidden
/// and is back, focused, when the editor exits.
#[test]
fn the_scrollback_opens_in_an_editor_in_the_panes_place() {
    let mut f = Fixture::new();
    let split = Command::Session(SessionCommand::Split {
        direction: plx_core::geometry::Direction::Right,
    });
    f.message(ClientMessage::Command(&split));
    let pane = f.session.structure().active().focus();
    f.step(Input::PtyOutput {
        pane,
        bytes: b"$ make\r\nok",
    });
    f.fx.take();
    // Unlock, scroll mode, edit.
    for event in [
        key('g', Mods::CTRL),
        key('s', Mods::empty()),
        key('e', Mods::empty()),
    ] {
        f.message(ClientMessage::Key(event));
    }
    let effects = f.fx.take();
    let editor = effects
        .iter()
        .find_map(|effect| {
            if let Effect::Editor(pane, text) = effect {
                Some((*pane, text.clone()))
            } else {
                None
            }
        })
        .unwrap_or_else(|| panic!("an editor started: {effects:?}"));
    assert!(editor.1.starts_with("$ make\nok\n"), "{:?}", editor.1);
    // No pane's program was hung up: the edited one runs on.
    assert!(!effects.contains(&Effect::Close(pane)), "{effects:?}");
    let structure = f.session.structure();
    assert_eq!(structure.tabs().count(), 1);
    assert_eq!(structure.active().focus(), editor.0);
    assert!(!structure.active().layout().contains(pane));
    f.step(Input::PtySpawned {
        pane: editor.0,
        pid: Pid::new(NonZeroU32::new(77).unwrap()),
    });
    f.step(Input::PtyExited {
        pane: editor.0,
        status: ExitStatus::Code(0),
    });
    let structure = f.session.structure();
    assert_eq!(structure.active().focus(), pane);
    assert_eq!(structure.active().layout().panes().len(), 2);
}

/// A split's shell starts where the pane it splits is; a session's first
/// shell, and a new tab's, where the session's programs start.
#[test]
fn a_split_starts_where_the_split_pane_is() {
    let mut f = Fixture::new();
    let first = f.session.structure().active().focus();
    let split = Command::Session(SessionCommand::Split {
        direction: plx_core::geometry::Direction::Down,
    });
    f.message(ClientMessage::Command(&split));
    let second = f.session.structure().active().focus();
    let new_tab = Command::Session(SessionCommand::NewTab);
    f.message(ClientMessage::Command(&new_tab));
    let third = f.session.structure().active().focus();
    assert_eq!(
        f.fx.starts,
        [
            (first, StartIn::Default),
            (second, StartIn::PaneOf(first)),
            (third, StartIn::Default),
        ]
    );
}

/// A pane's history is compressed once its output and every client's
/// input have been quiet for `COMPRESS_AFTER`: the session wakes for it
/// once, and then sleeps again.
#[test]
fn quiet_history_is_compressed_once_then_the_session_sleeps() {
    use plx_core::session::COMPRESS_AFTER;
    let mut f = Fixture::new();
    let pane = f.session.structure().active().focus();
    f.step(Input::PtyOutput {
        pane,
        bytes: b"output",
    });
    f.ack();
    let output_at = at(f.now - 20);
    assert_eq!(
        f.session.poll_timeout(),
        Some(output_at.saturating_add(COMPRESS_AFTER))
    );
    // Typing puts it off.
    f.message(ClientMessage::Key(key('x', Mods::empty())));
    let typed_at = at(f.now);
    assert_eq!(
        f.session
            .poll_timeout()
            .map(|t| t >= typed_at.saturating_add(COMPRESS_AFTER)),
        Some(true)
    );
    // Quiet long enough: one tick compresses (the fake has nothing to
    // compress), and nothing is left to wake for.
    f.now += 2_000;
    f.step(Input::Tick);
    assert_eq!(f.session.poll_timeout(), None);
}

/// A selection dragged past its pane's edge scrolls on while the pointer
/// stays there: the session wakes every `AUTOSCROLL_INTERVAL`, keeping that
/// pace however often the pointer moves, until the pointer comes back
/// inside or the button is let go.
#[test]
fn a_selection_dragged_past_the_edge_wakes_the_session_to_scroll() {
    use plx_core::session::COMPRESS_AFTER;
    use plx_emulator::{AUTOSCROLL_INTERVAL, MouseAction, MouseButton};
    let mut f = Fixture::new();
    let left = |action, x, y| mouse(action, MouseButton::Left, x, y);
    // Scrolling touches the history, which is compressed once quiet: that
    // wake is a second away, not a scroll.
    let scroll_wake = |f: &Fixture| {
        f.session
            .poll_timeout()
            .filter(|&wake| wake < at(f.now).saturating_add(COMPRESS_AFTER))
    };
    // The pane spans rows 0 to 22; row 23 is the status row. Frames are
    // left unacknowledged from here, so only scrolling wakes the session.
    f.message(left(MouseAction::Press, 5, 10));
    f.message(left(MouseAction::Motion, 5, 5));
    assert_eq!(scroll_wake(&f), None);
    // Onto the status row, below the pane.
    f.message(left(MouseAction::Motion, 5, 23));
    let dragged = at(f.now);
    let interval = |n: u32| AUTOSCROLL_INTERVAL * n;
    assert_eq!(scroll_wake(&f), Some(dragged.saturating_add(interval(1))));
    // Moving on past the edge keeps the pace; the step scrolled once.
    f.message(left(MouseAction::Motion, 6, 23));
    assert_eq!(scroll_wake(&f), Some(dragged.saturating_add(interval(2))));
    // A step that comes late catches up a few rows, then goes on from now.
    f.now += 200;
    f.step(Input::Tick);
    assert_eq!(scroll_wake(&f), Some(at(f.now).saturating_add(interval(1))));
    // Back inside: it stops; the history scrolled through is compressed
    // once quiet.
    f.message(left(MouseAction::Motion, 5, 10));
    assert_eq!(scroll_wake(&f), None);
    assert_eq!(
        f.session.poll_timeout(),
        Some(at(f.now).saturating_add(COMPRESS_AFTER))
    );
    // The pane's top row is the screen's: a pointer above the window is
    // reported there, so it counts as past the edge. Letting go stops it.
    f.message(left(MouseAction::Motion, 5, 0));
    assert_eq!(scroll_wake(&f), Some(at(f.now).saturating_add(interval(1))));
    f.message(left(MouseAction::Release, 5, 0));
    assert_eq!(scroll_wake(&f), None);
    // Motion with no button held means the button was let go unseen: the
    // drag is over, and a later motion starts none.
    f.message(left(MouseAction::Press, 5, 10));
    f.message(left(MouseAction::Motion, 5, 0));
    assert!(scroll_wake(&f).is_some());
    f.message(mouse(MouseAction::Motion, MouseButton::None, 5, 0));
    assert_eq!(scroll_wake(&f), None);
    f.message(left(MouseAction::Motion, 5, 0));
    assert_eq!(scroll_wake(&f), None);
}

fn percent(p: u8) -> Percent {
    Percent::new(p).unwrap()
}

impl Fixture {
    /// The progress reports of the frames sent since the last call.
    fn progress(&mut self) -> Vec<(ClientId, Option<Progress>)> {
        self.fx.effects.clear();
        std::mem::take(&mut self.fx.progress)
    }
}

/// The focused pane's progress report rides in the next frame, even with
/// nothing else to draw; each report goes once, a repeated one too (a
/// terminal drops a bar that is not renewed), and a report may arrive in
/// pieces.
#[test]
fn the_focused_panes_progress_rides_in_the_next_frame() {
    let mut f = Fixture::new();
    let (client, pane) = (f.client, PaneId::new(0));
    f.progress();
    for (bytes, progress) in [
        (
            &b"\x1b]9;4;1;50\x07"[..],
            Some(Progress::Set(Some(percent(50)))),
        ),
        (b"text", None),
        (b"\x1b]9;4;1;50\x07", Some(Progress::Set(Some(percent(50))))),
        (b"\x1b]9;4;0\x07", Some(Progress::Removed)),
        (b"\x1b]9;4;0\x07", None),
    ] {
        f.step(Input::PtyOutput { pane, bytes });
        assert_eq!(f.progress(), [(client, progress)], "{bytes:?}");
        f.ack();
    }
    f.step(Input::PtyOutput {
        pane,
        bytes: b"\x1b]9;4;",
    });
    f.ack();
    f.progress();
    f.step(Input::PtyOutput {
        pane,
        bytes: b"3\x07",
    });
    assert_eq!(f.progress(), [(client, Some(Progress::Indeterminate))]);
}

/// A client's terminal shows the focused pane's progress: moving the focus
/// shows the new pane's, or removes what was shown.
#[test]
fn the_focus_picks_whose_progress_is_shown() {
    let mut f = Fixture::new();
    let client = f.client;
    let split = Command::Session(SessionCommand::Split {
        direction: plx_core::geometry::Direction::Right,
    });
    f.message(ClientMessage::Command(&split));
    f.ack();
    let (left, right) = (PaneId::new(0), PaneId::new(1));
    assert_eq!(f.session.structure().active().focus(), right);
    f.progress();
    // A pane without the focus reports: nothing to show yet.
    f.step(Input::PtyOutput {
        pane: left,
        bytes: b"\x1b]9;4;2;30\x07",
    });
    assert_eq!(f.progress(), [(client, None)]);
    f.ack();
    for (pane, progress) in [
        (left, Some(Progress::Error(Some(percent(30))))),
        (right, Some(Progress::Removed)),
        (left, Some(Progress::Error(Some(percent(30))))),
    ] {
        let focus = Command::Session(SessionCommand::FocusPane { pane });
        f.message(ClientMessage::Command(&focus));
        assert_eq!(f.progress(), [(client, progress)], "{pane:?}");
        f.ack();
    }
}

/// A client that attaches is told the focused pane's progress at once.
#[test]
fn an_attaching_client_is_told_the_progress() {
    let mut f = Fixture::new();
    f.step(Input::PtyOutput {
        pane: PaneId::new(0),
        bytes: b"\x1b]9;4;3\x07",
    });
    f.progress();
    let second = f.session.connect();
    f.step(Input::Client {
        client: second,
        message: ClientMessage::Attach {
            size: size(80, 24),
            cell_px: None,
        },
    });
    assert!(
        f.progress()
            .contains(&(second, Some(Progress::Indeterminate)))
    );
}
