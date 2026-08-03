//! Generated-code to Rust runtime-call and allocation boundary.

use std::{
    cell::UnsafeCell,
    error::Error,
    ffi::c_void,
    fmt,
    marker::PhantomData,
    mem::{offset_of, size_of},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    Context, JsResult, JsValue, NativeFunction,
    error::JsNativeError,
    object::{FunctionObjectBuilder, JsObject, builtins::JsArray},
};

use super::{
    ActiveJitFrame, ExecutableMemory, FrameCaller, JitError, JitFrameChain, JitFrameDescriptor,
    JitFrameDescriptorId, JitFrameHeader, JitPcTable, Safepoint, SafepointKind, StackMap,
    ValueLocation, WritableMemory,
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
    /// Deterministically injected allocation failures.
    pub allocation_failures: u64,
}

#[derive(Debug, Clone, Copy)]
enum RuntimeRequest {
    Allocate(JitAllocationKind),
    Throw,
    NestedAllocate(JitAllocationKind),
}

struct RuntimeState {
    active_frames: JitFrameChain,
    fast_capacity: u32,
    fast_remaining: u32,
    allocation_budget: Option<u64>,
    diagnostics: JitRuntimeCallDiagnostics,
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
    frame_register_capacity: u32,
    call_code: Vec<RuntimeCallCode>,
    allocation_code: Vec<RuntimeCallCode>,
    pc_table: JitPcTable,
    state: UnsafeCell<RuntimeState>,
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
        Ok(Self {
            frame_register_capacity: frame_register_count,
            call_code,
            allocation_code,
            pc_table,
            state: UnsafeCell::new(RuntimeState {
                active_frames: JitFrameChain::default(),
                fast_capacity: fast_allocation_capacity,
                fast_remaining: fast_allocation_capacity,
                allocation_budget: None,
                diagnostics: JitRuntimeCallDiagnostics::default(),
            }),
            _not_send_or_sync: PhantomData,
        })
    }

    /// Sets a deterministic remaining-allocation budget for failure handling.
    #[doc(hidden)]
    pub fn set_allocation_budget(&mut self, budget: Option<u64>) {
        self.state.get_mut().allocation_budget = budget;
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
        let state = unsafe { &mut *self.state.get() };
        let frame_id = NEXT_FRAME_ID.fetch_add(1, Ordering::Relaxed);
        let caller = state.active_frames.frames().last().map_or(
            FrameCaller::Interpreter { frame_depth: 0 },
            |frame| FrameCaller::Jit {
                frame_id: frame.header.frame_id,
            },
        );
        if !state.active_frames.frames().is_empty() {
            state.diagnostics.nested_calls = state.diagnostics.nested_calls.saturating_add(1);
        }
        state.diagnostics.generated_calls = state.diagnostics.generated_calls.saturating_add(1);

        let code = match request {
            RuntimeRequest::Throw | RuntimeRequest::NestedAllocate(_) => &self.call_code,
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
        unsafe { &mut *self.state.get() }
            .active_frames
            .push(ActiveJitFrame {
                header: frame.header,
                safepoint_pc: code.memory.as_ptr() as usize + RUNTIME_CALL_RETURN_PC as usize,
            })
            .expect("the runtime constructs a valid nested frame chain");
        code.enter(&mut frame);
        let popped = unsafe { &mut *self.state.get() }
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
        }
    }

    fn allocate_helper(
        &self,
        kind: JitAllocationKind,
        arguments: &[JsValue],
        context: &mut Context,
    ) -> Result<JsResult<JsValue>, RuntimeCallError> {
        let state = unsafe { &mut *self.state.get() };
        if let Some(remaining) = &mut state.allocation_budget {
            if *remaining == 0 {
                state.diagnostics.allocation_failures =
                    state.diagnostics.allocation_failures.saturating_add(1);
                return Err(RuntimeCallError::AllocationFailure);
            }
            *remaining -= 1;
        }
        if state.fast_remaining == 0 {
            state.diagnostics.slow_allocations =
                state.diagnostics.slow_allocations.saturating_add(1);
            // Until Gate 4-3 teaches the collector to scan JIT stack maps
            // directly, promote object-valued spills to temporary native roots.
            // The generated frame and its exact stack map remain active during
            // collection, so Gate 4-3 can replace this bridge without changing
            // the runtime-call ABI.
            let _argument_roots = arguments
                .iter()
                .filter_map(JsValue::as_object)
                .map(JsObject::root)
                .collect::<Vec<_>>();
            boa_gc::force_collect();
            state.fast_remaining = state.fast_capacity;
        } else {
            state.diagnostics.fast_allocations =
                state.diagnostics.fast_allocations.saturating_add(1);
        }
        state.fast_remaining = state.fast_remaining.saturating_sub(1);

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
    debug_assert!(
        unsafe { &*runtime.state.get() }
            .active_frames
            .resolve_safepoints(&runtime.pc_table)
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
    use crate::Source;

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
        assert!(runtime.throw_for_test(&mut context).unwrap().is_err());
        runtime.set_allocation_budget(Some(0));
        assert!(matches!(
            runtime.allocate(JitAllocationKind::Object, &[], &mut context),
            Err(RuntimeCallError::AllocationFailure)
        ));
        let diagnostics = runtime.diagnostics();
        assert_eq!(diagnostics.nested_calls, 1);
        assert_eq!(diagnostics.slow_allocations, 1);
        assert_eq!(diagnostics.exceptions, 1);
        assert_eq!(diagnostics.allocation_failures, 1);
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
}
