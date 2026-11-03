//! Fake programs: what runs in a simulated pane.
//!
//! A program sees what the daemon writes to its PTY and produces output
//! the daemon reads, in budgeted chunks, like a real PTY. Each pane gets
//! one by its id, so a schedule replays identically.

use plx_core::id::PaneId;

/// What a program does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Echoes its input, like a shell's line discipline.
    Cat,
    /// Emits VT streams: text of every width, styles, cursor movement,
    /// queries that need replies, canaries that must not leak out of the
    /// pane, and garbage.
    Script,
    /// A full-screen program on the alternate screen, redrawn on resize.
    FullScreen,
    /// Exits as soon as it is written to.
    Exits,
}

/// Whether a schedule may end the session (its last pane exiting, its last
/// tab closed, a kill).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    Allowed,
    /// Endings are not offered, so long schedules keep running.
    Avoided,
}

/// Whether a program still runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Life {
    Running,
    /// It asked to exit (or was hung up); the daemon reports it next.
    Exiting,
}

/// Script output, chosen by the schedule.
const PIECES: &[&[u8]] = &[
    b"plain text ",
    b"\r\n",
    b"\n",
    "漢字".as_bytes(),
    "e\u{301}".as_bytes(),
    "👍🏽".as_bytes(),
    "👨\u{200d}👩\u{200d}👧".as_bytes(),
    b"\x1b[31mred\x1b[0m",
    b"\x1b[1;4:3;38;2;10;200;30mstyled\x1b[0m",
    b"\x1b[44m\x1b[K\x1b[0m",
    b"\x1b[H",
    b"\x1b[3;4H",
    b"\x1b[2J",
    b"\x1b[?25l",
    b"\x1b[?25h",
    b"\x1b[5 q",
    // Queries: the pane's emulator answers through the daemon.
    b"\x1b[c",
    b"\x1b[6n",
    // Canaries: they may change the pane, never the client's terminal.
    b"\x1bc",
    b"\x1b[2;4r",
    b"\x1b[?1049h",
    b"\x1b[?1049l",
    b"\x1b]0;title\x07",
    b"\x1b]52;c;aGk=\x07",
    b"\x07",
    // Progress reports: they reach the client's terminal only typed, as
    // the client rebuilds them (one past 100, one in a state the protocol
    // does not have).
    b"\x1b]9;4;1;42\x07",
    b"\x1b]9;4;3\x1b\\",
    b"\x1b]9;4;2;250\x07",
    b"\x1b]9;4;0\x07",
    b"\x1b]9;4;7\x07",
    // Garbage: cut sequences, stray bytes.
    b"\x1b[",
    b"\xff\xfe",
    b"\x1b]unterminated",
];

/// One simulated program.
#[derive(Debug)]
pub struct Program {
    kind: Kind,
    life: Life,
    /// Output the daemon has not read yet.
    pending: Vec<u8>,
    /// Everything written to it, for the responsiveness check.
    received: usize,
}

impl Program {
    /// The program a pane runs, by its id. Where a schedule keeps the
    /// session alive, no program exits on its own.
    #[must_use]
    pub fn for_pane(pane: PaneId, ending: Ending) -> Self {
        let kind = match (pane.get() % 4, ending) {
            (1, _) => Kind::Script,
            (2, _) => Kind::FullScreen,
            (3, Ending::Allowed) => Kind::Exits,
            _ => Kind::Cat,
        };
        let mut program = Self {
            kind,
            life: Life::Running,
            pending: Vec::new(),
            received: 0,
        };
        if kind == Kind::FullScreen {
            program.draw_full_screen();
        }
        program
    }

    #[must_use]
    pub const fn kind(&self) -> Kind {
        self.kind
    }

    #[must_use]
    pub const fn life(&self) -> Life {
        self.life
    }

    /// Bytes written to it so far.
    #[must_use]
    pub const fn received(&self) -> usize {
        self.received
    }

    /// Whether it has output the daemon has not read.
    #[must_use]
    pub const fn has_output(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The daemon wrote to its PTY.
    pub fn write(&mut self, bytes: &[u8]) {
        self.received += bytes.len();
        match self.kind {
            Kind::Cat => self.pending.extend_from_slice(bytes),
            Kind::Exits if !bytes.is_empty() => self.life = Life::Exiting,
            Kind::Script | Kind::FullScreen | Kind::Exits => {}
        }
    }

    /// Its PTY was resized.
    pub fn resize(&mut self) {
        if self.kind == Kind::FullScreen {
            self.draw_full_screen();
        }
    }

    /// The schedule lets it produce output; `choice` picks what.
    pub fn produce(&mut self, choice: u32) {
        match self.kind {
            Kind::Script => {
                // Up to four pieces, picked from the choice's bytes.
                for byte in choice.to_le_bytes() {
                    if let Some(piece) = PIECES.get(usize::from(byte) % (PIECES.len() + 4)) {
                        self.pending.extend_from_slice(piece);
                    }
                }
            }
            Kind::FullScreen => self.draw_full_screen(),
            Kind::Cat | Kind::Exits => {}
        }
    }

    /// Floods it: more output than the daemon reads in one iteration.
    pub fn flood(&mut self, bytes: usize) {
        while self.pending.len() < bytes {
            self.pending
                .extend_from_slice(b"flood flood flood flood flood flood\r\n");
        }
    }

    /// The daemon reads up to `budget` bytes of its output into `into`.
    pub fn read(&mut self, budget: usize, into: &mut Vec<u8>) {
        let take = budget.min(self.pending.len());
        into.extend(self.pending.drain(..take));
    }

    /// It exits, or was hung up.
    pub const fn exit(&mut self) {
        self.life = Life::Exiting;
    }

    fn draw_full_screen(&mut self) {
        self.pending.extend_from_slice(
            b"\x1b[?1049h\x1b[H\x1b[2J\x1b[7m status \x1b[0m\x1b[2;1H| a full-screen program |",
        );
    }
}
