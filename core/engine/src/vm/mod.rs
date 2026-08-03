//! Boa's ECMAScript Virtual Machine
//!
//! The Virtual Machine (VM) handles generating instructions, then executing them.
//! This module will provide an instruction set for the AST to use, various traits,
//! plus an interpreter to execute those instructions

use crate::{
    Context, JsError, JsNativeError, JsObject, JsResult, JsString, JsValue, Module,
    builtins::promise::{PromiseCapability, ResolvingFunctions},
    environments::EnvironmentStack,
    module::ModuleEdge,
    native_function::{NativeCallContinuation, NativeCallSuspension},
    object::{JsFunction, JsFunctionEdge},
    realm::Realm,
    script::{Script, ScriptEdge},
};
use boa_gc::{Finalize, RootProvider, Rooted, Trace, Tracer, custom_trace};
use shadow_stack::ShadowStack;
use std::{future::Future, ops::ControlFlow, pin::Pin, ptr::NonNull, task};

#[cfg(test)]
use std::{cell::Cell, rc::Rc};

#[cfg(feature = "trace")]
use crate::sys::time::Instant;

#[cfg(feature = "trace")]
use std::fmt::Write as _;

#[allow(unused_imports)]
pub(crate) use opcode::{Instruction, InstructionIterator, Opcode};

pub(crate) use {
    call_frame::{CallFrameFlags, SuspendedCallFrame},
    code_block::{
        CodeBlockFlags, Constant, Handler, create_function_object, create_function_object_fast,
    },
    completion_record::CompletionRecord,
    inline_cache::InlineCache,
};

pub use runtime_limits::RuntimeLimits;
pub use {
    bytecode_contract::{
        BYTECODE_CONTRACT_VERSION, BytecodeConstant, BytecodeContract, BytecodeContractError,
        BytecodeContractSnapshot, BytecodeHandler, BytecodeInstruction, BytecodeOperand,
        BytecodeOperandValue, JitCompilationState, JitMetadataSnapshot,
    },
    call_frame::{CallFrame, GeneratorResumeKind},
    code_block::CodeBlock,
    source_info::{NativeSourceInfo, SourcePath},
};

pub(crate) mod bytecode_contract;
mod call_frame;
mod code_block;
mod completion_record;
pub(crate) mod frame_contract;
mod inline_cache;
mod runtime_limits;

pub(crate) mod opcode;
pub(crate) mod shadow_stack;
pub(crate) mod source_info;

#[cfg(feature = "flowgraph")]
pub mod flowgraph;

#[cfg(test)]
mod tests;

/// Test-only frame handoff probe installed by focused VM contract tests.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) enum FallbackProbe {
    #[default]
    Disabled,
    RoundTripOnPush,
}

/// Virtual Machine.
#[derive(Debug)]
pub struct Vm {
    /// Keeps the VM's native state registered as a root provider.
    ///
    /// The VM is boxed before this pointer is installed, so the address observed by
    /// the collector remains stable even when the owning `Context` is moved.
    /// Declared first so it is dropped before the state it points at.
    root_provider: Option<RootProvider>,

    /// The current call frame.
    ///
    /// Whenever a new frame is pushed, it will be swaped into this field.
    /// Then the old frame will get pushed to the [`Self::frames`] stack.
    /// Whenever the current frame gets poped, the last frame on the [`Self::frames`] stack will be swaped into this field.
    ///
    /// By default this is a dummy frame that gets pushed to [`Self::frames`] when the first real frame is pushed.
    pub(crate) frame: CallFrame,

    /// The stack for call frames.
    pub(crate) frames: Vec<CallFrame>,

    /// The VM's value stack.
    ///
    /// Boxed so that its address is stable: the collector holds a pointer to it for as
    /// long as this VM lives, and a [`Vm`] itself gets moved when a [`Context`] is built.
    pub(crate) stack: Box<Stack>,
    pub(crate) return_value: JsValue,

    /// When an error is thrown, the pending exception is set.
    ///
    /// If we throw an empty exception ([`None`]), this means that `return()` was called on a generator,
    /// propagating though the exception handlers and executing the finally code (if any).
    ///
    /// See [`ReThrow`](crate::vm::Opcode::ReThrow) and [`ReThrow`](crate::vm::Opcode::Exception) opcodes.
    ///
    /// This is also used to eliminates [`crate::JsNativeError`] to opaque conversion if not needed.
    pub(crate) pending_exception: Option<JsError>,
    pub(crate) environments: EnvironmentStack,
    pub(crate) runtime_limits: RuntimeLimits,

    /// This is used to assign a native (rust) function as the active function,
    /// because we don't push a frame for them.
    pub(crate) native_active_function: Option<JsObject>,

    /// Whether the active native function was entered through `[[Construct]]`.
    pub(crate) native_active_function_is_constructor_call: bool,

    /// A synchronous native call waiting for an out-of-band host result.
    pub(crate) pending_native_call: Option<NativeCallSuspension>,

    pub(crate) native_call_continuations: Vec<NativeCallBoundary>,

    pub(crate) native_call_continuation_active: bool,

    #[cfg(test)]
    pub(crate) instruction_count: Rc<Cell<u64>>,

    #[cfg(test)]
    pub(crate) fallback_probe: FallbackProbe,

    /// realm holds both the global object and the environment
    pub(crate) realm: Realm,

    pub(crate) shadow_stack: ShadowStack,

    #[cfg(feature = "trace")]
    pub(crate) trace: bool,
}

// SAFETY: `Vm::new` installs this value as a root provider only after placing it in
// a `Box`, and the provider is dropped before the VM fields it traces. The provider
// is read by the collector at allocation safepoints, when no VM mutation may alias
// the state being traced.
unsafe impl Trace for Vm {
    unsafe fn trace(&self, tracer: &mut Tracer) {
        // Rooted handles in frames, environments and the realm register themselves.
        // The fields below are the native VM edges that are not inside a GC object.
        unsafe {
            self.stack.trace(tracer);
            self.return_value.trace(tracer);
            self.pending_exception.trace(tracer);
            self.native_active_function.trace(tracer);
            self.pending_native_call.trace(tracer);
        }

        unsafe { self.frame.trace_native_roots(tracer) };
        for frame in &self.frames {
            unsafe { frame.trace_native_roots(tracer) };
        }
        unsafe { self.environments.trace_native_roots(tracer) };

        // Continuation closures are already rooted; their native-call placeholders
        // are ordinary edges and still need to be marked.
        for boundary in &self.native_call_continuations {
            unsafe { boundary.target.trace(tracer) };
        }
    }

    fn run_finalizer(&self) {}
}

impl Finalize for Vm {}

/// The stack holds the [`JsValue`]s that the VM is operationg on.
///
/// The stack is persistent across frames.
/// It's addressing is relative to the frame pointer.
///
/// The stack stores the following elements:
/// - The function prologue
///   - The `this` value of the function
///   - The function object itself
/// - The arguments of the function
/// - The local function registers
/// - Some manually pushed values like the return value of a function.
///
/// This is the stack layout:
///
/// ```text
///                      Setup by the caller
///   ┌─────────────────────────────────────────────────────────┐ ┌───── register pointer
///   ▼                                                         ▼ ▼
/// | -(2 + N): this | -(1 + N): func | -N: arg1 | ... | -1: argN | 0: reg1 | ... | K: reglK |
///   ▲                              ▲   ▲                      ▲   ▲                        ▲
///   └──────────────────────────────┘   └──────────────────────┘   └────────────────────────┘
///         function prologue                    arguments              Setup by the callee
///   ▲
///   └─ Frame pointer
/// ```
///
/// ### Example
///
/// The following function calls, generate the following stack:
///
/// ```JavaScript
/// function x(a) {
/// }
/// function y(b, c) {
///     return x(b + c)
/// }
///
/// y(1, 2)
/// ```
///
/// ```text
///     caller prologue    caller arguments   callee prologue   callee arguments
///   ┌─────────────────┐   ┌─────────┐   ┌─────────────────┐  ┌──────┐
///   ▼                 ▼   ▼         ▼   │                 ▼  ▼      ▼
/// | 0: undefined | 1: y | 2: 1 | 3: 2 | 4: undefined | 5: x | 6:  3 |
/// ▲                                   ▲                             ▲
/// │       caller register pointer ────┤                             │
/// │                                   │                 callee register pointer
/// │                             callee frame pointer
/// │
/// └─────  caller frame pointer
/// ```
#[derive(Clone, Debug, Trace, Finalize)]
pub(crate) struct Stack {
    stack: Vec<JsValue>,
}

impl Stack {
    /// Creates a new stack with the given capacity.
    fn new(capacity: usize) -> Self {
        Self {
            stack: Vec::with_capacity(capacity),
        }
    }

    fn contains_object(&self, object: &JsObject) -> bool {
        self.stack.iter().any(|value| {
            value
                .as_object()
                .is_some_and(|value| JsObject::equals(&value, object))
        })
    }

    fn replace_object(&mut self, object: &JsObject, replacement: JsValue) -> bool {
        let Some(slot) = self.stack.iter_mut().rev().find(|value| {
            value
                .as_object()
                .is_some_and(|value| JsObject::equals(&value, object))
        }) else {
            return false;
        };
        *slot = replacement;
        true
    }

    /// Truncate the stack to the given frame.
    pub(crate) fn truncate_to_frame(&mut self, frame: &CallFrame) {
        self.stack.truncate(frame.frame_pointer());
    }

    /// Split the stack at the given frame.
    pub(crate) fn split_off_frame(&mut self, frame: &CallFrame) -> Self {
        let frame_pointer = frame.frame_pointer();
        Self {
            stack: self.stack.split_off(frame_pointer),
        }
    }

    /// Get the `this` value of the given frame.
    pub(crate) fn get_this(&self, frame: &CallFrame) -> JsValue {
        self.stack[frame.this_index()].clone()
    }

    /// Set the `this` value of the given frame.
    pub(crate) fn set_this(&mut self, frame: &CallFrame, this: JsValue) {
        self.stack[frame.this_index()] = this;
    }

    /// Get the function object of the given frame.
    pub(crate) fn get_function(&self, frame: &CallFrame) -> Option<JsObject> {
        if let Some(object) = self.stack[frame.function_index()].as_object() {
            return Some(object.clone());
        }
        None
    }

    /// Get the function arguments of the given frame.
    pub(crate) fn get_arguments(&self, frame: &CallFrame) -> &[JsValue] {
        &self.stack[frame.arguments_range()]
    }

    /// Get a single function argument of the given frame by index.
    pub(crate) fn get_argument(&self, frame: &CallFrame, index: usize) -> Option<&JsValue> {
        self.get_arguments(frame).get(index)
    }

    /// Get the rest arguments of the given frame.
    pub(crate) fn pop_rest_arguments(&mut self, frame: &CallFrame) -> Option<Vec<JsValue>> {
        let argument_count = frame.argument_count as usize;
        let param_count = frame.code_block().parameter_length as usize;
        if argument_count < param_count {
            return None;
        }
        let rp = frame.rp as usize;
        let rest_count = argument_count - param_count + 1;

        Some(self.stack.drain((rp - rest_count)..rp).collect())
    }

    /// Set the promise capability for the given frame.
    #[track_caller]
    pub(crate) fn set_promise_capability(
        &mut self,
        frame: &CallFrame,
        promise_capability: Option<&PromiseCapability>,
    ) {
        debug_assert!(
            frame.code_block().is_async(),
            "Only async functions have a promise capability"
        );

        self.stack[frame.promise_capability_promise_register_index()] = promise_capability
            .map(PromiseCapability::promise)
            .cloned()
            .map_or_else(JsValue::undefined, Into::into);
        self.stack[frame.promise_capability_resolve_register_index()] = promise_capability
            .map(PromiseCapability::resolve)
            .map(JsFunctionEdge::root)
            .map_or_else(JsValue::undefined, Into::into);
        self.stack[frame.promise_capability_reject_register_index()] = promise_capability
            .map(PromiseCapability::reject)
            .map(JsFunctionEdge::root)
            .map_or_else(JsValue::undefined, Into::into);
    }

    /// Get the promise capability for the given frame.
    #[track_caller]
    pub(crate) fn get_promise_capability(&self, frame: &CallFrame) -> Option<PromiseCapability> {
        if !frame.code_block().is_async() {
            return None;
        }

        let promise = self
            .stack
            .get(frame.promise_capability_promise_register_index())
            .expect("stack must have a promise capability")
            .as_object()?;
        let resolve = self
            .stack
            .get(frame.promise_capability_resolve_register_index())
            .expect("stack must have a resolve function")
            .as_object()
            .and_then(JsFunction::from_object)?;
        let reject = self
            .stack
            .get(frame.promise_capability_reject_register_index())
            .expect("stack must have a reject function")
            .as_object()
            .and_then(JsFunction::from_object)?;

        Some(PromiseCapability {
            promise,
            functions: ResolvingFunctions { resolve, reject }.into_edge(),
        })
    }

    /// Set the async generator object for the given frame.
    #[track_caller]
    pub(crate) fn set_async_generator_object(&mut self, frame: &CallFrame, object: JsObject) {
        self.stack[frame.async_generator_object_register_index()] = object.into();
    }

    /// Get the async generator object for the given frame.
    #[track_caller]
    pub(crate) fn async_generator_object<H, E, R, A>(
        &self,
        frame: &CallFrame<H, E, R, A>,
    ) -> Option<JsObject>
    where
        H: std::ops::Deref<Target = CodeBlock>,
    {
        if !frame.code_block().is_async_generator() {
            return None;
        }

        self.stack
            .get(frame.rp as usize + CallFrame::ASYNC_GENERATOR_OBJECT_REGISTER_INDEX)
            .expect("stack must have an async generator object")
            .as_object()
    }

    /// Push a value on the stack.
    pub(crate) fn push<T>(&mut self, value: T)
    where
        T: Into<JsValue>,
    {
        self.stack.push(value.into());
    }

    /// Pop a value off the stack.
    ///
    /// # Panics
    ///
    /// If there is nothing to pop, then this will panic.
    #[track_caller]
    pub(crate) fn pop(&mut self) -> JsValue {
        self.stack.pop().expect("stack was empty")
    }

    /// Pop the function arguments according to the calling convention.
    /// This will pop the last `argument_count` values from the stack.
    pub(crate) fn calling_convention_pop_arguments(
        &mut self,
        argument_count: usize,
    ) -> Vec<JsValue> {
        let index = self.stack.len() - argument_count;
        self.stack.split_off(index)
    }

    /// Push the function arguments according to the calling convention.
    /// This will push the given values onto the stack.
    pub(crate) fn calling_convention_push_arguments(&mut self, values: &[JsValue]) {
        self.stack.extend_from_slice(values);
    }

    /// Get the function object at the top of the stack according to the calling convention.
    #[track_caller]
    pub(crate) fn calling_convention_get_function(&self, argument_count: usize) -> &JsValue {
        let index = self.stack.len() - 1 - argument_count;
        self.stack
            .get(index)
            .expect("invalid calling convention function index")
    }

    /// Set the function object value at the top of the stack according to the calling convention.
    #[track_caller]
    pub(crate) fn calling_convention_set_function(
        &mut self,
        argument_count: usize,
        function: JsValue,
    ) {
        let index = self.stack.len() - 1 - argument_count;
        self.stack[index] = function;
    }

    /// Set the `this` value at the top of the stack according to the calling convention.
    #[track_caller]
    pub(crate) fn calling_convention_set_this(&mut self, argument_count: usize, function: JsValue) {
        let index = self.stack.len() - 2 - argument_count;
        self.stack[index] = function;
    }

    /// Insert the function arguments at the top of the stack according to the calling convention.
    /// This will insert the given values at the position of the function arguments.
    pub(crate) fn calling_convention_insert_arguments(
        &mut self,
        existing_argument_count: usize,
        arguments: &[JsValue],
    ) {
        let index = self.stack.len() - existing_argument_count;
        self.stack.splice(index..index, arguments.iter().cloned());
    }

    #[cfg(feature = "trace")]
    /// Display the stack trace of the current frame.
    fn display_trace(&self, frame: &CallFrame, frame_count: usize) -> String {
        let mut string = String::from("[ ");
        for (i, (j, value)) in self.stack.iter().enumerate().rev().enumerate() {
            match value {
                value if value.is_callable() => string.push_str("[function]"),
                value if value.is_object() => string.push_str("[object]"),
                value => string.push_str(&value.display().to_string()),
            }

            if frame.frame_pointer() == j {
                let _ = write!(string, " |{frame_count}|");
            } else if i + 1 != self.stack.len() {
                string.push(',');
            }

            string.push(' ');
        }

        string.push(']');
        string
    }
}

/// Active runnable in the current vm context.
#[derive(Debug, Clone, Finalize)]
#[doc(hidden)]
pub enum ActiveRunnable {
    /// A script record.
    Script(Script),
    /// A module record.
    Module(Module),
}

#[derive(Debug, Clone, Finalize)]
pub(crate) enum ActiveRunnableEdge {
    Script(ScriptEdge),
    Module(ModuleEdge),
}

unsafe impl Trace for ActiveRunnableEdge {
    custom_trace!(this, mark, {
        match this {
            Self::Script(script) => mark(script),
            Self::Module(module) => mark(module),
        }
    });
}

impl ActiveRunnable {
    pub(crate) fn to_edge(&self) -> ActiveRunnableEdge {
        match self {
            Self::Script(script) => ActiveRunnableEdge::Script(script.to_edge()),
            Self::Module(module) => ActiveRunnableEdge::Module(module.to_edge()),
        }
    }
}

impl ActiveRunnableEdge {
    pub(crate) fn to_rooted(&self) -> ActiveRunnable {
        match self {
            Self::Script(script) => ActiveRunnable::Script(script.to_rooted()),
            Self::Module(module) => ActiveRunnable::Module(module.to_rooted()),
        }
    }
}

impl Vm {
    /// Creates a new virtual machine.
    pub(crate) fn new(realm: Realm) -> Box<Self> {
        let stack = Box::new(Stack::new(1024));
        let mut vm = Box::new(Self {
            root_provider: None,
            frames: Vec::with_capacity(16),
            frame: CallFrame::new_rooted(
                Rooted::new(CodeBlock::new(JsString::default(), 0, true)),
                None,
                EnvironmentStack::new(realm.environment()),
                realm.clone(),
            ),
            stack,
            return_value: JsValue::undefined(),
            environments: EnvironmentStack::new(realm.environment()),
            pending_exception: None,
            runtime_limits: RuntimeLimits::default(),
            native_active_function: None,
            native_active_function_is_constructor_call: false,
            pending_native_call: None,
            native_call_continuations: Vec::new(),
            native_call_continuation_active: false,
            #[cfg(test)]
            instruction_count: Rc::new(Cell::new(0)),
            #[cfg(test)]
            fallback_probe: FallbackProbe::Disabled,
            realm,
            shadow_stack: ShadowStack::default(),
            #[cfg(feature = "trace")]
            trace: false,
        });

        // SAFETY: `vm` is boxed before registration and is never moved until this
        // provider is dropped with the VM itself.
        vm.root_provider = Some(unsafe { RootProvider::register(NonNull::from(&*vm)) });
        vm
    }

    #[track_caller]
    pub(crate) fn set_register(&mut self, index: usize, value: JsValue) {
        self.stack.stack[self.frame.rp as usize + index] = value;
    }

    /// Moves the value out of a register, leaving `undefined` behind.
    ///
    /// Taking rather than cloning is what lets an operation see a reference count of
    /// one for a value only this register holds, which is the condition for mutating
    /// it in place. Only sound when the register's value is dead, and when nothing
    /// between the take and the write-back can fail: on an early return the register
    /// is left holding `undefined` rather than its old value.
    #[track_caller]
    pub(crate) fn take_register(&mut self, index: usize) -> JsValue {
        std::mem::take(&mut self.stack.stack[self.frame.rp as usize + index])
    }

    #[track_caller]
    pub(crate) fn get_register(&self, index: usize) -> &JsValue {
        self.stack
            .stack
            .get(self.frame.rp as usize + index)
            .expect("registers must be initialized")
    }

    /// Retrieves the VM frame.
    #[track_caller]
    pub(crate) fn frame(&self) -> &CallFrame {
        &self.frame
    }

    /// Retrieves the VM frame mutably.
    #[track_caller]
    pub(crate) fn frame_mut(&mut self) -> &mut CallFrame {
        &mut self.frame
    }

    pub(crate) fn push_frame(&mut self, mut frame: CallFrame) {
        frame.code_block.jit_metadata.record_interpreter_entry();
        let current_stack_length = self.stack.stack.len();
        frame.set_register_pointer(current_stack_length as u32);
        std::mem::swap(&mut self.environments, &mut frame.environments);
        std::mem::swap(&mut self.realm, &mut frame.realm);

        // NOTE: We need to check if we already pushed the registers,
        //       since generator-like functions push the same call
        //       frame with pre-built stack.
        if !frame.registers_already_pushed() {
            self.stack.stack.resize_with(
                current_stack_length + frame.code_block.register_count as usize,
                JsValue::undefined,
            );
        }

        // Keep carrying the last active runnable in case the current callframe
        // yields.
        if frame.active_runnable.is_none() {
            frame
                .active_runnable
                .clone_from(&self.frame.active_runnable);
        }

        self.shadow_stack
            .push_bytecode(self.frame.pc, frame.code_block().source_info.clone());

        std::mem::swap(&mut self.frame, &mut frame);
        self.frames.push(frame);

        #[cfg(test)]
        if matches!(self.fallback_probe, FallbackProbe::RoundTripOnPush) {
            let layout = self
                .verify_interpreter_frame_layout()
                .expect("test fallback probe must verify every pushed frame");
            let mut registers = vec![JsValue::undefined(); layout.register_count() as usize];
            let state = self
                .capture_interpreter_frame(&layout, &mut registers)
                .expect("test fallback probe must capture every pushed frame");
            self.restore_interpreter_frame(state)
                .expect("test fallback probe must restore every pushed frame");
        }
    }

    pub(crate) fn push_frame_with_stack(
        &mut self,
        frame: CallFrame,
        this: JsValue,
        function: JsValue,
    ) {
        self.stack.push(this);
        self.stack.push(function);

        self.push_frame(frame);
    }

    pub(crate) fn pop_frame(&mut self) -> Option<CallFrame> {
        if let Some(mut frame) = self.frames.pop() {
            self.shadow_stack.pop();

            std::mem::swap(&mut self.frame, &mut frame);
            std::mem::swap(&mut self.environments, &mut frame.environments);
            std::mem::swap(&mut self.realm, &mut frame.realm);
            Some(frame)
        } else {
            None
        }
    }

    /// Handles an exception thrown at position `pc`.
    ///
    /// Returns `true` if the exception was handled, `false` otherwise.
    #[inline]
    pub(crate) fn handle_exception_at(&mut self, pc: u32) -> bool {
        let frame = self.frame_mut();
        let Some((_, handler)) = frame.code_block().find_handler(pc) else {
            return false;
        };

        let catch_address = handler.handler();
        let environment_sp = frame.env_fp + handler.environment_count;

        // Go to handler location.
        frame.pc = catch_address;

        self.environments.truncate(environment_sp as usize);

        true
    }

    pub(crate) fn get_return_value(&self) -> JsValue {
        self.return_value.clone()
    }

    pub(crate) fn set_return_value(&mut self, value: JsValue) {
        self.return_value = value;
    }

    pub(crate) fn take_return_value(&mut self) -> JsValue {
        std::mem::take(&mut self.return_value)
    }
}

#[derive(Debug, Finalize)]
pub(crate) struct NativeCallBoundary {
    pub(crate) target: NativeCallBoundaryTarget,
    pub(crate) continuation: NativeCallContinuation,
}

#[derive(Debug, Trace, Finalize)]
pub(crate) enum NativeCallBoundaryTarget {
    FrameDepth(usize),
    NativePlaceholder(JsObject),
}

#[allow(clippy::print_stdout)]
#[cfg(feature = "trace")]
impl Context {
    const COLUMN_WIDTH: usize = 26;
    const TIME_COLUMN_WIDTH: usize = Self::COLUMN_WIDTH / 2;
    const OPCODE_COLUMN_WIDTH: usize = Self::COLUMN_WIDTH;
    const OPERAND_COLUMN_WIDTH: usize = Self::COLUMN_WIDTH;
    const NUMBER_OF_COLUMNS: usize = 4;

    pub(crate) fn trace_call_frame(&self) {
        let frame = self.vm.frame();
        let msg = if self.vm.frames.is_empty() {
            " VM Start ".to_string()
        } else {
            format!(
                " Call Frame -- {} ",
                frame.code_block().name().to_std_string_escaped()
            )
        };

        println!("{}", **frame.code_block());
        println!(
            "{msg:-^width$}",
            width = Self::COLUMN_WIDTH * Self::NUMBER_OF_COLUMNS - 10
        );
        println!(
            "{:<TIME_COLUMN_WIDTH$} {:<OPCODE_COLUMN_WIDTH$} {:<OPERAND_COLUMN_WIDTH$} Stack\n",
            "Time",
            "Opcode",
            "Operands",
            TIME_COLUMN_WIDTH = Self::TIME_COLUMN_WIDTH,
            OPCODE_COLUMN_WIDTH = Self::OPCODE_COLUMN_WIDTH,
            OPERAND_COLUMN_WIDTH = Self::OPERAND_COLUMN_WIDTH,
        );
    }

    fn trace_execute_instruction<F>(
        &mut self,
        f: F,
        opcode: Opcode,
    ) -> ControlFlow<CompletionRecord>
    where
        F: FnOnce(&mut Context, Opcode) -> ControlFlow<CompletionRecord>,
    {
        let frame = self.vm.frame();
        let (instruction, _) = frame
            .code_block
            .bytecode
            .next_instruction(frame.pc as usize);
        let operands = self
            .vm
            .frame()
            .code_block()
            .instruction_operands(&instruction);

        match opcode {
            Opcode::Call
            | Opcode::CallSpread
            | Opcode::CallEval
            | Opcode::CallEvalSpread
            | Opcode::New
            | Opcode::NewSpread
            | Opcode::Return
            | Opcode::SuperCall
            | Opcode::SuperCallSpread
            | Opcode::SuperCallDerived => {
                println!();
            }
            _ => {}
        }

        let instant = Instant::now();
        let result = self.execute_instruction(f, opcode);
        let duration = instant.elapsed();

        let stack = self
            .vm
            .stack
            .display_trace(self.vm.frame(), self.vm.frames.len() - 1);

        println!(
            "{:<TIME_COLUMN_WIDTH$} {:<OPCODE_COLUMN_WIDTH$} {operands:<OPERAND_COLUMN_WIDTH$} {stack}",
            format!("{}μs", duration.as_micros()),
            format!("{}", opcode.as_str()),
            TIME_COLUMN_WIDTH = Self::TIME_COLUMN_WIDTH,
            OPCODE_COLUMN_WIDTH = Self::OPCODE_COLUMN_WIDTH,
            OPERAND_COLUMN_WIDTH = Self::OPERAND_COLUMN_WIDTH,
        );

        result
    }
}

impl Context {
    fn apply_native_call_completion(
        &mut self,
        placeholder: &JsObject,
        result: JsResult<JsValue>,
    ) -> ControlFlow<CompletionRecord> {
        let continuation = self
            .vm
            .native_call_continuations
            .last()
            .and_then(|boundary| match &boundary.target {
                NativeCallBoundaryTarget::NativePlaceholder(target)
                    if JsObject::equals(target, placeholder) =>
                {
                    Some(())
                }
                _ => None,
            })
            .and_then(|()| self.vm.native_call_continuations.pop());

        match result {
            Ok(value) => {
                if self.vm.stack.replace_object(placeholder, value) {
                    if let Some(boundary) = continuation {
                        let value = self.vm.stack.pop();
                        let continuation_depth = self.vm.native_call_continuations.len();
                        let continuation_was_active = self.vm.native_call_continuation_active;
                        self.vm.native_call_continuation_active = true;
                        let result = boundary.continuation.call(Ok(value), self);
                        self.vm.native_call_continuation_active = continuation_was_active;
                        match result {
                            Ok(value) => {
                                if self.vm.native_call_continuations.len() == continuation_depth {
                                    self.vm.stack.push(value);
                                }
                                ControlFlow::Continue(())
                            }
                            Err(error) => self.handle_error(error),
                        }
                    } else {
                        ControlFlow::Continue(())
                    }
                } else {
                    self.handle_error(
                        JsNativeError::error()
                            .with_message(
                                "suspended native call result was consumed before VM suspension",
                            )
                            .into(),
                    )
                }
            }
            Err(error) => {
                if self
                    .vm
                    .stack
                    .replace_object(placeholder, JsValue::undefined())
                {
                    if let Some(boundary) = continuation {
                        drop(self.vm.stack.pop());
                        let continuation_depth = self.vm.native_call_continuations.len();
                        let continuation_was_active = self.vm.native_call_continuation_active;
                        self.vm.native_call_continuation_active = true;
                        let result = boundary.continuation.call(Err(error), self);
                        self.vm.native_call_continuation_active = continuation_was_active;
                        match result {
                            Ok(value) => {
                                if self.vm.native_call_continuations.len() == continuation_depth {
                                    self.vm.stack.push(value);
                                }
                                ControlFlow::Continue(())
                            }
                            Err(error) => self.handle_error(error),
                        }
                    } else {
                        self.handle_error(error)
                    }
                } else {
                    self.handle_error(
                        JsNativeError::error()
                            .with_message(
                                "suspended native call result was consumed before VM suspension",
                            )
                            .into(),
                    )
                }
            }
        }
    }

    fn execute_instruction<F>(&mut self, f: F, opcode: Opcode) -> ControlFlow<CompletionRecord>
    where
        F: FnOnce(&mut Context, Opcode) -> ControlFlow<CompletionRecord>,
    {
        f(self, opcode)
    }

    fn execute_one<F>(&mut self, f: F, opcode: Opcode) -> ControlFlow<CompletionRecord>
    where
        F: FnOnce(&mut Context, Opcode) -> ControlFlow<CompletionRecord>,
    {
        #[cfg(test)]
        self.vm
            .instruction_count
            .set(self.vm.instruction_count.get() + 1);

        #[cfg(feature = "fuzz")]
        {
            if self.instructions_remaining == 0 {
                return ControlFlow::Break(CompletionRecord::Throw(JsError::from_native(
                    JsNativeError::no_instructions_remain(),
                )));
            }
            self.instructions_remaining -= 1;
        }

        #[cfg(feature = "trace")]
        if self.vm.trace || self.vm.frame().code_block.traceable() {
            self.trace_execute_instruction(f, opcode)
        } else {
            self.execute_instruction(f, opcode)
        }

        #[cfg(not(feature = "trace"))]
        self.execute_instruction(f, opcode)
    }

    fn handle_error(&mut self, mut err: JsError) -> ControlFlow<CompletionRecord> {
        // If we hit the execution step limit, bubble up the error to the
        // (Rust) caller instead of trying to handle as an exception.
        if !err.is_catchable() {
            if err.backtrace.is_none() {
                err.backtrace = Some(
                    self.vm
                        .shadow_stack
                        .take(self.vm.runtime_limits.backtrace_limit(), self.vm.frame.pc),
                );
            }

            let mut frame = None;
            let mut env_fp = self.vm.environments.len();
            loop {
                if self.vm.frame.exit_early() {
                    break;
                }

                env_fp = self.vm.frame.env_fp as usize;

                let Some(f) = self.vm.pop_frame() else {
                    break;
                };
                frame = Some(f);
            }
            self.vm.environments.truncate(env_fp);
            if let Some(frame) = frame {
                self.vm.stack.truncate_to_frame(&frame);
            }
            return ControlFlow::Break(CompletionRecord::Throw(err));
        }

        // Note: -1 because we increment after fetching the opcode.
        let pc = self.vm.frame().pc.saturating_sub(1);
        if self.vm.handle_exception_at(pc) {
            self.vm.pending_exception = Some(err);
            return ControlFlow::Continue(());
        }

        // Inject realm before crossing the function boundry
        let err = err.inject_realm(self.realm());

        self.vm.pending_exception = Some(err);
        self.handle_throw()
    }

    fn handle_return(&mut self) -> ControlFlow<CompletionRecord> {
        let exit_early = self.vm.frame().exit_early();
        self.vm.stack.truncate_to_frame(&self.vm.frame);

        let result = self.vm.take_return_value();
        let frame_depth = self.vm.frames.len();
        if let Some(boundary) = self.vm.native_call_continuations.pop_if(
            |boundary| matches!(boundary.target, NativeCallBoundaryTarget::FrameDepth(depth) if depth == frame_depth),
        ) {
            self.vm.pop_frame().expect("callback frame must exist");
            let continuation_depth = self.vm.native_call_continuations.len();
            let continuation_was_active = self.vm.native_call_continuation_active;
            self.vm.native_call_continuation_active = true;
            let continuation_result = boundary.continuation.call(Ok(result), self);
            self.vm.native_call_continuation_active = continuation_was_active;
            return match continuation_result {
                Ok(value) => {
                    if self.vm.native_call_continuations.len() == continuation_depth {
                        if self.vm.frames.is_empty() || exit_early {
                            return ControlFlow::Break(CompletionRecord::Normal(value));
                        }
                        self.vm.stack.push(value);
                    }
                    ControlFlow::Continue(())
                }
                Err(error) if self.vm.frames.is_empty() || exit_early => {
                    ControlFlow::Break(CompletionRecord::Throw(error))
                }
                Err(error) => self.handle_error(error),
            };
        }
        if exit_early {
            return ControlFlow::Break(CompletionRecord::Normal(result));
        }

        self.vm.stack.push(result);
        self.vm.pop_frame().expect("frame must exist");
        ControlFlow::Continue(())
    }

    fn handle_yield(&mut self) -> ControlFlow<CompletionRecord> {
        let result = self.vm.take_return_value();
        if self.vm.frame().exit_early() {
            return ControlFlow::Break(CompletionRecord::Return(result));
        }

        self.vm.stack.push(result);
        self.vm.pop_frame().expect("frame must exist");
        ControlFlow::Continue(())
    }

    fn handle_throw(&mut self) -> ControlFlow<CompletionRecord> {
        if let Some(err) = &mut self.vm.pending_exception
            && err.backtrace.is_none()
        {
            err.backtrace = Some(
                self.vm
                    .shadow_stack
                    .take(self.vm.runtime_limits.backtrace_limit(), self.vm.frame.pc),
            );
        }

        if let Some(result) = self.handle_native_continuation_throw() {
            return result;
        }

        let mut env_fp = self.vm.frame().env_fp;
        if self.vm.frame().exit_early() {
            self.vm.environments.truncate(env_fp as usize);
            self.vm.stack.truncate_to_frame(&self.vm.frame);
            return ControlFlow::Break(CompletionRecord::Throw(
                self.vm
                    .pending_exception
                    .take()
                    .expect("Err must exist for a CompletionType::Throw"),
            ));
        }

        let mut frame = self.vm.pop_frame().expect("frame must exist");

        loop {
            env_fp = self.vm.frame.env_fp;
            let pc = self.vm.frame.pc;
            let exit_early = self.vm.frame.exit_early();

            if self.vm.handle_exception_at(pc) {
                return ControlFlow::Continue(());
            }

            if let Some(result) = self.handle_native_continuation_throw() {
                return result;
            }

            if exit_early {
                return ControlFlow::Break(CompletionRecord::Throw(
                    self.vm
                        .pending_exception
                        .take()
                        .expect("Err must exist for a CompletionType::Throw"),
                ));
            }

            let Some(f) = self.vm.pop_frame() else {
                break;
            };
            frame = f;
        }
        self.vm.environments.truncate(env_fp as usize);
        self.vm.stack.truncate_to_frame(&frame);
        ControlFlow::Continue(())
    }

    fn handle_native_continuation_throw(&mut self) -> Option<ControlFlow<CompletionRecord>> {
        let frame_depth = self.vm.frames.len();
        let boundary = self.vm.native_call_continuations.pop_if(
            |boundary| matches!(boundary.target, NativeCallBoundaryTarget::FrameDepth(depth) if depth == frame_depth),
        )?;
        self.vm.environments.truncate(self.vm.frame.env_fp as usize);
        self.vm.stack.truncate_to_frame(&self.vm.frame);
        let exit_early = self.vm.frame().exit_early();
        self.vm.pop_frame().expect("callback frame must exist");
        let error = self
            .vm
            .pending_exception
            .take()
            .expect("a thrown completion must have an exception");
        let continuation_depth = self.vm.native_call_continuations.len();
        let continuation_was_active = self.vm.native_call_continuation_active;
        self.vm.native_call_continuation_active = true;
        let continuation_result = boundary.continuation.call(Err(error), self);
        self.vm.native_call_continuation_active = continuation_was_active;
        Some(match continuation_result {
            Ok(value) => {
                if self.vm.native_call_continuations.len() == continuation_depth {
                    if self.vm.frames.is_empty() || exit_early {
                        return Some(ControlFlow::Break(CompletionRecord::Normal(value)));
                    }
                    self.vm.stack.push(value);
                }
                ControlFlow::Continue(())
            }
            Err(error) if self.vm.frames.is_empty() || exit_early => {
                ControlFlow::Break(CompletionRecord::Throw(error))
            }
            Err(error) => self.handle_error(error),
        })
    }

    /// Runs the current frame to completion, yielding to the caller each time `budget`
    /// "clock cycles" have passed.
    #[allow(clippy::future_not_send)]
    pub(crate) async fn run_async_with_budget(&mut self, budget: u32) -> CompletionRecord {
        #[cfg(feature = "trace")]
        if self.vm.trace {
            self.trace_call_frame();
        }

        let mut runtime_budget: u32 = budget;

        while let Some(byte) = self
            .vm
            .frame
            .code_block
            .bytecode
            .bytecode
            .get(self.vm.frame.pc as usize)
        {
            let opcode = Opcode::decode(*byte);

            match self.execute_one(
                |context, opcode| {
                    context.execute_bytecode_instruction_with_budget(&mut runtime_budget, opcode)
                },
                opcode,
            ) {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(value) => return value,
            }

            while let Some(suspension) = self.vm.pending_native_call.take() {
                let placeholder = suspension.placeholder();
                let placeholder_was_consumed = !self.vm.stack.contains_object(&placeholder);
                if placeholder_was_consumed {
                    let error = JsNativeError::error()
                        .with_message("native call cannot suspend from this execution path")
                        .into();
                    let _ = suspension.resume(Err(error));
                }
                let result = suspension.wait().await;
                let completion = if placeholder_was_consumed {
                    match result {
                        Ok(_) => unreachable!("the rejected suspension completed successfully"),
                        Err(error) => self.handle_error(error),
                    }
                } else {
                    self.apply_native_call_completion(&placeholder, result)
                };
                if let ControlFlow::Break(value) = completion {
                    return value;
                }
            }

            if runtime_budget == 0 {
                runtime_budget = budget;
                yield_now().await;
            }
        }

        CompletionRecord::Throw(JsError::from_native(JsNativeError::error()))
    }

    pub(crate) fn run(&mut self) -> CompletionRecord {
        #[cfg(feature = "trace")]
        if self.vm.trace {
            self.trace_call_frame();
        }

        while let Some(byte) = self
            .vm
            .frame
            .code_block
            .bytecode
            .bytecode
            .get(self.vm.frame.pc as usize)
        {
            let opcode = Opcode::decode(*byte);

            match self.execute_one(Self::execute_bytecode_instruction, opcode) {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(value) => return value,
            }

            if let Some(suspension) = self.vm.pending_native_call.take() {
                let placeholder = suspension.placeholder();
                let result = suspension.try_take_result().unwrap_or_else(|| {
                    let error = JsNativeError::error()
                        .with_message(
                            "native call suspension requires asynchronous script evaluation",
                        )
                        .into();
                    let _ = suspension.resume(Err(error));
                    suspension
                        .try_take_result()
                        .expect("the suspension was just completed")
                });
                suspension.release_roots();
                if let ControlFlow::Break(value) =
                    self.apply_native_call_completion(&placeholder, result)
                {
                    return value;
                }
            }
        }

        CompletionRecord::Throw(JsError::from_native(JsNativeError::error()))
    }

    /// Checks if we haven't exceeded the defined runtime limits.
    pub(crate) fn check_runtime_limits(&self) -> JsResult<()> {
        // Must throw if the number of recursive calls exceeds the defined limit.
        if self.vm.runtime_limits.recursion_limit() <= self.vm.frames.len() {
            return Err(JsNativeError::runtime_limit()
                .with_message("exceeded maximum number of recursive calls")
                .into());
        }
        // Must throw if the stack size exceeds the defined maximum length.
        if self.vm.runtime_limits.stack_size_limit() <= self.vm.stack.stack.len() {
            return Err(JsNativeError::runtime_limit()
                .with_message("exceeded maximum call stack length")
                .into());
        }

        Ok(())
    }
}

/// Yields once to the executor.
fn yield_now() -> impl Future<Output = ()> {
    struct YieldNow(bool);

    impl Future for YieldNow {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> task::Poll<Self::Output> {
            if self.0 {
                task::Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                task::Poll::Pending
            }
        }
    }

    YieldNow(false)
}
