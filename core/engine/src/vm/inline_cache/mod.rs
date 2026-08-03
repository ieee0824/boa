use std::cell::Cell;

use boa_gc::GcRefCell;
use boa_macros::{Finalize, Trace};
use thin_vec::ThinVec;

use crate::{
    JsString,
    object::shape::{ShapeEdge, WeakShape, slot::Slot, slot::SlotAttributes},
};

#[cfg(test)]
mod tests;

/// An inline cache entry for a property access.
#[derive(Clone, Debug, Trace, Finalize)]
pub(crate) struct InlineCache {
    /// The property that is accessed.
    pub(crate) name: JsString,

    /// A pointer is kept to the shape to avoid the shape from being deallocated.
    pub(crate) shape: GcRefCell<WeakShape>,

    /// For a prototype-property slot, the shape of the holder prototype at the
    /// time the slot was cached.
    ///
    /// The cached [`Slot`] indexes into the *prototype's* property storage, but
    /// [`Self::shape`] only tracks the *receiver's* shape. Mutating the
    /// prototype (e.g. a polyfill deleting or redefining methods on
    /// `String.prototype`) leaves the receiver's shape untouched yet shifts the
    /// prototype's storage layout, so the cached slot index would then point at
    /// a different property. Guarding on the prototype's shape as well detects
    /// that mutation and forces a cache miss. It is [`WeakShape::None`] for
    /// own-property slots (which do not read from a prototype).
    pub(crate) prototype_shape: GcRefCell<WeakShape>,

    /// The [`Slot`] of the property.
    #[unsafe_ignore_trace]
    pub(crate) slot: Cell<Slot>,

    /// Additional receiver shapes retained after the first (monomorphic) entry.
    ///
    /// The first entry stays in the fields above because monomorphic accesses
    /// are the common case and should not pay for a collection scan. The
    /// bounded secondary list makes the same call site useful for a small,
    /// stable set of shapes without keeping any shape alive strongly.
    secondary: GcRefCell<ThinVec<CacheEntry>>,

    /// Round-robin victim used once all secondary entries are occupied.
    #[unsafe_ignore_trace]
    replacement: Cell<usize>,
}

/// One secondary polymorphic inline-cache entry.
///
/// This deliberately mirrors the two shape guards of the primary entry. The
/// receiver guard protects the receiver's slot index, while the optional
/// prototype guard protects a prototype-property slot from reindexing.
#[derive(Clone, Debug, Trace, Finalize)]
struct CacheEntry {
    shape: GcRefCell<WeakShape>,
    prototype_shape: GcRefCell<WeakShape>,
    #[unsafe_ignore_trace]
    slot: Cell<Slot>,
}

const MAX_SECONDARY_ENTRIES: usize = 7;

impl CacheEntry {
    const fn empty() -> Self {
        Self {
            shape: GcRefCell::new(WeakShape::None),
            prototype_shape: GcRefCell::new(WeakShape::None),
            slot: Cell::new(Slot::new()),
        }
    }

    fn from_weak_shapes(shape: WeakShape, prototype_shape: WeakShape, slot: Slot) -> Self {
        Self {
            shape: GcRefCell::new(shape),
            prototype_shape: GcRefCell::new(prototype_shape),
            slot: Cell::new(slot),
        }
    }

    fn shape_addr(&self) -> usize {
        self.shape.borrow().to_addr_usize()
    }

    fn matches(&self, shape: &ShapeEdge) -> bool {
        let current_addr = shape.to_addr_usize();
        current_addr != 0
            && self.shape_addr() == current_addr
            && prototype_matches(&self.prototype_shape, shape, self.slot.get())
    }

    fn clear(&self) {
        // SAFETY: clearing weak handles cannot allocate GC storage.
        *unsafe { self.shape.borrow_mut_no_gc() } = WeakShape::None;
        *unsafe { self.prototype_shape.borrow_mut_no_gc() } = WeakShape::None;
    }
}

fn prototype_shape(shape: &ShapeEdge, slot: Slot) -> Option<ShapeEdge> {
    if slot.attributes.contains(SlotAttributes::PROTOTYPE) {
        shape
            .prototype()
            .map(|prototype| prototype.borrow().shape_edge().clone())
    } else {
        None
    }
}

fn prototype_matches(
    cached_prototype: &GcRefCell<WeakShape>,
    shape: &ShapeEdge,
    slot: Slot,
) -> bool {
    if !slot.attributes.contains(SlotAttributes::PROTOTYPE) {
        return true;
    }

    let current = shape.prototype().map_or(0, |prototype| {
        prototype.borrow().shape_edge().to_addr_usize()
    });
    current != 0 && current == cached_prototype.borrow().to_addr_usize()
}

fn set_cached_entry(
    cached_shape: &GcRefCell<WeakShape>,
    cached_prototype: &GcRefCell<WeakShape>,
    cached_slot: &Cell<Slot>,
    shape: &ShapeEdge,
    slot: Slot,
) {
    // SAFETY: retargeting an existing ephemeron does not allocate GC storage.
    let reused_shape = { unsafe { cached_shape.borrow_mut_no_gc() }.retarget(shape) };
    if !reused_shape {
        // Creating the first weak handle allocates its ephemeron, so do that
        // before taking the no-GC mutable borrow used for assignment.
        let weak_shape = shape.into();
        *unsafe { cached_shape.borrow_mut_no_gc() } = weak_shape;
    }

    let prototype = prototype_shape(shape, slot);
    let reused_prototype = prototype.as_ref().is_some_and(|prototype| {
        // SAFETY: retargeting an existing ephemeron does not allocate GC storage.
        unsafe { cached_prototype.borrow_mut_no_gc() }.retarget(prototype)
    });
    if !reused_prototype {
        let weak_prototype = prototype.as_ref().map_or(WeakShape::None, WeakShape::from);
        *unsafe { cached_prototype.borrow_mut_no_gc() } = weak_prototype;
    }
    cached_slot.set(slot);
}

impl InlineCache {
    pub(crate) fn new(name: JsString) -> Self {
        Self {
            name,
            shape: GcRefCell::new(WeakShape::None),
            prototype_shape: GcRefCell::new(WeakShape::None),
            slot: Cell::new(Slot::new()),
            secondary: GcRefCell::new(ThinVec::new()),
            replacement: Cell::new(0),
        }
    }

    pub(crate) fn set(&self, shape: &ShapeEdge, slot: Slot) {
        let current_addr = shape.to_addr_usize();
        let primary_addr = self.shape.borrow().to_addr_usize();

        // Keep a receiver-shape match in the primary slot, even if only the
        // prototype guard changed. The property slot belongs to this call
        // site, so retargeting the existing weak handles is cheaper than
        // creating a duplicate secondary entry.
        if current_addr != 0 && (primary_addr == current_addr || primary_addr == 0) {
            set_cached_entry(&self.shape, &self.prototype_shape, &self.slot, shape, slot);
            return;
        }

        // A secondary entry with the same receiver shape is updated in place.
        // This is the normal path after a prototype reindex on a polymorphic
        // call site.
        let existing = {
            let secondary = self.secondary.borrow();
            secondary
                .iter()
                .position(|entry| entry.shape_addr() == current_addr && current_addr != 0)
        };
        if let Some(index) = existing {
            let secondary = self.secondary.borrow();
            let entry = &secondary[index];
            set_cached_entry(
                &entry.shape,
                &entry.prototype_shape,
                &entry.slot,
                shape,
                slot,
            );
            return;
        }

        // Allocate the receiver weak handle before borrowing the secondary
        // vector. Creating an ephemeron can collect, so it must be installed
        // in the traced cache before allocating the prototype weak handle.
        let weak_shape = WeakShape::from(shape);
        let index = {
            let mut secondary = self.secondary.borrow_mut();
            let index = secondary
                .iter()
                .position(|entry| entry.shape_addr() == 0)
                .or_else(|| {
                    if secondary.len() < MAX_SECONDARY_ENTRIES {
                        secondary.push(CacheEntry::empty());
                        Some(secondary.len() - 1)
                    } else {
                        let index = self.replacement.get() % MAX_SECONDARY_ENTRIES;
                        self.replacement.set((index + 1) % MAX_SECONDARY_ENTRIES);
                        Some(index)
                    }
                })
                .expect("secondary inline-cache victim should exist");
            secondary[index].clear();
            secondary[index] = CacheEntry::from_weak_shapes(weak_shape, WeakShape::None, slot);
            index
        };

        // The receiver weak handle is now owned by the traced cache, so a
        // collection during this second allocation cannot leave it dangling.
        let prototype = prototype_shape(shape, slot);
        let weak_prototype = prototype.as_ref().map_or(WeakShape::None, WeakShape::from);
        let secondary = self.secondary.borrow();
        *unsafe { secondary[index].prototype_shape.borrow_mut_no_gc() } = weak_prototype;
    }

    pub(crate) fn slot(&self) -> Slot {
        self.slot.get()
    }

    #[cfg(test)]
    fn secondary_shape_count(&self) -> usize {
        self.secondary
            .borrow()
            .iter()
            .filter(|entry| entry.shape_addr() != 0)
            .count()
    }

    /// Returns `Some(slot)`, if the [`InlineCache`]'s cached shape
    /// matches the given receiver shape (and, for a prototype-property slot, the
    /// holder prototype's shape still matches too).
    ///
    /// Otherwise dead or invalid entries are cleared, while live entries for
    /// other receiver shapes remain available to the polymorphic call site.
    pub(crate) fn match_or_reset(&self, shape: &ShapeEdge) -> Option<Slot> {
        if self.primary_matches(shape) {
            return Some(self.slot());
        }

        {
            let secondary = self.secondary.borrow();
            if let Some(entry) = secondary.iter().find(|entry| entry.matches(shape)) {
                return Some(entry.slot.get());
            }
        }

        self.clear_invalid_entries(shape);
        None
    }

    fn primary_matches(&self, shape: &ShapeEdge) -> bool {
        let current_addr = shape.to_addr_usize();
        current_addr != 0
            && self.shape.borrow().to_addr_usize() == current_addr
            && prototype_matches(&self.prototype_shape, shape, self.slot())
    }

    fn clear_invalid_entries(&self, shape: &ShapeEdge) {
        let current_addr = shape.to_addr_usize();
        let primary_addr = self.shape.borrow().to_addr_usize();
        if current_addr == 0
            || primary_addr == 0
            || (primary_addr == current_addr && !self.primary_matches(shape))
        {
            // SAFETY: resetting weak handles cannot allocate GC storage.
            *unsafe { self.shape.borrow_mut_no_gc() } = WeakShape::None;
            *unsafe { self.prototype_shape.borrow_mut_no_gc() } = WeakShape::None;
        }

        let secondary = self.secondary.borrow_mut();
        for entry in secondary.iter() {
            let entry_addr = entry.shape_addr();
            if current_addr == 0
                || entry_addr == 0
                || (entry_addr == current_addr && !entry.matches(shape))
            {
                entry.clear();
            }
        }
    }
}
