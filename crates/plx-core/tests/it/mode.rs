use std::collections::BTreeMap;

use core::num::NonZeroU16;

use plx_core::command::{Command, SessionCommand};
use plx_core::geometry::Direction;
use plx_core::mode::{Keymap, KeymapError, Mode, ModeKeys, Resolution, Searching, Unbound};
use plx_core::tab::TabNumber;
use plx_emulator::{Key, KeyAction, KeyChar, KeyChord, KeyEvent, Mods};
use proptest::prelude::*;

use crate::strategy::{key_event, mods};

fn presets() -> [Keymap; 2] {
    [Keymap::unlock_first(), Keymap::classic()]
}

fn ctrl_g() -> KeyEvent {
    KeyEvent::press(Key::Char(KeyChar::new('g').unwrap()), Mods::CTRL)
}

/// The mode after `event`, as a client would track it.
fn next_mode(keymap: &Keymap, mode: Mode, event: &KeyEvent) -> Mode {
    match keymap.resolve(mode, event) {
        Resolution::Run { next, .. } | Resolution::Switch(next) => next,
        Resolution::PassThrough | Resolution::Ignore | Resolution::Input => mode,
    }
}

/// Presets are built through `Keymap::new`, which validates; deserialising
/// goes through it too, so a serialised preset round-trips and a keymap
/// with no way back is refused even when decoded.
#[test]
fn presets_are_valid_and_round_trip() {
    for keymap in presets() {
        let bytes = postcard::to_allocvec(&keymap).unwrap();
        assert_eq!(postcard::from_bytes::<Keymap>(&bytes).unwrap(), keymap);
    }
}

#[test]
fn a_mode_without_a_way_back_is_rejected() {
    let (_, mut tables) = Keymap::unlock_first().into_tables();
    tables.scroll = ModeKeys {
        bindings: BTreeMap::new(),
        unbound: Unbound::Ignore,
    };
    // Encoded as a keymap would be: the base mode, then the tables.
    let mut bytes = postcard::to_allocvec(&Mode::Locked).unwrap();
    bytes.extend(postcard::to_allocvec(&tables).unwrap());
    assert_eq!(
        Keymap::new(Mode::Locked, tables),
        Err(KeymapError::BaseUnreachable {
            mode: Mode::Scroll,
            base: Mode::Locked
        })
    );
    assert!(postcard::from_bytes::<Keymap>(&bytes).is_err());
}

#[test]
fn unlock_first_passes_everything_but_ctrl_g_while_locked() {
    let keymap = Keymap::unlock_first();
    assert_eq!(keymap.base(), Mode::Locked);
    assert_eq!(
        keymap.resolve(Mode::Locked, &ctrl_g()),
        Resolution::Switch(Mode::Normal)
    );
    assert_eq!(
        keymap.resolve(
            Mode::Locked,
            &KeyEvent::press(Key::Char(KeyChar::new('p').unwrap()), Mods::CTRL)
        ),
        Resolution::PassThrough
    );
}

proptest! {
    /// From every mode of either preset, at most two `Ctrl-g` get back to
    /// the base mode (classic locks first, as Zellij does, then unlocks).
    #[test]
    fn ctrl_g_always_leads_home(events in proptest::collection::vec(key_event(), 0..40)) {
        for keymap in presets() {
            let mut mode = keymap.base();
            for event in &events {
                mode = next_mode(&keymap, mode, event);
                let once = next_mode(&keymap, mode, &ctrl_g());
                let twice = next_mode(&keymap, once, &ctrl_g());
                prop_assert!(
                    mode == keymap.base() || once == keymap.base() || twice == keymap.base(),
                    "{mode:?} -> {once:?} -> {twice:?}"
                );
            }
        }
    }

    /// A command never leaves the client in some third mode: it stays or
    /// returns to base, except that a typed needle goes on to search mode.
    /// And releases never trigger anything.
    #[test]
    fn resolutions_are_well_formed(event in key_event()) {
        for keymap in presets() {
            for mode in Mode::ALL {
                match keymap.resolve(mode, &event) {
                    Resolution::Run { next, .. } => {
                        prop_assert!(
                            next == mode
                                || next == keymap.base()
                                || (mode, next) == (Mode::EnterSearch, Mode::Search)
                        );
                        prop_assert_ne!(event.action, KeyAction::Release);
                    }
                    Resolution::Switch(_) => prop_assert_ne!(event.action, KeyAction::Release),
                    Resolution::PassThrough | Resolution::Ignore | Resolution::Input => {}
                }
            }
        }
    }

    /// Shift on an ASCII letter is folded into the letter's case.
    #[test]
    fn shift_folds_into_letter_case(c in proptest::char::range('a', 'z'), mods in mods()) {
        let upper = c.to_ascii_uppercase();
        prop_assert_eq!(
            KeyChord::new(Key::Char(KeyChar::new(c).unwrap()), mods.with(Mods::SHIFT)),
            KeyChord::new(Key::Char(KeyChar::new(upper).unwrap()), mods.without(Mods::SHIFT))
        );
        prop_assert_eq!(
            KeyChord::new(Key::Char(KeyChar::new(upper).unwrap()), mods.with(Mods::SHIFT)),
            KeyChord::new(Key::Char(KeyChar::new(upper).unwrap()), mods.without(Mods::SHIFT))
        );
    }
}

fn chord(c: char, mods: Mods) -> KeyEvent {
    KeyEvent::press(Key::Char(KeyChar::new(c).unwrap()), mods)
}

fn runs<'k>(keymap: &'k Keymap, mode: Mode, event: &KeyEvent) -> (&'k SessionCommand, Mode) {
    match keymap.resolve(mode, event) {
        Resolution::Run {
            command: Command::Session(command),
            next,
        } => (command, next),
        other @ (Resolution::Run {
            command:
                Command::Client(_) | Command::Scroll(_) | Command::Search(_) | Command::EditScrollback,
            ..
        }
        | Resolution::Switch(_)
        | Resolution::PassThrough
        | Resolution::Ignore
        | Resolution::Input) => panic!("{event:?} in {mode:?}: {other:?}"),
    }
}

/// Where the classic preset's key for mode `target` goes from `from`: a
/// mode's own key goes back to normal, and scroll mode's leaves a search
/// too, as in Zellij.
fn classic_mode_key(from: Mode, target: Mode) -> Mode {
    if from == target || (target == Mode::Scroll && from.searching() == Searching::Yes) {
        Mode::Normal
    } else {
        target
    }
}

/// The classic preset follows the user's Zellij configuration.
#[test]
fn classic_is_zellijs_layout() {
    let keymap = Keymap::classic();
    assert_eq!(keymap.base(), Mode::Normal);
    // Mode keys work from any unlocked mode; a mode's own key goes back.
    let modes = [
        ('p', Mode::Pane),
        ('t', Mode::Tab),
        ('n', Mode::Resize),
        ('h', Mode::Move),
        ('s', Mode::Scroll),
        ('m', Mode::Session),
    ];
    for from in Mode::ALL.into_iter().filter(|&mode| mode != Mode::Locked) {
        for (letter, target) in modes {
            let expected = classic_mode_key(from, target);
            assert_eq!(
                keymap.resolve(from, &chord(letter, Mods::CTRL)),
                Resolution::Switch(expected),
                "Ctrl-{letter} in {from:?}"
            );
        }
        assert_eq!(
            keymap.resolve(from, &ctrl_g()),
            Resolution::Switch(Mode::Locked)
        );
        // Alt-<digit> goes to a tab, then normal mode.
        assert_eq!(
            runs(&keymap, from, &chord('3', Mods::ALT)),
            (
                &SessionCommand::FocusTab {
                    tab: TabNumber::new(NonZeroU16::new(3).unwrap())
                },
                Mode::Normal
            )
        );
        assert_eq!(
            runs(&keymap, from, &chord('n', Mods::ALT)).0,
            &SessionCommand::Split {
                direction: Direction::Down
            }
        );
        assert_eq!(
            runs(&keymap, from, &chord('m', Mods::ALT)).0,
            &SessionCommand::Split {
                direction: Direction::Right
            }
        );
        assert_eq!(
            runs(&keymap, from, &chord('l', Mods::ALT)).0,
            &SessionCommand::Focus {
                direction: Direction::Right
            }
        );
    }
    // Locked passes everything but Ctrl-g, Alt chords included.
    assert_eq!(
        keymap.resolve(Mode::Locked, &chord('1', Mods::ALT)),
        Resolution::PassThrough
    );
    assert_eq!(
        keymap.resolve(Mode::Locked, &ctrl_g()),
        Resolution::Switch(Mode::Normal)
    );
    // Normal passes plain keys to the program.
    assert_eq!(
        keymap.resolve(Mode::Normal, &chord('x', Mods::empty())),
        Resolution::PassThrough
    );
    // Pane mode: n down, m right, f fullscreen, x close, then normal.
    for (letter, command) in [
        (
            'n',
            SessionCommand::Split {
                direction: Direction::Down,
            },
        ),
        (
            'm',
            SessionCommand::Split {
                direction: Direction::Right,
            },
        ),
        ('f', SessionCommand::ToggleZoom),
        ('x', SessionCommand::ClosePane),
    ] {
        assert_eq!(
            runs(&keymap, Mode::Pane, &chord(letter, Mods::empty())),
            (&command, Mode::Normal),
            "{letter} in pane mode"
        );
    }
    // Focus moves stay in pane mode.
    assert_eq!(
        runs(&keymap, Mode::Pane, &chord('h', Mods::empty())),
        (
            &SessionCommand::Focus {
                direction: Direction::Left
            },
            Mode::Pane
        )
    );
    assert_eq!(
        runs(&keymap, Mode::Session, &chord('q', Mods::CTRL)),
        (&SessionCommand::KillSession, Mode::Normal)
    );
}

/// Scroll mode, as Zellij binds it, in both presets.
#[test]
fn scroll_mode_moves_through_the_history() {
    use plx_core::command::ScrollBy;
    for keymap in presets() {
        for (key, mods, by) in [
            (
                Key::Char(KeyChar::new('j').unwrap()),
                Mods::empty(),
                ScrollBy::LineDown,
            ),
            (
                Key::Char(KeyChar::new('k').unwrap()),
                Mods::empty(),
                ScrollBy::LineUp,
            ),
            (
                Key::Char(KeyChar::new('d').unwrap()),
                Mods::empty(),
                ScrollBy::HalfPageDown,
            ),
            (
                Key::Char(KeyChar::new('u').unwrap()),
                Mods::empty(),
                ScrollBy::HalfPageUp,
            ),
            (
                Key::Char(KeyChar::new('h').unwrap()),
                Mods::empty(),
                ScrollBy::PageUp,
            ),
            (
                Key::Char(KeyChar::new('l').unwrap()),
                Mods::empty(),
                ScrollBy::PageDown,
            ),
            (Key::PageUp, Mods::empty(), ScrollBy::PageUp),
            (Key::PageDown, Mods::empty(), ScrollBy::PageDown),
        ] {
            assert_eq!(
                keymap.resolve(Mode::Scroll, &KeyEvent::press(key, mods)),
                Resolution::Run {
                    command: &Command::Scroll(by),
                    next: Mode::Scroll
                },
                "{key:?}"
            );
        }
    }
}

/// Ctrl-Up and Ctrl-Down jump between the commands the shell ran: in
/// scroll mode, and in the classic preset's normal mode too.
#[test]
fn ctrl_arrows_jump_between_prompts() {
    use plx_core::command::ScrollBy;
    let up = KeyEvent::press(Key::Up, Mods::CTRL);
    let down = KeyEvent::press(Key::Down, Mods::CTRL);
    let classic = Keymap::classic();
    for (keymap, mode) in [
        (&classic, Mode::Normal),
        (&classic, Mode::Scroll),
        (&Keymap::unlock_first(), Mode::Scroll),
    ] {
        for (event, by) in [
            (&up, ScrollBy::PreviousPrompt),
            (&down, ScrollBy::NextPrompt),
        ] {
            assert_eq!(
                keymap.resolve(mode, event),
                Resolution::Run {
                    command: &Command::Scroll(by),
                    next: mode
                },
                "{event:?} in {mode:?}"
            );
        }
    }
}

/// The user's pane and tab moves: `Alt-Shift-h/j/k/l` move the pane,
/// `Alt-Shift-n` breaks it out, `Alt-i`/`Alt-o` move the tab; tab mode has
/// Zellij's `b`, `[` and `]`. Plain `Alt-h/j/k/l` still move the focus.
#[test]
fn panes_and_tabs_move_with_alt_shift_and_alt_i_o() {
    use plx_core::command::TabSide;
    use plx_core::geometry::Direction;
    let keymap = Keymap::classic();
    let shifted = |c| chord(c, Mods::ALT.with(Mods::SHIFT));
    assert_eq!(
        runs(&keymap, Mode::Normal, &shifted('l')),
        (
            &SessionCommand::MovePane {
                direction: Direction::Right
            },
            Mode::Normal
        )
    );
    assert_eq!(
        runs(&keymap, Mode::Normal, &chord('h', Mods::ALT)).0,
        &SessionCommand::Focus {
            direction: Direction::Left
        }
    );
    assert_eq!(
        runs(&keymap, Mode::Normal, &shifted('n')).0,
        &SessionCommand::BreakPane
    );
    assert_eq!(
        runs(&keymap, Mode::Normal, &chord('o', Mods::ALT)).0,
        &SessionCommand::MoveTab {
            side: TabSide::Next
        }
    );
    assert_eq!(
        runs(&keymap, Mode::Tab, &chord('[', Mods::empty())),
        (
            &SessionCommand::MovePaneToTab {
                side: TabSide::Previous
            },
            Mode::Normal
        )
    );
}
