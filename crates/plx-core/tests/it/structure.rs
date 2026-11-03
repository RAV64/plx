use std::collections::BTreeSet;

use plx_core::command::SessionCommand;
use plx_core::event::{EndReason, StructureEvent};
use plx_core::geometry::{Direction, Rect};
use plx_core::id::{PaneId, TabId};
use plx_core::rejection::Rejection;
use plx_core::structure::{Lifecycle, MAX_TABS, Structure};
use plx_core::tab::{Tab, TabName, TabNumber, Zoom};
use proptest::prelude::*;

use crate::strategy::{area, command};

/// One step of a generated run: a user command, the program in some pane
/// exiting (picked by index modulo the live panes), or an editor covering
/// the focused pane.
#[derive(Debug)]
enum Step {
    Command(SessionCommand),
    Exit(usize),
    Cover,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => command().prop_map(Step::Command),
        1 => any::<usize>().prop_map(Step::Exit),
        1 => Just(()).prop_map(|()| Step::Cover),
    ]
}

/// A test-only copy of an event (events are not `Clone`).
fn copy_event(event: &StructureEvent) -> StructureEvent {
    match event {
        StructureEvent::TabOpened { tab, pane } => StructureEvent::TabOpened {
            tab: *tab,
            pane: *pane,
        },
        StructureEvent::TabClosed { tab, active } => StructureEvent::TabClosed {
            tab: *tab,
            active: *active,
        },
        StructureEvent::TabRenamed { tab, name } => StructureEvent::TabRenamed {
            tab: *tab,
            name: name.duplicate(),
        },
        StructureEvent::TabActivated { tab } => StructureEvent::TabActivated { tab: *tab },
        StructureEvent::PaneOpened {
            pane,
            tab,
            beside,
            direction,
        } => StructureEvent::PaneOpened {
            pane: *pane,
            tab: *tab,
            beside: *beside,
            direction: *direction,
        },
        StructureEvent::PaneClosed { pane, tab, focus } => StructureEvent::PaneClosed {
            pane: *pane,
            tab: *tab,
            focus: *focus,
        },
        StructureEvent::FocusChanged { tab, pane } => StructureEvent::FocusChanged {
            tab: *tab,
            pane: *pane,
        },
        StructureEvent::PaneGrown {
            tab,
            pane,
            direction,
            step,
        } => StructureEvent::PaneGrown {
            tab: *tab,
            pane: *pane,
            direction: *direction,
            step: *step,
        },
        StructureEvent::PanesRotated { tab } => StructureEvent::PanesRotated { tab: *tab },
        StructureEvent::PaneMovedToTab {
            pane,
            from,
            left_focus,
            to,
            into,
        } => StructureEvent::PaneMovedToTab {
            pane: *pane,
            from: *from,
            left_focus: *left_focus,
            to: *to,
            into: *into,
        },
        StructureEvent::TabMoved { tab, side } => StructureEvent::TabMoved {
            tab: *tab,
            side: *side,
        },
        StructureEvent::PaneCovered { tab, pane, cover } => StructureEvent::PaneCovered {
            tab: *tab,
            pane: *pane,
            cover: *cover,
        },
        StructureEvent::PaneUncovered { tab, cover, pane } => StructureEvent::PaneUncovered {
            tab: *tab,
            cover: *cover,
            pane: *pane,
        },
        StructureEvent::CoveredPaneExited { tab, pane } => StructureEvent::CoveredPaneExited {
            tab: *tab,
            pane: *pane,
        },
        StructureEvent::PanesSwapped { tab, a, b } => StructureEvent::PanesSwapped {
            tab: *tab,
            a: *a,
            b: *b,
        },
        StructureEvent::ZoomChanged { tab, zoom } => StructureEvent::ZoomChanged {
            tab: *tab,
            zoom: *zoom,
        },
        StructureEvent::SplitMoved { tab, split, ratio } => StructureEvent::SplitMoved {
            tab: *tab,
            split: *split,
            ratio: *ratio,
        },
    }
}

/// A session under test: its structure plus the full event log.
struct Run {
    structure: Structure,
    log: Vec<StructureEvent>,
    ended: Option<EndReason>,
}

impl Run {
    fn start() -> Self {
        Self {
            structure: Structure::new(),
            log: Vec::new(),
            ended: None,
        }
    }

    fn apply(&mut self, command: &SessionCommand, area: Rect) -> Result<(), Rejection> {
        let log = &mut self.log;
        let lifecycle = self
            .structure
            .apply(command, area, |event| log.push(copy_event(event)))?;
        self.track(lifecycle);
        Ok(())
    }

    fn exit(&mut self, pane: PaneId) {
        let log = &mut self.log;
        let lifecycle = self
            .structure
            .pane_exited(pane, |event| log.push(copy_event(event)));
        self.track(lifecycle);
    }

    fn cover(&mut self) -> PaneId {
        let log = &mut self.log;
        self.structure
            .cover_focus(|event| log.push(copy_event(event)))
    }

    fn track(&mut self, lifecycle: Lifecycle) {
        if let Lifecycle::Ended(reason) = lifecycle {
            self.ended = Some(reason);
        }
    }
}

/// What the tests need to remember about a structure before a step. Taken
/// from the structure, not cloned: the full `Debug` rendering compares every
/// detail (layout shapes and ratios included).
struct Before {
    rendering: String,
    panes: BTreeSet<PaneId>,
    tabs: usize,
    active_position: usize,
    focus: PaneId,
    zoom: Zoom,
    events: usize,
}

impl Before {
    fn of(run: &Run) -> Self {
        let structure = &run.structure;
        let active = structure.active();
        Self {
            rendering: format!("{structure:?}"),
            panes: structure.panes().into_iter().collect(),
            tabs: structure.tab_count(),
            active_position: structure.active_position(),
            focus: active.focus(),
            zoom: active.zoom(),
            events: run.log.len(),
        }
    }
}

fn area_80x24() -> Rect {
    Rect::at_origin(
        plx_emulator::GridSize::new(80.try_into().unwrap(), 24.try_into().unwrap()).unwrap(),
    )
}

proptest! {
    #[test]
    fn apply_keeps_every_invariant_and_the_log_replays(
        steps in proptest::collection::vec(step(), 0..80),
        area in area(),
    ) {
        let mut run = Run::start();
        for step in steps {
            if run.ended.is_some() {
                break;
            }
            let before = Before::of(&run);
            match step {
                Step::Command(command) => {
                    if run.apply(&command, area).is_ok() {
                        if run.ended.is_none() {
                            check_command_effect(&command, &before, &run)?;
                        }
                    } else {
                        prop_assert_eq!(format!("{:?}", run.structure), before.rendering);
                    }
                }
                Step::Cover => {
                    let cover = run.cover();
                    let after: BTreeSet<PaneId> = run.structure.panes().into_iter().collect();
                    let mut expected = before.panes;
                    expected.insert(cover);
                    prop_assert_eq!(after, expected);
                    let active = run.structure.active();
                    prop_assert_eq!(active.focus(), cover);
                    prop_assert!(!active.layout().contains(before.focus));
                }
                Step::Exit(i) => {
                    let panes: Vec<PaneId> = before.panes.iter().copied().collect();
                    let pane = panes[i % panes.len()];
                    run.exit(pane);
                    if run.ended.is_none() {
                        let after: BTreeSet<PaneId> = run.structure.panes().into_iter().collect();
                        let mut expected = before.panes;
                        expected.remove(&pane);
                        prop_assert_eq!(after, expected);
                    }
                }
            }
            if run.ended.is_none() {
                run.structure.assert_invariants();
            }
        }

        // The log alone reproduces the structure, its id allocator included.
        if run.ended.is_none() {
            prop_assert_eq!(Structure::replay(run.log), run.structure);
        }
    }
}

/// What each accepted command must have done.
fn check_command_effect(
    command: &SessionCommand,
    before: &Before,
    run: &Run,
) -> Result<(), TestCaseError> {
    let after = &run.structure;
    let panes = |s: &Structure| -> BTreeSet<PaneId> { s.panes().into_iter().collect() };
    let active = after.active();
    let new_events = run.log.len() - before.events;
    match command {
        SessionCommand::Split { .. } => {
            let added: Vec<_> = panes(after).difference(&before.panes).copied().collect();
            prop_assert_eq!(added.len(), 1);
            prop_assert_eq!(active.focus(), added[0]);
            prop_assert!(before.panes.is_subset(&panes(after)));
            prop_assert_eq!(active.zoom(), Zoom::Tiled);
        }
        SessionCommand::ClosePane => {
            let mut expected = before.panes.clone();
            expected.remove(&before.focus);
            prop_assert_eq!(panes(after), expected);
        }
        SessionCommand::Focus { .. } => {
            prop_assert_eq!(panes(after), before.panes.clone());
            prop_assert_ne!(active.focus(), before.focus);
        }
        SessionCommand::Grow { .. } | SessionCommand::Swap { .. } => {
            prop_assert_eq!(panes(after), before.panes.clone());
            prop_assert_eq!(active.focus(), before.focus);
            prop_assert_ne!(format!("{after:?}"), before.rendering.clone());
        }
        SessionCommand::RotatePanes => {
            // The same panes, the focus with its pane; a tab of one pane
            // does not change.
            prop_assert_eq!(panes(after), before.panes.clone());
            prop_assert_eq!(active.focus(), before.focus);
            prop_assert_eq!(new_events == 0, active.layout().panes().len() == 1);
        }
        SessionCommand::MovePane { .. }
        | SessionCommand::BreakPane
        | SessionCommand::MovePaneToTab { .. } => {
            // The same panes; the moved one keeps the focus, wherever it is.
            prop_assert_eq!(panes(after), before.panes.clone());
            prop_assert_eq!(active.focus(), before.focus);
            prop_assert!(active.holds(before.focus));
        }
        SessionCommand::MoveTab { .. } => {
            prop_assert_eq!(panes(after), before.panes.clone());
            prop_assert_eq!(after.tab_count(), before.tabs);
            prop_assert_eq!(active.focus(), before.focus);
            prop_assert_eq!(
                after.active_position().abs_diff(before.active_position),
                usize::from(new_events > 0)
            );
        }
        SessionCommand::ToggleZoom => prop_assert_eq!(active.zoom(), before.zoom.toggled()),
        SessionCommand::NewTab => {
            prop_assert_eq!(after.tab_count(), before.tabs + 1);
            prop_assert_eq!(after.active_position(), before.active_position + 1);
            prop_assert_eq!(active.layout().panes().len(), 1);
        }
        SessionCommand::CloseTab => prop_assert_eq!(after.tab_count() + 1, before.tabs),
        SessionCommand::FocusTab { tab } => {
            prop_assert_eq!(after.active_position() + 1, usize::from(tab.get().get()));
        }
        SessionCommand::NextTab | SessionCommand::PreviousTab => {
            prop_assert_eq!(after.tab_count(), before.tabs);
            prop_assert_eq!(new_events == 0, before.tabs == 1);
        }
        SessionCommand::RenameTab { name } => prop_assert_eq!(active.name(), Some(name)),
        SessionCommand::KillSession => {}
        SessionCommand::FocusPane { pane } => {
            prop_assert_eq!(panes(after), before.panes.clone());
            prop_assert_eq!(active.focus(), *pane);
        }
        SessionCommand::MoveSplit { split, ratio } => {
            prop_assert_eq!(panes(after), before.panes.clone());
            prop_assert_eq!(active.layout().split_ratio_or_none(*split), Some(*ratio));
        }
    }
    Ok(())
}

#[test]
fn closing_the_last_pane_ends_the_session() {
    let mut run = Run::start();
    run.apply(&SessionCommand::ClosePane, area_80x24()).unwrap();
    assert_eq!(run.ended, Some(EndReason::LastPaneClosed));
    assert!(
        run.log.is_empty(),
        "ending folds no event into the structure"
    );
}

#[test]
fn tabs_cycle_and_wrap_in_order() {
    let mut run = Run::start();
    for _ in 0..3 {
        run.apply(&SessionCommand::NewTab, area_80x24()).unwrap();
    }
    let order: Vec<TabId> = run.structure.tabs().map(Tab::id).collect();
    assert_eq!(run.structure.active_position(), 3);
    run.apply(&SessionCommand::NextTab, area_80x24()).unwrap();
    assert_eq!(
        run.structure.active().id(),
        order[0],
        "next wraps to the first"
    );
    run.apply(&SessionCommand::PreviousTab, area_80x24())
        .unwrap();
    assert_eq!(
        run.structure.active().id(),
        order[3],
        "previous wraps to the last"
    );
    let ids: Vec<TabId> = run.structure.tabs().map(Tab::id).collect();
    assert_eq!(ids, order, "cycling never reorders tabs");
}

#[test]
fn a_session_holds_at_most_max_tabs() {
    let mut run = Run::start();
    // Open tabs from the first one, then from the middle: the limit counts
    // tabs, whichever tab is active.
    for _ in 1..MAX_TABS / 2 {
        run.apply(&SessionCommand::NewTab, area_80x24()).unwrap();
    }
    let first = TabNumber::new(1.try_into().unwrap());
    run.apply(&SessionCommand::FocusTab { tab: first }, area_80x24())
        .unwrap();
    while run.structure.tab_count() < MAX_TABS {
        run.apply(&SessionCommand::NewTab, area_80x24()).unwrap();
    }
    assert_eq!(
        run.apply(&SessionCommand::NewTab, area_80x24()),
        Err(Rejection::TooManyTabs)
    );
}

#[test]
fn tab_names_are_parsed_where_they_enter() {
    use plx_core::tab::TabNameError;
    assert_eq!(TabName::new(""), Err(TabNameError::Empty));
    assert_eq!(TabName::new(&"x".repeat(65)), Err(TabNameError::TooLong));
    assert_eq!(TabName::new("a\u{7}b"), Err(TabNameError::ControlCharacter));
    assert_eq!(
        TabName::new(&"é".repeat(64)).unwrap().as_str(),
        "é".repeat(64)
    );
    assert!(serde_json::from_str::<TabName>("\"\"").is_err());
}

#[test]
#[should_panic(expected = "plx bug: an activated tab is in the session")]
fn a_forged_event_for_a_missing_tab_is_a_bug() {
    Structure::new().evolve(StructureEvent::TabActivated {
        tab: TabId::new(99),
    });
}

#[test]
#[should_panic(expected = "plx bug: panes open with the next pane id")]
fn a_forged_event_reusing_an_id_is_a_bug() {
    Structure::new().evolve(StructureEvent::PaneOpened {
        pane: PaneId::new(0),
        tab: TabId::new(0),
        beside: PaneId::new(0),
        direction: Direction::Right,
    });
}

/// An editor covering a pane stands in its place, the pane hidden but held
/// by its tab; when the editor exits (or is closed), the pane is back in
/// its place with the focus. A hidden pane that exits just goes.
#[test]
fn a_cover_stands_in_for_a_pane_until_it_goes() {
    let mut run = Run::start();
    let split = SessionCommand::Split {
        direction: Direction::Right,
    };
    run.apply(&split, area_80x24()).unwrap();
    let pane = run.structure.active().focus();
    let shape = run.structure.active().arrange(area_80x24());
    let cover = run.cover();
    let tab = run.structure.active();
    assert_eq!(tab.focus(), cover);
    assert!(tab.holds(pane) && !tab.layout().contains(pane));
    assert_eq!(
        run.structure.tab_holding_or_none(pane).map(Tab::id),
        Some(tab.id())
    );
    // The editor has the pane's place.
    let rect_of = |arrangement: &plx_core::layout::Arrangement, p: PaneId| {
        arrangement
            .panes
            .iter()
            .find(|placement| placement.pane == p)
            .map(|placement| placement.rect)
    };
    assert_eq!(
        rect_of(&tab.arrange(area_80x24()), cover),
        rect_of(&shape, pane)
    );
    run.exit(cover);
    assert_eq!(run.structure.active().focus(), pane);
    assert_eq!(run.structure.active().arrange(area_80x24()), shape);
    // Closing the editor gives the place back too.
    let cover = run.cover();
    run.apply(&SessionCommand::ClosePane, area_80x24()).unwrap();
    assert_eq!(run.structure.active().focus(), pane);
    assert!(!run.structure.panes().contains(&cover));
    // A hidden pane that exits goes; its editor stays, now an ordinary pane.
    let cover = run.cover();
    run.exit(pane);
    assert!(!run.structure.panes().contains(&pane));
    assert_eq!(run.structure.active().focus(), cover);
    assert_eq!(run.structure.active().covers(), []);
    run.structure.assert_invariants();
    assert_eq!(Structure::replay(run.log), run.structure);
}

/// Moving panes across tabs: out into a tab of its own, into the tab on
/// a side; a tab's last pane merges into the next tab, which becomes active
/// as its tab closes; and tabs move along the row.
#[test]
fn panes_move_across_tabs_and_tabs_along_the_row() {
    use plx_core::command::TabSide;
    let area = area_80x24();
    let mut run = Run::start();
    let split = SessionCommand::Split {
        direction: Direction::Right,
    };
    run.apply(&split, area).unwrap();
    let (first, second) = (PaneId::new(0), run.structure.active().focus());
    let tab_of = |run: &Run, pane| run.structure.tab_holding_or_none(pane).map(Tab::id);
    let first_tab = tab_of(&run, first).unwrap();
    // Out into a tab of its own, after the first.
    run.apply(&SessionCommand::BreakPane, area).unwrap();
    assert_eq!(run.structure.tab_count(), 2);
    assert_eq!(run.structure.active().focus(), second);
    assert_eq!(run.structure.active().layout().panes(), [second]);
    assert_eq!(run.structure.active_position(), 1);
    // Alone in its tab, moving left merges it into the tab on the left:
    // its tab closes, and that tab is active with it focused.
    let lone = Direction::Left;
    run.apply(&SessionCommand::MovePane { direction: lone }, area)
        .unwrap();
    assert_eq!(run.structure.tab_count(), 1);
    assert_eq!(run.structure.active().id(), first_tab);
    assert_eq!(run.structure.active().focus(), second);
    // It came in from the right, so it is on the right of the first pane.
    assert_eq!(run.structure.active().layout().panes(), [first, second]);
    // Moving it right swaps it with nothing there: past the edge, into a
    // new tab after.
    run.apply(
        &SessionCommand::MovePane {
            direction: Direction::Right,
        },
        area,
    )
    .unwrap();
    assert_eq!(run.structure.tab_count(), 2);
    assert_eq!(run.structure.active_position(), 1);
    // A lone pane at the left end has nowhere to go but stays.
    run.apply(
        &SessionCommand::MoveTab {
            side: TabSide::Previous,
        },
        area,
    )
    .unwrap();
    assert_eq!(run.structure.active_position(), 0);
    assert_eq!(run.structure.active().focus(), second);
    let before = format!("{:?}", run.structure);
    run.apply(
        &SessionCommand::MovePaneToTab {
            side: TabSide::Previous,
        },
        area,
    )
    .unwrap();
    assert_eq!(format!("{:?}", run.structure), before);
    run.structure.assert_invariants();
    assert_eq!(Structure::replay(run.log), run.structure);
}

/// Splitting stops before a side would be too small to use: each side keeps
/// at least `MIN_PANE_COLS` columns or `MIN_PANE_ROWS` rows.
#[test]
fn splits_stop_before_a_pane_gets_too_small() {
    use plx_core::structure::{MIN_PANE_COLS, MIN_PANE_ROWS};
    let area = area_80x24();
    for (direction, min) in [
        (Direction::Down, MIN_PANE_ROWS),
        (Direction::Right, MIN_PANE_COLS),
    ] {
        let mut run = Run::start();
        let split = SessionCommand::Split { direction };
        let mut splits = 0;
        while run.apply(&split, area).is_ok() {
            splits += 1;
            assert!(splits < 100, "splitting stops");
        }
        assert_eq!(
            run.apply(&split, area),
            Err(Rejection::NoRoomToSplit { direction })
        );
        // Every pane is still at least the minimum along the split.
        let arrangement = run.structure.active().arrange(area);
        for placement in &arrangement.panes {
            let rect = placement.rect.expect("every pane is shown");
            assert!(rect.extent(direction.axis()) >= min, "{rect:?}");
        }
    }
}
