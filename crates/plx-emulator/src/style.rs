//! Cell styles: colours, underline, attributes.

use serde::{Deserialize, Serialize};

/// An RGB colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// A cell colour as the program set it, before palette resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Color {
    /// The terminal's default foreground or background.
    Default,
    /// An index into the 256-colour palette.
    Palette(u8),
    Rgb(Rgb),
}

/// The underline style of a cell (SGR 4 and the kitty `4:n` extension).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Underline {
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// The on/off text attributes of a cell, as a set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u16")]
pub struct Attrs(u16);

/// Attribute bits that name no attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:#06x} sets attribute bits that name no attribute")]
pub struct UnknownAttrs(pub u16);

impl Attrs {
    pub const BOLD: Self = Self(1);
    pub const ITALIC: Self = Self(1 << 1);
    pub const FAINT: Self = Self(1 << 2);
    pub const BLINK: Self = Self(1 << 3);
    pub const INVERSE: Self = Self(1 << 4);
    pub const INVISIBLE: Self = Self(1 << 5);
    pub const STRIKETHROUGH: Self = Self(1 << 6);
    pub const OVERLINE: Self = Self(1 << 7);
    const ALL: Self = Self((1 << 8) - 1);

    /// The empty set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Whether every attribute in `other` is set in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// `self` with every attribute in `other` added.
    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// `self` with every attribute in `other` flipped.
    #[must_use]
    pub const fn toggled(self, other: Self) -> Self {
        Self(self.0 ^ other.0)
    }
}

impl TryFrom<u16> for Attrs {
    type Error = UnknownAttrs;

    fn try_from(bits: u16) -> Result<Self, UnknownAttrs> {
        if bits & !Self::ALL.0 == 0 {
            Ok(Self(bits))
        } else {
            Err(UnknownAttrs(bits))
        }
    }
}

/// The complete visual style of a cell. Interned per pane in [`crate::Tables`];
/// cells refer to it by [`crate::StyleId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub underline_color: Color,
    pub underline: Underline,
    pub attrs: Attrs,
}

impl Style {
    /// No colours, no attributes: what a fresh cell has.
    pub const DEFAULT: Self = Self {
        fg: Color::Default,
        bg: Color::Default,
        underline_color: Color::Default,
        underline: Underline::None,
        attrs: Attrs::empty(),
    };
}
