use crate::vm::call_frame::CallFrameLocation;
use crate::vm::source_info::SourcePath;
use crate::vm::{CallFrame, CompletionRecord, NativeCallBoundary, NativeCallBoundaryTarget};
use crate::{
    Context, JsNativeError, JsNativeErrorKind, JsObject, JsResult, JsString, JsValue, Module,
    NativeFunction, Script, TestAction,
    context::HostHooks,
    job::{
        AsyncContext, BoxedFuture, Job, JobCallback, JobExecutor, JobExecutorFuture,
        SimpleJobExecutor,
    },
    js_string,
    module::{ModuleLoader, Referrer, SimpleModuleLoader},
    native_function::{NativeCallAlreadyResumed, NativeCallContinuation, NativeCallSuspension},
    object::{FunctionObjectBuilder, ObjectInitializer},
    property::{Attribute, PropertyDescriptor},
    run_test_actions, run_test_actions_with,
};
use boa_ast::Position;
use boa_gc::{Gc, GcRefCell, Rooted};
use boa_macros::js_str;
use boa_parser::Source;
use futures_lite::future;
use indoc::indoc;
use std::{cell::Cell, future::Future, path::Path, rc::Rc};

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

#[derive(Debug, Default)]
struct InMemoryModuleLoader {
    modules: GcRefCell<Vec<(JsString, Module)>>,
}

impl InMemoryModuleLoader {
    fn insert(&self, specifier: JsString, module: Module) {
        self.modules.borrow_mut().push((specifier, module));
    }
}

impl ModuleLoader for InMemoryModuleLoader {
    fn load_imported_module(
        self: Rc<Self>,
        _referrer: Referrer,
        specifier: JsString,
        _context: &AsyncContext<'_>,
    ) -> impl Future<Output = JsResult<Module>> {
        let result = self
            .modules
            .borrow()
            .iter()
            .find(|(key, _)| key == &specifier)
            .map(|(_, module)| module.clone())
            .ok_or_else(|| {
                JsNativeError::typ()
                    .with_message(format!(
                        "missing in-memory module: {}",
                        specifier.to_std_string_escaped()
                    ))
                    .into()
            });
        async { result }
    }
}

#[test]
fn async_evaluation_resumes_a_native_call_exactly_once() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    let _slot_root = Rooted::from_gc(slot.clone());
    let after = Gc::new(GcRefCell::new(false));
    let _after_root = Rooted::from_gc(after.clone());
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
    let _slot_root = Rooted::from_gc(slot.clone());
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
fn native_continuation_resumes_synchronous_javascript_callback() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    let _slot_root = Rooted::from_gc(slot.clone());
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    context
        .register_global_callable(
            js_string!("dispatch"),
            0,
            NativeFunction::from_fn_ptr(|_, _, context| {
                let callback = context
                    .global_object()
                    .get(js_string!("listener"), context)?
                    .as_object()
                    .expect("listener must be callable")
                    .clone();
                context.call_with_native_continuation(
                    &callback,
                    &JsValue::undefined(),
                    &[],
                    NativeCallContinuation::from_copy_closure_with_captures(
                        |result, (), context| {
                            let value = result?.to_i32(context)?;
                            Ok(JsValue::from(value + 1))
                        },
                        (),
                    ),
                )
            }),
        )
        .unwrap();
    let script = Script::parse(
        Source::from_bytes("globalThis.listener = () => suspend() + 1; dispatch() + 1"),
        None,
        &mut context,
    )
    .unwrap();
    let mut evaluation = Box::pin(script.evaluate_async_with_budget(&mut context, 1));

    while slot.borrow().is_none() {
        assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
    }
    slot.borrow()
        .as_ref()
        .unwrap()
        .resume(Ok(JsValue::from(40)))
        .unwrap();

    assert_eq!(future::block_on(evaluation).unwrap(), JsValue::from(43));
}

#[test]
fn native_continuation_keeps_synchronous_native_api_compatible() {
    let mut context = Context::default();
    context
        .register_global_callable(
            js_string!("dispatch"),
            0,
            NativeFunction::from_fn_ptr(|_, _, context| {
                let callback = context
                    .global_object()
                    .get(js_string!("listener"), context)?
                    .as_object()
                    .expect("listener must be callable")
                    .clone();
                context.call_with_native_continuation(
                    &callback,
                    &JsValue::undefined(),
                    &[],
                    NativeCallContinuation::from_copy_closure_with_captures(
                        |result, (), context| Ok(JsValue::from(result?.to_i32(context)? + 1)),
                        (),
                    ),
                )
            }),
        )
        .unwrap();

    assert_eq!(
        context
            .eval(Source::from_bytes(
                "globalThis.listener = () => 40; dispatch() + 1",
            ))
            .unwrap(),
        JsValue::from(42)
    );
}

#[test]
fn direct_object_calls_complete_native_javascript_continuations() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    let _slot_root = Rooted::from_gc(slot.clone());
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    context
        .register_global_callable(
            js_string!("dispatch"),
            0,
            NativeFunction::from_fn_ptr(|_, _, context| {
                let callback = context
                    .global_object()
                    .get(js_string!("listener"), context)?
                    .as_object()
                    .expect("listener must be callable")
                    .clone();
                context.call_with_native_continuation(
                    &callback,
                    &JsValue::undefined(),
                    &[],
                    NativeCallContinuation::from_copy_closure_with_captures(
                        |result, (), context| Ok(JsValue::from(result?.to_i32(context)? + 1)),
                        (),
                    ),
                )
            }),
        )
        .unwrap();
    let dispatch = context
        .global_object()
        .get(js_string!("dispatch"), &mut context)
        .unwrap()
        .as_object()
        .unwrap()
        .clone();
    let initial_frames = context.vm.frames.len();
    let initial_stack = context.vm.stack.stack.len();

    context
        .eval(Source::from_bytes("globalThis.listener = () => 40"))
        .unwrap();
    assert_eq!(
        dispatch
            .call(&JsValue::undefined(), &[], &mut context)
            .unwrap(),
        JsValue::from(41)
    );
    assert_eq!(context.vm.frames.len(), initial_frames);
    assert_eq!(context.vm.stack.stack.len(), initial_stack);
    assert!(context.vm.native_call_continuations.is_empty());

    context
        .eval(Source::from_bytes(
            "globalThis.listener = () => { throw new Error('direct failure') }",
        ))
        .unwrap();
    let error = dispatch
        .call(&JsValue::undefined(), &[], &mut context)
        .unwrap_err();
    assert!(error.to_string().contains("direct failure"));
    assert_eq!(context.vm.frames.len(), initial_frames);
    assert_eq!(context.vm.stack.stack.len(), initial_stack);
    assert!(context.vm.native_call_continuations.is_empty());

    context
        .eval(Source::from_bytes("globalThis.listener = () => suspend()"))
        .unwrap();
    let this = JsValue::undefined();
    let mut call = Box::pin(dispatch.call_async(&this, &[], &mut context));
    assert!(future::block_on(future::poll_once(call.as_mut())).is_none());
    slot.borrow_mut()
        .take()
        .unwrap()
        .resume(Ok(JsValue::from(40)))
        .unwrap();
    assert_eq!(future::block_on(call).unwrap(), JsValue::from(41));
    assert_eq!(context.vm.frames.len(), initial_frames);
    assert_eq!(context.vm.stack.stack.len(), initial_stack);
    assert!(context.vm.native_call_continuations.is_empty());
}

#[test]
fn nested_direct_calls_do_not_resume_the_paused_javascript_caller() {
    let mut context = Context::default();
    context
        .register_global_callable(
            js_string!("dispatch"),
            0,
            NativeFunction::from_fn_ptr(|_, _, context| {
                let callback = context
                    .global_object()
                    .get(js_string!("listener"), context)?
                    .as_object()
                    .expect("listener must be callable")
                    .clone();
                context.call_with_native_continuation(
                    &callback,
                    &JsValue::undefined(),
                    &[],
                    NativeCallContinuation::from_copy_closure_with_captures(
                        |result, (), _| result,
                        (),
                    ),
                )
            }),
        )
        .unwrap();
    context
        .register_global_callable(
            js_string!("bridge"),
            0,
            NativeFunction::from_fn_ptr(|_, _, context| {
                let dispatch = context
                    .global_object()
                    .get(js_string!("dispatch"), context)?
                    .as_object()
                    .expect("dispatch must be callable")
                    .clone();
                let result = dispatch.call(&JsValue::undefined(), &[], context);
                if context
                    .global_object()
                    .get(js_string!("outerAdvanced"), context)?
                    .as_boolean()
                    == Some(true)
                {
                    return Err(JsNativeError::error()
                        .with_message("paused caller resumed inside bridge")
                        .into());
                }
                result
            }),
        )
        .unwrap();

    context
        .eval(Source::from_bytes(
            "globalThis.outerAdvanced = false; globalThis.listener = () => 41; globalThis.outer = () => { const value = bridge(); outerAdvanced = true; return value + 1; }",
        ))
        .unwrap();
    let outer = context
        .global_object()
        .get(js_string!("outer"), &mut context)
        .unwrap()
        .as_object()
        .unwrap()
        .clone();
    assert_eq!(
        outer
            .call(&JsValue::undefined(), &[], &mut context)
            .unwrap(),
        JsValue::from(42)
    );

    context
        .eval(Source::from_bytes(
            "outerAdvanced = false; listener = () => { throw new Error('nested failure') }; outer = () => { try { bridge() } catch (error) { outerAdvanced = true; return error.message; } }",
        ))
        .unwrap();
    let outer = context
        .global_object()
        .get(js_string!("outer"), &mut context)
        .unwrap()
        .as_object()
        .unwrap()
        .clone();
    assert_eq!(
        outer
            .call(&JsValue::undefined(), &[], &mut context)
            .unwrap(),
        js_string!("nested failure").into()
    );
}

#[test]
fn native_continuation_resumes_a_suspending_native_callback() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    context
        .register_global_callable(
            js_string!("nativeListener"),
            0,
            suspending_function(slot.clone()),
        )
        .unwrap();
    context
        .register_global_callable(
            js_string!("dispatch"),
            0,
            NativeFunction::from_fn_ptr(|_, _, context| {
                let callback = context
                    .global_object()
                    .get(js_string!("nativeListener"), context)?
                    .as_object()
                    .expect("listener must be callable")
                    .clone();
                context.call_with_native_continuation(
                    &callback,
                    &JsValue::undefined(),
                    &[],
                    NativeCallContinuation::from_copy_closure_with_captures(
                        |result, (), context| Ok(JsValue::from(result?.to_i32(context)? + 1)),
                        (),
                    ),
                )
            }),
        )
        .unwrap();
    let script = Script::parse(Source::from_bytes("dispatch() + 1"), None, &mut context).unwrap();
    let mut evaluation = Box::pin(script.evaluate_async(&mut context));
    while slot.borrow().is_none() {
        assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
    }
    slot.borrow()
        .as_ref()
        .unwrap()
        .resume(Ok(JsValue::from(40)))
        .unwrap();

    assert_eq!(future::block_on(evaluation).unwrap(), JsValue::from(42));
}

#[test]
fn native_continuation_captures_remain_rooted_while_suspended() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    let _slot_root = Rooted::from_gc(slot.clone());
    let resumed = Gc::new(GcRefCell::new(false));
    let _resumed_root = Rooted::from_gc(resumed.clone());
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    context
        .register_global_callable(
            js_string!("dispatch"),
            0,
            NativeFunction::from_copy_closure_with_captures(
                |_, _, resumed, context| {
                    let callback = context
                        .global_object()
                        .get(js_string!("listener"), context)?
                        .as_object()
                        .expect("listener must be callable")
                        .clone();
                    context.call_with_native_continuation(
                        &callback,
                        &JsValue::undefined(),
                        &[],
                        NativeCallContinuation::from_copy_closure_with_captures(
                            |result, resumed, _| {
                                *resumed.borrow_mut() = true;
                                result
                            },
                            resumed.clone(),
                        ),
                    )
                },
                resumed.clone(),
            ),
        )
        .unwrap();
    let script = Script::parse(
        Source::from_bytes("globalThis.listener = () => suspend(); dispatch()"),
        None,
        &mut context,
    )
    .unwrap();
    let mut evaluation = Box::pin(script.evaluate_async(&mut context));
    while slot.borrow().is_none() {
        assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
    }
    boa_gc::force_collect();
    slot.borrow()
        .as_ref()
        .unwrap()
        .resume(Ok(JsValue::from(42)))
        .unwrap();

    assert_eq!(future::block_on(evaluation).unwrap(), JsValue::from(42));
    assert!(*resumed.borrow());
}

#[test]
fn native_continuation_boundaries_are_lifo_for_reentrant_callbacks() {
    fn dispatch_named(name: &'static str) -> NativeFunction {
        NativeFunction::from_copy_closure_with_captures(
            |_, _, name, context| {
                let callback = context
                    .global_object()
                    .get(js_string!(*name), context)?
                    .as_object()
                    .expect("listener must be callable")
                    .clone();
                context.call_with_native_continuation(
                    &callback,
                    &JsValue::undefined(),
                    &[],
                    NativeCallContinuation::from_copy_closure_with_captures(
                        |result, (), context| Ok(JsValue::from(result?.to_i32(context)? + 1)),
                        (),
                    ),
                )
            },
            name,
        )
    }

    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    context
        .register_global_callable(
            js_string!("outerDispatch"),
            0,
            dispatch_named("outerListener"),
        )
        .unwrap();
    context
        .register_global_callable(
            js_string!("innerDispatch"),
            0,
            dispatch_named("innerListener"),
        )
        .unwrap();
    let script = Script::parse(
        Source::from_bytes(
            "globalThis.innerListener = () => suspend(); globalThis.outerListener = () => innerDispatch() + 1; outerDispatch()",
        ),
        None,
        &mut context,
    )
    .unwrap();
    let mut evaluation = Box::pin(script.evaluate_async(&mut context));
    while slot.borrow().is_none() {
        assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
    }
    slot.borrow()
        .as_ref()
        .unwrap()
        .resume(Ok(JsValue::from(39)))
        .unwrap();

    assert_eq!(future::block_on(evaluation).unwrap(), JsValue::from(42));
}

#[test]
fn nested_continuation_completion_restores_the_outer_active_guard() {
    fn dispatch_named(name: &'static str) -> NativeFunction {
        NativeFunction::from_copy_closure_with_captures(
            |_, _, name, context| {
                let callback = context
                    .global_object()
                    .get(js_string!(*name), context)?
                    .as_callable()
                    .expect("listener must be callable")
                    .clone();
                context.call_with_native_continuation(
                    &callback,
                    &JsValue::undefined(),
                    &[],
                    NativeCallContinuation::from_copy_closure_with_captures(
                        |result, (), context| Ok(JsValue::from(result?.to_i32(context)? + 1)),
                        (),
                    ),
                )
            },
            name,
        )
    }

    let mut context = Context::default();
    context
        .register_global_callable(
            js_string!("innerDispatch"),
            0,
            dispatch_named("innerListener"),
        )
        .unwrap();
    context
        .register_global_callable(
            js_string!("outerDispatch"),
            0,
            NativeFunction::from_fn_ptr(|_, _, context| {
                let callback = context
                    .global_object()
                    .get(js_string!("outerListener"), context)?
                    .as_callable()
                    .expect("outerListener must be callable")
                    .clone();
                context.call_with_native_continuation(
                    &callback,
                    &JsValue::undefined(),
                    &[],
                    NativeCallContinuation::from_copy_closure_with_captures(
                        |result, (), context| {
                            result?;
                            let nested = context
                                .global_object()
                                .get(js_string!("nested"), context)?
                                .as_callable()
                                .expect("nested must be callable")
                                .clone();
                            assert_eq!(
                                nested.call(&JsValue::undefined(), &[], context)?,
                                JsValue::from(2)
                            );
                            let final_listener = context
                                .global_object()
                                .get(js_string!("finalListener"), context)?
                                .as_callable()
                                .expect("finalListener must be callable")
                                .clone();
                            context.call_with_native_continuation(
                                &final_listener,
                                &JsValue::undefined(),
                                &[],
                                NativeCallContinuation::from_copy_closure_with_captures(
                                    |result, (), context| {
                                        Ok(JsValue::from(result?.to_i32(context)? + 1))
                                    },
                                    (),
                                ),
                            )
                        },
                        (),
                    ),
                )
            }),
        )
        .unwrap();

    assert_eq!(
        context
            .eval(Source::from_bytes(
                "globalThis.innerListener = () => 1; globalThis.nested = () => innerDispatch(); globalThis.outerListener = () => 0; globalThis.finalListener = () => 3; outerDispatch()",
            ))
            .unwrap(),
        JsValue::from(4)
    );
    assert!(context.vm.native_call_continuations.is_empty());
}

#[test]
fn nested_continuation_suspension_is_awaited_before_the_next_opcode() {
    let mut context = Context::default();
    let first_slot = Gc::new(GcRefCell::new(None));
    let _first_slot_root = Rooted::from_gc(first_slot.clone());
    let second_slot = Gc::new(GcRefCell::new(None));
    let _second_slot_root = Rooted::from_gc(second_slot.clone());
    let suspension_instruction = Gc::new(GcRefCell::new(None));
    let _suspension_instruction_root = Rooted::from_gc(suspension_instruction.clone());
    let instruction_count = context.vm.instruction_count.clone();
    context
        .register_global_callable(
            js_string!("suspendFirst"),
            0,
            suspending_function(first_slot.clone()),
        )
        .unwrap();
    context
        .register_global_callable(
            js_string!("suspendSecond"),
            0,
            NativeFunction::from_copy_closure_with_captures(
                |_, _, captures, context| {
                    let suspension = context.suspend_native_call()?;
                    *captures.0.borrow_mut() = Some(suspension);
                    *captures.1.borrow_mut() = Some(context.vm.instruction_count.get());
                    Ok(JsValue::undefined())
                },
                (second_slot.clone(), suspension_instruction.clone()),
            ),
        )
        .unwrap();
    context
        .register_global_callable(
            js_string!("dispatch"),
            0,
            NativeFunction::from_fn_ptr(|_, _, context| {
                let callback = context
                    .global_object()
                    .get(js_string!("listener"), context)?
                    .as_callable()
                    .expect("listener must be callable")
                    .clone();
                context.call_with_native_continuation(
                    &callback,
                    &JsValue::undefined(),
                    &[],
                    NativeCallContinuation::from_copy_closure_with_captures(
                        |result, (), context| {
                            result?;
                            let suspend = context
                                .global_object()
                                .get(js_string!("suspendSecond"), context)?
                                .as_callable()
                                .expect("suspend must be callable")
                                .clone();
                            suspend.call(&JsValue::undefined(), &[], context)
                        },
                        (),
                    ),
                )
            }),
        )
        .unwrap();
    context
        .eval(Source::from_bytes("globalThis.listener = suspendFirst"))
        .unwrap();
    let script = Script::parse(Source::from_bytes("dispatch()"), None, &mut context).unwrap();
    let mut evaluation = Box::pin(script.evaluate_async_with_budget(&mut context, u32::MAX));

    assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
    assert!(first_slot.borrow().is_some());
    assert!(second_slot.borrow().is_none());
    first_slot
        .borrow()
        .as_ref()
        .unwrap()
        .resume(Ok(JsValue::from(1)))
        .unwrap();

    assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
    assert!(second_slot.borrow().is_some());
    assert_eq!(
        instruction_count.get(),
        suspension_instruction
            .borrow()
            .expect("the suspension records its instruction position")
    );
    second_slot
        .borrow()
        .as_ref()
        .unwrap()
        .resume(Ok(JsValue::from(42)))
        .unwrap();

    assert_eq!(future::block_on(evaluation).unwrap(), JsValue::from(42));
}

#[test]
fn native_continuation_receives_throw_after_finally() {
    let mut context = Context::default();
    context
        .register_global_callable(
            js_string!("dispatch"),
            0,
            NativeFunction::from_fn_ptr(|_, _, context| {
                let callback = context
                    .global_object()
                    .get(js_string!("listener"), context)?
                    .as_object()
                    .expect("listener must be callable")
                    .clone();
                context.call_with_native_continuation(
                    &callback,
                    &JsValue::undefined(),
                    &[],
                    NativeCallContinuation::from_copy_closure_with_captures(
                        |result, (), _| match result {
                            Ok(value) => Ok(value),
                            Err(_) => Ok(js_string!("handled").into()),
                        },
                        (),
                    ),
                )
            }),
        )
        .unwrap();

    assert_eq!(
        future::block_on(
            Script::parse(
                Source::from_bytes(
                    "globalThis.finalized = false; globalThis.listener = () => { try { throw 'failure' } finally { finalized = true } }; dispatch()",
                ),
                None,
                &mut context,
            )
            .unwrap()
            .evaluate_async(&mut context),
        )
        .unwrap(),
        js_string!("handled").into()
    );
    assert_eq!(
        context
            .global_object()
            .get(js_string!("finalized"), &mut context)
            .unwrap(),
        JsValue::from(true)
    );
}

#[test]
fn dropping_native_continuation_cancels_suspension_and_boundary() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    let _slot_root = Rooted::from_gc(slot.clone());
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    context
        .register_global_callable(
            js_string!("dispatch"),
            0,
            NativeFunction::from_fn_ptr(|_, _, context| {
                let callback = context
                    .global_object()
                    .get(js_string!("listener"), context)?
                    .as_object()
                    .expect("listener must be callable")
                    .clone();
                context.call_with_native_continuation(
                    &callback,
                    &JsValue::undefined(),
                    &[],
                    NativeCallContinuation::from_copy_closure_with_captures(
                        |result, (), _| result,
                        (),
                    ),
                )
            }),
        )
        .unwrap();
    let script = Script::parse(
        Source::from_bytes("globalThis.listener = () => suspend(); dispatch()"),
        None,
        &mut context,
    )
    .unwrap();
    let mut evaluation = Box::pin(script.evaluate_async(&mut context));
    while slot.borrow().is_none() {
        assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
    }
    drop(evaluation);

    assert!(context.vm.native_call_continuations.is_empty());
    assert_eq!(
        slot.borrow()
            .as_ref()
            .unwrap()
            .resume(Ok(JsValue::undefined())),
        Err(NativeCallAlreadyResumed)
    );
    assert_eq!(
        context.eval(Source::from_bytes("1 + 1")).unwrap(),
        JsValue::from(2)
    );
}

#[test]
fn non_catchable_error_unwinds_native_continuation_boundary() {
    let mut context = Context::default();
    context.runtime_limits_mut().set_loop_iteration_limit(10);
    context
        .register_global_callable(
            js_string!("dispatch"),
            0,
            NativeFunction::from_fn_ptr(|_, _, context| {
                let callback = context
                    .global_object()
                    .get(js_string!("listener"), context)?
                    .as_object()
                    .expect("listener must be callable")
                    .clone();
                context.call_with_native_continuation(
                    &callback,
                    &JsValue::undefined(),
                    &[],
                    NativeCallContinuation::from_copy_closure_with_captures(
                        |result, (), _| result,
                        (),
                    ),
                )
            }),
        )
        .unwrap();
    let script = Script::parse(
        Source::from_bytes(
            "globalThis.listener = () => { for (let i = 0; i < 1_000; ++i) {} }; dispatch()",
        ),
        None,
        &mut context,
    )
    .unwrap();

    let error = future::block_on(script.evaluate_async(&mut context)).unwrap_err();
    assert!(
        error
            .as_native()
            .is_some_and(JsNativeError::is_runtime_limit)
    );
    assert!(context.vm.native_call_continuations.is_empty());
    assert_eq!(
        context.eval(Source::from_bytes("1 + 1")).unwrap(),
        JsValue::from(2)
    );
}

#[test]
fn async_jobs_propagate_suspension_from_nested_promise_reaction() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    let _slot_root = Rooted::from_gc(slot.clone());
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    context
        .eval(Source::from_bytes(
            "globalThis.result = 0; Promise.resolve().then(() => suspend() + 1).then(value => result = value)",
        ))
        .unwrap();
    let mut jobs = Box::pin(context.run_jobs_async());

    assert!(future::block_on(future::poll_once(jobs.as_mut())).is_none());
    slot.borrow()
        .as_ref()
        .unwrap()
        .resume(Ok(JsValue::from(41)))
        .unwrap();
    future::block_on(jobs).unwrap();

    assert_eq!(
        context
            .global_object()
            .get(js_string!("result"), &mut context)
            .unwrap(),
        JsValue::from(42)
    );
}

#[test]
fn direct_job_executor_run_jobs_async_enables_async_suspension() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    let _slot_root = Rooted::from_gc(slot.clone());
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    context
        .eval(Source::from_bytes(
            "globalThis.result = 0; Promise.resolve().then(() => suspend() + 1).then(value => result = value)",
        ))
        .unwrap();

    let executor = context
        .downcast_job_executor::<SimpleJobExecutor>()
        .unwrap();
    {
        let async_context = AsyncContext::new(&mut context);
        let mut jobs = Box::pin(executor.run_jobs_async(&async_context));

        assert!(future::block_on(future::poll_once(jobs.as_mut())).is_none());
        slot.borrow()
            .as_ref()
            .unwrap()
            .resume(Ok(JsValue::from(41)))
            .unwrap();
        future::block_on(jobs).unwrap();
    }

    assert!(!context.async_jobs_enabled);

    assert_eq!(
        context
            .global_object()
            .get(js_string!("result"), &mut context)
            .unwrap(),
        JsValue::from(42)
    );
}

#[test]
fn async_jobs_keep_promise_reactions_fifo() {
    let mut context = Context::default();
    context
        .eval(Source::from_bytes(
            "globalThis.order = []; Promise.resolve().then(() => order.push(1)); Promise.resolve().then(() => order.push(2));",
        ))
        .unwrap();

    future::block_on(context.run_jobs_async()).unwrap();
    assert_eq!(
        context.eval(Source::from_bytes("order.join(',')")).unwrap(),
        js_string!("1,2").into()
    );
}

#[test]
fn promise_jobs_preserve_host_call_job_callback_hooks() {
    struct CountingHooks(Cell<usize>);

    impl HostHooks for CountingHooks {
        fn call_job_callback(
            &self,
            job: JobCallback,
            this: &JsValue,
            args: &[JsValue],
            context: &mut Context,
        ) -> JsResult<JsValue> {
            self.0.set(self.0.get() + 1);
            job.callback().call(this, args, context)
        }

        fn call_job_callback_async<'a>(
            &'a self,
            job: JobCallback,
            this: &'a JsValue,
            args: &'a [JsValue],
            context: &'a mut Context,
        ) -> BoxedFuture<'a> {
            self.0.set(self.0.get() + 1);
            Box::pin(async move { job.callback().call_async(this, args, context).await })
        }
    }

    let hooks = Rc::new(CountingHooks(Cell::new(0)));
    let mut context = Context::builder()
        .host_hooks(hooks.clone())
        .build()
        .unwrap();
    context
        .eval(Source::from_bytes(
            "Promise.resolve(1).then(value => value + 1); Promise.resolve({ then(resolve) { resolve(2); } });",
        ))
        .unwrap();

    context.run_jobs().unwrap();

    assert_eq!(hooks.0.get(), 2);

    let mut context = Context::builder()
        .host_hooks(hooks.clone())
        .build()
        .unwrap();
    context
        .eval(Source::from_bytes(
            "Promise.resolve(1).then(value => value + 1); Promise.resolve({ then(resolve) { resolve(2); } });",
        ))
        .unwrap();
    future::block_on(context.run_jobs_async()).unwrap();

    assert_eq!(hooks.0.get(), 4);
}

#[test]
fn synchronous_jobs_reject_native_suspension_in_promise_callbacks() {
    for script in [
        "const p = Promise.resolve(); p.constructor = { [Symbol.species]: class { constructor(executor) { executor(value => suspend(value), () => {}); } } }; p.then(() => 1);",
        "const p = Promise.resolve(); p.constructor = { [Symbol.species]: class { constructor(executor) { executor(() => {}, error => suspend(error)); } } }; p.then(() => { throw new Error('reject'); });",
    ] {
        let mut context = Context::default();
        let slot = Gc::new(GcRefCell::new(None));
        let _slot_root = Rooted::from_gc(slot.clone());
        context
            .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
            .unwrap();
        context.eval(Source::from_bytes(script)).unwrap();

        let Err(error) = context.run_jobs() else {
            panic!("suspension did not produce an error for {script}");
        };

        assert!(
            error
                .to_string()
                .contains("native call suspension requires asynchronous"),
            "unexpected error for {script}: {error}"
        );
    }

    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    let _slot_root = Rooted::from_gc(slot.clone());
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    context
        .eval(Source::from_bytes(
            "Promise.resolve({ then: () => suspend() });",
        ))
        .unwrap();

    context.run_jobs().unwrap();
    assert!(slot.borrow().is_some());
}

#[test]
fn async_module_propagates_suspension_after_top_level_await() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    let module = Module::parse(
        Source::from_bytes("await Promise.resolve(); export const value = suspend() + 1"),
        None,
        &mut context,
    )
    .unwrap();
    let mut evaluation = Box::pin(module.load_link_evaluate_async(&mut context));

    while slot.borrow().is_none() {
        assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
    }
    slot.borrow()
        .as_ref()
        .unwrap()
        .resume(Ok(JsValue::from(41)))
        .unwrap();
    future::block_on(evaluation).unwrap();

    assert_eq!(
        module
            .namespace(&mut context)
            .get(js_string!("value"), &mut context)
            .unwrap(),
        JsValue::from(42)
    );
}

#[test]
fn async_module_propagates_suspension_before_top_level_await() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    let module = Module::parse(
        Source::from_bytes("export const value = suspend() + 1; await Promise.resolve()"),
        None,
        &mut context,
    )
    .unwrap();
    let mut evaluation = Box::pin(module.load_link_evaluate_async(&mut context));

    while slot.borrow().is_none() {
        assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
    }
    slot.borrow()
        .as_ref()
        .unwrap()
        .resume(Ok(JsValue::from(41)))
        .unwrap();
    future::block_on(evaluation).unwrap();

    assert_eq!(
        module
            .namespace(&mut context)
            .get(js_string!("value"), &mut context)
            .unwrap(),
        JsValue::from(42)
    );
}

#[test]
fn async_module_entry_propagates_suspension_without_top_level_await() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    let module = Module::parse(
        Source::from_bytes("export const value = suspend() + 1"),
        None,
        &mut context,
    )
    .unwrap();
    let mut evaluation = Box::pin(module.load_link_evaluate_async(&mut context));

    while slot.borrow().is_none() {
        assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
    }
    slot.borrow()
        .as_ref()
        .unwrap()
        .resume(Ok(JsValue::from(41)))
        .unwrap();
    future::block_on(evaluation).unwrap();

    assert_eq!(
        module
            .namespace(&mut context)
            .get(js_string!("value"), &mut context)
            .unwrap(),
        JsValue::from(42)
    );
}

#[test]
fn async_module_entry_preserves_non_tla_dependency_order() {
    let loader = Rc::new(InMemoryModuleLoader::default());
    let mut context = Context::builder()
        .module_loader(loader.clone())
        .build()
        .unwrap();
    let slot = Gc::new(GcRefCell::new(None));
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    let dependency = Module::parse(
        Source::from_reader(
            &b"globalThis.order = ['dependency-before']; export const value = suspend() + 1; order.push('dependency-after')"[..],
            None,
        ),
        None,
        &mut context,
    )
    .unwrap();
    loader.insert(js_string!("./dependency.js"), dependency);
    let module = Module::parse(
        Source::from_reader(
            &b"import { value } from './dependency.js'; order.push('root-before'); export const result = value + suspend(); order.push('root-after')"[..],
            None,
        ),
        None,
        &mut context,
    )
    .unwrap();
    let mut evaluation = Box::pin(module.load_link_evaluate_async(&mut context));

    for resumed in [20, 20] {
        while slot.borrow().is_none() {
            assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
        }
        slot.borrow_mut()
            .take()
            .unwrap()
            .resume(Ok(JsValue::from(resumed)))
            .unwrap();
    }
    future::block_on(evaluation).unwrap();

    assert_eq!(
        module
            .namespace(&mut context)
            .get(js_string!("result"), &mut context)
            .unwrap(),
        JsValue::from(41)
    );
    assert_eq!(
        context.eval(Source::from_bytes("order.join(',')")).unwrap(),
        js_string!("dependency-before,dependency-after,root-before,root-after").into()
    );
}

#[test]
fn dropping_async_module_entry_cancels_suspension_and_restores_sync_mode() {
    let mut context = Context::default();
    let slot = Gc::new(GcRefCell::new(None));
    context
        .register_global_callable(js_string!("suspend"), 0, suspending_function(slot.clone()))
        .unwrap();
    let module = Module::parse(
        Source::from_bytes("export const value = suspend()"),
        None,
        &mut context,
    )
    .unwrap();
    let mut evaluation = Box::pin(module.load_link_evaluate_async(&mut context));
    while slot.borrow().is_none() {
        assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
    }
    drop(evaluation);

    assert!(!context.async_jobs_enabled);
    assert_eq!(
        slot.borrow()
            .as_ref()
            .unwrap()
            .resume(Ok(JsValue::undefined())),
        Err(NativeCallAlreadyResumed)
    );
    let synchronous = Module::parse(
        Source::from_bytes("export const value = 42"),
        None,
        &mut context,
    )
    .unwrap();
    let promise = synchronous.load_link_evaluate(&mut context);
    context.run_jobs().unwrap();
    assert_eq!(
        promise.state(),
        crate::builtins::promise::PromiseState::Fulfilled(JsValue::undefined())
    );
    assert_eq!(
        synchronous
            .namespace(&mut context)
            .get(js_string!("value"), &mut context)
            .unwrap(),
        JsValue::from(42)
    );
}

#[test]
fn async_module_entry_reports_evaluation_and_load_errors() {
    let loader = Rc::new(SimpleModuleLoader::new(Path::new(".")).unwrap());
    let mut context = Context::builder().module_loader(loader).build().unwrap();
    let rejected = Module::parse(
        Source::from_bytes("throw new Error('module failure')"),
        None,
        &mut context,
    )
    .unwrap();
    let error = future::block_on(rejected.load_link_evaluate_async(&mut context)).unwrap_err();
    assert!(error.to_string().contains("module failure"));
    assert!(!context.async_jobs_enabled);

    let root = std::env::current_dir().unwrap().join("missing-main.js");
    let missing = Module::parse(
        Source::from_reader(&b"import './missing-dependency.js'"[..], Some(&root)),
        None,
        &mut context,
    )
    .unwrap();
    assert!(future::block_on(missing.load_link_evaluate_async(&mut context)).is_err());
    assert!(!context.async_jobs_enabled);
}

#[test]
fn async_module_entry_preserves_mixed_tla_dependency_order() {
    let loader = Rc::new(InMemoryModuleLoader::default());
    let mut context = Context::builder()
        .module_loader(loader.clone())
        .build()
        .unwrap();
    let dependency = Module::parse(
        Source::from_reader(
            &b"globalThis.mixedOrder = ['dependency-before']; await Promise.resolve(); mixedOrder.push('dependency-after'); export const value = 20"[..],
            None,
        ),
        None,
        &mut context,
    )
    .unwrap();
    loader.insert(js_string!("./mixed-dependency.js"), dependency);
    let module = Module::parse(
        Source::from_reader(
            &b"import { value } from './mixed-dependency.js'; mixedOrder.push('root'); export const result = value + 22"[..],
            None,
        ),
        None,
        &mut context,
    )
    .unwrap();

    future::block_on(module.load_link_evaluate_async(&mut context)).unwrap();

    assert_eq!(
        module
            .namespace(&mut context)
            .get(js_string!("result"), &mut context)
            .unwrap(),
        JsValue::from(42)
    );
    assert_eq!(
        context
            .eval(Source::from_bytes("mixedOrder.join(',')"))
            .unwrap(),
        js_string!("dependency-before,dependency-after,root").into()
    );
}

#[test]
fn async_module_entry_uses_custom_job_executor() {
    #[derive(Debug, Default)]
    struct DelegatingExecutor {
        inner: Rc<SimpleJobExecutor>,
        async_runs: Cell<usize>,
    }

    impl JobExecutor for DelegatingExecutor {
        fn enqueue_job(self: Rc<Self>, job: Job, context: &mut Context) {
            self.inner.clone().enqueue_job(job, context);
        }

        fn run_jobs(self: Rc<Self>, context: &mut Context) -> JsResult<()> {
            self.inner.clone().run_jobs(context)
        }

        fn run_jobs_async<'a>(
            self: Rc<Self>,
            context: &'a AsyncContext<'_>,
        ) -> JobExecutorFuture<'a> {
            self.async_runs.set(self.async_runs.get() + 1);
            self.inner.clone().run_jobs_async(context)
        }
    }

    let executor = Rc::new(DelegatingExecutor::default());
    let mut context = Context::builder()
        .job_executor(executor.clone())
        .build()
        .unwrap();
    let module = Module::parse(
        Source::from_bytes("export const value = 42"),
        None,
        &mut context,
    )
    .unwrap();

    future::block_on(module.load_link_evaluate_async(&mut context)).unwrap();

    assert!(executor.async_runs.get() > 0);
    assert_eq!(
        module
            .namespace(&mut context)
            .get(js_string!("value"), &mut context)
            .unwrap(),
        JsValue::from(42)
    );
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
    let _slot_root = Rooted::from_gc(slot.clone());
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
    let _slot_root = Rooted::from_gc(slot.clone());
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
fn rejected_native_completion_does_not_pop_after_its_placeholder_was_consumed() {
    let mut context = Context::default();
    let placeholder = ObjectInitializer::new(&mut context).build();
    let frame = context.vm.frame().clone();
    context
        .vm
        .push_frame_with_stack(frame, JsValue::undefined(), JsValue::null());
    context.vm.stack.push(JsValue::from(42));
    context.vm.frame.set_exit_early(true);
    context
        .vm
        .native_call_continuations
        .push(NativeCallBoundary {
            target: NativeCallBoundaryTarget::NativePlaceholder(placeholder.clone()),
            continuation: NativeCallContinuation::from_copy_closure_with_captures(
                |result, (), _| result,
                (),
            ),
        });

    let completion = context.apply_native_call_completion(
        &placeholder,
        Err(JsNativeError::error()
            .with_message("callback failure")
            .into()),
    );

    let std::ops::ControlFlow::Break(CompletionRecord::Throw(error)) = completion else {
        panic!("missing internal error after the placeholder was consumed");
    };
    assert!(
        error
            .to_string()
            .contains("suspended native call result was consumed before VM suspension")
    );
    assert!(context.vm.native_call_continuations.is_empty());
    context.vm.pop_frame();
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
    let _resumed_object_root = resumed_object.clone().root();
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
fn native_constructor_cannot_start_a_call_continuation() {
    let mut context = Context::default();
    context
        .register_global_callable(
            js_string!("ContinuationConstructor"),
            1,
            NativeFunction::from_copy_closure(|_, args, context| {
                let callback = args[0]
                    .as_callable()
                    .expect("the test passes a callback")
                    .clone();
                context.call_with_native_continuation(
                    &callback,
                    &JsValue::undefined(),
                    &[],
                    NativeCallContinuation::from_copy_closure_with_captures(
                        |result, (), _| result,
                        (),
                    ),
                )
            }),
        )
        .unwrap();

    let error = context
        .eval(Source::from_bytes("new ContinuationConstructor(() => 1)"))
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("native constructors cannot start call continuations")
    );
    assert!(context.vm.native_call_continuations.is_empty());
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
fn recursion_range_error() {
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
            JsNativeErrorKind::Range,
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
            JsNativeErrorKind::Range,
            "exceeded maximum number of recursive calls",
        ),
    ]);
}

#[test]
fn recursion_limit_is_catchable_as_range_error() {
    run_test_actions([
        TestAction::inspect_context(|context| {
            context.runtime_limits_mut().set_recursion_limit(10);
        }),
        TestAction::assert_eq(
            indoc! {r#"
                function probe() {
                    probe();
                }
                try {
                    probe();
                    false;
                } catch (error) {
                    error instanceof RangeError &&
                        error.message === "exceeded maximum number of recursive calls";
                }
            "#},
            JsValue::new(true),
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
    let _result_root = result.as_object().map(JsObject::root);

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
fn temporary_define_property_receivers_survive_collection() {
    let mut context = Context::default();
    context
        .register_global_builtin_callable(
            js_string!("collect"),
            0,
            NativeFunction::from_fn_ptr(|_, _, _| {
                boa_gc::force_collect();
                Ok(JsValue::undefined())
            }),
        )
        .unwrap();

    run_test_actions_with(
        [
            TestAction::run(indoc! {r#"
                let objectChecksum = 0;
                let accessorChecksum = 0;
                for (let i = 0; i < 2000; i++) {
                    const dataDescriptor = {
                        get value() {
                            collect();
                            return { i };
                        },
                        writable: true,
                        enumerable: true,
                        configurable: true,
                    };
                    const dataTarget = Object.defineProperty({}, "value", dataDescriptor);
                    objectChecksum += dataTarget.value.i;

                    const accessorDescriptor = {
                        get get() {
                            collect();
                            return function() {
                                return i;
                            };
                        },
                        enumerable: true,
                        configurable: true,
                    };
                    const accessorTarget = Object.defineProperty({}, "value", accessorDescriptor);
                    accessorChecksum += accessorTarget.value;
                    collect();
                }
            "#}),
            TestAction::assert_eq("objectChecksum", 1_999_000),
            TestAction::assert_eq("accessorChecksum", 1_999_000),
        ],
        &mut context,
    );
}

#[test]
fn existing_define_property_receivers_survive_collection() {
    let mut context = Context::default();
    context
        .register_global_builtin_callable(
            js_string!("collect"),
            0,
            NativeFunction::from_fn_ptr(|_, _, _| {
                boa_gc::force_collect();
                Ok(JsValue::undefined())
            }),
        )
        .unwrap();

    run_test_actions_with(
        [
            TestAction::run(indoc! {r#"
                const target = { value: { i: -1 } };
                let checksum = 0;
                for (let i = 0; i < 2000; i++) {
                    const descriptor = {
                        get value() {
                            collect();
                            return { i };
                        },
                        writable: true,
                        enumerable: true,
                        configurable: true,
                    };
                    Object.defineProperty(target, "value", descriptor);
                    checksum += target.value.i;
                    collect();
                }
            "#}),
            TestAction::assert_eq("checksum", 1_999_000),
        ],
        &mut context,
    );
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
