//! Interners: ids are dense, stable while live and reused once removed;
//! values are stored once; a slice arena never holds more dead than live;
//! looking up known values never allocates.

#![cfg(test)]
#![expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "the reference model keeps its own copies; proptest uses Arc and clone internally"
)]

plx_alloc_count::install!();

use std::collections::BTreeMap;

use plx_alloc_count::measure;
use plx_collections::{Full, IdMap, Interned, Interner, SliceInterner};
use proptest::prelude::*;

/// Small limits, so the generated runs fill the interners.
const VALUES: u32 = 16;
const SLICES: u32 = 8;

/// One step of a generated run: intern a value, or remove the live value at
/// some position (modulo the live count).
#[derive(Clone, Debug)]
enum Op<T> {
    Intern(T),
    Remove(usize),
}

fn ops<T: std::fmt::Debug + Clone>(
    value: impl Strategy<Value = T>,
) -> impl Strategy<Value = Vec<Op<T>>> {
    proptest::collection::vec(
        prop_oneof![
            3 => value.prop_map(Op::Intern),
            1 => any::<usize>().prop_map(Op::Remove),
        ],
        0..300,
    )
}

proptest! {
    /// Interning behaves like a map from live values to distinct ids below
    /// the limit: repeats find their id, new values get a free id, a new value
    /// past the limit is refused without being stored, and removed ids come
    /// back for later values.
    #[test]
    fn interner_matches_a_reference_model(ops in ops(0_u16..40)) {
        let mut interner = Interner::<u16, VALUES>::new();
        let mut live: BTreeMap<u16, u32> = BTreeMap::new();
        for op in ops {
            match op {
                Op::Intern(v) => match (interner.intern(v), live.get(&v)) {
                    (Ok((id, Interned::Existing)), Some(&known)) => prop_assert_eq!(id, known),
                    (Ok((id, Interned::New)), None) => {
                        prop_assert!(id < VALUES);
                        prop_assert!(!live.values().any(|&other| other == id));
                        live.insert(v, id);
                    }
                    (Err(Full), None) => {
                        prop_assert_eq!(live.len(), usize::try_from(VALUES).unwrap());
                    }
                    (result, known) => prop_assert!(false, "{result:?} for {v} known as {known:?}"),
                },
                Op::Remove(n) if !live.is_empty() => {
                    let v = *live.keys().nth(n % live.len()).unwrap();
                    let id = live.remove(&v).unwrap();
                    interner.remove(id);
                    prop_assert_eq!(interner.get_or_none(id), None);
                }
                Op::Remove(_) => {}
            }
            prop_assert_eq!(interner.len(), live.len());
            for (v, &id) in &live {
                prop_assert_eq!(interner.get_or_none(id), Some(v));
            }
        }
    }

    /// The same for slices, plus the arena bound: removed slices never take
    /// more room than live ones, so memory follows what is live.
    #[test]
    fn slice_interner_matches_a_reference_model(
        ops in ops(proptest::collection::vec(0_u8..4, 0..6)),
    ) {
        let mut interner = SliceInterner::<u8, SLICES>::new();
        let mut live: BTreeMap<Vec<u8>, u32> = BTreeMap::new();
        for op in ops {
            match op {
                Op::Intern(s) => match (interner.intern(&s), live.get(&s)) {
                    (Ok((id, Interned::Existing)), Some(&known)) => prop_assert_eq!(id, known),
                    (Ok((id, Interned::New)), None) => {
                        prop_assert!(id < SLICES);
                        prop_assert!(!live.values().any(|&other| other == id));
                        live.insert(s, id);
                    }
                    (Err(Full), None) => {
                        prop_assert_eq!(live.len(), usize::try_from(SLICES).unwrap());
                    }
                    (result, known) => prop_assert!(false, "{result:?} for {s:?} known as {known:?}"),
                },
                Op::Remove(n) if !live.is_empty() => {
                    let s = live.keys().nth(n % live.len()).unwrap().clone();
                    let id = live.remove(&s).unwrap();
                    interner.remove(id);
                    prop_assert_eq!(interner.get_or_none(id), None);
                }
                Op::Remove(_) => {}
            }
            prop_assert_eq!(interner.len(), live.len());
            for (s, &id) in &live {
                prop_assert_eq!(interner.get_or_none(id), Some(s.as_slice()));
            }
            let live_elements: usize = live.keys().map(Vec::len).sum();
            prop_assert!(interner.arena_len() <= 2 * live_elements);
        }
    }
}

proptest! {
    /// An id map behaves like a `BTreeMap` fed growing keys: same lookups,
    /// same order.
    #[test]
    fn id_map_matches_a_btree_map(ops in proptest::collection::vec((any::<bool>(), any::<usize>()), 0..200)) {
        let mut map = IdMap::<u32, u32>::new();
        let mut model: BTreeMap<u32, u32> = BTreeMap::new();
        let mut next = 0_u32;
        for (insert, pick) in ops {
            if insert || model.is_empty() {
                map.insert(next, next * 10);
                model.insert(next, next * 10);
                next += 1;
            } else {
                let key = *model.keys().nth(pick % model.len()).unwrap();
                prop_assert_eq!(map.remove(key), model.remove(&key).unwrap());
                prop_assert_eq!(map.get_or_none(key), None);
            }
            let entries: Vec<(u32, u32)> = map.iter().map(|(k, v)| (k, *v)).collect();
            let expected: Vec<(u32, u32)> = model.iter().map(|(k, v)| (*k, *v)).collect();
            prop_assert_eq!(entries, expected);
        }
    }
}

#[test]
#[should_panic(expected = "plx bug: an id is inserted once")]
fn inserting_an_id_twice_is_a_bug() {
    let mut map = IdMap::<u8, u8>::new();
    map.insert(1, 1);
    map.insert(1, 2);
}

#[test]
#[should_panic(expected = "plx bug: only live ids are removed")]
fn removing_a_dead_id_is_a_bug() {
    let mut interner = Interner::<u8, 4>::new();
    let (id, _) = interner.intern(1).unwrap();
    interner.remove(id);
    interner.remove(id);
}

#[test]
fn looking_up_known_values_allocates_nothing() {
    let mut styles = Interner::<u32, 64>::new();
    let mut clusters = SliceInterner::<u32, 64>::new();
    for v in 0_u32..64 {
        styles.intern(v).unwrap();
        clusters.intern(&[v, v + 1]).unwrap();
    }
    let (counts, ()) = measure(|| {
        for round in 0..100_u32 {
            let v = round % 64;
            assert_eq!(styles.intern(v), Ok((v, Interned::Existing)));
            assert_eq!(clusters.intern(&[v, v + 1]), Ok((v, Interned::Existing)));
        }
    });
    assert_eq!(counts.allocations, 0);
}
