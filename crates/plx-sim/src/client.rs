//! A simulated client: the real [`ClientCore`] between a pipe to the daemon
//! and a headless libghostty standing in for the user's terminal.
//!
//! Everything the core writes to the terminal is checked as it is drawn:
//! it holds only the compositor's sequences, the terminal answers nothing,
//! and afterwards the terminal shows the mirror.

use plx_client_core::client::{ClientCore, ClientEffects, End, Received};
use plx_client_core::compose::{Capabilities, ColorDepth, ProgressBars, UnderlineStyles};
use plx_client_core::mirror::Mirror;
use plx_core::id::ClientId;
use plx_core::rejection::Rejection;
use plx_emulator::{Emulator, GridSize, KeyEvent, Progress, Version};
use plx_ghostty::GhosttyEmulator;

use crate::daemon::OPTIONS;
use crate::oracle::{Failure, in_vocabulary, shows_mirror};
use crate::trace::{Tag, Trace};

const CAPABILITIES: Capabilities = Capabilities {
    color: ColorDepth::TrueColor,
    underlines: UnderlineStyles::Styled,
    progress: ProgressBars::Drawn,
};

/// The client shell's buffers.
#[derive(Debug, Default)]
struct Shell {
    daemon: Vec<u8>,
    terminal: Vec<u8>,
}

impl ClientEffects for Shell {
    fn daemon(&mut self) -> &mut Vec<u8> {
        &mut self.daemon
    }
    fn terminal(&mut self) -> &mut Vec<u8> {
        &mut self.terminal
    }
    fn rejected(&mut self, _: Rejection) {}
}

/// One simulated client.
#[derive(Debug)]
pub struct Client {
    id: ClientId,
    /// `None` once the daemon ended the connection.
    core: Option<ClientCore>,
    end: Option<End>,
    shell: Shell,
    /// Bytes from the daemon not yet a whole message.
    inbox: Vec<u8>,
    terminal: GridSize,
    outer: GhosttyEmulator,
    version: Version,
    terminal_bytes: usize,
    /// The progress the client told its terminal to show, as the terminal
    /// read it.
    progress: Progress,
}

impl Client {
    /// Attaches with a terminal of `terminal` (the daemon side is `id`).
    ///
    /// # Errors
    ///
    /// When the headless terminal cannot be made.
    pub fn attach(id: ClientId, terminal: GridSize) -> Result<Self, Failure> {
        let mut shell = Shell::default();
        let core = ClientCore::attach(terminal, None, CAPABILITIES, &mut shell);
        Ok(Self {
            id,
            core: Some(core),
            end: None,
            shell,
            inbox: Vec::new(),
            terminal,
            outer: GhosttyEmulator::new(terminal, &OPTIONS)
                .map_err(|error| format!("a headless terminal: {error}"))?,
            version: Version::ZERO,
            terminal_bytes: 0,
            progress: Progress::Removed,
        })
    }

    #[must_use]
    pub const fn id(&self) -> ClientId {
        self.id
    }

    /// How the daemon ended it; `None` while attached.
    #[must_use]
    pub const fn end(&self) -> Option<End> {
        self.end
    }

    #[must_use]
    pub fn mirror(&self) -> Option<&Mirror> {
        self.core.as_ref().map(ClientCore::mirror)
    }

    /// The progress its terminal shows.
    #[must_use]
    pub const fn progress(&self) -> Progress {
        self.progress
    }

    /// What it sent since the last call, for the pipe to the daemon.
    pub fn outbox(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.shell.daemon)
    }

    /// Bytes from the daemon arrive.
    ///
    /// # Errors
    ///
    /// When the daemon broke the protocol, or a check failed.
    pub fn receive(&mut self, bytes: &[u8], trace: &mut Trace) -> Result<(), Failure> {
        self.inbox.extend_from_slice(bytes);
        if let Some(core) = self.core.take() {
            match core.receive(&mut self.inbox, &mut self.shell) {
                Ok(Received::Attached(core)) => self.core = Some(core),
                Ok(Received::Ended(end)) => self.end = Some(end),
                Err(error) => return Err(format!("client {:?}: {error}", self.id)),
            }
        }
        self.draw(trace)
    }

    /// The user presses a key.
    ///
    /// # Errors
    ///
    /// When a check failed.
    pub fn key(&mut self, key: KeyEvent, trace: &mut Trace) -> Result<(), Failure> {
        if let Some(core) = self.core.as_mut() {
            core.key(key, &mut self.shell);
        }
        self.draw(trace)
    }

    /// The user pastes `text`.
    ///
    /// # Errors
    ///
    /// When a check failed (a paste always fits here).
    pub fn paste(&mut self, text: &str, trace: &mut Trace) -> Result<(), Failure> {
        if let Some(core) = self.core.as_mut() {
            core.paste(text, &mut self.shell)
                .map_err(|error| format!("client {:?}: {error}", self.id))?;
        }
        self.draw(trace)
    }

    /// The user's terminal resizes.
    ///
    /// # Errors
    ///
    /// When the headless terminal cannot follow, or a check failed.
    pub fn resize(&mut self, terminal: GridSize, trace: &mut Trace) -> Result<(), Failure> {
        self.terminal = terminal;
        self.outer
            .resize(terminal, None)
            .map_err(|error| format!("resizing a headless terminal: {error}"))?;
        if let Some(core) = self.core.as_mut() {
            core.resized(terminal, None, &mut self.shell);
        }
        self.draw(trace)
    }

    /// Bytes written to its terminal so far.
    #[must_use]
    pub const fn terminal_bytes(&self) -> usize {
        self.terminal_bytes
    }

    /// Writes what the core drew to the terminal and checks it.
    fn draw(&mut self, trace: &mut Trace) -> Result<(), Failure> {
        if self.shell.terminal.is_empty() {
            return Ok(());
        }
        self.terminal_bytes += self.shell.terminal.len();
        trace.record(Tag::ToTerminal, self.id.get(), &self.shell.terminal);
        in_vocabulary(&self.shell.terminal)
            .map_err(|failure| format!("client {:?}: {failure}", self.id))?;
        // The client's own clipboard writes and progress reports are
        // expected; anything the terminal answers, or a bell, is not.
        let events = self.outer.feed(&self.shell.terminal);
        let answered = !events.replies().is_empty() || events.bell() == plx_emulator::Bell::Rang;
        if let Some(progress) = events.progress_or_none() {
            self.progress = progress;
        }
        self.shell.terminal.clear();
        if answered {
            return Err(format!(
                "client {:?}: the terminal answered or rang",
                self.id
            ));
        }
        self.version = self.version.next();
        self.outer
            .sync(self.version)
            .map_err(|error| format!("syncing a headless terminal: {error}"))?;
        match &self.core {
            Some(core) => shows_mirror(core.mirror(), self.terminal, &self.outer)
                .map_err(|failure| format!("client {:?}: {failure}", self.id)),
            None => Ok(()),
        }
    }
}
