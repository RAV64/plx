//! What can go wrong in the adapter, and the three ways a libghostty-vt
//! result is handled:
//!
//! - [`check`]: the call can genuinely fail (libghostty's own limits, an
//!   untrusted snapshot); the failure is an [`Error`] for the caller.
//! - [`check_contract`]: the header documents that the call fails only on
//!   invalid arguments, which plx never passes; a failure breaks the pinned
//!   library's contract and panics.
//! - [`adopt`]: a handle a successful call returned is non-null, by the same
//!   contract.
//!
//! Allocation failure never reaches any of them: plx's allocator aborts on
//! out-of-memory like the rest of Rust (`alloc.rs`), so an `OUT_OF_MEMORY`
//! result can only come from a libghostty-internal limit.

use core::ptr::NonNull;

use plx_ghostty_sys as sys;

/// A failure code returned by a libghostty-vt call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    OutOfMemory,
    InvalidValue,
    OutOfSpace,
    NoValue,
    IoError,
    LimitExceeded,
    Rejected,
    /// A code this build of plx does not know; the raw value is kept.
    Unrecognised(i32),
}

impl Code {
    const fn from_raw(result: sys::GhosttyResult) -> Self {
        match result {
            sys::GhosttyResult::GHOSTTY_OUT_OF_MEMORY => Self::OutOfMemory,
            sys::GhosttyResult::GHOSTTY_INVALID_VALUE => Self::InvalidValue,
            sys::GhosttyResult::GHOSTTY_OUT_OF_SPACE => Self::OutOfSpace,
            sys::GhosttyResult::GHOSTTY_NO_VALUE => Self::NoValue,
            sys::GhosttyResult::GHOSTTY_IO_ERROR => Self::IoError,
            sys::GhosttyResult::GHOSTTY_LIMIT_EXCEEDED => Self::LimitExceeded,
            sys::GhosttyResult::GHOSTTY_REJECTED => Self::Rejected,
            other => Self::Unrecognised(other.0),
        }
    }
}

/// An operation on a terminal failed for a reason outside plx's control.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A call that can genuinely fail did: a libghostty-internal limit, or a
    /// malformed or incompatible snapshot.
    #[error("libghostty-vt `{call}` failed: {code:?}")]
    Call { call: &'static str, code: Code },

    /// A snapshot holds a terminal larger than plx supports.
    #[error(transparent)]
    GridTooLarge(#[from] plx_emulator::GridTooLarge),
}

/// The linked libghostty-vt is not the build plx requires.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error(
        "the linked libghostty-vt is {linked}, but plx was built against the flake's pinned {pinned}"
    )]
    NotThePinnedBuild {
        linked: &'static str,
        pinned: &'static str,
    },

    #[error(
        "the linked libghostty-vt was built with {0}, which plx's flake compiles out (-Dvt-features=-{0})"
    )]
    CompiledIn(crate::Feature),
}

/// A call that can genuinely fail: its result as a `Result`, naming the call.
pub(crate) fn check(call: &'static str, result: sys::GhosttyResult) -> Result<(), Error> {
    if result == sys::GhosttyResult::GHOSTTY_SUCCESS {
        Ok(())
    } else {
        Err(Error::Call {
            call,
            code: Code::from_raw(result),
        })
    }
}

/// A call the header documents as failing only on invalid arguments.
#[track_caller]
pub(crate) fn check_contract(call: &'static str, result: sys::GhosttyResult) {
    plx_bug::contract!(
        result == sys::GhosttyResult::GHOSTTY_SUCCESS,
        "`{}` fails only on invalid arguments, which plx never passes; it returned {:?}",
        call,
        Code::from_raw(result),
    );
}

/// The handle a successful constructor call wrote.
#[track_caller]
#[expect(
    clippy::option_if_let_else,
    reason = "map_or_else's closure would lose the caller's location"
)]
pub(crate) fn adopt<T>(raw: *mut T, call: &'static str) -> NonNull<T> {
    match NonNull::new(raw) {
        Some(handle) => handle,
        None => plx_bug::breach!("`{}` reported success but returned a null handle", call),
    }
}

/// `u32` to `usize`, lossless on every target plx builds for.
#[expect(
    clippy::as_conversions,
    reason = "usize holds every u32 (checked below); usize::from(u32) does not exist"
)]
pub(crate) const fn widen(value: u32) -> usize {
    value as usize
}

const _: () = assert!(usize::BITS >= u32::BITS, "plx targets 64-bit Linux");

/// A header `#define` (bindgen types them all `u32`) as the `u16` the C API
/// takes. Checked to fit at compile time, so mirrored constants are derived
/// from the header rather than restated.
macro_rules! header_u16 {
    ($value:expr) => {{
        const _: () = assert!($value <= 0xFFFF, "a header constant does not fit in u16");
        $crate::error::narrow_u16($value)
    }};
}
pub(crate) use header_u16;

/// The cast behind [`header_u16!`]; use the macro, which checks the range.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "header_u16! checks the range at compile time first"
)]
pub(crate) const fn narrow_u16(value: u32) -> u16 {
    value as u16
}
