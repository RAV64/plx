//! Every plx-bug entry point panics with its kind and message, at its
//! caller, and passes values through untouched otherwise.

#![cfg(test)]

use std::panic;
use std::sync::OnceLock;

use plx_bug::{OrBug, breach, bug, contract, invariant};

#[test]
#[should_panic(expected = "plx bug: tab 3 is not in the session")]
fn bug_panics_with_its_message() {
    bug!("tab {} is not in the session", 3);
}

#[test]
#[should_panic(expected = "plx bug: rows match: 2 != 3")]
fn a_failed_invariant_panics() {
    let (rows, expected) = (2, 3);
    invariant!(rows == expected, "rows match: {rows} != {expected}");
}

#[test]
fn a_holding_invariant_does_nothing() {
    invariant!(1 + 1 == 2, "arithmetic works");
    contract!(true, "libghostty keeps its word");
}

#[test]
#[should_panic(expected = "plx contract (libghostty): render state rows are non-zero")]
fn a_failed_contract_panics_as_a_contract() {
    contract!(0 > 1, "render state rows are non-zero");
}

#[test]
#[should_panic(expected = "plx contract (libghostty): unknown cursor style 9")]
fn breach_panics_as_a_contract() {
    breach!("unknown cursor style {}", 9);
}

#[test]
#[should_panic(expected = "plx bug: the active tab exists")]
fn or_bug_on_none_panics() {
    None::<u8>.or_bug("the active tab exists");
}

#[test]
#[should_panic(expected = "plx contract (libghostty): utf-8: 7")]
fn or_contract_on_err_shows_the_error() {
    let parsed: Result<u8, u8> = Err(7);
    parsed.or_contract("utf-8");
}

#[test]
fn present_values_pass_through() {
    assert_eq!(Some(5).or_bug("present"), 5);
    assert_eq!(Ok::<u8, u8>(6).or_contract("ok"), 6);
}

/// The panic points at the line that called `or_bug`, not into plx-bug.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "catching the panic is how the test reads its location"
)]
fn panics_report_the_callers_location() {
    static SEEN: OnceLock<(String, u32)> = OnceLock::new();
    panic::set_hook(Box::new(|info| {
        if let Some(location) = info.location() {
            match SEEN.set((location.file().into(), location.line())) {
                Ok(()) | Err(_) => {}
            }
        }
    }));
    let line = line!() + 1;
    let caught = panic::catch_unwind(|| None::<u8>.or_bug("absent"));
    assert!(caught.is_err());
    assert_eq!(SEEN.get(), Some(&(String::from(file!()), line)));
}
