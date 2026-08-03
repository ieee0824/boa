use std::{cell::Cell, fmt};

/// The generation of a collector allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Generation {
    /// Newly allocated objects are collected by a minor collection.
    Young,
    /// Objects that survived enough minor collections are collected by a major collection.
    Old,
}

/// The `Gcheader` contains the `GcBox`'s and `EphemeronBox`'s current state for the `Collector`'s
/// Mark/Sweep as well as a pointer to the next node in the heap.
///
/// The next node is set by the `Allocator` during initialization and by the
/// `Collector` during the sweep phase.
pub(crate) struct GcHeader {
    marked: Cell<bool>,

    /// Mark bit used only by minor collections. Major collection marks are kept
    /// separate because old objects are not swept by a minor collection.
    minor_marked: Cell<bool>,

    /// The current generation and the number of minor collections survived while
    /// young. The latter is deliberately small: it only controls promotion.
    generation: Cell<Generation>,
    minor_survivals: Cell<u8>,

    /// Whether promotion tracing has visited this allocation's old descendants
    /// and installed their mutable-cell write barriers. This is monotonic for
    /// the lifetime of an allocation; later writes are covered by those
    /// barriers and do not require rescanning the old graph.
    barriers_installed: Cell<bool>,

    /// How many explicitly registered roots point at this allocation.
    ///
    /// Kept here rather than in a side table so that registering a root costs a counter
    /// increment. A hash map keyed by address would put a hash and a probe on every
    /// clone and drop of a rooted handle, which is far too much for handles as common as
    /// object and value handles.
    root_count: Cell<u32>,
}

impl GcHeader {
    /// Creates a new unmarked, unrooted [`GcHeader`].
    pub(crate) fn new() -> Self {
        Self {
            marked: Cell::new(false),
            minor_marked: Cell::new(false),
            generation: Cell::new(Generation::Young),
            minor_survivals: Cell::new(0),
            barriers_installed: Cell::new(false),
            root_count: Cell::new(0),
        }
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

    /// Returns whether this allocation belongs to the nursery.
    pub(crate) fn is_young(&self) -> bool {
        self.generation.get() == Generation::Young
    }

    /// Returns whether this allocation has been promoted out of the nursery.
    pub(crate) fn is_old(&self) -> bool {
        self.generation.get() == Generation::Old
    }

    /// Returns whether this allocation was marked by the current minor collection.
    pub(crate) fn is_minor_marked(&self) -> bool {
        self.minor_marked.get()
    }

    /// Marks this allocation as reachable by the current minor collection.
    pub(crate) fn minor_mark(&self) {
        self.minor_marked.set(true);
    }

    /// Clears the minor mark at the end of a minor collection.
    pub(crate) fn minor_unmark(&self) {
        self.minor_marked.set(false);
    }

    /// Records one minor collection survived and promotes after the first
    /// survivor pass. Keeping the threshold here makes it easy to tune without
    /// changing the collector's pointer representation.
    pub(crate) fn promote_if_mature(&self) -> bool {
        const PROMOTION_SURVIVALS: u8 = 1;

        if self.is_old() {
            return false;
        }

        let survivals = self.minor_survivals.get();
        if survivals >= PROMOTION_SURVIVALS {
            self.generation.set(Generation::Old);
            self.minor_survivals.set(0);
            true
        } else {
            self.minor_survivals.set(survivals + 1);
            false
        }
    }

    pub(crate) fn barriers_installed(&self) -> bool {
        self.barriers_installed.get()
    }

    pub(crate) fn mark_barriers_installed(&self) {
        self.barriers_installed.set(true);
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
            .field("minor_marked", &self.is_minor_marked())
            .field("generation", &self.generation.get())
            .field("minor_survivals", &self.minor_survivals.get())
            .field("barriers_installed", &self.barriers_installed())
            .field("root_count", &self.root_count())
            .finish_non_exhaustive()
    }
}
