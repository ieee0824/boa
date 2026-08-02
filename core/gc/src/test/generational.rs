use super::{Harness, run_test};
use crate::{
    Ephemeron, Finalize, GcEdge, GcRefCell, Rooted, Trace, Tracer, WeakGcEdge, WeakMap,
    force_collect, force_minor_collect,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
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

#[derive(Debug)]
struct TraceCounter(Arc<AtomicUsize>);

impl Finalize for TraceCounter {}

unsafe impl Trace for TraceCounter {
    unsafe fn trace(&self, _tracer: &mut Tracer) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn run_finalizer(&self) {
        Finalize::finalize(self);
    }
}

#[test]
fn minor_collection_does_not_trace_old_roots() {
    run_test(|| {
        let traces = Arc::new(AtomicUsize::new(0));
        let root = Rooted::new(TraceCounter(Arc::clone(&traces)));

        force_minor_collect();
        force_minor_collect();
        assert!(is_old(&root));
        let after_promotion = traces.load(Ordering::Relaxed);

        force_minor_collect();
        assert_eq!(traces.load(Ordering::Relaxed), after_promotion);
    });
}

#[test]
fn major_collection_reclaims_old_garbage() {
    run_test(|| {
        let root = Rooted::new(1_u32);
        force_minor_collect();
        force_minor_collect();
        assert!(is_old(&root));

        drop(root);
        force_collect();
        Harness::assert_empty_gc();
    });
}

#[derive(Debug)]
struct FinalizerWritesYoungEdge {
    parent: GcEdge<OldHolder>,
    child: GcEdge<u32>,
}

impl Finalize for FinalizerWritesYoungEdge {
    fn finalize(&self) {
        *self.parent.child.borrow_mut() = Some(self.child.clone());
    }
}

unsafe impl Trace for FinalizerWritesYoungEdge {
    unsafe fn trace(&self, tracer: &mut Tracer) {
        // SAFETY: forwarding the collector's trace call preserves its pointer
        // validity contract.
        unsafe {
            self.parent.trace(tracer);
            self.child.trace(tracer);
        }
    }

    fn run_finalizer(&self) {
        Finalize::finalize(self);
    }
}

#[test]
fn major_second_mark_observes_old_parent_writes_from_finalizers() {
    run_test(|| {
        let parent = Rooted::new(OldHolder {
            child: GcRefCell::new(None),
        });
        force_minor_collect();
        force_minor_collect();
        assert!(is_old(&parent));

        let child = GcEdge::new(42_u32);
        let _unreachable_finalizer = GcEdge::new(FinalizerWritesYoungEdge {
            parent: parent.clone().into_edge(),
            child,
        });

        force_collect();

        assert_eq!(
            parent.child.borrow().as_ref().map(|value| **value),
            Some(42)
        );
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

#[derive(Debug, Finalize)]
struct CountingOldHolder {
    traces: Arc<AtomicUsize>,
    child: GcRefCell<Option<GcEdge<u32>>>,
}

unsafe impl Trace for CountingOldHolder {
    unsafe fn trace(&self, tracer: &mut Tracer) {
        self.traces.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarding the collector's trace call preserves its pointer
        // validity contract.
        unsafe { self.child.trace(tracer) };
    }

    fn run_finalizer(&self) {
        Finalize::finalize(self);
        self.child.run_finalizer();
    }
}

#[test]
fn old_parent_is_rescanned_only_after_another_write() {
    run_test(|| {
        let traces = Arc::new(AtomicUsize::new(0));
        let holder = Rooted::new(CountingOldHolder {
            traces: Arc::clone(&traces),
            child: GcRefCell::new(None),
        });

        force_minor_collect();
        force_minor_collect();
        assert!(is_old(&holder));

        *holder.child.borrow_mut() = Some(GcEdge::new(1));
        let before_first_write = traces.load(Ordering::Relaxed);
        force_minor_collect();
        let after_first_write = traces.load(Ordering::Relaxed);
        assert_eq!(after_first_write, before_first_write + 1);

        // The surviving child keeps this nursery collection non-empty. With no
        // intervening mutation, the old holder must not be traced again.
        force_minor_collect();
        assert_eq!(traces.load(Ordering::Relaxed), after_first_write);

        *holder.child.borrow_mut() = Some(GcEdge::new(2));
        force_minor_collect();
        assert_eq!(traces.load(Ordering::Relaxed), after_first_write + 1);
        assert_eq!(holder.child.borrow().as_ref().map(|value| **value), Some(2));
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
fn retargeting_an_old_weak_edge_reuses_its_ephemeron() {
    run_test(|| {
        let first = Rooted::new(1_u32);
        let mut weak = WeakGcEdge::new_rooted(&first);
        let _weak_root = weak.root();

        force_minor_collect();
        force_minor_collect();
        Harness::assert_ephemeron_allocations(1);
        Harness::assert_remembered_ephemerons(1);

        let second = Rooted::new(2_u32);
        let second_edge = second.clone().into_edge();
        weak.retarget_edge(&second_edge);
        Harness::assert_ephemeron_allocations(1);

        force_minor_collect();
        assert_eq!(weak.upgrade().as_deref().copied(), Some(2));

        drop(first);
        drop(second);
        force_collect();
        assert!(weak.upgrade().is_none());
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
