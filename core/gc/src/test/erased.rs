use std::any::TypeId;

use boa_macros::{Finalize, Trace};

use super::run_test;
use crate::{Gc, GcBox, GcErased, GcErasedEdge, Rooted, force_collect, test::Harness};

#[test]
fn erased_gc() {
    run_test(|| {
        let value = vec![1, 2, 3];
        let gc = Rooted::new(value.clone());

        assert_eq!(Gc::type_id(gc.as_gc()), TypeId::of::<Vec<i32>>());

        let erased = GcErased::new(gc.clone());

        assert_eq!(erased.type_id(), TypeId::of::<Vec<i32>>());
        assert!(erased.is::<Vec<i32>>());

        assert!(erased.clone().downcast::<i32>().is_none());

        let gc_from_erased = erased.downcast::<Vec<i32>>().unwrap();
        assert_eq!(**gc_from_erased, value);

        assert!(Gc::ptr_eq(gc.as_gc(), gc_from_erased.as_gc()));
    });
}

#[test]
fn nested_erased_gc() {
    #[derive(Debug, Trace, Finalize)]
    struct List {
        value: i32,
        next: Option<GcErasedEdge>,
    }

    run_test(|| {
        let mut root = GcErased::new(Rooted::new(List {
            value: 0,
            next: None,
        }));

        for value in 1..100 {
            root = GcErased::new(Rooted::new(List {
                value,
                next: Some(root.into_edge()),
            }));
        }

        Harness::assert_exact_bytes_allocated(100 * size_of::<GcBox<List>>());
        force_collect();
        Harness::assert_exact_bytes_allocated(100 * size_of::<GcBox<List>>());

        let mut head = root.into_edge().downcast::<List>();
        for value in (0..100).rev() {
            let head_unwrap = head.as_ref().unwrap();

            assert_eq!(head_unwrap.value, value);

            head = head_unwrap
                .next
                .clone()
                .and_then(GcErasedEdge::downcast::<List>);
        }
    });
}

#[test]
fn c_style_inheritance() {
    #[repr(C)]
    #[derive(Debug, Trace, Finalize, PartialEq, Eq)]
    struct Base {
        base_field: Vec<i32>,
    }

    #[repr(C)]
    #[derive(Debug, Trace, Finalize, PartialEq, Eq)]
    struct Derived {
        base: Base,
        derived_field: Vec<i64>,
    }

    run_test(|| {
        let value = vec![1, 2, 3];
        let derived = Rooted::new(Derived {
            base: Base {
                base_field: value.clone(),
            },
            derived_field: vec![4, 5, 6],
        });

        assert_eq!(Gc::type_id(derived.as_gc()), TypeId::of::<Derived>());
        assert!(Rooted::is::<Derived>(&derived));

        // SAFETY: The structs have #[repr(C)] so this is safe.
        let base = unsafe { Rooted::cast_unchecked::<Base>(derived.clone()) };

        assert_eq!(Gc::type_id(base.as_gc()), TypeId::of::<Derived>());
        assert!(Rooted::is::<Derived>(&base));

        assert_eq!(base.base_field, value);
        assert_eq!(base.base_field, derived.base.base_field);

        assert!(Rooted::ptr_eq(&base, &derived));

        assert!(Rooted::downcast::<i32>(base.clone()).is_none());
        assert_eq!(*Rooted::downcast::<Derived>(base).unwrap(), *derived);
    });
}
