use plx_emulator::{Percent, PercentTooLarge, Progress};
use proptest::prelude::*;

#[test]
fn percentages_past_100_are_refused() {
    assert_eq!(Percent::new(100).map(Percent::get), Ok(100));
    assert_eq!(Percent::new(101), Err(PercentTooLarge(101)));
}

/// Deserialising goes through the same bound.
#[test]
fn deserialising_checks_the_bound() {
    let ok: Percent = serde_json::from_str("42").unwrap();
    assert_eq!(ok.get(), 42);
    assert!(serde_json::from_str::<Percent>("101").is_err());
    assert!(serde_json::from_str::<Progress>(r#"{"Set":255}"#).is_err());
}

proptest! {
    /// A report made from a protocol state gives that state back, and its
    /// percentage where the state has one.
    #[test]
    fn states_round_trip(state in 0_u8..=255, percent in proptest::option::of(0_u8..=100)) {
        let percent = percent.map(|p| Percent::new(p).unwrap());
        match Progress::from_state_or_none(state, percent) {
            Some(progress) => {
                prop_assert!(state <= 4);
                prop_assert_eq!(progress.state(), state);
                let kept = matches!(state, 1 | 2 | 4);
                prop_assert_eq!(progress.percent_or_none(), percent.filter(|_| kept));
                let json = serde_json::to_string(&progress).unwrap();
                prop_assert_eq!(serde_json::from_str::<Progress>(&json).unwrap(), progress);
            }
            None => prop_assert!(state > 4),
        }
    }
}
