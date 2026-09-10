# Parser stack budget

The parser requires at least 2 MiB of available native stack at its public entry
points, including in unoptimized builds. It does not query or resize the host
thread's stack. An embedding application that has already consumed substantial
stack must account for that before calling the parser.

Every grammar production enters through `TokenParser::parse`. The wrapper checks
both the native stack distance from its outermost entry and the number of active
grammar productions. Parsing returns a normal `boa_parser::Error` with a message starting
with `parser recursion limit exceeded` when either limit is reached:

- 1.5 MiB of parser stack usage;
- 4096 simultaneous grammar productions.

The diagnostic includes the measured byte count and active grammar depth.

The remaining 512 KiB within the minimum host stack is reserved for the next
production, lexer work, error propagation and cleanup. On x86-64 and AArch64,
the measurement reads the native stack pointer directly. ASAN may relocate
address-taken locals to its fake stack, so local-variable addresses cannot measure
native stack consumption in that configuration (#640). Other architectures and
Miri retain the existing local-marker fallback; fake-stack support is verified
only on x86-64 and AArch64. Sampled addresses are compared as integers and never
dereferenced. Both growth directions are handled. Each outermost parse establishes
a new baseline, and ordinary errors restore the depth counter while propagating
outward.

These limits describe recursive parser work, not source length, the number of
statements, or the JavaScript VM's execution stack. Iterative expression chains
and flat lists remain supported. Accepted syntax nesting depends on the compiler,
profile and target; 4096 grammar productions is not 4096 JavaScript function levels.
The parser does not promise recovery from an embedding application entering with
less than the required available stack.

## Regression and frame sizes

Omoikane issue [#633](https://github.com/ieee0824/omoikane/issues/633) reproduces with
Test262 `test/staging/sm/regress/regress-672893.js`, which has valid nested function
expressions and a call inside the innermost body. Test262 is pinned by
`test262_config.toml`.

On native aarch64 Linux with Rust 1.98.0, Boa `4a708378` in the unoptimized dev
profile aborts on a 2 MiB stack and passes on an 8 MiB stack. A parser-only
reproduction also aborts at 2 MiB. Debugger measurements between consecutive
function-expression frames show 125360 bytes per nesting level before the fix
and 86064 bytes after the split paths and common guard were added. These are
measurements of this build, not portable layout guarantees.

The member/assignment/update/conditional and operator-precedence parsers now
keep their post-operand work in separate functions. Their temporary AST values
no longer remain on the stack while the initial operand recursively parses a
function body. These post-operand helpers are kept out of line even in optimized
builds: inlining them back into the recursive path can recreate large frames.
The release tests also retain 100 nested arrays and parentheses on both stack
sizes, which caught that regression in native x86_64 and aarch64 CI. Grammar,
precedence, public AST types and dependency versions are unchanged.

With ASAN enabled, relational, primary and left-hand-side parsing also keep
larger alternative paths and post-operand work out of active recursive frames.
Parenthesized expressions separate their comma/rest handling and final parameter
validation. This preserves the same 100-level release regressions with fake stack
enabled without increasing either budget or reducing the test inputs. Actual
frame sizes remain compiler- and instrumentation-dependent.

## Reproduce the checks

```sh
cargo test --locked -p boa_parser --profile dev --test parser_stack -- --nocapture
cargo test --locked -p boa_parser --profile release --test parser_stack -- --nocapture
cargo build --locked --profile dev --bin boa_tester
(
  ulimit -c 0
  ulimit -s 2048
  target/debug/boa_tester run --test262-path /path/to/test262 \
    --suite test/staging/sm/regress/regress-672893.js --disable-parallelism -vv
)
```

Repeat the original Test262 case with `ulimit -s 8192`, and with a release build.
Do not replace the dev profile with an optimized test profile: this repository's
normal test profile has `opt-level = 1`.

The regression tests create threads with explicit 2 MiB and 8 MiB stacks rather
than relying on `RUST_MIN_STACK`. They cover the original valid nested form,
UTF-8/UTF-16/reader input, script/module/eval/function-body/parameter entry points,
long iterative input, and deeply recursive functions, parentheses, arrays,
blocks, unary operators, assignments, `new`, arrows, templates, conditionals,
exponentiation, logical OR, call arguments, computed members and object literals.
They drop accepted ASTs and parse again after resource-limit errors.

`.github/workflows/parser-stack.yml` separates x86_64/aarch64 and dev/release
jobs. Every job runs both explicit thread stack sizes and the original Test262
case with both main-thread stack sizes, and uploads its environment, tested
revision, pinned Test262 revision and complete logs even on failure. Since the
single-file Test262 runner also returns exit status zero for a test assertion
failure, the workflow explicitly requires one `Passed` outcome for each size;
a successful process exit alone is not accepted as a conformance result.

The additional `.github/workflows/parser-asan.yml` records explicit
`detect_stack_use_after_return=1` and tests the same parser regressions plus the
original #640 Function-constructor calibration on native x86-64 and AArch64,
with dev/release profiles and both 2 MiB and 8 MiB stacks. The shallow regression
covers all public parse entry points and input representations. The pinned
calibration must actually pass; an exit status without a matching test report
is insufficient. The diagnostic settings and exact revisions accompany every
artifact.
