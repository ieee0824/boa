use boa_macros::{Finalize, Trace};

use super::{Harness, run_test};
use crate::{GcBox, GcEdge, GcRefCell, Rooted, force_collect};
#[cfg(feature = "gc-profile")]
use crate::{force_minor_collect, profile, reset_profile};

#[derive(Debug, Finalize, Trace)]
struct EdgeHolder {
    leaf: Option<GcEdge<u32>>,
}

#[test]
fn only_rooted_allocations_start_the_mark_phase() {
    run_test(|| {
        let leaf = GcEdge::new(1_u32);
        let holder = Rooted::new(EdgeHolder {
            leaf: Some(leaf.clone()),
        });
        // Nothing roots this one, and no rooted allocation points at it.
        let _orphan = GcEdge::new(2_u32);

        Harness::assert_strong_allocations(3);

        force_collect();

        // The leaf survives because a rooted allocation has an edge to it. The orphan
        // does not, even though a handle to it is still in scope: a heap edge is not a
        // root. Its handle must not be dereferenced after this point.
        Harness::assert_strong_allocations(2);
        assert_eq!(**holder.leaf.as_ref().expect("the leaf is set"), 1);
    });
}

#[test]
fn an_allocation_survives_the_collection_it_triggers() {
    run_test(|| {
        Harness::collect_on_every_allocation();

        // Allocating the holder runs a collection while the holder is the only thing
        // pointing at the leaf, and while the holder itself is reachable only through the
        // temporary root the allocator registers for it.
        let holder = Rooted::new(EdgeHolder {
            leaf: Some(GcEdge::new(41_u32)),
        });

        Harness::assert_collected_at_least(1);
        Harness::assert_strong_allocations(2);
        assert_eq!(**holder.leaf.as_ref().expect("the leaf is set"), 41);
    });
}

#[test]
fn gc_basic_cell_allocation() {
    run_test(|| {
        let gc_cell = Rooted::new(GcRefCell::new(16_u16));

        force_collect();
        Harness::assert_collections(1);
        Harness::assert_bytes_allocated();
        assert_eq!(*gc_cell.borrow_mut(), 16);
    });
}

#[test]
fn collection_is_deferred_while_a_cell_is_mutably_borrowed() {
    run_test(|| {
        Harness::collect_on_every_allocation();
        let holder = Rooted::new(GcRefCell::new(Vec::<GcEdge<u32>>::new()));
        Harness::assert_collections(1);

        let mut holder_value = holder.borrow_mut();
        holder_value.push(GcEdge::new(42));
        // The allocation above is allowed, but its collection is deferred because
        // the holder's fields are invisible to `GcRefCell::trace` while borrowed.
        force_collect();
        Harness::assert_collections(1);
        drop(holder_value);

        force_collect();
        Harness::assert_collections(2);
        assert_eq!(*holder.borrow()[0], 42);
    });
}

#[test]
fn gc_basic_pointer_alloc() {
    run_test(|| {
        let gc = Rooted::new(16_u8);

        force_collect();
        Harness::assert_collections(1);
        Harness::assert_bytes_allocated();
        assert_eq!(*gc, 16);

        drop(gc);
        force_collect();
        Harness::assert_collections(2);
        Harness::assert_empty_gc();
    });
}

#[test]
// Takes too long to finish in miri
#[cfg_attr(miri, ignore)]
fn gc_recursion() {
    run_test(|| {
        #[derive(Debug, Finalize, Trace)]
        struct S {
            i: usize,
            next: Option<GcEdge<S>>,
        }

        const SIZE: usize = size_of::<GcBox<S>>();
        const COUNT: usize = 1_000_000;

        let mut root = Rooted::new(S { i: 0, next: None });
        for i in 1..COUNT {
            root = Rooted::new(S {
                i,
                next: Some(root.into_edge()),
            });
        }

        Harness::assert_bytes_allocated();
        Harness::assert_exact_bytes_allocated(SIZE * COUNT);

        drop(root);
        force_collect();
        Harness::assert_empty_gc();
    });
}

/// Pin the measured split between nursery and full-heap collection. A 4 `MiB` nursery
/// cut Omoikane's allocation-shape collection counts by roughly 3-4x against 1 `MiB`.
/// The 8 `MiB` major threshold avoids following every nursery pass with a full-heap
/// traversal, while bounding old-generation growth sooner than the equally fast
/// 16 `MiB` alternative.
#[test]
fn default_generation_thresholds_are_the_measured_ones() {
    run_test(|| {
        Harness::assert_threshold(8 * 1024 * 1024);
        Harness::assert_nursery_threshold(4 * 1024 * 1024);
    });
}

/// The threshold has to gate collection rather than merely be recorded. Allocating
/// well under it must not trigger one.
#[test]
#[cfg_attr(miri, ignore)]
fn allocating_under_the_threshold_does_not_collect() {
    run_test(|| {
        // 256 KiB of live data, comfortably inside the nursery threshold.
        let mut held = Vec::new();
        for _ in 0..256 {
            held.push(Rooted::new([0u8; 1024]));
        }

        Harness::assert_collections(0);
        assert_eq!(held.len(), 256);
    });
}

/// And allocating past it must, so the threshold is an upper bound on accumulated
/// garbage rather than a number that happens never to be reached.
#[test]
#[cfg_attr(miri, ignore)]
fn allocating_past_the_threshold_collects() {
    run_test(|| {
        // Dropped immediately, so this is `8 MiB` of garbage against a `4 MiB` threshold
        // and cannot be satisfied without collecting.
        for _ in 0..8192 {
            drop(GcEdge::new([0u8; 1024]));
        }

        Harness::assert_collected_at_least(1);
    });
}

#[test]
#[cfg(feature = "gc-profile")]
fn profiles_minor_and_major_collection_phases() {
    run_test(|| {
        reset_profile();

        let value = Rooted::new(42_u32);
        force_minor_collect();
        force_collect();

        let profile = profile();
        assert_eq!(profile.minor.collections, 1);
        assert_eq!(profile.major.collections, 1);
        assert!(profile.minor.total >= profile.minor.mark);
        assert!(profile.minor.total >= profile.minor.finalize);
        assert!(profile.minor.total >= profile.minor.sweep);
        assert!(profile.major.total >= profile.major.mark);
        assert!(profile.major.total >= profile.major.finalize);
        assert!(profile.major.total >= profile.major.sweep);
        assert_eq!(*value, 42);
    });
}
