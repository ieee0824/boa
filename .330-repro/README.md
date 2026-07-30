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

## What fails

`fail-01-loop-nested-literal.js` — two lines. A loop building a nested object literal.

```
#330: accessed an ephemeron the collector already reclaimed — its holder is not
registered as a GC root
  ephemeron_box.rs:109
  SharedShape::insert_property_transition   shape/shared_shape/mod.rs:249
  Shape::insert_property_transition         shape/mod.rs:178
  PropertyMap::insert_with_slot             property_map.rs:543
  validate_and_apply_property_descriptor    internal_methods/mod.rs:1023
  DefineOwnPropertyByName::operation        vm/opcode/define/own_property.rs:29
```

Note what the shape of this failure rules out. The object being defined on, and the value
being defined, are both read from VM registers, and registers are inside the traced value
stack — `push_frame` grows it with `resize_with`, so the register range is within the
`Vec`'s length, and the stack provider was measured enqueueing from it. So this is not the
value stack. It is a shape or a transition-table ephemeron that is reachable only from a
native local at the moment `SharedShape::new` allocates.

`pass-03` builds the same nested literal once and passes; only the loop fails. The loop is
what exercises the *second* transition through an existing `ForwardTransition`, so the
suspect is the transition cache's weak entries rather than the freshly built shape.
