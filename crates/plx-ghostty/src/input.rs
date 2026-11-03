//! Encoding client input for a pane: keys through libghostty-vt's key
//! encoder (which follows the pane's own terminal modes), and pastes through
//! its paste encoder.

use core::ffi::c_char;
use core::ptr::{self, NonNull};

use plx_bug::{OrBug as _, contract};
use plx_emulator::{
    CellPx, FKey, Key, KeyAction, KeyEvent, Mods, MouseAction, MouseButton, MouseEvent,
};
use plx_ghostty_sys as sys;

use crate::error::{Error, adopt, check, check_contract, header_u16};

/// libghostty-vt's modifier bits (`GHOSTTY_MODS_*` in `key/event.h`), as the
/// `u16` the key event takes; each is checked to fit at compile time.
pub(crate) const MODS_SHIFT: sys::GhosttyMods = header_u16!(sys::GHOSTTY_MODS_SHIFT);
pub(crate) const MODS_CTRL: sys::GhosttyMods = header_u16!(sys::GHOSTTY_MODS_CTRL);
pub(crate) const MODS_ALT: sys::GhosttyMods = header_u16!(sys::GHOSTTY_MODS_ALT);
pub(crate) const MODS_SUPER: sys::GhosttyMods = header_u16!(sys::GHOSTTY_MODS_SUPER);

/// Encoded sequences up to this many bytes need no allocation retry.
const INLINE_BYTES: usize = 64;

/// A key encoder plus a reusable key event, owned by one emulator.
pub(crate) struct KeyEncoder {
    encoder: EncoderHandle,
    event: EventHandle,
}

impl KeyEncoder {
    /// # Errors
    ///
    /// When libghostty-vt cannot create the encoder or its event.
    pub(crate) fn new() -> Result<Self, Error> {
        // Each handle frees itself on drop, so a failure creating the event
        // cannot leak the encoder.
        Ok(Self {
            encoder: EncoderHandle::new()?,
            event: EventHandle::new()?,
        })
    }

    /// Appends the encoding of `key` for the current modes of `terminal`.
    pub(crate) fn encode(&self, terminal: sys::GhosttyTerminal, key: &KeyEvent, out: &mut Vec<u8>) {
        let physical = match Encoding::of(key) {
            Encoding::Physical(physical) => physical,
            Encoding::ControlChar(c) => {
                push_control_char(c, key.action, out);
                return;
            }
        };
        let encoder = self.encoder.0.as_ptr();
        let event = self.event.0.as_ptr();
        let mut text = [0_u8; 4];
        let utf8 = physical.text.map(|c| c.encode_utf8(&mut text).as_bytes());
        let alt_esc_prefix = true;
        // SAFETY: both handles are live and owned by `self`; `terminal` is live
        // (the emulator passes its own). `utf8` borrows `text`, which outlives
        // the encode call below, and the event does not keep the pointer past
        // it (we overwrite it on every call).
        unsafe {
            sys::ghostty_key_encoder_setopt_from_terminal(encoder, terminal);
            // xterm's metaSendsEscape: Alt+x sends ESC x.
            sys::ghostty_key_encoder_setopt(
                encoder,
                sys::GhosttyKeyEncoderOption::GHOSTTY_KEY_ENCODER_OPT_ALT_ESC_PREFIX,
                (&raw const alt_esc_prefix).cast(),
            );
            sys::ghostty_key_event_set_action(event, action(key.action));
            sys::ghostty_key_event_set_key(event, physical.key);
            sys::ghostty_key_event_set_mods(event, physical.mods);
            sys::ghostty_key_event_set_consumed_mods(event, physical.consumed);
            sys::ghostty_key_event_set_composing(event, false);
            let (text_ptr, text_len) = utf8.map_or((ptr::null(), 0), |bytes| {
                (bytes.as_ptr().cast(), bytes.len())
            });
            sys::ghostty_key_event_set_utf8(event, text_ptr, text_len);
            sys::ghostty_key_event_set_unshifted_codepoint(
                event,
                physical.unshifted.map_or(0, u32::from),
            );
        }
        let result = encode_with_retry(out, |buf, cap, written| {
            // SAFETY: the handles are live; `buf` is valid for `cap` bytes (or
            // null with `cap == 0`) and `written` is a valid out-pointer.
            unsafe { sys::ghostty_key_encoder_encode(encoder, event, buf, cap, written) }
        });
        // The header allows "other error codes", but the encoder fails only
        // for lack of space (handled by the retry) given a valid event: no
        // control character or function-key code as text, which `Encoding`
        // and `KeyChar` rule out.
        check_contract("ghostty_key_encoder_encode", result);
    }
}

/// An owned key encoder.
struct EncoderHandle(NonNull<sys::GhosttyKeyEncoderImpl>);

impl EncoderHandle {
    fn new() -> Result<Self, Error> {
        const CALL: &str = "ghostty_key_encoder_new";
        let mut raw: sys::GhosttyKeyEncoder = ptr::null_mut();
        // SAFETY: valid out-pointer and allocator.
        check(CALL, unsafe {
            sys::ghostty_key_encoder_new(crate::alloc::rust(), &raw mut raw)
        })?;
        Ok(Self(adopt(raw, CALL)))
    }
}

impl Drop for EncoderHandle {
    fn drop(&mut self) {
        // SAFETY: owned by this value and freed exactly once.
        unsafe { sys::ghostty_key_encoder_free(self.0.as_ptr()) };
    }
}

/// An owned key event, refilled for every key.
struct EventHandle(NonNull<sys::GhosttyKeyEventImpl>);

impl EventHandle {
    fn new() -> Result<Self, Error> {
        const CALL: &str = "ghostty_key_event_new";
        let mut raw: sys::GhosttyKeyEvent = ptr::null_mut();
        // SAFETY: valid out-pointer and allocator.
        check(CALL, unsafe {
            sys::ghostty_key_event_new(crate::alloc::rust(), &raw mut raw)
        })?;
        Ok(Self(adopt(raw, CALL)))
    }
}

impl Drop for EventHandle {
    fn drop(&mut self) {
        // SAFETY: owned by this value and freed exactly once.
        unsafe { sys::ghostty_key_event_free(self.0.as_ptr()) };
    }
}

/// A mouse encoder plus a reusable mouse event, owned by one emulator.
pub(crate) struct MouseEncoder {
    encoder: MouseEncoderHandle,
    event: MouseEventHandle,
}

/// The cell size given to the mouse encoder when the client reported none:
/// it converts positions through pixels, and cell reports come out exact
/// whatever the size.
const FALLBACK_CELL_PX: u32 = 10;

impl MouseEncoder {
    /// # Errors
    ///
    /// When libghostty-vt cannot create the encoder or its event.
    pub(crate) fn new() -> Result<Self, Error> {
        Ok(Self {
            encoder: MouseEncoderHandle::new()?,
            event: MouseEventHandle::new()?,
        })
    }

    /// Appends the encoding of `mouse` (a cell of the `cols`×`rows`
    /// terminal) for the mouse modes `terminal` has set; nothing when they
    /// ignore it.
    pub(crate) fn encode(
        &self,
        terminal: sys::GhosttyTerminal,
        (cols, rows): (u16, u16),
        cell_px: Option<CellPx>,
        mouse: &MouseEvent,
        out: &mut Vec<u8>,
    ) {
        let (cell_width, cell_height) = cell_px
            .map_or((FALLBACK_CELL_PX, FALLBACK_CELL_PX), |px| {
                (px.width.get(), px.height.get())
            });
        let size = sys::GhosttyMouseEncoderSize {
            size: size_of::<sys::GhosttyMouseEncoderSize>(),
            screen_width: u32::from(cols).saturating_mul(cell_width),
            screen_height: u32::from(rows).saturating_mul(cell_height),
            cell_width,
            cell_height,
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        };
        let pressed = match (mouse.action, mouse.button) {
            (MouseAction::Release, _) | (MouseAction::Motion, MouseButton::None) => false,
            (MouseAction::Press | MouseAction::Motion, _) => true,
        };
        // The middle of the cell, in the encoder's pixels.
        let centre = |cell: u16, px: u32| {
            #[expect(
                clippy::as_conversions,
                clippy::cast_precision_loss,
                reason = "cells and pixels of a terminal are far below f32's exact range"
            )]
            let pixels = (u32::from(cell).saturating_mul(px) + px / 2) as f32;
            pixels
        };
        let position = sys::GhosttyMousePosition {
            x: centre(mouse.position.x, cell_width),
            y: centre(mouse.position.y, cell_height),
        };
        let encoder = self.encoder.0.as_ptr();
        let event = self.event.0.as_ptr();
        // SAFETY: both handles are live and owned by `self`; `terminal` is
        // live (the emulator passes its own). Each option's value has the
        // type its documentation names, and lives across the call.
        unsafe {
            sys::ghostty_mouse_encoder_setopt_from_terminal(encoder, terminal);
            sys::ghostty_mouse_encoder_setopt(
                encoder,
                sys::GhosttyMouseEncoderOption::GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                (&raw const size).cast(),
            );
            sys::ghostty_mouse_encoder_setopt(
                encoder,
                sys::GhosttyMouseEncoderOption::GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED,
                (&raw const pressed).cast(),
            );
            sys::ghostty_mouse_event_set_action(event, mouse_action(mouse.action));
            match mouse_button(mouse.button) {
                Some(button) => sys::ghostty_mouse_event_set_button(event, button),
                None => sys::ghostty_mouse_event_clear_button(event),
            }
            sys::ghostty_mouse_event_set_mods(event, ghostty_mods(mouse.mods));
            sys::ghostty_mouse_event_set_position(event, position);
        }
        let result = encode_with_retry(out, |buf, cap, written| {
            // SAFETY: the handles are live; `buf` is valid for `cap` bytes
            // (or null with `cap == 0`) and `written` is a valid out-pointer.
            unsafe { sys::ghostty_mouse_encoder_encode(encoder, event, buf, cap, written) }
        });
        // The encoder fails only for lack of space, which the retry handles.
        check_contract("ghostty_mouse_encoder_encode", result);
    }
}

const fn mouse_action(action: MouseAction) -> sys::GhosttyMouseAction {
    match action {
        MouseAction::Press => sys::GhosttyMouseAction::GHOSTTY_MOUSE_ACTION_PRESS,
        MouseAction::Release => sys::GhosttyMouseAction::GHOSTTY_MOUSE_ACTION_RELEASE,
        MouseAction::Motion => sys::GhosttyMouseAction::GHOSTTY_MOUSE_ACTION_MOTION,
    }
}

/// libghostty's button numbers follow xterm's: 4 and 5 are the wheel up
/// and down, 6 and 7 left and right.
const fn mouse_button(button: MouseButton) -> Option<sys::GhosttyMouseButton> {
    use sys::GhosttyMouseButton as B;
    Some(match button {
        MouseButton::Left => B::GHOSTTY_MOUSE_BUTTON_LEFT,
        MouseButton::Middle => B::GHOSTTY_MOUSE_BUTTON_MIDDLE,
        MouseButton::Right => B::GHOSTTY_MOUSE_BUTTON_RIGHT,
        MouseButton::WheelUp => B::GHOSTTY_MOUSE_BUTTON_FOUR,
        MouseButton::WheelDown => B::GHOSTTY_MOUSE_BUTTON_FIVE,
        MouseButton::WheelLeft => B::GHOSTTY_MOUSE_BUTTON_SIX,
        MouseButton::WheelRight => B::GHOSTTY_MOUSE_BUTTON_SEVEN,
        MouseButton::None => return None,
    })
}

/// An owned mouse encoder.
struct MouseEncoderHandle(NonNull<sys::GhosttyMouseEncoderImpl>);

impl MouseEncoderHandle {
    fn new() -> Result<Self, Error> {
        const CALL: &str = "ghostty_mouse_encoder_new";
        let mut raw: sys::GhosttyMouseEncoder = ptr::null_mut();
        // SAFETY: valid out-pointer and allocator.
        check(CALL, unsafe {
            sys::ghostty_mouse_encoder_new(crate::alloc::rust(), &raw mut raw)
        })?;
        Ok(Self(adopt(raw, CALL)))
    }
}

impl Drop for MouseEncoderHandle {
    fn drop(&mut self) {
        // SAFETY: owned by this value and freed exactly once.
        unsafe { sys::ghostty_mouse_encoder_free(self.0.as_ptr()) };
    }
}

/// An owned mouse event, refilled for every report.
struct MouseEventHandle(NonNull<sys::GhosttyMouseEventImpl>);

impl MouseEventHandle {
    fn new() -> Result<Self, Error> {
        const CALL: &str = "ghostty_mouse_event_new";
        let mut raw: sys::GhosttyMouseEvent = ptr::null_mut();
        // SAFETY: valid out-pointer and allocator.
        check(CALL, unsafe {
            sys::ghostty_mouse_event_new(crate::alloc::rust(), &raw mut raw)
        })?;
        Ok(Self(adopt(raw, CALL)))
    }
}

impl Drop for MouseEventHandle {
    fn drop(&mut self) {
        // SAFETY: owned by this value and freed exactly once.
        unsafe { sys::ghostty_mouse_event_free(self.0.as_ptr()) };
    }
}

/// How the program wants pastes (DEC mode 2004).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PasteMode {
    /// As typed text, with newlines as carriage returns.
    Plain,
    /// Between bracketed-paste markers.
    Bracketed,
}

/// Appends the encoding of a paste: control bytes become spaces, and the text
/// is framed for `mode`. The encoder rewrites its input in place, so the text
/// goes through `scratch`, a reused buffer, never a fresh allocation.
pub(crate) fn encode_paste(text: &str, mode: PasteMode, scratch: &mut Vec<u8>, out: &mut Vec<u8>) {
    let bracketed = mode == PasteMode::Bracketed;
    scratch.clear();
    scratch.extend_from_slice(text.as_bytes());
    let data_ptr: *mut c_char = scratch.as_mut_ptr().cast();
    let data_len = scratch.len();
    let result = encode_with_retry(out, |buf, cap, written| {
        // SAFETY: `scratch` is valid for `data_len` writable bytes and outlives
        // the call; `buf` is valid for `cap` bytes (or null with `cap == 0`);
        // `written` is a valid out-pointer. Re-running on the already
        // sanitised data is idempotent (unsafe bytes are already spaces).
        unsafe { sys::ghostty_paste_encode(data_ptr, data_len, bracketed, buf, cap, written) }
    });
    // Only OUT_OF_SPACE is documented, and the retry handles it.
    check_contract("ghostty_paste_encode", result);
}

/// Runs an encoder that reports `OUT_OF_SPACE` with the required size and
/// appends its output to `out`: first through a stack buffer, and if that is
/// too small, directly into `out` grown to the required size. Returns the
/// final call's result; on failure nothing is appended.
fn encode_with_retry(
    out: &mut Vec<u8>,
    mut encode: impl FnMut(*mut c_char, usize, *mut usize) -> sys::GhosttyResult,
) -> sys::GhosttyResult {
    let mut inline = [0_u8; INLINE_BYTES];
    let mut written = 0_usize;
    let result = encode(inline.as_mut_ptr().cast(), inline.len(), &raw mut written);
    if result == sys::GhosttyResult::GHOSTTY_OUT_OF_SPACE {
        let start = out.len();
        out.resize(start + written, 0);
        let tail = out
            .get_mut(start..)
            .or_bug("the buffer was just grown past its old length");
        let capacity = tail.len();
        let result = encode(tail.as_mut_ptr().cast(), capacity, &raw mut written);
        if result == sys::GhosttyResult::GHOSTTY_SUCCESS {
            contract!(
                written <= capacity,
                "an encoder writes at most the space it reported needing: {} of {}",
                written,
                capacity,
            );
            out.truncate(start + written);
        } else {
            out.truncate(start);
        }
        return result;
    }
    if result == sys::GhosttyResult::GHOSTTY_SUCCESS {
        let encoded = inline
            .get(..written)
            .or_contract("an encoder writes at most the buffer it is given");
        out.extend_from_slice(encoded);
    }
    result
}

const fn action(action: KeyAction) -> sys::GhosttyKeyAction {
    match action {
        KeyAction::Press => sys::GhosttyKeyAction::GHOSTTY_KEY_ACTION_PRESS,
        KeyAction::Repeat => sys::GhosttyKeyAction::GHOSTTY_KEY_ACTION_REPEAT,
        KeyAction::Release => sys::GhosttyKeyAction::GHOSTTY_KEY_ACTION_RELEASE,
    }
}

/// How a plx key reaches the program.
enum Encoding {
    /// Through libghostty-vt's key encoder, as a physical key.
    Physical(PhysicalKey),
    /// A control character typed as text, which the encoder does not take:
    /// sent as its own bytes.
    ControlChar(char),
}

/// A plx key in libghostty-vt's model: a physical key (US layout for ASCII),
/// modifiers, the text the key types, and its unshifted codepoint.
struct PhysicalKey {
    key: sys::GhosttyKey,
    mods: sys::GhosttyMods,
    /// The modifiers that went into the text: Shift for a shifted
    /// character. The kitty protocol sends such a key as its text unless
    /// the program asked for every key as an escape code.
    consumed: sys::GhosttyMods,
    text: Option<char>,
    unshifted: Option<char>,
}

impl Encoding {
    fn of(event: &KeyEvent) -> Self {
        let mut mods = ghostty_mods(event.mods);
        let named = |key| {
            Self::Physical(PhysicalKey {
                key,
                mods,
                consumed: 0,
                text: None,
                unshifted: None,
            })
        };
        match event.key {
            Key::Char(c) if c.get().is_control() => Self::ControlChar(c.get()),
            Key::Char(c) => {
                let c = c.get();
                let (key, unshifted, shifted) = us_layout(c);
                // Shift typed this character (`:`, `A`, `Ä`): it is part of
                // the text, as a keyboard layout reports it.
                let consumed = if shifted || c.is_uppercase() {
                    mods |= MODS_SHIFT;
                    MODS_SHIFT
                } else {
                    0
                };
                Self::Physical(PhysicalKey {
                    key,
                    mods,
                    consumed,
                    text: Some(c),
                    unshifted: Some(unshifted),
                })
            }
            Key::Enter => named(sys::GhosttyKey::GHOSTTY_KEY_ENTER),
            Key::Tab => named(sys::GhosttyKey::GHOSTTY_KEY_TAB),
            Key::Backspace => named(sys::GhosttyKey::GHOSTTY_KEY_BACKSPACE),
            Key::Escape => named(sys::GhosttyKey::GHOSTTY_KEY_ESCAPE),
            Key::Up => named(sys::GhosttyKey::GHOSTTY_KEY_ARROW_UP),
            Key::Down => named(sys::GhosttyKey::GHOSTTY_KEY_ARROW_DOWN),
            Key::Left => named(sys::GhosttyKey::GHOSTTY_KEY_ARROW_LEFT),
            Key::Right => named(sys::GhosttyKey::GHOSTTY_KEY_ARROW_RIGHT),
            Key::Home => named(sys::GhosttyKey::GHOSTTY_KEY_HOME),
            Key::End => named(sys::GhosttyKey::GHOSTTY_KEY_END),
            Key::PageUp => named(sys::GhosttyKey::GHOSTTY_KEY_PAGE_UP),
            Key::PageDown => named(sys::GhosttyKey::GHOSTTY_KEY_PAGE_DOWN),
            Key::Insert => named(sys::GhosttyKey::GHOSTTY_KEY_INSERT),
            Key::Delete => named(sys::GhosttyKey::GHOSTTY_KEY_DELETE),
            Key::F(n) => named(function_key(n)),
        }
    }
}

fn ghostty_mods(mods: Mods) -> sys::GhosttyMods {
    [
        (Mods::SHIFT, MODS_SHIFT),
        (Mods::CTRL, MODS_CTRL),
        (Mods::ALT, MODS_ALT),
        (Mods::SUPER, MODS_SUPER),
    ]
    .into_iter()
    .filter(|(plx, _)| mods.contains(*plx))
    .fold(0, |acc, (_, bit)| acc | bit)
}

/// The US layout: each physical key with its plain and shifted character.
const US_LAYOUT: [(char, char, sys::GhosttyKey); 47] = {
    use sys::GhosttyKey as K;
    [
        ('a', 'A', K::GHOSTTY_KEY_A),
        ('b', 'B', K::GHOSTTY_KEY_B),
        ('c', 'C', K::GHOSTTY_KEY_C),
        ('d', 'D', K::GHOSTTY_KEY_D),
        ('e', 'E', K::GHOSTTY_KEY_E),
        ('f', 'F', K::GHOSTTY_KEY_F),
        ('g', 'G', K::GHOSTTY_KEY_G),
        ('h', 'H', K::GHOSTTY_KEY_H),
        ('i', 'I', K::GHOSTTY_KEY_I),
        ('j', 'J', K::GHOSTTY_KEY_J),
        ('k', 'K', K::GHOSTTY_KEY_K),
        ('l', 'L', K::GHOSTTY_KEY_L),
        ('m', 'M', K::GHOSTTY_KEY_M),
        ('n', 'N', K::GHOSTTY_KEY_N),
        ('o', 'O', K::GHOSTTY_KEY_O),
        ('p', 'P', K::GHOSTTY_KEY_P),
        ('q', 'Q', K::GHOSTTY_KEY_Q),
        ('r', 'R', K::GHOSTTY_KEY_R),
        ('s', 'S', K::GHOSTTY_KEY_S),
        ('t', 'T', K::GHOSTTY_KEY_T),
        ('u', 'U', K::GHOSTTY_KEY_U),
        ('v', 'V', K::GHOSTTY_KEY_V),
        ('w', 'W', K::GHOSTTY_KEY_W),
        ('x', 'X', K::GHOSTTY_KEY_X),
        ('y', 'Y', K::GHOSTTY_KEY_Y),
        ('z', 'Z', K::GHOSTTY_KEY_Z),
        ('0', ')', K::GHOSTTY_KEY_DIGIT_0),
        ('1', '!', K::GHOSTTY_KEY_DIGIT_1),
        ('2', '@', K::GHOSTTY_KEY_DIGIT_2),
        ('3', '#', K::GHOSTTY_KEY_DIGIT_3),
        ('4', '$', K::GHOSTTY_KEY_DIGIT_4),
        ('5', '%', K::GHOSTTY_KEY_DIGIT_5),
        ('6', '^', K::GHOSTTY_KEY_DIGIT_6),
        ('7', '&', K::GHOSTTY_KEY_DIGIT_7),
        ('8', '*', K::GHOSTTY_KEY_DIGIT_8),
        ('9', '(', K::GHOSTTY_KEY_DIGIT_9),
        ('`', '~', K::GHOSTTY_KEY_BACKQUOTE),
        ('-', '_', K::GHOSTTY_KEY_MINUS),
        ('=', '+', K::GHOSTTY_KEY_EQUAL),
        ('[', '{', K::GHOSTTY_KEY_BRACKET_LEFT),
        (']', '}', K::GHOSTTY_KEY_BRACKET_RIGHT),
        ('\\', '|', K::GHOSTTY_KEY_BACKSLASH),
        (';', ':', K::GHOSTTY_KEY_SEMICOLON),
        ('\'', '"', K::GHOSTTY_KEY_QUOTE),
        (',', '<', K::GHOSTTY_KEY_COMMA),
        ('.', '>', K::GHOSTTY_KEY_PERIOD),
        ('/', '?', K::GHOSTTY_KEY_SLASH),
    ]
};

/// The US-layout physical key for a typed character, its unshifted
/// character, and whether Shift produces it. Characters with no US key are
/// `UNIDENTIFIED` and rely on their text alone.
fn us_layout(c: char) -> (sys::GhosttyKey, char, bool) {
    if c == ' ' {
        return (sys::GhosttyKey::GHOSTTY_KEY_SPACE, ' ', false);
    }
    US_LAYOUT
        .into_iter()
        .find_map(|(plain, shifted, key)| {
            if c == plain {
                Some((key, plain, false))
            } else if c == shifted {
                Some((key, plain, true))
            } else {
                None
            }
        })
        .unwrap_or((sys::GhosttyKey::GHOSTTY_KEY_UNIDENTIFIED, c, false))
}

fn function_key(key: FKey) -> sys::GhosttyKey {
    use sys::GhosttyKey as K;
    let keys = [
        K::GHOSTTY_KEY_F1,
        K::GHOSTTY_KEY_F2,
        K::GHOSTTY_KEY_F3,
        K::GHOSTTY_KEY_F4,
        K::GHOSTTY_KEY_F5,
        K::GHOSTTY_KEY_F6,
        K::GHOSTTY_KEY_F7,
        K::GHOSTTY_KEY_F8,
        K::GHOSTTY_KEY_F9,
        K::GHOSTTY_KEY_F10,
        K::GHOSTTY_KEY_F11,
        K::GHOSTTY_KEY_F12,
        K::GHOSTTY_KEY_F13,
        K::GHOSTTY_KEY_F14,
        K::GHOSTTY_KEY_F15,
        K::GHOSTTY_KEY_F16,
        K::GHOSTTY_KEY_F17,
        K::GHOSTTY_KEY_F18,
        K::GHOSTTY_KEY_F19,
        K::GHOSTTY_KEY_F20,
        K::GHOSTTY_KEY_F21,
        K::GHOSTTY_KEY_F22,
        K::GHOSTTY_KEY_F23,
        K::GHOSTTY_KEY_F24,
        K::GHOSTTY_KEY_F25,
    ];
    // F26-F35 have no physical key in libghostty-vt; UNIDENTIFIED makes the
    // encoder produce nothing for them.
    keys.get(usize::from(key.get().get() - 1))
        .copied()
        .unwrap_or(K::GHOSTTY_KEY_UNIDENTIFIED)
}

/// A control character typed as text (e.g. a raw `\u{1b}`) goes to the
/// program as its own UTF-8 bytes, unless it is a release.
fn push_control_char(c: char, action: KeyAction, out: &mut Vec<u8>) {
    match action {
        KeyAction::Press | KeyAction::Repeat => {
            let mut buf = [0_u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
        KeyAction::Release => {}
    }
}
