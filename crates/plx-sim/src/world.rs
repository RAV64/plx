//! The simulated world: one daemon, up to [`MAX_CLIENTS`] clients, the
//! pipes between them and virtual time, advanced one schedule choice at a
//! time.
//!
//! A [`Choice`] picks among the actions possible right now (modulo their
//! number) and parameterises it, so every generated schedule is legal and
//! proptest shrinks interleavings as readily as values.

use plx_core::id::PaneId;
use plx_emulator::{GridSize, Key, KeyChar, KeyEvent, Mods};

use crate::client::Client;
use crate::daemon::{Activity, Daemon, Over};
use crate::oracle::{Failure, matches_session};
use crate::program::Ending;
use crate::trace::Trace;

/// The most clients attached at once.
pub const MAX_CLIENTS: usize = 2;

/// The most rounds [`World::settle`] runs before it calls the system stuck.
const SETTLE_ROUNDS: usize = 1000;

/// One step of a schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Choice {
    /// Which possible action, modulo how many there are.
    pub pick: u16,
    /// Its parameter: a chunk size, a key, a size, a program's output.
    pub arg: u32,
}

/// A client and the bytes it sent that are still in the pipe.
#[derive(Debug)]
struct Link {
    client: Client,
    to_daemon: Vec<u8>,
}

/// What can happen next.
#[derive(Clone, Copy, Debug)]
enum Action {
    /// Some of what a client sent reaches the daemon.
    ToDaemon(usize),
    /// Some of what the daemon sent reaches a client.
    ToClient(usize),
    Iterate,
    Clock,
    Produce(PaneId),
    Type(usize),
    Command(usize),
    Paste(usize),
    Resize(usize),
    Connect,
    HangUp(usize),
    Exit(PaneId),
    Settle,
}

/// What a run exercised, to check that schedules reach what they test.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Client deliveries that drew something (each checked by the oracle).
    pub draws: usize,
    /// Bytes clients wrote to their terminals.
    pub terminal_bytes: usize,
    pub keys: usize,
    pub commands: usize,
    /// Settles that reached quiescence (each checked against the daemon).
    pub quiet_checks: usize,
    pub most_panes: usize,
    pub most_clients: usize,
}

/// The simulated world.
#[derive(Debug)]
pub struct World {
    daemon: Daemon,
    links: Vec<Link>,
    trace: Trace,
    stats: Stats,
    ending: Ending,
}

/// A grid size, checked.
///
/// # Errors
///
/// When it is zero or too large.
pub fn size(cols: u16, rows: u16) -> Result<GridSize, Failure> {
    GridSize::new(
        cols.try_into().map_err(|_zero| "a size of zero columns")?,
        rows.try_into().map_err(|_zero| "a size of zero rows")?,
    )
    .map_err(|error| format!("{error}"))
}

const fn press(key: Key, mods: Mods) -> KeyEvent {
    KeyEvent::press(key, mods)
}

/// A plain key press.
#[must_use]
pub const fn letter(byte: u8) -> KeyEvent {
    press(Key::Char(KeyChar::from_byte(byte)), Mods::empty())
}

impl World {
    /// A daemon with one attached client.
    ///
    /// # Errors
    ///
    /// When the first client cannot be made.
    pub fn new(ending: Ending) -> Result<Self, Failure> {
        let mut world = Self {
            daemon: Daemon::new(ending),
            links: Vec::new(),
            trace: Trace::new(),
            stats: Stats::default(),
            ending,
        };
        world.connect(size(80, 24)?)?;
        Ok(world)
    }

    /// The session is over (its last pane closed, or it was killed).
    #[must_use]
    pub const fn over(&self) -> bool {
        matches!(self.daemon.over(), Over::Yes)
    }

    #[must_use]
    pub const fn clients(&self) -> usize {
        self.links.len()
    }

    #[must_use]
    pub fn panes(&self) -> usize {
        self.daemon.session().structure().panes().len()
    }

    #[must_use]
    pub const fn daemon(&self) -> &Daemon {
        &self.daemon
    }

    #[must_use]
    pub const fn stats(&self) -> Stats {
        self.stats
    }

    /// Everything that happened, as one fingerprint.
    #[must_use]
    pub const fn trace(&self) -> (Trace, Trace) {
        (self.daemon.trace(), self.trace)
    }

    /// Takes one step of a schedule.
    ///
    /// # Errors
    ///
    /// When a check failed or the system broke.
    pub fn step(&mut self, choice: Choice) -> Result<(), Failure> {
        if self.over() {
            return Ok(());
        }
        let actions = self.actions();
        let action = *actions
            .get(usize::from(choice.pick) % actions.len())
            .ok_or("there is always something to do")?;
        self.run(action, choice.arg)?;
        self.stats.most_panes = self.stats.most_panes.max(self.panes());
        self.stats.most_clients = self.stats.most_clients.max(self.links.len());
        Ok(())
    }

    fn actions(&self) -> Vec<Action> {
        let mut actions = vec![
            Action::Iterate,
            Action::Iterate,
            Action::Iterate,
            Action::Clock,
            Action::Settle,
        ];
        for (i, link) in self.links.iter().enumerate() {
            if !link.to_daemon.is_empty() {
                actions.extend([Action::ToDaemon(i); 3]);
            }
            if self.daemon.waiting_for(link.client.id()) > 0 {
                actions.extend([Action::ToClient(i); 3]);
            }
            actions.extend([
                Action::Type(i),
                Action::Type(i),
                Action::Command(i),
                Action::Command(i),
            ]);
            actions.extend([Action::Paste(i), Action::Resize(i), Action::HangUp(i)]);
        }
        let programs = self.daemon.programs();
        for &pane in &programs {
            actions.extend([Action::Produce(pane), Action::Produce(pane)]);
            if self.ending == Ending::Allowed || programs.len() > 1 {
                actions.push(Action::Exit(pane));
            }
        }
        if self.links.len() < MAX_CLIENTS {
            actions.push(Action::Connect);
        }
        actions
    }

    fn run(&mut self, action: Action, arg: u32) -> Result<(), Failure> {
        match action {
            Action::ToDaemon(i) => {
                if let Some(link) = self.links.get_mut(i) {
                    let n = chunk(arg, link.to_daemon.len());
                    let bytes: Vec<u8> = link.to_daemon.drain(..n).collect();
                    self.daemon.deliver(link.client.id(), &bytes);
                }
            }
            Action::ToClient(i) => self.deliver_to_client(i, Some(arg))?,
            Action::Iterate => {
                self.daemon.iteration()?;
            }
            Action::Clock => self.daemon.advance_clock(),
            Action::Produce(pane) => {
                if let Some(program) = self.daemon.program_mut(pane) {
                    program.produce(arg);
                }
            }
            Action::Type(i) => {
                self.stats.keys += 1;
                self.type_keys(i, &[typed(arg)])?;
            }
            Action::Command(i) => {
                self.stats.commands += 1;
                let keys = command(arg);
                if self.ending == Ending::Avoided && self.would_end(&keys) {
                    return Ok(());
                }
                self.type_keys(i, &keys)?;
            }
            Action::Paste(i) => {
                let text = [
                    "echo pasted\n",
                    "multi\nline\npaste",
                    "\x1b[201~escape attempt",
                    "漢字 👍🏽",
                ];
                let text = text.get(index(arg, text.len())).ok_or("a paste text")?;
                self.with_link(i, |link, trace| link.client.paste(text, trace))?;
            }
            Action::Resize(i) => {
                let cols = 2 + u16::try_from(arg % 99).map_err(|_overflow| "a width")?;
                let rows = 1 + u16::try_from((arg >> 8) % 40).map_err(|_overflow| "a height")?;
                let terminal = size(cols, rows)?;
                self.with_link(i, |link, trace| link.client.resize(terminal, trace))?;
            }
            Action::Connect => {
                let cols = 2 + u16::try_from(arg % 99).map_err(|_overflow| "a width")?;
                let rows = 1 + u16::try_from((arg >> 8) % 40).map_err(|_overflow| "a height")?;
                self.connect(size(cols, rows)?)?;
            }
            Action::HangUp(i) => {
                if i < self.links.len() {
                    let link = self.links.remove(i);
                    self.daemon.hang_up(link.client.id());
                }
            }
            Action::Exit(pane) => {
                if let Some(program) = self.daemon.program_mut(pane) {
                    program.exit();
                }
            }
            Action::Settle => self.settle()?,
        }
        Ok(())
    }

    /// Client `i` presses `keys` (for scenarios; schedules use [`World::step`]).
    ///
    /// # Errors
    ///
    /// When a check failed.
    pub fn press(&mut self, i: usize, keys: &[KeyEvent]) -> Result<(), Failure> {
        self.type_keys(i, keys)
    }

    /// Everything clients sent reaches the daemon (no iteration yet).
    pub fn deliver_all(&mut self) {
        for link in &mut self.links {
            let bytes = std::mem::take(&mut link.to_daemon);
            self.daemon.deliver(link.client.id(), &bytes);
        }
    }

    /// One daemon iteration.
    ///
    /// # Errors
    ///
    /// When the system broke.
    pub fn iterate(&mut self) -> Result<Activity, Failure> {
        self.daemon.iteration()
    }

    /// Every running program gets at least `bytes` of output to flood with.
    pub fn flood(&mut self, bytes: usize) {
        for pane in self.daemon.programs() {
            if let Some(program) = self.daemon.program_mut(pane) {
                program.flood(bytes);
            }
        }
    }

    /// A second client attaches with a terminal of `terminal`.
    ///
    /// # Errors
    ///
    /// When it cannot be made.
    pub fn attach(&mut self, terminal: GridSize) -> Result<(), Failure> {
        self.connect(terminal)
    }

    /// Whether typing `keys` could end the session: closing the last pane
    /// or the last tab, or killing it.
    fn would_end(&self, keys: &[KeyEvent]) -> bool {
        let structure = self.daemon.session().structure();
        let closes_pane = keys
            .windows(2)
            .any(|pair| pair == [letter(b'p'), letter(b'x')]);
        let closes_tab = keys
            .windows(2)
            .any(|pair| pair == [letter(b't'), letter(b'x')]);
        let kills = keys
            .windows(2)
            .any(|pair| pair == [letter(b'o'), letter(b'q')]);
        // Counted by running programs: a pane whose program is exiting is
        // about to go.
        kills
            || (closes_pane && self.daemon.programs().len() <= 1)
            || (closes_tab && (structure.tabs().count() <= 1 || self.daemon.programs().len() <= 1))
    }

    fn connect(&mut self, terminal: GridSize) -> Result<(), Failure> {
        let id = self.daemon.connect();
        let mut client = Client::attach(id, terminal)?;
        let to_daemon = client.outbox();
        self.links.push(Link { client, to_daemon });
        Ok(())
    }

    /// Runs `f` on client `i`, then queues what it sent.
    fn with_link(
        &mut self,
        i: usize,
        f: impl FnOnce(&mut Link, &mut Trace) -> Result<(), Failure>,
    ) -> Result<(), Failure> {
        if let Some(link) = self.links.get_mut(i) {
            let drawn = link.client.terminal_bytes();
            f(link, &mut self.trace)?;
            self.stats.terminal_bytes += link.client.terminal_bytes() - drawn;
            let sent = link.client.outbox();
            link.to_daemon.extend_from_slice(&sent);
        }
        Ok(())
    }

    fn type_keys(&mut self, i: usize, keys: &[KeyEvent]) -> Result<(), Failure> {
        self.with_link(i, |link, trace| {
            keys.iter().try_for_each(|&key| link.client.key(key, trace))
        })
    }

    /// Delivers to client `i` a chunk sized by `arg`, or everything.
    fn deliver_to_client(&mut self, i: usize, arg: Option<u32>) -> Result<(), Failure> {
        let Some(link) = self.links.get_mut(i) else {
            return Ok(());
        };
        let id = link.client.id();
        let waiting = self.daemon.waiting_for(id);
        let n = arg.map_or(waiting, |arg| chunk(arg, waiting));
        let bytes = self.daemon.send_to(id, n);
        let drawn_before = self.trace;
        self.with_link(i, |link, trace| link.client.receive(&bytes, trace))?;
        if self.trace != drawn_before {
            self.stats.draws += 1;
        }
        // A client the daemon ended is gone, with its connection.
        if let Some(link) = self.links.get(i)
            && link.client.end().is_some()
        {
            let link = self.links.remove(i);
            self.daemon.forget(link.client.id());
        }
        Ok(())
    }

    /// Lets everything in flight land: pipes empty, programs read, frames
    /// drawn and acknowledged, no tick due. Then every client's mirror must
    /// be the daemon's truth, and the daemon must hold exactly the clients
    /// that exist.
    ///
    /// # Errors
    ///
    /// When the system does not settle, or a check failed.
    pub fn settle(&mut self) -> Result<(), Failure> {
        for _ in 0..SETTLE_ROUNDS {
            if self.over() {
                return Ok(());
            }
            let mut moved = false;
            for link in &mut self.links {
                if !link.to_daemon.is_empty() {
                    let bytes = std::mem::take(&mut link.to_daemon);
                    self.daemon.deliver(link.client.id(), &bytes);
                    moved = true;
                }
            }
            if self.daemon.busy() && self.daemon.iteration()? == Activity::Busy {
                moved = true;
            }
            let mut i = 0;
            while i < self.links.len() {
                let before = self.links.len();
                let id = self.links.get(i).map(|link| link.client.id());
                if let Some(id) = id
                    && self.daemon.waiting_for(id) > 0
                {
                    self.deliver_to_client(i, None)?;
                    moved = true;
                }
                if self.links.len() == before {
                    i += 1;
                }
            }
            if !moved {
                if self.daemon.poll_timeout().is_none() {
                    self.check_quiet()?;
                    self.stats.quiet_checks += 1;
                    return Ok(());
                }
                self.daemon.advance_clock();
            }
        }
        Err(format!("did not settle in {SETTLE_ROUNDS} rounds"))
    }

    fn check_quiet(&self) -> Result<(), Failure> {
        let session = self.daemon.session();
        if session.client_count() != self.links.len() {
            return Err(format!(
                "the daemon holds {} clients, {} exist",
                session.client_count(),
                self.links.len()
            ));
        }
        for link in &self.links {
            if let Some(mirror) = link.client.mirror() {
                matches_session(mirror, session)
                    .map_err(|failure| format!("client {:?}: {failure}", link.client.id()))?;
                // Its terminal shows the focused pane's progress.
                let focus = session.structure().active().focus();
                let progress = session.pane_progress_or_none(focus);
                if progress != Some(link.client.progress()) {
                    return Err(format!(
                        "client {:?}'s terminal shows progress {:?}, the focused pane has {progress:?}",
                        link.client.id(),
                        link.client.progress()
                    ));
                }
            }
        }
        Ok(())
    }
}

/// `arg` reduced to an index below `len` (which is not zero).
#[expect(
    clippy::as_conversions,
    reason = "u32 widens losslessly into usize on the 64-bit targets plx builds for"
)]
const fn index(arg: u32, len: usize) -> usize {
    arg as usize % len
}

/// A chunk of `len` bytes sized by `arg`: everything a quarter of the time,
/// otherwise anything from one byte.
const fn chunk(arg: u32, len: usize) -> usize {
    if len == 0 || arg % 4 == 0 {
        len
    } else {
        1 + index(arg >> 2, len)
    }
}

/// A key typed into the focused pane (or into a mode).
fn typed(arg: u32) -> KeyEvent {
    match arg % 8 {
        0 => press(Key::Enter, Mods::empty()),
        1 => press(Key::Backspace, Mods::empty()),
        2 => press(Key::Up, Mods::empty()),
        3 => press(Key::Char(KeyChar::from_byte(b'c')), Mods::CTRL),
        _ => {
            let typing = b"abcxyz q";
            typing
                .get(index(arg >> 3, typing.len()))
                .map_or(letter(b'a'), |&byte| letter(byte))
        }
    }
}

/// A command, typed as keys from any mode: Escape returns to the base
/// mode (locked), Ctrl-g enters normal, then the mode and the command.
/// `arg % 16` picks it: 0–2 split right, 3–5 split down, 12 a new tab.
#[must_use]
pub fn command(arg: u32) -> Vec<KeyEvent> {
    let escape = press(Key::Escape, Mods::empty());
    let mut keys = vec![
        escape,
        press(Key::Char(KeyChar::from_byte(b'g')), Mods::CTRL),
    ];
    let tail: &[u8] = match arg % 16 {
        0..=2 => b"pn",
        3..=5 => b"pd",
        6 => b"px",
        7 => b"pf",
        8 => b"ph",
        9 => b"pl",
        10 => b"pj",
        11 => b"pk",
        12 => b"tn",
        13 => b"tl",
        14 => b"th",
        // Detaching or killing ends a client or the session: rare.
        _ if arg % 64 == 15 => b"od",
        _ => b"tx",
    };
    keys.extend(tail.iter().map(|&byte| letter(byte)));
    keys.push(escape);
    keys
}
