//! A cell's memory form is its wire form: the borrowed view `slice_as_wire`
//! must agree with the safe definition, `to_bytes`, for every cell.

use plx_emulator::{Cell, Glyph, GraphemeId, StyleId, Width};
use proptest::prelude::*;

fn cell() -> impl Strategy<Value = Cell> {
    let glyph = prop_oneof![
        Just(Glyph::EMPTY),
        any::<char>().prop_map(Glyph::from_char),
        (0..=GraphemeId::MAX).prop_map(|id| Glyph::from_cluster(GraphemeId::new(id).unwrap())),
    ];
    let style = (0_u32..1 << 24).prop_map(|id| StyleId::new(id).unwrap());
    let width = prop_oneof![
        Just(Width::Narrow),
        Just(Width::Wide),
        Just(Width::SpacerTail),
        Just(Width::SpacerHead),
    ];
    (glyph, style, width).prop_map(|(glyph, style, width)| Cell {
        glyph,
        style,
        width,
    })
}

proptest! {
    #[test]
    fn the_borrowed_wire_view_is_to_bytes(cells in proptest::collection::vec(cell(), 0..300)) {
        let expected: Vec<u8> = cells.iter().flat_map(|cell| cell.to_bytes()).collect();
        prop_assert_eq!(Cell::slice_as_wire(&cells), &expected[..]);
        // And it reads back as the same cells.
        let (wire, partial) = Cell::slice_as_wire(&cells).as_chunks::<{ Cell::BYTES }>();
        prop_assert!(partial.is_empty());
        let back: Vec<Cell> = wire.iter().map(|&bytes| Cell::from_bytes(bytes).unwrap()).collect();
        prop_assert_eq!(back, cells);
    }
}

#[test]
fn widths_are_their_wire_bytes() {
    for (width, index) in [
        (Width::Narrow, 0),
        (Width::Wide, 1),
        (Width::SpacerTail, 2),
        (Width::SpacerHead, 3),
    ] {
        assert_eq!(width.index(), index);
        assert_eq!(Width::from_index_or_none(index), Some(width));
    }
    assert_eq!(Width::from_index_or_none(4), None);
}

proptest! {
    /// Valid rows copy in as exactly the cells `from_bytes` makes of them.
    #[test]
    fn copying_wire_forms_in_is_from_bytes(cells in proptest::collection::vec(cell(), 0..300)) {
        let wire: Vec<[u8; Cell::BYTES]> = cells.iter().map(|cell| cell.to_bytes()).collect();
        let mut row = vec![Cell::BLANK; cells.len()];
        prop_assert_eq!(Cell::copy_from_wire(&mut row, &wire), Ok(()));
        prop_assert_eq!(row, cells);
    }

    /// Any bytes: accepted exactly when `from_bytes` accepts every wire form,
    /// refused with the first invalid one, and then nothing is written.
    #[test]
    fn copying_any_bytes_in_agrees_with_from_bytes(
        wire in proptest::collection::vec(any::<[u8; Cell::BYTES]>(), 0..40),
        widths_valid in any::<bool>(),
    ) {
        // Half the cases keep every width valid, so glyph checks are hit.
        let wire: Vec<[u8; Cell::BYTES]> = wire
            .into_iter()
            .map(|mut bytes| {
                if widths_valid {
                    bytes[7] %= 4;
                }
                bytes
            })
            .collect();
        let expected: Result<Vec<Cell>, _> = wire.iter().map(|&bytes| Cell::from_bytes(bytes)).collect();
        let mut row = vec![Cell::BLANK; wire.len()];
        match expected {
            Ok(cells) => {
                prop_assert_eq!(Cell::copy_from_wire(&mut row, &wire), Ok(()));
                prop_assert_eq!(row, cells);
            }
            Err(first) => {
                prop_assert_eq!(Cell::copy_from_wire(&mut row, &wire), Err(first));
                prop_assert!(row.iter().all(|&cell| cell == Cell::BLANK));
            }
        }
    }
}

#[test]
fn glyph_validity_has_its_edges_where_unicode_does() {
    let wire = |glyph: u32| {
        let [g0, g1, g2, g3] = glyph.to_le_bytes();
        [g0, g1, g2, g3, 0, 0, 0, 0]
    };
    for glyph in [0, 0xD7FF, 0xE000, 0x10_FFFF, 1 << 31, u32::MAX] {
        assert!(Cell::from_bytes(wire(glyph)).is_ok(), "{glyph:#x}");
    }
    for glyph in [0xD800, 0xDFFF, 0x11_0000, (1 << 31) - 1] {
        assert!(Cell::from_bytes(wire(glyph)).is_err(), "{glyph:#x}");
    }
}

/// The glyphs and width bytes at the edges of validity.
const EDGE_GLYPHS: [u32; 10] = [
    0,
    0xD7FF,
    0xD800,
    0xDFFF,
    0xE000,
    0x10_FFFF,
    0x11_0000,
    0x7FFF_FFFF,
    0x8000_0000,
    u32::MAX,
];

proptest! {
    /// A valid row with one edge-case cell put anywhere (inside a group of
    /// four, or in the tail): the bulk check agrees with `from_bytes`, cell
    /// by cell.
    #[test]
    fn one_edge_cell_anywhere_agrees_with_from_bytes(
        cells in proptest::collection::vec(cell(), 1..40),
        at in any::<proptest::sample::Index>(),
        glyph in proptest::sample::select(EDGE_GLYPHS.to_vec()),
        width in 0_u8..6,
    ) {
        let mut wire: Vec<[u8; Cell::BYTES]> = cells.iter().map(|cell| cell.to_bytes()).collect();
        let at = at.index(wire.len());
        let [g0, g1, g2, g3] = glyph.to_le_bytes();
        wire[at][..4].copy_from_slice(&[g0, g1, g2, g3]);
        wire[at][7] = width;
        let expected: Result<Vec<Cell>, _> = wire.iter().map(|&bytes| Cell::from_bytes(bytes)).collect();
        let mut row = vec![Cell::BLANK; wire.len()];
        match expected {
            Ok(cells) => {
                prop_assert_eq!(Cell::copy_from_wire(&mut row, &wire), Ok(()));
                prop_assert_eq!(row, cells);
            }
            Err(first) => {
                prop_assert_eq!(Cell::copy_from_wire(&mut row, &wire), Err(first));
                prop_assert!(row.iter().all(|&cell| cell == Cell::BLANK));
            }
        }
    }
}

/// The plain rule, spelled out independently of `Cell::is_plain`.
fn plain_by_definition(cell: Cell, style: StyleId) -> bool {
    use plx_emulator::GlyphKind;
    cell.width == Width::Narrow
        && cell.style == style
        && match cell.glyph.kind() {
            GlyphKind::Empty => true,
            GlyphKind::Char(c) => (' '..='~').contains(&c),
            GlyphKind::Cluster(_) => false,
        }
}

/// Plain cells in `style`, mostly.
fn plain_cell(style: StyleId) -> impl Strategy<Value = Cell> {
    prop_oneof![
        Just(Glyph::EMPTY),
        (0x20_u8..0x7F).prop_map(|byte| Glyph::from_char(char::from(byte))),
    ]
    .prop_map(move |glyph| Cell {
        glyph,
        style,
        width: Width::Narrow,
    })
}

/// A cell at the edge of plainness.
fn edge_cell(style: StyleId) -> impl Strategy<Value = Cell> {
    let near_style = StyleId::new(style.get() ^ 1).unwrap();
    let top_style = StyleId::new(style.get() ^ 0x80_0000).unwrap();
    prop_oneof![
        proptest::sample::select(vec![
            0x1F_u32, 0x20, 0x7E, 0x7F, 0x80, 0xFF, 0x100, 0x10_FFFF
        ])
        .prop_map(move |c| Cell {
            glyph: Glyph::from_char(char::from_u32(c).unwrap()),
            style,
            width: Width::Narrow
        }),
        Just(Cell {
            glyph: Glyph::from_cluster(GraphemeId::new(0).unwrap()),
            style,
            width: Width::Narrow
        }),
        Just(Cell {
            glyph: Glyph::from_char('a'),
            style,
            width: Width::Wide
        }),
        Just(Cell {
            glyph: Glyph::EMPTY,
            style,
            width: Width::SpacerTail
        }),
        Just(Cell {
            glyph: Glyph::from_char('a'),
            style: near_style,
            width: Width::Narrow
        }),
        Just(Cell {
            glyph: Glyph::from_char('a'),
            style: top_style,
            width: Width::Narrow
        }),
    ]
}

proptest! {
    /// The run is exactly the leading cells the rule calls plain, wherever
    /// the first non-plain cell falls (inside a group of eight or after).
    #[test]
    fn plain_runs_end_at_the_first_cell_that_is_not_plain(
        (style, mut row, breaks) in (0_u32..1 << 24).prop_flat_map(|id| {
            let style = StyleId::new(id).unwrap();
            (
                Just(style),
                proptest::collection::vec(plain_cell(style), 0..40),
                proptest::collection::vec(
                    (any::<proptest::sample::Index>(), prop_oneof![edge_cell(style), plain_cell(style)]),
                    0..3,
                ),
            )
        }),
    ) {
        for (at, cell) in breaks {
            if !row.is_empty() {
                let at = at.index(row.len());
                row[at] = cell;
            }
        }
        let expected = row.iter().take_while(|&&cell| plain_by_definition(cell, style)).count();
        prop_assert_eq!(Cell::plain_run(&row, style), expected);
        for &cell in &row {
            prop_assert_eq!(cell.is_plain(style), plain_by_definition(cell, style));
        }
    }

    /// A plain cell is written as its character, a blank as a space.
    #[test]
    fn plain_cells_are_their_byte(cell in plain_cell(StyleId::DEFAULT)) {
        use plx_emulator::GlyphKind;
        let expected = match cell.glyph.kind() {
            GlyphKind::Empty => b' ',
            GlyphKind::Char(c) => u8::try_from(c).unwrap(),
            GlyphKind::Cluster(_) => return Err(TestCaseError::fail("plain cells hold no clusters")),
        };
        prop_assert_eq!(cell.plain_byte(), expected);
    }
}

/// Every edge cell at every position of a row of three groups of eight:
/// exhaustive, so the bulk scans cannot miss a lane or an edge by chance.
#[test]
fn plain_runs_stop_at_every_edge_in_every_lane() {
    let style = StyleId::new(0x12_3456).unwrap();
    let plain = Cell {
        glyph: Glyph::from_char('x'),
        style,
        width: Width::Narrow,
    };
    let edges: Vec<Cell> = [0x1F_u32, 0x7F, 0x80, 0xFF, 0x100, 0x10_FFFF]
        .into_iter()
        .map(|c| Cell {
            glyph: Glyph::from_char(char::from_u32(c).unwrap()),
            style,
            width: Width::Narrow,
        })
        .chain([
            Cell {
                glyph: Glyph::from_cluster(GraphemeId::new(0).unwrap()),
                style,
                width: Width::Narrow,
            },
            Cell {
                glyph: Glyph::from_char('x'),
                style,
                width: Width::Wide,
            },
            Cell {
                glyph: Glyph::from_char('x'),
                style,
                width: Width::SpacerTail,
            },
            Cell {
                glyph: Glyph::from_char('x'),
                style,
                width: Width::SpacerHead,
            },
            Cell {
                glyph: Glyph::from_char('x'),
                style: StyleId::new(0x12_3457).unwrap(),
                width: Width::Narrow,
            },
            Cell {
                glyph: Glyph::from_char('x'),
                style: StyleId::new(0x92_3456).unwrap(),
                width: Width::Narrow,
            },
            Cell {
                glyph: Glyph::from_char('x'),
                style: StyleId::DEFAULT,
                width: Width::Narrow,
            },
        ])
        .collect();
    let still_plain = [
        Cell {
            glyph: Glyph::EMPTY,
            style,
            width: Width::Narrow,
        },
        Cell {
            glyph: Glyph::from_char(' '),
            style,
            width: Width::Narrow,
        },
        Cell {
            glyph: Glyph::from_char('~'),
            style,
            width: Width::Narrow,
        },
    ];
    for len in [0, 1, 7, 8, 9, 16, 24] {
        let row = vec![plain; len];
        assert_eq!(Cell::plain_run(&row, style), len);
    }
    for at in 0..24 {
        for &edge in &edges {
            let mut row = vec![plain; 24];
            row[at] = edge;
            assert_eq!(Cell::plain_run(&row, style), at, "{edge:?} at {at}");
            assert!(!edge.is_plain(style), "{edge:?}");
        }
        for &cell in &still_plain {
            let mut row = vec![plain; 24];
            row[at] = cell;
            assert_eq!(Cell::plain_run(&row, style), 24, "{cell:?} at {at}");
        }
    }
}

/// Every invalid glyph and width edge at every position of a row of three
/// groups of eight, and every valid edge, through the bulk wire check.
#[test]
fn wire_checks_catch_every_edge_in_every_lane() {
    let valid = Cell {
        glyph: Glyph::from_char('x'),
        style: StyleId::MAX,
        width: Width::Narrow,
    }
    .to_bytes();
    let with = |glyph: u32, width: u8| {
        let mut bytes = valid;
        bytes[..4].copy_from_slice(&glyph.to_le_bytes());
        bytes[7] = width;
        bytes
    };
    let invalid = [
        with(0xD800, 0),
        with(0xDFFF, 0),
        with(0x11_0000, 0),
        with(0x7FFF_FFFF, 0),
        with(u32::from('x'), 4),
        with(u32::from('x'), 0xFF),
        with(0x8000_0000, 4),
    ];
    let accepted = [
        with(0, 3),
        with(0xD7FF, 0),
        with(0xE000, 1),
        with(0x10_FFFF, 2),
        with(0x8000_0000, 0),
        with(u32::MAX, 3),
    ];
    for at in 0..24 {
        for bad in invalid {
            let mut wire = vec![valid; 24];
            wire[at] = bad;
            let mut row = vec![Cell::BLANK; 24];
            assert_eq!(
                Cell::copy_from_wire(&mut row, &wire),
                Err(plx_emulator::InvalidCell(bad)),
                "{bad:x?} at {at}"
            );
        }
        for good in accepted {
            let mut wire = vec![valid; 24];
            wire[at] = good;
            let mut row = vec![Cell::BLANK; 24];
            assert_eq!(
                Cell::copy_from_wire(&mut row, &wire),
                Ok(()),
                "{good:x?} at {at}"
            );
        }
    }
}
