use plx_emulator::{Emulator, Key, KeyAction, KeyChar, KeyEvent, Mods};
use proptest::prelude::*;

fn press(key: Key, mods: Mods) -> KeyEvent {
    KeyEvent::press(key, mods)
}

fn encode(emulator: &mut impl Emulator, key: KeyEvent) -> Vec<u8> {
    let mut out = Vec::new();
    emulator.encode_key(&key, &mut out);
    out
}

fn paste(emulator: &mut impl Emulator, text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    emulator.encode_paste(text, &mut out);
    out
}

#[test]
fn plain_and_control_keys_encode_like_xterm() {
    let mut e = crate::emulator(crate::size(80, 24));
    assert_eq!(
        encode(
            &mut e,
            press(Key::Char(KeyChar::new('a').unwrap()), Mods::empty())
        ),
        b"a"
    );
    assert_eq!(
        encode(
            &mut e,
            press(Key::Char(KeyChar::new('A').unwrap()), Mods::SHIFT)
        ),
        b"A"
    );
    assert_eq!(
        encode(
            &mut e,
            press(Key::Char(KeyChar::new('c').unwrap()), Mods::CTRL)
        ),
        b"\x03"
    );
    assert_eq!(
        encode(
            &mut e,
            press(Key::Char(KeyChar::new('x').unwrap()), Mods::ALT)
        ),
        b"\x1bx"
    );
    assert_eq!(encode(&mut e, press(Key::Enter, Mods::empty())), b"\r");
    assert_eq!(encode(&mut e, press(Key::Up, Mods::empty())), b"\x1b[A");
    assert_eq!(
        encode(
            &mut e,
            press(Key::Char(KeyChar::new('é').unwrap()), Mods::empty())
        ),
        "é".as_bytes()
    );
}

#[test]
fn keys_follow_the_panes_own_modes() {
    let mut e = crate::emulator(crate::size(80, 24));
    // Application cursor keys (DECCKM), as vim and less enable.
    e.feed(b"\x1b[?1h");
    assert_eq!(encode(&mut e, press(Key::Up, Mods::empty())), b"\x1bOA");
    // Kitty keyboard protocol, "disambiguate" flag: Ctrl+c becomes CSI u.
    e.feed(b"\x1b[>1u");
    assert_eq!(
        encode(
            &mut e,
            press(Key::Char(KeyChar::new('c').unwrap()), Mods::CTRL)
        ),
        b"\x1b[99;5u"
    );
}

#[test]
fn releases_produce_nothing_unless_the_program_asks() {
    let mut e = crate::emulator(crate::size(80, 24));
    let release = KeyEvent {
        key: Key::Char(KeyChar::new('a').unwrap()),
        mods: Mods::empty(),
        action: KeyAction::Release,
    };
    assert_eq!(encode(&mut e, release), b"");
}

#[test]
fn pastes_are_sanitised_and_bracketed_on_request() {
    let mut e = crate::emulator(crate::size(80, 24));
    assert_eq!(paste(&mut e, "ls\nrm\x1b[0m"), b"ls\rrm [0m");
    e.feed(b"\x1b[?2004h");
    assert_eq!(paste(&mut e, "a\nb"), b"\x1b[200~a\nb\x1b[201~");
}

proptest! {
    /// Encoding never fails and never emits a raw ESC for plain printable
    /// text without modifiers.
    #[test]
    fn printable_text_encodes_to_itself(c in proptest::char::range(' ', '~')) {
        let mut e = crate::emulator(crate::size(80, 24));
        let mut buf = [0_u8; 4];
        prop_assert_eq!(encode(&mut e, press(Key::Char(KeyChar::new(c).unwrap()), Mods::empty())), c.encode_utf8(&mut buf).as_bytes());
    }

    /// No pasted text can smuggle an escape sequence to the program.
    #[test]
    fn pastes_never_contain_escape(text in "\\PC*|[\\x00-\\x1f\\x7f]*", bracketed in any::<bool>()) {
        let mut e = crate::emulator(crate::size(80, 24));
        if bracketed {
            e.feed(b"\x1b[?2004h");
        }
        let encoded = paste(&mut e, &text);
        let body = if bracketed {
            prop_assert!(encoded.starts_with(b"\x1b[200~") && encoded.ends_with(b"\x1b[201~"));
            &encoded[6..encoded.len() - 6]
        } else {
            &encoded[..]
        };
        prop_assert!(!body.contains(&0x1b), "ESC in {body:?}");
    }
}

/// Mouse reports follow the modes the program set, in pane cells.
#[test]
fn mouse_reports_follow_the_programs_modes() {
    use plx_emulator::{
        ActiveScreen, Emulator, Mods, MouseAction, MouseButton, MouseEvent, MouseTracking, Position,
    };
    let mut emulator = crate::emulator(crate::size(20, 10));
    let event = |action, button| MouseEvent {
        action,
        button,
        position: Position { x: 2, y: 3 },
        mods: Mods::empty(),
    };
    let encode = |emulator: &mut plx_ghostty::GhosttyEmulator, event: MouseEvent| {
        let mut out = Vec::new();
        emulator.encode_mouse(&event, &mut out);
        String::from_utf8(out).unwrap()
    };
    // A program that never asked gets nothing.
    assert_eq!(emulator.mouse_tracking(), MouseTracking::Off);
    assert_eq!(
        encode(&mut emulator, event(MouseAction::Press, MouseButton::Left)),
        ""
    );
    // Clicks and the wheel, as SGR reports (one-based).
    emulator.feed(b"\x1b[?1000h\x1b[?1006h");
    assert_eq!(emulator.mouse_tracking(), MouseTracking::On);
    assert_eq!(
        encode(&mut emulator, event(MouseAction::Press, MouseButton::Left)),
        "\x1b[<0;3;4M"
    );
    assert_eq!(
        encode(
            &mut emulator,
            event(MouseAction::Release, MouseButton::Left)
        ),
        "\x1b[<0;3;4m"
    );
    assert_eq!(
        encode(
            &mut emulator,
            event(MouseAction::Press, MouseButton::WheelUp)
        ),
        "\x1b[<64;3;4M"
    );
    // Motion only once the program asks for button motion (1002).
    assert_eq!(
        encode(&mut emulator, event(MouseAction::Motion, MouseButton::Left)),
        ""
    );
    emulator.feed(b"\x1b[?1002h");
    assert_eq!(
        encode(&mut emulator, event(MouseAction::Motion, MouseButton::Left)),
        "\x1b[<32;3;4M"
    );
    assert_eq!(emulator.active_screen(), ActiveScreen::Primary);
    emulator.feed(b"\x1b[?1049h");
    assert_eq!(emulator.active_screen(), ActiveScreen::Alternate);
}

/// Selecting with the mouse: a drag selects what it covers and shows it in
/// inverse video; a double press selects a word, a triple one the line.
#[test]
fn the_mouse_selects_text() {
    use plx_emulator::{
        Attrs, Emulator, Past, Position, SelectAt, SelectGesture, Selection, Version,
    };
    let mut emulator = crate::emulator(crate::size(20, 4));
    emulator.feed(b"hello world\r\nsecond line");
    let at = |x, y| SelectAt {
        cell: Position { x, y },
        past: Past::Nothing,
    };
    let text = |emulator: &mut plx_ghostty::GhosttyEmulator| {
        let mut out = Vec::new();
        let selection = emulator.selection_text(&mut out);
        (selection, String::from_utf8(out).unwrap())
    };
    emulator.select(SelectGesture::Press, at(0, 0), 1_000_000_000);
    emulator.select(SelectGesture::Drag, at(4, 0), 1_100_000_000);
    emulator.select(SelectGesture::Release, at(4, 0), 1_200_000_000);
    assert_eq!(
        text(&mut emulator),
        (Selection::Some, String::from("hello"))
    );
    emulator.sync(Version::new(1)).unwrap();
    let screen = emulator.screen();
    let inverse: Vec<bool> = screen
        .grid()
        .rows()
        .next()
        .unwrap()
        .iter()
        .map(|&cell| screen.resolve(cell).style.attrs.contains(Attrs::INVERSE))
        .collect();
    assert_eq!(&inverse[..6], [true, true, true, true, true, false]);
    // A double press, soon after a press: the word.
    let start = 5_000_000_000;
    emulator.select(SelectGesture::Press, at(8, 0), start);
    emulator.select(SelectGesture::Release, at(8, 0), start + 10_000_000);
    emulator.select(SelectGesture::Press, at(8, 0), start + 20_000_000);
    assert_eq!(
        text(&mut emulator),
        (Selection::Some, String::from("world"))
    );
    // A third: the line.
    emulator.select(SelectGesture::Release, at(8, 0), start + 30_000_000);
    emulator.select(SelectGesture::Press, at(8, 0), start + 40_000_000);
    assert_eq!(
        text(&mut emulator),
        (Selection::Some, String::from("hello world"))
    );
    emulator.clear_selection();
    assert_eq!(text(&mut emulator), (Selection::None, String::new()));
    // A plain press later starts over: nothing selected yet.
    emulator.select(SelectGesture::Press, at(2, 1), start + 9_000_000_000);
    assert_eq!(text(&mut emulator).0, Selection::None);
}

/// As in Zellij: a triple press copies the whole line, even where it wraps
/// onto several rows; the copy joins the rows back into one line.
#[test]
fn a_triple_press_selects_a_wrapped_line_whole() {
    use plx_emulator::{Emulator, Past, Position, SelectAt, SelectGesture, Selection};
    let mut emulator = crate::emulator(crate::size(10, 4));
    // 16 characters on 10 columns: two rows, one line.
    emulator.feed(b"0123456789abcdef\r\nnext");
    let at = SelectAt {
        cell: Position { x: 3, y: 1 },
        past: Past::Nothing,
    };
    let start = 1_000_000_000;
    for (i, gesture) in [
        SelectGesture::Press,
        SelectGesture::Release,
        SelectGesture::Press,
        SelectGesture::Release,
        SelectGesture::Press,
    ]
    .into_iter()
    .enumerate()
    {
        emulator.select(gesture, at, start + 10_000_000 * u64::try_from(i).unwrap());
    }
    let mut out = Vec::new();
    assert_eq!(emulator.selection_text(&mut out), Selection::Some);
    assert_eq!(String::from_utf8(out).unwrap(), "0123456789abcdef");
}

/// A drag past the top edge scrolls the view back a row per tick, the
/// selection growing with it, so more than a screen can be copied; past the
/// bottom edge, forward again. Coming back inside, or letting go, stops it.
#[test]
fn a_drag_past_an_edge_scrolls_the_view_and_selects_along() {
    use plx_emulator::{Autoscroll, Emulator, Past, Position, SelectAt, SelectGesture, Selection};
    let mut emulator = crate::emulator(crate::size(20, 4));
    for n in 0..20 {
        emulator.feed(format!("line {n}\r\n").as_bytes());
    }
    // The screen: "line 17", "line 18", "line 19", and the empty last row.
    let at = |y, past| SelectAt {
        cell: Position { x: 0, y },
        past,
    };
    let text = |emulator: &mut plx_ghostty::GhosttyEmulator| {
        let mut out = Vec::new();
        assert_eq!(emulator.selection_text(&mut out), Selection::Some);
        String::from_utf8(out).unwrap()
    };
    let start = 1_000_000_000;
    let press = emulator.select(SelectGesture::Press, at(2, Past::Nothing), start);
    assert_eq!(press, Autoscroll::Stopped);
    let up = emulator.select(SelectGesture::Drag, at(0, Past::Top), start + 1);
    assert_eq!(up, Autoscroll::Scrolling);
    for _ in 0..5 {
        assert_eq!(emulator.autoscroll_selection(), Autoscroll::Scrolling);
    }
    assert_eq!(emulator.scroll_position().back, 5);
    assert_eq!(crate::view(&mut emulator).text()[0], "line 12");
    // From the row now on top to the press (going back from a press in
    // the middle of a cell leaves that cell out).
    assert_eq!(
        text(&mut emulator),
        "line 12\nline 13\nline 14\nline 15\nline 16\nline 17\nline 18"
    );
    // Past the bottom: forward to the live screen, where it can go no
    // further but still selects to the row under the pointer.
    let down = emulator.select(SelectGesture::Drag, at(3, Past::Bottom), start + 2);
    assert_eq!(down, Autoscroll::Scrolling);
    for _ in 0..6 {
        assert_eq!(emulator.autoscroll_selection(), Autoscroll::Scrolling);
    }
    assert_eq!(emulator.scroll_position().back, 0);
    assert_eq!(text(&mut emulator), "line 19");
    // Back inside: it stops, and the view stays.
    let inside = emulator.select(SelectGesture::Drag, at(1, Past::Nothing), start + 3);
    assert_eq!(inside, Autoscroll::Stopped);
    assert_eq!(emulator.autoscroll_selection(), Autoscroll::Stopped);
    // Past the top again, then let go: it stops.
    let up = emulator.select(SelectGesture::Drag, at(0, Past::Top), start + 4);
    assert_eq!(up, Autoscroll::Scrolling);
    let release = emulator.select(SelectGesture::Release, at(0, Past::Top), start + 5);
    assert_eq!(release, Autoscroll::Stopped);
    assert_eq!(emulator.autoscroll_selection(), Autoscroll::Stopped);
    assert_eq!(emulator.scroll_position().back, 0);
}

/// A program that asks for no cursor shape gets the user's own: the
/// terminal reports `Default` until a shape is asked for, and again after
/// a reset.
#[test]
fn the_cursor_is_the_users_own_until_a_program_asks() {
    use plx_emulator::{CursorBlink, CursorShape};
    let mut emulator = crate::emulator(crate::size(20, 4));
    let shape = |emulator: &mut plx_ghostty::GhosttyEmulator| crate::view(emulator).cursor.shape;
    assert_eq!(shape(&mut emulator), CursorShape::Default);
    emulator.feed(b"\x1b[6 q");
    assert_eq!(shape(&mut emulator), CursorShape::Bar);
    assert_eq!(crate::view(&mut emulator).cursor.blink, CursorBlink::Steady);
    emulator.feed(b"\x1b[3 q");
    assert_eq!(shape(&mut emulator), CursorShape::Underline);
    emulator.feed(b"\x1b[2 q");
    assert_eq!(shape(&mut emulator), CursorShape::Block);
    emulator.feed(b"\x1b[0 q");
    assert_eq!(shape(&mut emulator), CursorShape::Default);
}

/// A character Shift types (`:`, `A`) reaches a program using the kitty
/// keyboard protocol's "disambiguate" flag (Neovim) as its text, as it
/// reaches any other; asked for every key as an escape code, the program
/// gets the key, Shift and the text.
#[test]
fn shifted_characters_reach_kitty_programs_as_text() {
    let char_key = |c, mods| press(Key::Char(KeyChar::new(c).unwrap()), mods);
    let mut e = crate::emulator(crate::size(80, 24));
    e.feed(b"\x1b[>1u");
    for mods in [Mods::empty(), Mods::SHIFT] {
        assert_eq!(encode(&mut e, char_key(':', mods)), b":");
        assert_eq!(encode(&mut e, char_key('A', mods)), b"A");
    }
    // Other modifiers still make an escape code.
    assert_eq!(encode(&mut e, char_key('c', Mods::CTRL)), b"\x1b[99;5u");
    let mut e = crate::emulator(crate::size(80, 24));
    e.feed(b"\x1b[>31u");
    assert_eq!(
        encode(&mut e, char_key(':', Mods::empty())),
        b"\x1b[59:58;2;58u"
    );
}
