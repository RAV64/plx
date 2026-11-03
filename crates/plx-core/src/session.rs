//! The session: the whole state machine behind one daemon.
//!
//! [`Session::step`] takes one [`Input`] at a time, with the time it happened,
//! and asks the outside world for everything through an [`Effects`] sink. It
//! does no IO and reads no clock, so the simulation and the daemon drive the
//! same code.
//!
//! - Panes exist before their program: the structure decides a pane, the
//!   session creates its emulator and asks for a PTY, so output can never
//!   arrive for a pane it does not know.
//! - Program output is fed at once but synced lazily, once per frame, so a
//!   flood coalesces into whatever the next frame shows.
//! - Each client gets frames of what it has not drawn, at most one in flight
//!   (acknowledged before the next) and at most one per [`FRAME_INTERVAL`]:
//!   a slow client gets fewer frames, never a queue.
//! - Except for echo: the first output of a pane after a client typed into
//!   it is sent without waiting for the interval, so
//!   typing stays immediate while other panes flood.

use alloc::vec::Vec;
use core::time::Duration;

use plx_bug::{OrBug as _, bug, invariant};
use plx_collections::IdMap;
use plx_emulator::{
    AUTOSCROLL_INTERVAL, ActiveScreen, Autoscroll, CellPx, Compression, Emulator, EmulatorFactory,
    GridSize, Key, KeyEvent, Mods, MouseAction, MouseButton, MouseEvent, MouseTracking, Past,
    Position, Progress, Scroll, SelectAt, SelectGesture, Selection, Version, Wheel,
};

use crate::command::{ClientCommand, Command, ScrollBy, SearchCommand, SessionCommand};
use crate::effects::{Effects, History, PaneProgram, StartIn};
use crate::event::{EndReason, Event, EventSeq, Pid, StructureEvent};
use crate::geometry::{Axis, Direction, Rect};
use crate::id::{ClientId, PaneId};
use crate::input::Input;
use crate::layout::{Arrangement, SplitIndex};
use crate::message::{ClientMessage, Copied, FrameSeq, FrameView, Outgoing};
use crate::mode::{Keymap, Mode, Resolution, Scrolling, Searching};
use crate::rejection::Rejection;
use crate::search::{Search, Typed};
use crate::size::{ClientSize, SizePolicy, pane_area};
use crate::structure::{Lifecycle, Structure};
use crate::time::Timestamp;

/// How long a pane's output, and every client's input, must have been
/// quiet before its history is compressed: a step costs a few
/// milliseconds, so it never runs while someone types or output flows.
pub const COMPRESS_AFTER: Duration = Duration::from_secs(1);

/// How many rows a wheel notch scrolls.
const WHEEL_LINES: u16 = 3;

/// Whether `rect` holds the cell at `at`.
fn contains(rect: Rect, at: Position) -> bool {
    rect.contains(u32::from(at.x), u32::from(at.y))
}

/// The session cell `at` as a pointer selecting in the pane at `rect`, in a
/// session `size` big: its cell in the pane, and whether it is past the
/// pane's top or bottom edge. A terminal reports a pointer beyond its window
/// on the window's edge row, so where the pane touches the screen's edge,
/// that row counts as past it (as tmux scrolls a drag there).
fn select_at(rect: Rect, size: GridSize, at: Position) -> SelectAt {
    let y = u32::from(at.y);
    let (top, bottom) = (u32::from(rect.y()), rect.bottom());
    let past = if y < top || (y == top && top == 0) {
        Past::Top
    } else if y >= bottom || (y + 1 == bottom && bottom == u32::from(size.rows().get())) {
        Past::Bottom
    } else {
        Past::Nothing
    };
    SelectAt {
        cell: Position {
            x: at.x.saturating_sub(rect.x()),
            y: at.y.saturating_sub(rect.y()),
        },
        past,
    }
}

/// The shortest time between two frames to one client: 60 per second.
pub const FRAME_INTERVAL: Duration = Duration::from_nanos(16_666_667);

/// How long after input a pane's output still counts as its echo: enough
/// for a slow remote shell.
pub const ECHO_WINDOW: Duration = Duration::from_secs(1);

/// A pane's program and emulator.
#[derive(Debug)]
pub(crate) struct Pane<E> {
    pub(crate) emulator: E,
    process: Process,
    /// The size the emulator and PTY were last given.
    size: GridSize,
    sync: SyncNeed,
    echo: Echo,
    echoed: Echoed,
    reflow: Reflow,
    compression: HistoryCompression,
    progress: Reported,
}

/// A pane's progress, as its program last reported it (OSC 9;4), with how
/// many reports it has made: each report reaches a client once, a repeated
/// one too, since a terminal drops a progress bar that is not renewed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reported {
    progress: Progress,
    reports: u64,
}

impl Reported {
    const NONE: Self = Self {
        progress: Progress::Removed,
        reports: 0,
    };
}

/// What a client's terminal was last told about progress. Its terminal
/// shows the focused pane's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Told {
    /// Nothing, or that there is nothing to show.
    Nothing,
    /// A pane's report, by its number.
    Report { pane: PaneId, reports: u64 },
}

impl Told {
    /// The report a frame carries for a client told `self`, now that `pane`
    /// has focus, and what the client is told then: the focused pane's
    /// report, unless the client has it, or nothing to show where there is
    /// nothing.
    fn news(self, pane: PaneId, reported: Reported) -> (Option<Progress>, Self) {
        match (self, reported.progress) {
            (
                Self::Report {
                    pane: told,
                    reports,
                },
                _,
            ) if told == pane && reports == reported.reports => (None, self),
            (Self::Nothing, Progress::Removed) => (None, Self::Nothing),
            (Self::Report { .. }, Progress::Removed) => (Some(Progress::Removed), Self::Nothing),
            (
                Self::Nothing | Self::Report { .. },
                progress @ (Progress::Set(_)
                | Progress::Error(_)
                | Progress::Indeterminate
                | Progress::Paused(_)),
            ) => (
                Some(progress),
                Self::Report {
                    pane,
                    reports: reported.reports,
                },
            ),
        }
    }
}

/// Whether a pane's history is to be compressed. New output, a reflow,
/// scrolling, searching and editing the history leave pages uncompressed;
/// compression then waits for quiet (see [`COMPRESS_AFTER`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HistoryCompression {
    Done,
    /// Since the history was last touched.
    Wanted {
        since: Timestamp,
    },
}

/// Whether the emulator has the size the PTY was given.
///
/// Reflowing a full scrollback costs about 1.8 ms, so a resize
/// gives the PTY its size at once but the emulator only when it is next
/// needed: before the program's next output, or before the pane is next
/// shown. Hidden, quiet panes cost nothing until then, and repeated
/// resizes (a window being dragged) reflow once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reflow {
    Done,
    Pending,
}

impl<E: Emulator> Pane<E> {
    /// Gives the emulator the size its PTY has, if it does not have it yet.
    fn reflow(
        &mut self,
        id: PaneId,
        cell_px: Option<CellPx>,
        now: Timestamp,
        fx: &mut impl Effects,
    ) -> Result<(), E::Error> {
        if self.reflow == Reflow::Pending {
            self.reflow = Reflow::Done;
            // A reflow restores every compressed page.
            self.touched(now);
            let events = self.emulator.resize(self.size, cell_px)?;
            if !events.replies().is_empty() {
                fx.write_pty(id, events.replies());
            }
        }
        Ok(())
    }
}

impl<E> Pane<E> {
    /// Its history was changed or read: compress it again once quiet.
    const fn touched(&mut self, now: Timestamp) {
        self.compression = HistoryCompression::Wanted { since: now };
    }
}

/// Whether a pane's output since its last sync includes echo, which an
/// echo frame carries alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Echoed {
    No,
    Yes,
}

/// Which panes a sync covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    All,
    /// Only panes with echo: a frame sent early for echo carries nothing
    /// else, so it stays small however much the other panes print.
    Echoed,
}

/// Whether a pane's next output may be the echo of client input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Echo {
    None,
    /// Input was written to it at this time.
    Awaited(Timestamp),
}

/// Whether a client's next frame waits for the frame interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pace {
    Interval,
    /// It shows echo: sent as soon as nothing is in flight.
    Echo,
}

/// Where a pane's program is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Process {
    /// Asked for; `PtySpawned` or `PtySpawnFailed` comes next.
    Spawning,
    Running(Pid),
    /// The pane is closed; its program was hung up and `PtyExited` comes
    /// next. Output until then is dropped.
    Closing,
}

/// Whether a pane's screen lags its emulator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncNeed {
    Clean,
    Dirty,
}

/// Whether a client has something new to draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrameDue {
    Idle,
    Due,
}

/// A connected client.
#[derive(Debug)]
enum Client {
    /// Connected, not yet attached.
    Connected,
    Attached(Attached),
}

#[derive(Debug)]
struct Attached {
    size: GridSize,
    mode: Mode,
    last_active: Timestamp,
    /// The version of each pane this client has drawn.
    acked: IdMap<PaneId, Version>,
    /// The frame sent and not yet acknowledged, with the version it carried.
    inflight: Option<(FrameSeq, Version)>,
    /// The panes the in-flight frame showed (reused between frames).
    inflight_panes: Vec<PaneId>,
    next_seq: FrameSeq,
    due: FrameDue,
    pace: Pace,
    last_frame: Option<Timestamp>,
    /// What the button held down belongs to.
    grab: Grab,
    progress: Told,
}

/// What a mouse press started, until its release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Grab {
    None,
    /// A program that tracks the mouse: its motion and release go there,
    /// even outside the pane.
    Pane(PaneId),
    /// A separator being dragged: it moves its split.
    Split(SplitIndex),
    /// Text being selected in a pane whose program does not take the mouse.
    Selecting {
        pane: PaneId,
        scroll: Autoscrolling,
    },
}

/// Whether a selection dragged past its pane's edge scrolls the pane's view,
/// and when it moves the next row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Autoscrolling {
    No,
    Next(Timestamp),
}

impl Autoscrolling {
    /// After a drag step that says `autoscroll`. A drag that goes on
    /// scrolling keeps its pace, however often the pointer moves.
    fn after(self, autoscroll: Autoscroll, now: Timestamp) -> Self {
        match (autoscroll, self) {
            (Autoscroll::Stopped, _) => Self::No,
            (Autoscroll::Scrolling, Self::Next(next)) => Self::Next(next),
            (Autoscroll::Scrolling, Self::No) => {
                Self::Next(now.saturating_add(AUTOSCROLL_INTERVAL))
            }
        }
    }
}

/// The most rows an autoscrolling selection catches up in one step, when
/// the step comes late: more would jump rather than scroll.
const MAX_AUTOSCROLL_CATCH_UP: u8 = 8;

/// Whether a command took effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Applied {
    Yes,
    No,
}

/// Whether the session still runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Running,
    Ended,
}

/// One session. Not `Clone`: the daemon owns exactly one.
pub struct Session<F: EmulatorFactory> {
    keymap: Keymap,
    world: World<F>,
}

/// Everything but the keymap, so a binding's command can be lent from the
/// keymap while the rest changes.
struct World<F: EmulatorFactory> {
    factory: F,
    policy: SizePolicy,
    structure: Structure,
    panes: IdMap<PaneId, Pane<F::Emulator>>,
    clients: IdMap<ClientId, Client>,
    next_client: ClientId,
    size: GridSize,
    cell_px: Option<CellPx>,
    /// The version the next sync stamps; it only grows.
    version: Version,
    events: EventSeq,
    /// The active tab, arranged into the pane area.
    arrangement: Arrangement,
    /// Scratch: other tabs' arrangements, encoded input, failed panes and
    /// client sizes, reused so steady-state steps never allocate.
    scratch: Arrangement,
    bytes: Vec<u8>,
    /// Selected text (UTF-8) on its way to a client's clipboard (reused).
    copied: Vec<u8>,
    /// The pane holding the selection, if any.
    selected: Option<PaneId>,
    search: Search,
    /// A pane's history on its way to an editor (only while one opens).
    history: Vec<u8>,
    /// The pane being created to run the editor, during one reconcile.
    opening_editor: Option<PaneId>,
    /// A pane being split open, and the pane it splits, during one
    /// reconcile: the new shell starts where that pane is.
    splitting: Option<(PaneId, PaneId)>,
    failed: Vec<PaneId>,
    sizes: Vec<ClientSize>,
    state: State,
    /// The time of the step being handled.
    now: Timestamp,
    /// When a client last typed, pasted or used the mouse.
    last_input: Timestamp,
}

/// The attached client that was used last; `None` when none is attached.
fn most_recently_active_or_none(clients: &IdMap<ClientId, Client>) -> Option<ClientId> {
    clients
        .iter()
        .filter_map(|(id, client)| match client {
            Client::Attached(attached) => Some((attached.last_active, id)),
            Client::Connected => None,
        })
        .max()
        .map(|(_, id)| id)
}

/// The session size before any client attaches.
fn initial_size() -> GridSize {
    GridSize::new(
        80.try_into().or_bug("80 is not zero"),
        24.try_into().or_bug("24 is not zero"),
    )
    .or_bug("80x24 is within the grid limits")
}

impl<F: EmulatorFactory> Session<F> {
    /// A new session with its first pane, whose program is started at once.
    pub fn new(factory: F, keymap: Keymap, policy: SizePolicy, fx: &mut impl Effects) -> Self {
        let mut world = World {
            factory,
            policy,
            structure: Structure::new(),
            panes: IdMap::new(),
            clients: IdMap::new(),
            next_client: ClientId::FIRST,
            size: initial_size(),
            cell_px: None,
            version: Version::ZERO,
            events: EventSeq::FIRST,
            arrangement: Arrangement::default(),
            scratch: Arrangement::default(),
            bytes: Vec::new(),
            copied: Vec::new(),
            selected: None,
            search: Search::default(),
            history: Vec::new(),
            opening_editor: None,
            splitting: None,
            failed: Vec::new(),
            sizes: Vec::new(),
            state: State::Running,
            now: Timestamp::ZERO,
            last_input: Timestamp::ZERO,
        };
        world.reconcile(fx);
        Self { keymap, world }
    }

    /// A client connected; it attaches with its first message.
    pub fn connect(&mut self) -> ClientId {
        let client = self.world.next_client;
        self.world.next_client = client.next();
        self.world.clients.insert(client, Client::Connected);
        client
    }

    /// Handles one input that happened at `now`.
    #[track_caller]
    pub fn step(&mut self, now: Timestamp, input: Input<'_>, fx: &mut impl Effects) {
        let world = &mut self.world;
        invariant!(
            world.state == State::Running,
            "the shell stops stepping a session that exited"
        );
        world.now = now;
        match input {
            Input::Client { client, message } => {
                world.message(&self.keymap, now, client, message, fx);
            }
            Input::ClientGone { client } => world.client_gone(client, fx),
            Input::PtySpawned { pane, pid } => world.spawned(pane, pid, fx),
            Input::PtySpawnFailed { pane, error } => {
                world.emit(fx, Event::PaneSpawnFailed { pane, error });
                world.process_ended(pane, fx);
            }
            Input::PtyOutput { pane, bytes } => world.output(now, pane, bytes, fx),
            Input::PtyExited { pane, status } => {
                world.emit(fx, Event::PaneExited { pane, status });
                world.process_ended(pane, fx);
            }
            Input::Tick => {}
        }
        if world.state == State::Running {
            world.autoscroll(now);
            world.compress_idle(now);
            world.flush(now, fx);
        }
    }

    /// When to `Tick` next: the earliest time a due frame may be sent, a
    /// selection dragged past its pane's edge scrolls a row, or a pane's
    /// history is compressed. `None` when nothing is waiting, so an idle
    /// session never wakes up.
    #[must_use]
    pub fn poll_timeout(&self) -> Option<Timestamp> {
        let world = &self.world;
        let compress = world
            .panes
            .iter()
            .filter_map(|(_, pane)| match pane.compression {
                HistoryCompression::Wanted { since } => {
                    Some(since.max(world.last_input).saturating_add(COMPRESS_AFTER))
                }
                HistoryCompression::Done => None,
            })
            .min();
        let frame = self
            .world
            .clients
            .iter()
            .filter_map(|(_, client)| match client {
                Client::Attached(attached)
                    if attached.due == FrameDue::Due && attached.inflight.is_none() =>
                {
                    Some(match attached.pace {
                        Pace::Echo => Timestamp::ZERO,
                        Pace::Interval => attached
                            .last_frame
                            .map_or(Timestamp::ZERO, |last| last.saturating_add(FRAME_INTERVAL)),
                    })
                }
                Client::Attached(_) | Client::Connected => None,
            })
            .min();
        let autoscroll = world
            .clients
            .iter()
            .filter_map(|(_, client)| match client {
                Client::Attached(Attached {
                    grab:
                        Grab::Selecting {
                            scroll: Autoscrolling::Next(next),
                            ..
                        },
                    ..
                }) => Some(*next),
                Client::Attached(_) | Client::Connected => None,
            })
            .min();
        frame.into_iter().chain(compress).chain(autoscroll).min()
    }

    #[must_use]
    pub const fn structure(&self) -> &Structure {
        &self.world.structure
    }

    #[must_use]
    pub const fn size(&self) -> GridSize {
        self.world.size
    }

    /// A pane's screen as of its last sync: what clients are sent, for
    /// tests and the simulation to compare clients against. `None` for a
    /// pane the session does not have.
    #[cfg(any(test, feature = "testkit"))]
    #[must_use]
    pub fn pane_screen_or_none(&self, pane: PaneId) -> Option<&plx_emulator::Screen> {
        self.world
            .panes
            .get_or_none(pane)
            .map(|pane| pane.emulator.screen())
    }

    /// `pane`'s progress, as its program last reported it, for the
    /// simulation to compare clients' terminals against. `None` for a pane
    /// the session does not have.
    #[cfg(any(test, feature = "testkit"))]
    #[must_use]
    pub fn pane_progress_or_none(&self, pane: PaneId) -> Option<Progress> {
        self.world
            .panes
            .get_or_none(pane)
            .map(|pane| pane.progress.progress)
    }

    /// How many clients are connected, attached or not (for leak checks).
    /// Whether `pane`'s emulator still waits for the size its PTY has.
    #[cfg(any(test, feature = "testkit"))]
    #[must_use]
    pub fn pane_reflow_pending_or_none(&self, pane: PaneId) -> Option<bool> {
        self.world
            .panes
            .get_or_none(pane)
            .map(|pane| pane.reflow == Reflow::Pending)
    }

    #[cfg(any(test, feature = "testkit"))]
    #[must_use]
    pub const fn client_count(&self) -> usize {
        self.world.clients.len()
    }
}

impl<F: EmulatorFactory> core::fmt::Debug for Session<F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Session")
            .field("size", &self.world.size)
            .field("structure", &self.world.structure)
            .field("clients", &self.world.clients.len())
            .finish_non_exhaustive()
    }
}

impl<F: EmulatorFactory> World<F> {
    fn emit(&mut self, fx: &mut impl Effects, event: Event<'_>) {
        fx.event(self.events, event);
        self.events = self.events.next();
    }

    fn area(&self) -> Rect {
        pane_area(self.size)
    }

    fn message(
        &mut self,
        keymap: &Keymap,
        now: Timestamp,
        client: ClientId,
        message: ClientMessage<'_>,
        fx: &mut impl Effects,
    ) {
        let state = self
            .clients
            .get_mut_or_none(client)
            .or_bug("messages come from connected clients");
        let attached = match (state, message) {
            (state @ Client::Connected, ClientMessage::Attach { size, cell_px }) => {
                *state = Client::Attached(Attached {
                    size,
                    mode: keymap.base(),
                    last_active: now,
                    acked: IdMap::new(),
                    inflight: None,
                    inflight_panes: Vec::new(),
                    next_seq: FrameSeq::FIRST,
                    due: FrameDue::Due,
                    pace: Pace::Interval,
                    last_frame: None,
                    grab: Grab::None,
                    progress: Told::Nothing,
                });
                if cell_px.is_some() {
                    self.cell_px = cell_px;
                }
                self.emit(fx, Event::ClientAttached { client });
                self.fit_to_clients(fx);
                return;
            }
            // A connected client (the CLI) may run session commands without
            // attaching; it hears back either way.
            (Client::Connected, ClientMessage::Command(command @ Command::Session(_))) => {
                if self.run(client, command, fx) == Applied::Yes && self.state == State::Running {
                    fx.send::<F::Emulator>(client, Outgoing::Applied);
                }
                return;
            }
            (Client::Connected, _) => {
                fx.send::<F::Emulator>(client, Outgoing::Rejected(Rejection::NotAttached));
                return;
            }
            (Client::Attached(_), ClientMessage::Attach { .. }) => {
                fx.send::<F::Emulator>(client, Outgoing::Rejected(Rejection::AlreadyAttached));
                return;
            }
            (Client::Attached(attached), _) => attached,
        };
        match message {
            ClientMessage::Attach { .. } => bug!("attaching was handled above"),
            ClientMessage::Resize { size, cell_px } => {
                attached.size = size;
                attached.last_active = now;
                if cell_px.is_some() {
                    self.cell_px = cell_px;
                }
                self.fit_to_clients(fx);
            }
            ClientMessage::Key(event) => {
                attached.last_active = now;
                self.last_input = now;
                let mode = attached.mode;
                self.key(keymap, now, client, mode, event, fx);
            }
            ClientMessage::Paste(paste) => {
                attached.last_active = now;
                self.last_input = now;
                let focus = self.structure.active().focus();
                self.scroll_to_bottom(focus);
                if self.selected == Some(focus) {
                    self.clear_selection(focus);
                }
                let pane = self.panes.get_mut(focus);
                self.bytes.clear();
                pane.emulator.encode_paste(paste.text(), &mut self.bytes);
                pane.echo = Echo::Awaited(now);
                fx.write_pty(focus, &self.bytes);
            }
            ClientMessage::Command(command) => {
                // An attached client sees the outcome in its frames.
                let _seen_in_frames: Applied = self.run(client, command, fx);
            }
            ClientMessage::Mouse(mouse) => {
                attached.last_active = now;
                self.last_input = now;
                let grab = attached.grab;
                self.mouse(client, grab, mouse, now, fx);
            }
            ClientMessage::Ack(seq) => match attached.inflight {
                Some((inflight, version)) if inflight == seq => {
                    // The client keeps exactly the panes that frame showed
                    // (it drops the others' mirrors); a pane shown again
                    // later is sent whole.
                    let shown = &attached.inflight_panes;
                    attached.acked.retain(|pane, _| shown.contains(&pane));
                    for &pane in shown {
                        attached.acked.upsert(pane, version);
                    }
                    attached.inflight = None;
                }
                expected => fx.send::<F::Emulator>(
                    client,
                    Outgoing::Rejected(Rejection::UnexpectedAck {
                        got: seq,
                        expected: expected.map(|(seq, _)| seq),
                    }),
                ),
            },
        }
    }

    fn key(
        &mut self,
        keymap: &Keymap,
        now: Timestamp,
        client: ClientId,
        mode: Mode,
        event: KeyEvent,
        fx: &mut impl Effects,
    ) {
        match keymap.resolve(mode, &event) {
            Resolution::Run { command, next } => {
                self.set_mode(client, next, fx);
                self.run(client, command, fx);
            }
            Resolution::Switch(next) => self.set_mode(client, next, fx),
            Resolution::PassThrough => {
                let focus = self.structure.active().focus();
                self.scroll_to_bottom(focus);
                if self.selected == Some(focus) {
                    self.clear_selection(focus);
                }
                let pane = self.panes.get_mut(focus);
                self.bytes.clear();
                pane.emulator.encode_key(&event, &mut self.bytes);
                if !self.bytes.is_empty() {
                    pane.echo = Echo::Awaited(now);
                    fx.write_pty(focus, &self.bytes);
                }
            }
            Resolution::Ignore => {}
            Resolution::Input => {
                if self.search.needle.type_key(&event) == Typed::Changed {
                    self.search_again();
                }
            }
        }
    }

    fn set_mode(&mut self, client: ClientId, mode: Mode, fx: &mut impl Effects) {
        let Some(Client::Attached(attached)) = self.clients.get_mut_or_none(client) else {
            return;
        };
        if attached.mode != mode {
            let left = attached.mode;
            attached.mode = mode;
            attached.due = FrameDue::Due;
            self.emit(fx, Event::ModeChanged { client, mode });
            let focus = self.structure.active().focus();
            match (left.searching(), mode.searching()) {
                (Searching::No, Searching::Yes) => self.start_search(focus),
                (Searching::Yes, Searching::No) => self.stop_search(),
                (Searching::No, Searching::No) | (Searching::Yes, Searching::Yes) => {}
            }
            // Leaving scroll mode returns the view to the live screen, as
            // Zellij does.
            if left.scrolling() == Scrolling::Yes && mode.scrolling() == Scrolling::No {
                self.scroll_to_bottom(focus);
            }
        }
    }

    /// A search of `pane` starts, with no needle yet.
    fn start_search(&mut self, pane: PaneId) {
        self.stop_search();
        self.search.pane = Some(pane);
    }

    /// The search ends: its marks go.
    fn stop_search(&mut self) {
        self.search.needle.clear();
        if self.search.pane.is_some() {
            self.search_again();
        }
        self.search.pane = None;
    }

    /// Searches the searched pane again: for a new needle or options.
    fn search_again(&mut self) {
        let Some(entry) = self
            .search
            .pane
            .and_then(|pane| self.panes.get_mut_or_none(pane))
        else {
            return;
        };
        entry
            .emulator
            .search(self.search.needle.as_str(), self.search.options);
        entry.touched(self.now);
        entry.sync = SyncNeed::Dirty;
        self.all_due();
    }

    /// Runs `command` on the search going on, if any.
    fn search_command(&mut self, command: SearchCommand) -> Applied {
        let Some(entry) = self
            .search
            .pane
            .and_then(|pane| self.panes.get_mut_or_none(pane))
        else {
            return Applied::No;
        };
        match command {
            SearchCommand::Select(direction) => {
                entry.emulator.search_select(direction);
                entry.touched(self.now);
                entry.sync = SyncNeed::Dirty;
                self.all_due();
            }
            SearchCommand::ToggleCase => {
                self.search.options = self.search.options.toggled_case();
                self.search_again();
            }
            SearchCommand::ToggleWholeWord => {
                self.search.options = self.search.options.toggled_words();
                self.search_again();
            }
        }
        Applied::Yes
    }

    fn run(&mut self, client: ClientId, command: &Command, fx: &mut impl Effects) -> Applied {
        match command {
            Command::Session(command) => {
                let area = self.area();
                let Self {
                    structure, events, ..
                } = self;
                let mut split = None;
                let applied = structure.apply(command, area, |event| {
                    if let StructureEvent::PaneOpened { pane, beside, .. } = event {
                        split = Some((*pane, *beside));
                    }
                    fx.event(*events, Event::Structure(event));
                    *events = events.next();
                });
                match applied {
                    Ok(Lifecycle::Running) => {
                        self.splitting = split;
                        self.structure_changed(fx);
                        self.splitting = None;
                        Applied::Yes
                    }
                    Ok(Lifecycle::Ended(reason)) => {
                        self.end(reason, fx);
                        Applied::Yes
                    }
                    Err(rejection) => {
                        fx.send::<F::Emulator>(client, Outgoing::Rejected(rejection));
                        Applied::No
                    }
                }
            }
            Command::Scroll(by) => {
                let focus = self.structure.active().focus();
                self.scroll(focus, *by);
                Applied::Yes
            }
            Command::Search(command) => self.search_command(*command),
            Command::EditScrollback => self.edit_scrollback(fx),
            Command::Client(ClientCommand::Detach) => {
                fx.send::<F::Emulator>(client, Outgoing::Detached);
                fx.close_client(client);
                self.clients.remove(client);
                self.emit(fx, Event::ClientDetached { client });
                self.fit_to_clients(fx);
                Applied::Yes
            }
        }
    }

    /// Opens the focused pane's history in the user's editor, in the pane's
    /// place: the pane runs on hidden and comes back when the editor exits
    /// (or its pane is closed), as in Zellij.
    fn edit_scrollback(&mut self, fx: &mut impl Effects) -> Applied {
        let focus = self.structure.active().focus();
        self.history.clear();
        if let Some(entry) = self.panes.get_mut_or_none(focus) {
            entry.emulator.history_text(&mut self.history);
            entry.touched(self.now);
        }
        let Self {
            structure, events, ..
        } = self;
        let cover = structure.cover_focus(|event| {
            fx.event(*events, Event::Structure(event));
            *events = events.next();
        });
        self.opening_editor = Some(cover);
        self.structure_changed(fx);
        self.opening_editor = None;
        // A history can be large: its buffer is not kept.
        self.history = Vec::new();
        Applied::Yes
    }

    /// A mouse report from `client`, which holds `grab`.
    fn mouse(
        &mut self,
        client: ClientId,
        grab: Grab,
        mouse: MouseEvent,
        now: Timestamp,
        fx: &mut impl Effects,
    ) {
        let at = mouse.position;
        let next = match (mouse.action, mouse.button.wheel(), grab) {
            (_, Wheel::Up | Wheel::Down | Wheel::Sideways, _) => {
                if let Some((pane, rect)) = self.pane_at(at) {
                    self.wheel(pane, rect, mouse, fx);
                }
                grab
            }
            (MouseAction::Press, Wheel::No, _) => self.press(client, mouse, now, fx),
            (
                MouseAction::Motion | MouseAction::Release,
                Wheel::No,
                Grab::Selecting { pane, scroll },
            ) => {
                // Motion with no button held: the button was let go where
                // the terminal did not report it, outside its window.
                let released =
                    mouse.action == MouseAction::Release || mouse.button == MouseButton::None;
                if released {
                    let _over: Autoscroll = self.select(pane, SelectGesture::Release, at, now);
                    self.copy(client, pane, fx);
                    Grab::None
                } else {
                    let autoscroll = self.select(pane, SelectGesture::Drag, at, now);
                    Grab::Selecting {
                        pane,
                        scroll: scroll.after(autoscroll, now),
                    }
                }
            }
            (MouseAction::Motion, Wheel::No, Grab::Split(split)) => {
                let axis_position = self
                    .arrangement
                    .separators
                    .iter()
                    .find(|separator| separator.split == split)
                    .map(|separator| match separator.axis {
                        Axis::Horizontal => at.x,
                        Axis::Vertical => at.y,
                    });
                let area = self.area();
                let ratio = axis_position.and_then(|position| {
                    self.structure
                        .active()
                        .layout()
                        .ratio_at_or_none(split, position, area)
                });
                if let Some(ratio) = ratio {
                    let command = Command::Session(SessionCommand::MoveSplit { split, ratio });
                    self.run(client, &command, fx);
                }
                grab
            }
            (MouseAction::Motion | MouseAction::Release, Wheel::No, Grab::Pane(pane)) => {
                if let Some(rect) = self.arrangement.shown_rect_or_none(pane) {
                    self.forward_mouse(pane, rect, mouse, fx);
                }
                match mouse.action {
                    MouseAction::Release => Grab::None,
                    MouseAction::Press | MouseAction::Motion => grab,
                }
            }
            (MouseAction::Release, Wheel::No, Grab::Split(_) | Grab::None) => Grab::None,
            // Motion with nothing held: for a program that asked for every
            // movement.
            (MouseAction::Motion, Wheel::No, Grab::None) => {
                if let Some((pane, rect)) = self.pane_at(at) {
                    self.forward_mouse(pane, rect, mouse, fx);
                }
                Grab::None
            }
        };
        if let Some(Client::Attached(attached)) = self.clients.get_mut_or_none(client) {
            attached.grab = next;
        }
    }

    /// A button pressed: on a separator it starts a drag; on a pane it
    /// focuses the pane, and a program that tracks the mouse gets the press.
    fn press(
        &mut self,
        client: ClientId,
        mouse: MouseEvent,
        now: Timestamp,
        fx: &mut impl Effects,
    ) -> Grab {
        let at = mouse.position;
        if mouse.button == MouseButton::Left
            && let Some(separator) = self
                .arrangement
                .separators
                .iter()
                .find(|separator| contains(separator.rect, at))
        {
            return Grab::Split(separator.split);
        }
        let Some((pane, rect)) = self.pane_at(at) else {
            return Grab::None;
        };
        let command = Command::Session(SessionCommand::FocusPane { pane });
        self.run(client, &command, fx);
        let tracking = self
            .panes
            .get_or_none(pane)
            .map(|entry| entry.emulator.mouse_tracking());
        match tracking {
            Some(MouseTracking::On) => {
                self.forward_mouse(pane, rect, mouse, fx);
                Grab::Pane(pane)
            }
            // The left button selects text, as Zellij does.
            Some(MouseTracking::Off) if mouse.button == MouseButton::Left => {
                if let Some(old) = self.selected.filter(|&old| old != pane) {
                    self.clear_selection(old);
                }
                self.selected = Some(pane);
                let _pressed: Autoscroll = self.select(pane, SelectGesture::Press, at, now);
                Grab::Selecting {
                    pane,
                    scroll: Autoscrolling::No,
                }
            }
            Some(MouseTracking::Off) | None => Grab::None,
        }
    }

    /// A step of selecting text in `pane`, at the session cell `at`: says
    /// whether a drag past the pane's edge scrolls its view.
    fn select(
        &mut self,
        pane: PaneId,
        gesture: SelectGesture,
        at: Position,
        now: Timestamp,
    ) -> Autoscroll {
        let Some(rect) = self.arrangement.shown_rect_or_none(pane) else {
            return Autoscroll::Stopped;
        };
        let at = select_at(rect, self.size, at);
        let Some(entry) = self.panes.get_mut_or_none(pane) else {
            return Autoscroll::Stopped;
        };
        let autoscroll = entry.emulator.select(gesture, at, now.as_nanos());
        entry.sync = SyncNeed::Dirty;
        self.all_due();
        autoscroll
    }

    /// Scrolls the selections dragged past their pane's edge: a row for
    /// each [`AUTOSCROLL_INTERVAL`] gone by (a late step catches up a few),
    /// each extending its selection to the row then under the pointer.
    fn autoscroll(&mut self, now: Timestamp) {
        let Self {
            clients,
            panes,
            arrangement,
            ..
        } = self;
        let mut scrolled = false;
        for (_, client) in clients.iter_mut() {
            let Client::Attached(attached) = client else {
                continue;
            };
            let Grab::Selecting {
                pane,
                scroll: Autoscrolling::Next(mut next),
            } = attached.grab
            else {
                continue;
            };
            if now < next {
                continue;
            }
            // A pane closed or hidden mid-drag scrolls no more.
            let shown = arrangement.shown_rect_or_none(pane).is_some();
            let mut scroll = Autoscrolling::No;
            if let Some(entry) = panes.get_mut_or_none(pane).filter(|_| shown) {
                scroll = Autoscrolling::Next(next);
                let mut ticks = 0;
                while next <= now && ticks < MAX_AUTOSCROLL_CATCH_UP {
                    ticks += 1;
                    next = next.saturating_add(AUTOSCROLL_INTERVAL);
                    if entry.emulator.autoscroll_selection() == Autoscroll::Stopped {
                        scroll = Autoscrolling::No;
                        break;
                    }
                }
                if scroll != Autoscrolling::No {
                    // Too far behind: go on from now rather than burst.
                    scroll = Autoscrolling::Next(if next <= now {
                        now.saturating_add(AUTOSCROLL_INTERVAL)
                    } else {
                        next
                    });
                }
                entry.touched(now);
                entry.sync = SyncNeed::Dirty;
                scrolled = true;
            }
            attached.grab = Grab::Selecting { pane, scroll };
        }
        if scrolled {
            self.all_due();
        }
    }

    /// Sends `pane`'s selected text to `client`, for its clipboard.
    fn copy(&mut self, client: ClientId, pane: PaneId, fx: &mut impl Effects) {
        let Some(entry) = self.panes.get_mut_or_none(pane) else {
            return;
        };
        self.copied.clear();
        if entry.emulator.selection_text(&mut self.copied) == Selection::Some
            && !self.copied.is_empty()
        {
            fx.send::<F::Emulator>(client, Outgoing::Clipboard(Copied::new(&self.copied)));
        }
    }

    fn clear_selection(&mut self, pane: PaneId) {
        if let Some(entry) = self.panes.get_mut_or_none(pane) {
            entry.emulator.clear_selection();
            entry.sync = SyncNeed::Dirty;
            self.all_due();
        }
        if self.selected == Some(pane) {
            self.selected = None;
        }
    }

    /// A wheel notch over `pane`: the program's if it tracks the mouse; in
    /// a full-screen program that does not, arrow keys (as Zellij sends);
    /// otherwise it scrolls the pane's history.
    fn wheel(&mut self, pane: PaneId, rect: Rect, mouse: MouseEvent, fx: &mut impl Effects) {
        let Some(entry) = self.panes.get_or_none(pane) else {
            return;
        };
        if entry.emulator.mouse_tracking() == MouseTracking::On {
            self.forward_mouse(pane, rect, mouse, fx);
            return;
        }
        let up = match mouse.button.wheel() {
            Wheel::Up => Direction::Up,
            Wheel::Down => Direction::Down,
            Wheel::Sideways | Wheel::No => return,
        };
        match entry.emulator.active_screen() {
            ActiveScreen::Alternate => {
                let key = match up {
                    Direction::Up => Key::Up,
                    Direction::Down | Direction::Left | Direction::Right => Key::Down,
                };
                let entry = self.panes.get_mut(pane);
                self.bytes.clear();
                for _ in 0..WHEEL_LINES {
                    entry
                        .emulator
                        .encode_key(&KeyEvent::press(key, Mods::empty()), &mut self.bytes);
                }
                fx.write_pty(pane, &self.bytes);
            }
            ActiveScreen::Primary => {
                let scroll = match up {
                    Direction::Up => Scroll::Up(WHEEL_LINES),
                    Direction::Down | Direction::Left | Direction::Right => {
                        Scroll::Down(WHEEL_LINES)
                    }
                };
                self.scroll_view(pane, scroll);
            }
        }
    }

    /// Sends `mouse` to `pane`'s program, placed relative to `rect`, if its
    /// mouse modes take it.
    fn forward_mouse(
        &mut self,
        pane: PaneId,
        rect: Rect,
        mouse: MouseEvent,
        fx: &mut impl Effects,
    ) {
        let Some(entry) = self.panes.get_mut_or_none(pane) else {
            return;
        };
        let relative = MouseEvent {
            position: Position {
                x: mouse.position.x.saturating_sub(rect.x()),
                y: mouse.position.y.saturating_sub(rect.y()),
            },
            ..mouse
        };
        self.bytes.clear();
        entry.emulator.encode_mouse(&relative, &mut self.bytes);
        if !self.bytes.is_empty() {
            fx.write_pty(pane, &self.bytes);
        }
    }

    /// The shown pane under `at`, with where it is.
    fn pane_at(&self, at: Position) -> Option<(PaneId, Rect)> {
        self.arrangement.panes.iter().find_map(|placement| {
            placement
                .rect
                .filter(|rect| contains(*rect, at))
                .map(|rect| (placement.pane, rect))
        })
    }

    /// Moves `pane`'s view of its history by `by`.
    fn scroll(&mut self, pane: PaneId, by: ScrollBy) {
        let Some(entry) = self.panes.get_mut_or_none(pane) else {
            return;
        };
        let rows = entry.size.rows().get();
        let half = (rows / 2).max(1);
        let scroll = match by {
            ScrollBy::LineUp => Scroll::Up(1),
            ScrollBy::LineDown => Scroll::Down(1),
            ScrollBy::HalfPageUp => Scroll::Up(half),
            ScrollBy::HalfPageDown => Scroll::Down(half),
            ScrollBy::PageUp => Scroll::Up(rows),
            ScrollBy::PageDown => Scroll::Down(rows),
            ScrollBy::Top => Scroll::Top,
            ScrollBy::Bottom => Scroll::Bottom,
            ScrollBy::PreviousPrompt => Scroll::Prompts(-1),
            ScrollBy::NextPrompt => Scroll::Prompts(1),
        };
        self.scroll_view(pane, scroll);
    }

    fn scroll_view(&mut self, pane: PaneId, scroll: Scroll) {
        let Some(entry) = self.panes.get_mut_or_none(pane) else {
            return;
        };
        entry.emulator.scroll(scroll);
        entry.touched(self.now);
        entry.sync = SyncNeed::Dirty;
        self.all_due();
    }

    /// Back to the live screen, if `pane` is back in its history: when the
    /// user types into it, or leaves scroll mode.
    fn scroll_to_bottom(&mut self, pane: PaneId) {
        let back = self
            .panes
            .get_or_none(pane)
            .map_or(0, |entry| entry.emulator.scroll_position().back);
        if back > 0 {
            self.scroll(pane, ScrollBy::Bottom);
        }
    }

    fn client_gone(&mut self, client: ClientId, fx: &mut impl Effects) {
        if let Client::Attached(_) = self.clients.remove(client) {
            self.emit(fx, Event::ClientDetached { client });
            self.fit_to_clients(fx);
        }
    }

    fn spawned(&mut self, pane: PaneId, pid: Pid, fx: &mut impl Effects) {
        let entry = self.panes.get_mut(pane);
        match entry.process {
            Process::Spawning => {
                entry.process = Process::Running(pid);
                self.emit(fx, Event::PaneStarted { pane, pid });
            }
            // Closed before it started; its exit is reported next.
            Process::Closing => {}
            Process::Running(_) => bug!("a pane's program starts once"),
        }
    }

    /// The pane's program is gone (exited, or never started).
    fn process_ended(&mut self, pane: PaneId, fx: &mut impl Effects) {
        let ended = self.panes.remove(pane);
        for (_, client) in self.clients.iter_mut() {
            if let Client::Attached(attached) = client {
                attached.acked.remove_or_none(pane);
            }
        }
        if ended.process != Process::Closing {
            self.pane_gone(pane, fx);
        }
    }

    /// `pane`, still in the structure, is gone: close it there.
    fn pane_gone(&mut self, pane: PaneId, fx: &mut impl Effects) {
        let Self {
            structure, events, ..
        } = self;
        let lifecycle = structure.pane_exited(pane, |event| {
            fx.event(*events, Event::Structure(event));
            *events = events.next();
        });
        match lifecycle {
            Lifecycle::Running => self.structure_changed(fx),
            Lifecycle::Ended(reason) => self.end(reason, fx),
        }
    }

    fn output(&mut self, now: Timestamp, pane: PaneId, bytes: &[u8], fx: &mut impl Effects) {
        let entry = self.panes.get_mut(pane);
        if entry.process == Process::Closing {
            return;
        }
        let echo = match entry.echo {
            Echo::Awaited(at) if now <= at.saturating_add(ECHO_WINDOW) => Echo::Awaited(at),
            Echo::Awaited(_) | Echo::None => Echo::None,
        };
        entry.echo = Echo::None;
        if let Echo::Awaited(_) = echo {
            entry.echoed = Echoed::Yes;
        }
        // Output written for the new size is parsed at the new size.
        if entry.reflow(pane, self.cell_px, now, fx).is_err() {
            self.emulator_failed(pane, fx);
            return;
        }
        let entry = self.panes.get_mut(pane);
        entry.touched(now);
        let events = entry.emulator.feed(bytes);
        if !events.replies().is_empty() {
            fx.write_pty(pane, events.replies());
        }
        // The program put text on the clipboard: the client the user is at
        // (who used one last) sets its terminal's clipboard.
        if let Some(text) = events.clipboard_or_none()
            && let Some(client) = most_recently_active_or_none(&self.clients)
        {
            fx.send::<F::Emulator>(client, Outgoing::Clipboard(Copied::new(text)));
        }
        let bell = events.bell();
        let progress = events.progress_or_none();
        entry.sync = SyncNeed::Dirty;
        if let Some(progress) = progress {
            entry.progress = Reported {
                progress,
                reports: entry.progress.reports + 1,
            };
            // The clients' terminals show the focused pane's progress.
            if self.structure.active().focus() == pane {
                self.all_due();
            }
        }
        if bell == plx_emulator::Bell::Rang {
            self.emit(fx, Event::Bell { pane });
        }
        let visible = self
            .arrangement
            .panes
            .iter()
            .any(|placement| placement.pane == pane && placement.rect.is_some());
        if visible {
            self.all_due();
            if let Echo::Awaited(_) = echo {
                for (_, client) in self.clients.iter_mut() {
                    if let Client::Attached(attached) = client {
                        attached.pace = Pace::Echo;
                    }
                }
            }
        }
    }

    /// One step of compressing a quiet pane's history, when every client
    /// has been quiet too: one pane, one bounded step, per session step;
    /// [`Session::poll_timeout`] brings the next.
    fn compress_idle(&mut self, now: Timestamp) {
        if now < self.last_input.saturating_add(COMPRESS_AFTER) {
            return;
        }
        let due = self.panes.iter_mut().find(|(_, pane)| {
            pane.process != Process::Closing
                && matches!(
                    pane.compression,
                    HistoryCompression::Wanted { since } if now >= since.saturating_add(COMPRESS_AFTER)
                )
        });
        if let Some((_, pane)) = due
            && pane.emulator.compress() == Compression::Done
        {
            pane.compression = HistoryCompression::Done;
        }
    }

    fn structure_changed(&mut self, fx: &mut impl Effects) {
        self.reconcile(fx);
        self.all_due();
    }

    fn all_due(&mut self) {
        for (_, client) in self.clients.iter_mut() {
            if let Client::Attached(attached) = client {
                attached.due = FrameDue::Due;
            }
        }
    }

    /// The session size follows the attached clients (by policy).
    fn fit_to_clients(&mut self, fx: &mut impl Effects) {
        self.sizes.clear();
        for (client, state) in self.clients.iter() {
            if let Client::Attached(attached) = state {
                self.sizes.push(ClientSize {
                    client,
                    size: attached.size,
                    last_active: attached.last_active,
                });
            }
        }
        if let Some(size) = self.policy.effective(&self.sizes)
            && size != self.size
        {
            self.size = size;
            self.emit(fx, Event::SessionResized { size });
            self.reconcile(fx);
        }
        self.all_due();
    }

    /// Brings the panes in line with the structure: closes panes it no longer
    /// holds, creates and starts panes it gained, and sizes every pane to its
    /// place in its tab (hidden panes keep the pane area's size).
    fn reconcile(&mut self, fx: &mut impl Effects) {
        let area = self.area();
        let Self {
            factory,
            structure,
            panes,
            scratch,
            arrangement,
            failed,
            opening_editor,
            splitting,
            history,
            ..
        } = self;
        for (id, pane) in panes.iter_mut() {
            if pane.process != Process::Closing && structure.tab_holding_or_none(id).is_none() {
                fx.close_pty(id);
                pane.process = Process::Closing;
            }
        }
        let area_size = area.size();
        for tab in structure.tabs() {
            tab.arrange_into(area, scratch);
            for placement in &scratch.panes {
                let size = placement.rect.map_or(area_size, Rect::size);
                let id = placement.pane;
                match panes.get_mut_or_none(id) {
                    None => match factory.create(size) {
                        Ok(emulator) => {
                            panes.insert(
                                id,
                                Pane {
                                    emulator,
                                    process: Process::Spawning,
                                    size,
                                    sync: SyncNeed::Dirty,
                                    echo: Echo::None,
                                    echoed: Echoed::No,
                                    reflow: Reflow::Done,
                                    compression: HistoryCompression::Done,
                                    progress: Reported::NONE,
                                },
                            );
                            let program = match (*opening_editor, *splitting) {
                                (Some(editor), _) if editor == id => {
                                    PaneProgram::Editor(History::new(history))
                                }
                                (_, Some((opened, beside))) if opened == id => PaneProgram::Shell {
                                    start_in: StartIn::PaneOf(beside),
                                },
                                (_, _) => PaneProgram::Shell {
                                    start_in: StartIn::Default,
                                },
                            };
                            fx.spawn_pty(id, size, program);
                        }
                        Err(_limit) => failed.push(id),
                    },
                    Some(pane) if pane.size != size && pane.process != Process::Closing => {
                        fx.resize_pty(id, size);
                        pane.size = size;
                        pane.reflow = Reflow::Pending;
                        pane.sync = SyncNeed::Dirty;
                    }
                    Some(_) => {}
                }
            }
        }
        structure.active().arrange_into(area, arrangement);
        while let Some(pane) = self.failed.pop() {
            // An emulator libghostty cannot create or resize (an internal
            // limit) ends its pane, as if its program had exited.
            if let Some(entry) = self.panes.get_mut_or_none(pane) {
                fx.close_pty(pane);
                entry.process = Process::Closing;
            }
            if self.structure.tab_holding_or_none(pane).is_some() {
                self.pane_gone(pane, fx);
            }
        }
    }

    /// Syncs what the clients will see and sends every frame that is due and
    /// allowed.
    fn flush(&mut self, now: Timestamp, fx: &mut impl Effects) {
        let ready = |attached: &Attached| {
            attached.due == FrameDue::Due
                && attached.inflight.is_none()
                && (attached.pace == Pace::Echo
                    || attached
                        .last_frame
                        .is_none_or(|last| now >= last.saturating_add(FRAME_INTERVAL)))
        };
        let paced = |attached: &Attached| {
            attached
                .last_frame
                .is_none_or(|last| now >= last.saturating_add(FRAME_INTERVAL))
        };
        let any_ready = self.clients.iter().any(|(_, client)| match client {
            Client::Attached(attached) => ready(attached),
            Client::Connected => false,
        });
        if !any_ready {
            return;
        }
        // Frames that go out only for echo carry only the echo; any frame
        // that is due anyway carries everything. So does one after a
        // resize: a pane's rows must match its new size.
        let any_paced = self.clients.iter().any(|(_, client)| match client {
            Client::Attached(attached) => ready(attached) && paced(attached),
            Client::Connected => false,
        });
        let screens_fit = self.arrangement.panes.iter().all(|placement| {
            placement.rect.is_none_or(|rect| {
                self.panes
                    .get_or_none(placement.pane)
                    .is_none_or(|pane| pane.emulator.screen().size() == rect.size())
            })
        });
        let scope = if any_paced || !screens_fit {
            Scope::All
        } else {
            Scope::Echoed
        };
        self.sync_visible(scope, fx);
        // What an echo frame left out follows in the next paced frame.
        let after = if scope == Scope::Echoed && self.any_visible_dirty() {
            FrameDue::Due
        } else {
            FrameDue::Idle
        };
        let Self {
            structure,
            panes,
            clients,
            arrangement,
            search,
            size,
            version,
            ..
        } = self;
        for (client, state) in clients.iter_mut() {
            let Client::Attached(attached) = state else {
                continue;
            };
            if !ready(attached) {
                continue;
            }
            let seq = attached.next_seq;
            let focus = structure.active().focus();
            let (progress, told) = attached.progress.news(focus, panes.get(focus).progress);
            attached.progress = told;
            fx.send(
                client,
                Outgoing::Frame(FrameView {
                    seq,
                    size: *size,
                    mode: attached.mode,
                    structure,
                    arrangement,
                    panes,
                    search,
                    acked: &attached.acked,
                    progress,
                }),
            );
            attached.inflight = Some((seq, *version));
            attached.inflight_panes.clear();
            attached.inflight_panes.extend(
                arrangement
                    .panes
                    .iter()
                    .filter(|placement| placement.rect.is_some())
                    .map(|placement| placement.pane),
            );
            attached.next_seq = seq.next();
            attached.due = after;
            attached.pace = Pace::Interval;
            // Echo frames come on top of the paced ones: they do not move
            // the pacing clock, so typing never starves the other panes.
            if scope == Scope::All {
                attached.last_frame = Some(now);
            }
        }
    }

    /// Syncs the visible panes that changed, all with one new version.
    fn sync_visible(&mut self, scope: Scope, fx: &mut impl Effects) {
        let dirty = |panes: &IdMap<PaneId, Pane<F::Emulator>>, id: PaneId| {
            panes.get_or_none(id).is_some_and(|pane| {
                pane.sync == SyncNeed::Dirty
                    && pane.process != Process::Closing
                    && (scope == Scope::All || pane.echoed == Echoed::Yes)
            })
        };
        let any_dirty = self
            .arrangement
            .panes
            .iter()
            .any(|placement| placement.rect.is_some() && dirty(&self.panes, placement.pane));
        if !any_dirty {
            return;
        }
        self.version = self.version.next();
        for placement in &self.arrangement.panes {
            if placement.rect.is_none() || !dirty(&self.panes, placement.pane) {
                continue;
            }
            let pane = self.panes.get_mut(placement.pane);
            pane.echoed = Echoed::No;
            if pane
                .reflow(placement.pane, self.cell_px, self.now, fx)
                .is_err()
            {
                self.failed.push(placement.pane);
                continue;
            }
            match pane.emulator.sync(self.version) {
                Ok(_damage) => pane.sync = SyncNeed::Clean,
                Err(_limit) => self.failed.push(placement.pane),
            }
        }
        while let Some(pane) = self.failed.pop() {
            fx.close_pty(pane);
            self.panes.get_mut(pane).process = Process::Closing;
            self.pane_gone(pane, fx);
        }
    }

    /// An emulator libghostty cannot resize (an internal limit) ends its
    /// pane, as if its program had exited.
    fn emulator_failed(&mut self, pane: PaneId, fx: &mut impl Effects) {
        fx.close_pty(pane);
        self.panes.get_mut(pane).process = Process::Closing;
        if self.structure.tab_holding_or_none(pane).is_some() {
            self.pane_gone(pane, fx);
        }
    }

    /// Whether a shown pane has output its screen does not hold yet.
    fn any_visible_dirty(&self) -> bool {
        self.arrangement.panes.iter().any(|placement| {
            placement.rect.is_some()
                && self.panes.get_or_none(placement.pane).is_some_and(|pane| {
                    pane.sync == SyncNeed::Dirty && pane.process != Process::Closing
                })
        })
    }

    fn end(&mut self, reason: EndReason, fx: &mut impl Effects) {
        self.state = State::Ended;
        self.emit(fx, Event::SessionEnded { reason });
        for (client, _) in self.clients.iter() {
            fx.send::<F::Emulator>(client, Outgoing::SessionEnded(reason));
            fx.close_client(client);
        }
        for (pane, entry) in self.panes.iter() {
            if entry.process != Process::Closing {
                fx.close_pty(pane);
            }
        }
        fx.exit();
    }
}
