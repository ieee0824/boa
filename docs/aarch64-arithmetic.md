# ARM64 arithmetic baseline tier

Related: Omoikane #542, following the [ARM64 foundation](aarch64-jit.md).

The existing loop analysis, hotness, code map, scalar frame, safepoints and deopt
recipes select the architecture emitter. Both backends support the same bounded
safe-integer operations. Other ECMAScript Numbers, including NaN, infinities,
fractions and negative zero, resume the interpreter at the guarded bytecode.
Property regions use the shared shape/IC guards and typed slot buffers described
in [ARM64 properties and helpers](aarch64-properties.md).

## Generated code

The ARM64 leaf reserves x0 for `NativeFrame`, x9 for the i64 register buffer and
x10 for its dirty Number/Boolean tags. x11/x12 hold operands; x13/x14 hold scratch
values and large register offsets. Every completed result is spilled to that
buffer with its type tag. x18, x19-x30, floating registers and SP are untouched.
The ABI probe verifies callee-saved registers and stack alignment around an
actual emitted body; a large-index test covers registers 5000/5001.

Add/subtract/increment check signed overflow and the exact safe-integer bounds.
Multiplication compares `SMULH` with the sign extension of `MUL` before checking
those bounds. Zero times a negative value falls back before storing a result.
Remainder uses `SDIV`/`MSUB`, with zero-divisor and negative-zero checks. Six
comparisons preserve Boolean tags, including through Move. Strict equality and
inequality guard Boolean operands created inside a loop and resume the interpreter
with their original types; false/true must not compare equal to numeric zero/one.
This guard also corrects the x86 emitter. All branches use the
foundation's checked A64 label/fixup resolver.

Backedges decrement the native poll allowance and check the shared iteration
limit. Exhaustion returns an Interrupt deopt to the common VM deadline/budget
logic. Generated code never invokes an allocator or carries an unregistered GC
pointer. The existing deopt materializations commit only completed writes.

## Verification

`cargo test --locked -p boa_engine --features baseline-jit jit:: -- --nocapture`
runs on all four native OS/CPU jobs. Pure arithmetic tests from the x86 tier now
run on ARM64, including the #305 checksum and reduced interpreter dispatch count,
exact bailout PC, Boolean tags, internal branches, iteration limits, deadline
recovery and mixed interpreter exception semantics. A shared JIT on/off corpus
covers safe-integer boundaries, multiplication overflow, NaN, infinity, negative
zero, type changes, remainder signs and all comparisons.

With `BOA_JIT_DIAGNOSTICS=1`, a matched 200,000-iteration #305 measurement reports
CPU, JIT setting, elapsed nanoseconds, checksum and generated entries for each
mode. Timings are observations from that runner; the test asserts semantic and
native-entry correctness without imposing a noisy wall-clock speed threshold.
