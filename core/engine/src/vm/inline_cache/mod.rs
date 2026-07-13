use std::cell::Cell;

use boa_gc::GcRefCell;
use boa_macros::{Finalize, Trace};

use crate::{
    JsString,
    object::shape::{Shape, WeakShape, slot::Slot, slot::SlotAttributes},
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
}

impl InlineCache {
    pub(crate) const fn new(name: JsString) -> Self {
        Self {
            name,
            shape: GcRefCell::new(WeakShape::None),
            prototype_shape: GcRefCell::new(WeakShape::None),
            slot: Cell::new(Slot::new()),
        }
    }

    pub(crate) fn set(&self, shape: &Shape, slot: Slot) {
        *self.shape.borrow_mut() = shape.into();
        // Prototype-property slots index into the holder prototype's storage, so
        // remember the prototype's shape to guard against it being reindexed
        // later (see the `prototype_shape` field docs). A cachable prototype
        // slot is always resolved on the receiver's immediate prototype
        // (`Slot::set_not_cachable_if_already_prototype` makes deeper lookups
        // non-cachable), so `shape.prototype()` is exactly the holder.
        *self.prototype_shape.borrow_mut() = if slot.attributes.contains(SlotAttributes::PROTOTYPE) {
            match shape.prototype() {
                Some(prototype) => WeakShape::from(prototype.borrow().shape()),
                None => WeakShape::None,
            }
        } else {
            WeakShape::None
        };
        self.slot.set(slot);
    }

    pub(crate) fn slot(&self) -> Slot {
        self.slot.get()
    }

    /// Returns `Some((shape, slot))`, if the [`InlineCache`]'s cached shape
    /// matches the given receiver shape (and, for a prototype-property slot, the
    /// holder prototype's shape still matches too).
    ///
    /// Otherwise we reset the internal weak reference(s) to [`WeakShape::None`],
    /// so they can be deallocated by the GC.
    pub(crate) fn match_or_reset(&self, shape: &Shape) -> Option<(Shape, Slot)> {
        let mut old = self.shape.borrow_mut();

        let old_upgraded = old.upgrade();
        if old_upgraded.as_ref().map_or(0, Shape::to_addr_usize) != shape.to_addr_usize() {
            *old = WeakShape::None;
            *self.prototype_shape.borrow_mut() = WeakShape::None;
            return None;
        }

        let matched = old_upgraded.expect("addr matched a live shape, so it upgrades");
        let slot = self.slot();

        // A prototype-property slot indexes into the holder prototype's storage;
        // the receiver-shape match above does not observe changes to that
        // prototype. Require the holder prototype's current shape to still match
        // the one recorded at cache time, otherwise the slot index may be stale.
        if slot.attributes.contains(SlotAttributes::PROTOTYPE) {
            let current_prototype_addr = matched
                .prototype()
                .map_or(0, |prototype| prototype.borrow().shape().to_addr_usize());

            let mut cached_prototype = self.prototype_shape.borrow_mut();
            let cached_addr = cached_prototype
                .upgrade()
                .as_ref()
                .map_or(0, Shape::to_addr_usize);

            // Treat a missing prototype (`0`) as a miss: a `0 == 0` comparison
            // here would be a false match between a collected cached shape and a
            // now-prototype-less receiver.
            if current_prototype_addr == 0 || cached_addr != current_prototype_addr {
                *cached_prototype = WeakShape::None;
                *old = WeakShape::None;
                return None;
            }
        }

        Some((matched, slot))
    }
}
