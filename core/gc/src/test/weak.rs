use std::{cell::Cell, rc::Rc};

use super::run_test;
use crate::{
    Ephemeron, EphemeronEdge, Finalize, GcBox, GcEdge, GcRefCell, Rooted, Trace, WeakGc,
    WeakGcEdge, force_collect, internals::EphemeronBox, test::Harness,
};

#[test]
fn eph_weak_gc_test() {
    run_test(|| {
        let gc_value = Rooted::new(3);

        {
            let cloned_gc = gc_value.clone();

            let weak = WeakGc::new(&cloned_gc);

            assert_eq!(*weak.upgrade().expect("Is live currently"), 3);
            drop(cloned_gc);
            force_collect();
            assert_eq!(*weak.upgrade().expect("WeakGc is still live here"), 3);

            drop(gc_value);
            force_collect();

            assert!(weak.upgrade().is_none());
        }
    });
}

#[test]
fn eph_ephemeron_test() {
    run_test(|| {
        let gc_value = Rooted::new(3);

        {
            let cloned_gc = gc_value.clone();

            let ephemeron = Ephemeron::new(&cloned_gc, String::from("Hello World!"));

            assert_eq!(
                *ephemeron.value().expect("Ephemeron is live"),
                String::from("Hello World!")
            );
            drop(cloned_gc);
            force_collect();
            assert_eq!(
                *ephemeron.value().expect("Ephemeron is still live here"),
                String::from("Hello World!")
            );

            drop(gc_value);
            force_collect();

            assert!(ephemeron.value().is_none());
        }
    });
}

#[test]
fn eph_allocation_chains() {
    run_test(|| {
        let gc_value = Rooted::new(String::from("foo"));

        {
            let cloned_gc = gc_value.clone();
            let weak = WeakGcEdge::new_rooted(&cloned_gc);
            let wrap = Rooted::new(weak);

            assert_eq!(wrap.upgrade().as_deref().map(String::as_str), Some("foo"));

            let eph = Ephemeron::new(&wrap, 3);

            drop(cloned_gc);
            force_collect();
            assert_eq!(wrap.upgrade().as_deref().map(String::as_str), Some("foo"));
            assert_eq!(eph.value(), Some(3));

            drop(gc_value);
            force_collect();
            assert!(wrap.upgrade().is_none());
            assert_eq!(eph.value(), Some(3));

            drop(wrap);
            force_collect();
            assert!(eph.value().is_none());
        }
    });
}

#[test]
fn eph_basic_alloc_dump_test() {
    run_test(|| {
        let gc_value = Rooted::new(String::from("gc here"));
        let _gc_two = Rooted::new("hmmm");

        let eph = Ephemeron::new(&gc_value, 4);
        let _fourth = Rooted::new("tail");

        assert_eq!(eph.value(), Some(4));
    });
}

#[test]
fn eph_basic_upgrade_test() {
    run_test(|| {
        let init_gc = Rooted::new(String::from("foo"));

        let weak = WeakGc::new(&init_gc);

        let new_gc = weak.upgrade().expect("Weak is still live");

        drop(weak);
        force_collect();

        assert_eq!(*init_gc, *new_gc);
    });
}

#[test]
fn eph_basic_clone_test() {
    run_test(|| {
        let init_gc = Rooted::new(String::from("bar"));

        let weak = WeakGc::new(&init_gc);

        let new_gc = weak.upgrade().expect("Weak is live");
        let new_weak = weak.clone();

        drop(weak);
        force_collect();

        assert_eq!(*new_gc, *new_weak.upgrade().expect("weak should be live"));
        assert_eq!(
            *init_gc,
            *new_weak.upgrade().expect("weak_should be live still")
        );
    });
}

#[test]
fn eph_self_referential() {
    #[derive(Trace, Finalize, Clone)]
    struct InnerCell {
        inner: GcRefCell<Option<EphemeronEdge<InnerCell, TestCell>>>,
    }
    #[derive(Trace, Finalize, Clone)]
    struct TestCell {
        inner: GcEdge<InnerCell>,
    }
    run_test(|| {
        let root_inner = Rooted::new(InnerCell {
            inner: GcRefCell::new(None),
        });
        let root = TestCell {
            inner: root_inner.clone().into_edge(),
        };
        let root_size = size_of::<GcBox<InnerCell>>();

        Harness::assert_exact_bytes_allocated(root_size);

        {
            // Generate a self-referential ephemeron
            let eph = Ephemeron::new(&root_inner, root.clone());
            *root.inner.inner.borrow_mut() = Some(eph.clone().into_edge());

            assert!(eph.value().is_some());
            let eph_size = size_of::<EphemeronBox<InnerCell, TestCell>>();
            Harness::assert_exact_bytes_allocated(root_size + eph_size);
        }

        *root.inner.inner.borrow_mut() = None;

        force_collect();

        Harness::assert_exact_bytes_allocated(root_size);
    });
}

#[test]
fn eph_self_referential_chain() {
    #[derive(Trace, Finalize, Clone)]
    struct TestCell {
        inner: GcEdge<GcRefCell<Option<EphemeronEdge<u8, TestCell>>>>,
    }
    run_test(|| {
        type ChainCell = GcRefCell<Option<EphemeronEdge<u8, TestCell>>>;
        let root = Rooted::new(GcRefCell::new(None));
        let root_size = size_of::<GcBox<GcRefCell<Option<Ephemeron<u8, TestCell>>>>>();

        Harness::assert_exact_bytes_allocated(root_size);

        let watched = Rooted::new(0);

        {
            // Generate a self-referential loop of weak and non-weak pointers
            let chain1 = TestCell {
                inner: GcEdge::new(GcRefCell::new(None)),
            };
            let chain2 = TestCell {
                inner: GcEdge::new(GcRefCell::new(None)),
            };

            let eph_start = Ephemeron::new(&watched, chain1.clone());
            let eph_chain2 = Ephemeron::new(&watched, chain2.clone());

            *chain1.inner.borrow_mut() = Some(eph_chain2.clone().into_edge());
            *chain2.inner.borrow_mut() = Some(eph_start.clone().into_edge());

            *root.borrow_mut() = Some(eph_start.clone().into_edge());

            force_collect();

            assert!(eph_start.value().is_some());
            assert!(eph_chain2.value().is_some());
            let chain_cell_size = size_of::<GcBox<ChainCell>>();
            let eph_size = size_of::<EphemeronBox<u8, TestCell>>();
            let watched_size = size_of::<GcBox<u8>>();
            let expected = root_size + watched_size + 2 * (chain_cell_size + eph_size);
            Harness::assert_exact_bytes_allocated(expected);
        }

        *root.borrow_mut() = None;

        force_collect();

        drop(watched);

        force_collect();

        Harness::assert_exact_bytes_allocated(root_size);
    });
}

#[test]
fn eph_finalizer() {
    #[derive(Clone, Trace)]
    struct S {
        #[unsafe_ignore_trace]
        inner: Rc<Cell<u8>>,
    }

    impl Finalize for S {
        fn finalize(&self) {
            self.inner.set(self.inner.get() + 1);
        }
    }

    run_test(|| {
        let val = S {
            inner: Rc::new(Cell::new(0)),
        };

        let key = Rooted::new(50u32);
        let eph = Ephemeron::new(&key, val.clone());
        assert!(eph.has_value());
        // finalize hasn't been run
        assert_eq!(val.inner.get(), 0);

        drop(key);
        force_collect();
        assert!(!eph.has_value());
        // finalize ran when collecting
        assert_eq!(val.inner.get(), 1);
    });
}

#[test]
fn eph_gc_finalizer() {
    #[derive(Clone, Trace)]
    struct S {
        #[unsafe_ignore_trace]
        inner: Rc<Cell<u8>>,
    }

    impl Finalize for S {
        fn finalize(&self) {
            self.inner.set(self.inner.get() + 1);
        }
    }

    run_test(|| {
        let val = S {
            inner: Rc::new(Cell::new(0)),
        };

        let key = Rooted::new(50u32);
        let eph = Ephemeron::new(&key, GcEdge::new(val.clone()));
        assert!(eph.has_value());
        // finalize hasn't been run
        assert_eq!(val.inner.get(), 0);

        drop(key);
        force_collect();
        assert!(!eph.has_value());
        // finalize ran when collecting
        assert_eq!(val.inner.get(), 1);
    });
}

#[test]
fn eph_strong_self_reference() {
    type Inner = GcRefCell<(Option<TestCell>, Option<TestCell>)>;
    #[derive(Trace, Finalize, Clone)]
    struct TestCell {
        inner: GcEdge<Inner>,
    }
    run_test(|| {
        let root_inner = Rooted::new(GcRefCell::new((None, None)));
        let root = TestCell {
            inner: root_inner.clone().into_edge(),
        };
        let root_size = size_of::<GcBox<Inner>>();

        Harness::assert_exact_bytes_allocated(root_size);

        let watched = Rooted::new(0);
        let watched_size = size_of::<GcBox<i32>>();

        {
            let eph = Ephemeron::new(&watched, root.clone());
            let eph_size = size_of::<EphemeronBox<i32, TestCell>>();

            root.inner.borrow_mut().0 = Some(root.clone());
            root.inner.borrow_mut().1 = Some(root.clone());

            force_collect();

            assert!(eph.value().is_some());
            Harness::assert_exact_bytes_allocated(root_size + eph_size + watched_size);
        }

        force_collect();

        drop(watched);

        force_collect();

        Harness::assert_exact_bytes_allocated(root_size);
    });
}
