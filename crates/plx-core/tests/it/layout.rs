use std::collections::BTreeSet;

use plx_core::geometry::{Direction, Rect};
use plx_core::id::PaneId;
use plx_core::layout::Layout;
use proptest::prelude::*;

use crate::strategy::{area, build, direction, layout, layout_ops};

/// Paints every visible pane and separator into a grid of `area` and checks
/// that each cell is painted exactly once.
fn assert_partitions(layout: &Layout, area: Rect) -> Result<(), TestCaseError> {
    let arrangement = layout.arrange(area);
    let cols = usize::from(area.cols());
    let mut painted = vec![0_u8; cols * usize::from(area.rows())];
    let rects = arrangement
        .panes
        .iter()
        .filter_map(|p| p.rect)
        .chain(arrangement.separators.iter().map(|s| s.rect));
    for rect in rects {
        prop_assert!(
            rect.x() >= area.x() && rect.y() >= area.y(),
            "{rect:?} starts outside {area:?}"
        );
        prop_assert!(
            rect.right() <= area.right() && rect.bottom() <= area.bottom(),
            "{rect:?} ends outside {area:?}"
        );
        for y in rect.y()..rect.y() + rect.rows() {
            for x in rect.x()..rect.x() + rect.cols() {
                painted[usize::from(y - area.y()) * cols + usize::from(x - area.x())] += 1;
            }
        }
    }
    prop_assert!(
        painted.iter().all(|&n| n == 1),
        "cells covered {painted:?} times in a {cols}-wide area"
    );
    Ok(())
}

proptest! {
    #[test]
    fn arrangement_partitions_the_area_exactly(layout in layout(), area in area()) {
        assert_partitions(&layout, area)?;
    }

    #[test]
    fn arrangement_lists_every_pane_once_in_layout_order(layout in layout(), area in area()) {
        let placed: Vec<PaneId> = layout.arrange(area).panes.iter().map(|p| p.pane).collect();
        prop_assert_eq!(&placed, &layout.panes());
        let unique: BTreeSet<PaneId> = placed.iter().copied().collect();
        prop_assert_eq!(unique.len(), placed.len());
    }

    /// However small the area, something is shown: a split too small to
    /// divide gives everything to its first child, so the first pane in
    /// layout order is always visible.
    #[test]
    fn the_first_pane_is_always_visible(layout in layout(), area in area()) {
        prop_assert!(layout.arrange(area).visible_rect_or_none(layout.first_pane()).is_some());
    }

    #[test]
    fn removing_a_new_split_restores_the_layout(ops in layout_ops(), i in any::<usize>(), dir in direction()) {
        let layout = build(&ops);
        let panes = layout.panes();
        let target = panes[i % panes.len()];
        let new = PaneId::new(u64::MAX);
        let mut edited = build(&ops);
        edited.split(target, dir, new);
        prop_assert!(edited.contains(new));
        edited.remove(new);
        prop_assert_eq!(edited, layout);
    }

    #[test]
    fn grow_changes_sizes_but_never_membership(ops in layout_ops(), i in any::<usize>(), dir in direction(), step in 1_u16..400) {
        let layout = build(&ops);
        let panes = layout.panes();
        let pane = panes[i % panes.len()];
        let mut grown = build(&ops);
        if layout.can_grow(pane, dir, step) {
            grown.grow(pane, dir, step);
            prop_assert_ne!(&grown, &layout);
        }
        prop_assert_eq!(grown.panes(), panes);
    }

    /// A neighbour lies directly across a separator in that direction and
    /// shares at least one row or column with the origin.
    #[test]
    fn neighbors_are_adjacent_in_the_requested_direction(
        layout in layout(), area in area(), i in any::<usize>(), dir in direction(),
    ) {
        let panes = layout.panes();
        let from = panes[i % panes.len()];
        let arrangement = layout.arrange(area);
        if let Some(to) = layout.neighbor_or_none(from, dir, area) {
            let a = arrangement.visible_rect_or_none(from).unwrap();
            let b = arrangement.visible_rect_or_none(to).unwrap();
            let (across, shared) = match dir {
                Direction::Right => (u32::from(b.x()) == a.right() + 1, a.bottom().min(b.bottom()) > u32::from(a.y().max(b.y()))),
                Direction::Left => (b.right() + 1 == u32::from(a.x()), a.bottom().min(b.bottom()) > u32::from(a.y().max(b.y()))),
                Direction::Down => (u32::from(b.y()) == a.bottom() + 1, a.right().min(b.right()) > u32::from(a.x().max(b.x()))),
                Direction::Up => (b.bottom() + 1 == u32::from(a.y()), a.right().min(b.right()) > u32::from(a.x().max(b.x()))),
            };
            prop_assert!(across && shared, "{from:?} {a:?} -> {to:?} {b:?} going {dir:?}");
        }
    }

    #[test]
    fn swap_exchanges_exactly_two_panes(layout in layout(), i in any::<usize>(), j in any::<usize>()) {
        let panes = layout.panes();
        let (a, b) = (panes[i % panes.len()], panes[j % panes.len()]);
        let mut swapped = layout;
        swapped.swap(a, b);
        let expected: Vec<PaneId> = panes
            .iter()
            .map(|&p| if p == a { b } else if p == b { a } else { p })
            .collect();
        prop_assert_eq!(swapped.panes(), expected);
    }
}

#[test]
#[should_panic(expected = "plx bug: a tab's only pane closes with its tab, not by removal")]
fn removing_the_only_pane_is_a_bug() {
    Layout::single(PaneId::new(7)).remove(PaneId::new(7));
}

#[test]
#[should_panic(expected = "plx bug: a split's target is in the layout")]
fn splitting_an_unknown_pane_is_a_bug() {
    Layout::single(PaneId::new(0)).split(PaneId::new(1), Direction::Right, PaneId::new(2));
}

#[test]
#[should_panic(expected = "plx bug: a new pane is not in the layout yet")]
fn splitting_in_a_pane_twice_is_a_bug() {
    Layout::single(PaneId::new(0)).split(PaneId::new(0), Direction::Right, PaneId::new(0));
}

#[test]
fn rectangles_stay_inside_the_coordinate_space() {
    let size = plx_emulator::GridSize::new(10.try_into().unwrap(), 10.try_into().unwrap()).unwrap();
    assert!(
        Rect::new(u16::MAX - 9, 0, size).is_ok(),
        "the last column is addressable"
    );
    assert!(Rect::new(u16::MAX - 8, 0, size).is_err());
    assert!(Rect::new(0, u16::MAX - 8, size).is_err());
    let raw = r#"{"x":65535,"y":0,"size":{"cols":2,"rows":1}}"#;
    assert!(
        serde_json::from_str::<Rect>(raw).is_err(),
        "deserialising checks it too"
    );
}

fn sized(cols: u16, rows: u16) -> Rect {
    Rect::at_origin(
        plx_emulator::GridSize::new(cols.try_into().unwrap(), rows.try_into().unwrap()).unwrap(),
    )
}

/// Four panes in a 2x2 grid: 0 top left, 1 top right, 2 bottom left,
/// 3 bottom right.
fn grid_2x2() -> Layout {
    let mut layout = Layout::single(PaneId::new(0));
    layout.split(PaneId::new(0), Direction::Right, PaneId::new(1));
    layout.split(PaneId::new(0), Direction::Down, PaneId::new(2));
    layout.split(PaneId::new(1), Direction::Down, PaneId::new(3));
    layout
}

#[test]
fn neighbours_in_a_grid_are_exactly_the_adjacent_panes() {
    let layout = grid_2x2();
    let area = sized(81, 25);
    let p = PaneId::new;
    let cases = [
        (0, Direction::Right, Some(1)),
        (0, Direction::Down, Some(2)),
        (0, Direction::Left, None),
        (0, Direction::Up, None),
        (3, Direction::Left, Some(2)),
        (3, Direction::Up, Some(1)),
        (3, Direction::Right, None),
        (3, Direction::Down, None),
        (1, Direction::Left, Some(0)),
        (2, Direction::Up, Some(0)),
    ];
    for (from, direction, expected) in cases {
        assert_eq!(
            layout.neighbor_or_none(p(from), direction, area),
            expected.map(p),
            "from {from} going {direction:?}"
        );
    }
}

#[test]
fn the_neighbour_sharing_most_cells_wins() {
    // 0 on the left; on the right, 1 (one row) above 2 (the rest).
    let mut layout = Layout::single(PaneId::new(0));
    layout.split(PaneId::new(0), Direction::Right, PaneId::new(1));
    layout.split(PaneId::new(1), Direction::Down, PaneId::new(2));
    for _ in 0..9 {
        layout.grow(PaneId::new(2), Direction::Up, 50);
    }
    assert_eq!(
        layout.neighbor_or_none(PaneId::new(0), Direction::Right, sized(40, 30)),
        Some(PaneId::new(2))
    );
}

#[test]
fn growth_moves_the_nearest_split_on_the_axis_until_its_limit() {
    let mut layout = Layout::single(PaneId::new(0));
    layout.split(PaneId::new(0), Direction::Right, PaneId::new(1));
    let left = PaneId::new(0);
    assert!(layout.can_grow(left, Direction::Right, 50));
    assert!(
        !layout.can_grow(left, Direction::Down, 50),
        "no split on that axis"
    );
    layout.grow(left, Direction::Right, 50);
    let Layout::Split(split) = &layout else {
        panic!("a split")
    };
    assert_eq!(split.ratio().get(), 550);
    // Up to the maximum, then no further.
    while layout.can_grow(left, Direction::Right, 50) {
        layout.grow(left, Direction::Right, 50);
    }
    let Layout::Split(split) = &layout else {
        panic!("a split")
    };
    assert_eq!(split.ratio().get(), plx_core::layout::Ratio::MAX);
    // A border at its limit stays: the pane does not shrink instead.
    assert!(!layout.can_grow(left, Direction::Right, 50));
    // The other pane grows it back.
    assert!(layout.can_grow(PaneId::new(1), Direction::Left, 50));
}

/// As Zellij's `Resize Increase <direction>`: a border moves that way
/// whichever pane has the focus. A pane at the edge on that side has its
/// other border move instead, so it shrinks.
#[test]
fn at_the_edge_resizing_moves_the_other_border() {
    let mut layout = Layout::single(PaneId::new(0));
    layout.split(PaneId::new(0), Direction::Right, PaneId::new(1));
    let (left, right) = (PaneId::new(0), PaneId::new(1));
    let ratio = |layout: &Layout| {
        let Layout::Split(split) = layout else {
            panic!("a split")
        };
        split.ratio().get()
    };
    for (pane, direction, expected) in [
        (left, Direction::Left, 450),
        (right, Direction::Left, 400),
        (right, Direction::Right, 450),
        (left, Direction::Right, 500),
    ] {
        assert!(layout.can_grow(pane, direction, 50));
        layout.grow(pane, direction, 50);
        assert_eq!(ratio(&layout), expected, "{pane:?} {direction:?}");
    }
    // Across the split's axis there is no border to move.
    assert!(!layout.can_grow(left, Direction::Up, 50));
}

#[test]
fn a_split_needs_three_cells_to_show_both_sides() {
    let mut layout = Layout::single(PaneId::new(0));
    layout.split(PaneId::new(0), Direction::Right, PaneId::new(1));
    let shown = |cols| {
        let arrangement = layout.arrange(sized(cols, 5));
        arrangement.visible_rect_or_none(PaneId::new(1)).is_some()
    };
    assert!(shown(3), "one cell each plus the separator");
    assert!(!shown(2), "too narrow: the second pane is hidden");
}

#[test]
fn a_zoomed_tab_lists_every_pane_and_shows_only_the_focus() {
    let mut structure = plx_core::structure::Structure::new();
    let split = plx_core::command::SessionCommand::Split {
        direction: Direction::Right,
    };
    let zoom = plx_core::command::SessionCommand::ToggleZoom;
    for command in [split, zoom] {
        assert_eq!(
            structure.apply(&command, sized(80, 24), |_| {}),
            Ok(plx_core::structure::Lifecycle::Running)
        );
    }
    let arrangement = structure.active().arrange(sized(80, 24));
    let shown: Vec<(PaneId, bool)> = arrangement
        .panes
        .iter()
        .map(|placement| (placement.pane, placement.rect == Some(sized(80, 24))))
        .collect();
    assert_eq!(shown, [(PaneId::new(0), false), (PaneId::new(1), true)]);
    assert_eq!(arrangement.separators, []);
}

#[test]
fn ratios_clamp_into_their_range() {
    use plx_core::layout::Ratio;
    assert_eq!(Ratio::clamped(0).get(), Ratio::MIN);
    assert_eq!(Ratio::clamped(Ratio::MIN).get(), Ratio::MIN);
    assert_eq!(Ratio::clamped(Ratio::MIN + 1).get(), Ratio::MIN + 1);
    assert_eq!(Ratio::clamped(Ratio::MAX).get(), Ratio::MAX);
    assert_eq!(Ratio::clamped(u16::MAX).get(), Ratio::MAX);
    assert_eq!(Ratio::new(Ratio::MAX + 1), None);
}

#[test]
fn growth_reaches_a_nested_split_when_the_outer_one_cannot_help() {
    // Outer split stacked (vertical); the top half split side by side.
    let mut layout = Layout::single(PaneId::new(0));
    layout.split(PaneId::new(0), Direction::Down, PaneId::new(2));
    layout.split(PaneId::new(0), Direction::Right, PaneId::new(1));
    assert!(layout.can_grow(PaneId::new(0), Direction::Right, 50));
    assert!(layout.can_grow(PaneId::new(1), Direction::Left, 50));
    assert!(layout.can_grow(PaneId::new(2), Direction::Up, 50));
    assert!(!layout.can_grow(PaneId::new(2), Direction::Right, 50));
}

proptest! {
    /// Dragging a separator puts it where the pointer is (thousandths are
    /// exact for splits up to 1000 cells; the screens here are smaller).
    #[test]
    fn a_dragged_separator_lands_under_the_pointer(
        cols in 10_u16..300,
        rows in 4_u16..80,
        target in 0_u16..300,
        stacked in any::<bool>(),
    ) {
        use plx_core::layout::SplitIndex;
        let area = Rect::at_origin(
            plx_emulator::GridSize::new(cols.try_into().unwrap(), rows.try_into().unwrap()).unwrap(),
        );
        let mut layout = Layout::single(PaneId::new(0));
        let direction = if stacked { Direction::Down } else { Direction::Right };
        layout.split(PaneId::new(0), direction, PaneId::new(1));
        let split = SplitIndex(0);
        let ratio = layout.ratio_at_or_none(split, target, area).unwrap();
        layout.move_split(split, ratio);
        let separator = layout.arrange(area).separators[0];
        let (start, extent) = if stacked { (separator.rect.y(), rows) } else { (separator.rect.x(), cols) };
        // Within the split's reach, exactly there; beyond it, at the edge
        // a ratio allows.
        let lowest = 1 + u16::try_from(u32::from(extent - 3) * 50 / 1000).unwrap();
        let highest = extent - 2 - u16::try_from(u32::from(extent - 3) * 50 / 1000).unwrap();
        prop_assert!(start >= lowest.saturating_sub(1) && start <= highest + 1, "{start} not in {lowest}..={highest}");
        if target > lowest && target < highest {
            prop_assert_eq!(start, target);
        }
    }
}

/// Rotating moves every pane one place on in reading order, the last to
/// the first; the places keep their sizes, and as many rotations as panes
/// bring every pane back.
#[test]
fn rotating_moves_every_pane_one_place_on() {
    // 0 on the left; on the right, 1 above 2.
    let mut layout = Layout::single(PaneId::new(0));
    layout.split(PaneId::new(0), Direction::Right, PaneId::new(1));
    layout.split(PaneId::new(1), Direction::Down, PaneId::new(2));
    let before = layout.arrange(sized(40, 30));
    layout.rotate();
    assert_eq!(
        layout.panes(),
        [PaneId::new(2), PaneId::new(0), PaneId::new(1)]
    );
    let after = layout.arrange(sized(40, 30));
    let rects = |arrangement: &plx_core::layout::Arrangement| {
        arrangement.panes.iter().map(|p| p.rect).collect::<Vec<_>>()
    };
    assert_eq!(rects(&after), rects(&before));
    assert_eq!(after.separators, before.separators);
    layout.rotate();
    layout.rotate();
    assert_eq!(
        layout.panes(),
        [PaneId::new(0), PaneId::new(1), PaneId::new(2)]
    );
}
