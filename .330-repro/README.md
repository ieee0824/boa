# #330 root-gap reproducers

Scripts that pin down what the registered-roots collector still gets wrong, and what it
already gets right. Everything here is scaffolding for
[omoikane#330](https://github.com/ieee0824/omoikane/issues/330) and comes out before this
reaches a PR.

## Running them

```sh
cargo build -p boa_cli
BOA_GC_DIAGNOSE_ROOTS=1 BOA_GC_THRESHOLD=0 ./target/debug/boa .330-repro/fail-01-loop-nested-literal.js
```

Four diagnostic environment variables, all added on this branch:

| Variable | Effect |
|---|---|
| `BOA_GC_THRESHOLD=<bytes>` | Forces the collection threshold and **keeps it forced**. `0` collects on every allocation. Without the pinning, `manage_state` grows the threshold after the first collection and the run silently stops being adversarial. |
| `BOA_GC_DIAGNOSE_ROOTS=1` | The sweep leaks and poisons what it would have freed, for both strong allocations and ephemerons. A handle that outlived its allocation then panics at its own dereference, naming the type, instead of reading freed memory. |
| `BOA_GC_POISON_TRACE=<type name fragment>` | Prints a backtrace when an allocation of a matching type is reclaimed. That backtrace is the allocation site that triggered the collection, so it names the operation that was holding the value in a local — which the dereference-side report cannot show. |
| `BOA_GC_DIAGNOSE_WRITING_ALLOC=1` | Stops at the first GC allocation made while any `GcRefCell` writing borrow is active and prints its allocation-side backtrace. `NoGcScope` windows are excluded because collection is intentionally suspended there. |

`BOA_GC_THRESHOLD=0` is far harsher than any real configuration. It is what turns a
timing-dependent crash into a deterministic one: every window where native code holds a
value in a local across a single allocation becomes a failure.

## What passes

All of `pass-*.js` run clean under `BOA_GC_THRESHOLD=0`, which covers rather more than it
looks like: realm bootstrap, every builtin, host class registration, the console, object
and array literals, property assignment, function calls, and the VM value stack.

## The systemic cause: a `GcRefCell` being written to is not traced

`GcRefCell::trace` skips the cell's contents entirely while the cell is mutably borrowed
(`core/gc/src/cell.rs:233`). So an allocation that happens while a `borrow_mut()` is held
runs a collection that cannot see anything below that cell.

Under the old scheme this failed safe. Root discovery worked by tracing the heap to count
how many of an allocation's handles were internal to it, so a skipped subtree merely
*undercounted* those handles, which made the allocation look more rooted and kept it
alive. Under explicit roots the same skip means the subtree has no root at all, and it is
reclaimed.

`Object`'s data lives in a `GcRefCell`, so this applies to any `object.borrow_mut()` held
across an allocation — the object survives (its box is marked from the register), but
every object reachable only through its properties does not.

`fail-01` was one instance of this, in the shape transition table, and is fixed: the weak
reference is now allocated before the borrow rather than inside it.

## Current implementation state (2026-07-30)

The writing-borrow allocation detector is implemented, and the sites it reports are being
worked down. Fixed so far:

1. `Script::codeblock` held the script's code-block cache writing borrow throughout
   compilation, including the final `CodeBlock` allocation. It now checks the cache under
   a shared borrow and takes the writing borrow only to publish the finished edge.
2. `ForwardTransition::insert_property` allocated the weak reference's ephemeron while
   holding the transition map's borrow, so that collection left every transition in the
   map unmarked. The ephemeron is now allocated before the borrow.
3. `validate_and_apply_property_descriptor` held the target object's writing borrow while
   creating a new shape, so the object's own property values were invisible to that
   collection. `PropertyMap::insert_with_slot` now splits into `plan_insert`, which
   computes the transition and may be called under a shared borrow, and `apply_insert`,
   which applies it and allocates nothing.

   The earlier attempt at this was reverted because it let the shape report a slot the
   storage did not have yet. The cause: **a unique shape transitions by mutating its own
   property table in place and handing back the same shape**, so planning one ahead makes
   `lookup` succeed immediately. Only shared shapes are planned ahead now; a unique shape
   transitions inside the borrow, where it allocates nothing anyway.
4. Iterator `next` implementations — array, map, set, string, and for-in — held the
   iterator's mutable borrow across the construction of the result object, and some across
   `get` calls that run arbitrary user code. They now read what they need, write back the
   advanced state, release the borrow, and only then build the result.

Every reproducer passes with the threshold pinned at zero, meaning a collection at every
single allocation.

This branch is diagnostic work, is not a merge candidate, and must not be merged as-is.

### Enumerating the rest

`BOA_GC_DIAGNOSE_WRITING_ALLOC` reports allocations made while a writing borrow is active,
which is the whole class rather than the subset a given script happens to collide with.
Use `warn` to list every distinct site in one run:

```sh
BOA_GC_DIAGNOSE_WRITING_ALLOC=warn cargo test -p boa_engine --lib -- --nocapture --test-threads=1
```

`--nocapture` matters: libtest swallows the stderr of passing tests, so without it the run
looks clean. Any other value for the variable stops at the first site instead, which is
what you want once you are down to fixing them.

Across `boa_engine`'s own tests this is now 84 reports in 11 distinct holder contexts, down
from 289 in 40. What remains, by the frame that holds the borrow:

| Count | Holder |
|---:|---|
| 24 | `builtins/map/map_iterator.rs:87` |
| 20 | `object/mod.rs:280` |
| 16 | `object/internal_methods/mod.rs:1047` — the planning call itself, flagged because an *outer* frame holds a borrow |
| 5 | `builtins/iterable/mod.rs:221` |
| 5 | `object/mod.rs:319` |
| 4 | `builtins/array/mod.rs:343` |
| 3 | `builtins/weak_map/mod.rs:272` |
| 2 | `builtins/array/mod.rs:412` |
| 2 | `builtins/weak_map/mod.rs:396` |
| 2 | `module/namespace.rs:76` |
| 1 | `builtins/weak_map/mod.rs:328` |

These are the same shape as the ones already fixed, and fix the same way.

### The open question: per-site or systemic

The per-site route is what the fixes so far take, and it removes real latent bugs — user
code was running while an iterator was mutably borrowed. But the list above is what one
crate's unit tests reach; test262 will reach more, and every new builtin can add one.

The systemic alternative is to treat an open writing borrow as "not at a safepoint" and
suspend collection for its duration, a few lines next to `collection_suspended` in
`manage_state`. That closes the class by construction: no collection can observe a cell
mid-write. The cost is deferred reclamation while a borrow is open, bounded by what that
borrow allocates — and a long-lived borrow becomes a memory-overshoot bug rather than a
silent reclamation, which this same diagnostic already finds. `force_collect` would need
to assert rather than run while a borrow is open.

Worth deciding before working much further down the list.

## Continuation plan

Continue in this exact order:

1. Run formatting, `boa_gc` tests, engine check, and Clippy for the detector and the two
   current fixes.
2. Run every `.330-repro/pass-*.js` and `.330-repro/fail-*.js` with
   `BOA_GC_DIAGNOSE_WRITING_ALLOC=1`, `BOA_GC_DIAGNOSE_ROOTS=1`, and
   `BOA_GC_THRESHOLD=0`.
3. For each newly reported site, move only the allocation-producing work before the
   writing borrow. Do not hide a site with `NoGcScope`; VM execution is not a bounded
   allocation window.
4. Repeat until the complete reproducer set is clean at the fixed zero threshold, then
   run `boa_gc`, `boa_engine`, `boa_runtime`, all-features/all-targets checks, and Clippy
   with warnings denied.
5. Integrate the verified functional fixes with `feat/registered-roots-collector`. Remove
   every `TEMPORARY #330 DIAGNOSTIC` path, all diagnostic environment variables, poison
   behavior, and `.330-repro/` before making PR #50 reviewable.
6. Run the full GitHub CI and do not merge until every required check has completed
   successfully.
7. Measure collection cost with the #315 stage-2 method and verify the required reduction,
   then run the omoikane full suite, Acid3 100/100, and WPT with zero regression.

PR #50 remains draft throughout steps 1-5. Its merge is forbidden until steps 6-7 are
proven, not merely started.
