use super::run_test;
use crate::{GcEdge, GcRefCell, Rooted, collection_blocked_borrows};

#[test]
fn tracks_collection_blocking_borrows() {
    run_test(|| {
        let first = GcRefCell::new(1);
        let second = GcRefCell::new(2);

        assert_eq!(collection_blocked_borrows(), 0);
        let first_borrow = first.borrow_mut();
        assert_eq!(collection_blocked_borrows(), 1);
        let second_borrow = second.borrow_mut();
        assert_eq!(collection_blocked_borrows(), 2);
        drop(first_borrow);
        assert_eq!(collection_blocked_borrows(), 1);
        drop(second_borrow);
        assert_eq!(collection_blocked_borrows(), 0);
    });
}

#[test]
fn no_gc_borrow_does_not_block_collection() {
    run_test(|| {
        let cell = GcRefCell::new(1);

        assert_eq!(collection_blocked_borrows(), 0);
        // SAFETY: the test only mutates an integer while the guard is alive.
        let mut value = unsafe { cell.borrow_mut_no_gc() };
        *value = 2;
        assert_eq!(collection_blocked_borrows(), 0);
        drop(value);
        assert_eq!(collection_blocked_borrows(), 0);
    });
}

#[test]
fn boa_borrow_mut_test() {
    run_test(|| {
        let v = Rooted::new(GcRefCell::new(Vec::new()));

        for _ in 1..=259 {
            let cell = GcEdge::new(GcRefCell::new([0u8; 10]));
            v.borrow_mut().push(cell);
        }
    });
}
