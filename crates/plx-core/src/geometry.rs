//! Cell geometry: rectangles, axes and directions.

use core::num::NonZeroU16;

use plx_emulator::GridSize;
use serde::{Deserialize, Serialize};

/// A non-empty rectangle of cells, positioned from the top left, lying
/// within the `u16` coordinate space: its last column and row are addressable,
/// so arithmetic inside it cannot overflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawRect")]
pub struct Rect {
    x: u16,
    y: u16,
    size: GridSize,
}

/// A rectangle as it arrives from outside, before its extent is checked.
#[derive(Deserialize)]
struct RawRect {
    x: u16,
    y: u16,
    size: GridSize,
}

/// A rectangle reaching past the last addressable column or row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a {cols}x{rows} rectangle at ({x}, {y}) reaches past the coordinate space")]
pub struct RectOutOfRange {
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
}

impl Rect {
    /// # Errors
    ///
    /// When the rectangle's last column or row is past `u16::MAX`.
    pub fn new(x: u16, y: u16, size: GridSize) -> Result<Self, RectOutOfRange> {
        let rect = Self { x, y, size };
        let limit = u32::from(u16::MAX) + 1;
        if rect.right() <= limit && rect.bottom() <= limit {
            Ok(rect)
        } else {
            Err(RectOutOfRange {
                x,
                y,
                cols: size.cols().get(),
                rows: size.rows().get(),
            })
        }
    }

    /// A rectangle at the origin.
    #[must_use]
    pub const fn at_origin(size: GridSize) -> Self {
        Self { x: 0, y: 0, size }
    }

    /// The first column.
    #[must_use]
    pub const fn x(self) -> u16 {
        self.x
    }

    /// The first row.
    #[must_use]
    pub const fn y(self) -> u16 {
        self.y
    }

    #[must_use]
    pub const fn size(self) -> GridSize {
        self.size
    }

    /// The part from `offset` spanning `len` cells along `axis`, which lies
    /// inside this rectangle.
    #[must_use]
    #[track_caller]
    pub fn part(self, axis: Axis, offset: u16, len: NonZeroU16) -> Self {
        plx_bug::invariant!(
            u32::from(offset) + u32::from(len.get()) <= u32::from(self.extent(axis)),
            "a part of a rectangle lies inside it",
        );
        match axis {
            Axis::Horizontal => Self {
                x: self.x + offset,
                y: self.y,
                size: self.size.within(len, self.size.rows()),
            },
            Axis::Vertical => Self {
                x: self.x,
                y: self.y + offset,
                size: self.size.within(self.size.cols(), len),
            },
        }
    }

    #[must_use]
    pub const fn cols(self) -> u16 {
        self.size.cols().get()
    }

    #[must_use]
    pub const fn rows(self) -> u16 {
        self.size.rows().get()
    }

    /// One past the last column (`x + cols`), computed in `u32` so it cannot
    /// overflow.
    #[must_use]
    pub fn right(self) -> u32 {
        u32::from(self.x) + u32::from(self.cols())
    }

    /// One past the last row (`y + rows`), computed in `u32`.
    #[must_use]
    pub fn bottom(self) -> u32 {
        u32::from(self.y) + u32::from(self.rows())
    }

    /// Whether the cell at (`x`, `y`) lies inside.
    #[must_use]
    pub fn contains(self, x: u32, y: u32) -> bool {
        x >= u32::from(self.x) && x < self.right() && y >= u32::from(self.y) && y < self.bottom()
    }

    /// The extent along `axis`.
    #[must_use]
    pub const fn extent(self, axis: Axis) -> u16 {
        match axis {
            Axis::Horizontal => self.cols(),
            Axis::Vertical => self.rows(),
        }
    }
}

/// The axis a split divides along.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Axis {
    /// Children side by side, separated by a vertical line.
    Horizontal,
    /// Children stacked, separated by a horizontal line.
    Vertical,
}

/// A direction on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    /// The axis this direction moves along.
    #[must_use]
    pub const fn axis(self) -> Axis {
        match self {
            Self::Left | Self::Right => Axis::Horizontal,
            Self::Up | Self::Down => Axis::Vertical,
        }
    }

    /// Whether this direction points towards larger coordinates.
    #[must_use]
    pub const fn is_forward(self) -> bool {
        match self {
            Self::Right | Self::Down => true,
            Self::Left | Self::Up => false,
        }
    }
}

impl TryFrom<RawRect> for Rect {
    type Error = RectOutOfRange;

    fn try_from(raw: RawRect) -> Result<Self, RectOutOfRange> {
        Self::new(raw.x, raw.y, raw.size)
    }
}
