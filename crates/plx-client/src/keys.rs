//! termina's key and mouse events, as plx's: the client's input mapping.
//! Pure, so it is tested without a terminal.

use plx_emulator::{
    FKey, Key, KeyAction, KeyChar, KeyEvent, Mods, MouseAction, MouseButton, MouseEvent, Position,
};
use termina::event::{KeyCode, KeyEventKind, Modifiers, MouseEventKind};

/// The plx mouse event for a termina one: every report has one.
#[must_use]
pub fn mouse_event(event: &termina::event::MouseEvent) -> MouseEvent {
    use termina::event::MouseButton as Theirs;
    let button = |button| match button {
        Theirs::Left => MouseButton::Left,
        Theirs::Middle => MouseButton::Middle,
        Theirs::Right => MouseButton::Right,
    };
    let (action, button) = match event.kind {
        MouseEventKind::Down(b) => (MouseAction::Press, button(b)),
        MouseEventKind::Up(b) => (MouseAction::Release, button(b)),
        MouseEventKind::Drag(b) => (MouseAction::Motion, button(b)),
        MouseEventKind::Moved => (MouseAction::Motion, MouseButton::None),
        MouseEventKind::ScrollUp => (MouseAction::Press, MouseButton::WheelUp),
        MouseEventKind::ScrollDown => (MouseAction::Press, MouseButton::WheelDown),
        MouseEventKind::ScrollLeft => (MouseAction::Press, MouseButton::WheelLeft),
        MouseEventKind::ScrollRight => (MouseAction::Press, MouseButton::WheelRight),
    };
    MouseEvent {
        action,
        button,
        position: Position {
            x: event.column,
            y: event.row,
        },
        mods: mods(event.modifiers),
    }
}

/// The plx key event for a termina one, if plx has one.
///
/// `None` for a key plx has no use for (lock keys, media keys, a bare
/// modifier) or cannot represent (a character in the platform function-key
/// range, which `KeyChar` refuses).
#[must_use]
pub fn key_event(event: &termina::event::KeyEvent) -> Option<KeyEvent> {
    let mut mods = mods(event.modifiers);
    let key = match event.code {
        KeyCode::Char(c) => match KeyChar::new(c) {
            Ok(c) => Key::Char(c),
            Err(_function_key_code) => return None,
        },
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => {
            mods = mods.with(Mods::SHIFT);
            Key::Tab
        }
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Escape => Key::Escape,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Insert => Key::Insert,
        KeyCode::Delete => Key::Delete,
        KeyCode::Function(n) => match FKey::new(n) {
            Ok(n) => Key::F(n),
            Err(_no_such_key) => return None,
        },
        KeyCode::KeypadBegin
        | KeyCode::CapsLock
        | KeyCode::ScrollLock
        | KeyCode::NumLock
        | KeyCode::PrintScreen
        | KeyCode::Pause
        | KeyCode::Menu
        | KeyCode::Null
        | KeyCode::Modifier(_)
        | KeyCode::Media(_) => return None,
    };
    let action = match event.kind {
        KeyEventKind::Press => KeyAction::Press,
        KeyEventKind::Repeat => KeyAction::Repeat,
        KeyEventKind::Release => KeyAction::Release,
    };
    Some(KeyEvent { key, mods, action })
}

fn mods(modifiers: Modifiers) -> Mods {
    [
        (Modifiers::SHIFT, Mods::SHIFT),
        (Modifiers::ALT, Mods::ALT),
        (Modifiers::CONTROL, Mods::CTRL),
        (Modifiers::SUPER, Mods::SUPER),
    ]
    .into_iter()
    .filter(|(theirs, _)| modifiers.contains(*theirs))
    .fold(Mods::empty(), |mods, (_, ours)| mods.with(ours))
}
