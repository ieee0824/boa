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
use std::{
    cell::{Cell, RefCell},
    mem,
    ptr::NonNull,
};

pub use crate::trace::{Finalize, Trace, Tracer};
pub use boa_macros::{Finalize, Trace};
pub use cell::{GcRef, GcRefCell, GcRefMut};
pub use internals::GcBox;
pub use pointers::{
    Ephemeron, EphemeronEdge, Gc, GcEdge, GcErased, GcErasedEdge, Rooted, WeakGc, WeakGcEdge,
    WeakMap, WeakMapEdge,
};

type GcErasedPointer = NonNull<GcBox<NonTraceable>>;
type EphemeronPointer = NonNull<dyn ErasedEphemeronBox>;
type ErasedWeakMapBoxPointer = NonNull<dyn ErasedWeakMapBox>;
type RootProviderPointer = NonNull<dyn Trace>;

thread_local!(static GC_DROPPING: Cell<bool> = const { Cell::new(false) });
thread_local!(static GC_SUSPENDED: Cell<usize> = const { Cell::new(0) });
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
thread_local!(static BOA_GC: RefCell<BoaGc> = {
    // The collector can own traced values containing `Rooted` handles. Initialize
    // the root bookkeeping first so it remains alive while the collector is dropped
    // during thread teardown.
    ROOTED_ALLOCATIONS.with(|_| {});
    ROOTED_EPHEMERONS.with(|_| {});
    ROOT_PROVIDERS.with(|_| {});
    RefCell::new(BoaGc {
        config: GcConfig::default(),
        runtime: GcRuntimeData::default(),
        strongs: Vec::default(),
        weaks: Vec::default(),
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
    if unsafe { pointer.as_ref() }.header.register_root() {
        ROOTED_ALLOCATIONS.with(|count| count.set(count.get() + 1));
    }
}

fn unregister_root(pointer: GcErasedPointer) {
    if teardown_in_progress() {
        return;
    }

    // SAFETY: a caller holding a handle to the allocation proves the pointer is valid.
    if unsafe { pointer.as_ref() }.header.unregister_root() {
        ROOTED_ALLOCATIONS.with(|count| {
            count.set(
                count
                    .get()
                    .checked_sub(1)
                    .expect("rooted allocation count underflowed"),
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
    GC_SUSPENDED.with(Cell::get) > 0
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
        register_root(pointer);
        Self(pointer)
    }
}

impl Drop for TemporaryStrongRoot {
    fn drop(&mut self) {
        unregister_root(self.0);
    }
}

struct TemporaryEphemeronRoot(EphemeronPointer);

impl TemporaryEphemeronRoot {
    fn new(pointer: EphemeronPointer) -> Self {
        register_ephemeron_root(pointer);
        Self(pointer)
    }
}

impl Drop for TemporaryEphemeronRoot {
    fn drop(&mut self) {
        unregister_ephemeron_root(self.0);
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
    /// The percentage of used space at which the garbage collector will trigger a collection.
    used_space_percentage: usize,
}

// Setting the defaults to an arbitrary value currently.
//
// TODO: Add a configure later
impl Default for GcConfig {
    fn default() -> Self {
        Self {
            // A collection walks the whole heap, so it costs time proportional to
            // everything allocated since the last one — live or not — while reclaiming
            // only the garbage. The threshold sets how much garbage that is, and so
            // trades collection frequency against the cost of each one.
            //
            // Measured on the omoikane benchmark: 1 MiB left `closure-alloc` spending
            // 40% of its wall time in the collector, at 239 collections over 300,000
            // iterations. 4 MiB cuts that to 27 collections and takes 21% off the
            // shape, for 1.8% more peak RSS on an allocation-heavy workload and no
            // measurable change on a page render. 16 MiB is no faster than 4 MiB and
            // costs 61% more peak RSS, since by then the growth in what each
            // collection must walk past has caught up with the drop in their number.
            //
            // The nursery is ~1-8MB in V8 and up to 16MB in SpiderMonkey, but both are
            // generational and so never walk the accumulated garbage at all, which is
            // why they can hold more of it without paying for it.
            threshold: diagnostic_threshold().unwrap_or(4 * 1_048_576),
            used_space_percentage: 70,
        }
    }
}

/// TEMPORARY #330 DIAGNOSTIC — remove before merging.
///
/// Lets a run force a tiny collection threshold so that missing root
/// registrations surface immediately instead of depending on allocation timing.
fn diagnostic_threshold() -> Option<usize> {
    std::env::var("BOA_GC_THRESHOLD").ok()?.parse().ok()
}

/// TEMPORARY #330 DIAGNOSTIC — remove before merging.
///
/// When set, the collector leaks and poisons what it would have freed, so a missing
/// root registration surfaces as a panic at the offending dereference.
pub(crate) fn diagnose_roots() -> bool {
    thread_local!(static ENABLED: bool = std::env::var_os("BOA_GC_DIAGNOSE_ROOTS").is_some());
    ENABLED.with(|enabled| *enabled)
}

#[derive(Default, Debug, Clone, Copy)]
struct GcRuntimeData {
    collections: usize,
    bytes_allocated: usize,
}

#[derive(Debug)]
struct BoaGc {
    config: GcConfig,
    runtime: GcRuntimeData,
    strongs: Vec<GcErasedPointer>,
    weaks: Vec<EphemeronPointer>,
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
            gc.strongs.push(erased);
            gc.runtime.bytes_allocated += element_size;

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
            gc.weaks.push(erased);
            gc.runtime.bytes_allocated += element_size;

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
    /// Run a collection on the full heap.
    fn collect(gc: &mut BoaGc) {
        gc.runtime.collections += 1;

        let mut tracer = Tracer::new();

        let unreachables = Self::mark_heap(&mut tracer, &gc.strongs, &gc.weaks, &gc.weak_maps);

        assert!(tracer.is_empty(), "The queue should be empty");

        // Only finalize if there are any unreachable nodes.
        if !unreachables.strong.is_empty() || !unreachables.weak.is_empty() {
            // Finalize all the unreachable nodes.
            // SAFETY: All passed pointers are valid, since we won't deallocate until `Self::sweep`.
            unsafe { Self::finalize(unreachables) };

            // Reuse the tracer's already allocated capacity.
            let _final_unreachables =
                Self::mark_heap(&mut tracer, &gc.strongs, &gc.weaks, &gc.weak_maps);
        }

        // SAFETY: The head of our linked list is always valid per the invariants of our GC.
        unsafe {
            Self::sweep(
                &mut gc.strongs,
                &mut gc.weaks,
                &mut gc.runtime.bytes_allocated,
            );
        }

        // Weak maps have to be cleared after the sweep, since the process dereferences GcBoxes.
        gc.weak_maps.retain(|w| {
            // SAFETY: The caller must ensure the validity of every node of `heap_start`.
            let node_ref = unsafe { w.as_ref() };

            if node_ref.is_live() {
                node_ref.clear_dead_entries();

                true
            } else {
                // SAFETY:
                // The `Allocator` must always ensure its start node is a valid, non-null pointer that
                // was allocated by `Box::from_raw(Box::new(..))`.
                let _unmarked_node = unsafe { Box::from_raw(w.as_ptr()) };

                false
            }
        });

        gc.strongs.shrink_to(gc.strongs.len() >> 2);
        gc.weaks.shrink_to(gc.weaks.len() >> 2);
        gc.weak_maps.shrink_to(gc.weak_maps.len() >> 2);
    }

    /// Walk the heap and mark any nodes deemed reachable
    fn mark_heap(
        tracer: &mut Tracer,
        strongs: &[GcErasedPointer],
        weaks: &[EphemeronPointer],
        weak_maps: &[ErasedWeakMapBoxPointer],
    ) -> Unreachables {
        // Walk the list, tracing and marking the nodes
        let mut strong_dead = Vec::new();
        let mut pending_ephemerons = Vec::new();

        // === Preliminary mark phase ===
        //
        // 0. Trace every explicitly registered strong root. The registration lives in
        // each allocation's header, so finding the roots is one pass reading a counter
        // rather than tracing every object's fields to work out which handles are
        // internal to the heap.
        for node in strongs {
            // SAFETY: node must be valid as this phase cannot drop any node.
            if unsafe { node.as_ref() }.header.is_rooted() {
                tracer.enqueue(*node);
            }
        }
        // 0.0. Trace every registered heap-external root provider, such as the VM's
        // value stack, which holds roots without registering them one by one.
        ROOT_PROVIDERS.with(|providers| {
            for (_, provider) in providers.borrow().iter() {
                // SAFETY: `RootProvider::register` requires the provider to stay valid and
                // free of mutable aliases for as long as it is registered.
                unsafe { provider.as_ref().trace(tracer) };
            }
        });
        // SAFETY: registered roots point to live collector allocations.
        unsafe { tracer.trace_until_empty() };

        // Get the naive list of possibly dead nodes.
        for node in strongs {
            // SAFETY: node must be valid as this phase cannot drop any node.
            if unsafe { !node.as_ref().is_marked() } {
                strong_dead.push(*node);
            }
        }

        // 0.1. Early return if there are no ephemerons in the GC
        if weaks.is_empty() {
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
        // 1. Mark explicitly registered ephemeron roots, then get the naive list of ephemerons
        // that are supposedly dead or whose key is dead.
        for eph in weaks {
            // SAFETY: node must be valid as this phase cannot drop any node.
            let header = unsafe { eph.as_ref() }.header();
            if header.is_rooted() {
                header.mark();
            }
        }

        for eph in weaks {
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
            } else if diagnose_roots() {
                // TEMPORARY #330 DIAGNOSTIC — remove before merging.
                //
                // Leak the allocation and poison it instead of freeing, so that a handle
                // the collector failed to see reports itself at its next dereference
                // with a usable backtrace, rather than reading freed memory.
                *total_allocated -= node_ref.size();
                node_ref.header.poison();
                #[allow(clippy::print_stderr, reason = "temporary #330 diagnostic")]
                {
                    eprintln!("[#330] collected {}", node_ref.type_name());
                }

                false
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

        for node in mem::take(&mut gc.strongs) {
            // SAFETY:
            // The `Allocator` must always ensure its start node is a valid, non-null pointer that
            // was allocated by `Box::from_raw(Box::new(..))`.
            let drop_fn = unsafe { node.as_ref() }.drop_fn();

            // SAFETY: The function pointer is appropriate for this node type because we extract it from it's VTable.
            unsafe {
                drop_fn(node);
            }
        }

        for node in mem::take(&mut gc.weaks) {
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

        if gc.runtime.bytes_allocated > 0 {
            Collector::collect(&mut gc);
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
