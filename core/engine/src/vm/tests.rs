use crate::vm::CallFrame;
use crate::vm::call_frame::CallFrameLocation;
use crate::vm::source_info::SourcePath;
use crate::{
    Context, JsNativeError, JsNativeErrorKind, JsValue, NativeFunction, Script, TestAction,
    js_string,
    native_function::{NativeCallAlreadyResumed, NativeCallSuspension},
    object::{FunctionObjectBuilder, ObjectInitializer},
    property::{Attribute, PropertyDescriptor},
    run_test_actions, run_test_actions_with,
};
use boa_ast::Position;
use boa_gc::{Gc, GcRefCell};
use boa_macros::js_str;
use boa_parser::Source;
use futures_lite::future;
use indoc::indoc;

fn suspending_function(slot: Gc<GcRefCell<Option<NativeCallSuspension>>>) -> NativeFunction {
    NativeFunction::from_copy_closure_with_captures(
        |_, _, slot, context| {
            let suspension = context.suspend_native_call()?;
            *slot.borrow_mut() = Some(suspension);
            Ok(JsValue::undefined())
        },
        slot,
    )
}

#[test]
fn async_evaluation_resumes_a_native_call_exactly_once() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    let after = Gc::new(GcRefCell::new(false));
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    context
        .register_global_callable(
            js_string!("after"),
            0,
            NativeFunction::from_copy_closure_with_captures(
                |_, _, after, _| {
                    *after.borrow_mut() = true;
                    Ok(JsValue::undefined())
                },
                after.clone(),
            ),
        )
        .unwrap();
    let script = Script::parse(
        Source::from_bytes("const value = suspend(); after(); value + 1"),
        None,
        &mut context,
    )
    .unwrap();
    let mut evaluation = Box::pin(script.evaluate_async(&mut context));

    assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
    assert!(!*after.borrow());
    let suspension = slot.borrow().clone().unwrap();
    assert_eq!(suspension.resume(Ok(JsValue::from(41))), Ok(()));
    assert_eq!(
        suspension.resume(Ok(JsValue::from(99))),
        Err(NativeCallAlreadyResumed)
    );

    assert_eq!(future::block_on(evaluation).unwrap(), JsValue::from(42));
    assert!(*after.borrow());
}

#[test]
fn async_object_call_propagates_native_suspension() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    context
        .eval(Source::from_bytes(
            "globalThis.listener = value => suspend() + value",
        ))
        .unwrap();
    let listener = context
        .global_object()
        .get(js_string!("listener"), &mut context)
        .unwrap()
        .as_object()
        .unwrap()
        .clone();
    let this = JsValue::undefined();
    let args = [JsValue::from(1)];
    let mut call = Box::pin(listener.call_async(&this, &args, &mut context));

    assert!(future::block_on(future::poll_once(call.as_mut())).is_none());
    slot.borrow()
        .as_ref()
        .unwrap()
        .resume(Ok(JsValue::from(41)))
        .unwrap();

    assert_eq!(future::block_on(call).unwrap(), JsValue::from(42));
}

#[test]
fn native_call_resume_error_is_thrown_at_the_original_call_site() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    let script = Script::parse(
        Source::from_bytes("try { suspend(); 'not reached' } catch (error) { error.message }"),
        None,
        &mut context,
    )
    .unwrap();
    let mut evaluation = Box::pin(script.evaluate_async(&mut context));

    assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
    slot.borrow()
        .as_ref()
        .unwrap()
        .resume(Err(JsNativeError::error().with_message("dismissed").into()))
        .unwrap();

    assert_eq!(
        future::block_on(evaluation).unwrap(),
        JsValue::from(js_string!("dismissed"))
    );
}

#[test]
fn native_accessor_suspension_replaces_its_result_register() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    let getter =
        FunctionObjectBuilder::new(context.realm(), suspending_function(slot.clone())).build();
    let target = ObjectInitializer::new(&mut context).build();
    target
        .define_property_or_throw(
            js_string!("value"),
            PropertyDescriptor::builder()
                .get(getter)
                .enumerable(true)
                .configurable(true),
            &mut context,
        )
        .unwrap();
    context
        .register_global_property(js_string!("target"), target, Attribute::all())
        .unwrap();
    let script = Script::parse(Source::from_bytes("target.value + 1"), None, &mut context).unwrap();
    let mut evaluation = Box::pin(script.evaluate_async(&mut context));

    assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
    slot.borrow()
        .as_ref()
        .unwrap()
        .resume(Ok(JsValue::from(41)))
        .unwrap();

    assert_eq!(future::block_on(evaluation).unwrap(), JsValue::from(42));
}

#[test]
fn function_prototype_call_preserves_native_call_suspension() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    let script = Script::parse(
        Source::from_bytes("suspend.call(null) + 1"),
        None,
        &mut context,
    )
    .unwrap();
    let mut evaluation = Box::pin(script.evaluate_async(&mut context));

    assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
    slot.borrow()
        .as_ref()
        .unwrap()
        .resume(Ok(JsValue::from(41)))
        .unwrap();

    assert_eq!(future::block_on(evaluation).unwrap(), JsValue::from(42));
}

#[test]
fn reentrant_native_call_restores_the_outer_suspension_origin() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    context
        .register_global_builtin_callable(
            js_string!("innerNative"),
            0,
            NativeFunction::from_copy_closure(|_, _, _| Ok(JsValue::undefined())),
        )
        .unwrap();
    context
        .register_global_builtin_callable(
            js_string!("outerNative"),
            1,
            NativeFunction::from_copy_closure_with_captures(
                |_, args, slot, context| {
                    args[0]
                        .as_callable()
                        .expect("the test passes a callback")
                        .call(&JsValue::undefined(), &[], context)?;
                    let suspension = context.suspend_native_call()?;
                    *slot.borrow_mut() = Some(suspension);
                    Ok(JsValue::undefined())
                },
                slot.clone(),
            ),
        )
        .unwrap();
    let script = Script::parse(
        Source::from_bytes("outerNative(() => innerNative()) + 1"),
        None,
        &mut context,
    )
    .unwrap();
    let mut evaluation = Box::pin(script.evaluate_async(&mut context));

    assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
    slot.borrow()
        .as_ref()
        .unwrap()
        .resume(Ok(JsValue::from(41)))
        .unwrap();

    assert_eq!(future::block_on(evaluation).unwrap(), JsValue::from(42));
}

#[test]
fn suspension_is_rejected_if_the_opcode_consumes_the_native_result() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    let script = Script::parse(
        Source::from_bytes("const proxy = new Proxy({}, { has: suspend }); 'key' in proxy"),
        None,
        &mut context,
    )
    .unwrap();

    let error = future::block_on(script.evaluate_async(&mut context)).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("native call cannot suspend from this execution path")
    );
    assert_eq!(
        slot.borrow()
            .as_ref()
            .unwrap()
            .resume(Ok(JsValue::undefined())),
        Err(NativeCallAlreadyResumed)
    );
}

#[test]
fn suspended_native_call_roots_its_resumed_object_until_evaluation_continues() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    let resumed_object = ObjectInitializer::new(&mut context)
        .property(js_string!("answer"), 42, Attribute::all())
        .build();
    let script = Script::parse(Source::from_bytes("suspend().answer"), None, &mut context).unwrap();
    let mut evaluation = Box::pin(script.evaluate_async(&mut context));

    assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
    slot.borrow()
        .as_ref()
        .unwrap()
        .resume(Ok(resumed_object.into()))
        .unwrap();
    boa_gc::force_collect();

    assert_eq!(future::block_on(evaluation).unwrap(), JsValue::from(42));
}

#[test]
fn synchronous_evaluation_rejects_an_unresolved_native_call_suspension() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();

    let error = context.eval(Source::from_bytes("suspend()")).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("native call suspension requires asynchronous script evaluation")
    );
    assert_eq!(
        slot.borrow()
            .as_ref()
            .unwrap()
            .resume(Ok(JsValue::undefined())),
        Err(NativeCallAlreadyResumed)
    );
}

#[test]
fn synchronous_evaluation_accepts_an_immediately_resumed_native_call() {
    let mut context = Context::default();
    context
        .register_global_callable(
            js_string!("resume_immediately"),
            0,
            NativeFunction::from_copy_closure(|_, _, context| {
                context
                    .suspend_native_call()?
                    .resume(Ok(JsValue::from(41)))
                    .expect("new suspension must accept its first result");
                Ok(JsValue::undefined())
            }),
        )
        .unwrap();

    assert_eq!(
        context
            .eval(Source::from_bytes("resume_immediately() + 1"))
            .unwrap(),
        JsValue::from(42)
    );
}

#[test]
fn dropping_suspended_evaluation_cancels_the_handle_and_restores_the_context() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    let script = Script::parse(Source::from_bytes("suspend() + 1"), None, &mut context).unwrap();
    let mut evaluation = Box::pin(script.evaluate_async(&mut context));

    assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
    drop(evaluation);

    assert_eq!(
        slot.borrow()
            .as_ref()
            .unwrap()
            .resume(Ok(JsValue::from(41))),
        Err(NativeCallAlreadyResumed)
    );
    assert_eq!(
        context.eval(Source::from_bytes("1 + 1")).unwrap(),
        JsValue::from(2)
    );
}

#[test]
fn throwing_native_function_cancels_its_requested_suspension() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    context
        .register_global_callable(
            js_string!("invalid_suspend"),
            0,
            NativeFunction::from_copy_closure_with_captures(
                |_, _, slot, context| {
                    let suspension = context.suspend_native_call()?;
                    *slot.borrow_mut() = Some(suspension);
                    Err(JsNativeError::error().with_message("native failed").into())
                },
                slot.clone(),
            ),
        )
        .unwrap();
    let script =
        Script::parse(Source::from_bytes("invalid_suspend()"), None, &mut context).unwrap();

    let error = future::block_on(script.evaluate_async(&mut context)).unwrap_err();

    assert!(error.to_string().contains("native failed"));
    assert_eq!(
        slot.borrow()
            .as_ref()
            .unwrap()
            .resume(Ok(JsValue::undefined())),
        Err(NativeCallAlreadyResumed)
    );
}

#[test]
fn native_constructor_cannot_suspend() {
    let mut context = Context::default();
    context
        .register_global_builtin_callable(
            js_string!("innerNative"),
            0,
            NativeFunction::from_copy_closure(|_, _, _| Ok(JsValue::undefined())),
        )
        .unwrap();
    context
        .register_global_callable(
            js_string!("SuspendingConstructor"),
            1,
            NativeFunction::from_copy_closure(|_, args, context| {
                args[0]
                    .as_callable()
                    .expect("the test passes a callback")
                    .call(&JsValue::undefined(), &[], context)?;
                context.suspend_native_call()?;
                Ok(JsValue::undefined())
            }),
        )
        .unwrap();

    let error = context
        .eval(Source::from_bytes(
            "new SuspendingConstructor(() => innerNative())",
        ))
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("native constructors cannot suspend")
    );
}

#[test]
fn typeof_string() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            const a = "hello";
            typeof a;
        "#},
        js_str!("string"),
    )]);
}

#[test]
fn typeof_number() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            let a = 1234;
            typeof a;
        "#},
        js_str!("number"),
    )]);
}

#[test]
fn basic_op() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            const a = 1;
            const b = 2;
            a + b
        "#},
        3,
    )]);
}

#[test]
fn position() {
    let context = &mut Context::default();
    context
        .register_global_callable(
            js_string!("check_stack"),
            2,
            NativeFunction::from_copy_closure(|_, _, context| {
                let frame = context.stack_trace().collect::<Vec<&CallFrame>>();

                assert_eq!(frame.len(), 4);
                assert_eq!(
                    frame[0].position(),
                    CallFrameLocation {
                        function_name: js_string!("myOtherFunction"),
                        path: SourcePath::None,
                        position: Some(Position::new(2, 16))
                    }
                );
                assert_eq!(
                    frame[1].position(),
                    CallFrameLocation {
                        function_name: js_string!("<eval>"),
                        path: SourcePath::Eval,
                        position: Some(Position::new(1, 16))
                    }
                );
                assert_eq!(
                    frame[2].position(),
                    CallFrameLocation {
                        function_name: js_string!("myFunction"),
                        path: SourcePath::None,
                        position: Some(Position::new(5, 9))
                    }
                );
                assert_eq!(
                    frame[3].position(),
                    CallFrameLocation {
                        function_name: js_string!("<main>"),
                        path: SourcePath::None,
                        position: Some(Position::new(8, 11))
                    }
                );
                Ok(JsValue::undefined())
            }),
        )
        .expect("Could not register function");
    run_test_actions_with(
        [TestAction::run(indoc! {r#"
            const myOtherFunction = () => {
                check_stack();
            };
            function myFunction() {
                eval("myOtherFunction()");
            }

            myFunction();
        "#})],
        context,
    );
}

#[test]
fn try_catch_finally_from_init() {
    // the initialisation of the array here emits a PopOnReturnAdd op
    //
    // here we test that the stack is not popped more than intended due to multiple catches in the
    // same function, which could lead to VM stack corruption
    run_test_actions([TestAction::assert_opaque_error(
        indoc! {r#"
            try {
                [(() => {throw "h";})()];
            } catch (x) {
                throw "h";
            } finally {
            }
        "#},
        js_str!("h"),
    )]);
}

#[test]
fn multiple_catches() {
    // see explanation on `try_catch_finally_from_init`
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            try {
                try {
                    [(() => {throw "h";})()];
                } catch (x) {
                    throw "h";
                }
            } catch (y) {
            }
        "#},
        JsValue::undefined(),
    )]);
}

#[test]
fn use_last_expr_try_block() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            try {
                19;
                7.5;
                "Hello!";
            } catch (y) {
                14;
                "Bye!"
            }
        "#},
        js_str!("Hello!"),
    )]);
}

#[test]
fn use_last_expr_catch_block() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            try {
                throw Error("generic error");
                19;
                7.5;
            } catch (y) {
                14;
                "Hello!";
            }
        "#},
        js_str!("Hello!"),
    )]);
}

#[test]
fn no_use_last_expr_finally_block() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            try {
            } catch (y) {
            } finally {
                "Unused";
            }
        "#},
        JsValue::undefined(),
    )]);
}

#[test]
fn finally_block_binding_env() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            let buf = "Hey hey";
            try {
            } catch (y) {
            } finally {
                let x = " people";
                buf += x;
            }
            buf
        "#},
        js_str!("Hey hey people"),
    )]);
}

#[test]
fn run_super_method_in_object() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            let proto = {
                m() { return "super"; }
            };
            let obj = {
                v() { return super.m(); }
            };
            Object.setPrototypeOf(obj, proto);
            obj.v();
        "#},
        js_str!("super"),
    )]);
}

#[test]
fn get_reference_by_super() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            var fromA, fromB;
            var A = { fromA: 'a', fromB: 'a' };
            var B = { fromB: 'b' };
            Object.setPrototypeOf(B, A);
            var obj = {
                fromA: 'c',
                fromB: 'c',
                method() {
                    fromA = (() => { return super.fromA; })();
                    fromB = (() => { return super.fromB; })();
                }
            };
            Object.setPrototypeOf(obj, B);
            obj.method();
            fromA + fromB
        "#},
        js_str!("ab"),
    )]);
}

#[test]
fn super_call_constructor_null() {
    run_test_actions([TestAction::assert_native_error(
        indoc! {r#"
            class A extends Object {
                constructor() {
                    Object.setPrototypeOf(A, null);
                    super(A);
                }
            }
            new A();
        "#},
        JsNativeErrorKind::Type,
        "super constructor object must be constructor",
    )]);
}

#[test]
fn super_call_get_constructor_before_arguments_execution() {
    run_test_actions([TestAction::assert(indoc! {r#"
        class A extends Object {
            constructor() {
                super(Object.setPrototypeOf(A, null));
            }
        }
        new A() instanceof A;
    "#})]);
}

#[test]
fn order_of_execution_in_assigment() {
    run_test_actions([
        TestAction::run(indoc! {r#"
                let i = 0;
                let array = [[]];

                array[i++][i++] = i++;
            "#}),
        TestAction::assert_eq("i", 3),
        TestAction::assert_eq("array.length", 1),
        TestAction::assert_eq("array[0].length", 2),
    ]);
}

#[test]
fn order_of_execution_in_assigment_with_comma_expressions() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            let result = "";
            function f(i) {
                result += i;
            }
            let a = [[]];
            (f(1), a)[(f(2), 0)][(f(3), 0)] = (f(4), 123);
            result
        "#},
        js_str!("1234"),
    )]);
}

#[test]
fn loop_runtime_limit() {
    run_test_actions([
        TestAction::assert_eq(
            indoc! {r#"
                for (let i = 0; i < 20; ++i) { }
            "#},
            JsValue::undefined(),
        ),
        TestAction::inspect_context(|context| {
            context.runtime_limits_mut().set_loop_iteration_limit(10);
        }),
        TestAction::assert_native_error(
            indoc! {r#"
                for (let i = 0; i < 20; ++i) { }
            "#},
            JsNativeErrorKind::RuntimeLimit,
            "Maximum loop iteration limit 10 exceeded",
        ),
        TestAction::assert_eq(
            indoc! {r#"
                for (let i = 0; i < 10; ++i) { }
            "#},
            JsValue::undefined(),
        ),
        TestAction::assert_native_error(
            indoc! {r#"
                while (1) { }
            "#},
            JsNativeErrorKind::RuntimeLimit,
            "Maximum loop iteration limit 10 exceeded",
        ),
    ]);
}

#[test]
fn loop_runtime_limit_escapes_promise_constructor() {
    let mut context = Context::default();
    context.runtime_limits_mut().set_loop_iteration_limit(10);

    let error = context
        .eval(Source::from_bytes(
            "new Promise(() => { for (let i = 0; i < 1_000; ++i) {} })",
        ))
        .expect_err("the runtime limit must escape the Promise constructor");

    assert!(
        error
            .as_native()
            .is_some_and(JsNativeError::is_runtime_limit)
    );
}

#[test]
fn runtime_limit_can_be_materialized_without_panicking() {
    let mut context = Context::default();
    let error = JsNativeError::runtime_limit().with_message("loop limit exceeded");

    let opaque = error.to_opaque(&mut context);

    assert_eq!(
        opaque
            .get(js_string!("message"), &mut context)
            .expect("the error message must be readable"),
        js_string!("loop limit exceeded").into()
    );
}

#[test]
fn recursion_runtime_limit() {
    run_test_actions([
        TestAction::run(indoc! {r#"
            function factorial(n) {
                if (n == 0) {
                    return 1;
                }

                return n * factorial(n - 1);
            }
        "#}),
        TestAction::assert_eq("factorial(8)", JsValue::new(40_320)),
        TestAction::assert_eq("factorial(11)", JsValue::new(39_916_800)),
        TestAction::inspect_context(|context| {
            context.runtime_limits_mut().set_recursion_limit(10);
        }),
        TestAction::assert_native_error(
            "factorial(11)",
            JsNativeErrorKind::RuntimeLimit,
            "exceeded maximum number of recursive calls",
        ),
        TestAction::assert_eq("factorial(8)", JsValue::new(40_320)),
        TestAction::assert_native_error(
            indoc! {r#"
                function x() {
                    x()
                }

                x()
            "#},
            JsNativeErrorKind::RuntimeLimit,
            "exceeded maximum number of recursive calls",
        ),
    ]);
}

#[test]
fn arguments_object_constructor_valid_index() {
    run_test_actions([TestAction::assert_eq(
        indoc! {r#"
            let args;
            function F(a = 1) {
                args = arguments;
            }
            new F();
            typeof args
        "#},
        js_str!("object"),
    )]);
}

#[test]
fn empty_return_values() {
    run_test_actions([
        TestAction::run(indoc! {r#"do {{}} while (false);"#}),
        TestAction::run(indoc! {r#"do try {{}} catch {} while (false);"#}),
        TestAction::run(indoc! {r#"do {} while (false);"#}),
        TestAction::run(indoc! {r#"do try {{}{}} catch {} while (false);"#}),
        TestAction::run(indoc! {r#"do {{}{}} while (false);"#}),
        TestAction::run(indoc! {r#"do {;{}} while (false);"#}),
        TestAction::run(indoc! {r#"do {e: {}} while (false);"#}),
        TestAction::run(indoc! {r#"do {e: ;} while (false);"#}),
        TestAction::run(indoc! {r#"do { break } while (false);"#}),
        TestAction::run(indoc! {r#"while (true) a: break"#}),
        TestAction::run(indoc! {r#"while (true) a: {"a"; break};"#}),
        TestAction::run(indoc! {r#"do {"a";{}} while (false);"#}),
        TestAction::run(indoc! {r#"
            switch (false) {
                default: {}
            }
        "#}),
        TestAction::run(indoc! {r#"
            switch (false) {
                default: {}{}
            }
        "#}),
        TestAction::run(indoc! {r#"
            switch (false) {
                default: ;{}{}
            }
        "#}),
    ]);
}

#[test]
fn truncate_environments_on_non_caught_native_error() {
    let source = "with (new Proxy({}, {has: p => false})) {a}";
    run_test_actions([
        TestAction::assert_native_error(source, JsNativeErrorKind::Reference, "a is not defined"),
        TestAction::assert_native_error(source, JsNativeErrorKind::Reference, "a is not defined"),
    ]);
}

#[test]
fn super_construction_with_paramater_expression() {
    run_test_actions([
        TestAction::run(indoc! {r#"
            class Person {
                constructor(name) {
                    this.name = name;
                }
            }

            class Student extends Person {
                constructor(name = 'unknown') {
                    super(name);
                }
            }
        "#}),
        TestAction::assert_eq("new Student().name", js_str!("unknown")),
        TestAction::assert_eq("new Student('Jack').name", js_str!("Jack")),
    ]);
}

#[test]
fn cross_context_funtion_call() {
    let context1 = &mut Context::default();
    let result = context1.eval(Source::from_bytes(indoc! {r"
        var global = 100;

        (function x() {
            return global;
        })
    "}));

    assert!(result.is_ok());
    let result = result.unwrap();
    assert!(result.is_callable());

    let context2 = &mut Context::default();

    context2
        .register_global_property(js_string!("func"), result, Attribute::all())
        .unwrap();

    let result = context2.eval(Source::from_bytes("func()"));

    assert_eq!(result, Ok(JsValue::new(100)));
}

// See: https://github.com/boa-dev/boa/issues/1848
#[test]
fn long_object_chain_gc_trace_stack_overflow() {
    run_test_actions([
        TestAction::run(indoc! {r#"
            let old = {};
            for (let i = 0; i < 100000; i++) {
                old = { old };
            }
        "#}),
        TestAction::inspect_context(|_| boa_gc::force_collect()),
    ]);
}

#[test]
fn suspended_generator_code_survives_forced_collection() {
    run_test_actions([
        TestAction::run(indoc! {r#"
            function* values() {
                yield 1;
                return 2;
            }

            globalThis.generator = values();
            generator.next();
        "#}),
        TestAction::inspect_context(|_| boa_gc::force_collect()),
        TestAction::assert_eq("generator.next().value", 2),
    ]);
}

#[test]
fn captured_environment_survives_forced_collection() {
    run_test_actions([
        TestAction::run(indoc! {r#"
            function makeClosure() {
                let captured = 41;
                return function() {
                    return captured + 1;
                };
            }

            globalThis.closure = makeClosure();
        "#}),
        TestAction::inspect_context(|_| boa_gc::force_collect()),
        TestAction::assert_eq("closure()", 42),
    ]);
}
