use boa_macros::{Finalize, Trace};

use super::{Harness, run_test};
use crate::{GcBox, GcEdge, GcRefCell, Rooted, force_collect};

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

/// The collection threshold decides how much garbage piles up before a collection,
/// and a collection walks the whole heap — everything allocated since the last one,
/// live or not. Pinned because lowering it is a large slowdown that no other test
/// would notice: measured on the omoikane benchmark, `1 MiB` put 40% of
/// `closure-alloc`'s wall time inside the collector, and `4 MiB` took 21% off the shape.
///
/// Raising it further is not free either. `16 MiB` bought no more speed than `4 MiB` and
/// cost 61% more peak RSS on an allocation-heavy workload, because a collection's cost
/// grows with the garbage it has to walk past while the amount it reclaims does not.
#[test]
fn default_threshold_is_the_measured_one() {
    run_test(|| {
        Harness::assert_threshold(4 * 1024 * 1024);
    });
}

/// The threshold has to gate collection rather than merely be recorded. Allocating
/// well under it must not trigger one.
#[test]
#[cfg_attr(miri, ignore)]
fn allocating_under_the_threshold_does_not_collect() {
    run_test(|| {
        // 256 KiB of live data, comfortably inside a `4 MiB` threshold.
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
