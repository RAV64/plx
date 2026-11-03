//! The structure of a session: tabs, their layouts, focus and zoom.
//!
//! Changed only by folding [`StructureEvent`]s. [`Structure::apply`] decides a
//! command against the current structure (a [`Rejection`] when it cannot
//! apply) and folds the resulting events at once, handing each to the caller
//! first; nothing can come between deciding and folding, so two clients'
//! commands can never be decided against the same state. The events alone
//! reproduce the structure: `replay(events) == structure`, ids included.
//!
//! Folding is infallible: an event the structure did not decide can only be
//! forged, so a misfit is a bug.

use alloc::vec::Vec;
use core::iter;

use plx_bug::{OrBug as _, invariant};

use crate::command::{SessionCommand, TabSide};
use crate::event::{EndReason, PaneDestination, StructureEvent};
use crate::geometry::{Axis, Direction, Rect};
use crate::id::{IdAllocator, PaneId, TabId};
use crate::layout::{Layout, Ratio, SplitIndex};
use crate::rejection::Rejection;
use crate::tab::{Cover, Tab, Zoom};

/// Most tabs a session holds: a bound against runaway scripts that keeps
/// every tab number two digits wide in the tab bar.
pub const MAX_TABS: usize = 99;

/// Grow step for [`SessionCommand::Grow`], in thousandths of the split.
pub const GROW_STEP: u16 = 50;

/// The smallest pane a split may make: anything smaller is too small to
/// use, and a split's sides shrink to nothing below three cells.
pub const MIN_PANE_COLS: u16 = 5;
pub const MIN_PANE_ROWS: u16 = 5;

/// Whether the session continues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum Lifecycle {
    Running,
    Ended(EndReason),
}

/// The session's tabs: never empty, and the active tab always exists. Tabs
/// left of the active one are in `before` (in order), tabs right of it in
/// `after_rev` (reversed, so the next tab is at the end: switching is a push
/// and a pop).
#[derive(Debug, PartialEq, Eq)]
struct Tabs {
    before: Vec<Tab>,
    active: Tab,
    after_rev: Vec<Tab>,
}

impl Tabs {
    const fn len(&self) -> usize {
        self.before.len() + 1 + self.after_rev.len()
    }

    fn iter(&self) -> impl Iterator<Item = &Tab> {
        self.before
            .iter()
            .chain(iter::once(&self.active))
            .chain(self.after_rev.iter().rev())
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = &mut Tab> {
        self.before
            .iter_mut()
            .chain(iter::once(&mut self.active))
            .chain(self.after_rev.iter_mut().rev())
    }

    /// The active tab's zero-based position.
    const fn active_position(&self) -> usize {
        self.before.len()
    }

    #[track_caller]
    fn get_mut(&mut self, tab: TabId) -> &mut Tab {
        self.iter_mut()
            .find(|t| t.id == tab)
            .or_bug("an event's tab is in the session")
    }

    /// Opens `tab` right after the active tab, and activates it.
    fn open_after_active(&mut self, tab: Tab) {
        let previous = core::mem::replace(&mut self.active, tab);
        self.before.push(previous);
    }

    /// Opens `tab` on `side` of the active tab, and activates it.
    fn open_beside_active(&mut self, tab: Tab, side: TabSide) {
        match side {
            TabSide::Next => self.open_after_active(tab),
            TabSide::Previous => {
                let previous = core::mem::replace(&mut self.active, tab);
                self.after_rev.push(previous);
            }
        }
    }

    /// The tab on `side` of the active one, without wrapping.
    fn beside_active_or_none(&self, side: TabSide) -> Option<&Tab> {
        match side {
            TabSide::Next => self.after_rev.last(),
            TabSide::Previous => self.before.last(),
        }
    }

    /// Moves the active tab one place towards `side`, if it can go.
    fn move_active(&mut self, side: TabSide) {
        match side {
            TabSide::Next => {
                if let Some(next) = self.after_rev.pop() {
                    self.before.push(next);
                }
            }
            TabSide::Previous => {
                if let Some(previous) = self.before.pop() {
                    self.after_rev.push(previous);
                }
            }
        }
    }

    /// Moves one tab right (`Cycle::Next`) or left, wrapping around.
    fn step(&mut self, cycle: Cycle) {
        match cycle {
            Cycle::Next => {
                if self.after_rev.is_empty() {
                    // Wrap: everything before the active tab comes after it.
                    self.after_rev.extend(self.before.drain(..).rev());
                }
                if let Some(next) = self.after_rev.pop() {
                    let previous = core::mem::replace(&mut self.active, next);
                    self.before.push(previous);
                }
            }
            Cycle::Previous => {
                if self.before.is_empty() {
                    self.before.extend(self.after_rev.drain(..).rev());
                }
                if let Some(next) = self.before.pop() {
                    let previous = core::mem::replace(&mut self.active, next);
                    self.after_rev.push(previous);
                }
            }
        }
    }

    /// Activates `tab`, which is in the session.
    #[track_caller]
    fn activate(&mut self, tab: TabId) {
        let target = self
            .iter()
            .position(|t| t.id == tab)
            .or_bug("an activated tab is in the session");
        while self.active_position() < target {
            self.step(Cycle::Next);
        }
        while self.active_position() > target {
            self.step(Cycle::Previous);
        }
    }

    /// Closes `tab`, then activates `active`; there are other tabs.
    #[track_caller]
    fn close(&mut self, tab: TabId, active: TabId) {
        invariant!(self.len() > 1, "the last tab closes by ending the session");
        if self.active.id == tab {
            let next = self
                .after_rev
                .pop()
                .or_else(|| self.before.pop())
                .or_bug("there are other tabs");
            self.active = next;
        } else if let Some(i) = self.before.iter().position(|t| t.id == tab) {
            self.before.remove(i);
        } else {
            let i = self
                .after_rev
                .iter()
                .position(|t| t.id == tab)
                .or_bug("a closed tab is in the session");
            self.after_rev.remove(i);
        }
        self.activate(active);
    }

    /// The tab after (`Cycle::Next`) or before the active one, wrapping.
    fn neighbour(&self, cycle: Cycle) -> &Tab {
        match cycle {
            Cycle::Next => self
                .after_rev
                .last()
                .or_else(|| self.before.first())
                .unwrap_or(&self.active),
            Cycle::Previous => self
                .before
                .last()
                .or_else(|| self.after_rev.first())
                .unwrap_or(&self.active),
        }
    }
}

/// A direction through the tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cycle {
    Next,
    Previous,
}

/// The events a command produces: one or two, never none (a command that
/// changes nothing produces no `Decision::Changed`).
#[derive(Debug)]
struct Events {
    first: StructureEvent,
    second: Option<StructureEvent>,
}

impl Events {
    const fn one(event: StructureEvent) -> Self {
        Self {
            first: event,
            second: None,
        }
    }
}

/// What a command, decided against the current structure, does.
#[derive(Debug)]
enum Decision {
    Unchanged,
    Changed(Events),
    Ended(EndReason),
}

/// The session's tabs and the allocator of their ids. Not `Clone`: the
/// session owns exactly one.
#[derive(Debug, PartialEq, Eq)]
pub struct Structure {
    tabs: Tabs,
    ids: IdAllocator,
}

impl Default for Structure {
    fn default() -> Self {
        Self::new()
    }
}

impl Structure {
    /// A new session: one tab with one pane (the first ids).
    #[must_use]
    pub fn new() -> Self {
        let mut ids = IdAllocator::new();
        let (tab, pane) = (ids.peek_tab(), ids.peek_pane());
        ids.commit_tab(tab);
        ids.commit_pane(pane);
        Self {
            tabs: Tabs {
                before: Vec::new(),
                active: Tab::new(tab, pane),
                after_rev: Vec::new(),
            },
            ids,
        }
    }

    /// A new session's structure after `events` (moved in, never copied).
    #[must_use]
    #[track_caller]
    pub fn replay(events: impl IntoIterator<Item = StructureEvent>) -> Self {
        let mut structure = Self::new();
        for event in events {
            structure.evolve(event);
        }
        structure
    }

    /// The tabs, in order.
    pub fn tabs(&self) -> impl Iterator<Item = &Tab> {
        self.tabs.iter()
    }

    #[must_use]
    pub const fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    #[must_use]
    pub const fn active(&self) -> &Tab {
        &self.tabs.active
    }

    /// The active tab's zero-based position.
    #[must_use]
    pub const fn active_position(&self) -> usize {
        self.tabs.active_position()
    }

    /// The tab holding `pane`, in its layout or hidden behind a cover;
    /// `None` when no tab does.
    #[must_use]
    pub fn tab_holding_or_none(&self, pane: PaneId) -> Option<&Tab> {
        self.tabs.iter().find(|tab| tab.holds(pane))
    }

    /// Every pane of the session, tab by tab in layout order, each tab's
    /// hidden panes after its layout.
    #[must_use]
    pub fn panes(&self) -> Vec<PaneId> {
        self.tabs
            .iter()
            .flat_map(|tab| {
                tab.layout
                    .panes()
                    .into_iter()
                    .chain(tab.covers.iter().map(|cover| cover.covered))
            })
            .collect()
    }

    /// Puts a new pane in the place of the active tab's focused pane, which
    /// runs on hidden until the new one goes; returns the new pane.
    pub fn cover_focus(&mut self, emit: impl FnMut(&StructureEvent)) -> PaneId {
        let tab = &self.tabs.active;
        let cover = self.ids.peek_pane();
        let covered = StructureEvent::PaneCovered {
            tab: tab.id,
            pane: tab.focus,
            cover,
        };
        let _running: Lifecycle = self.fold(Decision::Changed(Events::one(covered)), emit);
        cover
    }

    /// Decides `command` (arranged into `area`) and applies it: each event is
    /// handed to `emit`, then folded. Nothing changes on a rejection.
    ///
    /// # Errors
    ///
    /// A [`Rejection`] when the command cannot apply.
    pub fn apply(
        &mut self,
        command: &SessionCommand,
        area: Rect,
        emit: impl FnMut(&StructureEvent),
    ) -> Result<Lifecycle, Rejection> {
        let decision = self.decide(command, area)?;
        Ok(self.fold(decision, emit))
    }

    /// `pane`, which is in the session, went away (its program exited): it
    /// closes, or its tab, or the whole session.
    #[track_caller]
    pub fn pane_exited(&mut self, pane: PaneId, emit: impl FnMut(&StructureEvent)) -> Lifecycle {
        let tab = self
            .tab_holding_or_none(pane)
            .or_bug("an exiting pane is in the session");
        let decision = if tab.layout.contains(pane) {
            self.close_pane(pane, EndReason::LastPaneClosed)
        } else {
            Decision::Changed(Events::one(StructureEvent::CoveredPaneExited {
                tab: tab.id,
                pane,
            }))
        };
        self.fold(decision, emit)
    }

    fn fold(&mut self, decision: Decision, mut emit: impl FnMut(&StructureEvent)) -> Lifecycle {
        match decision {
            Decision::Unchanged => Lifecycle::Running,
            Decision::Changed(Events { first, second }) => {
                for event in iter::once(first).chain(second) {
                    emit(&event);
                    self.evolve(event);
                }
                Lifecycle::Running
            }
            Decision::Ended(reason) => Lifecycle::Ended(reason),
        }
    }

    /// Focusing `pane` (a click): the active tab must hold it.
    fn decide_focus_pane(&self, pane: PaneId) -> Result<Decision, Rejection> {
        let tab = &self.tabs.active;
        if !tab.layout.contains(pane) {
            return Err(Rejection::NoSuchPane);
        }
        if pane == tab.focus {
            return Ok(Decision::Unchanged);
        }
        Ok(Decision::Changed(Events::one(
            StructureEvent::FocusChanged { tab: tab.id, pane },
        )))
    }

    /// Moving the split at `split` (its separator dragged): the active
    /// tab's layout must have it.
    fn decide_move_split(&self, split: SplitIndex, ratio: Ratio) -> Result<Decision, Rejection> {
        let tab = &self.tabs.active;
        match tab.layout.split_ratio_or_none(split) {
            None => Err(Rejection::NoSuchSplit),
            Some(current) if current == ratio => Ok(Decision::Unchanged),
            Some(_) => Ok(Decision::Changed(Events::one(StructureEvent::SplitMoved {
                tab: tab.id,
                split,
                ratio,
            }))),
        }
    }

    fn decide(&self, command: &SessionCommand, area: Rect) -> Result<Decision, Rejection> {
        let tab = &self.tabs.active;
        let focus = tab.focus;
        let one = |event| Ok(Decision::Changed(Events::one(event)));
        match command {
            SessionCommand::Split { direction } => {
                room_to_split(&tab.layout, focus, *direction, area)?;
                Ok(self.decide_split(*direction))
            }
            SessionCommand::ClosePane => Ok(self.close_pane(focus, EndReason::LastPaneClosed)),
            SessionCommand::Focus { direction } => {
                let target = tab.layout.neighbor_or_none(focus, *direction, area).ok_or(
                    Rejection::NoPaneInDirection {
                        direction: *direction,
                    },
                )?;
                one(StructureEvent::FocusChanged {
                    tab: tab.id,
                    pane: target,
                })
            }
            SessionCommand::Grow { direction } => {
                if !tab.layout.can_grow(focus, *direction, GROW_STEP) {
                    return Err(Rejection::CannotGrow {
                        direction: *direction,
                    });
                }
                one(StructureEvent::PaneGrown {
                    tab: tab.id,
                    pane: focus,
                    direction: *direction,
                    step: GROW_STEP,
                })
            }
            SessionCommand::Swap { direction } => {
                let target = tab.layout.neighbor_or_none(focus, *direction, area).ok_or(
                    Rejection::NoPaneInDirection {
                        direction: *direction,
                    },
                )?;
                one(StructureEvent::PanesSwapped {
                    tab: tab.id,
                    a: focus,
                    b: target,
                })
            }
            SessionCommand::MovePane { direction } => self.decide_move_pane(*direction, area),
            SessionCommand::BreakPane => self.move_pane_to_new_tab(TabSide::Next),
            SessionCommand::MovePaneToTab { side } => self.move_pane_to_tab(*side, area),
            SessionCommand::MoveTab { side } => Ok(self.decide_move_tab(*side)),
            SessionCommand::RotatePanes => {
                if matches!(tab.layout, Layout::Single(_)) {
                    Ok(Decision::Unchanged)
                } else {
                    one(StructureEvent::PanesRotated { tab: tab.id })
                }
            }
            SessionCommand::ToggleZoom => one(StructureEvent::ZoomChanged {
                tab: tab.id,
                zoom: tab.zoom.toggled(),
            }),
            SessionCommand::NewTab => {
                if self.tabs.len() >= MAX_TABS {
                    return Err(Rejection::TooManyTabs);
                }
                one(StructureEvent::TabOpened {
                    tab: self.ids.peek_tab(),
                    pane: self.ids.peek_pane(),
                })
            }
            SessionCommand::CloseTab => Ok(self.close_tab(tab.id)),
            SessionCommand::NextTab => Ok(self.activate(self.tabs.neighbour(Cycle::Next).id)),
            SessionCommand::PreviousTab => {
                Ok(self.activate(self.tabs.neighbour(Cycle::Previous).id))
            }
            SessionCommand::FocusTab { tab: number } => {
                let target = self
                    .tabs
                    .iter()
                    .nth(number.position())
                    .ok_or(Rejection::NoSuchTab { tab: *number })?;
                Ok(self.activate(target.id))
            }
            SessionCommand::RenameTab { name } => one(StructureEvent::TabRenamed {
                tab: tab.id,
                name: name.duplicate(),
            }),
            SessionCommand::KillSession => Ok(Decision::Ended(EndReason::Killed)),
            SessionCommand::FocusPane { pane } => self.decide_focus_pane(*pane),
            SessionCommand::MoveSplit { split, ratio } => self.decide_move_split(*split, *ratio),
        }
    }

    /// The decision for `pane` going away: the pane closes, or its tab, or
    /// the whole session.
    #[track_caller]
    fn close_pane(&self, pane: PaneId, reason_if_last: EndReason) -> Decision {
        let tab = self
            .tab_holding_or_none(pane)
            .or_bug("a closing pane is in the session");
        // A cover going gives the pane it hid its place back.
        if let Some(cover) = tab.cover_of_or_none(pane) {
            return Decision::Changed(Events::one(StructureEvent::PaneUncovered {
                tab: tab.id,
                cover: pane,
                pane: cover.covered,
            }));
        }
        match tab.layout.first_pane_except_or_none(pane) {
            Some(first_remaining) => {
                // Focus stays put unless the focused pane is the one closing;
                // then it moves to the first pane of the remaining layout.
                let focus = if tab.focus == pane {
                    first_remaining
                } else {
                    tab.focus
                };
                Decision::Changed(Events::one(StructureEvent::PaneClosed {
                    pane,
                    tab: tab.id,
                    focus,
                }))
            }
            None if self.tabs.len() == 1 => Decision::Ended(reason_if_last),
            None => self.close_tab(tab.id),
        }
    }

    /// Splitting the focused pane: a zoomed tab shows every pane again.
    const fn decide_split(&self, direction: Direction) -> Decision {
        let tab = &self.tabs.active;
        let opened = StructureEvent::PaneOpened {
            pane: self.ids.peek_pane(),
            tab: tab.id,
            beside: tab.focus,
            direction,
        };
        Decision::Changed(match tab.zoom {
            Zoom::Tiled => Events::one(opened),
            Zoom::Zoomed => Events {
                first: StructureEvent::ZoomChanged {
                    tab: tab.id,
                    zoom: Zoom::Tiled,
                },
                second: Some(opened),
            },
        })
    }

    /// Moving the focused pane towards `direction`: a swap with the
    /// neighbour that way, else across to the tab on the left or right.
    fn decide_move_pane(&self, direction: Direction, area: Rect) -> Result<Decision, Rejection> {
        let tab = &self.tabs.active;
        tab.layout
            .neighbor_or_none(tab.focus, direction, area)
            .map_or_else(
                || match direction {
                    Direction::Left => self.move_pane_to_tab(TabSide::Previous, area),
                    Direction::Right => self.move_pane_to_tab(TabSide::Next, area),
                    Direction::Up | Direction::Down => {
                        Err(Rejection::NoPaneInDirection { direction })
                    }
                },
                |target| {
                    Ok(Decision::Changed(Events::one(
                        StructureEvent::PanesSwapped {
                            tab: tab.id,
                            a: tab.focus,
                            b: target,
                        },
                    )))
                },
            )
    }

    /// Moving the active tab one place towards `side`; at the end of the
    /// row it stays.
    fn decide_move_tab(&self, side: TabSide) -> Decision {
        let tab = self.tabs.active.id;
        self.tabs
            .beside_active_or_none(side)
            .map_or(Decision::Unchanged, |_| {
                Decision::Changed(Events::one(StructureEvent::TabMoved { tab, side }))
            })
    }

    /// Moving the focused pane into the tab on `side`: split in beside its
    /// focus, entering from the side it comes from; with no tab there, into
    /// a new one. The tab it leaves closes if it was its last pane.
    fn move_pane_to_tab(&self, side: TabSide, area: Rect) -> Result<Decision, Rejection> {
        let from = &self.tabs.active;
        let pane = from.focus;
        let Some(to) = self.tabs.beside_active_or_none(side) else {
            return self.move_pane_to_new_tab(side);
        };
        let direction = match side {
            TabSide::Next => Direction::Left,
            TabSide::Previous => Direction::Right,
        };
        // It splits the focused pane there.
        room_to_split(&to.layout, to.focus, direction, area)?;
        let moved = StructureEvent::PaneMovedToTab {
            pane,
            from: from.id,
            left_focus: from.layout.first_pane_except_or_none(pane),
            to: to.id,
            into: PaneDestination::Beside {
                pane: to.focus,
                direction,
            },
        };
        Ok(Decision::Changed(match to.zoom {
            Zoom::Tiled => Events::one(moved),
            // Like a split there, it shows every pane.
            Zoom::Zoomed => Events {
                first: StructureEvent::ZoomChanged {
                    tab: to.id,
                    zoom: Zoom::Tiled,
                },
                second: Some(moved),
            },
        }))
    }

    /// Moving the focused pane into a new tab of its own, on `side` of the
    /// active one. A tab's only pane stays.
    fn move_pane_to_new_tab(&self, side: TabSide) -> Result<Decision, Rejection> {
        let from = &self.tabs.active;
        let pane = from.focus;
        let Some(left_focus) = from.layout.first_pane_except_or_none(pane) else {
            return Ok(Decision::Unchanged);
        };
        if self.tabs.len() >= MAX_TABS {
            return Err(Rejection::TooManyTabs);
        }
        Ok(Decision::Changed(Events::one(
            StructureEvent::PaneMovedToTab {
                pane,
                from: from.id,
                left_focus: Some(left_focus),
                to: self.ids.peek_tab(),
                into: PaneDestination::NewTab { side },
            },
        )))
    }

    fn close_tab(&self, tab: TabId) -> Decision {
        if self.tabs.len() == 1 {
            return Decision::Ended(EndReason::LastTabClosed);
        }
        let active = if self.tabs.active.id == tab {
            // The tab after it becomes active, or the one before if it was last.
            self.tabs
                .after_rev
                .last()
                .or_else(|| self.tabs.before.last())
                .or_bug("there are other tabs")
                .id
        } else {
            self.tabs.active.id
        };
        Decision::Changed(Events::one(StructureEvent::TabClosed { tab, active }))
    }

    fn activate(&self, tab: TabId) -> Decision {
        if tab == self.tabs.active.id {
            Decision::Unchanged
        } else {
            Decision::Changed(Events::one(StructureEvent::TabActivated { tab }))
        }
    }

    /// Folds one event, taking ownership of its data (a renamed tab's name
    /// moves into the tab). The event must fit: the structure decided it, or
    /// replay feeds the events it decided, in order.
    #[track_caller]
    pub fn evolve(&mut self, event: StructureEvent) {
        match event {
            StructureEvent::TabOpened { tab, pane } => {
                self.ids.commit_tab(tab);
                self.ids.commit_pane(pane);
                self.tabs.open_after_active(Tab::new(tab, pane));
            }
            StructureEvent::TabClosed { tab, active } => self.tabs.close(tab, active),
            StructureEvent::TabRenamed { tab, name } => self.tabs.get_mut(tab).name = Some(name),
            StructureEvent::TabActivated { tab } => self.tabs.activate(tab),
            StructureEvent::PaneOpened {
                pane,
                tab,
                beside,
                direction,
            } => {
                self.ids.commit_pane(pane);
                let tab = self.tabs.get_mut(tab);
                tab.layout.split(beside, direction, pane);
                tab.focus = pane;
            }
            StructureEvent::PaneClosed { pane, tab, focus } => {
                let tab = self.tabs.get_mut(tab);
                tab.layout.remove(pane);
                tab.focus = focus;
            }
            StructureEvent::FocusChanged { tab, pane } => self.tabs.get_mut(tab).focus = pane,
            StructureEvent::PaneGrown {
                tab,
                pane,
                direction,
                step,
            } => self.tabs.get_mut(tab).layout.grow(pane, direction, step),
            StructureEvent::PanesSwapped { tab, a, b } => self.tabs.get_mut(tab).layout.swap(a, b),
            StructureEvent::PanesRotated { tab } => self.tabs.get_mut(tab).layout.rotate(),
            StructureEvent::PaneMovedToTab {
                pane,
                from,
                left_focus,
                to,
                into,
            } => self.move_pane(pane, from, left_focus, to, into),
            StructureEvent::TabMoved { tab, side } => {
                self.tabs.activate(tab);
                self.tabs.move_active(side);
            }
            StructureEvent::PaneCovered { tab, pane, cover } => {
                self.ids.commit_pane(cover);
                let tab = self.tabs.get_mut(tab);
                tab.layout.replace(pane, cover);
                if tab.focus == pane {
                    tab.focus = cover;
                }
                tab.covers.push(Cover {
                    cover,
                    covered: pane,
                });
            }
            StructureEvent::PaneUncovered { tab, cover, pane } => {
                let tab = self.tabs.get_mut(tab);
                tab.layout.replace(cover, pane);
                if tab.focus == cover {
                    tab.focus = pane;
                }
                tab.covers.retain(|record| record.cover != cover);
            }
            StructureEvent::CoveredPaneExited { tab, pane } => {
                let tab = self.tabs.get_mut(tab);
                let cover = tab
                    .covers
                    .iter()
                    .find(|record| record.covered == pane)
                    .map(|record| record.cover)
                    .or_bug("an exited hidden pane was behind a cover");
                tab.covers.retain(|record| record.covered != pane);
                // What it hid in turn is now behind its cover.
                for record in &mut tab.covers {
                    if record.cover == pane {
                        record.cover = cover;
                    }
                }
            }
            StructureEvent::ZoomChanged { tab, zoom } => self.tabs.get_mut(tab).zoom = zoom,
            StructureEvent::SplitMoved { tab, split, ratio } => {
                self.tabs.get_mut(tab).layout.move_split(split, ratio);
            }
        }
    }

    /// Folds a `PaneMovedToTab`.
    #[track_caller]
    fn move_pane(
        &mut self,
        pane: PaneId,
        from: TabId,
        left_focus: Option<PaneId>,
        to: TabId,
        into: PaneDestination,
    ) {
        let source = self.tabs.get_mut(from);
        let covers = take_covers(source, pane);
        if let Some(focus) = left_focus {
            source.layout.remove(pane);
            source.focus = focus;
        }
        match into {
            PaneDestination::Beside {
                pane: beside,
                direction,
            } => {
                if left_focus.is_none() {
                    self.tabs.close(from, to);
                }
                let target = self.tabs.get_mut(to);
                target.layout.split(beside, direction, pane);
                target.focus = pane;
                target.covers.extend(covers);
                self.tabs.activate(to);
            }
            PaneDestination::NewTab { side } => {
                self.ids.commit_tab(to);
                let mut tab = Tab::new(to, pane);
                tab.covers = covers;
                self.tabs.activate(from);
                self.tabs.open_beside_active(tab, side);
            }
        }
    }

    /// Panics on any broken structural invariant: every tab and pane appears
    /// once, and every tab's focus is one of its panes. (The zipper makes an
    /// empty session or a missing active tab unrepresentable.)
    #[cfg(any(test, feature = "testkit"))]
    #[track_caller]
    pub fn assert_invariants(&self) {
        let mut tabs = Vec::new();
        let mut panes = Vec::new();
        for tab in self.tabs.iter() {
            invariant!(!tabs.contains(&tab.id), "tab {:?} appears once", tab.id);
            tabs.push(tab.id);
            for pane in tab
                .layout
                .panes()
                .into_iter()
                .chain(tab.covers.iter().map(|cover| cover.covered))
            {
                invariant!(!panes.contains(&pane), "pane {:?} appears once", pane);
                panes.push(pane);
            }
            for record in &tab.covers {
                invariant!(
                    !tab.layout.contains(record.covered),
                    "a hidden pane {:?} is not in the layout",
                    record.covered
                );
                invariant!(
                    tab.holds(record.cover),
                    "the cover {:?} is in its tab",
                    record.cover
                );
            }
            invariant!(
                tab.layout.contains(tab.focus),
                "tab {:?} focuses one of its panes, not {:?}",
                tab.id,
                tab.focus,
            );
        }
        invariant!(
            tabs.len() <= MAX_TABS,
            "a session holds at most {} tabs",
            MAX_TABS
        );
    }
}

/// Takes from `tab` the cover records that go with `pane`: those it stands
/// in for, and in turn those its hidden panes stand in for.
fn take_covers(tab: &mut Tab, pane: PaneId) -> Vec<Cover> {
    let mut taken = Vec::new();
    let mut frontier = Vec::from([pane]);
    while let Some(cover) = frontier.pop() {
        while let Some(index) = tab.covers.iter().position(|record| record.cover == cover) {
            let record = tab.covers.swap_remove(index);
            frontier.push(record.covered);
            taken.push(record);
        }
    }
    taken
}

/// Whether splitting `pane` of `layout` (arranged into `area`, every pane
/// in its place) towards `direction` leaves both sides at least
/// [`MIN_PANE_COLS`] wide or [`MIN_PANE_ROWS`] tall.
fn room_to_split(
    layout: &Layout,
    pane: PaneId,
    direction: Direction,
    area: Rect,
) -> Result<(), Rejection> {
    let no_room = Rejection::NoRoomToSplit { direction };
    let rect = layout
        .arrange(area)
        .visible_rect_or_none(pane)
        .ok_or(no_room)?;
    // One cell goes to the separator; the sides share the rest.
    let (extent, min) = match direction.axis() {
        Axis::Horizontal => (rect.cols(), MIN_PANE_COLS),
        Axis::Vertical => (rect.rows(), MIN_PANE_ROWS),
    };
    if extent.saturating_sub(1) / 2 >= min {
        Ok(())
    } else {
        Err(no_room)
    }
}
