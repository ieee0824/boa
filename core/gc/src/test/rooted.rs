use super::run_test;
use crate::{Rooted, registered_roots};

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
