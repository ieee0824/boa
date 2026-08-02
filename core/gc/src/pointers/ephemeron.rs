#![allow(clippy::doc_link_with_quotes)]

use crate::{
    Allocator, EphemeronPointer, Gc, GcEdge, Rooted, Tracer, finalizer_safe,
    internals::EphemeronBox,
    register_ephemeron_root,
    trace::{Finalize, Trace},
    unregister_ephemeron_root,
};
use std::{mem::ManuallyDrop, ptr, ptr::NonNull};

/// An explicitly registered external owner of an ephemeron allocation.
#[derive(Debug)]
pub struct Ephemeron<K: Trace + ?Sized + 'static, V: Trace + 'static> {
    inner: EphemeronEdge<K, V>,
}

impl<K: Trace + ?Sized, V: Trace + Clone> Ephemeron<K, V> {
    /// Gets the stored value, or `None` if the key was collected.
    #[must_use]
    pub fn value(&self) -> Option<V> {
        self.inner.value()
    }

    /// Gets the stored key as an explicitly registered root.
    #[must_use]
    pub fn key(&self) -> Option<Rooted<K>> {
        self.inner.key().map(GcEdge::root)
    }

    /// Returns whether this ephemeron still has a live value.
    #[must_use]
    pub fn has_value(&self) -> bool {
        self.inner.has_value()
    }
}

impl<K: Trace + ?Sized, V: Trace> Ephemeron<K, V> {
    /// Creates and registers an externally owned ephemeron.
    #[must_use]
    pub fn new(key: &Rooted<K>, value: V) -> Self {
        Self::from_edge(EphemeronEdge::new_gc(key.as_gc(), value))
    }

    /// Promotes a heap edge into an explicitly registered ephemeron root.
    #[must_use]
    pub fn from_edge(inner: EphemeronEdge<K, V>) -> Self {
        register_ephemeron_root(inner.erased_inner_ptr());
        Self { inner }
    }

    /// Converts this root into an unregistered heap edge.
    #[must_use]
    pub fn into_edge(self) -> EphemeronEdge<K, V> {
        let this = ManuallyDrop::new(self);
        unregister_ephemeron_root(this.inner.erased_inner_ptr());
        // SAFETY: `this` cannot run `Drop`, and the field is moved exactly once.
        unsafe { ptr::read(&raw const this.inner) }
    }

    /// Returns whether two roots refer to the same ephemeron allocation.
    #[must_use]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        EphemeronEdge::ptr_eq(&this.inner, &other.inner)
    }
}

impl<K: Trace + ?Sized, V: Trace> Clone for Ephemeron<K, V> {
    fn clone(&self) -> Self {
        Self::from_edge(self.inner.clone())
    }
}

impl<K: Trace + ?Sized, V: Trace> Drop for Ephemeron<K, V> {
    fn drop(&mut self) {
        unregister_ephemeron_root(self.inner.erased_inner_ptr());
    }
}

impl<K: Trace + ?Sized, V: Trace> Finalize for Ephemeron<K, V> {}

unsafe impl<K: Trace + ?Sized, V: Trace> Trace for Ephemeron<K, V> {
    unsafe fn trace(&self, tracer: &mut Tracer) {
        // SAFETY: Delegated to the valid inner edge.
        unsafe { self.inner.trace(tracer) };
    }

    fn run_finalizer(&self) {
        self.inner.run_finalizer();
    }
}

/// A key-value pair where the value becomes unaccesible when the key is garbage collected.
///
/// You can read more about ephemerons on:
/// - Racket's page about [**ephemerons**][eph], which gives a brief overview.
/// - Barry Hayes' paper ["_Ephemerons_: a new finalization mechanism"][acm] which explains the topic
///   in full detail.
///
///
/// [eph]: https://docs.racket-lang.org/reference/ephemerons.html
/// [acm]: https://dl.acm.org/doi/10.1145/263700.263733
#[derive(Debug)]
pub struct EphemeronEdge<K: Trace + ?Sized + 'static, V: Trace + 'static> {
    inner_ptr: NonNull<EphemeronBox<K, V>>,
}

impl<K: Trace + ?Sized, V: Trace + Clone> EphemeronEdge<K, V> {
    /// Gets the stored value of this `EphemeronEdge`, or `None` if the key was already garbage collected.
    ///
    /// This needs to return a clone of the value because holding a reference to it between
    /// garbage collection passes could drop the underlying allocation, causing an Use After Free.
    #[must_use]
    pub fn value(&self) -> Option<V> {
        // SAFETY: this is safe because `EphemeronEdge` is tracked to always point to a valid pointer
        // `inner_ptr`.
        // SAFETY: the pointer stays valid while this handle is live.
        let inner = unsafe { self.inner_ptr.as_ref() };
        // SAFETY: forwarded from this function's existing contract.
        unsafe { inner.value() }.cloned()
    }

    /// Gets the stored key of this `EphemeronEdge`, or `None` if the key was already garbage collected.
    #[inline]
    #[must_use]
    pub fn key(&self) -> Option<GcEdge<K>> {
        // SAFETY: this is safe because `EphemeronEdge` is tracked to always point to a valid pointer
        // `inner_ptr`.
        // SAFETY: the pointer stays valid while this handle is live.
        let inner = unsafe { self.inner_ptr.as_ref() };
        // SAFETY: the pointer stays valid while this handle is live.
        let key_ptr = unsafe { inner.key_ptr() }?;

        // SAFETY: the key remains owned by the collector while this ephemeron is live.
        Some(GcEdge::from(unsafe { Gc::from_raw(key_ptr) }))
    }

    /// Checks if the [`EphemeronEdge`] has a value.
    #[must_use]
    pub fn has_value(&self) -> bool {
        // SAFETY: this is safe because `EphemeronEdge` is tracked to always point to a valid pointer
        // `inner_ptr`.
        // SAFETY: the pointer stays valid while this handle is live.
        let inner = unsafe { self.inner_ptr.as_ref() };
        // SAFETY: forwarded from this function's existing contract.
        unsafe { inner.value() }.is_some()
    }
}

impl<K: Trace + ?Sized, V: Trace> EphemeronEdge<K, V> {
    /// Creates a new `EphemeronEdge`.
    #[must_use]
    pub(crate) fn new_gc(key: &Gc<K>, value: V) -> Self {
        let inner_ptr = Allocator::alloc_ephemeron(EphemeronBox::new(key, value));
        Self { inner_ptr }
    }

    /// Creates an ephemeron edge from a heap key edge.
    #[must_use]
    pub fn new(key: &GcEdge<K>, value: V) -> Self {
        Self::new_gc(key.as_gc(), value)
    }

    /// Returns `true` if the two `EphemeronEdge`s point to the same allocation.
    #[must_use]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        ptr::addr_eq(this.inner(), other.inner())
    }

    pub(crate) fn inner_ptr(&self) -> NonNull<EphemeronBox<K, V>> {
        assert!(finalizer_safe());
        self.inner_ptr
    }

    pub(crate) fn erased_inner_ptr(&self) -> EphemeronPointer {
        self.inner_ptr
    }

    pub(crate) fn inner(&self) -> &EphemeronBox<K, V> {
        // SAFETY: Please see Gc::inner_ptr()
        unsafe { self.inner_ptr().as_ref() }
    }

    /// Constructs an `EphemeronEdge<K, V>` from a raw pointer.
    ///
    /// # Safety
    ///
    /// This function is unsafe because improper use may lead to memory corruption, double-free,
    /// or misbehaviour of the garbage collector.
    #[must_use]
    pub(crate) const unsafe fn from_raw(inner_ptr: NonNull<EphemeronBox<K, V>>) -> Self {
        Self { inner_ptr }
    }
}

impl<K: Trace + ?Sized, V: Trace> Finalize for EphemeronEdge<K, V> {}

// SAFETY: `EphemeronEdge`s trace implementation only queues its inner box because we want to stop
// tracing through weakly held pointers until the collector has checked the key.
unsafe impl<K: Trace + ?Sized, V: Trace> Trace for EphemeronEdge<K, V> {
    unsafe fn trace(&self, tracer: &mut Tracer) {
        tracer.enqueue_ephemeron(self.inner_ptr);
    }

    fn run_finalizer(&self) {
        Finalize::finalize(self);
    }
}

impl<K: Trace + ?Sized, V: Trace> Clone for EphemeronEdge<K, V> {
    fn clone(&self) -> Self {
        let ptr = self.inner_ptr();
        // SAFETY: `&self` is a valid EphemeronEdge pointer.
        unsafe { Self::from_raw(ptr) }
    }
}

impl<K: Trace + ?Sized, V: Trace> Drop for EphemeronEdge<K, V> {
    fn drop(&mut self) {
        if finalizer_safe() {
            Finalize::finalize(self);
        }
    }
}
