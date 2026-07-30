use super::{GcEdge, Rooted, WeakGcEdge};
use crate::{
    Allocator, EphemeronEdge, GcErasedPointer, Tracer, custom_trace, finalizer_safe,
    internals::{EphemeronBox, GcBox, VTable},
    trace::{Finalize, Trace},
};
use std::{
    any::TypeId,
    cmp::Ordering,
    fmt::{self, Debug, Display},
    hash::{Hash, Hasher},
    marker::PhantomData,
    ops::Deref,
    ptr::NonNull,
    rc::Rc,
};

/// Zero sized struct that is used to ensure that we do not call trace methods,
/// call its finalization method or drop it.
///
/// This can only happen if we are accessing a [`GcErasedPointer`] directly which is a bug.
/// Panics if any of it's methods are called.
///
/// Note: Accessing the [`crate::internals::GcHeader`] of [`GcErasedPointer`] is fine.
pub(crate) struct NonTraceable(());

impl Finalize for NonTraceable {
    fn finalize(&self) {
        unreachable!()
    }
}

unsafe impl Trace for NonTraceable {
    unsafe fn trace(&self, _tracer: &mut Tracer) {
        unreachable!()
    }
    fn run_finalizer(&self) {
        unreachable!()
    }
}

impl Drop for NonTraceable {
    fn drop(&mut self) {
        unreachable!()
    }
}

/// A type-erased, explicitly registered GC root.
#[repr(transparent)]
pub struct GcErased {
    inner: Rooted<NonTraceable>,
}

impl GcErased {
    /// Converts an explicitly rooted handle into a type-erased root.
    #[inline]
    #[must_use]
    pub fn new<T: Trace>(root: Rooted<T>) -> Self {
        let inner_ptr = Rooted::into_raw(root).cast();
        // SAFETY: Type erasure preserves the allocation and its vtable. The raw
        // pointer came from `Rooted::into_raw` and is reconstructed exactly once.
        Self {
            inner: unsafe { Rooted::from_raw(inner_ptr) },
        }
    }

    /// Returns `true` if the two [`GcErased`]s point to the same allocation.
    #[must_use]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        Gc::ptr_eq(this.inner.as_gc(), other.inner.as_gc())
    }

    /// Returns the [`TypeId`] of the inner type.
    #[inline]
    #[must_use]
    pub fn type_id(&self) -> TypeId {
        Gc::type_id(self.inner.as_gc())
    }

    /// Returns true if the inner type is the same as `T`.
    #[inline]
    #[must_use]
    pub fn is<T: Trace + 'static>(&self) -> bool {
        Gc::is::<T>(self.inner.as_gc())
    }

    /// Converts this external root into an unregistered type-erased heap edge.
    #[must_use]
    pub fn into_edge(self) -> GcErasedEdge {
        GcErasedEdge {
            inner: self.inner.into_edge(),
        }
    }

    /// Returns a typed root if the allocation contains `T`.
    #[inline]
    #[must_use]
    pub fn downcast<T: Trace + 'static>(self) -> Option<Rooted<T>> {
        if !self.is::<T>() {
            return None;
        }
        // SAFETY: The type id was checked above, and the raw pointer is
        // reconstructed exactly once after consuming the erased root.
        Some(unsafe { self.downcast_unchecked::<T>() })
    }

    /// Downcast the inner value of type `T`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the cast is valid.
    #[inline]
    #[must_use]
    pub unsafe fn downcast_unchecked<T: Trace + 'static>(self) -> Rooted<T> {
        let inner_ptr = Rooted::into_raw(self.inner).cast();
        // SAFETY: Forwarded from this function's contract.
        unsafe { Rooted::from_raw(inner_ptr) }
    }

    /// Returns reference to the inner value of type `T`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the cast is valid.
    #[inline]
    #[must_use]
    pub unsafe fn downcast_ref_unchecked<T: Trace + 'static>(&self) -> &Gc<T> {
        // SAFETY: It's the callers responsibility to make sure this is valid.
        unsafe { Gc::cast_ref_unchecked::<T>(self.inner.as_gc()) }
    }
}

impl Debug for GcErased {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcErased")
            .field("inner", &self.inner.as_gc().inner_ptr)
            .finish()
    }
}

/// A type-erased GC edge stored inside traced heap data.
#[repr(transparent)]
pub struct GcErasedEdge {
    inner: GcEdge<NonTraceable>,
}

impl GcErasedEdge {
    /// Converts a typed heap edge into a type-erased edge.
    #[inline]
    #[must_use]
    pub fn new<T: Trace>(edge: GcEdge<T>) -> Self {
        let inner_ptr = GcEdge::into_raw(edge).cast();
        // SAFETY: Type erasure preserves the allocation and its vtable. The raw
        // pointer came from `GcEdge::into_raw` and is reconstructed exactly once.
        Self {
            inner: unsafe { GcEdge::from_raw(inner_ptr) },
        }
    }

    /// Returns `true` if two erased edges point to the same allocation.
    #[must_use]
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        Gc::ptr_eq(this.inner.as_gc(), other.inner.as_gc())
    }

    /// Returns the allocation's concrete type id.
    #[must_use]
    pub fn type_id(&self) -> TypeId {
        Gc::type_id(self.inner.as_gc())
    }

    /// Returns whether the allocation contains `T`.
    #[must_use]
    pub fn is<T: Trace + 'static>(&self) -> bool {
        Gc::is::<T>(self.inner.as_gc())
    }

    /// Promotes this edge into an explicitly registered type-erased root.
    #[must_use]
    pub fn root(self) -> GcErased {
        GcErased {
            inner: self.inner.root(),
        }
    }

    /// Returns a typed edge if the allocation contains `T`.
    #[must_use]
    pub fn downcast<T: Trace + 'static>(self) -> Option<GcEdge<T>> {
        if !self.is::<T>() {
            return None;
        }
        // SAFETY: The type id was checked above.
        Some(unsafe { self.downcast_unchecked::<T>() })
    }

    /// Downcasts this edge without checking its concrete type.
    ///
    /// # Safety
    /// The caller must ensure the allocation contains `T`.
    #[must_use]
    pub unsafe fn downcast_unchecked<T: Trace + 'static>(self) -> GcEdge<T> {
        let inner_ptr = GcEdge::into_raw(self.inner).cast();
        // SAFETY: Forwarded from this function's contract.
        unsafe { GcEdge::from_raw(inner_ptr) }
    }
}

impl Debug for GcErasedEdge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcErasedEdge")
            .field("inner", &self.inner.as_gc().inner_ptr)
            .finish()
    }
}

impl Finalize for GcErasedEdge {}

unsafe impl Trace for GcErasedEdge {
    custom_trace!(this, mark, {
        mark(&this.inner);
    });
}

impl Clone for GcErasedEdge {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl Finalize for GcErased {
    fn finalize(&self) {}
}

impl Clone for GcErased {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

/// A garbage-collected pointer type over an immutable value.
pub struct Gc<T: Trace + ?Sized + 'static> {
    pub(crate) inner_ptr: NonNull<GcBox<T>>,
    pub(crate) marker: PhantomData<Rc<T>>,
}

impl<T: Trace + ?Sized> Gc<T> {
    /// Constructs a new `Gc<T>` with the given value.
    #[must_use]
    pub fn new(value: T) -> Self
    where
        T: Sized,
    {
        // Create GcBox and allocate it to heap.
        //
        // Note: Allocator can cause Collector to run
        let inner_ptr = Allocator::alloc_gc(GcBox::new(value));

        Self {
            inner_ptr,
            marker: PhantomData,
        }
    }

    /// Constructs a new `Gc<T>` while giving you a `WeakGcEdge<T>` to the allocation, to allow
    /// constructing a T which holds a weak pointer to itself.
    ///
    /// Since the new `Gc<T>` is not fully-constructed until `Gc<T>::new_cyclic` returns, calling
    /// [`upgrade`][WeakGcEdge::upgrade] on the weak reference inside the closure will fail and result
    /// in a `None` value.
    #[must_use]
    pub fn new_cyclic<F>(data_fn: F) -> Self
    where
        F: FnOnce(&WeakGcEdge<T>) -> T,
        T: Sized,
    {
        // SAFETY: The newly allocated ephemeron is only live here, meaning `Ephemeron` is the
        // sole owner of the allocation after passing it to `from_raw`, making this operation safe.
        let weak = unsafe {
            EphemeronEdge::from_raw(Allocator::alloc_ephemeron(EphemeronBox::new_empty())).into()
        };

        let gc = Self::new(data_fn(&weak));

        // SAFETY:
        // - `as_mut`: `weak` is properly initialized by `alloc_ephemeron` and cannot escape the
        //   `unsafe` block.
        // - `set_kv`: `weak` is a newly created `EphemeronBox`, meaning it isn't possible to
        //   collect it since `weak` is still live.
        unsafe { weak.inner().inner_ptr().as_mut().set(&gc, ()) }

        gc
    }

    /// Consumes the `Gc`, returning a wrapped raw pointer.
    ///
    /// To avoid a memory leak, the pointer must be converted back to a `Gc` using [`Gc::from_raw`].
    #[must_use]
    pub fn into_raw(this: Self) -> NonNull<GcBox<T>> {
        let ptr = this.inner_ptr();
        std::mem::forget(this);
        ptr
    }

    /// Returns `true` if the two `Gc`s point to the same allocation.
    #[must_use]
    pub fn ptr_eq<U: Trace + ?Sized>(this: &Self, other: &Gc<U>) -> bool {
        std::ptr::addr_eq(this.inner(), other.inner())
    }

    /// Constructs a `Gc<T>` from a raw pointer.
    ///
    /// The raw pointer must have been returned by a previous call to [`Gc<U>::into_raw`][Gc::into_raw]
    /// where `U` must have the same size and alignment as `T`.
    ///
    /// # Safety
    ///
    /// This function is unsafe because improper use may lead to memory corruption, double-free,
    /// or misbehaviour of the garbage collector.
    #[must_use]
    pub const unsafe fn from_raw(inner_ptr: NonNull<GcBox<T>>) -> Self {
        Self {
            inner_ptr,
            marker: PhantomData,
        }
    }

    pub(crate) fn as_erased_pointer(&self) -> GcErasedPointer {
        self.inner_ptr.cast()
    }

    /// Return the [`TypeId`] of the `T`.
    #[inline]
    #[must_use]
    pub fn type_id(this: &Self) -> TypeId {
        this.vtable().type_id()
    }

    /// Returns true if the inner type is the same as `T`.
    #[inline]
    #[must_use]
    pub fn is<U: Trace + 'static>(this: &Self) -> bool {
        Gc::type_id(this) == TypeId::of::<U>()
    }

    /// Returns [`Some`] reference to the inner value if it is of type `T`, or [`None`] if it isn’t.
    #[inline]
    #[must_use]
    pub fn downcast<U: Trace + 'static>(this: Self) -> Option<Gc<U>> {
        if !Gc::is::<U>(&this) {
            return None;
        }

        // SAFETY: We check that the type is correct above, so this is safe.
        Some(unsafe { Gc::cast_unchecked::<U>(this) })
    }

    /// Returns reference to the inner value of type `T`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the cast is valid.
    #[inline]
    #[must_use]
    pub unsafe fn cast_unchecked<U: Trace + 'static>(this: Self) -> Gc<U> {
        let inner_ptr = this.inner_ptr.cast::<U>();
        core::mem::forget(this); // Prevents double free.
        Gc {
            inner_ptr: inner_ptr.cast(),
            marker: PhantomData,
        }
    }

    /// Returns reference to the inner value of type `T`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the cast is valid.
    #[inline]
    #[must_use]
    pub unsafe fn cast_ref_unchecked<U: Trace + 'static>(this: &Self) -> &Gc<U> {
        // SAFETY: Casting a Gc<T> to a Gc<U> of any type is safe, as long as you don’t actually access it as a U.
        //         The correct functions for T will still be called during tracing, finalization, and dropping.
        unsafe { &(*(&raw const *this).cast::<Gc<U>>()) }
    }
}

impl<T: Trace + ?Sized> Gc<T> {
    pub(crate) fn vtable(&self) -> &'static VTable {
        // SAFETY: The inner pointer is valid at all times.
        unsafe { self.inner_ptr.as_ref() }.vtable
    }

    #[inline(always)]
    #[allow(clippy::inline_always)]
    pub(crate) fn inner_ptr(&self) -> NonNull<GcBox<T>> {
        debug_assert!(finalizer_safe());
        self.inner_ptr
    }

    fn inner(&self) -> &GcBox<T> {
        // SAFETY: Please see Gc::inner_ptr()
        unsafe { self.inner_ptr().as_ref() }
    }
}

impl<T: Trace + ?Sized> Finalize for Gc<T> {}

// SAFETY: `Gc` maintains it's own rootedness and implements all methods of
// Trace. It is not possible to root an already rooted `Gc` and vice versa.
unsafe impl<T: Trace + ?Sized> Trace for Gc<T> {
    unsafe fn trace(&self, tracer: &mut Tracer) {
        tracer.enqueue(self.as_erased_pointer());
    }

    fn run_finalizer(&self) {
        Finalize::finalize(self);
    }
}

impl<T: Trace + ?Sized> Clone for Gc<T> {
    fn clone(&self) -> Self {
        let ptr = self.inner_ptr();
        // SAFETY: the collector owns the allocation and `self` proves the pointer is valid.
        unsafe { Self::from_raw(ptr) }
    }
}

impl<T: Trace + ?Sized> Deref for Gc<T> {
    type Target = T;

    fn deref(&self) -> &T {
        let inner = self.inner();
        // TEMPORARY #330 DIAGNOSTIC — remove before merging.
        if crate::diagnose_roots() {
            assert!(
                !inner.header.is_poisoned(),
                "#330: dereferenced a `{}` the collector already reclaimed — its holder is \
                 not registered as a GC root",
                inner.type_name()
            );
        }
        inner.value()
    }
}

impl<T: Trace + ?Sized> Drop for Gc<T> {
    fn drop(&mut self) {
        if finalizer_safe() {
            Finalize::finalize(self);
        }
    }
}

impl<T: Trace + Default> Default for Gc<T> {
    fn default() -> Self {
        Self::new(Default::default())
    }
}

#[allow(clippy::inline_always)]
impl<T: Trace + ?Sized + PartialEq> PartialEq for Gc<T> {
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl<T: Trace + ?Sized + Eq> Eq for Gc<T> {}

#[allow(clippy::inline_always)]
impl<T: Trace + ?Sized + PartialOrd> PartialOrd for Gc<T> {
    #[inline(always)]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        (**self).partial_cmp(&**other)
    }

    #[inline(always)]
    fn lt(&self, other: &Self) -> bool {
        **self < **other
    }

    #[inline(always)]
    fn le(&self, other: &Self) -> bool {
        **self <= **other
    }

    #[inline(always)]
    fn gt(&self, other: &Self) -> bool {
        **self > **other
    }

    #[inline(always)]
    fn ge(&self, other: &Self) -> bool {
        **self >= **other
    }
}

impl<T: Trace + ?Sized + Ord> Ord for Gc<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        (**self).cmp(&**other)
    }
}

impl<T: Trace + ?Sized + Hash> Hash for Gc<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

impl<T: Trace + ?Sized + Display> Display for Gc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(&**self, f)
    }
}

impl<T: Trace + ?Sized + Debug> Debug for Gc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Debug::fmt(&**self, f)
    }
}

impl<T: Trace + ?Sized> fmt::Pointer for Gc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Pointer::fmt(&self.inner(), f)
    }
}

impl<T: Trace + ?Sized> std::borrow::Borrow<T> for Gc<T> {
    fn borrow(&self) -> &T {
        self
    }
}

impl<T: Trace + ?Sized> AsRef<T> for Gc<T> {
    fn as_ref(&self) -> &T {
        self
    }
}
