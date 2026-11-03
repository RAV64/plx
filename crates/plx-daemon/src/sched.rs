//! A short scheduler slice for plx's own threads, so a keystroke's echo
//! does not wait out a busy program's slice.
//!
//! Linux's fair scheduler (EEVDF, since 6.6) lets a task ask for a shorter
//! slice than the default, which a machine of 16 cores puts at 2.8 ms. A
//! waking task whose slice is shorter has the earlier deadline and takes the
//! CPU from a running one at once. That costs it nothing in share: a CPU
//! hog still gets as much time, in more pieces. Unprivileged; kernels before
//! 6.12 ignore the request.
//!
//! Children inherit the slice, `SCHED_FLAG_RESET_ON_FORK` included, so
//! panes' programs get the default back before they start ([`reset`]):
//! a build in a pane must not take latency priority over the rest of the
//! machine.

use std::io;

/// The slice plx's threads ask for: the shortest the kernel grants.
///
/// Measured with the echo bench on its four CPUs (Ryzen 9 7950X3D, the
/// default slice 2.8 ms), the daemon and client at this slice and the
/// pane's shell at the default:
/// - four CPU hogs: p99 1.5-2.5 ms before, 0.89-0.97 ms after, p50 the
///   same (about 30 µs);
/// - four `od` floods on the same CPUs: p99 1.68 ms before, 1.26 ms after
///   (medians of nine runs), p99.9 3.6 ms before, 2.5 ms after;
/// - with no load, or the floods on other CPUs: no change.
///
/// 0.3 ms did less (p99 0.92 ms with the hogs, all of the path short).
pub const SLICE_NS: u64 = 100_000;

/// What [`shorten`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slice {
    /// The thread runs with [`SLICE_NS`].
    Short,
    /// The thread runs under another policy than the normal one (batch,
    /// idle, real-time): someone chose it, so it stays.
    Kept,
}

/// Asks for [`SLICE_NS`] on the calling thread, keeping its nice value; a
/// thread not under the normal policy is left as it is.
///
/// # Errors
/// The system call's error (`ENOSYS` before Linux 3.14); plx runs as
/// before without the slice.
pub fn shorten() -> io::Result<Slice> {
    let mut attr = get()?;
    if attr.sched_policy != SCHED_NORMAL {
        return Ok(Slice::Kept);
    }
    attr.sched_runtime = SLICE_NS;
    set(&attr)?;
    Ok(Slice::Short)
}

/// Gives the calling thread the default slice back.
///
/// Async-signal-safe: two system calls on the stack, no allocation (an
/// `io::Error` from errno is a plain value), so a child may call it
/// between fork and exec.
///
/// # Errors
/// The system call's error.
pub fn reset() -> io::Result<()> {
    let mut attr = get()?;
    if attr.sched_policy != SCHED_NORMAL {
        return Ok(());
    }
    // 0 asks for the default; the kernel then reports the default's length.
    attr.sched_runtime = 0;
    set(&attr)
}

/// The slice the calling thread runs with, in nanoseconds: the default's
/// length when it asked for none.
///
/// # Errors
/// The system call's error.
pub fn slice_ns() -> io::Result<u64> {
    get().map(|attr| attr.sched_runtime)
}

/// `SCHED_NORMAL` (`SCHED_OTHER`), the fair scheduler's default policy.
const SCHED_NORMAL: u32 = 0;

/// `SCHED_FLAG_RESET_ON_FORK`: the one flag kept when attributes are set
/// again (the others select what to change, such as utilisation clamps).
const RESET_ON_FORK: u64 = 0x01;

/// The kernel's `struct sched_attr`, as of its first version
/// (`SCHED_ATTR_SIZE_VER1`, 56 bytes; include/uapi/linux/sched/types.h).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct SchedAttr {
    size: u32,
    sched_policy: u32,
    sched_flags: u64,
    sched_nice: i32,
    sched_priority: u32,
    /// For the fair policies: the slice. Set to 0 it asks for the default;
    /// read, it is the slice in effect.
    sched_runtime: u64,
    sched_deadline: u64,
    sched_period: u64,
    sched_util_min: u32,
    sched_util_max: u32,
}

const ATTR_SIZE: u32 = 56;
const _: () = assert!(size_of::<SchedAttr>() == 56, "SCHED_ATTR_SIZE_VER1");
const _: () = assert!(align_of::<SchedAttr>() == 8, "the kernel's u64 alignment");

/// The calling thread's attributes.
fn get() -> io::Result<SchedAttr> {
    let mut attr = SchedAttr::default();
    // SAFETY: sched_getattr(2) for the calling thread (pid 0) writes at
    // most `ATTR_SIZE` bytes to `attr`, a live, writable, 8-aligned
    // `repr(C)` value of exactly that size and of the kernel's layout
    // (both asserted at compile time); flags must be 0. It reads nothing
    // else of ours and keeps no pointer after returning.
    #[expect(unsafe_code, reason = "sched_getattr(2): rustix lacks it")]
    let result =
        unsafe { libc::syscall(libc::SYS_sched_getattr, 0, &raw mut attr, ATTR_SIZE, 0_u32) };
    if result == 0 {
        Ok(attr)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Sets the calling thread's attributes to `attr`: its policy, nice value,
/// priority and slice, with only [`RESET_ON_FORK`] of its flags.
fn set(attr: &SchedAttr) -> io::Result<()> {
    let attr = SchedAttr {
        size: ATTR_SIZE,
        sched_flags: attr.sched_flags & RESET_ON_FORK,
        ..*attr
    };
    // SAFETY: sched_setattr(2) for the calling thread (pid 0) reads
    // `attr.size` bytes from `attr`, a live, 8-aligned `repr(C)` value of
    // exactly that size and of the kernel's layout (asserted at compile
    // time); flags must be 0. It writes nothing of ours and keeps no
    // pointer after returning.
    #[expect(unsafe_code, reason = "sched_setattr(2): rustix lacks it")]
    let result = unsafe { libc::syscall(libc::SYS_sched_setattr, 0, &raw const attr, 0_u32) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
