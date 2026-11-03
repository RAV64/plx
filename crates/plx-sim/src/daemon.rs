//! The reference daemon: the real session behind a reactor that models
//! plx-daemon's.
//!
//! One [`Daemon::iteration`] does what one reactor wake-up does, in its
//! order: every client's input first, then each pane's output up to a read
//! budget, then exits, then a due tick. Client input first is what keeps a
//! keystroke responsive while every pane floods.

use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU32, NonZeroUsize};

use plx_core::effects::{Effects, PaneProgram};
use plx_core::event::{Event, EventSeq, ExitStatus, Pid};
use plx_core::id::{ClientId, PaneId};
use plx_core::input::Input;
use plx_core::message::Outgoing;
use plx_core::mode::Keymap;
use plx_core::session::{FRAME_INTERVAL, Session};
use plx_core::size::SizePolicy;
use plx_core::time::Timestamp;
use plx_emulator::{Emulator, GridSize};
use plx_ghostty::{GhosttyFactory, GraphemeClustering, Options, Scrollback};
use plx_protocol::client::{Request, decode};
use plx_protocol::framing::{MAX_CLIENT_MESSAGE, split};
use plx_protocol::server::encode_outgoing;

use crate::oracle::Failure;
use crate::program::{Ending, Life, Program};
use crate::trace::{Tag, Trace};

/// The most a pane's output is read in one iteration.
pub const READ_BUDGET: usize = 4096;

/// libghostty's settings, the same for panes and outer terminals.
pub const OPTIONS: Options = Options {
    scrollback: Scrollback::Disabled,
    continuation_max_bytes: NonZeroUsize::MIN.saturating_add(4095),
    grapheme_clustering: GraphemeClustering::Enabled,
};

/// Whether an iteration did anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    Idle,
    Busy,
}

impl Activity {
    /// Busy if either was.
    #[must_use]
    pub const fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Idle, Self::Idle) => Self::Idle,
            (Self::Busy, _) | (_, Self::Busy) => Self::Busy,
        }
    }
}

/// Whether the session is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Over {
    No,
    Yes,
}

/// What the session asks of the world.
#[derive(Debug)]
struct Fx {
    programs: BTreeMap<PaneId, Program>,
    /// Programs started, not yet reported.
    spawned: Vec<PaneId>,
    /// The pid the next program gets.
    next_pid: NonZeroU32,
    ending: Ending,
    /// Bytes for each connected client, not yet sent.
    outboxes: BTreeMap<ClientId, Vec<u8>>,
    /// Clients the session closed: their input is dropped from now on.
    closed: BTreeSet<ClientId>,
    over: Over,
    trace: Trace,
    failure: Option<Failure>,
}

impl Effects for Fx {
    fn spawn_pty(&mut self, pane: PaneId, _: GridSize, _: PaneProgram<'_>) {
        self.programs
            .insert(pane, Program::for_pane(pane, self.ending));
        self.spawned.push(pane);
    }

    fn write_pty(&mut self, pane: PaneId, bytes: &[u8]) {
        self.trace.record(Tag::ToProgram, pane.get(), bytes);
        match self.programs.get_mut(&pane) {
            Some(program) => program.write(bytes),
            None => self.fail(format!("a write to pane {pane:?}, which has no program")),
        }
    }

    fn resize_pty(&mut self, pane: PaneId, _: GridSize) {
        match self.programs.get_mut(&pane) {
            Some(program) => program.resize(),
            None => self.fail(format!("a resize of pane {pane:?}, which has no program")),
        }
    }

    fn close_pty(&mut self, pane: PaneId) {
        match self.programs.get_mut(&pane) {
            Some(program) => program.exit(),
            None => self.fail(format!("a hang-up of pane {pane:?}, which has no program")),
        }
    }

    fn send<E: Emulator>(&mut self, client: ClientId, message: Outgoing<'_, E>) {
        let Some(outbox) = self.outboxes.get_mut(&client) else {
            return self.fail(format!(
                "a message to client {client:?}, which is not connected"
            ));
        };
        if let Err(error) = encode_outgoing(&message, outbox) {
            self.fail(format!("encoding a message to {client:?}: {error}"));
        }
    }

    fn close_client(&mut self, client: ClientId) {
        // The connection closes once its outbox is sent; the client ends on
        // the message before (detached, session ended).
        self.closed.insert(client);
    }

    fn event(&mut self, seq: EventSeq, event: Event<'_>) {
        self.trace
            .record(Tag::Event, seq.get(), format!("{event:?}").as_bytes());
    }

    fn exit(&mut self) {
        self.over = Over::Yes;
    }
}

impl Fx {
    fn fail(&mut self, failure: Failure) {
        if self.failure.is_none() {
            self.failure = Some(failure);
        }
    }
}

/// Steps the session, then reports at once every program the step started
/// (spawning is synchronous: its outcome comes before anything else about
/// the pane).
fn step(session: &mut Session<GhosttyFactory>, fx: &mut Fx, now: Timestamp, input: Input<'_>) {
    session.step(now, input, fx);
    while let Some(pane) = fx.spawned.pop() {
        if fx.over == Over::Yes {
            return;
        }
        let pid = Pid::new(fx.next_pid);
        fx.next_pid = fx.next_pid.saturating_add(1);
        session.step(now, Input::PtySpawned { pane, pid }, fx);
    }
}

/// The daemon.
#[derive(Debug)]
pub struct Daemon {
    session: Session<GhosttyFactory>,
    fx: Fx,
    /// Bytes each client sent that arrived, not yet decoded.
    inboxes: BTreeMap<ClientId, Vec<u8>>,
    /// Clients whose connection closed from their side.
    gone: Vec<ClientId>,
    now: Timestamp,
    /// A pane's output read this iteration.
    read: Vec<u8>,
}

impl Daemon {
    #[must_use]
    pub fn new(ending: Ending) -> Self {
        let mut fx = Fx {
            ending,
            programs: BTreeMap::new(),
            spawned: Vec::new(),
            next_pid: NonZeroU32::MIN.saturating_add(999),
            outboxes: BTreeMap::new(),
            closed: BTreeSet::new(),
            over: Over::No,
            trace: Trace::new(),
            failure: None,
        };
        let mut session = Session::new(
            GhosttyFactory { options: OPTIONS },
            Keymap::unlock_first(),
            SizePolicy::Latest,
            &mut fx,
        );
        // The first pane was started by `new`, before any step.
        while let Some(pane) = fx.spawned.pop() {
            let pid = Pid::new(fx.next_pid);
            fx.next_pid = fx.next_pid.saturating_add(1);
            session.step(Timestamp::ZERO, Input::PtySpawned { pane, pid }, &mut fx);
        }
        Self {
            session,
            fx,
            inboxes: BTreeMap::new(),
            gone: Vec::new(),
            now: Timestamp::ZERO,
            read: Vec::new(),
        }
    }

    /// A client connects.
    pub fn connect(&mut self) -> ClientId {
        let client = self.session.connect();
        self.inboxes.insert(client, Vec::new());
        self.fx.outboxes.insert(client, Vec::new());
        client
    }

    /// Bytes from `client` arrive (and are dropped once the session closed
    /// it).
    pub fn deliver(&mut self, client: ClientId, bytes: &[u8]) {
        self.fx.trace.record(Tag::ToDaemon, client.get(), bytes);
        if self.fx.closed.contains(&client) {
            return;
        }
        if let Some(inbox) = self.inboxes.get_mut(&client) {
            inbox.extend_from_slice(bytes);
        }
    }

    /// Up to `max` bytes the daemon sent `client` leave its outbox.
    pub fn send_to(&mut self, client: ClientId, max: usize) -> Vec<u8> {
        let Some(outbox) = self.fx.outboxes.get_mut(&client) else {
            return Vec::new();
        };
        let taken: Vec<u8> = outbox.drain(..max.min(outbox.len())).collect();
        self.fx.trace.record(Tag::ToClient, client.get(), &taken);
        taken
    }

    /// Bytes waiting for `client`.
    #[must_use]
    pub fn waiting_for(&self, client: ClientId) -> usize {
        self.fx.outboxes.get(&client).map_or(0, Vec::len)
    }

    /// `client` closed its connection. A client the session already
    /// closed is just forgotten: the session must not hear of it again.
    pub fn hang_up(&mut self, client: ClientId) {
        if self.fx.closed.contains(&client) {
            self.forget(client);
        } else {
            self.gone.push(client);
        }
    }

    /// `client` ended (it was detached or saw the session end) and its
    /// connection closed.
    pub fn forget(&mut self, client: ClientId) {
        self.inboxes.remove(&client);
        self.fx.outboxes.remove(&client);
        self.fx.closed.remove(&client);
    }

    #[must_use]
    pub const fn session(&self) -> &Session<GhosttyFactory> {
        &self.session
    }

    #[must_use]
    pub const fn over(&self) -> Over {
        self.fx.over
    }

    #[must_use]
    pub const fn trace(&self) -> Trace {
        self.fx.trace
    }

    /// The panes with a running program, in id order.
    #[must_use]
    pub fn programs(&self) -> Vec<PaneId> {
        self.fx
            .programs
            .iter()
            .filter(|(_, program)| program.life() == Life::Running)
            .map(|(&pane, _)| pane)
            .collect()
    }

    pub fn program_mut(&mut self, pane: PaneId) -> Option<&mut Program> {
        self.fx.programs.get_mut(&pane)
    }

    /// Bytes ever written to `pane`'s program.
    #[must_use]
    pub fn received(&self, pane: PaneId) -> usize {
        self.fx.programs.get(&pane).map_or(0, Program::received)
    }

    /// When the session next needs a tick; `None` when idle.
    #[must_use]
    pub fn poll_timeout(&self) -> Option<Timestamp> {
        self.session.poll_timeout()
    }

    /// Time passes: to the next deadline, or by one frame interval.
    pub fn advance_clock(&mut self) {
        self.now = match self.session.poll_timeout() {
            Some(deadline) if deadline > self.now => deadline,
            Some(_) | None => self.now.saturating_add(FRAME_INTERVAL),
        };
    }

    /// Whether an iteration has anything to do.
    #[must_use]
    pub fn busy(&self) -> bool {
        !self.fx.spawned.is_empty()
            || !self.gone.is_empty()
            || self.inboxes.values().any(|inbox| !inbox.is_empty())
            || self
                .fx
                .programs
                .values()
                .any(|program| program.has_output() || program.life() == Life::Exiting)
            || self
                .session
                .poll_timeout()
                .is_some_and(|deadline| deadline <= self.now)
    }

    /// One reactor iteration, phase by phase. It stops as soon as the
    /// session exits: nothing may step it after that.
    ///
    /// # Errors
    ///
    /// When a client sent something undecodable, or the session asked for
    /// something impossible.
    pub fn iteration(&mut self) -> Result<Activity, Failure> {
        let mut activity = self.read_clients()?;
        activity = activity.and(self.hang_ups());
        activity = activity.and(self.read_programs());
        activity = activity.and(self.report_exits());
        activity = activity.and(self.tick());
        self.fx.failure.take().map_or(Ok(activity), Err)
    }

    /// Client input first: every whole message from every client.
    fn read_clients(&mut self) -> Result<Activity, Failure> {
        let mut activity = Activity::Idle;
        let now = self.now;
        for (&client, inbox) in &mut self.inboxes {
            let mut used = 0;
            loop {
                // A client the session closed (even by one of these
                // messages) is read no further.
                if self.fx.over == Over::Yes || self.fx.closed.contains(&client) {
                    break;
                }
                let rest = inbox.get(used..).ok_or("used stays within the inbox")?;
                let Some((payload, length)) = split(rest, MAX_CLIENT_MESSAGE)
                    .map_err(|error| format!("framing from {client:?}: {error}"))?
                else {
                    break;
                };
                let wire = decode(payload)
                    .map_err(|error| format!("decoding from {client:?}: {error}"))?;
                let Request::Session(message) = wire.request() else {
                    return Err(format!("{client:?} subscribed; simulated clients never do"));
                };
                step(
                    &mut self.session,
                    &mut self.fx,
                    now,
                    Input::Client { client, message },
                );
                used += length;
                activity = Activity::Busy;
            }
            if self.fx.closed.contains(&client) {
                inbox.clear();
            } else {
                inbox.drain(..used);
            }
        }
        Ok(activity)
    }

    /// Connections clients closed.
    fn hang_ups(&mut self) -> Activity {
        let mut activity = Activity::Idle;
        for client in std::mem::take(&mut self.gone) {
            // Closed by the session meanwhile (its own input detached it):
            // the session must not hear of it again.
            if self.fx.over == Over::No && !self.fx.closed.contains(&client) {
                step(
                    &mut self.session,
                    &mut self.fx,
                    self.now,
                    Input::ClientGone { client },
                );
            }
            self.forget(client);
            activity = Activity::Busy;
        }
        activity
    }

    /// Each pane's output, up to the read budget.
    fn read_programs(&mut self) -> Activity {
        let mut activity = Activity::Idle;
        let panes: Vec<PaneId> = self.fx.programs.keys().copied().collect();
        for pane in panes {
            if self.fx.over == Over::Yes {
                break;
            }
            self.read.clear();
            if let Some(program) = self.fx.programs.get_mut(&pane) {
                program.read(READ_BUDGET, &mut self.read);
            }
            if !self.read.is_empty() {
                self.fx
                    .trace
                    .record(Tag::FromProgram, pane.get(), &self.read);
                step(
                    &mut self.session,
                    &mut self.fx,
                    self.now,
                    Input::PtyOutput {
                        pane,
                        bytes: &self.read,
                    },
                );
                activity = Activity::Busy;
            }
        }
        activity
    }

    /// Exits, reported once their output is read.
    fn report_exits(&mut self) -> Activity {
        let mut activity = Activity::Idle;
        let exiting: Vec<PaneId> = self
            .fx
            .programs
            .iter()
            .filter(|(_, program)| program.life() == Life::Exiting && !program.has_output())
            .map(|(&pane, _)| pane)
            .collect();
        for pane in exiting {
            if self.fx.over == Over::Yes {
                break;
            }
            self.fx.programs.remove(&pane);
            step(
                &mut self.session,
                &mut self.fx,
                self.now,
                Input::PtyExited {
                    pane,
                    status: ExitStatus::Code(0),
                },
            );
            activity = Activity::Busy;
        }
        activity
    }

    /// A due tick.
    fn tick(&mut self) -> Activity {
        if self.fx.over == Over::No
            && self
                .session
                .poll_timeout()
                .is_some_and(|deadline| deadline <= self.now)
        {
            step(&mut self.session, &mut self.fx, self.now, Input::Tick);
            Activity::Busy
        } else {
            Activity::Idle
        }
    }
}

impl Default for Daemon {
    fn default() -> Self {
        Self::new(Ending::Allowed)
    }
}
