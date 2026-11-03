//! A proptest grammar for program output: mostly meaningful VT sequences,
//! plus raw garbage, so tests exercise both real-world and hostile streams.

use proptest::collection::vec;
use proptest::prelude::*;

use plx_emulator::GridSize;
use plx_ghostty::{GraphemeClustering, Options};

/// Grid sizes small enough to keep cases fast but including 1-column and
/// 1-row edge cases.
pub(crate) fn grid_size() -> impl Strategy<Value = GridSize> {
    (1_u16..=40, 1_u16..=12).prop_map(|(cols, rows)| crate::size(cols, rows))
}

/// Terminal settings; grapheme clustering is varied because it changes cell
/// layout throughout.
pub(crate) fn options() -> impl Strategy<Value = Options> {
    prop_oneof![
        Just(GraphemeClustering::Enabled),
        Just(GraphemeClustering::Disabled)
    ]
    .prop_map(crate::options)
}

/// A stream of program output.
pub(crate) fn stream() -> impl Strategy<Value = Vec<u8>> {
    vec(token(), 0..60).prop_map(|tokens| tokens.concat())
}

/// `bytes` cut into consecutive chunks at arbitrary points (some empty).
pub(crate) fn chunked(bytes: Vec<u8>) -> impl Strategy<Value = Vec<Vec<u8>>> {
    let len = bytes.len();
    vec(0..=len, 0..8).prop_map(move |mut cuts| {
        cuts.sort_unstable();
        let mut chunks = Vec::with_capacity(cuts.len() + 1);
        let mut start = 0;
        for cut in cuts {
            chunks.push(bytes[start..cut].to_vec());
            start = cut;
        }
        chunks.push(bytes[start..].to_vec());
        chunks
    })
}

fn token() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        6 => text(),
        2 => prop::sample::select(vec![&b"\r"[..], b"\n", b"\r\n", b"\x08", b"\t", b"\x07"]).prop_map(<[u8]>::to_vec),
        2 => cursor_motion(),
        2 => sgr(),
        1 => erase(),
        1 => scrolling(),
        1 => mode(),
        1 => query(),
        1 => osc(),
        1 => vec(any::<u8>(), 1..8),
    ]
}

fn text() -> impl Strategy<Value = Vec<u8>> {
    let piece = prop_oneof![
        8 => "[ -~]{1,12}",
        1 => prop::sample::select(vec![
            "é", "ß", "漢字", "한글", "😀", "👩‍💻", "e\u{301}", "🇫🇮", "\u{200b}", "ｱ",
        ])
        .prop_map(str::to_owned),
    ];
    piece.prop_map(String::into_bytes)
}

fn cursor_motion() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        (0_u16..20, 0_u16..50).prop_map(|(row, col)| format!("\x1b[{row};{col}H")),
        (
            prop::sample::select(vec!['A', 'B', 'C', 'D', 'E', 'F', 'G']),
            0_u16..60
        )
            .prop_map(|(op, n)| format!("\x1b[{n}{op}")),
        Just("\x1b7".to_owned()),
        Just("\x1b8".to_owned()),
    ]
    .prop_map(String::into_bytes)
}

fn sgr() -> impl Strategy<Value = Vec<u8>> {
    let param = prop_oneof![
        prop::sample::select(vec![
            0_u16, 1, 2, 3, 4, 5, 7, 8, 9, 21, 22, 23, 24, 27, 29, 39, 49, 53, 55
        ])
        .prop_map(|p| p.to_string()),
        (30_u16..38).prop_map(|p| p.to_string()),
        (90_u16..98).prop_map(|p| p.to_string()),
        any::<u8>().prop_map(|n| format!("38;5;{n}")),
        any::<(u8, u8, u8)>().prop_map(|(r, g, b)| format!("48;2;{r};{g};{b}")),
        (0_u8..6).prop_map(|n| format!("4:{n}")),
        any::<u8>().prop_map(|n| format!("58;5;{n}")),
    ];
    vec(param, 0..4).prop_map(|params| format!("\x1b[{}m", params.join(";")).into_bytes())
}

fn erase() -> impl Strategy<Value = Vec<u8>> {
    (
        prop::sample::select(vec!['J', 'K', 'X', 'P', '@', 'L', 'M']),
        0_u8..4,
    )
        .prop_map(|(op, n)| format!("\x1b[{n}{op}").into_bytes())
}

fn scrolling() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        (0_u16..14, 0_u16..14).prop_map(|(top, bottom)| format!("\x1b[{top};{bottom}r")),
        Just("\x1bD".to_owned()),
        Just("\x1bM".to_owned()),
        (0_u8..5).prop_map(|n| format!("\x1b[{n}S")),
        (0_u8..5).prop_map(|n| format!("\x1b[{n}T")),
    ]
    .prop_map(String::into_bytes)
}

fn mode() -> impl Strategy<Value = Vec<u8>> {
    (
        prop::sample::select(vec![25_u16, 1049, 47, 7, 6, 2004, 1, 2026, 2027]),
        any::<bool>(),
    )
        .prop_map(|(mode, set)| format!("\x1b[?{mode}{}", if set { 'h' } else { 'l' }).into_bytes())
}

fn query() -> impl Strategy<Value = Vec<u8>> {
    prop::sample::select(vec![
        &b"\x1b[c"[..],
        b"\x1b[>c",
        b"\x1b[5n",
        b"\x1b[6n",
        b"\x1b[>q",
        b"\x1b[?25$p",
    ])
    .prop_map(<[u8]>::to_vec)
}

fn osc() -> impl Strategy<Value = Vec<u8>> {
    let title = "[a-z ]{0,10}".prop_map(|title| format!("2;{title}"));
    // Progress reports, some with states or percentages past the protocol's.
    let progress = (0_u8..6, proptest::option::of(0_u8..120)).prop_map(|(state, percent)| {
        percent.map_or_else(
            || format!("9;4;{state}"),
            |percent| format!("9;4;{state};{percent}"),
        )
    });
    (prop_oneof![title, progress], any::<bool>()).prop_map(|(body, bel)| {
        let terminator = if bel { "\x07" } else { "\x1b\\" };
        format!("\x1b]{body}{terminator}").into_bytes()
    })
}
