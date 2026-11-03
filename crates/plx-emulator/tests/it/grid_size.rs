use core::num::NonZeroU16;

use plx_emulator::{GridSize, GridTooLarge};

fn dims(cols: u16, rows: u16) -> (NonZeroU16, NonZeroU16) {
    (
        NonZeroU16::new(cols).unwrap(),
        NonZeroU16::new(rows).unwrap(),
    )
}

#[test]
fn sizes_past_the_maximum_are_refused() {
    let (cols, rows) = dims(GridSize::MAX_COLS, GridSize::MAX_ROWS);
    let largest = GridSize::new(cols, rows).unwrap();
    assert_eq!(largest.area(), GridSize::MAX_CELLS);

    let (wide, tall) = dims(GridSize::MAX_COLS + 1, GridSize::MAX_ROWS + 1);
    assert_eq!(
        GridSize::new(wide, rows),
        Err(GridTooLarge { cols: wide, rows })
    );
    assert_eq!(
        GridSize::new(cols, tall),
        Err(GridTooLarge { cols, rows: tall })
    );
}

/// Deserialising goes through the same bound: a client cannot claim a
/// 65535x65535 terminal.
#[test]
fn deserialising_checks_the_bound() {
    let ok: GridSize = serde_json::from_str(r#"{"cols":80,"rows":24}"#).unwrap();
    let (cols, rows) = dims(80, 24);
    assert_eq!(ok, GridSize::new(cols, rows).unwrap());
    assert!(serde_json::from_str::<GridSize>(r#"{"cols":65535,"rows":65535}"#).is_err());
    assert!(serde_json::from_str::<GridSize>(r#"{"cols":0,"rows":24}"#).is_err());
}

#[test]
fn smallest_and_largest_stay_in_bounds() {
    let (a_cols, a_rows) = dims(GridSize::MAX_COLS, 1);
    let (b_cols, b_rows) = dims(1, GridSize::MAX_ROWS);
    let a = GridSize::new(a_cols, a_rows).unwrap();
    let b = GridSize::new(b_cols, b_rows).unwrap();
    assert_eq!(a.largest(b).area(), GridSize::MAX_CELLS);
    assert_eq!(a.smallest(b).area(), 1);
}

#[test]
#[should_panic(expected = "plx bug: a part of a grid is no larger than the grid")]
#[expect(unused_must_use, reason = "the call panics before there is a result")]
fn a_part_larger_than_the_whole_is_a_bug() {
    let (cols, rows) = dims(10, 10);
    let (wider, _) = dims(11, 1);
    GridSize::new(cols, rows).unwrap().within(wider, rows);
}
