//! libghostty-vt allocates through Rust's global allocator.
//!
//! Every libghostty-vt object plx creates is given [`RUST`], so all of its
//! memory (pages, scrollback, render state) goes through Rust's global
//! allocator: counted by the allocation tests, and measurable for the memory
//! budgets. libghostty-vt's allocator interface is
//! Zig's, which passes size and alignment on every call, including frees, so
//! it maps one to one onto `std::alloc`.

use core::ffi::c_void;
use core::ptr;
use std::alloc::{self as rust, Layout};

use plx_bug::OrBug as _;
use plx_ghostty_sys as sys;

/// The allocator to pass to every libghostty-vt constructor.
pub(crate) fn rust() -> *const sys::GhosttyAllocator {
    &raw const RUST.0
}

/// A `GhosttyAllocator` holds raw pointers, so it is not `Sync`; this one
/// has a null context and a `'static` vtable of stateless functions, so
/// sharing it between threads is sound.
struct Shared(sys::GhosttyAllocator);

// SAFETY: see `Shared`: nothing behind the pointers is mutable or
// thread-bound.
unsafe impl Sync for Shared {}

static VTABLE: sys::GhosttyAllocatorVtable = sys::GhosttyAllocatorVtable {
    alloc: Some(alloc),
    resize: Some(resize),
    remap: Some(remap),
    free: Some(free),
};

static RUST: Shared = Shared(sys::GhosttyAllocator {
    ctx: ptr::null_mut(),
    vtable: &raw const VTABLE,
});

/// The alignment Zig passes as a power-of-two exponent.
#[track_caller]
fn align(alignment: u8) -> usize {
    1_usize
        .checked_shl(u32::from(alignment))
        .or_contract("an allocation's alignment fits in usize")
}

/// The layout of memory libghostty-vt got from this allocator and now frees
/// or remaps: it passes back the exact length and alignment it was given.
#[track_caller]
fn existing(len: usize, alignment: u8) -> Layout {
    plx_bug::contract!(
        len > 0,
        "zero-length memory is never freed or remapped through the allocator"
    );
    Layout::from_size_align(len, align(alignment))
        .or_contract("memory comes back with the layout it was allocated with")
}

extern "C" fn alloc(_ctx: *mut c_void, len: usize, alignment: u8, _ret_addr: usize) -> *mut c_void {
    let align = align(alignment);
    if len == 0 {
        // Zig's interface allows zero-length requests. They need no memory,
        // only a non-null, aligned pointer, which `free` recognises by its
        // zero length.
        return ptr::without_provenance_mut(align);
    }
    match Layout::from_size_align(len, align) {
        Ok(layout) => {
            // SAFETY: `layout` has a non-zero size.
            let memory = unsafe { rust::alloc(layout) };
            if memory.is_null() {
                // Out of memory aborts, as it does for every Rust allocation;
                // libghostty never sees it.
                rust::handle_alloc_error(layout);
            }
            memory.cast()
        }
        // Larger than `isize::MAX`: nothing can satisfy it. Null is
        // libghostty's out-of-memory signal, which plx reports as an error of
        // the call that asked.
        Err(_too_large) => ptr::null_mut(),
    }
}

/// Growing or shrinking in place is never promised; libghostty-vt then
/// falls back to `remap` or to allocating and copying.
const extern "C" fn resize(
    _ctx: *mut c_void,
    _memory: *mut c_void,
    _memory_len: usize,
    _alignment: u8,
    _new_len: usize,
    _ret_addr: usize,
) -> bool {
    false
}

extern "C" fn remap(
    _ctx: *mut c_void,
    memory: *mut c_void,
    memory_len: usize,
    alignment: u8,
    new_len: usize,
    _ret_addr: usize,
) -> *mut c_void {
    let old = existing(memory_len, alignment);
    plx_bug::contract!(new_len > 0, "remap's new length is non-zero");
    match Layout::from_size_align(new_len, old.align()) {
        // SAFETY: `memory` came from this allocator with layout `old`, and
        // `new_len` is non-zero and forms a valid layout with its alignment.
        // A null result (no memory) tells libghostty to allocate and copy,
        // where running out aborts in `alloc`.
        Ok(_) => unsafe { rust::realloc(memory.cast(), old, new_len) }.cast(),
        Err(_too_large) => ptr::null_mut(),
    }
}

extern "C" fn free(
    _ctx: *mut c_void,
    memory: *mut c_void,
    memory_len: usize,
    alignment: u8,
    _ret_addr: usize,
) {
    if memory_len == 0 {
        // A zero-length allocation: no memory behind it (see `alloc`).
        return;
    }
    // SAFETY: `memory` came from this allocator with this layout.
    unsafe { rust::dealloc(memory.cast(), existing(memory_len, alignment)) };
}
