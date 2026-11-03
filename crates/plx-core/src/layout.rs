//! The pane layout of a tab: a binary split tree.
//!
//! A [`Layout`] cannot be empty: a tab's only pane closes with its tab, never
//! by removal. Placing a layout into an area always partitions it exactly:
//! every cell belongs to exactly one visible pane or one separator line.
//!
//! Edits are infallible: the structure decides commands against the layout
//! (a rejection when a command cannot apply) and folds only events it
//! decided, so every edit names panes the layout holds. A miss is a bug.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::num::NonZeroU16;

use plx_bug::{OrBug as _, bug, invariant};
use serde::{Deserialize, Serialize};

use crate::geometry::{Axis, Direction, Rect};
use crate::id::PaneId;

/// Where a split divides its area: the share, in thousandths, of the spare
/// cells (beyond the separator and one cell per child) that the first child
/// gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "u16")]
pub struct Ratio(u16);

// By hand: serde's `into = "u16"` would clone the value to convert it.
impl Serialize for Ratio {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u16(self.0)
    }
}

impl Ratio {
    pub const MIN: u16 = 50;
    pub const MAX: u16 = 950;
    pub const HALF: Self = Self(500);

    /// A ratio in `MIN..=MAX` thousandths.
    #[must_use]
    pub const fn new(per_mille: u16) -> Option<Self> {
        if per_mille >= Self::MIN && per_mille <= Self::MAX {
            Some(Self(per_mille))
        } else {
            None
        }
    }

    /// `per_mille` brought into `MIN..=MAX`.
    #[must_use]
    pub const fn clamped(per_mille: u16) -> Self {
        if per_mille < Self::MIN {
            Self(Self::MIN)
        } else if per_mille > Self::MAX {
            Self(Self::MAX)
        } else {
            Self(per_mille)
        }
    }

    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// A ratio outside `Ratio::MIN..=Ratio::MAX`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("split ratio {0} is outside {min}..={max} thousandths", min = Ratio::MIN, max = Ratio::MAX)]
pub struct InvalidRatio(pub u16);

impl TryFrom<u16> for Ratio {
    type Error = InvalidRatio;

    fn try_from(per_mille: u16) -> Result<Self, Self::Error> {
        Self::new(per_mille).ok_or(InvalidRatio(per_mille))
    }
}

/// A binary split tree of panes. Deliberately not `Clone`: a layout is owned
/// by its tab and changed in place.
#[derive(Debug, PartialEq, Eq, Hash, Serialize)]
pub enum Layout {
    /// One pane filling the area (a whole tab, or one side of a split).
    Single(PaneId),
    Split(Box<Split>),
}

/// An inner node: two children divided along an axis.
#[derive(Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Split {
    axis: Axis,
    ratio: Ratio,
    /// The left (horizontal) or top (vertical) child.
    first: Layout,
    second: Layout,
}

impl Split {
    #[must_use]
    pub const fn axis(&self) -> Axis {
        self.axis
    }

    #[must_use]
    pub const fn ratio(&self) -> Ratio {
        self.ratio
    }

    /// The left (horizontal) or top (vertical) child.
    #[must_use]
    pub const fn first(&self) -> &Layout {
        &self.first
    }

    #[must_use]
    pub const fn second(&self) -> &Layout {
        &self.second
    }
}

/// A pane's place on screen. `rect` is `None` when the area is too small to
/// show the pane at all (a split needs at least 3 cells along its axis).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Placement {
    pub pane: PaneId,
    pub rect: Option<Rect>,
}

/// A separator line between the two children of a visible split.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Separator {
    /// One cell wide (horizontal split) or one cell tall (vertical split).
    pub rect: Rect,
    pub axis: Axis,
    /// The split it divides, so dragging it can move that split.
    pub split: SplitIndex,
}

/// A split's place in its layout, counted in pre-order.
///
/// A split comes before its first child, the first child before the
/// second. It depends only on the layout's shape, never on the area, so an
/// event can name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SplitIndex(pub u16);

/// A layout placed into an area.
#[derive(Debug, Default, PartialEq, Eq, Hash, Serialize)]
pub struct Arrangement {
    /// Every pane, in layout order.
    pub panes: Vec<Placement>,
    pub separators: Vec<Separator>,
}

impl Arrangement {
    /// Where `pane` is shown; `None` when it is not in this arrangement
    /// (another tab's, or gone) or not shown.
    #[must_use]
    pub fn shown_rect_or_none(&self, pane: PaneId) -> Option<Rect> {
        self.panes
            .iter()
            .find(|p| p.pane == pane)
            .and_then(|p| p.rect)
    }

    /// Where `pane`, which the arranged layout holds, is shown: `None` when
    /// the area is too small to show it.
    #[must_use]
    #[track_caller]
    pub fn visible_rect_or_none(&self, pane: PaneId) -> Option<Rect> {
        self.panes
            .iter()
            .find(|p| p.pane == pane)
            .or_bug("only panes of the arranged layout are looked up")
            .rect
    }
}

impl Layout {
    /// A layout holding one pane.
    #[must_use]
    pub const fn single(pane: PaneId) -> Self {
        Self::Single(pane)
    }

    /// The panes in layout order (left to right, top to bottom).
    #[must_use]
    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect_panes(&mut out);
        out
    }

    fn collect_panes(&self, out: &mut Vec<PaneId>) {
        match self {
            Self::Single(pane) => out.push(*pane),
            Self::Split(split) => {
                split.first.collect_panes(out);
                split.second.collect_panes(out);
            }
        }
    }

    /// The first pane in layout order.
    #[must_use]
    pub fn first_pane(&self) -> PaneId {
        match self {
            Self::Single(pane) => *pane,
            Self::Split(split) => split.first.first_pane(),
        }
    }

    /// The first pane in layout order other than `except`: the layout's
    /// first pane once `except` is removed. `None` if `except` is the only
    /// pane (then it closes with its tab).
    #[must_use]
    pub fn first_pane_except_or_none(&self, except: PaneId) -> Option<PaneId> {
        match self {
            Self::Single(pane) => (*pane != except).then_some(*pane),
            Self::Split(split) => split
                .first
                .first_pane_except_or_none(except)
                .or_else(|| split.second.first_pane_except_or_none(except)),
        }
    }

    #[must_use]
    pub fn contains(&self, pane: PaneId) -> bool {
        match self {
            Self::Single(p) => *p == pane,
            Self::Split(split) => split.first.contains(pane) || split.second.contains(pane),
        }
    }

    /// Splits `target`'s space in two and puts `new` (a fresh pane) on the
    /// `direction` side of it, at half the space.
    #[track_caller]
    pub fn split(&mut self, target: PaneId, direction: Direction, new: PaneId) {
        invariant!(!self.contains(new), "a new pane is not in the layout yet");
        let node = self
            .single_mut(target)
            .or_bug("a split's target is in the layout");
        let (first, second) = if direction.is_forward() {
            (target, new)
        } else {
            (new, target)
        };
        *node = Self::Split(Box::new(Split {
            axis: direction.axis(),
            ratio: Ratio::HALF,
            first: Self::Single(first),
            second: Self::Single(second),
        }));
    }

    fn single_mut(&mut self, pane: PaneId) -> Option<&mut Self> {
        match self {
            Self::Single(p) if *p == pane => Some(self),
            Self::Single(_) => None,
            Self::Split(split) => {
                if split.first.contains(pane) {
                    split.first.single_mut(pane)
                } else {
                    split.second.single_mut(pane)
                }
            }
        }
    }

    /// Removes `pane`; its sibling takes over the space of their split. The
    /// layout holds `pane` and at least one other pane (a tab's only pane
    /// closes with its tab).
    #[track_caller]
    pub fn remove(&mut self, pane: PaneId) {
        let Self::Split(split) = self else {
            bug!("a tab's only pane closes with its tab, not by removal");
        };
        let direct_first = matches!(split.first, Self::Single(p) if p == pane);
        let direct_second = matches!(split.second, Self::Single(p) if p == pane);
        if direct_first || direct_second {
            // The removed pane holds the place while the split is taken
            // apart; the sibling overwrites it at once.
            let Self::Split(split) = core::mem::replace(self, Self::Single(pane)) else {
                bug!("the node was matched as a split above");
            };
            let Split { first, second, .. } = *split;
            *self = if direct_first { second } else { first };
            return;
        }
        if split.first.contains(pane) {
            split.first.remove(pane);
        } else {
            invariant!(
                split.second.contains(pane),
                "a removed pane is in the layout"
            );
            split.second.remove(pane);
        }
    }

    /// Exchanges the positions of two panes of the layout.
    #[track_caller]
    pub fn swap(&mut self, a: PaneId, b: PaneId) {
        invariant!(
            self.contains(a) && self.contains(b),
            "swapped panes are in the layout"
        );
        self.relabel(|p| {
            if p == a {
                b
            } else if p == b {
                a
            } else {
                p
            }
        });
    }

    /// Puts `new` in the place of `old`.
    #[track_caller]
    pub fn replace(&mut self, old: PaneId, new: PaneId) {
        invariant!(
            self.contains(old) && !self.contains(new),
            "a pane is replaced by one not yet in the layout"
        );
        self.relabel(|p| if p == old { new } else { p });
    }

    /// Moves every pane one place on, in reading order (the first split's
    /// first side first): the last pane takes the first place. The places
    /// keep their sizes.
    pub fn rotate(&mut self) {
        let mut carry = self.last_pane();
        self.shift(&mut carry);
    }

    fn last_pane(&self) -> PaneId {
        match self {
            Self::Single(pane) => *pane,
            Self::Split(split) => split.second.last_pane(),
        }
    }

    /// Puts `carry` in each place in reading order, carrying on the pane
    /// that was there.
    fn shift(&mut self, carry: &mut PaneId) {
        match self {
            Self::Single(pane) => core::mem::swap(pane, carry),
            Self::Split(split) => {
                split.first.shift(carry);
                split.second.shift(carry);
            }
        }
    }

    fn relabel(&mut self, f: impl Fn(PaneId) -> PaneId + Copy) {
        match self {
            Self::Single(p) => *p = f(*p),
            Self::Split(split) => {
                split.first.relabel(f);
                split.second.relabel(f);
            }
        }
    }

    /// Resizes `pane` towards `direction` by `step` thousandths, as Zellij's
    /// `Resize Increase <direction>` does: the nearest border of the pane on
    /// that side moves that way, growing it; a pane with no border on that
    /// side (at the edge) has its border on the other side move that way
    /// instead, shrinking it. Only for a resize [`Layout::can_grow`] allows.
    #[track_caller]
    pub fn grow(&mut self, pane: PaneId, direction: Direction, step: u16) {
        let border = self.border_to_move(pane, direction);
        invariant!(
            self.grow_inner(pane, direction, step, border),
            "a recorded growth applies to its layout"
        );
    }

    /// The border of `pane` a resize towards `direction` moves: the one on
    /// that side, unless the pane is at the edge there.
    fn border_to_move(&self, pane: PaneId, direction: Direction) -> Border {
        if self.has_border(pane, direction, Border::Ahead) {
            Border::Ahead
        } else {
            Border::Behind
        }
    }

    /// Whether some split holding `pane` has the pane's `border` side on it.
    fn has_border(&self, pane: PaneId, direction: Direction, border: Border) -> bool {
        let Self::Split(split) = self else {
            return false;
        };
        let (side, child) = if split.first.contains(pane) {
            (Side::First, &split.first)
        } else if split.second.contains(pane) {
            (Side::Second, &split.second)
        } else {
            return false;
        };
        border_on(split, side, direction) == Some(border)
            || child.has_border(pane, direction, border)
    }

    /// Whether [`Layout::grow`] would move a split, without changing anything.
    #[must_use]
    pub fn can_grow(&self, pane: PaneId, direction: Direction, step: u16) -> bool {
        self.can_move_border(pane, direction, step, self.border_to_move(pane, direction))
    }

    /// Whether a split holding `pane` with the pane's `border` side on it
    /// can move towards `direction`.
    fn can_move_border(
        &self,
        pane: PaneId,
        direction: Direction,
        step: u16,
        border: Border,
    ) -> bool {
        let Self::Split(split) = self else {
            return false;
        };
        let side = if split.first.contains(pane) {
            Side::First
        } else if split.second.contains(pane) {
            Side::Second
        } else {
            return false;
        };
        let child = if side == Side::First {
            &split.first
        } else {
            &split.second
        };
        child.can_move_border(pane, direction, step, border)
            || next_ratio(split, side, direction, step, border).is_some()
    }

    /// Returns whether a split was moved: the deepest with the pane's
    /// `border` side on it.
    fn grow_inner(
        &mut self,
        pane: PaneId,
        direction: Direction,
        step: u16,
        border: Border,
    ) -> bool {
        let Self::Split(split) = self else {
            return false;
        };
        let side = if split.first.contains(pane) {
            Side::First
        } else {
            Side::Second
        };
        let child = if side == Side::First {
            &mut split.first
        } else {
            &mut split.second
        };
        if child.grow_inner(pane, direction, step, border) {
            return true;
        }
        match next_ratio(split, side, direction, step, border) {
            Some(ratio) => {
                split.ratio = ratio;
                true
            }
            None => false,
        }
    }

    /// Places the layout into `area`.
    #[must_use]
    pub fn arrange(&self, area: Rect) -> Arrangement {
        let mut arrangement = Arrangement::default();
        self.arrange_into(area, &mut arrangement);
        arrangement
    }

    /// Places the layout into `area`, reusing `out`'s buffers (cleared
    /// first): no allocation once they are large enough.
    pub fn arrange_into(&self, area: Rect, out: &mut Arrangement) {
        out.panes.clear();
        out.separators.clear();
        self.place(Some(area), out, &mut 0);
    }

    /// The split at `index`, if the layout has one there.
    fn split_mut_or_none(&mut self, index: SplitIndex, next: &mut u16) -> Option<&mut Split> {
        let Self::Split(split) = self else {
            return None;
        };
        let here = *next;
        *next += 1;
        if here == index.0 {
            return Some(split);
        }
        split
            .first
            .split_mut_or_none(index, next)
            .or_else(|| split.second.split_mut_or_none(index, next))
    }

    fn split_or_none(&self, index: SplitIndex, next: &mut u16) -> Option<&Split> {
        let Self::Split(split) = self else {
            return None;
        };
        let here = *next;
        *next += 1;
        if here == index.0 {
            return Some(split);
        }
        split
            .first
            .split_or_none(index, next)
            .or_else(|| split.second.split_or_none(index, next))
    }

    /// The ratio of the split at `index`, if the layout has one there.
    #[must_use]
    pub fn split_ratio_or_none(&self, index: SplitIndex) -> Option<Ratio> {
        self.split_or_none(index, &mut 0).map(Split::ratio)
    }

    /// Moves the split at `index` to `ratio`. Only for a split the layout
    /// has (the decision checked it).
    #[track_caller]
    pub fn move_split(&mut self, index: SplitIndex, ratio: Ratio) {
        let split = self.split_mut_or_none(index, &mut 0);
        invariant!(
            split.is_some(),
            "a recorded split move names a split of its layout"
        );
        if let Some(split) = split {
            split.ratio = ratio;
        }
    }

    /// The ratio that puts the separator of the split at `index` at
    /// `position` (a column for side-by-side panes, a row for stacked ones)
    /// when the layout fills `area`, as near as thousandths allow. `None`
    /// when that split is not shown.
    #[must_use]
    pub fn ratio_at_or_none(&self, index: SplitIndex, position: u16, area: Rect) -> Option<Ratio> {
        let (area, axis) = self.split_area_or_none(index, area, &mut 0)?;
        let start = match axis {
            Axis::Horizontal => area.x(),
            Axis::Vertical => area.y(),
        };
        let extent = area.extent(axis);
        if extent < 3 {
            return None;
        }
        // The separator sits after the first child, which keeps at least
        // one cell, as does the second.
        let first_len = position.saturating_sub(start).clamp(1, extent - 2);
        let spare = u32::from(extent - 3);
        let first_extra = u32::from(first_len - 1);
        let per_mille = match spare {
            0 => u32::from(Ratio::HALF.get()),
            // Rounded up, so the arrangement's rounding down lands on it.
            _ => (first_extra * 1000).div_ceil(spare),
        };
        Some(Ratio::clamped(
            u16::try_from(per_mille.min(1000)).or_bug("at most 1000 fits u16"),
        ))
    }

    /// The area and axis of the split at `index` when the layout fills
    /// `area`; `None` when it is not shown.
    fn split_area_or_none(
        &self,
        index: SplitIndex,
        area: Rect,
        next: &mut u16,
    ) -> Option<(Rect, Axis)> {
        let Self::Split(split) = self else {
            return None;
        };
        let here = *next;
        *next += 1;
        if here == index.0 {
            return Some((area, split.axis));
        }
        match divide(area, split) {
            Division::Both { first, second, .. } => split
                .first
                .split_area_or_none(index, first, next)
                .or_else(|| split.second.split_area_or_none(index, second, next)),
            Division::FirstOnly(first) => {
                let found = split.first.split_area_or_none(index, first, next);
                // The hidden second child's splits still count.
                *next += split.second.split_count();
                found
            }
        }
    }

    /// How many splits the layout holds.
    fn split_count(&self) -> u16 {
        match self {
            Self::Single(_) => 0,
            Self::Split(split) => 1 + split.first.split_count() + split.second.split_count(),
        }
    }

    /// Calls `f` with each pane in layout order, without collecting them.
    pub fn for_each_pane(&self, f: &mut impl FnMut(PaneId)) {
        match self {
            Self::Single(pane) => f(*pane),
            Self::Split(split) => {
                split.first.for_each_pane(f);
                split.second.for_each_pane(f);
            }
        }
    }

    /// Places the layout into `area`; `next` counts splits in pre-order.
    fn place(&self, area: Option<Rect>, out: &mut Arrangement, next: &mut u16) {
        match self {
            Self::Single(pane) => out.panes.push(Placement {
                pane: *pane,
                rect: area,
            }),
            Self::Split(split) => {
                let index = SplitIndex(*next);
                *next += 1;
                match area.map(|area| divide(area, split)) {
                    Some(Division::Both {
                        first,
                        separator,
                        second,
                    }) => {
                        out.separators.push(Separator {
                            rect: separator,
                            axis: split.axis,
                            split: index,
                        });
                        split.first.place(Some(first), out, next);
                        split.second.place(Some(second), out, next);
                    }
                    Some(Division::FirstOnly(first)) => {
                        split.first.place(Some(first), out, next);
                        split.second.place(None, out, next);
                    }
                    None => {
                        split.first.place(None, out, next);
                        split.second.place(None, out, next);
                    }
                }
            }
        }
    }

    /// The pane next to `from` in `direction` within `area`: among the
    /// visible panes directly across the separator, the one sharing the most
    /// cells with `from` along the other axis (ties: the topmost/leftmost).
    /// `None` when there is none, or `from` is hidden.
    #[must_use]
    pub fn neighbor_or_none(
        &self,
        from: PaneId,
        direction: Direction,
        area: Rect,
    ) -> Option<PaneId> {
        let arrangement = self.arrange(area);
        let origin = arrangement.visible_rect_or_none(from)?;
        arrangement
            .panes
            .iter()
            .filter_map(|p| p.rect.map(|rect| (p.pane, rect)))
            .filter(|(pane, rect)| *pane != from && is_across(origin, *rect, direction))
            .map(|(pane, rect)| (overlap(origin, rect, direction.axis()), rect, pane))
            .filter(|(shared, _, _)| *shared > 0)
            .max_by(|(shared_a, rect_a, _), (shared_b, rect_b, _)| {
                shared_a
                    .cmp(shared_b)
                    // Prefer the smaller coordinate on ties: reverse it.
                    .then_with(|| (rect_b.y(), rect_b.x()).cmp(&(rect_a.y(), rect_a.x())))
            })
            .map(|(_, _, pane)| pane)
    }
}

/// Which child of a split a pane is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    First,
    Second,
}

/// The ratio `split` moves to when a pane on `side` grows towards
/// `direction` by `step`, or `None` when this split cannot help: wrong axis,
/// wrong side, or already at its limit.
/// Which of a pane's borders a resize towards a direction moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Border {
    /// The one on that side: the pane grows.
    Ahead,
    /// The one on the other side, when the pane is at the edge: it shrinks.
    Behind,
}

/// Which border of a pane in the `side` child the divider of `split` is,
/// looking towards `direction`; `None` across the split's axis.
fn border_on(split: &Split, side: Side, direction: Direction) -> Option<Border> {
    if split.axis != direction.axis() {
        return None;
    }
    // The divider is ahead of a pane in the first child going forward, and
    // of one in the second child going backward.
    Some(match (direction.is_forward(), side) {
        (true, Side::First) | (false, Side::Second) => Border::Ahead,
        (true, Side::Second) | (false, Side::First) => Border::Behind,
    })
}

/// The ratio of `split` once its divider moves `step` towards `direction`,
/// if that divider is the `border` of a pane in its `side` child.
fn next_ratio(
    split: &Split,
    side: Side,
    direction: Direction,
    step: u16,
    border: Border,
) -> Option<Ratio> {
    if border_on(split, side, direction) != Some(border) {
        return None;
    }
    let moved = if direction.is_forward() {
        Ratio::clamped(split.ratio.get().saturating_add(step))
    } else {
        Ratio::clamped(split.ratio.get().saturating_sub(step))
    };
    (moved != split.ratio).then_some(moved)
}

/// How a split shares out its area.
enum Division {
    /// Both children shown, with the separator between them.
    Both {
        first: Rect,
        separator: Rect,
        second: Rect,
    },
    /// Too small to divide: the first child gets everything, the second is
    /// hidden.
    FirstOnly(Rect),
}

/// Divides `area` for `split`. A split needs 3 cells along its axis (one
/// each plus the separator).
fn divide(area: Rect, split: &Split) -> Division {
    let extent = area.extent(split.axis);
    if extent < 3 {
        return Division::FirstOnly(area);
    }
    // Both children get one cell for sure; the ratio shares out the rest, so
    // neither length can be zero by construction.
    let spare = extent - 3;
    let first_extra = scale(spare, split.ratio);
    let first_len = NonZeroU16::MIN.saturating_add(first_extra);
    let second_len = NonZeroU16::MIN.saturating_add(spare - first_extra);
    Division::Both {
        first: area.part(split.axis, 0, first_len),
        separator: area.part(split.axis, first_len.get(), NonZeroU16::MIN),
        second: area.part(split.axis, first_len.get() + 1, second_len),
    }
}

/// `spare * ratio / 1000`, rounded down; at most `spare`.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "the quotient is at most `spare`, which is a u16"
)]
fn scale(spare: u16, ratio: Ratio) -> u16 {
    (u32::from(spare) * u32::from(ratio.get()) / 1000) as u16
}

/// Whether `rect` lies directly across the separator from `origin` in
/// `direction`.
fn is_across(origin: Rect, rect: Rect, direction: Direction) -> bool {
    match direction {
        Direction::Right => u32::from(rect.x()) == origin.right() + 1,
        Direction::Left => rect.right() + 1 == u32::from(origin.x()),
        Direction::Down => u32::from(rect.y()) == origin.bottom() + 1,
        Direction::Up => rect.bottom() + 1 == u32::from(origin.y()),
    }
}

/// How many cells two rectangles share along the axis perpendicular to
/// `axis` (rows for a horizontal move, columns for a vertical one).
fn overlap(a: Rect, b: Rect, axis: Axis) -> u32 {
    let (a_start, a_end, b_start, b_end) = match axis {
        Axis::Horizontal => (u32::from(a.y()), a.bottom(), u32::from(b.y()), b.bottom()),
        Axis::Vertical => (u32::from(a.x()), a.right(), u32::from(b.x()), b.right()),
    };
    a_end.min(b_end).saturating_sub(a_start.max(b_start))
}
