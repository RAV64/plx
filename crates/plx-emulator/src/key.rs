//! Keys, independent of keyboard layout and terminal encoding.

use core::num::NonZeroU8;

use serde::{Deserialize, Serialize};

/// The character a key types: any Unicode scalar value except the platform
/// function-key codes U+F700–U+F8FF.
///
/// macOS reports arrows and F-keys as those codes. They are not text:
/// libghostty's key encoder must not be given them, and clients send such
/// keys as named [`Key`]s instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "char")]
pub struct KeyChar(char);

/// A platform function-key code where a typed character was expected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("U+{:04X} is a platform function-key code, not a typed character", u32::from(*.0))]
pub struct FunctionKeyCode(pub char);

impl KeyChar {
    const FUNCTION_KEY_CODES: core::ops::RangeInclusive<char> = '\u{F700}'..='\u{F8FF}';

    /// # Errors
    ///
    /// When `c` is a platform function-key code.
    pub const fn new(c: char) -> Result<Self, FunctionKeyCode> {
        if c >= *Self::FUNCTION_KEY_CODES.start() && c <= *Self::FUNCTION_KEY_CODES.end() {
            Err(FunctionKeyCode(c))
        } else {
            Ok(Self(c))
        }
    }

    /// The character for a byte, U+0000 to U+00FF: never a function-key code.
    #[must_use]
    #[expect(
        clippy::as_conversions,
        reason = "u8 to char is lossless; char::from is not const"
    )]
    pub const fn from_byte(byte: u8) -> Self {
        Self(byte as char)
    }

    #[must_use]
    pub const fn get(self) -> char {
        self.0
    }
}

impl TryFrom<char> for KeyChar {
    type Error = FunctionKeyCode;

    fn try_from(c: char) -> Result<Self, FunctionKeyCode> {
        Self::new(c)
    }
}

/// A function key: F1 to F35.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8")]
pub struct FKey(NonZeroU8);

/// A function-key number outside 1 to 35.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("there is no function key F{0}; they run from F1 to F{max}", max = FKey::MAX)]
pub struct NoSuchFunctionKey(pub u8);

impl FKey {
    /// The highest function key.
    pub const MAX: u8 = 35;

    /// # Errors
    ///
    /// When `n` is not 1 to 35.
    pub const fn new(n: u8) -> Result<Self, NoSuchFunctionKey> {
        match NonZeroU8::new(n) {
            Some(n) if n.get() <= Self::MAX => Ok(Self(n)),
            Some(_) | None => Err(NoSuchFunctionKey(n)),
        }
    }

    /// The key's number, 1 to 35.
    #[must_use]
    pub const fn get(self) -> NonZeroU8 {
        self.0
    }
}

impl TryFrom<u8> for FKey {
    type Error = NoSuchFunctionKey;

    fn try_from(n: u8) -> Result<Self, NoSuchFunctionKey> {
        Self::new(n)
    }
}

/// A key, independent of keyboard layout and terminal encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Key {
    /// A key that produces text. Letters are stored as typed (`'A'` for
    /// Shift+a); see [`KeyEvent::chord`] for how bindings compare them.
    Char(KeyChar),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(FKey),
}

/// The modifier keys held during a key event, as a set.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(try_from = "u8")]
pub struct Mods(u8);

/// Modifier bits that name no modifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:#010b} sets modifier bits that name no modifier")]
pub struct UnknownMods(pub u8);

impl Mods {
    pub const SHIFT: Self = Self(1);
    pub const ALT: Self = Self(1 << 1);
    pub const CTRL: Self = Self(1 << 2);
    pub const SUPER: Self = Self(1 << 3);
    const ALL: Self = Self(Self::SHIFT.0 | Self::ALT.0 | Self::CTRL.0 | Self::SUPER.0);

    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    #[must_use]
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

impl TryFrom<u8> for Mods {
    type Error = UnknownMods;

    fn try_from(bits: u8) -> Result<Self, UnknownMods> {
        if bits & !Self::ALL.0 == 0 {
            Ok(Self(bits))
        } else {
            Err(UnknownMods(bits))
        }
    }
}

/// Whether a key went down, auto-repeated, or came up. Terminals without the
/// kitty keyboard protocol only ever report presses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KeyAction {
    Press,
    Repeat,
    Release,
}

/// One keyboard event from a client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyEvent {
    pub key: Key,
    pub mods: Mods,
    pub action: KeyAction,
}

/// A key and its modifiers in canonical form, for comparing an event with a
/// binding. The only constructor is [`KeyChord::new`], which canonicalises;
/// deserialising goes through it too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(from = "RawKeyChord")]
pub struct KeyChord {
    key: Key,
    mods: Mods,
}

/// A chord as it arrives from outside, before it is canonicalised.
#[derive(Deserialize)]
struct RawKeyChord {
    key: Key,
    mods: Mods,
}

impl From<RawKeyChord> for KeyChord {
    fn from(raw: RawKeyChord) -> Self {
        Self::new(raw.key, raw.mods)
    }
}

impl KeyChord {
    /// The canonical chord: an ASCII letter carries its case and never the
    /// Shift modifier, so `Shift+a`, `A` and `Shift+A` are all `A`, and
    /// `Ctrl+Shift+a` is `Ctrl+A`.
    #[must_use]
    pub const fn new(key: Key, mods: Mods) -> Self {
        match key {
            Key::Char(KeyChar(c)) if c.is_ascii_alphabetic() => {
                let c = if mods.contains(Mods::SHIFT) {
                    c.to_ascii_uppercase()
                } else {
                    c
                };
                Self {
                    // An ASCII letter is never a function-key code.
                    key: Key::Char(KeyChar(c)),
                    mods: mods.without(Mods::SHIFT),
                }
            }
            Key::Char(_)
            | Key::Enter
            | Key::Tab
            | Key::Backspace
            | Key::Escape
            | Key::Up
            | Key::Down
            | Key::Left
            | Key::Right
            | Key::Home
            | Key::End
            | Key::PageUp
            | Key::PageDown
            | Key::Insert
            | Key::Delete
            | Key::F(_) => Self { key, mods },
        }
    }

    #[must_use]
    pub const fn key(self) -> Key {
        self.key
    }

    #[must_use]
    pub const fn mods(self) -> Mods {
        self.mods
    }
}

impl KeyEvent {
    /// A key press.
    #[must_use]
    pub const fn press(key: Key, mods: Mods) -> Self {
        Self {
            key,
            mods,
            action: KeyAction::Press,
        }
    }

    /// The canonical chord of this event.
    #[must_use]
    pub const fn chord(&self) -> KeyChord {
        KeyChord::new(self.key, self.mods)
    }
}
