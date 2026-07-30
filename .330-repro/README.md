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

Three diagnostic environment variables, all added on this branch:

| Variable | Effect |
|---|---|
| `BOA_GC_THRESHOLD=<bytes>` | Forces the collection threshold and **keeps it forced**. `0` collects on every allocation. Without the pinning, `manage_state` grows the threshold after the first collection and the run silently stops being adversarial. |
| `BOA_GC_DIAGNOSE_ROOTS=1` | The sweep leaks and poisons what it would have freed, for both strong allocations and ephemerons. A handle that outlived its allocation then panics at its own dereference, naming the type, instead of reading freed memory. |
| `BOA_GC_POISON_TRACE=<type name fragment>` | Prints a backtrace when an allocation of a matching type is reclaimed. That backtrace is the allocation site that triggered the collection, so it names the operation that was holding the value in a local — which the dereference-side report cannot show. |

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

## What still fails

Scripts that build several properties on one object still fail, on the same path and for
the same reason as `fail-01` did, one level up:

```
#330: dereferenced a `VTableObject<OrdinaryObject>` the collector already reclaimed
  reclaimed while collecting here:
    SharedShape::new                          shape/shared_shape/mod.rs:138
    SharedShape::insert_property_transition   shape/shared_shape/mod.rs:272
    PropertyMap::insert_with_slot             property_map.rs:543
    validate_and_apply_property_descriptor    internal_methods/mod.rs:1023
    DefineOwnPropertyByName::operation        vm/opcode/define/own_property.rs:29
```

Defining a property needs `&mut Object`, so `validate_and_apply_property_descriptor` holds
the object's `borrow_mut()` while `SharedShape::new` allocates. The reclaimed object is a
value already stored in one of that object's properties: for
`{ a: i, b: { c: i } }`, defining a later property drops `b`'s object from the trace.

Note what this rules out. The object being defined on and the value being defined are both
read from VM registers, and registers are inside the traced value stack — `push_frame`
grows it with `resize_with`, so the register range is within the `Vec`'s length, and the
stack provider was measured enqueueing from it. The value stack is not the problem.

### Finding the rest

Worth building next: count active `Writing` borrows in a thread local, and have the
allocator report a backtrace when it allocates while that count is non-zero. That turns
this class from a silent reclamation into an enumerable list of sites, which is what
sizing the remaining work needs. Each site is then fixed locally by hoisting the
allocation out of the borrow, as `ForwardTransition::insert_property` now does.
