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

impl<T: Trace + ?Sized> GcEdge<T> {
    pub(super) fn from_gc(inner: Gc<T>) -> Self {
        Self { inner }
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

// SAFETY: The compatibility edge owns a valid Gc handle and delegates all
// tracing operations to it until the inferred-root collector is removed.
unsafe impl<T: Trace + ?Sized> Trace for GcEdge<T> {
    unsafe fn trace(&self, tracer: &mut Tracer) {
        unsafe { self.inner.trace(tracer) };
    }

    unsafe fn trace_non_roots(&self) {
        unsafe { self.inner.trace_non_roots() };
    }

    fn run_finalizer(&self) {
        self.inner.run_finalizer();
    }
}
