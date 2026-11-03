use plx_client::keys::key_event;
use plx_emulator::{FKey, Key, KeyAction, KeyChar, KeyEvent, Mods};
use proptest::prelude::*;
use termina::Event;
use termina::Parser;

/// The plx key events in `bytes`, parsed in the given chunks.
fn keys(chunks: &[&[u8]]) -> Vec<KeyEvent> {
    let mut parser = Parser::default();
    let mut out = Vec::new();
    for (i, chunk) in chunks.iter().enumerate() {
        parser.parse(chunk, i + 1 < chunks.len());
        while let Some(event) = parser.pop() {
            if let Event::Key(key) = event
                && let Some(key) = key_event(&key)
            {
                out.push(key);
            }
        }
    }
    out
}

fn press(key: Key, mods: Mods) -> KeyEvent {
    KeyEvent {
        key,
        mods,
        action: KeyAction::Press,
    }
}

fn ch(c: char) -> Key {
    Key::Char(KeyChar::new(c).unwrap())
}

#[test]
fn what_terminals_send_maps_to_plx_keys() {
    let cases: &[(&[u8], KeyEvent)] = &[
        (b"a", press(ch('a'), Mods::empty())),
        (b"\r", press(Key::Enter, Mods::empty())),
        (b"\t", press(Key::Tab, Mods::empty())),
        (b"\x7f", press(Key::Backspace, Mods::empty())),
        (b"\x1b[A", press(Key::Up, Mods::empty())),
        (b"\x1b[1;5A", press(Key::Up, Mods::CTRL)),
        (b"\x1b[1;3D", press(Key::Left, Mods::ALT)),
        (b"\x1b[Z", press(Key::Tab, Mods::SHIFT)),
        (
            b"\x1bOP",
            press(Key::F(FKey::new(1).unwrap()), Mods::empty()),
        ),
        (
            b"\x1b[15~",
            press(Key::F(FKey::new(5).unwrap()), Mods::empty()),
        ),
        (b"\x1b[3~", press(Key::Delete, Mods::empty())),
        // The kitty keyboard protocol: Ctrl+a, unambiguous.
        (b"\x1b[97;5u", press(ch('a'), Mods::CTRL)),
        // The classic keymap's chords. Ctrl-m and Ctrl-h are Enter and
        // Backspace in legacy encoding; the kitty protocol tells them apart.
        (b"\x1b[109;5u", press(ch('m'), Mods::CTRL)),
        (b"\x1b[104;5u", press(ch('h'), Mods::CTRL)),
        (b"\x10", press(ch('p'), Mods::CTRL)),
        (b"\x14", press(ch('t'), Mods::CTRL)),
        (b"\x1b1", press(ch('1'), Mods::ALT)),
        (b"\x1b[49;3u", press(ch('1'), Mods::ALT)),
        (b"\x1bm", press(ch('m'), Mods::ALT)),
    ];
    for &(bytes, expected) in cases {
        assert_eq!(keys(&[bytes]), [expected], "{bytes:?}");
    }
}

#[test]
fn kitty_reports_repeats_and_releases() {
    let [repeat, release] = [&b"\x1b[97;1:2u"[..], b"\x1b[97;1:3u"].map(|bytes| keys(&[bytes]));
    assert_eq!(
        repeat,
        [KeyEvent {
            key: ch('a'),
            mods: Mods::empty(),
            action: KeyAction::Repeat
        }]
    );
    assert_eq!(
        release,
        [KeyEvent {
            key: ch('a'),
            mods: Mods::empty(),
            action: KeyAction::Release
        }]
    );
}

#[test]
fn keys_plx_has_no_use_for_are_dropped() {
    // Caps Lock in the kitty protocol (57358).
    assert_eq!(keys(&[b"\x1b[57358u"]), []);
}

proptest! {
    /// Input cut anywhere parses to the same keys.
    #[test]
    fn input_split_anywhere_gives_the_same_keys(
        pieces in proptest::collection::vec(prop_oneof![
            Just(&b"a"[..]), Just(&b"\x1b[A"[..]), Just(&b"\x1b[1;5C"[..]), Just(&b"\x1b[97;5u"[..]),
            Just(&b"\x1bOQ"[..]), Just(&b"\r"[..]), Just(&b"\x1b[200~pasted\x1b[201~"[..]),
        ], 1..12),
        cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..6),
    ) {
        let bytes: Vec<u8> = pieces.concat();
        let mut at: Vec<usize> = cuts.iter().map(|cut| cut.index(bytes.len() + 1)).collect();
        at.sort_unstable();
        at.dedup();
        let mut chunks = Vec::new();
        let mut start = 0;
        for &cut in &at {
            chunks.push(&bytes[start..cut]);
            start = cut;
        }
        chunks.push(&bytes[start..]);
        prop_assert_eq!(keys(&chunks), keys(&[&bytes]));
    }
}

/// SGR mouse reports (what the client turns on) map to plx's events, in
/// zero-based cells.
#[test]
fn mouse_reports_map_to_plx_events() {
    use plx_client::keys::mouse_event;
    use plx_emulator::{MouseAction, MouseButton, MouseEvent, Position};
    let parse = |bytes: &[u8]| {
        let mut parser = Parser::default();
        parser.parse(bytes, false);
        match parser.pop() {
            Some(Event::Mouse(mouse)) => mouse_event(&mouse),
            other => panic!("{bytes:?}: {other:?}"),
        }
    };
    let at = |action, button, x, y, mods| MouseEvent {
        action,
        button,
        position: Position { x, y },
        mods,
    };
    let cases: [(&[u8], MouseEvent); 6] = [
        (
            b"\x1b[<0;3;4M",
            at(MouseAction::Press, MouseButton::Left, 2, 3, Mods::empty()),
        ),
        (
            b"\x1b[<0;3;4m",
            at(MouseAction::Release, MouseButton::Left, 2, 3, Mods::empty()),
        ),
        (
            b"\x1b[<32;9;4M",
            at(MouseAction::Motion, MouseButton::Left, 8, 3, Mods::empty()),
        ),
        (
            b"\x1b[<2;1;1M",
            at(MouseAction::Press, MouseButton::Right, 0, 0, Mods::empty()),
        ),
        (
            b"\x1b[<64;5;6M",
            at(
                MouseAction::Press,
                MouseButton::WheelUp,
                4,
                5,
                Mods::empty(),
            ),
        ),
        (
            b"\x1b[<65;5;6M",
            at(
                MouseAction::Press,
                MouseButton::WheelDown,
                4,
                5,
                Mods::empty(),
            ),
        ),
    ];
    for (bytes, expected) in cases {
        assert_eq!(parse(bytes), expected, "{bytes:?}");
    }
}
