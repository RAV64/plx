//! A counting global allocator for zero-allocation and memory-budget tests.
//!
//! A test binary installs it with [`install!`]; tests then wrap the code
//! under test in [`measure`]. The counters are per thread: a measurement
//! sees exactly what its closure allocated on the calling thread, whatever
//! the test harness's other threads do (they made zero-allocation tests
//! flaky when the counters were process-wide). plx's code under test is
//! single-threaded by design, so nothing it does is missed.
//!
//! Only Rust's global allocator is counted, which includes libghostty-vt:
//! plx-ghostty routes its allocator through Rust's.

#![expect(
    unsafe_code,
    reason = "a GlobalAlloc implementation is unsafe by definition"
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

/// One thread's counters. Atomics only because the allocator gets `&self`;
/// no other thread reads them, so `Relaxed` is exact.
struct ThreadCounts {
    allocations: AtomicU64,
    allocated_bytes: AtomicU64,
    freed_bytes: AtomicU64,
}

thread_local! {
    // Const-initialised and without drop glue: never lazily set up, never
    // destroyed, so the allocator can reach it at any point of a thread's
    // life without allocating or failing.
    static COUNTS: ThreadCounts = const {
        ThreadCounts {
            allocations: AtomicU64::new(0),
            allocated_bytes: AtomicU64::new(0),
            freed_bytes: AtomicU64::new(0),
        }
    };
}

/// The system allocator, counting every allocation and reallocation.
#[derive(Debug)]
pub struct CountingAllocator;

// SAFETY: every method forwards to `System` with the same arguments, which
// upholds `GlobalAlloc`'s contract; the counters have no effect on memory.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        // SAFETY: forwarded unchanged; the caller upholds `alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        // SAFETY: forwarded unchanged; the caller upholds the contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_allocation(new_size);
        record_free(layout.size());
        // SAFETY: forwarded unchanged; the caller upholds the contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record_free(layout.size());
        // SAFETY: forwarded unchanged; the caller upholds the contract.
        unsafe { System.dealloc(ptr, layout) }
    }
}

fn record_allocation(size: usize) {
    COUNTS.with(|counts| {
        counts.allocations.fetch_add(1, Ordering::Relaxed);
        counts
            .allocated_bytes
            .fetch_add(widen(size), Ordering::Relaxed);
    });
}

fn record_free(size: usize) {
    COUNTS.with(|counts| counts.freed_bytes.fetch_add(widen(size), Ordering::Relaxed));
}

#[cfg(not(target_pointer_width = "64"))]
compile_error!("plx targets 64-bit Linux");

/// `usize` to `u64`.
#[expect(
    clippy::as_conversions,
    reason = "usize is 64 bits (checked above), so this is lossless"
)]
const fn widen(size: usize) -> u64 {
    size as u64
}

/// Allocation activity over some span of code.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Calls to `alloc`, `alloc_zeroed` and `realloc`.
    pub allocations: u64,
    /// Bytes requested by those calls.
    pub allocated_bytes: u64,
    /// Bytes released by `dealloc` and `realloc`.
    pub freed_bytes: u64,
}

impl Counts {
    /// Net heap growth: allocated minus freed (negative when memory was
    /// released).
    #[must_use]
    pub fn net_bytes(self) -> i128 {
        i128::from(self.allocated_bytes) - i128::from(self.freed_bytes)
    }
}

fn now() -> Counts {
    COUNTS.with(|counts| Counts {
        allocations: counts.allocations.load(Ordering::Relaxed),
        allocated_bytes: counts.allocated_bytes.load(Ordering::Relaxed),
        freed_bytes: counts.freed_bytes.load(Ordering::Relaxed),
    })
}

/// Runs `f` and reports the allocation activity it caused on this thread.
pub fn measure<R>(f: impl FnOnce() -> R) -> (Counts, R) {
    let before = now();
    let result = f();
    let after = now();
    let counts = Counts {
        allocations: after.allocations - before.allocations,
        allocated_bytes: after.allocated_bytes - before.allocated_bytes,
        freed_bytes: after.freed_bytes - before.freed_bytes,
    };
    (counts, result)
}

/// Installs [`CountingAllocator`] as the global allocator of the calling test
/// binary. Use once, at the root of a test crate.
#[macro_export]
macro_rules! install {
    () => {
        #[global_allocator]
        static PLX_COUNTING_ALLOCATOR: $crate::CountingAllocator = $crate::CountingAllocator;
    };
}
