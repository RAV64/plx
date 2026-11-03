//! The one way plx panics.
//!
//! A panic means a broken *residual invariant*: something plx's types cannot
//! express, such as "an event's tab exists" or "the
//! pinned libghostty-vt honours its documented contract". Bad input from
//! outside is never a panic; it is a typed `Result` at its parse boundary.
//!
//! - [`bug!`] and [`invariant!`] report plx's own broken invariants;
//!   [`breach!`] and [`contract!`] report a broken libghostty-vt guarantee.
//! - [`OrBug`] does the same for an `Option` or `Result` that cannot be
//!   empty or an error.
//!
//! Every message is a string literal stating the invariant, and every panic
//! reports its caller's `file:line`. `xtask lint` inventories each site
//! against `invariants.lock`, so new ones show up in review. Nothing else in
//! plx may panic: the workspace denies `panic!`, `unwrap`, `expect`,
//! `unreachable!` and indexing.
//!
//! The release profile aborts on panic; the shells' panic hooks log and
//! abort too, so a half-applied step is never caught and continued.

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;

/// Whose expectation broke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One of plx's own invariants.
    Bug,
    /// A documented guarantee of the pinned libghostty-vt.
    Contract,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Bug => "plx bug",
            Self::Contract => "plx contract (libghostty)",
        })
    }
}

/// The one failure path behind every macro and [`OrBug`] method, kept cold
/// and out of line so the checks stay small where they are inlined. Not for
/// direct use: `xtask lint` only inventories the macros and methods.
///
/// # Panics
///
/// Always, with `kind: message`, reporting the caller's location.
#[doc(hidden)]
#[cold]
#[inline(never)]
#[track_caller]
#[expect(clippy::panic, reason = "the one sanctioned panic site")]
pub fn __fail(kind: Kind, message: fmt::Arguments<'_>) -> ! {
    panic!("{kind}: {message}")
}

/// A broken plx invariant: panics with the message, at the caller.
///
/// ```should_panic
/// plx_bug::bug!("tab {} is not in the session", 3);
/// ```
#[macro_export]
macro_rules! bug {
    ($message:literal $(, $arg:expr)* $(,)?) => {
        $crate::__fail($crate::Kind::Bug, ::core::format_args!($message $(, $arg)*))
    };
}

/// Checks a plx invariant: panics with the message unless `condition` holds.
///
/// ```should_panic
/// let cols = 3;
/// plx_bug::invariant!(cols == 4, "a row has as many cells as the grid has columns");
/// ```
#[macro_export]
macro_rules! invariant {
    ($condition:expr, $message:literal $(, $arg:expr)* $(,)?) => {
        if !$condition {
            $crate::bug!($message $(, $arg)*)
        }
    };
}

/// A broken libghostty-vt guarantee: panics with the message, at the caller.
/// For the arm of a `match` over a value whose every case the header lists.
#[macro_export]
macro_rules! breach {
    ($message:literal $(, $arg:expr)* $(,)?) => {
        $crate::__fail($crate::Kind::Contract, ::core::format_args!($message $(, $arg)*))
    };
}

/// Checks a documented libghostty-vt guarantee: panics with the message
/// unless `condition` holds.
#[macro_export]
macro_rules! contract {
    ($condition:expr, $message:literal $(, $arg:expr)* $(,)?) => {
        if !$condition {
            $crate::__fail($crate::Kind::Contract, ::core::format_args!($message $(, $arg)*))
        }
    };
}

/// Unwraps a value that cannot be absent (or an error) unless an invariant
/// broke. `what` states the invariant.
pub trait OrBug<T> {
    /// The value; a [`Kind::Bug`] panic, at the caller, if there is none.
    #[track_caller]
    fn or_bug(self, what: &'static str) -> T;

    /// The value; a [`Kind::Contract`] panic, at the caller, if there is none.
    #[track_caller]
    fn or_contract(self, what: &'static str) -> T;
}

// A `match`, not `map_or_else`: a closure would not pass the caller's
// location through `#[track_caller]`.
#[expect(
    clippy::option_if_let_else,
    reason = "map_or_else's closure would lose the caller's location"
)]
impl<T> OrBug<T> for Option<T> {
    #[inline]
    #[track_caller]
    fn or_bug(self, what: &'static str) -> T {
        match self {
            Some(value) => value,
            None => __fail(Kind::Bug, format_args!("{what}")),
        }
    }

    #[inline]
    #[track_caller]
    fn or_contract(self, what: &'static str) -> T {
        match self {
            Some(value) => value,
            None => __fail(Kind::Contract, format_args!("{what}")),
        }
    }
}

impl<T, E: fmt::Debug> OrBug<T> for Result<T, E> {
    #[inline]
    #[track_caller]
    fn or_bug(self, what: &'static str) -> T {
        match self {
            Ok(value) => value,
            Err(error) => __fail(Kind::Bug, format_args!("{what}: {error:?}")),
        }
    }

    #[inline]
    #[track_caller]
    fn or_contract(self, what: &'static str) -> T {
        match self {
            Ok(value) => value,
            Err(error) => __fail(Kind::Contract, format_args!("{what}: {error:?}")),
        }
    }
}
