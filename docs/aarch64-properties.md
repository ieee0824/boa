# ARM64 monomorphic properties and VM helper calls

Related: Omoikane #543, following [arithmetic lowering](aarch64-arithmetic.md).

ARM64 now uses the same property-region analysis, inline-cache validation and
deopt recipes as x86_64. Before native entry, the shared Rust code checks object
identity/aliases, shape and cache generation, descriptors and numeric slot values.
It copies validated direct slots into the engine-owned scalar frame. Generated
loads/stores operate on those slots, and completed writes are committed through
the shared object write barrier on native exit. No unregistered object pointer is
kept in a machine register, and the leaf loop cannot call user code or collect GC.
This is the existing x86 execution model; it does not speculate across helpers.

Shape transitions, stale cache slots, inherited properties, accessors and
megamorphic sites use the same interpreter fallback. Guard hits/misses, property
bailouts and deopt counters come from the common runtime. A Boolean produced
inside a loop causes a type deopt before a property store, preserving its type and
committing earlier writes exactly once. Both CPU emitters enforce this boundary.
Object-copy instructions retain a side-tagged index into the rooted VM register
file. Normal exits and deopt restore those aliases even when a temporary was
previously used for a Number or Boolean. Scalar operations on an alias deopt to
perform JavaScript coercion in the interpreter. Alias source registers must keep
the same object throughout the region; otherwise compilation is rejected.

VM helper sites use the ARM64 foundation's AAPCS64/Apple trampoline. The generated
stub preserves the host ABI and reports its actual return offset (16 bytes on
ARM64, 9 on x86_64). Safepoints and exception unwinding use that offset for every
site. JavaScript values remain in the VM's registered stack and helper roots;
native code contains only the borrowed invocation and trampoline pointers.

The existing property/GC/deopt corpus and generated-helper exception tests run on
both CPU families: #305 prop-mono, aliasing, shape/descriptor/prototype mutation,
stale IC invalidation, GC before deopt, completed stores, nested native reentry,
opaque exceptions, cache eviction, allocation failure and interrupt recovery.
A metadata test checks every helper entry against the architecture return PC.
Run `cargo test --locked -p boa_engine --features baseline-jit jit:: -- --nocapture`.
The native CI matrix executes this command on Linux/macOS x86_64 and ARM64.
