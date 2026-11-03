//! Zero-allocation guarantees of the core's input path.

use plx_alloc_count::measure;
use plx_core::geometry::Direction;
use plx_core::mode::{Keymap, Mode, Resolution};
use plx_core::structure::GROW_STEP;
use plx_emulator::{Key, KeyChar, KeyEvent, Mods};

use crate::strategy::LayoutOp;
use crate::strategy::build;

/// Resolving keys, for every preset and mode, never allocates: bound commands
/// are lent from the keymap.
#[test]
fn key_resolution_allocates_nothing() {
    let keymaps = [Keymap::unlock_first(), Keymap::classic()];
    let keys = [
        KeyEvent::press(Key::Char(KeyChar::new('g').unwrap()), Mods::CTRL),
        KeyEvent::press(Key::Char(KeyChar::new('p').unwrap()), Mods::empty()),
        KeyEvent::press(Key::Char(KeyChar::new('n').unwrap()), Mods::empty()),
        KeyEvent::press(Key::Left, Mods::empty()),
        KeyEvent::press(Key::Char(KeyChar::new('x').unwrap()), Mods::empty()),
        KeyEvent::press(Key::Escape, Mods::empty()),
        KeyEvent::press(Key::Char(KeyChar::new('é').unwrap()), Mods::empty()),
    ];
    let (counts, runs) = measure(|| {
        let mut runs = 0_usize;
        for keymap in &keymaps {
            for mode in Mode::ALL {
                for key in &keys {
                    if let Resolution::Run { .. } = keymap.resolve(mode, key) {
                        runs += 1;
                    }
                }
            }
        }
        runs
    });
    assert!(runs > 0, "the sample keys trigger some commands");
    assert_eq!(counts.allocations, 0);
}

/// The read-only layout queries on the command path do not allocate.
#[test]
fn layout_queries_allocate_nothing() {
    let ops = [
        LayoutOp::Split(0, Direction::Right),
        LayoutOp::Split(1, Direction::Down),
        LayoutOp::Split(0, Direction::Down),
        LayoutOp::Split(2, Direction::Left),
    ];
    let layout = build(&ops);
    let probe = layout.first_pane();
    let (counts, answers) = measure(|| {
        let directions = [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ];
        let growable = directions
            .iter()
            .filter(|d| layout.can_grow(probe, **d, GROW_STEP))
            .count();
        (
            layout.contains(probe),
            layout.first_pane_except_or_none(probe),
            growable,
        )
    });
    assert!(answers.0 && answers.1.is_some());
    assert_eq!(counts.allocations, 0);
}
