//! Shared generators.

use core::num::NonZeroU16;

use plx_core::command::{SessionCommand, TabSide};
use plx_core::geometry::{Direction, Rect};
use plx_core::id::PaneId;
use plx_core::layout::Layout;
use plx_core::tab::{TabName, TabNumber};
use plx_emulator::{FKey, GridSize, Key, KeyAction, KeyChar, KeyEvent, Mods};
use proptest::prelude::*;

pub(crate) fn direction() -> impl Strategy<Value = Direction> {
    prop::sample::select(vec![
        Direction::Left,
        Direction::Right,
        Direction::Up,
        Direction::Down,
    ])
}

/// Areas from 1x1 (everything hidden but one pane) to roomy.
pub(crate) fn area() -> impl Strategy<Value = Rect> {
    (0_u16..5, 0_u16..5, 1_u16..=90, 1_u16..=40).prop_map(|(x, y, cols, rows)| {
        let size = GridSize::new(
            NonZeroU16::new(cols).unwrap(),
            NonZeroU16::new(rows).unwrap(),
        )
        .unwrap();
        Rect::new(x, y, size).unwrap()
    })
}

/// One edit to a layout. Pane references are indices into the current pane
/// list, taken modulo its length, so every operation applies to an existing
/// pane and sequences shrink well.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LayoutOp {
    Split(usize, Direction),
    Remove(usize),
    Grow(usize, Direction, u16),
    Swap(usize, usize),
}

pub(crate) fn layout_op() -> impl Strategy<Value = LayoutOp> {
    prop_oneof![
        4 => (any::<usize>(), direction()).prop_map(|(i, d)| LayoutOp::Split(i, d)),
        2 => any::<usize>().prop_map(LayoutOp::Remove),
        2 => (any::<usize>(), direction(), 1_u16..400).prop_map(|(i, d, s)| LayoutOp::Grow(i, d, s)),
        1 => (any::<usize>(), any::<usize>()).prop_map(|(a, b)| LayoutOp::Swap(a, b)),
    ]
}

/// A layout built by applying `ops` to a single pane; ids are allocated in
/// order. Removing the last pane is skipped (that ends the tab, not the
/// layout), and so is a growth no split can make, as the structure would
/// reject it.
pub(crate) fn build(ops: &[LayoutOp]) -> Layout {
    let mut layout = Layout::single(PaneId::new(0));
    let mut next = 1;
    for op in ops {
        let panes = layout.panes();
        let pick = |i: usize| panes[i % panes.len()];
        match *op {
            LayoutOp::Split(i, direction) => {
                layout.split(pick(i), direction, PaneId::new(next));
                next += 1;
            }
            LayoutOp::Remove(i) => {
                if panes.len() > 1 {
                    layout.remove(pick(i));
                }
            }
            LayoutOp::Grow(i, direction, step) => {
                if layout.can_grow(pick(i), direction, step) {
                    layout.grow(pick(i), direction, step);
                }
            }
            LayoutOp::Swap(a, b) => layout.swap(pick(a), pick(b)),
        }
    }
    layout
}

/// The operations that build a layout; tests that need a layout twice build
/// it twice from these (layouts are not `Clone`).
pub(crate) fn layout_ops() -> impl Strategy<Value = Vec<LayoutOp>> {
    proptest::collection::vec(layout_op(), 0..24)
}

pub(crate) fn layout() -> impl Strategy<Value = Layout> {
    layout_ops().prop_map(|ops| build(&ops))
}

pub(crate) fn grid_size() -> impl Strategy<Value = GridSize> {
    (1_u16..=300, 1_u16..=100).prop_map(|(cols, rows)| {
        GridSize::new(
            NonZeroU16::new(cols).unwrap(),
            NonZeroU16::new(rows).unwrap(),
        )
        .unwrap()
    })
}

pub(crate) fn key() -> impl Strategy<Value = Key> {
    prop_oneof![
        4 => any::<char>().prop_filter_map("platform function-key code", |c| KeyChar::new(c).ok()).prop_map(Key::Char),
        3 => prop::sample::select(vec!['a', 'g', 'h', 'j', 'k', 'l', 'n', 'p', 'q', 'x', 'd', '1', '9', 'A', 'G'])
            .prop_map(|c| Key::Char(KeyChar::new(c).unwrap())),
        2 => prop::sample::select(vec![
            Key::Enter, Key::Tab, Key::Backspace, Key::Escape, Key::Up, Key::Down, Key::Left,
            Key::Right, Key::Home, Key::End, Key::PageUp, Key::PageDown, Key::Insert, Key::Delete,
        ]),
        1 => (1_u8..=FKey::MAX).prop_map(|n| Key::F(FKey::new(n).unwrap())),
    ]
}

pub(crate) fn mods() -> impl Strategy<Value = Mods> {
    (any::<bool>(), any::<bool>(), any::<bool>(), any::<bool>()).prop_map(
        |(shift, alt, ctrl, sup)| {
            [
                (shift, Mods::SHIFT),
                (alt, Mods::ALT),
                (ctrl, Mods::CTRL),
                (sup, Mods::SUPER),
            ]
            .into_iter()
            .filter(|(on, _)| *on)
            .fold(Mods::empty(), |acc, (_, m)| acc.with(m))
        },
    )
}

pub(crate) fn key_event() -> impl Strategy<Value = KeyEvent> {
    let action = prop::sample::select(vec![
        KeyAction::Press,
        KeyAction::Press,
        KeyAction::Repeat,
        KeyAction::Release,
    ]);
    (key(), mods(), action).prop_map(|(key, mods, action)| KeyEvent { key, mods, action })
}

pub(crate) fn command() -> impl Strategy<Value = SessionCommand> {
    use proptest::strategy::LazyJust;
    prop_oneof![
        4 => direction().prop_map(|direction| SessionCommand::Split { direction }),
        2 => LazyJust::new(|| SessionCommand::ClosePane),
        3 => direction().prop_map(|direction| SessionCommand::Focus { direction }),
        2 => direction().prop_map(|direction| SessionCommand::Grow { direction }),
        1 => direction().prop_map(|direction| SessionCommand::Swap { direction }),
        1 => LazyJust::new(|| SessionCommand::RotatePanes),
        2 => direction().prop_map(|direction| SessionCommand::MovePane { direction }),
        1 => LazyJust::new(|| SessionCommand::BreakPane),
        1 => prop_oneof![Just(TabSide::Previous), Just(TabSide::Next)]
            .prop_map(|side| SessionCommand::MovePaneToTab { side }),
        1 => prop_oneof![Just(TabSide::Previous), Just(TabSide::Next)]
            .prop_map(|side| SessionCommand::MoveTab { side }),
        1 => LazyJust::new(|| SessionCommand::ToggleZoom),
        2 => LazyJust::new(|| SessionCommand::NewTab),
        1 => LazyJust::new(|| SessionCommand::CloseTab),
        1 => LazyJust::new(|| SessionCommand::NextTab),
        1 => LazyJust::new(|| SessionCommand::PreviousTab),
        1 => (1_u16..7).prop_map(|n| SessionCommand::FocusTab { tab: TabNumber::new(NonZeroU16::new(n).unwrap()) }),
        1 => prop_oneof!["[a-z]{1,8}", "\\PC{60,64}"]
            .prop_filter_map("a valid tab name", |name| TabName::new(&name).ok())
            .prop_map(|name| SessionCommand::RenameTab { name }),
        // Mouse commands, with ids that may name nothing (they are claims).
        1 => (0_u64..12).prop_map(|n| SessionCommand::FocusPane { pane: plx_core::id::PaneId::new(n) }),
        2 => (0_u16..8, 0_u16..1000).prop_map(|(split, ratio)| SessionCommand::MoveSplit {
            split: plx_core::layout::SplitIndex(split),
            ratio: plx_core::layout::Ratio::clamped(ratio),
        }),
    ]
}
