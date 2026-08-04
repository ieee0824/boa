use super::{Gc, Rooted};
use crate::{Finalize, Trace, Tracer};
use std::{fmt, ops::Deref};

/// A garbage-collected pointer stored as an edge inside the traced heap.
///
/// `GcEdge<T>` is deliberately not registered in the explicit root set. Native
/// code that needs to keep the allocation alive across a collection must call
/// [`Self::root`] and retain the returned [`Rooted<T>`].
pub struct GcEdge<T: Trace + ?Sized + 'static> {
    inner: Gc<T>,
}

impl<T: Trace> GcEdge<T> {
    /// Allocates a garbage-collected value as an unregistered heap edge.
    #[must_use]
    pub fn new(value: T) -> Self {
        Self::from_gc(Gc::new(value))
    }
}

impl<T: Trace + ?Sized> GcEdge<T> {
    pub(super) fn from_gc(inner: Gc<T>) -> Self {
        Self { inner }
    }

    /// Consumes the edge and returns its allocation pointer.
    #[must_use]
    pub fn into_raw(this: Self) -> std::ptr::NonNull<crate::GcBox<T>> {
        Gc::into_raw(this.inner)
    }

    /// Reconstructs an edge previously consumed by [`Self::into_raw`].
    ///
    /// # Safety
    /// `inner` must have been produced by `GcEdge::into_raw` for a compatible type.
    #[must_use]
    pub const unsafe fn from_raw(inner: std::ptr::NonNull<crate::GcBox<T>>) -> Self {
        Self {
            // SAFETY: Forwarded from this function's contract.
            inner: unsafe { Gc::from_raw(inner) },
        }
    }

    /// Returns true when two edges point to the same allocation.
    #[must_use]
    pub fn ptr_eq<U: Trace + ?Sized>(this: &Self, other: &GcEdge<U>) -> bool {
        Gc::ptr_eq(&this.inner, &other.inner)
    }

    /// Returns true when the allocation contains a value of type `U`.
    #[must_use]
    pub fn is<U: Trace + 'static>(this: &Self) -> bool {
        Gc::is::<U>(&this.inner)
    }

    /// Reinterprets an edge as another allocation type.
    ///
    /// # Safety
    /// The caller must ensure the cast is valid.
    #[must_use]
    pub unsafe fn cast_unchecked<U: Trace + 'static>(this: Self) -> GcEdge<U> {
        GcEdge {
            // SAFETY: Forwarded from this function's contract.
            inner: unsafe { Gc::cast_unchecked::<U>(this.inner) },
        }
    }

    /// Converts this heap edge into a registered external root.
    #[must_use]
    pub fn root(self) -> Rooted<T> {
        Rooted::from_gc(self.inner)
    }

    /// Borrows the legacy pointer used while the collector migration is active.
    #[must_use]
    pub fn as_gc(&self) -> &Gc<T> {
        &self.inner
    }
}

impl<T: Trace + ?Sized> From<Gc<T>> for GcEdge<T> {
    fn from(inner: Gc<T>) -> Self {
        Self::from_gc(inner)
    }
}

impl<T: Trace + ?Sized> Clone for GcEdge<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: Trace + ?Sized> Deref for GcEdge<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T: Trace + ?Sized + fmt::Debug> fmt::Debug for GcEdge<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("GcEdge").field(&self.inner).finish()
    }
}

impl<T: Trace + ?Sized> Finalize for GcEdge<T> {}

// SAFETY: The edge owns a valid Gc handle and delegates tracing to it.
unsafe impl<T: Trace + ?Sized> Trace for GcEdge<T> {
    unsafe fn trace(&self, tracer: &mut Tracer) {
        unsafe { self.inner.trace(tracer) };
    }

    fn run_finalizer(&self) {
        self.inner.run_finalizer();
    }
}
