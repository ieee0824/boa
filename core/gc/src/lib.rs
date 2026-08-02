//! Boa's **`boa_gc`** crate implements a garbage collector.
//!
//! # Crate Overview
//! **`boa_gc`** is a mark-sweep garbage collector that implements a [`Trace`] and [`Finalize`] trait
//! for garbage collected values.
#![doc = include_str!("../ABOUT.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/boa-dev/boa/main/assets/logo_black.svg",
    html_favicon_url = "https://raw.githubusercontent.com/boa-dev/boa/main/assets/logo_black.svg"
)]
#![cfg_attr(not(test), forbid(clippy::unwrap_used))]
#![allow(
    clippy::module_name_repetitions,
    clippy::redundant_pub_crate,
    clippy::let_unit_value
)]

extern crate self as boa_gc;

mod cell;
mod pointers;
mod trace;

pub(crate) mod internals;

use internals::{EphemeronBox, ErasedEphemeronBox, ErasedWeakMapBox, WeakMapBox};
use pointers::{NonTraceable, RawWeakMap};
#[cfg(feature = "gc-profile")]
use std::time::{Duration, Instant};
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    mem,
    ptr::NonNull,
};

pub use crate::trace::{Finalize, Trace, Tracer};
pub use boa_macros::{Finalize, Trace};
pub use cell::{GcRef, GcRefCell, GcRefMut};
pub use internals::GcBox;
pub use pointers::{
    Ephemeron, EphemeronEdge, Gc, GcEdge, GcErased, GcErasedEdge, Rooted, WeakGc, WeakGcEdge,
    WeakGcRoot, WeakMap, WeakMapEdge, WeakMapRoot,
};

type GcErasedPointer = NonNull<GcBox<NonTraceable>>;
type EphemeronPointer = NonNull<dyn ErasedEphemeronBox>;
type ErasedWeakMapBoxPointer = NonNull<dyn ErasedWeakMapBox>;
type RootProviderPointer = NonNull<dyn Trace>;

thread_local!(static GC_DROPPING: Cell<bool> = const { Cell::new(false) });
thread_local!(static GC_SUSPENDED: Cell<usize> = const { Cell::new(0) });
// A collection cannot trace through a `GcRefCell` while its value is mutably
// borrowed. Defer collection until all writing borrows have been released.
thread_local!(static COLLECTION_BLOCKED_BORROWS: Cell<usize> = const { Cell::new(0) });
// Set while the collector is tearing the whole heap down. Root counts live in the
// allocation headers, so a handle dropped after its allocation was freed must not touch
// them. During a sweep this cannot happen — an allocation a root points at is kept — but
// teardown frees everything regardless of what still points at it.
thread_local!(static GC_TEARDOWN: Cell<bool> = const { Cell::new(false) });
thread_local!(static ROOT_PROVIDERS: RefCell<Vec<(usize, RootProviderPointer)>> = RefCell::new(Vec::new()));
// How many allocations currently have at least one registered root. Maintained
// alongside the per-allocation counts so that "are there any roots" and the tests'
// whole-registry assertions stay O(1).
thread_local!(static ROOTED_ALLOCATIONS: Cell<usize> = const { Cell::new(0) });
thread_local!(static ROOTED_EPHEMERONS: Cell<usize> = const { Cell::new(0) });
// The header count makes repeated root clones cheap; these registries make the
// mark phase enumerate roots without scanning every allocation in the heap.
thread_local!(static ROOT_REGISTRY: RefCell<HashSet<GcErasedPointer>> = RefCell::new(HashSet::new()));
thread_local!(static EPHEMERON_ROOT_REGISTRY: RefCell<HashSet<EphemeronPointer>> = RefCell::new(HashSet::new()));
// Allocations can trigger collection before the newly allocated pointer reaches
// its caller. These stacks keep that one in-flight allocation alive without
// paying for a root-count update and hash-table insertion on every allocation.
thread_local!(static TEMPORARY_STRONG_ROOTS: RefCell<Vec<GcErasedPointer>> = const { RefCell::new(Vec::new()) });
thread_local!(static TEMPORARY_EPHEMERON_ROOTS: RefCell<Vec<EphemeronPointer>> = const { RefCell::new(Vec::new()) });
// Young edges already discovered while promoting an allocation.
thread_local!(static REMEMBERED_STRONGS: RefCell<HashSet<GcErasedPointer>> = RefCell::new(HashSet::new()));
thread_local!(static REMEMBERED_EPHEMERONS: RefCell<HashSet<EphemeronPointer>> = RefCell::new(HashSet::new()));
// A mutable borrow of a cell in an old allocation is the write barrier. The hot
// path only dirties the parent; minor collection scans each dirty parent once.
thread_local!(static REMEMBERED_OLD_PARENTS: RefCell<HashSet<GcErasedPointer>> = RefCell::new(HashSet::new()));
#[cfg(feature = "gc-profile")]
thread_local!(static GC_PROFILE: Cell<GcProfile> = const { Cell::new(GcProfile::new()) });
thread_local!(static BOA_GC: RefCell<BoaGc> = {
    // The collector can own traced values containing `Rooted` handles. Initialize
    // the root bookkeeping first so it remains alive while the collector is dropped
    // during thread teardown.
    ROOTED_ALLOCATIONS.with(|_| {});
    ROOTED_EPHEMERONS.with(|_| {});
    ROOT_REGISTRY.with(|_| {});
    EPHEMERON_ROOT_REGISTRY.with(|_| {});
    TEMPORARY_STRONG_ROOTS.with(|_| {});
    TEMPORARY_EPHEMERON_ROOTS.with(|_| {});
    REMEMBERED_STRONGS.with(|_| {});
    REMEMBERED_EPHEMERONS.with(|_| {});
    REMEMBERED_OLD_PARENTS.with(|_| {});
    ROOT_PROVIDERS.with(|_| {});
    RefCell::new(BoaGc {
        config: GcConfig::default(),
        runtime: GcRuntimeData::default(),
        youngs: Vec::default(),
        old_strongs: Vec::default(),
        young_weaks: Vec::default(),
        old_weaks: Vec::default(),
        weak_maps: Vec::default(),
    })
});

/// Registers one root pointing at `pointer`.
///
/// The count lives in the allocation's own header, so this is a counter increment
/// rather than a side-table insertion.
fn register_root(pointer: GcErasedPointer) {
    if teardown_in_progress() {
        return;
    }

    // SAFETY: a caller holding a handle to the allocation proves the pointer is valid.
    let became_rooted = unsafe { pointer.as_ref() }.header.register_root();
    if became_rooted {
        ROOTED_ALLOCATIONS.with(|count| count.set(count.get() + 1));
        ROOT_REGISTRY.with(|roots| {
            assert!(
                roots.borrow_mut().insert(pointer),
                "GC root registry already contained a newly rooted allocation"
            );
        });
    }
}

fn unregister_root(pointer: GcErasedPointer) {
    if teardown_in_progress() {
        return;
    }

    // SAFETY: a caller holding a handle to the allocation proves the pointer is valid.
    let became_unrooted = unsafe { pointer.as_ref() }.header.unregister_root();
    if became_unrooted {
        ROOTED_ALLOCATIONS.with(|count| {
            count.set(
                count
                    .get()
                    .checked_sub(1)
                    .expect("rooted allocation count underflowed"),
            );
        });
        ROOT_REGISTRY.with(|roots| {
            assert!(
                roots.borrow_mut().remove(&pointer),
                "GC root registry did not contain an allocation losing its last root"
            );
        });
    }
}

fn register_ephemeron_root(pointer: EphemeronPointer) {
    if teardown_in_progress() {
        return;
    }

    // SAFETY: a caller holding a handle to the ephemeron proves the pointer is valid.
    if unsafe { pointer.as_ref() }.header().register_root() {
        ROOTED_EPHEMERONS.with(|count| count.set(count.get() + 1));
        EPHEMERON_ROOT_REGISTRY.with(|roots| {
            assert!(
                roots.borrow_mut().insert(pointer),
                "GC ephemeron root registry already contained a newly rooted allocation"
            );
        });
    }
}

fn unregister_ephemeron_root(pointer: EphemeronPointer) {
    if teardown_in_progress() {
        return;
    }

    // SAFETY: a caller holding a handle to the ephemeron proves the pointer is valid.
    if unsafe { pointer.as_ref() }.header().unregister_root() {
        ROOTED_EPHEMERONS.with(|count| {
            count.set(
                count
                    .get()
                    .checked_sub(1)
                    .expect("rooted ephemeron count underflowed"),
            );
        });
        EPHEMERON_ROOT_REGISTRY.with(|roots| {
            assert!(
                roots.borrow_mut().remove(&pointer),
                "GC ephemeron root registry did not contain an allocation losing its last root"
            );
        });
    }
}

fn remember_young(pointer: GcErasedPointer) {
    if teardown_in_progress() {
        return;
    }

    // SAFETY: the pointer was emitted by tracing a live value during a mutable
    // borrow, so its allocation is valid until the barrier has finished.
    if unsafe { pointer.as_ref() }.header.is_young() {
        REMEMBERED_STRONGS.with(|remembered| {
            remembered.borrow_mut().insert(pointer);
        });
    }
}

/// Marks an old allocation as mutated so its direct edges are reconsidered by
/// the next minor collection.
pub(crate) fn remember_old_parent(pointer: GcErasedPointer) {
    if !teardown_in_progress() {
        REMEMBERED_OLD_PARENTS.with(|remembered| {
            remembered.borrow_mut().insert(pointer);
        });
    }
}

pub(crate) fn remember_ephemeron_pointer(pointer: EphemeronPointer) {
    if !teardown_in_progress() {
        REMEMBERED_EPHEMERONS.with(|remembered| {
            remembered.borrow_mut().insert(pointer);
        });
    }
}

/// Records the shallow edges of an allocation immediately before it is
/// promoted. Existing young children then remain visible to future minors even
/// though the parent is no longer in the nursery.
fn remember_young_allocation(pointer: GcErasedPointer) {
    // SAFETY: the allocation is live and is being promoted before any sweep.
    let mut tracer = Tracer::new_minor();
    // SAFETY: the allocation's vtable matches its erased pointer. Setting the
    // current node also installs parent-aware barriers in nested cells.
    unsafe { tracer.trace_shallow_node(pointer) };

    for child in tracer.take_shallow_strong() {
        remember_young(child);
    }
    for ephemeron in tracer.take_shallow_ephemerons() {
        remember_ephemeron_pointer(ephemeron);
    }
}

/// Suspends collection for as long as the guard is alive.
///
/// Bootstrap code builds a graph of objects in native locals and only links it into the
/// heap at the end, so there is no root to register while it runs. Suspending collection
/// over such a window is only sound when the window allocates a bounded amount — it
/// defers reclamation rather than preventing it.
#[derive(Debug)]
pub struct NoGcScope;

impl NoGcScope {
    /// Suspends collection until the returned guard is dropped.
    #[must_use]
    pub fn new() -> Self {
        GC_SUSPENDED.with(|suspended| suspended.set(suspended.get() + 1));
        Self
    }
}

impl Default for NoGcScope {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for NoGcScope {
    fn drop(&mut self) {
        GC_SUSPENDED.with(|suspended| {
            suspended.set(
                suspended
                    .get()
                    .checked_sub(1)
                    .expect("unbalanced `NoGcScope` drop"),
            );
        });
    }
}

fn collection_suspended() -> bool {
    // `GcRefCell::trace` deliberately skips the value while a mutable borrow is
    // active.  Collecting at that point would therefore make the collector observe
    // an incomplete graph.  Treat the borrow as a safepoint boundary: allocations
    // remain allowed, but reclamation is deferred until the borrow is released.
    GC_SUSPENDED.with(Cell::get) > 0 || COLLECTION_BLOCKED_BORROWS.with(Cell::get) > 0
}

pub(crate) fn begin_collection_block() {
    COLLECTION_BLOCKED_BORROWS.with(|active| active.set(active.get() + 1));
}

pub(crate) fn end_collection_block() {
    COLLECTION_BLOCKED_BORROWS.with(|active| {
        active.set(
            active
                .get()
                .checked_sub(1)
                .expect("unbalanced GC collection block"),
        );
    });
}

#[cfg(test)]
pub(crate) fn collection_blocked_borrows() -> usize {
    COLLECTION_BLOCKED_BORROWS.with(Cell::get)
}

/// Whether the collector is freeing the entire heap, so allocation headers may already
/// be gone and must not be touched by root bookkeeping.
fn teardown_in_progress() -> bool {
    GC_TEARDOWN.with(Cell::get)
}

/// Marks the whole-heap teardown window for the root bookkeeping.
struct TeardownGuard;

impl TeardownGuard {
    fn new() -> Self {
        GC_TEARDOWN.with(|teardown| teardown.set(true));
        ROOT_REGISTRY.with(|roots| roots.borrow_mut().clear());
        EPHEMERON_ROOT_REGISTRY.with(|roots| roots.borrow_mut().clear());
        REMEMBERED_STRONGS.with(|remembered| remembered.borrow_mut().clear());
        REMEMBERED_EPHEMERONS.with(|remembered| remembered.borrow_mut().clear());
        REMEMBERED_OLD_PARENTS.with(|remembered| remembered.borrow_mut().clear());
        Self
    }
}

impl Drop for TeardownGuard {
    fn drop(&mut self) {
        GC_TEARDOWN.with(|teardown| teardown.set(false));
    }
}

/// A registration of a heap-external structure that owns garbage collected roots.
///
/// Individual [`Rooted`] handles register themselves one at a time, which is the right
/// trade for handles a native caller holds for a while. It is the wrong trade for the
/// VM's value stack: registering there would put a hash map operation on every push and
/// pop. Instead the stack registers *itself* once, and the collector traces it as a root
/// during the mark phase — no per-value bookkeeping, and a mark cost proportional to the
/// live stack depth rather than to the heap.
///
/// The registration lasts until the guard is dropped.
#[derive(Debug)]
pub struct RootProvider {
    key: usize,
}

impl RootProvider {
    /// Registers `provider` as a source of garbage collected roots.
    ///
    /// # Safety
    ///
    /// - `provider` must point to a live value that stays at this address until the
    ///   returned guard is dropped. Registering a value that can move (because it is
    ///   held by value in a structure the caller may move) is Undefined Behaviour.
    /// - `provider` must not be mutably aliased at any point where an allocation, and so
    ///   a collection, can run. Tracing reads the value, so an outstanding `&mut` to it
    ///   while the collector runs is Undefined Behaviour.
    #[must_use]
    pub unsafe fn register(provider: RootProviderPointer) -> Self {
        let key = provider.as_ptr().cast::<()>().addr();
        ROOT_PROVIDERS.with(|providers| {
            providers.borrow_mut().push((key, provider));
        });
        Self { key }
    }
}

impl Drop for RootProvider {
    fn drop(&mut self) {
        ROOT_PROVIDERS.with(|providers| {
            let mut providers = providers.borrow_mut();
            let index = providers
                .iter()
                .rposition(|(key, _)| *key == self.key)
                .expect("attempted to unregister an unknown GC root provider");
            providers.remove(index);
        });
    }
}

struct TemporaryStrongRoot(GcErasedPointer);

impl TemporaryStrongRoot {
    fn new(pointer: GcErasedPointer) -> Self {
        TEMPORARY_STRONG_ROOTS.with(|roots| roots.borrow_mut().push(pointer));
        Self(pointer)
    }
}

impl Drop for TemporaryStrongRoot {
    fn drop(&mut self) {
        TEMPORARY_STRONG_ROOTS.with(|roots| {
            let mut roots = roots.borrow_mut();
            assert_eq!(
                roots.last().copied(),
                Some(self.0),
                "temporary strong roots must be released in stack order"
            );
            roots.pop();
        });
    }
}

struct TemporaryEphemeronRoot(EphemeronPointer);

impl TemporaryEphemeronRoot {
    fn new(pointer: EphemeronPointer) -> Self {
        TEMPORARY_EPHEMERON_ROOTS.with(|roots| roots.borrow_mut().push(pointer));
        Self(pointer)
    }
}

impl Drop for TemporaryEphemeronRoot {
    fn drop(&mut self) {
        TEMPORARY_EPHEMERON_ROOTS.with(|roots| {
            let mut roots = roots.borrow_mut();
            assert_eq!(
                roots.last().copied(),
                Some(self.0),
                "temporary ephemeron roots must be released in stack order"
            );
            roots.pop();
        });
    }
}

/// The number of allocations with at least one registered root.
#[cfg(test)]
fn registered_root_count() -> usize {
    ROOTED_ALLOCATIONS.with(Cell::get)
}

/// The number of ephemerons with at least one registered root.
#[cfg(test)]
fn registered_ephemeron_root_count() -> usize {
    ROOTED_EPHEMERONS.with(Cell::get)
}

/// How many registered roots point at `pointer`.
#[cfg(test)]
fn root_handles(pointer: GcErasedPointer) -> u32 {
    // SAFETY: the caller holds a handle to the allocation, so the pointer is valid.
    unsafe { pointer.as_ref() }.header.root_count()
}

/// How many registered roots point at the ephemeron `pointer`.
#[cfg(test)]
fn ephemeron_root_handles(pointer: EphemeronPointer) -> u32 {
    // SAFETY: the caller holds a handle to the ephemeron, so the pointer is valid.
    unsafe { pointer.as_ref() }.header().root_count()
}

#[derive(Debug, Clone, Copy)]
struct GcConfig {
    /// The threshold at which the garbage collector will trigger a collection.
    threshold: usize,
    /// The amount of young allocation that triggers a minor collection.
    nursery_threshold: usize,
    /// The percentage of used space at which the garbage collector will trigger a collection.
    used_space_percentage: usize,
}

// Setting the defaults to an arbitrary value currently.
//
// TODO: Add a configure later
impl Default for GcConfig {
    fn default() -> Self {
        Self {
            // The nursery handles short-lived allocation without walking the old
            // generation. Omoikane's 300,000-iteration allocation shapes measured a
            // 1 MiB nursery at 106/37 minor collections and 43.5%/37.6% GC time for
            // closure/object allocation. At 4 MiB that fell to 27/10 collections and
            // 28.4%/21.6% GC time.
            //
            // The full-heap threshold must sit above the nursery or every nursery
            // fill immediately triggers a major collection. 4 MiB caused 37/11
            // majors in those same shapes; 8 and 16 MiB caused none. Choose the
            // smaller bound so long-lived garbage cannot grow for no measured gain.
            threshold: 8 * 1_048_576,
            nursery_threshold: 4 * 1_048_576,
            used_space_percentage: 70,
        }
    }
}

#[derive(Default, Debug, Clone, Copy)]
struct GcRuntimeData {
    collections: usize,
    bytes_allocated: usize,
    nursery_bytes: usize,
}

/// Timing totals for one kind of collection.
#[cfg(feature = "gc-profile")]
#[derive(Debug, Clone, Copy)]
pub struct CollectionProfile {
    /// Number of collections recorded.
    pub collections: usize,
    /// Total wall time spent in this kind of collection.
    pub total: Duration,
    /// Time spent marking reachable allocations.
    pub mark: Duration,
    /// Time spent running finalizers and preparing a second mark.
    pub finalize: Duration,
    /// Time spent sweeping allocations and maintaining heap indexes.
    pub sweep: Duration,
}

#[cfg(feature = "gc-profile")]
impl CollectionProfile {
    const fn new() -> Self {
        Self {
            collections: 0,
            total: Duration::ZERO,
            mark: Duration::ZERO,
            finalize: Duration::ZERO,
            sweep: Duration::ZERO,
        }
    }
}

/// Per-phase garbage-collection timings for the current thread.
#[cfg(feature = "gc-profile")]
#[derive(Debug, Clone, Copy)]
pub struct GcProfile {
    /// Nursery-only collection timings.
    pub minor: CollectionProfile,
    /// Full-heap collection timings.
    pub major: CollectionProfile,
}

#[cfg(feature = "gc-profile")]
impl GcProfile {
    const fn new() -> Self {
        Self {
            minor: CollectionProfile::new(),
            major: CollectionProfile::new(),
        }
    }
}

/// Returns garbage-collection timings accumulated on the current thread.
#[cfg(feature = "gc-profile")]
#[must_use]
pub fn profile() -> GcProfile {
    GC_PROFILE.with(Cell::get)
}

/// Clears garbage-collection timings for the current thread.
#[cfg(feature = "gc-profile")]
pub fn reset_profile() {
    GC_PROFILE.with(|profile| profile.set(GcProfile::new()));
}

#[cfg(feature = "gc-profile")]
fn record_collection_profile(
    minor: bool,
    total: Duration,
    mark: Duration,
    finalize: Duration,
    sweep: Duration,
) {
    GC_PROFILE.with(|profile_cell| {
        let mut profile = profile_cell.get();
        let collection = if minor {
            &mut profile.minor
        } else {
            &mut profile.major
        };
        collection.collections += 1;
        collection.total += total;
        collection.mark += mark;
        collection.finalize += finalize;
        collection.sweep += sweep;
        profile_cell.set(profile);
    });
}

#[derive(Debug)]
struct BoaGc {
    config: GcConfig,
    runtime: GcRuntimeData,
    /// Young strong allocations. Minor collections never need to scan the old index.
    youngs: Vec<GcErasedPointer>,
    /// Strong allocations promoted out of the nursery.
    old_strongs: Vec<GcErasedPointer>,
    /// Young ephemeron allocations.
    young_weaks: Vec<EphemeronPointer>,
    /// Ephemerons promoted out of the nursery.
    old_weaks: Vec<EphemeronPointer>,
    weak_maps: Vec<ErasedWeakMapBoxPointer>,
}

impl Drop for BoaGc {
    fn drop(&mut self) {
        Collector::dump(self);
    }
}

// Whether or not the thread is currently in the sweep phase of garbage collection.
// During this phase, attempts to dereference a `Gc<T>` pointer will trigger a panic.
/// `DropGuard` flags whether the Collector is currently running `Collector::sweep()` or `Collector::dump()`
///
/// While the `DropGuard` is active, all `GcBox`s must not be dereferenced or accessed as it could cause Undefined Behavior
#[derive(Debug, Clone)]
struct DropGuard;

impl DropGuard {
    fn new() -> Self {
        GC_DROPPING.with(|dropping| dropping.set(true));
        Self
    }
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        GC_DROPPING.with(|dropping| dropping.set(false));
    }
}

/// Returns `true` if it is safe for a type to run [`Finalize::finalize`].
#[must_use]
#[inline]
pub fn finalizer_safe() -> bool {
    GC_DROPPING.with(|dropping| !dropping.get())
}

/// The Allocator handles allocation of garbage collected values.
///
/// The allocator can trigger a garbage collection.
#[derive(Debug, Clone, Copy)]
struct Allocator;

impl Allocator {
    /// Allocate a new garbage collected value to the Garbage Collector's heap.
    fn alloc_gc<T: Trace>(value: GcBox<T>) -> NonNull<GcBox<T>> {
        let element_size = size_of_val::<GcBox<T>>(&value);
        // Safety: value cannot be a null pointer, since `Box` cannot return null pointers.
        let ptr = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(value))) };
        let erased: NonNull<GcBox<NonTraceable>> = ptr.cast();
        let temporary_root = TemporaryStrongRoot::new(erased);

        BOA_GC.with(|st| {
            let mut gc = st.borrow_mut();

            // Publish the allocation before the collection that `manage_state` may run.
            // Roots are found by walking the heap, so an allocation the collector cannot
            // see is one whose contents it would not keep alive.
            gc.youngs.push(erased);
            gc.runtime.bytes_allocated += element_size;
            gc.runtime.nursery_bytes += element_size;

            Self::manage_state(&mut gc);

            drop(temporary_root);
            ptr
        })
    }

    fn alloc_ephemeron<K: Trace + ?Sized, V: Trace>(
        value: EphemeronBox<K, V>,
    ) -> NonNull<EphemeronBox<K, V>> {
        let element_size = size_of_val::<EphemeronBox<K, V>>(&value);
        // Safety: value cannot be a null pointer, since `Box` cannot return null pointers.
        let ptr = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(value))) };
        let erased: NonNull<dyn ErasedEphemeronBox> = ptr;
        let temporary_root = TemporaryEphemeronRoot::new(erased);

        BOA_GC.with(|st| {
            let mut gc = st.borrow_mut();

            // Publish before the collection that `manage_state` may run, for the same
            // reason as in `alloc_gc`.
            gc.young_weaks.push(erased);
            gc.runtime.bytes_allocated += element_size;
            gc.runtime.nursery_bytes += element_size;

            Self::manage_state(&mut gc);

            drop(temporary_root);
            ptr
        })
    }

    fn alloc_weak_map<K: Trace + ?Sized, V: Trace + Clone>() -> WeakMap<K, V> {
        let weak_map = WeakMap {
            inner: Rooted::new(GcRefCell::new(RawWeakMap::new())),
        };
        let weak = WeakGc::new(&weak_map.inner);

        BOA_GC.with(|st| {
            let mut gc = st.borrow_mut();

            let weak_box = WeakMapBox { map: weak };

            // Safety: value cannot be a null pointer, since `Box` cannot return null pointers.
            let ptr = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(weak_box))) };
            let erased: ErasedWeakMapBoxPointer = ptr;

            gc.weak_maps.push(erased);

            weak_map
        })
    }

    fn manage_state(gc: &mut BoaGc) {
        if collection_suspended() {
            return;
        }

        if gc.runtime.nursery_bytes > gc.config.nursery_threshold {
            Collector::collect_minor(gc);
        }

        if gc.runtime.bytes_allocated > gc.config.threshold {
            Collector::collect(gc);

            // Post collection check
            // If the allocated bytes are still above the threshold, increase the threshold.
            if gc.runtime.bytes_allocated
                > gc.config.threshold / 100 * gc.config.used_space_percentage
            {
                gc.config.threshold =
                    gc.runtime.bytes_allocated / gc.config.used_space_percentage * 100;
            }
        }
    }
}

struct Unreachables {
    strong: Vec<GcErasedPointer>,
    weak: Vec<NonNull<dyn ErasedEphemeronBox>>,
}

/// This collector currently functions in four main phases
///
/// Mark -> Finalize -> Mark -> Sweep
///
/// 1. Mark nodes as reachable.
/// 2. Finalize the unreachable nodes.
/// 3. Mark again because `Finalize::finalize` can potentially resurrect dead nodes.
/// 4. Sweep and drop all dead nodes.
///
/// A better approach in a more concurrent structure may be to reorder.
///
/// Mark -> Sweep -> Finalize
struct Collector;

impl Collector {
    fn forget_minor_remembered(
        dead_strongs: &[GcErasedPointer],
        promoted_strongs: &[GcErasedPointer],
        dead_ephemerons: &[EphemeronPointer],
    ) {
        REMEMBERED_STRONGS.with(|remembered| {
            let mut remembered = remembered.borrow_mut();
            for pointer in dead_strongs.iter().chain(promoted_strongs) {
                remembered.remove(pointer);
            }
        });
        REMEMBERED_EPHEMERONS.with(|remembered| {
            let mut remembered = remembered.borrow_mut();
            for pointer in dead_ephemerons {
                remembered.remove(pointer);
            }
        });
    }

    fn clear_weak_maps(gc: &mut BoaGc) {
        // Weak maps have to be cleared after the sweep, since the process
        // dereferences GcBoxes.
        gc.weak_maps.retain(|w| {
            // SAFETY: the weak-map registry only contains live host allocations
            // until the entry is removed here.
            let node_ref = unsafe { w.as_ref() };

            if node_ref.is_live() {
                node_ref.clear_dead_entries();
                true
            } else {
                // SAFETY: every entry was allocated by Box::into_raw and is
                // removed exactly once.
                let _unmarked_node = unsafe { Box::from_raw(w.as_ptr()) };
                false
            }
        });
    }

    fn recalculate_nursery_bytes(gc: &mut BoaGc) {
        gc.runtime.nursery_bytes = gc
            .youngs
            .iter()
            .map(|pointer| {
                // SAFETY: the young index contains only live strong allocations.
                unsafe { pointer.as_ref() }.size()
            })
            .sum::<usize>()
            + gc.young_weaks
                .iter()
                .map(|pointer| {
                    // SAFETY: the young index contains only live ephemerons.
                    unsafe { size_of_val(pointer.as_ref()) }
                })
                .sum::<usize>();
    }

    /// Run a collection over the nursery while leaving old allocations alone.
    #[allow(clippy::too_many_lines)]
    fn collect_minor(gc: &mut BoaGc) {
        if gc.youngs.is_empty() && gc.young_weaks.is_empty() {
            return;
        }

        #[cfg(feature = "gc-profile")]
        let total_started = Instant::now();
        #[cfg(feature = "gc-profile")]
        let mut mark_elapsed = Duration::ZERO;
        #[cfg(feature = "gc-profile")]
        let mut finalize_elapsed = Duration::ZERO;

        gc.runtime.collections += 1;
        #[cfg(feature = "gc-profile")]
        let mark_started = Instant::now();
        let pending_ephemerons = Self::mark_minor(&gc.weak_maps);
        #[cfg(feature = "gc-profile")]
        {
            mark_elapsed += mark_started.elapsed();
        }

        let dead_strong: Vec<_> = gc
            .youngs
            .iter()
            .copied()
            .filter(|pointer| {
                // SAFETY: young allocations are not swept until this function
                // has finished finalization and marking.
                unsafe { !pointer.as_ref().header.is_minor_marked() }
            })
            .collect();

        let mut dead_ephemerons: HashSet<EphemeronPointer> = gc
            .young_weaks
            .iter()
            .copied()
            .filter(|pointer| {
                // SAFETY: young ephemerons are not swept until this function has
                // finished finalization and marking.
                unsafe { !pointer.as_ref().header().is_minor_marked() }
            })
            .collect();
        dead_ephemerons.extend(pending_ephemerons.iter().copied());

        // Finalizers can resurrect young allocations, so use the same
        // finalize-and-remark shape as a major collection.
        if !dead_strong.is_empty() || !dead_ephemerons.is_empty() {
            #[cfg(feature = "gc-profile")]
            let finalize_started = Instant::now();
            for pointer in &dead_strong {
                // SAFETY: no young allocation is dropped before finalization.
                let node = unsafe { pointer.as_ref() };
                let run_finalizer = node.run_finalizer_fn();
                // SAFETY: the vtable belongs to this live allocation.
                unsafe { run_finalizer(*pointer) };
            }
            for pointer in &dead_ephemerons {
                // SAFETY: no young ephemeron is dropped before finalization.
                unsafe { pointer.as_ref() }.finalize_and_clear();
            }
            #[cfg(feature = "gc-profile")]
            {
                finalize_elapsed += finalize_started.elapsed();
            }

            #[cfg(feature = "gc-profile")]
            let second_mark_started = Instant::now();
            let second_pending = Self::mark_minor(&gc.weak_maps);
            #[cfg(feature = "gc-profile")]
            {
                mark_elapsed += second_mark_started.elapsed();
            }
            #[cfg(feature = "gc-profile")]
            let second_finalize_started = Instant::now();
            for pointer in second_pending {
                // A newly reachable ephemeron can still have a dead young key.
                // Its value is cleared conservatively before the next pass.
                unsafe { pointer.as_ref() }.finalize_and_clear();
            }
            #[cfg(feature = "gc-profile")]
            {
                finalize_elapsed += second_finalize_started.elapsed();
            }
        }

        #[cfg(feature = "gc-profile")]
        let sweep_started = Instant::now();
        let mut final_dead_strongs = Vec::new();
        let mut promoted_strongs = Vec::new();
        gc.youngs.retain(|pointer| {
            // SAFETY: the pointer remains valid until the nursery sweep below.
            let node = unsafe { pointer.as_ref() };
            if !node.header.is_minor_marked() {
                final_dead_strongs.push(*pointer);
                return false;
            }

            node.header.minor_unmark();
            if node.header.promote_if_mature() {
                remember_young_allocation(*pointer);
                promoted_strongs.push(*pointer);
                return false;
            }
            true
        });
        gc.old_strongs.extend(promoted_strongs.iter().copied());

        let mut final_dead_ephemerons = Vec::new();
        let mut promoted_ephemerons = Vec::new();
        gc.young_weaks.retain(|pointer| {
            // SAFETY: the pointer remains valid until the nursery sweep below.
            let eph = unsafe { pointer.as_ref() };
            if !eph.header().is_minor_marked() {
                final_dead_ephemerons.push(*pointer);
                return false;
            }

            eph.header().minor_unmark();
            if eph.header().promote_if_mature() {
                remember_ephemeron_pointer(*pointer);
                promoted_ephemerons.push(*pointer);
                return false;
            }
            true
        });
        gc.old_weaks.extend(promoted_ephemerons.iter().copied());

        // Remove stale remembered pointers before their allocations are freed.
        // Promoted strong children no longer need to be nursery roots. Promoted
        // ephemerons stay remembered because an old ephemeron can still have a
        // young key or value that minor collection must inspect.
        Self::forget_minor_remembered(
            &final_dead_strongs,
            &promoted_strongs,
            &final_dead_ephemerons,
        );

        {
            // Match the full-heap sweep: destructors of GC edges must not run
            // finalizers while the collector is reclaiming storage.
            let _drop_guard = DropGuard::new();
            for pointer in final_dead_strongs {
                // SAFETY: only unmarked young allocations are present here.
                let node = unsafe { pointer.as_ref() };
                gc.runtime.bytes_allocated = gc
                    .runtime
                    .bytes_allocated
                    .checked_sub(node.size())
                    .expect("allocation byte count underflowed during minor sweep");
                let drop_fn = node.drop_fn();
                // SAFETY: the allocation is unreachable and is removed exactly once.
                unsafe { drop_fn(pointer) };
            }

            for pointer in final_dead_ephemerons {
                // SAFETY: only unmarked young ephemerons are present here.
                let eph = unsafe { pointer.as_ref() };
                gc.runtime.bytes_allocated = gc
                    .runtime
                    .bytes_allocated
                    .checked_sub(size_of_val(eph))
                    .expect("allocation byte count underflowed during minor sweep");
                // SAFETY: the allocation is unreachable and is removed exactly once.
                unsafe { drop(Box::from_raw(pointer.as_ptr())) };
            }
        }

        // A weak map may contain young keys whose ephemerons were cleared above.
        // Its registry is not a heap traversal and can be cleaned after the sweep.
        Self::recalculate_nursery_bytes(gc);
        Self::clear_weak_maps(gc);

        gc.youngs.shrink_to(gc.youngs.len() >> 2);
        gc.young_weaks.shrink_to(gc.young_weaks.len() >> 2);

        #[cfg(feature = "gc-profile")]
        record_collection_profile(
            true,
            total_started.elapsed(),
            mark_elapsed,
            finalize_elapsed,
            sweep_started.elapsed(),
        );
    }

    /// Seeds and solves the minor strong/ephemeron fixed point.
    fn mark_minor(weak_maps: &[ErasedWeakMapBoxPointer]) -> Vec<EphemeronPointer> {
        let mut tracer = Tracer::new_minor();

        ROOT_REGISTRY.with(|roots| {
            for pointer in roots.borrow().iter().copied() {
                tracer.enqueue_root(pointer);
            }
        });
        TEMPORARY_STRONG_ROOTS.with(|roots| {
            for pointer in roots.borrow().iter().copied() {
                tracer.enqueue_root(pointer);
            }
        });
        ROOT_PROVIDERS.with(|providers| {
            for (_, provider) in providers.borrow().iter() {
                // SAFETY: the provider contract is upheld by its registration
                // guard and the provider remains alive during collection.
                unsafe { tracer.trace_root(provider.as_ref()) };
            }
        });
        REMEMBERED_STRONGS.with(|remembered| {
            for pointer in remembered.borrow().iter().copied() {
                // SAFETY: remembered pointers are retained while their
                // allocations are valid; old entries are discarded below.
                if unsafe { pointer.as_ref() }.header.is_young() {
                    tracer.enqueue_root(pointer);
                }
            }
        });
        REMEMBERED_OLD_PARENTS.with(|remembered| {
            // Consume the current dirty set before tracing. Tracing rearms each
            // nested cell's barrier, and a finalizer that mutates a parent later
            // in this collection can then enqueue it into the fresh set for the
            // next minor pass.
            let parents = mem::take(&mut *remembered.borrow_mut());
            let mut parent_tracer = Tracer::new_minor();
            for pointer in parents {
                // Old allocations are not reclaimed by a minor collection, so
                // these parent pointers stay valid until the next major sweep.
                unsafe { parent_tracer.trace_shallow_node(pointer) };

                // Turn the parent's current young edges into direct remembered
                // roots. They stay there until they die or promote, so the old
                // parent itself does not have to be rescanned on every minor.
                for child in parent_tracer.take_shallow_strong() {
                    remember_young(child);
                    tracer.enqueue_root(child);
                }
                for ephemeron in parent_tracer.take_shallow_ephemerons() {
                    remember_ephemeron_pointer(ephemeron);
                    tracer.enqueue_ephemeron(ephemeron);
                }
            }
        });
        EPHEMERON_ROOT_REGISTRY.with(|roots| {
            for pointer in roots.borrow().iter().copied() {
                tracer.enqueue_ephemeron_root(pointer);
            }
        });
        TEMPORARY_EPHEMERON_ROOTS.with(|roots| {
            for pointer in roots.borrow().iter().copied() {
                tracer.enqueue_ephemeron_root(pointer);
            }
        });
        REMEMBERED_EPHEMERONS.with(|remembered| {
            for pointer in remembered.borrow().iter().copied() {
                // SAFETY: remembered pointers are retained while their
                // allocations are valid; old entries are discarded below.
                tracer.enqueue_ephemeron(pointer);
            }
        });

        // Weak maps are host-side registries rather than strong heap edges, so
        // their backing ephemerons must seed a minor collection explicitly.
        for weak_map in weak_maps {
            // SAFETY: the weak-map registry contains live host allocations until
            // the post-sweep cleanup.
            unsafe { weak_map.as_ref().trace(&mut tracer) };
        }

        let mut pending = Vec::new();
        let mut pending_set = HashSet::new();
        loop {
            // SAFETY: all queued pointers come from roots, live heap edges, or
            // the write barrier and remain valid during this pass.
            unsafe { tracer.trace_until_empty() };

            let mut changed = false;
            for pointer in tracer.take_discovered_ephemerons() {
                if pending_set.insert(pointer) {
                    pending.push(pointer);
                    changed = true;
                }
            }

            let mut next_pending = Vec::with_capacity(pending.len());
            for pointer in pending {
                // SAFETY: discovered ephemerons are valid until the minor sweep.
                let is_key_live = unsafe { pointer.as_ref().minor_trace(&mut tracer) };
                if is_key_live {
                    changed = true;
                } else {
                    next_pending.push(pointer);
                }
            }
            pending = next_pending;

            // Values traced by live ephemerons can make a previously dead young
            // key reachable. Repeat until the fixed point stops changing.
            if !tracer.is_empty() || changed {
                continue;
            }
            break;
        }

        pending
    }

    /// Run a collection on the full heap.
    fn collect(gc: &mut BoaGc) {
        #[cfg(feature = "gc-profile")]
        let total_started = Instant::now();
        #[cfg(feature = "gc-profile")]
        let mut mark_elapsed = Duration::ZERO;
        #[cfg(feature = "gc-profile")]
        let mut finalize_elapsed = Duration::ZERO;

        gc.runtime.collections += 1;

        let mut tracer = Tracer::new();

        #[cfg(feature = "gc-profile")]
        let mark_started = Instant::now();
        let unreachables = Self::mark_heap(
            &mut tracer,
            &gc.youngs,
            &gc.old_strongs,
            &gc.young_weaks,
            &gc.old_weaks,
            &gc.weak_maps,
        );
        #[cfg(feature = "gc-profile")]
        {
            mark_elapsed += mark_started.elapsed();
        }

        assert!(tracer.is_empty(), "The queue should be empty");

        // Only finalize if there are any unreachable nodes.
        if !unreachables.strong.is_empty() || !unreachables.weak.is_empty() {
            #[cfg(feature = "gc-profile")]
            let finalize_started = Instant::now();
            // Finalize all the unreachable nodes.
            // SAFETY: All passed pointers are valid, since we won't deallocate until `Self::sweep`.
            unsafe { Self::finalize(unreachables) };
            #[cfg(feature = "gc-profile")]
            {
                finalize_elapsed += finalize_started.elapsed();
            }

            // Reuse the tracer's already allocated capacity.
            #[cfg(feature = "gc-profile")]
            let second_mark_started = Instant::now();
            let _final_unreachables = Self::mark_heap(
                &mut tracer,
                &gc.youngs,
                &gc.old_strongs,
                &gc.young_weaks,
                &gc.old_weaks,
                &gc.weak_maps,
            );
            #[cfg(feature = "gc-profile")]
            {
                mark_elapsed += second_mark_started.elapsed();
            }
        }

        #[cfg(feature = "gc-profile")]
        let sweep_started = Instant::now();
        // Remembered entries are hints, not roots for a major collection. Drop
        // stale hints while all heap pointers are still valid, before sweep can
        // free them.
        Self::retain_major_remembered();

        // SAFETY: The head of our linked list is always valid per the invariants of our GC.
        unsafe {
            Self::sweep(
                &mut gc.youngs,
                &mut gc.young_weaks,
                &mut gc.runtime.bytes_allocated,
            );
            Self::sweep(
                &mut gc.old_strongs,
                &mut gc.old_weaks,
                &mut gc.runtime.bytes_allocated,
            );
        }
        Self::recalculate_nursery_bytes(gc);

        Self::clear_weak_maps(gc);

        gc.youngs.shrink_to(gc.youngs.len() >> 2);
        gc.old_strongs.shrink_to(gc.old_strongs.len() >> 2);
        gc.young_weaks.shrink_to(gc.young_weaks.len() >> 2);
        gc.old_weaks.shrink_to(gc.old_weaks.len() >> 2);
        gc.weak_maps.shrink_to(gc.weak_maps.len() >> 2);

        #[cfg(feature = "gc-profile")]
        record_collection_profile(
            false,
            total_started.elapsed(),
            mark_elapsed,
            finalize_elapsed,
            sweep_started.elapsed(),
        );
    }

    fn retain_major_remembered() {
        REMEMBERED_STRONGS.with(|remembered| {
            remembered.borrow_mut().retain(|pointer| {
                // SAFETY: this runs before the major sweep.
                unsafe { pointer.as_ref() }.is_marked()
            });
        });
        REMEMBERED_EPHEMERONS.with(|remembered| {
            remembered.borrow_mut().retain(|pointer| {
                // SAFETY: this runs before the major sweep.
                unsafe { pointer.as_ref() }.header().is_marked()
            });
        });
        // Every surviving old cell will dirty itself again on its next mutable
        // borrow. Entries for dead parents must be gone before the major sweep.
        REMEMBERED_OLD_PARENTS.with(|remembered| remembered.borrow_mut().clear());
    }

    /// Walk the heap and mark any nodes deemed reachable
    fn mark_heap(
        tracer: &mut Tracer,
        youngs: &[GcErasedPointer],
        old_strongs: &[GcErasedPointer],
        young_weaks: &[EphemeronPointer],
        old_weaks: &[EphemeronPointer],
        weak_maps: &[ErasedWeakMapBoxPointer],
    ) -> Unreachables {
        // Walk the list, tracing and marking the nodes
        let mut strong_dead = Vec::new();
        let mut pending_ephemerons = Vec::new();

        // === Preliminary mark phase ===
        //
        // 0. Trace every explicitly registered strong root. The registry is
        // maintained only when an allocation changes between zero and one roots,
        // so the mark phase is proportional to the root set rather than the heap.
        ROOT_REGISTRY.with(|roots| {
            for pointer in roots.borrow().iter().copied() {
                tracer.enqueue_root(pointer);
            }
        });
        TEMPORARY_STRONG_ROOTS.with(|roots| {
            for pointer in roots.borrow().iter().copied() {
                tracer.enqueue_root(pointer);
            }
        });
        // 0.0. Trace every registered heap-external root provider, such as the VM's
        // value stack, which holds roots without registering them one by one.
        ROOT_PROVIDERS.with(|providers| {
            for (_, provider) in providers.borrow().iter() {
                // SAFETY: `RootProvider::register` requires the provider to stay valid and
                // free of mutable aliases for as long as it is registered.
                unsafe { tracer.trace_root(provider.as_ref()) };
            }
        });
        EPHEMERON_ROOT_REGISTRY.with(|roots| {
            for pointer in roots.borrow().iter().copied() {
                tracer.enqueue_ephemeron_root(pointer);
            }
        });
        TEMPORARY_EPHEMERON_ROOTS.with(|roots| {
            for pointer in roots.borrow().iter().copied() {
                tracer.enqueue_ephemeron_root(pointer);
            }
        });
        // SAFETY: registered roots point to live collector allocations.
        unsafe { tracer.trace_until_empty() };

        // A finalizer between the two major mark passes can mutate an old,
        // already-marked parent. Enqueuing that parent would be skipped by the
        // normal marked-node fast path, so retrace it shallowly and mark the
        // new edges directly. A dirty but unreachable parent is deliberately
        // ignored: remembered entries are hints, not major-collection roots.
        REMEMBERED_OLD_PARENTS.with(|remembered| {
            let parents = mem::take(&mut *remembered.borrow_mut());
            for pointer in parents {
                // SAFETY: major sweep has not started, so remembered old
                // pointers are still valid. Only reachable parents are traced.
                if unsafe { pointer.as_ref() }.is_marked() {
                    unsafe { tracer.trace_shallow_node(pointer) };
                }
            }
        });
        // SAFETY: shallow tracing above emitted edges from live allocations.
        unsafe { tracer.trace_until_empty() };

        // Get the naive list of possibly dead nodes.
        for node in youngs.iter().chain(old_strongs) {
            // SAFETY: node must be valid as this phase cannot drop any node.
            if unsafe { !node.as_ref().is_marked() } {
                strong_dead.push(*node);
            }
        }

        // 0.1. Early return if there are no ephemerons in the GC
        if young_weaks.is_empty() && old_weaks.is_empty() {
            strong_dead.retain_mut(|node| {
                // SAFETY: node must be valid as this phase cannot drop any node.
                unsafe { !node.as_ref().is_marked() }
            });
            return Unreachables {
                strong: strong_dead,
                weak: Vec::new(),
            };
        }

        // === Weak mark phase ===
        //
        //
        // 1. Mark explicitly registered ephemeron roots, then get the naive list
        // of ephemerons that are supposedly dead or whose key is dead.
        for eph in young_weaks.iter().chain(old_weaks) {
            // SAFETY: node must be valid as this phase cannot drop any node.
            let eph_ref = unsafe { eph.as_ref() };
            // SAFETY: the garbage collector ensures `eph_ref` always points to valid data.
            if unsafe { !eph_ref.trace(tracer) } {
                pending_ephemerons.push(*eph);
            }

            // SAFETY: all nodes must be valid as this phase cannot drop any node.
            unsafe {
                tracer.trace_until_empty();
            }
        }

        // 2. Trace all the weak pointers in the live weak maps to make sure they do not get swept.
        for w in weak_maps {
            // SAFETY: node must be valid as this phase cannot drop any node.
            let node_ref = unsafe { w.as_ref() };

            // SAFETY: The garbage collector ensures that all nodes are valid.
            unsafe { node_ref.trace(tracer) };

            // SAFETY: all nodes must be valid as this phase cannot drop any node.
            unsafe {
                tracer.trace_until_empty();
            }
        }

        // 3. Iterate through all pending ephemerons, removing the ones which have been successfully
        // traced. If there are no changes in the pending ephemerons list, it means that there are no
        // more reachable ephemerons from the remaining ephemeron values.
        let mut previous_len = pending_ephemerons.len();
        loop {
            pending_ephemerons.retain_mut(|eph| {
                // SAFETY: node must be valid as this phase cannot drop any node.
                let eph_ref = unsafe { eph.as_ref() };
                // SAFETY: the garbage collector ensures `eph_ref` always points to valid data.
                let is_key_marked = unsafe { !eph_ref.trace(tracer) };

                // SAFETY: all nodes must be valid as this phase cannot drop any node.
                unsafe {
                    tracer.trace_until_empty();
                }

                is_key_marked
            });

            if previous_len == pending_ephemerons.len() {
                break;
            }

            previous_len = pending_ephemerons.len();
        }

        // 4. The remaining list should contain the ephemerons that are either unreachable or its key
        // is dead. Cleanup the strong pointers since this procedure could have marked some more strong
        // pointers.
        strong_dead.retain_mut(|node| {
            // SAFETY: node must be valid as this phase cannot drop any node.
            unsafe { !node.as_ref().is_marked() }
        });

        Unreachables {
            strong: strong_dead,
            weak: pending_ephemerons,
        }
    }

    /// # Safety
    ///
    /// Passing a `strong` or a `weak` vec with invalid pointers will result in Undefined Behaviour.
    unsafe fn finalize(unreachables: Unreachables) {
        for node in unreachables.strong {
            // SAFETY: The caller must ensure all pointers inside `unreachables.strong` are valid.
            let node_ref = unsafe { node.as_ref() };
            let run_finalizer_fn = node_ref.run_finalizer_fn();

            // SAFETY: The function pointer is appropriate for this node type because we extract it from it's VTable.
            unsafe {
                run_finalizer_fn(node);
            }
        }
        for node in unreachables.weak {
            // SAFETY: The caller must ensure all pointers inside `unreachables.weak` are valid.
            let node = unsafe { node.as_ref() };
            node.finalize_and_clear();
        }
    }

    /// # Safety
    ///
    /// - Providing an invalid pointer in the `heap_start` or in any of the headers of each
    ///   node will result in Undefined Behaviour.
    /// - Providing a list of pointers that weren't allocated by `Box::into_raw(Box::new(..))`
    ///   will result in Undefined Behaviour.
    unsafe fn sweep(
        strong: &mut Vec<GcErasedPointer>,
        weak: &mut Vec<EphemeronPointer>,
        total_allocated: &mut usize,
    ) {
        let _guard = DropGuard::new();

        strong.retain(|node| {
            // SAFETY: The caller must ensure the validity of every node of `heap_start`.
            let node_ref = unsafe { node.as_ref() };
            if node_ref.is_marked() {
                node_ref.header.unmark();

                true
            } else {
                // SAFETY: The algorithm ensures only unmarked/unreachable pointers are dropped.
                // The caller must ensure all pointers were allocated by `Box::into_raw(Box::new(..))`.
                let drop_fn = node_ref.drop_fn();
                let size = node_ref.size();
                *total_allocated -= size;

                // SAFETY: The function pointer is appropriate for this node type because we extract it from it's VTable.
                unsafe {
                    drop_fn(*node);
                }

                false
            }
        });

        weak.retain(|eph| {
            // SAFETY: The caller must ensure the validity of every node of `heap_start`.
            let eph_ref = unsafe { eph.as_ref() };
            let header = eph_ref.header();
            if header.is_marked() {
                header.unmark();

                true
            } else {
                // SAFETY: The algorithm ensures only unmarked/unreachable pointers are dropped.
                // The caller must ensure all pointers were allocated by `Box::into_raw(Box::new(..))`.
                let unmarked_eph = unsafe { Box::from_raw(eph.as_ptr()) };
                let unallocated_bytes = size_of_val(&*unmarked_eph);
                *total_allocated -= unallocated_bytes;

                false
            }
        });
    }

    // Clean up the heap when BoaGc is dropped
    fn dump(gc: &mut BoaGc) {
        // Everything here is freed regardless of what still points at it, so a handle
        // dropped along the way must not reach into an allocation's header to update its
        // root count.
        let _teardown = TeardownGuard::new();

        // Weak maps have to be dropped first, since the process dereferences GcBoxes.
        // This can be done without initializing a dropguard since no GcBox's are being dropped.
        for node in mem::take(&mut gc.weak_maps) {
            // SAFETY:
            // The `Allocator` must always ensure its start node is a valid, non-null pointer that
            // was allocated by `Box::from_raw(Box::new(..))`.
            let _unmarked_node = unsafe { Box::from_raw(node.as_ptr()) };
        }

        // Not initializing a dropguard since this should only be invoked when BOA_GC is being dropped.
        let _guard = DropGuard::new();

        for node in mem::take(&mut gc.youngs)
            .into_iter()
            .chain(mem::take(&mut gc.old_strongs))
        {
            // SAFETY:
            // The `Allocator` must always ensure its start node is a valid, non-null pointer that
            // was allocated by `Box::from_raw(Box::new(..))`.
            let drop_fn = unsafe { node.as_ref() }.drop_fn();

            // SAFETY: The function pointer is appropriate for this node type because we extract it from it's VTable.
            unsafe {
                drop_fn(node);
            }
        }

        for node in mem::take(&mut gc.young_weaks)
            .into_iter()
            .chain(mem::take(&mut gc.old_weaks))
        {
            // SAFETY:
            // The `Allocator` must always ensure its start node is a valid, non-null pointer that
            // was allocated by `Box::from_raw(Box::new(..))`.
            let _unmarked_node = unsafe { Box::from_raw(node.as_ptr()) };
        }
    }
}

/// Forcefully runs a garbage collection of all unaccessible nodes.
pub fn force_collect() {
    BOA_GC.with(|current| {
        let mut gc = current.borrow_mut();

        // A mutable `GcRefCell` borrow is not traceable, so forcing a collection
        // inside it would be just as unsound as an allocation-triggered collection.
        // This call is a no-op while collection is suspended; callers that need a
        // forced collection must retry after releasing the borrow or scope.
        if !collection_suspended() && gc.runtime.bytes_allocated > 0 {
            Collector::collect(&mut gc);
        }
    });
}

/// Forcefully runs a nursery collection of all young allocations.
#[cfg(test)]
pub(crate) fn force_minor_collect() {
    BOA_GC.with(|current| {
        let mut gc = current.borrow_mut();

        if !collection_suspended() {
            Collector::collect_minor(&mut gc);
        }
    });
}

#[cfg(test)]
mod test;

/// Returns `true` is any weak maps are currently allocated.
#[cfg(test)]
#[must_use]
pub fn has_weak_maps() -> bool {
    BOA_GC.with(|current| {
        let gc = current.borrow();

        !gc.weak_maps.is_empty()
    })
}
