use plx_emulator::Emulator;
use plx_ghostty::{GhosttyEmulator, GraphemeClustering};
use proptest::prelude::*;

use crate::{Effects, view, vt};

proptest! {
    /// A restored terminal is indistinguishable from the original: same
    /// screen now, and the same screen and events after any further output.
    /// The cut may fall inside an escape sequence or a UTF-8 character.
    #[test]
    fn snapshot_restore_round_trips(
        size in vt::grid_size(),
        options in vt::options(),
        (bytes, cut) in vt::stream().prop_flat_map(|bytes| {
            let len = bytes.len();
            (Just(bytes), 0..=len)
        }),
        suffix in vt::stream(),
    ) {
        let (prefix, rest) = bytes.split_at(cut);
        let mut original = GhosttyEmulator::new(size, &options).unwrap();
        original.feed(prefix);

        let mut restored = GhosttyEmulator::restore(&original.snapshot().unwrap(), &options).unwrap();
        prop_assert_eq!(restored.screen().size(), size);
        prop_assert_eq!(view(&mut restored), view(&mut original));

        let mut original_effects = Effects::default();
        let mut restored_effects = Effects::default();
        for chunk in [rest, suffix.as_slice()] {
            original_effects.feed(&mut original, chunk);
            restored_effects.feed(&mut restored, chunk);
        }
        prop_assert_eq!(view(&mut restored), view(&mut original));
        prop_assert_eq!(restored_effects, original_effects);
    }
}

#[test]
fn restoring_garbage_fails_loudly() {
    let result = GhosttyEmulator::restore(
        b"definitely not a snapshot",
        &crate::options(GraphemeClustering::Enabled),
    );
    assert!(result.is_err(), "garbage restored into {result:?}");
}
