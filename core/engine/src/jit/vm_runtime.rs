//! Generated runtime-helper sites backed by the live VM register file.
//!
//! Each verified call/throw/allocation/property site has a native entry and an
//! exact return-PC safepoint. Helpers share the interpreter's opcode semantics;
//! arithmetic loops continue to use the scalar emitter. The native frame only
//! holds the trampoline and a borrowed Rust invocation: all JavaScript values
//! remain in the VM's registered stack or the helper's own registered roots.

use std::{
    any::Any,
    collections::{BTreeMap, HashMap, VecDeque},
    ffi::c_void,
    fmt::Write as _,
    mem::size_of,
    ops::ControlFlow,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    Context, JsError,
    vm::{CodeBlock, CompletionRecord, Opcode, Vm},
};

use super::{
    ActiveJitFrame, ExecutableMemory, FrameCaller, JitError, JitExceptionHandler,
    JitExceptionMetadata, JitExceptionUnwindPlan, JitExceptionUnwindTarget, JitFrameChain,
    JitFrameDescriptor, JitFrameDescriptorId, JitFrameHeader, JitPcTable, JitSourceLocation,
    Safepoint, SafepointKind, StackMap, WritableMemory,
};

static NEXT_DESCRIPTOR: AtomicU64 = AtomicU64::new(1 << 48);
static NEXT_FRAME: AtomicU64 = AtomicU64::new(1 << 48);
// System V AMD64: align the stack, call frame.trampoline(frame), return.
#[cfg(not(target_arch = "aarch64"))]
const STUB: [u8; 14] = [
    0x48, 0x83, 0xec, 0x08, 0x48, 0x8b, 0x07, 0xff, 0xd0, 0x48, 0x83, 0xc4, 0x08, 0xc3,
];

/// Counters for live generated runtime-helper execution and exception cleanup.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JitExceptionDiagnostics {
    /// Executed generated runtime-helper sites.
    pub generated_entries: u64,
    /// Exceptions resolved through verified generated-frame metadata.
    pub exception_unwinds: u64,
    /// Catch/finally entries restored from generated handler metadata.
    pub handler_entries: u64,
    /// Logical generated frames removed while propagating an exception.
    pub unwound_frames: u64,
    /// Currently executing native frames, including suspended outer helpers.
    pub active_frames: usize,
    /// Maximum simultaneously executing native runtime-helper frames.
    pub maximum_active_frames: usize,
}

#[derive(Debug)]
struct RuntimeCode {
    memory: ExecutableMemory,
    descriptor: Arc<JitFrameDescriptor>,
    entries: BTreeMap<u32, u32>,
    return_pc: u32,
}

impl RuntimeCode {
    fn compile(block: &CodeBlock) -> Result<Self, JitError> {
        if !cfg!(all(
            any(target_arch = "x86_64", target_arch = "aarch64"),
            any(target_os = "linux", target_os = "macos")
        )) {
            return Err(JitError::UnsupportedPlatform);
        }
        let snapshot = block
            .bytecode_contract()
            .verify()
            .map_err(|_| JitError::InvalidCodeSize)?;
        #[cfg(target_arch = "aarch64")]
        let (stub, return_pc) = super::aarch64::runtime_call()?;
        #[cfg(not(target_arch = "aarch64"))]
        let (stub, return_pc) = (STUB, 9);
        let mut bytes = Vec::new();
        let mut entries = BTreeMap::new();
        let mut safepoints = Vec::new();
        for instruction in &snapshot.instructions {
            let opcode = Opcode::decode(instruction.opcode);
            if !supports(opcode) {
                continue;
            }
            let offset = u32::try_from(bytes.len()).map_err(|_| JitError::InvalidCodeSize)?;
            entries.insert(instruction.offset, offset);
            safepoints.push(Safepoint {
                machine_offset: offset
                    .checked_add(return_pc)
                    .ok_or(JitError::InvalidCodeSize)?,
                bytecode_offset: instruction.offset,
                kind: if allocation(opcode) {
                    SafepointKind::Allocation
                } else {
                    SafepointKind::Call
                },
                // No values are copied out of the VM into this native frame.
                stack_map: StackMap::new([]),
            });
            bytes.extend_from_slice(&stub);
        }
        let metadata = JitExceptionMetadata::new(
            snapshot
                .instructions
                .iter()
                .map(|i| i.offset)
                .chain([snapshot.byte_len]),
            snapshot.handlers.iter().map(|h| JitExceptionHandler {
                start: h.start,
                end: h.end,
                handler: h.end,
                environment_count: h.environment_count,
            }),
            snapshot.instructions.iter().map(|i| JitSourceLocation {
                bytecode_offset: i.offset,
                line: i.source_line,
                column: i.source_column,
            }),
        )?;
        let descriptor = Arc::new(
            JitFrameDescriptor::new(
                JitFrameDescriptorId(NEXT_DESCRIPTOR.fetch_add(1, Ordering::Relaxed)),
                u32::try_from(bytes.len()).map_err(|_| JitError::InvalidCodeSize)?,
                size_of::<NativeFrame>() as u32,
                0,
                safepoints,
            )?
            .with_exception_metadata(metadata),
        );
        let mut memory = WritableMemory::allocate(bytes.len())?;
        memory.write(0, &bytes)?;
        Ok(Self {
            memory: memory.publish()?,
            descriptor,
            entries,
            return_pc,
        })
    }
}

fn allocation(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::PushEmptyObject | Opcode::PushNewArray | Opcode::GetFunction
    )
}

fn supports(opcode: Opcode) -> bool {
    allocation(opcode)
        || matches!(
            opcode,
            Opcode::Call
                | Opcode::CallSpread
                | Opcode::CallEval
                | Opcode::CallEvalSpread
                | Opcode::New
                | Opcode::NewSpread
                | Opcode::SuperCall
                | Opcode::SuperCallSpread
                | Opcode::SuperCallDerived
                | Opcode::Throw
                | Opcode::ReThrow
                | Opcode::ThrowNewTypeError
                | Opcode::ThrowNewSyntaxError
                | Opcode::ThrowNewReferenceError
                | Opcode::GetPropertyByName
                | Opcode::SetPropertyByName
                | Opcode::GetPropertyByValue
                | Opcode::SetPropertyByValue
        )
}

#[derive(Debug)]
enum CacheEntry {
    Warming(usize),
    Compiled(Rc<RuntimeCode>),
    Unsupported,
}

#[derive(Debug)]
struct Activation {
    id: u64,
    code_id: u64,
    frame_depth: usize,
    bytecode_pc: u32,
    machine_offset: u32,
    code: Rc<RuntimeCode>,
    unwound: bool,
}

#[cfg(test)]
#[cfg(all(
    any(target_arch = "x86_64", target_arch = "aarch64"),
    any(target_os = "linux", target_os = "macos")
))]
mod tests;

#[derive(Debug, Default)]
pub(crate) struct VmRuntime {
    cache: HashMap<u64, CacheEntry>,
    insertion_order: VecDeque<u64>,
    active: Vec<Activation>,
    diagnostics: JitExceptionDiagnostics,
}

impl VmRuntime {
    pub(crate) const fn diagnostics(&self) -> JitExceptionDiagnostics {
        self.diagnostics
    }

    pub(crate) fn write_debug_snapshot(&self, output: &mut String) {
        let mut entries = self.cache.iter().collect::<Vec<_>>();
        entries.sort_unstable_by_key(|(id, _)| **id);
        for (id, entry) in entries {
            if let CacheEntry::Compiled(code) = entry {
                writeln!(
                    output,
                    "tier=runtime-helper code_id={id}\nframe={:#?}\nentries={:#?}",
                    code.descriptor, code.entries
                )
                .expect("String formatting cannot fail");
                code.memory.write_debug_bytes(output);
            }
        }
        for frame in &self.active {
            writeln!(output, "active frame_id={} code_id={} vm_depth={} bytecode_pc={} machine_offset={} unwound={}",
                frame.id, frame.code_id, frame.frame_depth, frame.bytecode_pc,
                frame.machine_offset, frame.unwound).expect("String formatting cannot fail");
            if !matches!(
                self.cache.get(&frame.code_id),
                Some(CacheEntry::Compiled(_))
            ) {
                writeln!(output, "evicted_active_frame={:#?}", frame.code.descriptor)
                    .expect("String formatting cannot fail");
                frame.code.memory.write_debug_bytes(output);
            }
        }
    }

    fn code(&mut self, block: &CodeBlock) -> Option<Rc<RuntimeCode>> {
        let id = block.jit_code_id;
        if !self.cache.contains_key(&id) {
            if self.cache.len() == 256 {
                let oldest = self
                    .insertion_order
                    .pop_front()
                    .expect("full cache has an oldest entry");
                self.cache.remove(&oldest);
            }
            self.insertion_order.push_back(id);
            self.cache.insert(id, CacheEntry::Warming(0));
        }
        let entry = self.cache.get_mut(&id).expect("inserted cache entry");
        match entry {
            CacheEntry::Warming(count) => {
                *count += 1;
                if *count < 8 && block.jit_metadata().interpreter_entries < 8 {
                    return None;
                }
                *entry = RuntimeCode::compile(block).map_or(CacheEntry::Unsupported, |code| {
                    CacheEntry::Compiled(Rc::new(code))
                });
            }
            CacheEntry::Compiled(_) | CacheEntry::Unsupported => {}
        }
        match entry {
            CacheEntry::Compiled(code) => Some(Rc::clone(code)),
            _ => None,
        }
    }

    /// Build only the generated segment belonging to this live interpreter
    /// frame. An intervening interpreter frame must get its own handler search
    /// before propagation reaches an older generated caller.
    fn plan(&self, depth: usize, code_id: u64) -> Option<JitExceptionUnwindPlan> {
        let frames = self
            .active
            .iter()
            .rev()
            .take_while(|frame| {
                !frame.unwound && frame.frame_depth == depth && frame.code_id == code_id
            })
            .collect::<Vec<_>>();
        if frames.is_empty() {
            return None;
        }
        let mut chain = JitFrameChain::default();
        let mut table = JitPcTable::default();
        let mut previous = None;
        let mut installed = Vec::new();
        for frame in frames.into_iter().rev() {
            let code = &frame.code;
            if !installed.contains(&code.descriptor.id()) {
                table
                    .install(code.memory.as_ptr() as usize, Arc::clone(&code.descriptor))
                    .expect("live code objects have distinct mappings");
                installed.push(code.descriptor.id());
            }
            chain
                .push(ActiveJitFrame {
                    header: JitFrameHeader {
                        frame_id: frame.id,
                        descriptor_id: code.descriptor.id(),
                        caller: previous.map_or(
                            FrameCaller::Interpreter { frame_depth: depth },
                            |frame_id| FrameCaller::Jit { frame_id },
                        ),
                    },
                    safepoint_pc: code.memory.as_ptr() as usize
                        + frame.machine_offset as usize
                        + code.return_pc as usize,
                })
                .expect("generated callers are linked in physical stack order");
            previous = Some(frame.id);
        }
        Some(
            JitExceptionUnwindPlan::build(&chain, &table)
                .expect("installed sites have exact safepoints"),
        )
    }

    fn finish(&mut self, id: u64) {
        let frame = self
            .active
            .pop()
            .expect("returning generated frame is active");
        assert_eq!(
            frame.id, id,
            "generated calls return in physical stack order"
        );
        self.diagnostics.active_frames = self.active.len();
    }
}

impl Vm {
    /// Applies a generated unwind plan to the owning live VM frame. A missing
    /// generated segment lets normal interpreter propagation continue.
    pub(crate) fn handle_generated_exception(&mut self) -> Option<bool> {
        let plan = self
            .runtime_jit
            .plan(self.frames.len(), self.frame.code_block.jit_code_id)?;
        self.runtime_jit.diagnostics.exception_unwinds += 1;
        for id in plan.popped_frame_ids() {
            let frame = self
                .runtime_jit
                .active
                .iter_mut()
                .rev()
                .find(|frame| frame.id == *id)
                .expect("unwind plan refers to an active generated frame");
            assert!(!frame.unwound);
            frame.unwound = true;
            self.runtime_jit.diagnostics.unwound_frames += 1;
        }
        // Runtime helpers use the VM register file directly. No scalar state or
        // pending call is replayed here; completed helper side effects stay committed.
        Some(match plan.target() {
            JitExceptionUnwindTarget::Handler {
                bytecode_offset,
                environment_count,
                ..
            } => {
                self.frame.pc = bytecode_offset;
                self.environments
                    .truncate((self.frame.env_fp + environment_count) as usize);
                self.runtime_jit.diagnostics.handler_entries += 1;
                true
            }
            JitExceptionUnwindTarget::Interpreter { frame_depth } => {
                assert_eq!(frame_depth, self.frames.len());
                false
            }
        })
    }
}

#[repr(C)]
struct NativeFrame {
    trampoline: unsafe extern "C" fn(*mut NativeFrame),
    pending: *mut c_void,
}

struct Invocation<F> {
    context: *mut Context,
    operation: Option<F>,
    result: Option<Result<ControlFlow<CompletionRecord>, Box<dyn Any + Send>>>,
}

unsafe extern "C" fn trampoline<F>(frame: *mut NativeFrame)
where
    F: FnOnce(&mut Context) -> ControlFlow<CompletionRecord>,
{
    // SAFETY: invoke_runtime_site keeps both stack allocations alive until the
    // native entry returns. No Rust reference to Context is used concurrently.
    let pending = unsafe { &mut *(*frame).pending.cast::<Invocation<F>>() };
    pending.result = Some(catch_unwind(AssertUnwindSafe(|| {
        let operation = pending
            .operation
            .take()
            .expect("one helper call per native entry");
        operation(unsafe { &mut *pending.context })
    })));
}

impl Context {
    pub(crate) fn prepare_generated_exception(&self, error: &mut JsError) {
        if error.backtrace.is_some() {
            return;
        }
        if let Some(frame) = self.vm.runtime_jit.active.last().filter(|frame| {
            !frame.unwound
                && frame.frame_depth == self.vm.frames.len()
                && frame.code_id == self.vm.frame.code_block.jit_code_id
        }) {
            // The VM shadow stack already contains every JS/native caller. Use
            // the generated site's exact bytecode PC, without duplicating frames.
            error.backtrace = Some(self.vm.shadow_stack.take(
                self.vm.runtime_limits.backtrace_limit(),
                frame.bytecode_pc + 1,
            ));
        }
    }

    pub(crate) fn invoke_runtime_site<F>(
        &mut self,
        opcode: Opcode,
        operation: F,
    ) -> ControlFlow<CompletionRecord>
    where
        F: FnOnce(&mut Context) -> ControlFlow<CompletionRecord>,
    {
        if self.vm.baseline_jit_policy != crate::vm::BaselineJitPolicy::Enabled || !supports(opcode)
        {
            return operation(self);
        }
        let Some(code) = self.vm.runtime_jit.code(&self.vm.frame.code_block) else {
            return operation(self);
        };
        let pc = self.vm.frame.pc;
        let Some(&offset) = code.entries.get(&pc) else {
            return operation(self);
        };
        let id = NEXT_FRAME.fetch_add(1, Ordering::Relaxed);
        self.vm.runtime_jit.active.push(Activation {
            id,
            code_id: self.vm.frame.code_block.jit_code_id,
            frame_depth: self.vm.frames.len(),
            bytecode_pc: pc,
            machine_offset: offset,
            code: Rc::clone(&code),
            unwound: false,
        });
        let diagnostics = &mut self.vm.runtime_jit.diagnostics;
        diagnostics.generated_entries += 1;
        diagnostics.active_frames = self.vm.runtime_jit.active.len();
        diagnostics.maximum_active_frames = diagnostics
            .maximum_active_frames
            .max(diagnostics.active_frames);
        let mut pending = Invocation {
            context: self,
            operation: Some(operation),
            result: None,
        };
        let mut frame = NativeFrame {
            trampoline: trampoline::<F>,
            pending: (&raw mut pending).cast(),
        };
        // SAFETY: RuntimeCode emits the fixed ABI stub at every verified entry.
        // `code` pins the RX allocation even if a recursive call evicts its cache entry.
        let entry: unsafe extern "C" fn(*mut NativeFrame) =
            unsafe { std::mem::transmute(code.memory.as_ptr().add(offset as usize)) };
        unsafe {
            entry(&raw mut frame);
        }
        self.vm.runtime_jit.finish(id);
        match pending
            .result
            .expect("generated trampoline publishes a result")
        {
            Ok(result) => result,
            // Rust unwinding resumes only after leaving generated code.
            Err(panic) => resume_unwind(panic),
        }
    }
}
