use std::collections::{BTreeMap, BTreeSet};

use plx_emulator::{
    Cell, CellText, Color, GlyphKind, GridSize, Rgb, Screen, Style, StyleId, Text, Version, Width,
};
use proptest::prelude::*;

const CLUSTERS: [&[char]; 3] = [
    &['e', '\u{301}'],
    &['👍', '\u{1F3FD}'],
    &['a', '\u{308}', '\u{301}'],
];

/// What a generated cell shows and how it is styled, by small indices so
/// runs reuse and drop entries often.
#[derive(Clone, Debug)]
struct GenCell {
    text: u8,
    style: u8,
}

fn text(choice: u8) -> Text<'static> {
    match choice % 6 {
        0 => Text::Empty,
        1 => Text::Char('a'),
        2 => Text::Char('z'),
        n => Text::Cluster(CLUSTERS[usize::from(n - 3)]),
    }
}

/// Style 0 is the default; the others differ in foreground.
fn style(choice: u8) -> Style {
    match choice % 5 {
        0 => Style::DEFAULT,
        n => Style {
            fg: Color::Rgb(Rgb { r: n, g: 0, b: 0 }),
            ..Style::DEFAULT
        },
    }
}

#[derive(Clone, Debug)]
enum Op {
    /// Rewrite row `y` (modulo the rows).
    Write { y: u16, cells: Vec<GenCell> },
    /// A client catches up.
    Frame,
}

const COLS: u16 = 4;
const ROWS: u16 = 3;

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (any::<u16>(), proptest::collection::vec((any::<u8>(), any::<u8>()), usize::from(COLS)))
            .prop_map(|(y, cells)| Op::Write {
                y,
                cells: cells.into_iter().map(|(text, style)| GenCell { text, style }).collect(),
            }),
        1 => Just(Op::Frame),
    ]
}

/// A client's copy, kept up to date only through what a frame would carry:
/// the slots defined and the rows changed since its last acknowledged
/// version.
#[derive(Default)]
struct Mirror {
    styles: BTreeMap<u32, Style>,
    clusters: BTreeMap<u32, Vec<char>>,
    rows: BTreeMap<u16, Vec<Cell>>,
    acked: Version,
}

impl Mirror {
    fn catch_up(&mut self, screen: &Screen, now: Version) {
        let tables = screen.tables();
        for (id, style) in tables.styles_changed_since(self.acked) {
            self.styles.insert(id.get(), *style);
        }
        for (id, codepoints) in tables.graphemes_changed_since(self.acked) {
            self.clusters.insert(id.get(), codepoints.to_vec());
        }
        for (y, row) in screen.grid().rows_newer_than(self.acked) {
            self.rows.insert(y, row.to_vec());
        }
        self.acked = now;
    }

    /// What the mirror shows at `(y, x)`: text and style, owned.
    fn shows(&self, y: u16, x: usize) -> (Vec<char>, Style) {
        let cell = self.rows.get(&y).map_or(Cell::BLANK, |row| row[x]);
        let text = match cell.glyph.kind() {
            GlyphKind::Empty => Vec::new(),
            GlyphKind::Char(c) => vec![c],
            GlyphKind::Cluster(id) => self.clusters[&id.get()].clone(),
        };
        let style = if cell.style == StyleId::DEFAULT {
            Style::DEFAULT
        } else {
            self.styles[&cell.style.get()]
        };
        (text, style)
    }
}

fn shows(screen: &Screen, y: u16, x: usize) -> (Vec<char>, Style) {
    let cell = screen.grid().row_or_none(y).unwrap()[x];
    let resolved = screen.resolve(cell);
    let text = match resolved.text {
        CellText::Empty => Vec::new(),
        CellText::Char(c) => vec![c],
        CellText::Cluster(chars) => chars.to_vec(),
    };
    (text, *resolved.style)
}

proptest! {
    /// Tables hold exactly what the grid uses, and a client that applies
    /// only "slots defined and rows changed since my ack" always shows what
    /// the screen shows, although freed slots are reused for new meanings.
    #[test]
    fn mirrors_converge_and_tables_hold_only_live_entries(ops in proptest::collection::vec(op(), 0..120)) {
        let size = GridSize::new(COLS.try_into().unwrap(), ROWS.try_into().unwrap()).unwrap();
        let mut screen = Screen::new(size);
        let mut mirror = Mirror::default();
        let mut version = Version::ZERO;
        for op in ops {
            match op {
                Op::Write { y, cells } => {
                    version = version.next();
                    let mut row = screen.write_row(y % ROWS, version);
                    for cell in &cells {
                        row.push(text(cell.text), Width::Narrow, style(cell.style));
                    }
                    let _change = row.finish();
                }
                Op::Frame => {
                    mirror.catch_up(&screen, version);
                    for y in 0..ROWS {
                        for x in 0..usize::from(COLS) {
                            prop_assert_eq!(mirror.shows(y, x), shows(&screen, y, x));
                        }
                    }
                }
            }
            let cells: Vec<Cell> = screen.grid().rows().flatten().copied().collect();
            let styles: BTreeSet<StyleId> =
                cells.iter().map(|c| c.style).filter(|&s| s != StyleId::DEFAULT).collect();
            let clusters: BTreeSet<u32> = cells
                .iter()
                .filter_map(|c| match c.glyph.kind() {
                    GlyphKind::Cluster(id) => Some(id.get()),
                    GlyphKind::Empty | GlyphKind::Char(_) => None,
                })
                .collect();
            prop_assert_eq!(screen.tables().style_count(), styles.len() + 1);
            prop_assert_eq!(screen.tables().grapheme_count(), clusters.len());
        }
    }
}

#[test]
#[should_panic(expected = "plx bug: a row written to a screen lies within it")]
fn writing_past_the_last_row_is_a_bug() {
    let size = GridSize::new(2.try_into().unwrap(), 2.try_into().unwrap()).unwrap();
    let mut screen = Screen::new(size);
    let _writer = screen.write_row(2, Version::ZERO.next());
}

#[test]
#[should_panic(expected = "plx bug: a row written to a screen has as many cells as it has columns")]
fn a_short_row_is_a_bug() {
    let size = GridSize::new(2.try_into().unwrap(), 2.try_into().unwrap()).unwrap();
    let mut screen = Screen::new(size);
    let mut row = screen.write_row(0, Version::ZERO.next());
    row.push(Text::Char('a'), Width::Narrow, Style::DEFAULT);
    let _change = row.finish();
}

/// A control character is never a cell's text: the cell is empty, keeping
/// its column and style.
#[test]
fn control_characters_are_written_as_empty_cells() {
    let size = GridSize::new(5.try_into().unwrap(), 1.try_into().unwrap()).unwrap();
    let mut screen = Screen::new(size);
    let styled = Style {
        fg: Color::Rgb(Rgb { r: 1, g: 2, b: 3 }),
        ..Style::DEFAULT
    };
    let mut row = screen.write_row(0, Version::new(1));
    row.push(Text::Char('\u{7f}'), Width::Narrow, styled);
    row.push(Text::Char('\u{1b}'), Width::Narrow, Style::DEFAULT);
    row.push(Text::Char('\u{85}'), Width::Narrow, Style::DEFAULT);
    row.push(
        Text::Cluster(&['a', '\u{7}']),
        Width::Narrow,
        Style::DEFAULT,
    );
    row.push(Text::Char('x'), Width::Narrow, Style::DEFAULT);
    let _changed = row.finish();
    let cells = screen.grid().row_or_none(0).unwrap();
    for &cell in &cells[..4] {
        assert_eq!(cell.glyph.kind(), GlyphKind::Empty, "{cell:?}");
    }
    assert_eq!(*screen.resolve(cells[0]).style, styled);
    assert_eq!(cells[4].glyph.kind(), GlyphKind::Char('x'));
}
