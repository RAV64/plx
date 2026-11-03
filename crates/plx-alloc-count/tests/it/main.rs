//! The harness measures what it should, so zero-allocation tests elsewhere
//! can be trusted.

#![cfg(test)]

plx_alloc_count::install!();

use plx_alloc_count::measure;

#[test]
fn arithmetic_allocates_nothing() {
    let (counts, sum) = measure(|| (1_u64..=100).sum::<u64>());
    assert_eq!(sum, 5050);
    assert_eq!(counts.allocations, 0);
    assert_eq!(counts.net_bytes(), 0);
}

#[test]
fn heap_allocations_are_counted() {
    let (counts, v) = measure(|| {
        let mut v: Vec<u64> = Vec::with_capacity(16);
        v.push(7);
        v
    });
    assert_eq!(counts.allocations, 1);
    assert_eq!(counts.allocated_bytes, 16 * 8);
    assert_eq!(counts.net_bytes(), 16 * 8);
    let (freed, ()) = measure(|| drop(v));
    assert_eq!(freed.freed_bytes, 16 * 8);
    assert_eq!(freed.net_bytes(), -(16 * 8));
}

#[test]
fn reusing_a_buffer_allocates_nothing() {
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    let (counts, ()) = measure(|| {
        for round in 0..100_u8 {
            buf.clear();
            buf.extend_from_slice(&[round; 32]);
        }
    });
    assert_eq!(counts.allocations, 0);
    assert_eq!(buf, [99; 32]);
}
