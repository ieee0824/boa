use super::*;
use crate::{JsNativeError, JsResult, JsValue, NativeFunction, Source, js_string};

#[test]
fn helper_safepoints_use_the_native_call_return_pc() {
    let mut context = Context::default();
    let script =
        crate::Script::parse(Source::from_bytes("let o={x:42};o.x"), None, &mut context).unwrap();
    let block = script.codeblock(&mut context).unwrap();
    let code = RuntimeCode::compile(&block).unwrap();
    let expected = if cfg!(target_arch = "aarch64") { 16 } else { 9 };
    assert_eq!(code.return_pc, expected);
    assert!(code.entries.len() >= 2);
    assert_eq!(code.descriptor.safepoints().len(), code.entries.len());
    for (bytecode_pc, entry) in &code.entries {
        assert!(code.descriptor.safepoints().iter().any(|point| {
            point.bytecode_offset == *bytecode_pc && point.machine_offset == entry + expected
        }));
    }
}

fn gc_throw(_: &JsValue, args: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    boa_gc::force_minor_collect();
    boa_gc::force_collect();
    Err(JsError::from_opaque(args[0].clone()))
}

fn gc_bridge(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    boa_gc::force_collect();
    args[0]
        .as_object()
        .unwrap()
        .call(&JsValue::undefined(), &[], context)
}

fn native_error(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    boa_gc::force_minor_collect();
    Err(JsNativeError::typ().with_message("native failure").into())
}

fn context(enabled: bool) -> Context {
    type Helper = fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>;
    let mut context = Context::default();
    context.set_baseline_jit_enabled(enabled);
    let helpers: [(&str, Helper); 3] = [
        ("gcThrow", gc_throw),
        ("bridge", gc_bridge),
        ("nativeError", native_error),
    ];
    for (name, function) in helpers {
        context
            .register_global_builtin_callable(
                js_string!(name),
                1,
                NativeFunction::from_fn_ptr(function),
            )
            .unwrap();
    }
    context
}

fn evaluate(source: &str, enabled: bool) -> (Result<JsValue, String>, JitExceptionDiagnostics) {
    let mut context = context(enabled);
    // Snapshot the host-facing error before the next differential run forces
    // GC. A raw opaque JsError is not a registered host root across that run.
    let result = context
        .eval(Source::from_reader(
            source.as_bytes(),
            Some(std::path::Path::new("exception.js")),
        ))
        .map_err(|error| error.to_string());
    let diagnostics = context.jit_exception_diagnostics();
    assert_eq!(diagnostics.active_frames, 0);
    assert_eq!(
        context.eval(Source::from_bytes("21*2")).unwrap(),
        JsValue::from(42)
    );
    (result, diagnostics)
}

#[test]
fn nested_handlers_restore_lexical_environments_and_finally_once() {
    const SOURCE: &str = r#"
        function f(fail) {
            let log = ''; let outer = 11;
            try {
                let outer = 22;
                try { let outer = 33; if (fail) throw outer; log += outer; }
                catch (e) { log += 'c'+e+':'+outer; throw e+1; }
                finally { log += 'i'+outer; }
            } catch (e) { log += 'o'+e+':'+outer; }
            finally { log += 'f'+outer; }
            return log;
        }
        for (let i=0;i<40;i++) f(false);
        f(true)
    "#;
    let (expected, _) = evaluate(SOURCE, false);
    let (actual, diagnostics) = evaluate(SOURCE, true);
    assert_eq!(actual.unwrap(), expected.unwrap());
    assert!(diagnostics.generated_entries > 0);
    assert!(diagnostics.handler_entries >= 2);
    assert!(diagnostics.exception_unwinds >= 2);
}

#[test]
fn mixed_native_reentry_preserves_opaque_value_across_gc_and_rethrow() {
    const SOURCE: &str = r#"
        let log = ''; let payload = {answer:42};
        function inner(fail) { if (fail) gcThrow(payload); return 1; }
        function middle(fail) {
            try { return bridge(()=>inner(fail)); }
            finally { if(fail) log += 'middle'; }
        }
        function outer(fail) {
            try { return middle(fail); }
            catch(e) { if(e!==payload || e.answer!==42) throw 'lost root'; log+='catch'; throw e; }
            finally { if(fail) log+='outer'; }
        }
        for(let i=0;i<40;i++) outer(false);
        try { outer(true); } catch(e) { log += ':'+(e===payload)+':'+e.answer; }
        log
    "#;
    let (expected, _) = evaluate(SOURCE, false);
    let (actual, diagnostics) = evaluate(SOURCE, true);
    assert_eq!(actual.unwrap(), expected.unwrap());
    assert!(diagnostics.maximum_active_frames >= 2, "{diagnostics:?}");
    assert!(diagnostics.unwound_frames >= 1, "{diagnostics:?}");
    assert!(diagnostics.handler_entries >= 1, "{diagnostics:?}");
}

#[test]
fn getter_errors_and_native_errors_keep_the_same_host_stack_trace() {
    for operation in ["nativeError()", "o.x", "throw new Error('explicit')"] {
        let source = format!(
            r#"
            let o={{get x(){{nativeError()}}}};
            function inner(fail) {{ if(fail) {{ {operation}; }} return 1; }}
            function middle(fail) {{ return bridge(()=>inner(fail)); }}
            function outer(fail) {{ return middle(fail); }}
            for(let i=0;i<40;i++) outer(false);
            outer(true)
        "#
        );
        let (expected, _) = evaluate(&source, false);
        let (actual, diagnostics) = evaluate(&source, true);
        let actual = actual.unwrap_err();
        assert_eq!(actual, expected.unwrap_err(), "{operation}");
        assert!(
            actual.contains("exception.js:"),
            "missing source location: {actual}"
        );
        for function in ["inner", "middle", "outer", "bridge"] {
            assert!(actual.contains(function), "missing {function}: {actual}");
        }
        assert!(
            diagnostics.exception_unwinds > 0,
            "{operation}: {diagnostics:?}"
        );
        assert!(diagnostics.unwound_frames > 0);
    }
}

#[test]
fn interpreter_handler_between_generated_helpers_is_not_skipped() {
    const SOURCE: &str = r#"
        function compiled(fail) { if(fail) gcThrow(17); return 2; }
        for(let i=0;i<40;i++) compiled(false);
        function cold() { try { compiled(true); } catch(e) { return e+3; } }
        function warm(fail) { try { return bridge(fail ? cold : ()=>1); } catch(e) { return 'wrong'; } }
        for(let i=0;i<40;i++) warm(false);
        warm(true)
    "#;
    let (expected, _) = evaluate(SOURCE, false);
    let (actual, diagnostics) = evaluate(SOURCE, true);
    assert_eq!(actual.unwrap(), JsValue::from(20));
    assert_eq!(expected.unwrap(), JsValue::from(20));
    assert!(diagnostics.maximum_active_frames >= 2);
    assert!(diagnostics.unwound_frames > 0);
}

#[test]
fn runtime_limit_bypasses_catch_and_leaves_no_generated_frames() {
    for enabled in [false, true] {
        let mut context = context(enabled);
        context.eval(Source::from_bytes("function f(n){let s=0;for(let i=0;i<n;i++)s+=i;return s}for(let i=0;i<40;i++)f(40)")).unwrap();
        context.runtime_limits_mut().set_loop_iteration_limit(50);
        let error = context
            .eval(Source::from_bytes(
                "bridge(()=>{try {f(1000)} catch(e) {return 'wrong'}})",
            ))
            .unwrap_err();
        assert!(error.as_native().unwrap().is_runtime_limit());
        assert_eq!(context.jit_exception_diagnostics().active_frames, 0);
        assert_eq!(
            context.eval(Source::from_bytes("6*7")).unwrap(),
            JsValue::from(42)
        );
    }
}

#[test]
fn rust_panic_never_unwinds_through_generated_code() {
    #[expect(
        clippy::unnecessary_wraps,
        reason = "NativeFunction callbacks use the common JsResult return channel"
    )]
    fn panic_helper(_: &JsValue, args: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
        assert!(!args[0].as_boolean().unwrap(), "injected helper panic");
        Ok(JsValue::undefined())
    }
    let mut context = context(true);
    context
        .register_global_builtin_callable(
            js_string!("panicHelper"),
            1,
            NativeFunction::from_fn_ptr(panic_helper),
        )
        .unwrap();
    context
        .eval(Source::from_bytes(
            "function f(x){panicHelper(x)}for(let i=0;i<40;i++)f(false)",
        ))
        .unwrap();
    assert!(
        catch_unwind(AssertUnwindSafe(
            || context.eval(Source::from_bytes("f(true)"))
        ))
        .is_err()
    );
    let diagnostics = context.jit_exception_diagnostics();
    assert!(diagnostics.generated_entries > 0);
    assert_eq!(diagnostics.active_frames, 0);
}

#[test]
fn allocation_failure_is_caught_after_active_generated_unwind() {
    fn allocate(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        let mut runtime = crate::jit::JitRuntimeCall::new(0, 0)
            .map_err(crate::jit::RuntimeCallError::into_js_error)?;
        if args[0].as_boolean().unwrap() {
            runtime.set_allocation_budget(Some(0));
        }
        let result = runtime.allocate_or_throw(crate::jit::JitAllocationKind::Object, &[], context);
        if result.is_err() {
            assert_eq!(runtime.diagnostics().exception_unwinds, 1);
            assert_eq!(runtime.diagnostics().unwound_frames, 1);
        }
        result
    }
    let mut results = Vec::new();
    for enabled in [false, true] {
        let mut context = context(enabled);
        context
            .register_global_builtin_callable(
                js_string!("allocate"),
                1,
                NativeFunction::from_fn_ptr(allocate),
            )
            .unwrap();
        results.push(
            context
                .eval(Source::from_bytes(
                    r#"
            function f(fail) {
                let log='';
                try { allocate(fail); }
                catch(e) { log=e.name+':'+e.message; }
                finally { log+=':finally'; }
                return log;
            }
            for(let i=0;i<40;i++)f(false);
            f(true)
        "#,
                ))
                .unwrap(),
        );
        let diagnostics = context.jit_exception_diagnostics();
        assert_eq!(diagnostics.active_frames, 0);
        if enabled {
            assert!(diagnostics.handler_entries > 0);
        }
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(
        results[0],
        JsValue::from(js_string!(
            "RangeError:JIT allocation budget exhausted:finally"
        ))
    );
}

#[test]
fn recursive_compilation_keeps_the_active_callers_code_and_handler_alive() {
    fn evict(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
        if !args[0].as_boolean().unwrap() {
            return Ok(JsValue::undefined());
        }
        for _ in 0..270 {
            context.eval(Source::from_bytes(
                "function temporary(){return ({x:42}).x}for(let i=0;i<10;i++)temporary()",
            ))?;
        }
        boa_gc::force_collect();
        Err(JsNativeError::typ()
            .with_message("after cache eviction")
            .into())
    }
    let mut context = context(true);
    context
        .register_global_builtin_callable(
            js_string!("evict"),
            1,
            NativeFunction::from_fn_ptr(evict),
        )
        .unwrap();
    let value = context
        .eval(Source::from_bytes(
            r#"
        function outer(fail) {
            try { evict(fail); }
            catch(e) { return e.message; }
            finally { globalThis.finalized = fail; }
        }
        for(let i=0;i<40;i++)outer(false);
        outer(true)+':'+globalThis.finalized
    "#,
        ))
        .unwrap();
    assert_eq!(
        value,
        JsValue::from(js_string!("after cache eviction:true"))
    );
    let diagnostics = context.jit_exception_diagnostics();
    assert!(diagnostics.handler_entries > 0);
    assert_eq!(diagnostics.active_frames, 0);
}
