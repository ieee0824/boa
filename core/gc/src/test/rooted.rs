use super::run_test;
use crate::{Finalize, Gc, GcEdge, Rooted, Trace, WeakGc, registered_roots};

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

        let weak = WeakGc::new_edge(&edge);
        let root = weak.upgrade_rooted().expect("edge is still live");
        assert_eq!(registered_roots().len(), 1);
        assert_eq!(*root, 13);

        drop(root);
        assert!(registered_roots().is_empty());
    });
}

#[test]
fn rooted_fields_drop_safely_during_thread_teardown() {
    #[derive(Trace, Finalize)]
    struct Holder {
        root: Rooted<u32>,
    }

    std::thread::spawn(|| {
        let _holder = Gc::new(Holder {
            root: Rooted::new(7),
        });
    })
    .join()
    .expect("collector teardown must not outlive its root registry");
}
