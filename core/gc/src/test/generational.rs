use super::{Harness, run_test};
use crate::{
    Ephemeron, Finalize, GcEdge, GcRefCell, Rooted, Trace, WeakMap, force_collect,
    force_minor_collect,
};

#[derive(Debug, Finalize, Trace)]
struct OldRoot {
    holder: GcEdge<OldHolder>,
}

#[derive(Debug, Finalize, Trace)]
struct OldHolder {
    child: GcRefCell<Option<GcEdge<u32>>>,
}

fn is_old<T: Trace + ?Sized>(root: &Rooted<T>) -> bool {
    // SAFETY: the explicit root keeps the allocation alive.
    unsafe { root.as_gc().inner_ptr.as_ref() }.header.is_old()
}

fn edge_is_old<T: Trace + ?Sized>(edge: &GcEdge<T>) -> bool {
    // SAFETY: the edge is valid for the duration of this test.
    unsafe { edge.as_gc().inner_ptr.as_ref() }.header.is_old()
}

#[test]
fn minor_collection_sweeps_garbage_and_promotes_survivors() {
    run_test(|| {
        let live = Rooted::new(1_u32);
        let _garbage = GcEdge::new(2_u32);

        force_minor_collect();
        Harness::assert_strong_allocations(1);
        assert!(!is_old(&live));

        force_minor_collect();
        assert!(is_old(&live));
        Harness::assert_strong_allocations(1);
    });
}

#[test]
fn mutable_cell_write_remembers_old_to_young_edge() {
    run_test(|| {
        let root = Rooted::new(OldRoot {
            holder: GcEdge::new(OldHolder {
                child: GcRefCell::new(None),
            }),
        });

        force_minor_collect();
        force_minor_collect();
        assert!(is_old(&root));
        assert!(edge_is_old(&root.holder));

        *root.holder.child.borrow_mut() = Some(GcEdge::new(42));
        force_minor_collect();

        assert_eq!(
            root.holder.child.borrow().as_ref().map(|value| **value),
            Some(42)
        );
    });
}

fn run_ephemeron_generation_case(key_old: bool, value_old: bool) {
    let key = Rooted::new(0_u32);
    if key_old {
        force_minor_collect();
        force_minor_collect();
        assert!(is_old(&key));
    }

    let value = Rooted::new(99_u32);
    if value_old {
        force_minor_collect();
        force_minor_collect();
        assert!(is_old(&value));
    }
    let value = value.into_edge();

    let ephemeron = Ephemeron::new(&key, value);
    force_minor_collect();

    assert_eq!(ephemeron.value().as_deref().copied(), Some(99));
}

#[test]
fn minor_ephemeron_tracing_handles_all_generation_pairs() {
    run_test(|| {
        for key_old in [false, true] {
            for value_old in [false, true] {
                run_ephemeron_generation_case(key_old, value_old);
                force_collect();
            }
        }
    });
}

#[test]
fn old_ephemeron_keys_are_conservative_only_until_major_collection() {
    run_test(|| {
        let key = Rooted::new(0_u32);
        force_minor_collect();
        force_minor_collect();

        let ephemeron = Ephemeron::new(&key, GcEdge::new(99_u32));
        drop(key);

        force_minor_collect();
        assert!(ephemeron.has_value());

        force_collect();
        assert!(!ephemeron.has_value());
    });
}

#[test]
fn minor_collection_keeps_live_weak_maps_and_clears_dead_keys() {
    run_test(|| {
        let mut map = WeakMap::new();
        let key = Rooted::new(7_u32);
        map.insert(&key, 11_u32);

        force_minor_collect();
        assert_eq!(map.inner.borrow().len(), 1);

        drop(key);
        force_minor_collect();
        assert_eq!(map.inner.borrow().len(), 0);
    });
}
