use super::run_test;
use crate::{GcEdge, GcRefCell, Rooted};

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
