use super::Gc;
use crate::{register_root, unregister_root, Finalize, Trace};
use std::{fmt, ops::Deref};

/// An explicitly registered, heap-external owner of a garbage-collected value.
///
/// During migration this also retains a `Gc<T>`, so collection semantics remain
/// unchanged until all heap edges use a distinct unrooted type.
pub struct Rooted<T: Trace + ?Sized + 'static> {
    inner: Gc<T>,
}

impl<T: Trace> Rooted<T> {
    /// Allocates a value and registers its handle as an explicit root.
    #[must_use]
    pub fn new(value: T) -> Self {
        Self::from_gc(Gc::new(value))
    }
}

impl<T: Trace + ?Sized> Rooted<T> {
    /// Promotes a legacy `Gc<T>` handle to an explicitly registered root.
    #[must_use]
    pub fn from_gc(inner: Gc<T>) -> Self {
        register_root(inner.as_erased_pointer());
        Self { inner }
    }

    #[must_use]
    /// Borrows the compatibility `Gc<T>` handle used during migration.
    pub fn as_gc(&self) -> &Gc<T> {
        &self.inner
    }
}

impl<T: Trace + ?Sized> Clone for Rooted<T> {
    fn clone(&self) -> Self {
        Self::from_gc(self.inner.clone())
    }
}

impl<T: Trace + ?Sized> Drop for Rooted<T> {
    fn drop(&mut self) {
        unregister_root(self.inner.as_erased_pointer());
    }
}

impl<T: Trace + ?Sized> Deref for Rooted<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T: Trace + ?Sized + fmt::Debug> fmt::Debug for Rooted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Rooted").field(&self.inner).finish()
    }
}

impl<T: Trace + ?Sized> Finalize for Rooted<T> {}

// SAFETY: This compatibility implementation delegates to the valid Gc handle.
unsafe impl<T: Trace + ?Sized> Trace for Rooted<T> {
    unsafe fn trace(&self, tracer: &mut crate::Tracer) {
        unsafe { self.inner.trace(tracer) };
    }

    unsafe fn trace_non_roots(&self) {
        unsafe { self.inner.trace_non_roots() };
    }

    fn run_finalizer(&self) {
        self.inner.run_finalizer();
    }
}
