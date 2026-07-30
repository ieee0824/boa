use crate::BOA_GC;

mod allocation;
mod cell;
mod erased;
mod rooted;
mod weak;
mod weak_map;

struct Harness;

impl Harness {
    #[track_caller]
    fn assert_collections(o: usize) {
        BOA_GC.with(|current| {
            let gc = current.borrow();
            assert_eq!(gc.runtime.collections, o);
        });
    }

    #[track_caller]
    fn assert_threshold(bytes: usize) {
        BOA_GC.with(|current| {
            let gc = current.borrow();
            assert_eq!(gc.config.threshold, bytes);
        });
    }

    #[track_caller]
    fn assert_collected_at_least(collections: usize) {
        BOA_GC.with(|current| {
            let gc = current.borrow();
            assert!(
                gc.runtime.collections >= collections,
                "expected at least {collections} collections, got {}",
                gc.runtime.collections
            );
        });
    }

    #[track_caller]
    fn assert_empty_gc() {
        BOA_GC.with(|current| {
            let gc = current.borrow();

            assert!(gc.strongs.is_empty());
            assert_eq!(gc.runtime.bytes_allocated, 0);
        });
    }

    #[track_caller]
    fn assert_bytes_allocated() {
        BOA_GC.with(|current| {
            let gc = current.borrow();
            assert!(gc.runtime.bytes_allocated > 0);
        });
    }

    #[track_caller]
    fn assert_exact_bytes_allocated(bytes: usize) {
        BOA_GC.with(|current| {
            let gc = current.borrow();
            assert_eq!(gc.runtime.bytes_allocated, bytes);
        });
    }

    /// Asserts how many strong allocations the collector is still holding.
    #[track_caller]
    fn assert_strong_allocations(count: usize) {
        BOA_GC.with(|current| {
            let gc = current.borrow();
            assert_eq!(
                gc.strongs.len(),
                count,
                "expected {count} strong allocations, got {}",
                gc.strongs.len()
            );
        });
    }

    /// Makes every subsequent allocation trigger a collection.
    fn collect_on_every_allocation() {
        BOA_GC.with(|current| {
            current.borrow_mut().config.threshold = 0;
        });
    }
}

#[track_caller]
fn run_test(test: impl FnOnce() + Send + 'static) {
    let handle = std::thread::spawn(test);
    handle.join().unwrap();
}
