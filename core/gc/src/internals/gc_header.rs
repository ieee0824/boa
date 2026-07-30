use std::{cell::Cell, fmt};

/// The `Gcheader` contains the `GcBox`'s and `EphemeronBox`'s current state for the `Collector`'s
/// Mark/Sweep as well as a pointer to the next node in the heap.
///
/// The next node is set by the `Allocator` during initialization and by the
/// `Collector` during the sweep phase.
pub(crate) struct GcHeader {
    marked: Cell<bool>,

    /// How many explicitly registered roots point at this allocation.
    ///
    /// Kept here rather than in a side table so that registering a root costs a counter
    /// increment. A hash map keyed by address would put a hash and a probe on every
    /// clone and drop of a rooted handle, which is far too much for handles as common as
    /// object and value handles.
    root_count: Cell<u32>,

    /// TEMPORARY #330 DIAGNOSTIC — remove before merging.
    ///
    /// Set instead of freeing when the root diagnostic is enabled, so that a handle
    /// which outlived its allocation reports itself on the next dereference rather
    /// than reading freed memory.
    poisoned: Cell<bool>,
}

impl GcHeader {
    /// Creates a new unmarked, unrooted [`GcHeader`].
    pub(crate) fn new() -> Self {
        Self {
            marked: Cell::new(false),
            root_count: Cell::new(0),
            poisoned: Cell::new(false),
        }
    }

    /// TEMPORARY #330 DIAGNOSTIC — remove before merging.
    pub(crate) fn is_poisoned(&self) -> bool {
        self.poisoned.get()
    }

    /// TEMPORARY #330 DIAGNOSTIC — remove before merging.
    pub(crate) fn poison(&self) {
        self.poisoned.set(true);
    }

    /// Returns a bool for whether [`GcHeader`]'s mark bit is 1.
    pub(crate) fn is_marked(&self) -> bool {
        self.marked.get()
    }

    /// Sets [`GcHeader`]'s mark bit to 1.
    pub(crate) fn mark(&self) {
        self.marked.set(true);
    }

    /// Sets [`GcHeader`]'s mark bit to 0.
    pub(crate) fn unmark(&self) {
        self.marked.set(false);
    }

    /// Returns whether any explicitly registered root points at this allocation.
    pub(crate) fn is_rooted(&self) -> bool {
        self.root_count.get() > 0
    }

    /// Returns how many explicitly registered roots point at this allocation.
    pub(crate) fn root_count(&self) -> u32 {
        self.root_count.get()
    }

    /// Registers one more root pointing at this allocation.
    ///
    /// Returns whether this made the allocation rooted, so the caller can maintain a
    /// count of rooted allocations without walking the heap.
    pub(crate) fn register_root(&self) -> bool {
        let previous = self.root_count.get();
        self.root_count.set(
            previous
                .checked_add(1)
                .expect("root count overflowed for a single allocation"),
        );
        previous == 0
    }

    /// Removes one root pointing at this allocation.
    ///
    /// Returns whether this made the allocation unrooted.
    pub(crate) fn unregister_root(&self) -> bool {
        let previous = self.root_count.get();
        assert!(previous > 0, "attempted to unregister an unknown GC root");
        self.root_count.set(previous - 1);
        previous == 1
    }
}

impl fmt::Debug for GcHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcHeader")
            .field("marked", &self.is_marked())
            .field("root_count", &self.root_count())
            .finish_non_exhaustive()
    }
}
