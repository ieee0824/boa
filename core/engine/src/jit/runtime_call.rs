//! Generated-code to Rust runtime-call and allocation boundary.

use std::{
    cell::UnsafeCell,
    error::Error,
    ffi::c_void,
    fmt,
    marker::PhantomData,
    mem::{offset_of, size_of},
    ptr::NonNull,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use boa_gc::{Finalize, RootProvider, Trace, Tracer};

use crate::{
    Context, JsError, JsResult, JsValue, NativeFunction,
    error::JsNativeError,
    object::{FunctionObjectBuilder, JsObject, builtins::JsArray},
};

use super::{
    ActiveJitFrame, ExecutableMemory, FrameCaller, JitError, JitExceptionUnwindPlan,
    JitExceptionUnwindTarget, JitFrameChain, JitFrameDescriptor, JitFrameDescriptorId,
    JitFrameHeader, JitPcTable, Safepoint, SafepointKind, StackMap, ValueLocation, WritableMemory,
};

static NEXT_DESCRIPTOR_ID: AtomicU64 = AtomicU64::new(1 << 32);
static NEXT_FRAME_ID: AtomicU64 = AtomicU64::new(1 << 32);

const RUNTIME_CALL_RETURN_PC: u32 = 9;

/// Allocation operation implemented by the fixed JIT runtime-call table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitAllocationKind {
    /// Ordinary object with the current realm's Object prototype.
    Object,
    /// Array initialized from the spilled argument values.
    Array,
    /// Native no-op closure in the current realm.
    Closure,
}

/// Failure before or while crossing the generated runtime-call boundary.
#[derive(Debug)]
pub enum RuntimeCallError {
    /// Native code publication or execution is unsupported or failed.
    Jit(JitError),
    /// More arguments were supplied than the frame descriptor can describe.
    TooManyArguments {
        /// Number of arguments supplied by the generated call site.
        supplied: usize,
        /// Number of spill slots reserved by this runtime boundary.
        capacity: u32,
    },
    /// The deterministic allocation budget was exhausted.
    AllocationFailure,
    /// The generated helper returned without publishing a result.
    MissingResult,
    /// A Rust helper panicked; unwinding never crossed the generated ABI.
    HelperPanicked,
    /// Reserving one code object per exact live-slot count would be excessive.
    FrameCapacityTooLarge {
        /// Requested number of argument spill slots.
        requested: u32,
        /// Largest supported number of argument spill slots.
        maximum: u32,
    },
}

impl fmt::Display for RuntimeCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Jit(error) => write!(formatter, "JIT runtime call failed: {error}"),
            Self::TooManyArguments { supplied, capacity } => write!(
                formatter,
                "JIT runtime call received {supplied} arguments but its frame holds {capacity}"
            ),
            Self::AllocationFailure => formatter.write_str("JIT allocation budget exhausted"),
            Self::MissingResult => formatter.write_str("JIT runtime helper returned no result"),
            Self::HelperPanicked => formatter.write_str("JIT runtime helper panicked"),
            Self::FrameCapacityTooLarge { requested, maximum } => write!(
                formatter,
                "JIT runtime frame requested {requested} spill slots; maximum is {maximum}"
            ),
        }
    }
}

impl Error for RuntimeCallError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Jit(error) => Some(error),
            _ => None,
        }
    }
}

impl RuntimeCallError {
    /// Converts a generated-boundary failure into a JavaScript error that can
    /// travel through the normal interpreter catch/finally machinery.
    #[must_use]
    pub fn into_js_error(self) -> JsError {
        match self {
            Self::AllocationFailure => JsNativeError::range()
                .with_message("JIT allocation budget exhausted")
                .into(),
            error => JsNativeError::error()
                .with_message(error.to_string())
                .into(),
        }
    }
}

impl From<JitError> for RuntimeCallError {
    fn from(error: JitError) -> Self {
        Self::Jit(error)
    }
}

/// Observable fast/slow allocation and generated-call counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitRuntimeCallDiagnostics {
    /// Entries through generated machine code.
    pub generated_calls: u64,
    /// Allocations completed while fast-path credits remained.
    pub fast_allocations: u64,
    /// Allocations that collected and refilled fast-path credits.
    pub slow_allocations: u64,
    /// Nested generated calls made while another JIT frame was active.
    pub nested_calls: u64,
    /// Runtime exceptions returned through the common result slot.
    pub exceptions: u64,
    /// Runtime exceptions planned through the active generated frame chain.
    pub exception_unwinds: u64,
    /// Deterministically injected allocation failures.
    pub allocation_failures: u64,
}

#[derive(Debug, Clone, Copy)]
enum RuntimeRequest {
    Allocate(JitAllocationKind),
    Throw,
    NestedAllocate(JitAllocationKind),
    CollectMinor,
    CollectMajor,
}

#[derive(Debug)]
struct RuntimeState {
    active_frames: JitFrameChain,
    active_roots: Vec<ActiveFrameRoots>,
    pc_table: JitPcTable,
    fast_capacity: u32,
    fast_remaining: u32,
    allocation_budget: Option<u64>,
    diagnostics: JitRuntimeCallDiagnostics,
}

#[derive(Debug, Clone, Copy)]
struct ActiveFrameRoots {
    frame_id: u64,
    values: *const JsValue,
    len: usize,
}

#[derive(Debug)]
struct RuntimeStateCell(UnsafeCell<RuntimeState>);

impl RuntimeStateCell {
    fn get(&self) -> *mut RuntimeState {
        self.0.get()
    }
}

impl Finalize for RuntimeStateCell {}

// SAFETY: the cell is boxed before it is registered as a root provider. Runtime
// mutation is deliberately ended before any helper can allocate. Every root
// pointer names the argument slice retained by the corresponding generated call.
unsafe impl Trace for RuntimeStateCell {
    unsafe fn trace(&self, tracer: &mut Tracer) {
        let state = unsafe { &*self.0.get() };
        assert_eq!(state.active_frames.frames().len(), state.active_roots.len());
        for (frame, roots) in state.active_frames.frames().iter().zip(&state.active_roots) {
            assert_eq!(frame.header.frame_id, roots.frame_id);
            let lookup = state
                .pc_table
                .lookup(frame.safepoint_pc)
                .filter(|lookup| lookup.descriptor.id() == frame.header.descriptor_id)
                .expect("active JIT frames must resolve while the collector scans roots");
            for location in lookup.safepoint.stack_map.live_values() {
                let ValueLocation::FrameRegister(register) = *location else {
                    panic!("runtime-call stack maps may only name spilled frame registers");
                };
                let index = register as usize;
                assert!(index < roots.len);
                // SAFETY: the generated frame remains active, and the stack map
                // proves this initialized argument slot is live at this PC.
                unsafe { (&*roots.values.add(index)).trace(tracer) };
            }
        }
    }

    fn run_finalizer(&self) {}
}

#[repr(C)]
struct RuntimeCallFrame {
    trampoline: unsafe extern "C" fn(*mut RuntimeCallFrame),
    state: *mut c_void,
    header: JitFrameHeader,
    spilled_values: *const JsValue,
    spilled_len: usize,
}

static_assertions::const_assert!(offset_of!(RuntimeCallFrame, trampoline) == 0);

struct PendingCall {
    runtime: *const JitRuntimeCall,
    context: *mut Context,
    request: RuntimeRequest,
    arguments: *const JsValue,
    argument_count: usize,
    result: Option<Result<JsResult<JsValue>, RuntimeCallError>>,
}

#[derive(Debug)]
struct RuntimeCallCode {
    memory: ExecutableMemory,
    descriptor: Arc<JitFrameDescriptor>,
}

impl RuntimeCallCode {
    fn compile(frame_register_count: u32, safepoint_kind: SafepointKind) -> Result<Self, JitError> {
        #[cfg(not(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos"))))]
        {
            let _ = (frame_register_count, safepoint_kind);
            return Err(JitError::UnsupportedPlatform);
        }
        #[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
        {
            // System V AMD64: align rsp, load the fixed trampoline from frame[0],
            // call it with the frame pointer still in rdi, restore rsp, return.
            let code = [
                0x48, 0x83, 0xec, 0x08, // sub rsp, 8
                0x48, 0x8b, 0x07, // mov rax, [rdi]
                0xff, 0xd0, // call rax (return PC = +9)
                0x48, 0x83, 0xc4, 0x08, // add rsp, 8
                0xc3, // ret
            ];
            let descriptor = Arc::new(JitFrameDescriptor::new(
                JitFrameDescriptorId(NEXT_DESCRIPTOR_ID.fetch_add(1, Ordering::Relaxed)),
                u32::try_from(code.len()).map_err(|_| JitError::InvalidCodeSize)?,
                u32::try_from(size_of::<RuntimeCallFrame>())
                    .map_err(|_| JitError::InvalidCodeSize)?,
                frame_register_count,
                [Safepoint {
                    machine_offset: RUNTIME_CALL_RETURN_PC,
                    bytecode_offset: 0,
                    kind: safepoint_kind,
                    stack_map: StackMap::new(
                        (0..frame_register_count).map(ValueLocation::FrameRegister),
                    ),
                }],
            )?);
            let mut writable = WritableMemory::allocate(code.len())?;
            writable.write(0, &code)?;
            Ok(Self {
                memory: writable.publish()?,
                descriptor,
            })
        }
    }

    fn enter(&self, frame: &mut RuntimeCallFrame) {
        // SAFETY: compile emits one fixed System V function taking the frame in
        // rdi. The RX mapping and stack frame remain live for the whole call.
        let entry: unsafe extern "C" fn(*mut RuntimeCallFrame) =
            unsafe { std::mem::transmute(self.memory.as_ptr()) };
        unsafe { entry(frame) };
    }
}

/// Runtime-local generated helper boundary for allocation-capable JIT code.
///
/// The helper table is closed: generated code cannot invoke host callbacks or
/// re-enter script evaluation through this API. This type is thread-local by
/// construction and supports nested generated helper calls on the same runtime.
#[derive(Debug)]
pub struct JitRuntimeCall {
    // Dropped first, so the collector cannot observe `state` during teardown.
    _root_provider: RootProvider,
    frame_register_capacity: u32,
    call_code: Vec<RuntimeCallCode>,
    allocation_code: Vec<RuntimeCallCode>,
    state: Box<RuntimeStateCell>,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl JitRuntimeCall {
    const MAX_FRAME_REGISTERS: u32 = 256;

    /// Compiles the fixed runtime-call stub and reserves argument spill slots.
    pub fn new(
        frame_register_count: u32,
        fast_allocation_capacity: u32,
    ) -> Result<Self, RuntimeCallError> {
        if frame_register_count > Self::MAX_FRAME_REGISTERS {
            return Err(RuntimeCallError::FrameCapacityTooLarge {
                requested: frame_register_count,
                maximum: Self::MAX_FRAME_REGISTERS,
            });
        }
        let call_code = (0..=frame_register_count)
            .map(|live| RuntimeCallCode::compile(live, SafepointKind::Call))
            .collect::<Result<Vec<_>, _>>()?;
        let allocation_code = (0..=frame_register_count)
            .map(|live| RuntimeCallCode::compile(live, SafepointKind::Allocation))
            .collect::<Result<Vec<_>, _>>()?;
        let mut pc_table = JitPcTable::default();
        for code in call_code.iter().chain(&allocation_code) {
            pc_table
                .install(code.memory.as_ptr() as usize, Arc::clone(&code.descriptor))
                .map_err(JitError::from)?;
        }
        let state = Box::new(RuntimeStateCell(UnsafeCell::new(RuntimeState {
            active_frames: JitFrameChain::default(),
            active_roots: Vec::new(),
            pc_table,
            fast_capacity: fast_allocation_capacity,
            fast_remaining: fast_allocation_capacity,
            allocation_budget: None,
            diagnostics: JitRuntimeCallDiagnostics::default(),
        })));
        // SAFETY: `state` is boxed and the registration guard is dropped before
        // the box, so the provider address remains valid for the registration.
        let root_provider = unsafe { RootProvider::register(NonNull::from(&*state)) };
        Ok(Self {
            _root_provider: root_provider,
            frame_register_capacity: frame_register_count,
            call_code,
            allocation_code,
            state,
            _not_send_or_sync: PhantomData,
        })
    }

    /// Sets a deterministic remaining-allocation budget for failure handling.
    #[doc(hidden)]
    pub fn set_allocation_budget(&mut self, budget: Option<u64>) {
        self.state.0.get_mut().allocation_budget = budget;
    }

    /// Returns runtime-call counters.
    #[must_use]
    pub fn diagnostics(&self) -> JitRuntimeCallDiagnostics {
        // SAFETY: the type is !Send/!Sync and no helper is executing while this
        // public observation method has control.
        unsafe { (*self.state.get()).diagnostics }
    }

    /// Allocates an object, array, or closure through generated code.
    pub fn allocate(
        &self,
        kind: JitAllocationKind,
        arguments: &[JsValue],
        context: &mut Context,
    ) -> Result<JsResult<JsValue>, RuntimeCallError> {
        self.invoke(RuntimeRequest::Allocate(kind), arguments, context)
    }

    /// Allocates through generated code and flattens boundary failures into the
    /// same `JsResult` channel used by interpreter and native-function calls.
    pub fn allocate_or_throw(
        &self,
        kind: JitAllocationKind,
        arguments: &[JsValue],
        context: &mut Context,
    ) -> JsResult<JsValue> {
        self.allocate(kind, arguments, context)
            .map_err(RuntimeCallError::into_js_error)?
    }

    /// Exercises the common exception return path through generated code.
    #[doc(hidden)]
    pub fn throw_for_test(
        &self,
        context: &mut Context,
    ) -> Result<JsResult<JsValue>, RuntimeCallError> {
        self.invoke(RuntimeRequest::Throw, &[], context)
    }

    /// Enters a second generated helper while the outer JIT frame remains live.
    #[doc(hidden)]
    pub fn nested_allocate_for_test(
        &self,
        kind: JitAllocationKind,
        arguments: &[JsValue],
        context: &mut Context,
    ) -> Result<JsResult<JsValue>, RuntimeCallError> {
        self.invoke(RuntimeRequest::NestedAllocate(kind), arguments, context)
    }

    /// Forces a nursery collection while generated frame roots are live.
    #[doc(hidden)]
    pub fn collect_minor_for_test(
        &self,
        arguments: &[JsValue],
        context: &mut Context,
    ) -> Result<JsResult<JsValue>, RuntimeCallError> {
        self.invoke(RuntimeRequest::CollectMinor, arguments, context)
    }

    /// Forces a whole-heap collection while generated frame roots are live.
    #[doc(hidden)]
    pub fn collect_major_for_test(
        &self,
        arguments: &[JsValue],
        context: &mut Context,
    ) -> Result<JsResult<JsValue>, RuntimeCallError> {
        self.invoke(RuntimeRequest::CollectMajor, arguments, context)
    }

    fn invoke(
        &self,
        request: RuntimeRequest,
        arguments: &[JsValue],
        context: &mut Context,
    ) -> Result<JsResult<JsValue>, RuntimeCallError> {
        let capacity = self.frame_register_capacity;
        if arguments.len() > capacity as usize {
            return Err(RuntimeCallError::TooManyArguments {
                supplied: arguments.len(),
                capacity,
            });
        }
        let frame_id = NEXT_FRAME_ID.fetch_add(1, Ordering::Relaxed);
        let caller = {
            // This mutation ends before generated code can enter an allocating
            // helper and invoke the root provider.
            let state = unsafe { &mut *self.state.get() };
            let caller = state.active_frames.frames().last().map_or(
                FrameCaller::Interpreter {
                    frame_depth: context.vm.frames.len(),
                },
                |frame| FrameCaller::Jit {
                    frame_id: frame.header.frame_id,
                },
            );
            if !state.active_frames.frames().is_empty() {
                state.diagnostics.nested_calls = state.diagnostics.nested_calls.saturating_add(1);
            }
            state.diagnostics.generated_calls = state.diagnostics.generated_calls.saturating_add(1);
            caller
        };

        let code = match request {
            RuntimeRequest::Throw
            | RuntimeRequest::NestedAllocate(_)
            | RuntimeRequest::CollectMinor
            | RuntimeRequest::CollectMajor => &self.call_code,
            RuntimeRequest::Allocate(_) => &self.allocation_code,
        }
        .get(arguments.len())
        .expect("argument count was checked against the compiled table");

        let mut pending = PendingCall {
            runtime: self,
            context,
            request,
            arguments: arguments.as_ptr(),
            argument_count: arguments.len(),
            result: None,
        };
        let mut frame = RuntimeCallFrame {
            trampoline: runtime_call_trampoline,
            state: (&raw mut pending).cast(),
            header: JitFrameHeader {
                frame_id,
                descriptor_id: code.descriptor.id(),
                caller,
            },
            spilled_values: arguments.as_ptr(),
            spilled_len: arguments.len(),
        };
        {
            let state = unsafe { &mut *self.state.get() };
            state
                .active_frames
                .push(ActiveJitFrame {
                    header: frame.header,
                    safepoint_pc: code.memory.as_ptr() as usize + RUNTIME_CALL_RETURN_PC as usize,
                })
                .expect("the runtime constructs a valid nested frame chain");
            state.active_roots.push(ActiveFrameRoots {
                frame_id,
                values: arguments.as_ptr(),
                len: arguments.len(),
            });
        }
        code.enter(&mut frame);
        let state = unsafe { &mut *self.state.get() };
        if pending
            .result
            .as_ref()
            .is_some_and(|result| matches!(result, Ok(Err(_))))
        {
            let plan = JitExceptionUnwindPlan::build(&state.active_frames, &state.pc_table)
                .expect("active generated exception frames resolve at exact safepoints");
            assert!(matches!(
                plan.target(),
                JitExceptionUnwindTarget::Interpreter { .. }
            ));
            state.diagnostics.exception_unwinds =
                state.diagnostics.exception_unwinds.saturating_add(1);
        }
        let popped_roots = state
            .active_roots
            .pop()
            .expect("every generated frame has a root record");
        assert_eq!(popped_roots.frame_id, frame_id);
        let popped = state
            .active_frames
            .pop(frame_id)
            .expect("generated calls return in stack order");
        debug_assert_eq!(popped.header.frame_id, frame_id);
        pending
            .result
            .unwrap_or(Err(RuntimeCallError::MissingResult))
    }

    fn dispatch(
        &self,
        request: RuntimeRequest,
        arguments: &[JsValue],
        context: &mut Context,
    ) -> Result<JsResult<JsValue>, RuntimeCallError> {
        match request {
            RuntimeRequest::Allocate(kind) => self.allocate_helper(kind, arguments, context),
            RuntimeRequest::Throw => {
                let state = unsafe { &mut *self.state.get() };
                state.diagnostics.exceptions = state.diagnostics.exceptions.saturating_add(1);
                Ok(Err(JsNativeError::typ()
                    .with_message("JIT runtime helper exception")
                    .into()))
            }
            RuntimeRequest::NestedAllocate(kind) => {
                self.invoke(RuntimeRequest::Allocate(kind), arguments, context)
            }
            RuntimeRequest::CollectMinor => {
                boa_gc::force_minor_collect();
                Ok(Ok(arguments.first().cloned().unwrap_or_default()))
            }
            RuntimeRequest::CollectMajor => {
                boa_gc::force_collect();
                Ok(Ok(arguments.first().cloned().unwrap_or_default()))
            }
        }
    }

    fn allocate_helper(
        &self,
        kind: JitAllocationKind,
        arguments: &[JsValue],
        context: &mut Context,
    ) -> Result<JsResult<JsValue>, RuntimeCallError> {
        let slow_path = {
            // Finish mutating runtime bookkeeping before collection invokes the
            // root provider and reads the active frame/root vectors.
            let state = unsafe { &mut *self.state.get() };
            if let Some(remaining) = &mut state.allocation_budget {
                if *remaining == 0 {
                    state.diagnostics.allocation_failures =
                        state.diagnostics.allocation_failures.saturating_add(1);
                    return Err(RuntimeCallError::AllocationFailure);
                }
                *remaining -= 1;
            }
            let slow_path = state.fast_remaining == 0;
            if slow_path {
                state.diagnostics.slow_allocations =
                    state.diagnostics.slow_allocations.saturating_add(1);
            } else {
                state.diagnostics.fast_allocations =
                    state.diagnostics.fast_allocations.saturating_add(1);
                state.fast_remaining = state.fast_remaining.saturating_sub(1);
            }
            slow_path
        };
        if slow_path {
            boa_gc::force_collect();
            let state = unsafe { &mut *self.state.get() };
            state.fast_remaining = state.fast_capacity;
            state.fast_remaining = state.fast_remaining.saturating_sub(1);
        }

        let value = match kind {
            JitAllocationKind::Object => JsObject::with_object_proto(context.intrinsics()).into(),
            JitAllocationKind::Array => {
                JsArray::from_iter(arguments.iter().cloned(), context).into()
            }
            JitAllocationKind::Closure => FunctionObjectBuilder::new(
                context.realm(),
                NativeFunction::from_fn_ptr(noop_closure),
            )
            .build()
            .into(),
        };
        Ok(Ok(value))
    }
}

// NativeFunction requires the common exception-capable result signature even
// though this particular probe closure cannot fail.
#[allow(clippy::unnecessary_wraps)]
fn noop_closure(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

unsafe extern "C" fn runtime_call_trampoline(frame: *mut RuntimeCallFrame) {
    // SAFETY: only RuntimeCallCode invokes this function, with a live frame and
    // PendingCall for the duration of the generated call.
    let frame = unsafe { &mut *frame };
    let pending = unsafe { &mut *frame.state.cast::<PendingCall>() };
    let runtime = unsafe { &*pending.runtime };
    let state = unsafe { &*runtime.state.get() };
    debug_assert!(
        state
            .active_frames
            .resolve_safepoints(&state.pc_table)
            .is_ok(),
        "every active runtime-call frame must resolve at its exact safepoint"
    );
    let context = unsafe { &mut *pending.context };
    let arguments =
        unsafe { std::slice::from_raw_parts(pending.arguments, pending.argument_count) };
    pending.result = Some(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.dispatch(pending.request, arguments, context)
        }))
        .unwrap_or(Err(RuntimeCallError::HelperPanicked)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Source, js_string};
    use boa_gc::WeakGc;

    #[test]
    fn excessive_frame_capacity_is_rejected_before_code_allocation() {
        assert!(matches!(
            JitRuntimeCall::new(257, 1),
            Err(RuntimeCallError::FrameCapacityTooLarge {
                requested: 257,
                maximum: 256
            })
        ));
    }

    #[test]
    #[cfg(not(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos"))))]
    fn unsupported_target_fails_loudly() {
        assert!(matches!(
            JitRuntimeCall::new(0, 0),
            Err(RuntimeCallError::Jit(JitError::UnsupportedPlatform))
        ));
    }

    #[test]
    #[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
    fn allocations_cross_generated_boundary_and_return_values() {
        let mut context = Context::default();
        let runtime = JitRuntimeCall::new(4, 2).unwrap();
        let object = runtime
            .allocate(JitAllocationKind::Object, &[], &mut context)
            .unwrap()
            .unwrap();
        assert!(object.is_object());
        let array = runtime
            .allocate(
                JitAllocationKind::Array,
                &[1.into(), 2.into()],
                &mut context,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            array
                .as_object()
                .unwrap()
                .length_of_array_like(&mut context)
                .unwrap(),
            2
        );
        let closure = runtime
            .allocate(JitAllocationKind::Closure, &[], &mut context)
            .unwrap()
            .unwrap();
        assert!(closure.as_object().unwrap().is_callable());
        assert_eq!(runtime.diagnostics().generated_calls, 3);
        assert_eq!(runtime.diagnostics().fast_allocations, 2);
        assert_eq!(runtime.diagnostics().slow_allocations, 1);
        assert_eq!(
            runtime.allocation_code[2].descriptor.safepoints()[0]
                .stack_map
                .live_values(),
            &[
                ValueLocation::FrameRegister(0),
                ValueLocation::FrameRegister(1)
            ]
        );
        assert_eq!(
            runtime.allocation_code[0].descriptor.safepoints()[0].kind,
            SafepointKind::Allocation
        );
    }

    #[test]
    #[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
    fn gc_slow_path_nested_call_exception_and_failure_are_distinct() {
        let mut context = Context::default();
        let mut runtime = JitRuntimeCall::new(2, 0).unwrap();
        for _ in 0..16 {
            let live_object = JsObject::with_null_proto();
            let nested = runtime
                .nested_allocate_for_test(
                    JitAllocationKind::Array,
                    &[live_object.clone().into()],
                    &mut context,
                )
                .unwrap()
                .unwrap();
            let nested = nested.as_object().unwrap();
            assert_eq!(nested.length_of_array_like(&mut context).unwrap(), 1);
            assert!(JsObject::equals(
                &nested.get(0, &mut context).unwrap().as_object().unwrap(),
                &live_object,
            ));
        }
        assert!(runtime.throw_for_test(&mut context).unwrap().is_err());
        runtime.set_allocation_budget(Some(0));
        assert!(matches!(
            runtime.allocate(JitAllocationKind::Object, &[], &mut context),
            Err(RuntimeCallError::AllocationFailure)
        ));
        let allocation_error = runtime
            .allocate_or_throw(JitAllocationKind::Object, &[], &mut context)
            .unwrap_err();
        assert!(allocation_error.as_native().unwrap().is_range());
        assert_eq!(
            allocation_error.as_native().unwrap().message(),
            "JIT allocation budget exhausted"
        );
        let diagnostics = runtime.diagnostics();
        assert_eq!(diagnostics.nested_calls, 16);
        assert_eq!(diagnostics.slow_allocations, 16);
        assert_eq!(diagnostics.exceptions, 1);
        assert_eq!(diagnostics.exception_unwinds, 1);
        assert_eq!(diagnostics.allocation_failures, 2);
        assert_eq!(
            runtime.call_code[0].descriptor.safepoints()[0].kind,
            SafepointKind::Call
        );
        assert!(
            unsafe { &*runtime.state.get() }
                .active_frames
                .frames()
                .is_empty()
        );
        assert!(matches!(
            runtime.allocate(
                JitAllocationKind::Array,
                &[1.into(), 2.into(), 3.into()],
                &mut context
            ),
            Err(RuntimeCallError::TooManyArguments {
                supplied: 3,
                capacity: 2
            })
        ));

        // The runtime helper table cannot call eval or host callbacks. Normal
        // interpreter evaluation remains usable immediately after every exit.
        assert_eq!(
            context
                .eval(Source::from_bytes("({answer: 42}).answer"))
                .unwrap(),
            JsValue::from(42)
        );
    }

    #[test]
    #[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
    fn stack_map_roots_survive_minor_and_major_collection_then_die() {
        let mut context = Context::default();
        let runtime = JitRuntimeCall::new(2, 1).unwrap();
        let object = JsObject::with_null_proto();
        let weak = WeakGc::new(&object.root_inner());

        for _ in 0..2 {
            let returned = runtime
                .collect_minor_for_test(&[object.clone().into()], &mut context)
                .unwrap()
                .unwrap();
            assert!(JsObject::equals(&returned.as_object().unwrap(), &object));
            assert!(weak.is_upgradable());
        }
        let returned = runtime
            .collect_major_for_test(&[object.clone().into()], &mut context)
            .unwrap()
            .unwrap();
        assert!(JsObject::equals(&returned.as_object().unwrap(), &object));
        assert!(weak.is_upgradable());

        drop(returned);
        drop(object);
        runtime
            .collect_major_for_test(&[], &mut context)
            .unwrap()
            .unwrap();
        assert!(!weak.is_upgradable());
    }

    #[test]
    #[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
    fn generated_helper_errors_cross_js_catch_finally_rethrow_and_gc_boundary() {
        fn helper_throw(_: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
            let runtime = JitRuntimeCall::new(1, 0).map_err(RuntimeCallError::into_js_error)?;
            runtime
                .collect_major_for_test(&[JsValue::from(7)], context)
                .map_err(RuntimeCallError::into_js_error)??;
            runtime
                .throw_for_test(context)
                .map_err(RuntimeCallError::into_js_error)?
        }

        fn allocation_failure(
            _: &JsValue,
            _: &[JsValue],
            context: &mut Context,
        ) -> JsResult<JsValue> {
            let mut runtime = JitRuntimeCall::new(0, 0).map_err(RuntimeCallError::into_js_error)?;
            runtime.set_allocation_budget(Some(0));
            runtime.allocate_or_throw(JitAllocationKind::Object, &[], context)
        }

        let mut context = Context::default();
        context
            .register_global_builtin_callable(
                js_string!("helperThrow"),
                0,
                NativeFunction::from_fn_ptr(helper_throw),
            )
            .unwrap();
        context
            .register_global_builtin_callable(
                js_string!("allocationFailure"),
                0,
                NativeFunction::from_fn_ptr(allocation_failure),
            )
            .unwrap();
        let result = context
            .eval(Source::from_bytes(
                "let log=[];function middle(){try{helperThrow()}finally{log.push('finally')}}\
                 try{middle()}catch(error){log.push(error.name);log.push(error.message);\
                   try{throw error}catch(same){log.push(same===error)}}\
                 try{allocationFailure()}catch(error){log.push(error.name);log.push(error.message)}\
                 log.join('|')",
            ))
            .unwrap();
        assert_eq!(
            result.display().to_string(),
            "\"finally|TypeError|JIT runtime helper exception|true|RangeError|JIT allocation budget exhausted\""
        );

        let uncaught = context
            .eval(Source::from_bytes(
                "function hostBoundary(){helperThrow()}hostBoundary()",
            ))
            .unwrap_err();
        let native = uncaught.as_native().expect("helper returns a native error");
        assert!(native.is_type());
        assert_eq!(native.message(), "JIT runtime helper exception");
    }

    #[test]
    #[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
    fn old_to_young_store_survives_minor_collection_from_jit_root() {
        let mut context = Context::default();
        let runtime = JitRuntimeCall::new(1, 1).unwrap();
        let parent = JsObject::with_null_proto();

        // Surviving two nursery collections promotes the parent to old.
        for _ in 0..2 {
            runtime
                .collect_minor_for_test(&[parent.clone().into()], &mut context)
                .unwrap()
                .unwrap();
        }

        let child = JsObject::with_null_proto();
        let weak_child = WeakGc::new(&child.root_inner());
        parent
            .set(js_string!("child"), child.clone(), true, &mut context)
            .unwrap();
        drop(child);

        let returned = runtime
            .collect_minor_for_test(&[parent.clone().into()], &mut context)
            .unwrap()
            .unwrap();
        assert!(weak_child.is_upgradable());
        let stored = returned
            .as_object()
            .unwrap()
            .get(js_string!("child"), &mut context)
            .unwrap();
        assert!(stored.is_object());
    }
}
