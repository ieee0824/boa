use super::run_test;
use crate::{
    Ephemeron, GcEdge, Rooted, WeakGc, WeakGcEdge, registered_ephemeron_roots, registered_roots,
};

#[test]
fn explicit_roots_follow_handle_lifetimes() {
    run_test(|| {
        assert!(registered_roots().is_empty());

        let root = Rooted::new(7_u32);
        let entries = registered_roots();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].handles, 1);
        assert_eq!(entries[0].pointer, root.as_gc().as_erased_pointer());

        let clone = root.clone();
        assert_eq!(registered_roots()[0].handles, 2);

        drop(clone);
        assert_eq!(registered_roots()[0].handles, 1);

        drop(root);
        assert!(registered_roots().is_empty());
    });
}

#[test]
fn root_edge_conversions_update_registration() {
    run_test(|| {
        let root = Rooted::new(11_u32);
        assert_eq!(registered_roots().len(), 1);

        let edge = root.into_edge();
        assert!(registered_roots().is_empty());
        assert_eq!(**edge.as_gc(), 11);

        let root = edge.root();
        assert_eq!(registered_roots().len(), 1);
        assert_eq!(**root.as_gc(), 11);

        drop(root);
        assert!(registered_roots().is_empty());
    });
}

#[test]
fn edge_allocation_and_weak_promotion_register_only_explicit_roots() {
    run_test(|| {
        let edge = GcEdge::new(13_u32);
        assert!(registered_roots().is_empty());

        let weak = WeakGcEdge::new_edge(&edge);
        let root = weak.upgrade_rooted().expect("edge is still live");
        assert_eq!(registered_roots().len(), 1);
        assert_eq!(*root, 13);

        drop(root);
        assert!(registered_roots().is_empty());
    });
}

#[test]
fn ephemeron_roots_follow_handle_lifetimes() {
    run_test(|| {
        assert!(registered_ephemeron_roots().is_empty());

        let key = Rooted::new(19_u32);
        let ephemeron = Ephemeron::new(&key, 23_u32);
        let entries = registered_ephemeron_roots();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].handles, 1);

        let edge = ephemeron.clone().into_edge();
        let entries = registered_ephemeron_roots();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].handles, 1);
        assert!(std::ptr::addr_eq(
            entries[0].pointer.as_ptr(),
            edge.erased_inner_ptr().as_ptr()
        ));

        let second_root = Ephemeron::from_edge(edge.clone());
        assert_eq!(registered_ephemeron_roots()[0].handles, 2);
        drop(second_root);
        assert_eq!(registered_ephemeron_roots()[0].handles, 1);

        drop(ephemeron);
        assert!(registered_ephemeron_roots().is_empty());
    });
}

#[test]
fn weak_root_edge_conversions_update_ephemeron_registration() {
    run_test(|| {
        let key = Rooted::new(29_u32);
        let weak = WeakGc::new(&key);
        assert_eq!(registered_ephemeron_roots().len(), 1);

        let edge = weak.into_edge();
        assert!(registered_ephemeron_roots().is_empty());
        assert_eq!(*edge.upgrade().expect("key remains live"), 29);

        let weak = WeakGc::from_edge(edge);
        assert_eq!(registered_ephemeron_roots().len(), 1);
        assert_eq!(*weak.upgrade().expect("key remains live"), 29);

        drop(weak);
        assert!(registered_ephemeron_roots().is_empty());
    });
}

#[test]
fn raw_round_trip_restores_root_registration() {
    run_test(|| {
        let root = Rooted::new(17_u32);
        let raw = Rooted::into_raw(root);
        assert!(registered_roots().is_empty());

        // SAFETY: `raw` was just produced by `Rooted::into_raw` and has not
        // been reconstructed elsewhere.
        let root = unsafe { Rooted::from_raw(raw) };
        assert_eq!(registered_roots().len(), 1);
        assert_eq!(*root, 17);

        drop(root);
        assert!(registered_roots().is_empty());
    });
}

#[test]
fn roots_drop_safely_during_thread_teardown() {
    std::thread::spawn(|| {
        let _root = Rooted::new(7_u32);
    })
    .join()
    .expect("collector teardown must not outlive its root registry");
}
