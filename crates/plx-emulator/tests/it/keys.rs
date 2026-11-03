use plx_emulator::{
    FKey, FunctionKeyCode, Key, KeyChar, KeyChord, Mods, NoSuchFunctionKey, UnknownMods,
};

#[test]
fn key_chars_exclude_exactly_the_platform_function_key_codes() {
    for c in ['\u{F700}', '\u{F7FF}', '\u{F8FF}'] {
        assert_eq!(KeyChar::new(c), Err(FunctionKeyCode(c)));
    }
    for c in ['a', '\u{1b}', '\u{F6FF}', '\u{F900}', '😀'] {
        assert_eq!(KeyChar::new(c).map(KeyChar::get), Ok(c));
    }
    assert!(serde_json::from_str::<KeyChar>("\"\u{F700}\"").is_err());
}

#[test]
fn function_keys_run_from_f1_to_f35() {
    assert_eq!(FKey::new(0), Err(NoSuchFunctionKey(0)));
    assert_eq!(FKey::new(36), Err(NoSuchFunctionKey(36)));
    assert_eq!(FKey::new(1).map(|k| k.get().get()), Ok(1));
    assert_eq!(FKey::new(FKey::MAX).map(|k| k.get().get()), Ok(35));
    assert!(serde_json::from_str::<FKey>("36").is_err());
}

#[test]
fn modifiers_reject_unknown_bits() {
    let all = Mods::SHIFT
        .with(Mods::ALT)
        .with(Mods::CTRL)
        .with(Mods::SUPER);
    assert_eq!(serde_json::from_str::<Mods>("15").unwrap(), all);
    assert_eq!(Mods::try_from(16), Err(UnknownMods(16)));
    assert!(serde_json::from_str::<Mods>("16").is_err());
}

/// A chord read from outside is canonical, like one built with `new`.
#[test]
fn deserialised_chords_are_canonical() {
    let shift = serde_json::to_string(&Mods::SHIFT).unwrap();
    let key = serde_json::to_string(&Key::Char(KeyChar::from_byte(b'a'))).unwrap();
    let raw = format!(r#"{{"key":{key},"mods":{shift}}}"#);
    let chord: KeyChord = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        chord,
        KeyChord::new(Key::Char(KeyChar::from_byte(b'A')), Mods::empty())
    );
    assert_eq!(chord.mods(), Mods::empty());
}

#[test]
fn cell_parts_deserialise_only_what_their_constructors_accept() {
    use plx_emulator::{Attrs, GraphemeId};
    assert!(serde_json::from_str::<Attrs>("255").is_ok());
    assert!(serde_json::from_str::<Attrs>("256").is_err());
    assert_eq!(
        serde_json::from_str::<GraphemeId>("2147483647").unwrap(),
        GraphemeId::new(GraphemeId::MAX).unwrap()
    );
    assert!(serde_json::from_str::<GraphemeId>("2147483648").is_err());
}

#[test]
fn cells_round_trip_through_their_wire_form() {
    use plx_emulator::{Cell, Glyph, GraphemeId, InvalidCell, StyleId, Width};
    let cells = [
        Cell::BLANK,
        Cell {
            glyph: Glyph::from_char('é'),
            style: StyleId::MAX,
            width: Width::Wide,
        },
        Cell {
            glyph: Glyph::from_cluster(GraphemeId::new(GraphemeId::MAX).unwrap()),
            style: StyleId::new(70_000).unwrap(),
            width: Width::SpacerHead,
        },
    ];
    for cell in cells {
        assert_eq!(Cell::from_bytes(cell.to_bytes()), Ok(cell));
    }
    let bad_width = [0, 0, 0, 0, 0, 0, 0, 4];
    assert_eq!(Cell::from_bytes(bad_width), Err(InvalidCell(bad_width)));
    // A surrogate is no glyph.
    let bad_glyph = [0x00, 0xD8, 0, 0, 0, 0, 0, 0];
    assert_eq!(Cell::from_bytes(bad_glyph), Err(InvalidCell(bad_glyph)));
}
