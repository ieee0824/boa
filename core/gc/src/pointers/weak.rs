use crate::{
    Ephemeron, EphemeronEdge, Finalize, Gc, GcEdge, Rooted, Trace, remember_ephemeron_pointer,
};
use std::{
    hash::{Hash, Hasher},
    mem::ManuallyDrop,
    ptr,
};

/// An explicitly registered external weak handle.
#[derive(Debug, Trace, Finalize)]
#[repr(transparent)]
pub struct WeakGc<T: Trace + ?Sized + 'static> {
    inner: Ephemeron<T, ()>,
}

impl<T: Trace + ?Sized> WeakGc<T> {
    /// Creates an externally owned weak handle from a rooted key.
    #[must_use]
    pub fn new(value: &Rooted<T>) -> Self {
        Self {
            inner: Ephemeron::new(value, ()),
        }
    }

    /// Promotes an unregistered weak edge into an external weak root.
    #[must_use]
    pub fn from_edge(edge: WeakGcEdge<T>) -> Self {
        let edge = ManuallyDrop::new(edge);
        // SAFETY: `edge` cannot run `Drop`, and its field is moved exactly once.
        let inner = unsafe { ptr::read(&raw const edge.inner) };
        Self {
            inner: Ephemeron::from_edge(inner),
        }
    }

    /// Converts this external weak root into a heap edge.
    #[must_use]
    pub fn into_edge(self) -> WeakGcEdge<T> {
        let this = ManuallyDrop::new(self);
        // SAFETY: `this` cannot run `Drop`, and its field is moved exactly once.
        let inner = unsafe { ptr::read(&raw const this.inner) };
        WeakGcEdge {
            inner: inner.into_edge(),
        }
    }

    /// Upgrades to an explicitly registered strong root while the key is live.
    #[must_use]
    pub fn upgrade(&self) -> Option<Rooted<T>> {
        self.inner.key()
    }

    /// Returns whether the weak key is still live.
    #[must_use]
    pub fn is_upgradable(&self) -> bool {
        self.inner.has_value()
    }
}

impl<T: Trace> Clone for WeakGc<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: Trace> PartialEq for WeakGc<T> {
    fn eq(&self, other: &Self) -> bool {
        match (self.upgrade(), other.upgrade()) {
            (Some(a), Some(b)) => Gc::ptr_eq(a.as_gc(), b.as_gc()),
            _ => false,
        }
    }
}

impl<T: Trace> Eq for WeakGc<T> {}

impl<T: Trace> Hash for WeakGc<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        if let Some(value) = self.upgrade() {
            ptr::hash(value.as_gc().as_ref(), state);
        } else {
            ptr::hash(self, state);
        }
    }
}

/// A weak reference to a [`Gc`].
///
/// This type allows keeping references to [`Gc`] managed values without keeping them alive for
/// garbage collections. However, this also means [`WeakGcEdge::upgrade`] could return `None` at any moment.
#[derive(Debug, Trace, Finalize)]
#[repr(transparent)]
pub struct WeakGcEdge<T: Trace + ?Sized + 'static> {
    inner: EphemeronEdge<T, ()>,
}

/// A temporary external root for the ephemeron backing a [`WeakGcEdge`].
#[derive(Debug)]
pub struct WeakGcRoot<T: Trace + ?Sized + 'static> {
    #[allow(dead_code)]
    inner: Ephemeron<T, ()>,
}

impl<T: Trace + ?Sized> WeakGcEdge<T> {
    /// Registers the backing ephemeron while an external caller is assembling a heap object
    /// that will take ownership of this edge.
    #[must_use]
    pub fn root(&self) -> WeakGcRoot<T> {
        WeakGcRoot {
            inner: Ephemeron::from_edge(self.inner.clone()),
        }
    }

    /// Creates a new weak pointer for a garbage collected value.
    #[inline]
    #[must_use]
    pub(crate) fn new_gc(value: &Gc<T>) -> Self {
        Self {
            inner: EphemeronEdge::new_gc(value, ()),
        }
    }

    /// Creates a new weak pointer from a heap edge.
    #[inline]
    #[must_use]
    pub fn new_edge(value: &GcEdge<T>) -> Self {
        Self::new_gc(value.as_gc())
    }

    /// Creates a new weak heap edge from an explicitly rooted value.
    #[inline]
    #[must_use]
    pub fn new_rooted(value: &Rooted<T>) -> Self {
        Self::new_gc(value.as_gc())
    }

    /// Retargets this weak edge to another heap allocation without allocating a
    /// new ephemeron.
    ///
    /// The ephemeron is added to the remembered set because either its key or
    /// the ephemeron itself may already be old while the new key is young.
    pub fn retarget_edge(&mut self, value: &GcEdge<T>) {
        // SAFETY: this handle uniquely mutates the ephemeron's data on the
        // collector thread, outside collection. Its allocation remains live
        // through `self`, and the remembered set covers the new weak edge.
        unsafe { self.inner.inner_ptr().as_ref().set(value.as_gc(), ()) };
        remember_ephemeron_pointer(self.inner.erased_inner_ptr());
    }

    /// Upgrade returns a `Gc` pointer for the internal value if the pointer is still live, or `None`
    /// if the value was already garbage collected.
    #[inline]
    #[must_use]
    pub fn upgrade(&self) -> Option<GcEdge<T>> {
        self.inner.key()
    }

    /// Upgrades this weak pointer into an unregistered heap edge.
    #[inline]
    #[must_use]
    pub fn upgrade_edge(&self) -> Option<GcEdge<T>> {
        self.upgrade()
    }

    /// Upgrades this weak pointer into an explicitly registered root.
    #[inline]
    #[must_use]
    pub fn upgrade_rooted(&self) -> Option<Rooted<T>> {
        self.upgrade().map(GcEdge::root)
    }

    /// Check if the [`WeakGcEdge`] can be upgraded.
    #[inline]
    #[must_use]
    pub fn is_upgradable(&self) -> bool {
        self.inner.has_value()
    }

    #[must_use]
    pub(crate) const fn inner(&self) -> &EphemeronEdge<T, ()> {
        &self.inner
    }
}

impl<T: Trace> Clone for WeakGcEdge<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: Trace> From<EphemeronEdge<T, ()>> for WeakGcEdge<T> {
    fn from(inner: EphemeronEdge<T, ()>) -> Self {
        Self { inner }
    }
}

impl<T: Trace> PartialEq for WeakGcEdge<T> {
    fn eq(&self, other: &Self) -> bool {
        match (self.upgrade(), other.upgrade()) {
            (Some(a), Some(b)) => GcEdge::ptr_eq(&a, &b),
            _ => false,
        }
    }
}

impl<T: Trace> Eq for WeakGcEdge<T> {}

impl<T: Trace> Hash for WeakGcEdge<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        if let Some(obj) = self.upgrade() {
            ptr::hash(obj.as_gc().as_ref(), state);
        } else {
            ptr::hash(self, state);
        }
    }
}
