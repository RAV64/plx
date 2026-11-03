//! Input modes and keymaps (Zellij-style).
//!
//! Each client is in one [`Mode`]. A key is resolved against that mode's
//! bindings to exactly one [`Resolution`]: run a command, switch mode, pass
//! the key to the focused pane, or ignore it. Every keymap is validated so
//! that the base mode is reachable from every mode: no configuration can
//! trap a user in a mode.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::num::NonZeroU16;

use plx_bug::OrBug as _;
use plx_emulator::{Key, KeyAction, KeyChar, KeyChord, KeyEvent, Mods, SearchDirection};
use serde::{Deserialize, Serialize};

use crate::command::{ClientCommand, Command, ScrollBy, SearchCommand, SessionCommand, TabSide};
use crate::geometry::Direction;
use crate::tab::TabNumber;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Mode {
    /// Every key goes to the pane except the one that unlocks.
    Locked,
    /// The mode selector (or, in the classic preset, the passthrough mode
    /// with mode-entry chords).
    Normal,
    Pane,
    Tab,
    Resize,
    Move,
    Scroll,
    Session,
    /// Typing the needle to search the focused pane's history for.
    EnterSearch,
    /// Going from match to match.
    Search,
}

impl Mode {
    pub const ALL: [Self; 10] = [
        Self::Locked,
        Self::Normal,
        Self::Pane,
        Self::Tab,
        Self::Resize,
        Self::Move,
        Self::Scroll,
        Self::Session,
        Self::EnterSearch,
        Self::Search,
    ];

    /// Whether a search is going on in this mode.
    #[must_use]
    pub const fn searching(self) -> Searching {
        match self {
            Self::EnterSearch | Self::Search => Searching::Yes,
            Self::Locked
            | Self::Normal
            | Self::Pane
            | Self::Tab
            | Self::Resize
            | Self::Move
            | Self::Scroll
            | Self::Session => Searching::No,
        }
    }

    /// Whether the view may be back in the history in this mode: leaving
    /// these modes returns it to the live screen.
    #[must_use]
    pub const fn scrolling(self) -> Scrolling {
        match self {
            Self::Scroll | Self::EnterSearch | Self::Search => Scrolling::Yes,
            Self::Locked
            | Self::Normal
            | Self::Pane
            | Self::Tab
            | Self::Resize
            | Self::Move
            | Self::Session => Scrolling::No,
        }
    }
}

/// See [`Mode::searching`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Searching {
    No,
    Yes,
}

/// See [`Mode::scrolling`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scrolling {
    No,
    Yes,
}

/// What happens after a bound command runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum After {
    /// Stay in the current mode (e.g. repeated resizing).
    Stay,
    /// Return to the base mode.
    Base,
    /// Go to this mode.
    Enter(Mode),
}

/// What a binding does.
#[derive(Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Action {
    Run {
        command: Command,
        after: After,
    },
    Enter(Mode),
    /// Return to the base mode.
    Exit,
}

/// What happens to a key with no binding in the current mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Unbound {
    /// The focused pane receives it.
    PassThrough,
    /// It is dropped.
    Ignore,
    /// It is typed into the mode's text (a search needle).
    Input,
}

/// The bindings of one mode.
#[derive(Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModeKeys {
    pub bindings: BTreeMap<KeyChord, Action>,
    pub unbound: Unbound,
}

/// The outcome of resolving a key. A bound command is lent from the keymap,
/// never copied per keystroke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution<'k> {
    /// Run `command`, then be in mode `next`.
    Run { command: &'k Command, next: Mode },
    /// Switch to another mode.
    Switch(Mode),
    /// Send the key to the focused pane.
    PassThrough,
    /// Drop the key.
    Ignore,
    /// Type the key into the mode's text.
    Input,
}

/// A keymap that failed validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeymapError {
    #[error("there is no way back from {mode:?} mode to the base mode {base:?}")]
    BaseUnreachable { mode: Mode, base: Mode },
}

/// One table per mode. Total by construction: there is no mode without a
/// table, so resolving a key can never miss.
#[derive(Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModeTables {
    pub locked: ModeKeys,
    pub normal: ModeKeys,
    pub pane: ModeKeys,
    pub tab: ModeKeys,
    pub resize: ModeKeys,
    pub moving: ModeKeys,
    pub scroll: ModeKeys,
    pub session: ModeKeys,
    pub enter_search: ModeKeys,
    pub search: ModeKeys,
}

impl ModeTables {
    /// The table of `mode`.
    #[must_use]
    pub const fn get(&self, mode: Mode) -> &ModeKeys {
        match mode {
            Mode::Locked => &self.locked,
            Mode::Normal => &self.normal,
            Mode::Pane => &self.pane,
            Mode::Tab => &self.tab,
            Mode::Resize => &self.resize,
            Mode::Move => &self.moving,
            Mode::Scroll => &self.scroll,
            Mode::Session => &self.session,
            Mode::EnterSearch => &self.enter_search,
            Mode::Search => &self.search,
        }
    }

    /// The table of `mode`, to change.
    pub const fn get_mut(&mut self, mode: Mode) -> &mut ModeKeys {
        match mode {
            Mode::Locked => &mut self.locked,
            Mode::Normal => &mut self.normal,
            Mode::Pane => &mut self.pane,
            Mode::Tab => &mut self.tab,
            Mode::Resize => &mut self.resize,
            Mode::Move => &mut self.moving,
            Mode::Scroll => &mut self.scroll,
            Mode::Session => &mut self.session,
            Mode::EnterSearch => &mut self.enter_search,
            Mode::Search => &mut self.search,
        }
    }
}

/// The tables of every mode plus the base mode, validated on creation: the
/// only constructor is [`Keymap::new`], and deserialising goes through it.
#[derive(Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawKeymap")]
pub struct Keymap {
    base: Mode,
    tables: ModeTables,
}

/// A keymap as it arrives from outside, before it is validated.
#[derive(Deserialize)]
struct RawKeymap {
    base: Mode,
    tables: ModeTables,
}

impl TryFrom<RawKeymap> for Keymap {
    type Error = KeymapError;

    fn try_from(raw: RawKeymap) -> Result<Self, KeymapError> {
        Self::new(raw.base, raw.tables)
    }
}

impl Keymap {
    /// A keymap with base mode `base`.
    ///
    /// # Errors
    ///
    /// When some mode has no sequence of bindings leading back to `base`.
    pub fn new(base: Mode, tables: ModeTables) -> Result<Self, KeymapError> {
        let keymap = Self { base, tables };
        keymap.check()?;
        Ok(keymap)
    }

    #[must_use]
    pub const fn base(&self) -> Mode {
        self.base
    }

    #[must_use]
    pub const fn tables(&self) -> &ModeTables {
        &self.tables
    }

    /// The base mode and tables, moved out (e.g. to edit one table and build a
    /// new keymap with [`Keymap::new`]).
    #[must_use]
    pub fn into_tables(self) -> (Mode, ModeTables) {
        (self.base, self.tables)
    }

    /// Resolves `event` in `mode`.
    #[must_use]
    pub fn resolve(&self, mode: Mode, event: &KeyEvent) -> Resolution<'_> {
        let keys = self.tables.get(mode);
        match keys.bindings.get(&event.chord()) {
            // A release never triggers a binding, and the release of a bound
            // key is not leaked to the pane either.
            Some(_) if event.action == KeyAction::Release => Resolution::Ignore,
            Some(action) => match action {
                Action::Run { command, after } => Resolution::Run {
                    command,
                    next: match after {
                        After::Stay => mode,
                        After::Base => self.base,
                        After::Enter(target) => *target,
                    },
                },
                Action::Enter(target) => Resolution::Switch(*target),
                Action::Exit => Resolution::Switch(self.base),
            },
            None => match keys.unbound {
                Unbound::PassThrough => Resolution::PassThrough,
                Unbound::Ignore => Resolution::Ignore,
                Unbound::Input => Resolution::Input,
            },
        }
    }

    /// Checks that the base mode is reachable from every mode; part of
    /// constructing a keymap, so every keymap has passed it.
    fn check(&self) -> Result<(), KeymapError> {
        for mode in Mode::ALL {
            if !self.reaches_base(mode) {
                return Err(KeymapError::BaseUnreachable {
                    mode,
                    base: self.base,
                });
            }
        }
        Ok(())
    }

    fn reaches_base(&self, from: Mode) -> bool {
        let mut seen: Vec<Mode> = Vec::new();
        let mut frontier = Vec::from([from]);
        while let Some(mode) = frontier.pop() {
            if mode == self.base {
                return true;
            }
            if seen.contains(&mode) {
                continue;
            }
            seen.push(mode);
            for action in self.tables.get(mode).bindings.values() {
                match action {
                    Action::Exit
                    | Action::Run {
                        after: After::Base, ..
                    } => return true,
                    Action::Enter(target)
                    | Action::Run {
                        after: After::Enter(target),
                        ..
                    } => frontier.push(*target),
                    Action::Run {
                        after: After::Stay, ..
                    } => {}
                }
            }
        }
        false
    }

    /// The default preset: start locked so plx steals no keys from programs;
    /// `Ctrl-g` unlocks, then one letter picks a mode.
    #[must_use]
    pub fn unlock_first() -> Self {
        let normal = keys(
            Unbound::Ignore,
            [
                (plain(b'p'), Action::Enter(Mode::Pane)),
                (plain(b't'), Action::Enter(Mode::Tab)),
                (plain(b'r'), Action::Enter(Mode::Resize)),
                (plain(b'm'), Action::Enter(Mode::Move)),
                (plain(b's'), Action::Enter(Mode::Scroll)),
                (plain(b'o'), Action::Enter(Mode::Session)),
                (KeyChord::new(Key::Escape, Mods::empty()), Action::Exit),
                (ctrl(b'g'), Action::Exit),
            ],
        );
        let locked = keys(
            Unbound::PassThrough,
            [(ctrl(b'g'), Action::Enter(Mode::Normal))],
        );
        Self::from_preset(Mode::Locked, locked, normal)
    }

    /// The classic preset, Zellij's layout: it starts in normal mode, where
    /// unbound keys pass through.
    ///
    /// - `Ctrl-p` pane, `Ctrl-t` tab, `Ctrl-n` resize, `Ctrl-h` move,
    ///   `Ctrl-s` scroll and `Ctrl-m` session enter their mode from any mode
    ///   but locked; a mode's own key returns to normal. `Ctrl-g` locks and
    ///   unlocks.
    /// - Outside locked mode, `Alt-1`..`Alt-9` go to a tab, `Alt-n` and
    ///   `Alt-m` split down and right, `Alt-h/j/k/l` move the focus, `Alt-e`
    ///   opens the focused pane's history in the editor, `Alt-r` moves
    ///   every pane one place on. With Shift, `Alt-H/J/K/L` move the pane
    ///   (into the tab before or after at the left and right edges),
    ///   `Alt-N` moves it into a tab of its own; `Alt-i`/`Alt-o` move the
    ///   tab.
    /// - In pane mode `n` splits down, `m` right, `f` toggles fullscreen and
    ///   `x` closes; in session mode `d` detaches and `Ctrl-q` quits.
    #[must_use]
    pub fn classic() -> Self {
        let normal = keys(Unbound::PassThrough, []);
        let locked = keys(
            Unbound::PassThrough,
            [(ctrl(b'g'), Action::Enter(Mode::Normal))],
        );
        let (_, mut tables) = Self::from_preset(Mode::Normal, locked, normal).into_tables();
        // Pane mode splits as Zellij does: `n` down, `m` right.
        for &letter in b"nrd" {
            tables.pane.bindings.remove(&plain(letter));
        }
        tables.pane.bindings.extend([
            (plain(b'n'), split(Direction::Down, After::Base)),
            (plain(b'm'), split(Direction::Right, After::Base)),
        ]);
        tables.session.bindings.remove(&plain(b'q'));
        tables
            .session
            .bindings
            .insert(ctrl(b'q'), run(SessionCommand::KillSession, After::Base));
        for mode in Mode::ALL {
            if mode == Mode::Locked {
                continue;
            }
            let table = tables.get_mut(mode);
            table.bindings.extend(CLASSIC_MODES.map(|(letter, target)| {
                let action = if target == mode {
                    Action::Exit
                } else {
                    Action::Enter(target)
                };
                (ctrl(letter), action)
            }));
            table
                .bindings
                .insert(ctrl(b'g'), Action::Enter(Mode::Locked));
            table.bindings.extend(classic_alt_keys());
        }
        // In normal mode too: the view stays back until the next keystroke
        // to the program.
        tables.normal.bindings.extend(prompt_jumps());
        // Scroll mode's key leaves a search too, as in Zellij.
        for mode in [Mode::EnterSearch, Mode::Search] {
            tables
                .get_mut(mode)
                .bindings
                .insert(ctrl(b's'), Action::Exit);
        }
        Self::new(Mode::Normal, tables).or_bug("the presets are valid keymaps")
    }

    /// A preset from its locked and normal tables plus the shared ones,
    /// validated like any keymap.
    #[track_caller]
    fn from_preset(base: Mode, locked: ModeKeys, normal: ModeKeys) -> Self {
        let session = keys(
            Unbound::Ignore,
            exits().into_iter().chain([
                (plain(b'd'), run_client(ClientCommand::Detach, After::Base)),
                (plain(b'q'), run(SessionCommand::KillSession, After::Base)),
            ]),
        );
        let tables = ModeTables {
            locked,
            normal,
            pane: pane_keys(),
            tab: tab_keys(),
            resize: directional(|direction| SessionCommand::Grow { direction }),
            moving: directional(|direction| SessionCommand::Swap { direction }),
            scroll: scroll_keys(),
            session,
            enter_search: enter_search_keys(),
            search: search_keys(),
        };
        Self::new(base, tables).or_bug("the presets are valid keymaps")
    }
}

/// The classic preset's mode keys: `Ctrl-<letter>`.
const CLASSIC_MODES: [(u8, Mode); 6] = [
    (b'p', Mode::Pane),
    (b't', Mode::Tab),
    (b'n', Mode::Resize),
    (b'h', Mode::Move),
    (b's', Mode::Scroll),
    (b'm', Mode::Session),
];

/// The classic preset's `Alt` chords, outside locked mode.
fn classic_alt_keys() -> impl Iterator<Item = (KeyChord, Action)> {
    let tabs = (b'1'..=b'9').zip(NonZeroU16::MIN..).map(|(digit, number)| {
        let tab = TabNumber::new(number);
        (
            alt(digit),
            run(SessionCommand::FocusTab { tab }, After::Base),
        )
    });
    let splits = [
        (alt(b'n'), split(Direction::Down, After::Stay)),
        (alt(b'm'), split(Direction::Right, After::Stay)),
        (alt(b'e'), edit_scrollback()),
        (alt(b'r'), run(SessionCommand::RotatePanes, After::Stay)),
        (alt(b'N'), run(SessionCommand::BreakPane, After::Stay)),
        (
            alt(b'i'),
            run(
                SessionCommand::MoveTab {
                    side: TabSide::Previous,
                },
                After::Stay,
            ),
        ),
        (
            alt(b'o'),
            run(
                SessionCommand::MoveTab {
                    side: TabSide::Next,
                },
                After::Stay,
            ),
        ),
    ];
    let focus = [
        (b'h', Direction::Left),
        (b'j', Direction::Down),
        (b'k', Direction::Up),
        (b'l', Direction::Right),
    ]
    .map(|(letter, direction)| {
        (
            alt(letter),
            run(SessionCommand::Focus { direction }, After::Stay),
        )
    });
    // With Shift, the pane itself moves (across tabs at the left and right
    // edges).
    let moves = [
        (b'H', Direction::Left),
        (b'J', Direction::Down),
        (b'K', Direction::Up),
        (b'L', Direction::Right),
    ]
    .map(|(letter, direction)| {
        (
            alt(letter),
            run(SessionCommand::MovePane { direction }, After::Stay),
        )
    });
    tabs.chain(splits).chain(focus).chain(moves)
}

const fn split(direction: Direction, after: After) -> Action {
    run(SessionCommand::Split { direction }, after)
}

fn keys(unbound: Unbound, bindings: impl IntoIterator<Item = (KeyChord, Action)>) -> ModeKeys {
    ModeKeys {
        bindings: bindings.into_iter().collect(),
        unbound,
    }
}

const fn plain(c: u8) -> KeyChord {
    KeyChord::new(Key::Char(KeyChar::from_byte(c)), Mods::empty())
}

const fn ctrl(c: u8) -> KeyChord {
    KeyChord::new(Key::Char(KeyChar::from_byte(c)), Mods::CTRL)
}

const fn alt(c: u8) -> KeyChord {
    KeyChord::new(Key::Char(KeyChar::from_byte(c)), Mods::ALT)
}

const fn run(command: SessionCommand, after: After) -> Action {
    Action::Run {
        command: Command::Session(command),
        after,
    }
}

const fn run_client(command: ClientCommand, after: After) -> Action {
    Action::Run {
        command: Command::Client(command),
        after,
    }
}

/// `Esc`, `Enter` and `Ctrl-g` leave any non-base mode.
const fn exits() -> [(KeyChord, Action); 3] {
    [
        (KeyChord::new(Key::Escape, Mods::empty()), Action::Exit),
        (KeyChord::new(Key::Enter, Mods::empty()), Action::Exit),
        (ctrl(b'g'), Action::Exit),
    ]
}

/// The direction keys: `h j k l` and the arrows.
const DIRECTIONS: [(Key, Key, Direction); 4] = [
    (
        Key::Char(KeyChar::from_byte(b'h')),
        Key::Left,
        Direction::Left,
    ),
    (
        Key::Char(KeyChar::from_byte(b'j')),
        Key::Down,
        Direction::Down,
    ),
    (Key::Char(KeyChar::from_byte(b'k')), Key::Up, Direction::Up),
    (
        Key::Char(KeyChar::from_byte(b'l')),
        Key::Right,
        Direction::Right,
    ),
];

/// A mode whose direction keys repeat a command (staying in the mode).
fn directional(command: fn(Direction) -> SessionCommand) -> ModeKeys {
    let moves = DIRECTIONS
        .into_iter()
        .flat_map(move |(letter, arrow, direction)| {
            [letter, arrow].map(|key| {
                (
                    KeyChord::new(key, Mods::empty()),
                    run(command(direction), After::Stay),
                )
            })
        });
    keys(Unbound::Ignore, exits().into_iter().chain(moves))
}

fn pane_keys() -> ModeKeys {
    let mut pane = directional(|direction| SessionCommand::Focus { direction });
    pane.bindings.extend([
        (
            plain(b'n'),
            run(
                SessionCommand::Split {
                    direction: Direction::Right,
                },
                After::Base,
            ),
        ),
        (
            plain(b'r'),
            run(
                SessionCommand::Split {
                    direction: Direction::Right,
                },
                After::Base,
            ),
        ),
        (
            plain(b'd'),
            run(
                SessionCommand::Split {
                    direction: Direction::Down,
                },
                After::Base,
            ),
        ),
        (plain(b'x'), run(SessionCommand::ClosePane, After::Base)),
        (plain(b'f'), run(SessionCommand::ToggleZoom, After::Base)),
    ]);
    pane
}

/// Scroll mode, as Zellij binds it: `j`/`k` a line, `d`/`u` half a page,
/// `h`/`l` and PageUp/PageDown a page, `/` searches. Leaving the mode
/// returns to the live screen.
fn scroll_keys() -> ModeKeys {
    let mut scroll = keys(Unbound::Ignore, exits());
    let page = |key, by| (KeyChord::new(key, Mods::empty()), scroll_by(by));
    scroll.bindings.extend([
        (plain(b'j'), scroll_by(ScrollBy::LineDown)),
        (plain(b'k'), scroll_by(ScrollBy::LineUp)),
        (plain(b'd'), scroll_by(ScrollBy::HalfPageDown)),
        (plain(b'u'), scroll_by(ScrollBy::HalfPageUp)),
        (plain(b'h'), scroll_by(ScrollBy::PageUp)),
        (plain(b'l'), scroll_by(ScrollBy::PageDown)),
        page(Key::PageUp, ScrollBy::PageUp),
        page(Key::PageDown, ScrollBy::PageDown),
        page(Key::Up, ScrollBy::LineUp),
        page(Key::Down, ScrollBy::LineDown),
    ]);
    scroll.bindings.extend(prompt_jumps());
    scroll
        .bindings
        .insert(plain(b'/'), Action::Enter(Mode::EnterSearch));
    scroll.bindings.insert(plain(b'e'), edit_scrollback());
    scroll
}

/// Opens the focused pane's history in the user's editor, then back to the
/// base mode.
const fn edit_scrollback() -> Action {
    Action::Run {
        command: Command::EditScrollback,
        after: After::Base,
    }
}

/// Typing the needle: keys are typed into it (`Backspace` deletes), the
/// matches marked as it grows. `Enter` selects the newest match and goes
/// on to search mode; `Esc` gives up, back to scroll mode.
fn enter_search_keys() -> ModeKeys {
    keys(
        Unbound::Input,
        [
            (
                KeyChord::new(Key::Enter, Mods::empty()),
                Action::Run {
                    command: Command::Search(SearchCommand::Select(SearchDirection::Older)),
                    after: After::Enter(Mode::Search),
                },
            ),
            (
                KeyChord::new(Key::Escape, Mods::empty()),
                Action::Enter(Mode::Scroll),
            ),
            (ctrl(b'g'), Action::Exit),
        ],
    )
}

/// Search mode, as the user's Zellij binds it: `n` the next match down,
/// `N` up, `c` letter case, `o` whole words; scrolling as in scroll mode.
fn search_keys() -> ModeKeys {
    let mut search = scroll_keys();
    let command = |command| Action::Run {
        command: Command::Search(command),
        after: After::Stay,
    };
    search.bindings.extend([
        (
            plain(b'n'),
            command(SearchCommand::Select(SearchDirection::Newer)),
        ),
        (
            plain(b'N'),
            command(SearchCommand::Select(SearchDirection::Older)),
        ),
        (plain(b'c'), command(SearchCommand::ToggleCase)),
        (plain(b'o'), command(SearchCommand::ToggleWholeWord)),
    ]);
    search
}

/// `Ctrl-Up` and `Ctrl-Down`: the previous and next command the shell ran.
const fn prompt_jumps() -> [(KeyChord, Action); 2] {
    [
        (
            KeyChord::new(Key::Up, Mods::CTRL),
            scroll_by(ScrollBy::PreviousPrompt),
        ),
        (
            KeyChord::new(Key::Down, Mods::CTRL),
            scroll_by(ScrollBy::NextPrompt),
        ),
    ]
}

const fn scroll_by(by: ScrollBy) -> Action {
    Action::Run {
        command: Command::Scroll(by),
        after: After::Stay,
    }
}

fn tab_keys() -> ModeKeys {
    let mut tab = keys(Unbound::Ignore, exits());
    tab.bindings.extend([
        (plain(b'h'), run(SessionCommand::PreviousTab, After::Stay)),
        (
            KeyChord::new(Key::Left, Mods::empty()),
            run(SessionCommand::PreviousTab, After::Stay),
        ),
        (plain(b'l'), run(SessionCommand::NextTab, After::Stay)),
        (
            KeyChord::new(Key::Right, Mods::empty()),
            run(SessionCommand::NextTab, After::Stay),
        ),
        (plain(b'n'), run(SessionCommand::NewTab, After::Base)),
        (plain(b'x'), run(SessionCommand::CloseTab, After::Base)),
        // Zellij's: the focused pane into a tab of its own, or the one
        // before or after.
        (plain(b'b'), run(SessionCommand::BreakPane, After::Base)),
        (
            plain(b'['),
            run(
                SessionCommand::MovePaneToTab {
                    side: TabSide::Previous,
                },
                After::Base,
            ),
        ),
        (
            plain(b']'),
            run(
                SessionCommand::MovePaneToTab {
                    side: TabSide::Next,
                },
                After::Base,
            ),
        ),
    ]);
    tab.bindings
        .extend((b'1'..=b'9').zip(NonZeroU16::MIN..).map(|(digit, number)| {
            (
                plain(digit),
                run(
                    SessionCommand::FocusTab {
                        tab: TabNumber::new(number),
                    },
                    After::Base,
                ),
            )
        }));
    tab
}
