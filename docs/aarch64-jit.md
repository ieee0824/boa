# ARM64 JIT foundation

The `baseline-jit` feature exposes the same fixed-return code cache and generated
runtime-call boundary on Linux aarch64 and macOS arm64. It does not yet enable
ARM64 arithmetic or property lowering in the JavaScript VM. Those are separate
Gate 5 tasks; unsupported VM compilation continues to use the interpreter.

## ABI and frame metadata

The generated stubs follow [AAPCS64](https://github.com/ARM-software/abi-aa/blob/main/aapcs64/aapcs64.rst)
and [Apple's arm64 ABI](https://developer.apple.com/documentation/xcode/writing-arm64-code-for-apple-platforms).
They use x0 for the first pointer argument or integer result, reserve x18, and
keep the stack aligned to 16 bytes. The runtime-call stub saves x29/x30, links
the frame pointer, loads its fixed helper through x16, calls it, restores the
frame record and returns. It does not use the red zone. The callee preserves
x19-x29 and the low 64 bits of v8-v15. No variadic call or stack-passed argument
is part of this fixed helper ABI.

`JitFrameDescriptor::register_map()` identifies the architecture's general-purpose
register numbering. ARM64 uses x0-x30; x18 and the stack pointer cannot be GC-value
locations. x86_64 keeps its DWARF GPR numbers. Runtime-call GC roots continue to
use the engine-owned spilled `FrameRegister` slots. Introducing an architecture
register map does not make the collector scan arbitrary machine registers.

The assembler returns the actual post-call offset to the common safepoint table:
16 bytes for ARM64 and 9 for the existing AMD64 sequence. Nested runtime calls,
allocation, collection and exception cleanup use the same logical frame chain.
Rust panics are caught inside the helper boundary and never unwind through the
generated frame.

## Instructions and code lifetime

The ARM64 emitter writes little-endian 32-bit instructions. It resolves labels
relative to the instruction PC, checks signed branch/literal ranges and rejects
unbound or duplicate labels. A deduplicated u64 literal pool is aligned to eight
bytes after the terminating return/branch. The foundation disassembler prints
the emitted instruction subset and referenced literals; unknown instructions
remain explicit `.inst` records.

Linux allocates private RW pages, publishes them as RX with `mprotect`, and calls
the compiler runtime's `__clear_cache` for the emitted range. The allocation's
owner calls `munmap` when retired, including after a failed publication.

On macOS arm64, the feature requires macOS 11.4 or later. It uses `MAP_JIT` with
[`pthread_jit_write_with_callback_np`](https://github.com/apple-oss-distributions/libpthread/blob/main/include/pthread/pthread.h)
and a statically registered callback. Only that callback copies bounded bytes;
the OS restores executable permission before returning. Publication then calls
[`sys_icache_invalidate`](https://developer.apple.com/documentation/apple-silicon/porting-just-in-time-compilers-to-apple-silicon).
When `pthread_jit_write_protect_supported_np()` reports support, write and execute
permissions are exclusive for the current thread. ARM64 virtual machines may lack
that facility: libpthread then invokes the callback without changing permissions.
In that case the allocator removes execute permission before writing code and
publishes with `mprotect(PROT_READ | PROT_EXEC)` before flushing the instruction
cache. A failed transition returns an OS error and releases the mapping. The same
write-rejection test must pass on either path. Published code is immutable through
this API, and mappings remain local to their owning runtime/thread.

The current allocator owns one mapping per code object. Normal/ad-hoc signed
macOS binaries are the supported execution environment. Hardened Runtime
configurations that restrict the process to a single `MAP_JIT` region require
a shared arena allocator and are not supported by this allocator; allocation
errors propagate instead of pretending compilation succeeded. A host using the
optional late-freeze callback entitlement must follow Apple's callback-freeze
contract before invoking generated-code allocation. The feature does not alter
host signing policy or entitlements.

## Verification

```sh
cargo test --locked -p boa_engine --features baseline-jit jit:: -- --nocapture
```

Tests cover literal alignment/deduplication, forward/backward/conditional fixups,
range edges, invalid labels/registers, actual fixed and runtime calls, all
callee-saved integer and floating registers, stack alignment and repeated code
publication with changing instructions/literals. Isolated child processes also
verify that published code rejects writes and retired code pages are unmapped. Shared frame/GC/exception/runtime tests also run on ARM64.

The native CI jobs run on Linux x86_64/aarch64 and macOS Intel/ARM64. They retain
the tested revision, architecture, compiler and complete test output. With
`BOA_JIT_DIAGNOSTICS=1`, the ABI probes also emit code and disassembly to that log.
Cross-compilation alone is not an execution result.
