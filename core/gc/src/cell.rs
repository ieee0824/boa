//! A garbage collected cell implementation

use crate::{
    GcErasedPointer, Tracer, begin_collection_block, end_collection_block, remember_old_parent,
    trace::{Finalize, Trace},
};
use std::{
    cell::{Cell, UnsafeCell},
    cmp::Ordering,
    fmt::{self, Debug, Display},
    hash::Hash,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    ptr::{self, NonNull},
};

/// `BorrowFlag` represent the internal state of a `GcCell` and
/// keeps track of the number of current borrows.
#[derive(Copy, Clone)]
struct BorrowFlag(usize);

/// `BorrowState` represents the various states of a `BorrowFlag`
///
///  - Reading: the value is currently being read/borrowed.
///  - Writing: the value is currently being written/borrowed mutably.
///  - Unused: the value is currently unrooted.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum BorrowState {
    Reading,
    Writing,
    Unused,
}

const WRITING: usize = !0;
const UNUSED: usize = 0;

/// The base borrow flag init is rooted, and has no outstanding borrows.
const BORROWFLAG_INIT: BorrowFlag = BorrowFlag(UNUSED);

impl BorrowFlag {
    /// Check the current `BorrowState` of `BorrowFlag`.
    const fn borrowed(self) -> BorrowState {
        match self.0 {
            UNUSED => BorrowState::Unused,
            WRITING => BorrowState::Writing,
            _ => BorrowState::Reading,
        }
    }

    /// Set the `BorrowFlag`'s state to writing.
    const fn set_writing(self) -> Self {
        Self(self.0 | WRITING)
    }

    /// Increments the counter for a new borrow.
    ///
    /// # Panic
    ///  - This method will panic if the current `BorrowState` is writing.
    ///  - This method will panic after incrementing if the borrow count overflows.
    #[inline]
    fn add_reading(self) -> Self {
        assert_ne!(self.borrowed(), BorrowState::Writing);
        let flags = Self(self.0 + 1);

        // This will fail if the borrow count overflows, which shouldn't happen,
        // but let's be safe
        {
            assert_eq!(flags.borrowed(), BorrowState::Reading);
        }
        flags
    }

    /// Decrements the counter to remove a borrow.
    ///
    /// # Panic
    ///  - This method will panic if the current `BorrowState` is not reading.
    fn sub_reading(self) -> Self {
        assert_eq!(self.borrowed(), BorrowState::Reading);
        Self(self.0 - 1)
    }
}

impl Debug for BorrowFlag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BorrowFlag")
            .field("State", &self.borrowed())
            .finish()
    }
}

/// A mutable memory location with dynamically checked borrow rules
/// that can be used inside of a garbage-collected pointer.
///
/// This object is a `RefCell` that can be used inside of a `Gc<T>`.
pub struct GcRefCell<T: ?Sized + 'static> {
    borrow: Cell<BorrowFlag>,
    barrier: BarrierState,
    cell: UnsafeCell<T>,
}

type BarrierFn = unsafe fn(*const ());

struct BarrierState {
    // Set when the cell is traced as part of an old allocation. Young parents
    // do not need a write barrier, and promotion traces them once to install it.
    // The state itself is sized even when T is a DST, which lets the
    // mutable-borrow guard erase its pointer without losing metadata.
    callback: Cell<Option<BarrierFn>>,
    owner: Cell<Option<GcErasedPointer>>,
    // Avoid hashing the same old parent on every write between collections. A
    // minor collection resets this while consuming the remembered parent, so a
    // later write can enqueue it for the next nursery pass.
    dirty: Cell<bool>,
}

impl<T> GcRefCell<T> {
    /// Creates a new `GcCell` containing `value`.
    pub const fn new(value: T) -> Self {
        Self {
            borrow: Cell::new(BORROWFLAG_INIT),
            barrier: BarrierState {
                callback: Cell::new(None),
                owner: Cell::new(None),
                dirty: Cell::new(false),
            },
            cell: UnsafeCell::new(value),
        }
    }

    /// Consumes the `GcCell`, returning the wrapped value.
    pub fn into_inner(self) -> T {
        self.cell.into_inner()
    }
}

impl<T: ?Sized> GcRefCell<T> {
    /// Immutably borrows the wrapped value.
    ///
    /// The borrow lasts until the returned `GcCellRef` exits scope.
    /// Multiple immutable borrows can be taken out at the same time.
    ///
    /// # Panics
    ///
    /// Panics if the value is currently mutably borrowed.
    pub fn borrow(&self) -> GcRef<'_, T> {
        match self.try_borrow() {
            Ok(value) => value,
            Err(e) => panic!("{}", e),
        }
    }

    /// Mutably borrows the wrapped value.
    ///
    /// The borrow lasts until the returned `GcCellRefMut` exits scope.
    /// The value cannot be borrowed while this borrow is active.
    ///
    /// # Panics
    ///
    /// Panics if the value is currently borrowed.
    #[track_caller]
    pub fn borrow_mut(&self) -> GcRefMut<'_, T> {
        match self.try_borrow_mut() {
            Ok(value) => value,
            Err(e) => panic!("{}", e),
        }
    }

    /// Immutably borrows the wrapped value, returning an error if the value is currently mutably
    /// borrowed.
    ///
    /// The borrow lasts until the returned `GcCellRef` exits scope. Multiple immutable borrows can be
    /// taken out at the same time.
    ///
    /// This is the non-panicking variant of [`borrow`](#method.borrow).
    ///
    /// # Errors
    ///
    /// Returns an `Err` if the value is currently mutably borrowed.
    pub fn try_borrow(&self) -> Result<GcRef<'_, T>, BorrowError> {
        if self.borrow.get().borrowed() == BorrowState::Writing {
            return Err(BorrowError);
        }
        self.borrow.set(self.borrow.get().add_reading());

        // SAFETY: calling value on a rooted value may cause Undefined Behavior
        unsafe {
            Ok(GcRef {
                borrow: BorrowGcRef {
                    borrow: &self.borrow,
                },
                value: NonNull::new_unchecked(self.cell.get()),
            })
        }
    }

    /// Mutably borrows the wrapped value, returning an error if the value is currently borrowed.
    ///
    /// The borrow lasts until the returned `GcCellRefMut` exits scope.
    /// The value cannot be borrowed while this borrow is active.
    ///
    /// This is the non-panicking variant of [`borrow_mut`](#method.borrow_mut).
    ///
    /// # Errors
    ///
    /// Returns an `Err` if the value is currently borrowed.
    pub fn try_borrow_mut(&self) -> Result<GcRefMut<'_, T>, BorrowMutError> {
        self.try_borrow_mut_impl(true)
    }

    /// Mutably borrows the wrapped value without blocking garbage collection.
    ///
    /// # Panics
    ///
    /// Panics if the value is already borrowed.
    ///
    /// # Safety
    ///
    /// The caller must not allocate garbage-collected memory or otherwise
    /// trigger collection until the returned guard is dropped.
    #[must_use]
    pub unsafe fn borrow_mut_no_gc(&self) -> GcRefMut<'_, T> {
        match self.try_borrow_mut_impl(false) {
            Ok(value) => value,
            Err(e) => panic!("{}", e),
        }
    }

    #[inline]
    fn try_borrow_mut_impl(
        &self,
        blocks_collection: bool,
    ) -> Result<GcRefMut<'_, T>, BorrowMutError> {
        if self.borrow.get().borrowed() != BorrowState::Unused {
            return Err(BorrowMutError);
        }
        self.borrow.set(self.borrow.get().set_writing());
        if blocks_collection {
            begin_collection_block();
        }

        // SAFETY: This is safe as the value is rooted if it was not previously rooted,
        // so it cannot be dropped.
        let barrier_state = ptr::from_ref(&self.barrier).cast::<()>();
        unsafe {
            Ok(GcRefMut {
                borrow: BorrowGcRefMut {
                    borrow: &self.borrow,
                    barrier: self.barrier.callback.get(),
                    barrier_state,
                    blocks_collection,
                },
                value: NonNull::new_unchecked(self.cell.get()),
                marker: PhantomData,
            })
        }
    }
}

/// An error returned by [`GcCell::try_borrow`](struct.GcCell.html#method.try_borrow).
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Default, Hash)]
pub struct BorrowError;

impl Display for BorrowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt("GcCell<T> already mutably borrowed", f)
    }
}

/// An error returned by [`GcCell::try_borrow_mut`](struct.GcCell.html#method.try_borrow_mut).
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Default, Hash)]
pub struct BorrowMutError;

impl Display for BorrowMutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt("GcCell<T> already borrowed", f)
    }
}

impl<T: Trace + ?Sized> Finalize for GcRefCell<T> {}

// SAFETY: GcCell maintains its own BorrowState and rootedness. GcCell's implementation
// focuses on only continuing Trace-based methods while the cell state is not written.
// Implementing a Trace while the cell is being written to or incorrectly implementing Trace
// on GcCell's value may cause Undefined Behavior
unsafe impl<T: Trace + ?Sized> Trace for GcRefCell<T> {
    unsafe fn trace(&self, tracer: &mut Tracer) {
        if let Some(owner) = tracer
            .current_node()
            // SAFETY: the current node is live for the duration of tracing.
            .filter(|owner| unsafe { owner.as_ref() }.header.is_old())
        {
            self.barrier.owner.set(Some(owner));
            self.barrier.callback.set(Some(trace_barrier));
            self.barrier.dirty.set(false);
        }

        match self.borrow.get().borrowed() {
            BorrowState::Writing => (),
            // SAFETY: Please see GcCell's Trace impl Safety note.
            _ => unsafe { (&*self.cell.get()).trace(tracer) },
        }
    }

    fn run_finalizer(&self) {
        Finalize::finalize(self);
        match self.borrow.get().borrowed() {
            BorrowState::Writing => (),
            // SAFETY: Please see GcCell's Trace impl Safety note.
            _ => unsafe { (*self.cell.get()).run_finalizer() },
        }
    }
}

unsafe fn trace_barrier(state: *const ()) {
    // SAFETY: `state` is the stable BarrierState belonging to a traced cell,
    // and its owner pointer was initialized by that cell's Trace method.
    let state = unsafe { &*(state.cast::<BarrierState>()) };
    let owner = state
        .owner
        .get()
        .expect("traced cell barrier was missing its owner pointer");
    if !state.dirty.replace(true) {
        remember_old_parent(owner);
    }
}

struct BorrowGcRef<'a> {
    borrow: &'a Cell<BorrowFlag>,
}

impl Drop for BorrowGcRef<'_> {
    fn drop(&mut self) {
        debug_assert_eq!(self.borrow.get().borrowed(), BorrowState::Reading);
        self.borrow.set(self.borrow.get().sub_reading());
    }
}

impl Clone for BorrowGcRef<'_> {
    #[inline]
    fn clone(&self) -> Self {
        self.borrow.set(self.borrow.get().add_reading());
        BorrowGcRef {
            borrow: self.borrow,
        }
    }
}

/// A wrapper type for an immutably borrowed value from a `GcCell<T>`.
pub struct GcRef<'a, T: ?Sized + 'static> {
    value: NonNull<T>,
    borrow: BorrowGcRef<'a>,
}

impl<'a, T: ?Sized> GcRef<'a, T> {
    /// Casts to a `GcRef` of another type.
    ///
    /// # Safety
    /// * The caller must ensure that `T` can be safely cast to `U`.
    #[must_use]
    pub unsafe fn cast<U>(orig: Self) -> GcRef<'a, U> {
        let value = orig.value.cast::<U>();

        GcRef {
            borrow: orig.borrow,
            value,
        }
    }

    /// Copies a `GcCellRef`.
    ///
    /// The `GcCell` is already immutably borrowed, so this cannot fail.
    ///
    /// This is an associated function that needs to be used as
    /// `GcCellRef::clone(...)`. A `Clone` implementation or a method
    /// would interfere with the use of `c.borrow().clone()` to clone
    /// the contents of a `GcCell`.
    #[allow(clippy::should_implement_trait)]
    #[must_use]
    pub fn clone(orig: &GcRef<'a, T>) -> GcRef<'a, T> {
        GcRef {
            borrow: orig.borrow.clone(),
            value: orig.value,
        }
    }

    /// Tries to make a new `GcCellRef` from a component of the borrowed data, returning `None`
    /// if the mapping function returns `None`.
    ///
    /// The `GcCell` is already immutably borrowed, so this cannot fail.
    ///
    /// This is an associated function that needs to be used as `GcCellRef::try_map(...)`.
    /// A method would interfere with methods of the same name on the contents
    /// of a `GcCellRef` used through `Deref`.
    pub fn try_map<U: ?Sized, F>(orig: Self, f: F) -> Option<GcRef<'a, U>>
    where
        F: FnOnce(&T) -> Option<&U>,
    {
        let value = NonNull::from(f(&*orig)?);

        let ret = GcRef {
            borrow: orig.borrow,
            value,
        };

        Some(ret)
    }

    /// Makes a new `GcCellRef` from a component of the borrowed data.
    ///
    /// The `GcCell` is already immutably borrowed, so this cannot fail.
    ///
    /// This is an associated function that needs to be used as `GcCellRef::map(...)`.
    /// A method would interfere with methods of the same name on the contents
    /// of a `GcCellRef` used through `Deref`.
    pub fn map<U: ?Sized, F>(orig: Self, f: F) -> GcRef<'a, U>
    where
        F: FnOnce(&T) -> &U,
    {
        let value = NonNull::from(f(&*orig));

        GcRef {
            borrow: orig.borrow,
            value,
        }
    }

    /// Splits a `GcCellRef` into multiple `GcCellRef`s for different components of the borrowed data.
    ///
    /// The `GcCell` is already immutably borrowed, so this cannot fail.
    ///
    /// This is an associated function that needs to be used as `GcCellRef::map_split(...)`.
    /// A method would interfere with methods of the same name on the contents of a `GcCellRef` used through `Deref`.
    pub fn map_split<U: ?Sized, V: ?Sized, F>(orig: Self, f: F) -> (GcRef<'a, U>, GcRef<'a, V>)
    where
        F: FnOnce(&T) -> (&U, &V),
    {
        let (a, b) = f(&*orig);
        let borrow = orig.borrow.clone();

        (
            GcRef {
                borrow,
                value: NonNull::from(a),
            },
            GcRef {
                value: NonNull::from(b),
                borrow: orig.borrow,
            },
        )
    }
}

impl<T: ?Sized> Deref for GcRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { self.value.as_ref() }
    }
}

impl<T: ?Sized + Debug> Debug for GcRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + Display> Display for GcRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(&**self, f)
    }
}

struct BorrowGcRefMut<'a> {
    borrow: &'a Cell<BorrowFlag>,
    barrier: Option<BarrierFn>,
    barrier_state: *const (),
    blocks_collection: bool,
}

impl Drop for BorrowGcRefMut<'_> {
    fn drop(&mut self) {
        debug_assert_eq!(self.borrow.get().borrowed(), BorrowState::Writing);
        if let Some(barrier) = self.barrier {
            // SAFETY: The pointer was captured from the cell while it was
            // traced, and the mutable borrow keeps the allocation alive and
            // stable until this guard releases it.
            unsafe { barrier(self.barrier_state) };
        }
        self.borrow.set(BorrowFlag(UNUSED));
        if self.blocks_collection {
            end_collection_block();
        }
    }
}

/// A wrapper type for a mutably borrowed value from a `GcCell<T>`.
pub struct GcRefMut<'a, T: ?Sized> {
    // NB: we use a pointer instead of `&'a mut U` to avoid `noalias` violations, because
    // a `GcRefMut` argument doesn't hold exclusivity for its whole scope, only until it
    // drops.
    value: NonNull<T>,
    borrow: BorrowGcRefMut<'a>,
    // `NonNull` is covariant over `T`, so we need to reintroduce invariance.
    marker: PhantomData<&'a mut T>,
}

impl<'a, T: ?Sized> GcRefMut<'a, T> {
    /// Casts to a `GcRefMut` of another type.
    ///
    /// # Safety
    /// * The caller must ensure that `U` can be safely cast to `V`.
    #[must_use]
    pub unsafe fn cast<V>(orig: Self) -> GcRefMut<'a, V> {
        let value = orig.value.cast::<V>();
        let borrow = orig.borrow;

        GcRefMut {
            borrow,
            value,
            marker: PhantomData,
        }
    }

    /// Tries to make a new `GcCellRefMut` for a component of the borrowed data, returning `None`
    /// if the mapping function returns `None`.
    ///
    /// The `GcCellRefMut` is already mutably borrowed, so this cannot fail.
    ///
    /// This is an associated function that needs to be used as
    /// `GcCellRefMut::map(...)`. A method would interfere with methods of the same
    /// name on the contents of a `GcCell` used through `Deref`.
    pub fn try_map<V: ?Sized, F>(mut orig: GcRefMut<'a, T>, f: F) -> Option<GcRefMut<'a, V>>
    where
        F: FnOnce(&mut T) -> Option<&mut V>,
    {
        let value = NonNull::from(f(&mut *orig)?);
        let borrow = orig.borrow;

        let ret = GcRefMut {
            borrow,
            value,
            marker: PhantomData,
        };

        Some(ret)
    }

    /// Makes a new `GcCellRefMut` for a component of the borrowed data, e.g., an enum
    /// variant.
    ///
    /// The `GcCellRefMut` is already mutably borrowed, so this cannot fail.
    ///
    /// This is an associated function that needs to be used as
    /// `GcCellRefMut::map(...)`. A method would interfere with methods of the same
    /// name on the contents of a `GcCell` used through `Deref`.
    pub fn map<V: ?Sized, F>(mut orig: Self, f: F) -> GcRefMut<'a, V>
    where
        F: FnOnce(&mut T) -> &mut V,
    {
        let value = NonNull::from(f(&mut *orig));
        let borrow = orig.borrow;

        GcRefMut {
            borrow,
            value,
            marker: PhantomData,
        }
    }
}

impl<T: ?Sized> Deref for GcRefMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the value is accessible as long as we hold our borrow.
        unsafe { self.value.as_ref() }
    }
}

impl<T: ?Sized> DerefMut for GcRefMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the value is accessible as long as we hold our borrow.
        unsafe { self.value.as_mut() }
    }
}

impl<T: Debug + ?Sized> Debug for GcRefMut<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Debug::fmt(&**self, f)
    }
}

impl<T: Display + ?Sized> Display for GcRefMut<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(&**self, f)
    }
}

// SAFETY: GcCell<T> tracks it's `BorrowState` is `Writing`
unsafe impl<T: ?Sized + Send> Send for GcRefCell<T> {}

impl<T: Trace + Clone> Clone for GcRefCell<T> {
    fn clone(&self) -> Self {
        Self::new(self.borrow().clone())
    }
}

impl<T: Default> Default for GcRefCell<T> {
    fn default() -> Self {
        Self::new(Default::default())
    }
}

#[allow(clippy::inline_always)]
impl<T: ?Sized + PartialEq> PartialEq for GcRefCell<T> {
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        *self.borrow() == *other.borrow()
    }
}

impl<T: ?Sized + Eq> Eq for GcRefCell<T> {}

#[allow(clippy::inline_always)]
impl<T: ?Sized + PartialOrd> PartialOrd for GcRefCell<T> {
    #[inline(always)]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        (*self.borrow()).partial_cmp(&*other.borrow())
    }

    #[inline(always)]
    fn lt(&self, other: &Self) -> bool {
        *self.borrow() < *other.borrow()
    }

    #[inline(always)]
    fn le(&self, other: &Self) -> bool {
        *self.borrow() <= *other.borrow()
    }

    #[inline(always)]
    fn gt(&self, other: &Self) -> bool {
        *self.borrow() > *other.borrow()
    }

    #[inline(always)]
    fn ge(&self, other: &Self) -> bool {
        *self.borrow() >= *other.borrow()
    }
}

impl<T: ?Sized + Ord> Ord for GcRefCell<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        (*self.borrow()).cmp(&*other.borrow())
    }
}

impl<T: ?Sized + Debug> Debug for GcRefCell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.borrow.get().borrowed() {
            BorrowState::Unused | BorrowState::Reading => f
                .debug_struct("GcCell")
                .field("flags", &self.borrow.get())
                .field("value", &self.borrow())
                .finish_non_exhaustive(),
            BorrowState::Writing => f
                .debug_struct("GcCell")
                .field("flags", &self.borrow.get())
                .field("value", &"<borrowed>")
                .finish_non_exhaustive(),
        }
    }
}
