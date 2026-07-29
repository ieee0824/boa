# Explicit rooting migration

`boa_gc` currently infers roots by counting every `Gc<T>` handle and then
tracing the complete heap to count the handles stored inside it. An explicit
root set cannot safely replace that scan until heap edges and external owners
are distinguishable.

## Required invariants

1. Every handle held outside the traced heap is registered for its complete
   lifetime.
2. Handles stored in a traced allocation are edges, not roots.
3. Cloning an edge for use by native Rust code creates a root. Moving a root
   into a traced allocation converts it to an edge.
4. A rooted handle cannot be stored in `GcBox`, `GcRefCell`, `Cell`, or another
   traced container without an explicit conversion.
5. Collection semantics remain on the inferred-root implementation until the
   migration is complete and root-lifetime stress tests pass.

## Engine boundary

The engine cannot migrate by replacing the `Gc<T>` fields of public wrappers
one at a time. Types such as `JsObject`, `Realm`, `Script`, `Module`, and
`SharedShape` are used both as native-code owners and as fields of traced
objects. Making their inner handle unconditionally rooted would retain every
heap edge; making it unconditionally unrooted would allow live native locals to
be collected.

Those wrappers therefore need distinct rooted and edge representations (or a
generic representation whose ownership state is encoded in its type). Public
API entry and exit points perform promotion to a root, while `Trace`-derived
fields accept only the edge representation.

## Migration order

1. Add the explicit root registry and `Rooted<T>` compatibility handle.
2. Add `GcEdge<T>` as a distinct edge representation without changing
   collector semantics. Conversion from `Rooted<T>` unregisters the root;
   conversion back registers it again.
3. Split the wrapper types, starting with `JsObject`, and migrate traced fields.
4. Migrate VM stack, call frames, realms, scripts, modules, shapes, closures,
   and embedding API return values.
5. Add compile-fail coverage for putting roots in traced fields, plus forced
   collection tests for native locals.
6. Switch marking to the explicit roots and remove `trace_non_roots` only after
   all previous steps pass the complete engine suite.

Write barriers and generations are deliberately excluded from this migration;
they are subsequent stages of the collector work.
