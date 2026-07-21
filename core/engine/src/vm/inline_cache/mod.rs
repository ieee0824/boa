use arrayvec::ArrayVec;
use itertools::Itertools;
use std::{cell::Cell, fmt};

use boa_gc::GcRefCell;
use boa_macros::{Finalize, Trace};

use crate::{
    JsString,
    object::shape::{
        Shape, WeakShape,
        slot::{Slot, SlotAttributes},
    },
};

#[cfg(test)]
mod tests;

pub(crate) const PIC_CAPACITY: usize = 4;

/// A cached shape-to-slot mapping for a polymorphic inline cache.
#[derive(Clone, Debug, Trace, Finalize)]
pub(crate) struct CacheEntry {
    /// A weak reference is kept to the shape to avoid the shape preventing deallocation.
    pub(crate) shape: WeakShape,

    /// For a prototype-property slot, the shape of the holder prototype at the
    /// time the slot was cached. `None` for own-property slots.
    ///
    /// The cached [`Slot`] indexes into the *prototype's* property storage, but
    /// [`Self::shape`] only tracks the *receiver's* shape. Mutating the
    /// prototype (e.g. a polyfill deleting or redefining methods on
    /// `String.prototype`) leaves the receiver's shape untouched yet shifts the
    /// prototype's storage layout, so the cached slot index would then point at
    /// a different property. Guarding on the prototype's shape as well detects
    /// that mutation and forces a cache miss.
    pub(crate) prototype_shape: Option<WeakShape>,

    #[unsafe_ignore_trace]
    pub(crate) slot: Slot,
}

/// An inline cache entry for a property access.
#[derive(Clone, Debug, Trace, Finalize)]
pub(crate) struct InlineCache {
    /// The property that is accessed.
    pub(crate) name: JsString,

    /// Multiple cached shape-to-slot entries.
    pub(crate) entries: GcRefCell<ArrayVec<CacheEntry, PIC_CAPACITY>>,

    /// Whether this access site has seen too many shapes and should no longer be cached.
    #[unsafe_ignore_trace]
    pub(crate) megamorphic: Cell<bool>,
}

impl fmt::Display for InlineCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(name:{} entries:", self.name.display_escaped())?;

        if self.megamorphic.get() {
            return write!(f, "(megamorphic))");
        }

        let entries = self.entries.borrow();
        let entries = entries.iter().map(|e| e.shape.to_addr_usize()).format(", ");

        write!(f, "({entries:#x}))")
    }
}

impl InlineCache {
    pub(crate) fn new(name: JsString) -> Self {
        Self {
            name,
            entries: GcRefCell::new(ArrayVec::new()),
            megamorphic: Cell::new(false),
        }
    }

    pub(crate) fn set(&self, shape: &Shape, slot: Slot) {
        if self.megamorphic.get() {
            return;
        }

        // A prototype-property slot indexes into the holder prototype's storage,
        // so remember the prototype's shape to guard against it being reindexed
        // later (see the `CacheEntry::prototype_shape` field docs). A cacheable
        // prototype slot is always resolved on the receiver's immediate
        // prototype (`Slot::set_not_cacheable_if_already_prototype` makes deeper
        // lookups non-cacheable), so `shape.prototype()` is exactly the holder.
        let prototype_shape = if slot.attributes.contains(SlotAttributes::PROTOTYPE) {
            let Some(prototype) = shape.prototype() else {
                // A prototype slot without a prototype cannot be guarded; don't
                // cache it rather than poisoning an entry.
                return;
            };
            Some(WeakShape::from(prototype.borrow().shape()))
        } else {
            None
        };

        let mut entries = self.entries.borrow_mut();

        // Add a new entry if there's space.
        if entries
            .try_push(CacheEntry {
                shape: shape.into(),
                prototype_shape,
                slot,
            })
            .is_err()
        {
            // Polymorphic cache is full, transition to megamorphic.
            self.megamorphic.set(true);
            entries.clear();
        }
    }

    /// Returns the cached `(Shape, Slot)` if a matching shape exists in the inline cache.
    ///
    /// Opportunistically cleans up stale weak shape references during lookup.
    pub(crate) fn get(&self, shape: &Shape) -> Option<(Shape, Slot)> {
        if self.megamorphic.get() {
            return None;
        }

        let mut entries = self.entries.borrow_mut();
        let mut i = 0;
        let mut result = None;
        let shape_addr = shape.to_addr_usize();

        while i < entries.len() {
            if let Some(upgraded) = entries[i].shape.upgrade() {
                if upgraded.to_addr_usize() == shape_addr {
                    // A prototype-property slot indexes into the holder
                    // prototype's storage; the receiver-shape match above does
                    // not observe changes to that prototype. Require the holder
                    // prototype's current shape to still match the one recorded
                    // at cache time, otherwise the slot index may be stale.
                    if let Some(cached_prototype) = &entries[i].prototype_shape {
                        let current_prototype_addr = upgraded
                            .prototype()
                            .map_or(0, |prototype| prototype.borrow().shape().to_addr_usize());

                        // Treat a missing prototype (`0`) as a miss: a `0 == 0`
                        // comparison here would be a false match between a
                        // collected cached shape and a now-prototype-less
                        // receiver.
                        if current_prototype_addr == 0
                            || cached_prototype.to_addr_usize() != current_prototype_addr
                        {
                            // Drop only the stale entry; the slow path will
                            // re-resolve and re-cache it with the fresh
                            // prototype shape.
                            entries.swap_remove(i);
                            break;
                        }
                    }

                    result = Some((upgraded, entries[i].slot));
                    break;
                }
                i += 1;
            } else {
                // Opportunistically clean up stale weak shapes.
                entries.swap_remove(i);
            }
        }

        result
    }
}
