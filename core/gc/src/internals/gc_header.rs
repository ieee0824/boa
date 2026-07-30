use std::{cell::Cell, fmt};

/// The `Gcheader` contains the `GcBox`'s and `EphemeronBox`'s current state for the `Collector`'s
/// Mark/Sweep as well as a pointer to the next node in the heap.
///
/// The next node is set by the `Allocator` during initialization and by the
/// `Collector` during the sweep phase.
pub(crate) struct GcHeader {
    marked: Cell<bool>,
    /// TEMPORARY #330 DIAGNOSTIC — remove before merging.
    ///
    /// Set instead of freeing when the root diagnostic is enabled, so that a handle
    /// which outlived its allocation reports itself on the next dereference rather
    /// than reading freed memory.
    poisoned: Cell<bool>,
}

impl GcHeader {
    /// Creates a new unmarked [`GcHeader`].
    pub(crate) fn new() -> Self {
        Self {
            marked: Cell::new(false),
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
}

impl fmt::Debug for GcHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcHeader")
            .field("marked", &self.is_marked())
            .finish_non_exhaustive()
    }
}
