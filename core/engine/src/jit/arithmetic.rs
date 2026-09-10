//! Bounded native integer loops using the shared baseline lowering and frame contract.
//!
//! Values cross this boundary as checked safe integers in an engine-owned scratch frame.
//! Generated code cannot allocate or retain GC edges. Any operation whose exact
//! ECMAScript Number result is not representable by this tier exits before that
//! bytecode and lets the interpreter perform the operation.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    fmt::Write as _,
    mem::{offset_of, size_of},
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use crate::{
    JsObject, JsValue,
    object::shape::slot::SlotAttributes,
    vm::{BYTECODE_CONTRACT_VERSION, BytecodeContractSnapshot, InlineCache, Vm},
};

use super::{
    BytecodeCodeMap, DeoptEnvironment, DeoptFrameLayout, DeoptMaterialization, DeoptPendingCall,
    DeoptReason, DeoptRecipe, DeoptResumePoint, DeoptSourceValue, DeoptValueRepresentation,
    ExecutableMemory, FrameCaller, JitError, JitFrameDescriptor, JitFrameDescriptorId,
    JitFrameHeader, Safepoint, SafepointKind, StackMap, ValueLocation, WritableMemory,
};

#[cfg(target_arch = "aarch64")]
mod arm64;
#[cfg(target_arch = "aarch64")]
use arm64::{Assembler, emit_exit, emit_instruction};
#[cfg(not(target_arch = "aarch64"))]
mod x86;
#[cfg(not(target_arch = "aarch64"))]
use x86::{Assembler, emit_exit, emit_instruction};

static NEXT_FRAME_DESCRIPTOR_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_ACTIVE_FRAME_ID: AtomicU64 = AtomicU64::new(1);

#[repr(C)]
struct NativeFrame {
    registers: *mut i64,
    dirty: *mut u8,
    loop_iterations: u64,
    loop_limit: u64,
    pc: u32,
    status: u32,
    header: JitFrameHeader,
    poll_remaining: u64,
}

#[derive(Debug, Clone, Copy)]
struct NativeEntryLimits {
    loop_limit: u64,
    poll_iterations: u64,
    interpreter_frame_depth: usize,
}

#[cfg(all(
    any(target_arch = "x86_64", target_arch = "aarch64"),
    any(target_os = "linux", target_os = "macos")
))]
static_assertions::const_assert!(offset_of!(NativeFrame, registers) == 0x00);
#[cfg(all(
    any(target_arch = "x86_64", target_arch = "aarch64"),
    any(target_os = "linux", target_os = "macos")
))]
static_assertions::const_assert!(offset_of!(NativeFrame, dirty) == 0x08);
#[cfg(all(
    any(target_arch = "x86_64", target_arch = "aarch64"),
    any(target_os = "linux", target_os = "macos")
))]
static_assertions::const_assert!(offset_of!(NativeFrame, loop_iterations) == 0x10);
#[cfg(all(
    any(target_arch = "x86_64", target_arch = "aarch64"),
    any(target_os = "linux", target_os = "macos")
))]
static_assertions::const_assert!(offset_of!(NativeFrame, loop_limit) == 0x18);
#[cfg(all(
    any(target_arch = "x86_64", target_arch = "aarch64"),
    any(target_os = "linux", target_os = "macos")
))]
static_assertions::const_assert!(offset_of!(NativeFrame, pc) == 0x20);
#[cfg(all(
    any(target_arch = "x86_64", target_arch = "aarch64"),
    any(target_os = "linux", target_os = "macos")
))]
static_assertions::const_assert!(offset_of!(NativeFrame, status) == 0x24);
#[cfg(all(
    any(target_arch = "x86_64", target_arch = "aarch64"),
    any(target_os = "linux", target_os = "macos")
))]
static_assertions::const_assert!(offset_of!(NativeFrame, header) == 0x28);

#[derive(Debug)]
pub(crate) struct ArithmeticCode {
    memory: ExecutableMemory,
    resumed_entry_offset: u32,
    bytecode_resume: u32,
    required: Box<[u32]>,
    properties: Box<[PropertyBinding]>,
    pub(crate) code_map: BytecodeCodeMap,
    frame_descriptor: JitFrameDescriptor,
    deopt_recipes: BTreeMap<u32, DeoptRecipe>,
    iteration_cost: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PropertyBinding {
    ic_index: u32,
    object_register: u32,
    shape: usize,
    slot: u32,
    scratch_register: u32,
    writable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArithmeticExit {
    Completed(u32),
    Bailout { pc: u32, reason: DeoptReason },
}

impl ArithmeticCode {
    fn generated_code_bytes(&self) -> usize {
        self.memory.requested_len()
    }

    pub(crate) fn compile(
        snapshot: &BytecodeContractSnapshot,
        inline_caches: &[InlineCache],
        bytecode_resume: u32,
    ) -> Result<Option<Self>, JitError> {
        #[cfg(not(all(
            any(target_arch = "x86_64", target_arch = "aarch64"),
            any(target_os = "linux", target_os = "macos")
        )))]
        {
            let _ = (snapshot, inline_caches, bytecode_resume);
            return Ok(None);
        }
        #[cfg(all(
            any(target_arch = "x86_64", target_arch = "aarch64"),
            any(target_os = "linux", target_os = "macos")
        ))]
        {
            let Some(mut region) = LoopRegion::find(snapshot, bytecode_resume) else {
                return Ok(None);
            };
            let Some((properties, object_move_offsets)) =
                property_bindings(snapshot, inline_caches, &region)
            else {
                return Ok(None);
            };
            let object_roots: HashSet<_> = properties.iter().map(|p| p.object_register).collect();
            region
                .required
                .retain(|register| !object_roots.contains(register));
            let mut assembler = Assembler::default();
            let resumed_entry_offset = assembler.position();
            assembler.entry(snapshot.instructions[region.increment].next_offset);
            let mut code_map = BytecodeCodeMap::default();
            let mut bailouts = BTreeSet::new();
            let mut safepoints = Vec::new();

            for instruction in &snapshot.instructions[region.first..region.end] {
                code_map
                    .push(instruction.offset, assembler.position())
                    .expect("verified instructions and monotonic emission");
                assembler.bind(Label::Bytecode(instruction.offset));
                if instruction.name == "IncrementLoopIteration" {
                    safepoints.push(Safepoint {
                        machine_offset: assembler.position(),
                        bytecode_offset: instruction.offset,
                        kind: SafepointKind::LoopBackedge,
                        // This tier keeps only checked i64 values in generated code;
                        // the owning JsValues remain in the interpreter frame.
                        stack_map: StackMap::new([]),
                    });
                }
                emit_instruction(
                    &mut assembler,
                    instruction,
                    &properties,
                    &object_move_offsets,
                    &mut bailouts,
                )?;
            }
            assembler.bind(Label::Bytecode(region.exit));
            emit_exit(&mut assembler, region.exit, 0);
            for &(pc, reason) in &bailouts {
                assembler.bind(Label::Bailout(pc, reason));
                safepoints.push(Safepoint {
                    machine_offset: assembler.position(),
                    bytecode_offset: pc,
                    kind: SafepointKind::Bailout,
                    stack_map: StackMap::new([]),
                });
                emit_exit(&mut assembler, pc, reason.status());
            }
            assembler.resolve()?;
            safepoints.sort_unstable_by_key(|point| point.machine_offset);
            let valid_program_counters = snapshot
                .instructions
                .iter()
                .map(|instruction| instruction.offset)
                .collect::<Vec<_>>();
            let exception_metadata = super::JitExceptionMetadata::new(
                valid_program_counters.iter().copied(),
                snapshot
                    .handlers
                    .iter()
                    .map(|handler| super::JitExceptionHandler {
                        start: handler.start,
                        end: handler.end,
                        handler: handler.end,
                        environment_count: handler.environment_count,
                    }),
                snapshot
                    .instructions
                    .iter()
                    .map(|instruction| super::JitSourceLocation {
                        bytecode_offset: instruction.offset,
                        line: instruction.source_line,
                        column: instruction.source_column,
                    }),
            )?;
            let frame_descriptor = JitFrameDescriptor::new(
                JitFrameDescriptorId(NEXT_FRAME_DESCRIPTOR_ID.fetch_add(1, Ordering::Relaxed)),
                u32::try_from(assembler.code.len()).map_err(|_| JitError::InvalidCodeSize)?,
                u32::try_from(size_of::<NativeFrame>()).map_err(|_| JitError::InvalidCodeSize)?,
                snapshot.register_count,
                safepoints,
            )?
            .with_exception_metadata(exception_metadata);
            let deopt_layout = DeoptFrameLayout::new(
                &valid_program_counters,
                snapshot.register_count,
                snapshot.register_count,
                0,
                u32::try_from(size_of::<NativeFrame>()).map_err(|_| JitError::InvalidCodeSize)?,
                0,
            );
            let materializations = (0..snapshot.register_count)
                .map(|register| DeoptMaterialization {
                    destination: register,
                    source: ValueLocation::FrameRegister(register),
                    representation: DeoptValueRepresentation::NativeTagged,
                })
                .collect::<Vec<_>>();
            let recipe_pcs = bailouts
                .iter()
                .map(|&(pc, _)| pc)
                .chain(std::iter::once(
                    snapshot.instructions[region.increment].next_offset,
                ))
                .collect::<BTreeSet<_>>();
            let deopt_recipes = recipe_pcs
                .into_iter()
                .map(|pc| {
                    DeoptRecipe::new(
                        pc,
                        deopt_layout,
                        DeoptResumePoint::BeforeOperation,
                        materializations.iter().copied(),
                        DeoptEnvironment::Preserve,
                        DeoptPendingCall::Preserve,
                    )
                    .map(|recipe| (pc, recipe))
                    .map_err(JitError::from)
                })
                .collect::<Result<BTreeMap<_, _>, _>>()?;
            let mut writable = WritableMemory::allocate(assembler.code.len())?;
            writable.write(0, &assembler.code)?;
            let memory = writable.publish()?;
            // A conservative instruction-budget bound between loop polls,
            // including paths skipped by conditional branches.
            let iteration_cost = snapshot.instructions[region.first..region.end]
                .iter()
                .fold(0_u32, |cost, instruction| {
                    cost.saturating_add(u32::from(
                        crate::vm::Opcode::decode(instruction.opcode).cost(),
                    ))
                })
                .max(1);
            Ok(Some(Self {
                memory,
                resumed_entry_offset,
                bytecode_resume: snapshot.instructions[region.increment].next_offset,
                required: region.required.into_boxed_slice(),
                properties: properties.into_boxed_slice(),
                code_map,
                frame_descriptor,
                deopt_recipes,
                iteration_cost,
            }))
        }
    }

    #[cfg(test)]
    fn execute_after_increment(
        &self,
        values: &mut [Option<i64>],
        loop_iterations: &mut u64,
        loop_limit: u64,
    ) -> Option<ArithmeticExit> {
        let mut write_kinds = vec![0; values.len()];
        self.execute_after_increment_typed(
            values,
            &mut write_kinds,
            loop_iterations,
            NativeEntryLimits {
                loop_limit,
                poll_iterations: u64::MAX,
                interpreter_frame_depth: 0,
            },
        )
    }

    fn execute_after_increment_typed(
        &self,
        values: &mut [Option<i64>],
        write_kinds: &mut [u8],
        loop_iterations: &mut u64,
        limits: NativeEntryLimits,
    ) -> Option<ArithmeticExit> {
        self.execute_at(
            self.resumed_entry_offset,
            values,
            write_kinds,
            loop_iterations,
            limits,
        )
    }

    fn execute_at(
        &self,
        machine_offset: u32,
        values: &mut [Option<i64>],
        write_kinds: &mut [u8],
        loop_iterations: &mut u64,
        limits: NativeEntryLimits,
    ) -> Option<ArithmeticExit> {
        if values.len() != write_kinds.len()
            || self.required.iter().any(|&r| values[r as usize].is_none())
        {
            return None;
        }
        let mut registers = values
            .iter()
            .map(|value| value.unwrap_or_default())
            .collect::<Vec<_>>();
        write_kinds.fill(0);
        let mut frame = NativeFrame {
            registers: registers.as_mut_ptr(),
            dirty: write_kinds.as_mut_ptr(),
            loop_iterations: *loop_iterations,
            loop_limit: limits.loop_limit,
            pc: 0,
            status: 0,
            header: JitFrameHeader {
                frame_id: NEXT_ACTIVE_FRAME_ID.fetch_add(1, Ordering::Relaxed),
                descriptor_id: self.frame_descriptor.id(),
                caller: FrameCaller::Interpreter {
                    frame_depth: limits.interpreter_frame_depth,
                },
            },
            poll_remaining: limits.poll_iterations.saturating_sub(1),
        };
        // SAFETY: the emitter validates every register and branch, generated code
        // only accesses this frame and its fixed-size register allocation, and the
        // RX mapping remains owned for the duration of the call.
        let entry: unsafe extern "C" fn(*mut NativeFrame) =
            unsafe { std::mem::transmute(self.memory.as_ptr().add(machine_offset as usize)) };
        unsafe { entry(&raw mut frame) };
        debug_assert_eq!(frame.header.descriptor_id, self.frame_descriptor.id());
        *loop_iterations = frame.loop_iterations;
        for (register, &write_kind) in write_kinds.iter().enumerate() {
            if write_kind != 0 {
                values[register] = Some(registers[register]);
            }
        }
        Some(if frame.status == 0 {
            ArithmeticExit::Completed(frame.pc)
        } else {
            ArithmeticExit::Bailout {
                pc: frame.pc,
                reason: DeoptReason::from_status(frame.status)
                    .expect("generated exit status is emitted from a deopt reason"),
            }
        })
    }
}

#[derive(Debug, Default)]
pub(crate) struct ArithmeticRuntime {
    entries: HashMap<(u64, u32), RuntimeEntry>,
    insertion_order: VecDeque<(u64, u32)>,
    diagnostics: ArithmeticJitDiagnostics,
}

/// Runtime-wide counters for the arithmetic baseline tier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ArithmeticJitDiagnostics {
    /// Hot loops submitted to the emitter.
    pub compile_requests: u64,
    /// Requests that installed generated code.
    pub successful_compilations: u64,
    /// Requests rejected as unsupported or invalid.
    pub compile_rejections: u64,
    /// Wall-clock nanoseconds spent verifying and emitting all compile requests.
    pub total_compile_time_ns: u64,
    /// Executable bytes emitted by successful compile requests.
    pub generated_code_bytes: u64,
    /// Calls that entered generated machine code.
    pub compiled_entries: u64,
    /// Object aliases copied before writing completed native results to the VM.
    pub completed_alias_copies: u64,
    /// Allocations and capacity growths of completed-entry alias snapshots.
    /// Pure arithmetic completions do not allocate a snapshot.
    pub completed_alias_allocations: u64,
    /// Calls that resumed the interpreter.
    pub bailouts: u64,
    /// Cached loop sites evicted to keep executable memory bounded.
    pub cache_evictions: u64,
    /// Native entries whose property shape and IC guards all matched.
    pub property_guard_hits: u64,
    /// Native entries rejected because an IC was relinked or a shape changed.
    pub property_guard_misses: u64,
    /// Property-enabled native entries that resumed at an exact bytecode PC.
    pub property_bailouts: u64,
    /// Deopts caused by stale property shape or inline-cache guards.
    pub shape_deopts: u64,
    /// Deopts caused by entry values outside the baseline representation contract,
    /// including negative zero supplied by the interpreter.
    pub type_deopts: u64,
    /// Deopts caused when generated arithmetic overflows, creates negative zero,
    /// or reaches another unsupported Number result.
    pub arithmetic_deopts: u64,
    /// Deopts caused by cooperative interruption at a loop safepoint.
    pub interrupt_deopts: u64,
    /// Deopts that entered interpreter exception handling.
    pub exception_deopts: u64,
    /// Explicitly requested interpreter reconstructions.
    pub explicit_deopts: u64,
}

impl ArithmeticJitDiagnostics {
    fn record_deopt(&mut self, reason: DeoptReason) {
        self.bailouts = self.bailouts.saturating_add(1);
        let counter = match reason {
            DeoptReason::ShapeGuard => &mut self.shape_deopts,
            DeoptReason::TypeGuard => &mut self.type_deopts,
            DeoptReason::ArithmeticGuard => &mut self.arithmetic_deopts,
            DeoptReason::Interrupt => &mut self.interrupt_deopts,
            DeoptReason::Exception => &mut self.exception_deopts,
            DeoptReason::Explicit => &mut self.explicit_deopts,
        };
        *counter = counter.saturating_add(1);
    }
}

fn reconstruct_deopt(
    code: &ArithmeticCode,
    vm: &mut Vm,
    diagnostics: &mut ArithmeticJitDiagnostics,
    values: &[Option<i64>],
    write_kinds: &[u8],
    pc: u32,
    reason: DeoptReason,
) {
    let recipe = code
        .deopt_recipes
        .get(&pc)
        .expect("generated and entry exits have verified deopt recipes");
    debug_assert_eq!(recipe.bytecode_offset(), pc);
    debug_assert_eq!(recipe.resume_point(), DeoptResumePoint::BeforeOperation);
    debug_assert_eq!(recipe.environment(), DeoptEnvironment::Preserve);
    debug_assert_eq!(recipe.pending_call(), DeoptPendingCall::Preserve);

    let register_count = vm.frame.code_block.register_count as usize;
    let mut reconstructed = (0..register_count)
        .map(|register| vm.get_register(register).clone())
        .collect::<Vec<_>>();
    recipe
        .materialize(&mut reconstructed, |location| {
            let ValueLocation::FrameRegister(source) = location else {
                unreachable!("the arithmetic tier only emits frame-register recipes");
            };
            let source = source as usize;
            match write_kinds[source] {
                0 => Some(DeoptSourceValue::Preserve),
                1 | 2 => Some(DeoptSourceValue::NativeTagged {
                    value: values[source].expect("a dirty generated register has a value"),
                    is_boolean: write_kinds[source] == 2,
                }),
                3 => Some(DeoptSourceValue::Tagged(
                    vm.get_register(values[source].expect("object alias has a source") as usize)
                        .clone(),
                )),
                _ => unreachable!("emitter only writes validated arithmetic value kinds"),
            }
        })
        .expect("installed arithmetic deopt recipes match the generated frame");
    for (register, value) in reconstructed.into_iter().enumerate() {
        vm.set_register(register, value);
    }
    vm.frame.pc = recipe.bytecode_offset();
    vm.frame.code_block.jit_metadata.record_reusable_fallback();
    diagnostics.record_deopt(reason);
}

#[derive(Debug)]
enum RuntimeEntry {
    Warming(u32),
    Compiled(ArithmeticCode),
    Unsupported,
}

impl ArithmeticRuntime {
    const HOT_LOOP_THRESHOLD: u32 = 32;
    const MAX_CACHE_ENTRIES: usize = 256;

    pub(crate) const fn diagnostics(&self) -> ArithmeticJitDiagnostics {
        self.diagnostics
    }

    pub(crate) fn write_debug_snapshot(&self, output: &mut String) {
        let mut entries = self.entries.iter().collect::<Vec<_>>();
        entries.sort_unstable_by_key(|(key, _)| **key);
        for ((code_id, pc), entry) in entries {
            if let RuntimeEntry::Compiled(code) = entry {
                writeln!(
                    output,
                    "tier=arithmetic code_id={code_id} bytecode_entry_pc={pc}"
                )
                .expect("String formatting cannot fail");
                writeln!(
                    output,
                    "frame={:#?}\ncode_map={:#?}\ndeopt={:#?}",
                    code.frame_descriptor, code.code_map, code.deopt_recipes
                )
                .expect("String formatting cannot fail");
                code.memory.write_debug_bytes(output);
            }
        }
    }

    fn ensure_entry(&mut self, key: (u64, u32)) {
        if !self.entries.contains_key(&key) {
            if self.entries.len() >= Self::MAX_CACHE_ENTRIES {
                let oldest = self
                    .insertion_order
                    .pop_front()
                    .expect("a full arithmetic cache has an insertion-order entry");
                let removed = self.entries.remove(&oldest);
                debug_assert!(removed.is_some());
                self.diagnostics.cache_evictions =
                    self.diagnostics.cache_evictions.saturating_add(1);
            }
            self.insertion_order.push_back(key);
        }
        self.entries.entry(key).or_insert(RuntimeEntry::Warming(0));
    }

    /// Observes a loop header and, once hot, replaces repeated bytecode dispatch
    /// with one bounded generated-code call.
    pub(crate) fn try_execute_after_increment(&mut self, vm: &mut Vm) -> bool {
        if vm.arithmetic_jit_budget == Some(0) {
            return false;
        }
        let pc = vm.frame.pc;
        let key = (vm.frame.code_block.jit_code_id, pc);
        self.ensure_entry(key);
        let entry = self
            .entries
            .get_mut(&key)
            .expect("the arithmetic cache entry was just ensured");
        match entry {
            RuntimeEntry::Warming(count) => {
                *count = count.saturating_add(1);
                if *count < Self::HOT_LOOP_THRESHOLD {
                    return false;
                }
                vm.frame.code_block.jit_metadata.mark_queued();
                self.diagnostics.compile_requests =
                    self.diagnostics.compile_requests.saturating_add(1);
                let compile_started = Instant::now();
                let snapshot = vm.frame.code_block.bytecode_contract().verify().ok();
                let single_loop = snapshot.as_ref().is_some_and(|snapshot| {
                    snapshot
                        .instructions
                        .iter()
                        .filter(|instruction| instruction.name == "IncrementLoopIteration")
                        .count()
                        == 1
                });
                let compiled = snapshot.as_ref().and_then(|snapshot| {
                    ArithmeticCode::compile(snapshot, &vm.frame.code_block.ic, pc)
                        .unwrap_or_default()
                });
                self.diagnostics.total_compile_time_ns =
                    self.diagnostics.total_compile_time_ns.saturating_add(
                        u64::try_from(compile_started.elapsed().as_nanos())
                            .unwrap_or(u64::MAX)
                            .max(1),
                    );
                let Some(code) = compiled.filter(|code| code.bytecode_resume == pc) else {
                    if single_loop {
                        vm.frame.code_block.jit_metadata.disable();
                    } else {
                        vm.frame.code_block.jit_metadata.mark_interpreter();
                    }
                    self.diagnostics.compile_rejections =
                        self.diagnostics.compile_rejections.saturating_add(1);
                    *entry = RuntimeEntry::Unsupported;
                    return false;
                };
                vm.frame
                    .code_block
                    .jit_metadata
                    .mark_compiled(BYTECODE_CONTRACT_VERSION);
                self.diagnostics.successful_compilations =
                    self.diagnostics.successful_compilations.saturating_add(1);
                self.diagnostics.generated_code_bytes = self
                    .diagnostics
                    .generated_code_bytes
                    .saturating_add(u64::try_from(code.generated_code_bytes()).unwrap_or(u64::MAX));
                *entry = RuntimeEntry::Compiled(code);
            }
            RuntimeEntry::Unsupported => return false,
            RuntimeEntry::Compiled(code) if code.bytecode_resume != pc => return false,
            RuntimeEntry::Compiled(_) => {}
        }

        let RuntimeEntry::Compiled(code) = entry else {
            unreachable!("successful compilation was installed above")
        };
        let mut poll_iterations = if vm.runtime_limits.deadline().is_some() {
            4096
        } else {
            u64::MAX
        };
        if let Some(budget) = vm.arithmetic_jit_budget {
            let iterations = budget / code.iteration_cost;
            if iterations == 0 {
                return false;
            }
            poll_iterations = poll_iterations.min(u64::from(iterations));
        }
        debug_assert!(!code.code_map.entries().is_empty());
        debug_assert!(!code.frame_descriptor.safepoints().is_empty());
        let register_count = vm.frame.code_block.register_count as usize;
        let mut values = (0..register_count)
            .map(|index| {
                let value = vm.get_register(index);
                let number = value.as_number()?;
                if number == 0.0 && number.is_sign_negative() {
                    return None;
                }
                (number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0)
                    .then_some(number as i64)
            })
            .collect::<Vec<_>>();
        let mut property_objects = vec![None::<JsObject>; code.properties.len()];
        let scratch_count = code
            .properties
            .iter()
            .map(|binding| binding.scratch_register as usize + 1)
            .max()
            .unwrap_or(register_count)
            .max(register_count);
        values.resize(scratch_count, None);
        let mut write_kinds = vec![0; values.len()];
        let mut property_guard_miss = false;
        for (binding_index, binding) in code.properties.iter().enumerate() {
            let Some(ic) = vm.frame.code_block.ic.get(binding.ic_index as usize) else {
                property_guard_miss = true;
                break;
            };
            let Some((cached_shape, cached_slot)) = ic.monomorphic_own_data_slot() else {
                ic.invalidate_native_contract();
                property_guard_miss = true;
                break;
            };
            if cached_shape != binding.shape
                || cached_slot.index != binding.slot
                || (binding.writable && !cached_slot.attributes.contains(SlotAttributes::WRITABLE))
            {
                ic.invalidate_native_contract();
                property_guard_miss = true;
                break;
            }
            let Some(object) = vm
                .get_register(binding.object_register as usize)
                .as_object()
            else {
                property_guard_miss = true;
                break;
            };
            let object_borrowed = object.borrow();
            if object_borrowed.shape_edge().to_addr_usize() != binding.shape {
                drop(object_borrowed);
                ic.invalidate_native_contract();
                property_guard_miss = true;
                break;
            }
            let Some(slot_value) = object_borrowed
                .properties()
                .storage
                .get(binding.slot as usize)
            else {
                drop(object_borrowed);
                ic.invalidate_native_contract();
                property_guard_miss = true;
                break;
            };
            let Some(number) = slot_value.as_number() else {
                reconstruct_deopt(
                    code,
                    vm,
                    &mut self.diagnostics,
                    &values,
                    &write_kinds,
                    pc,
                    DeoptReason::TypeGuard,
                );
                return false;
            };
            if number == 0.0 && number.is_sign_negative() {
                reconstruct_deopt(
                    code,
                    vm,
                    &mut self.diagnostics,
                    &values,
                    &write_kinds,
                    pc,
                    DeoptReason::TypeGuard,
                );
                return false;
            }
            let Some(value) = (number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0)
                .then_some(number as i64)
            else {
                reconstruct_deopt(
                    code,
                    vm,
                    &mut self.diagnostics,
                    &values,
                    &write_kinds,
                    pc,
                    DeoptReason::TypeGuard,
                );
                return false;
            };
            let scratch = binding.scratch_register as usize;
            if let Some(previous) = values[scratch]
                && previous != value
            {
                property_guard_miss = true;
                break;
            }
            values[scratch] = Some(value);
            drop(object_borrowed);
            property_objects[binding_index] = Some(object);
        }
        if property_guard_miss {
            self.diagnostics.property_guard_misses =
                self.diagnostics.property_guard_misses.saturating_add(1);
            reconstruct_deopt(
                code,
                vm,
                &mut self.diagnostics,
                &values,
                &write_kinds,
                pc,
                DeoptReason::ShapeGuard,
            );
            *entry = RuntimeEntry::Warming(0);
            return false;
        }
        if !code.properties.is_empty() {
            self.diagnostics.property_guard_hits =
                self.diagnostics.property_guard_hits.saturating_add(1);
        }
        let interpreter_frame_depth = vm.frames.len();
        let iterations_before = vm.frame.loop_iteration_count;
        let Some(exit) = code.execute_after_increment_typed(
            &mut values,
            &mut write_kinds,
            &mut vm.frame.loop_iteration_count,
            NativeEntryLimits {
                loop_limit: vm.runtime_limits.loop_iteration_limit(),
                poll_iterations,
                interpreter_frame_depth,
            },
        ) else {
            reconstruct_deopt(
                code,
                vm,
                &mut self.diagnostics,
                &values,
                &write_kinds,
                pc,
                DeoptReason::TypeGuard,
            );
            return false;
        };
        if let Some(budget) = &mut vm.arithmetic_jit_budget {
            let iterations = vm
                .frame
                .loop_iteration_count
                .wrapping_sub(iterations_before)
                .saturating_add(1);
            let cost = iterations.saturating_mul(u64::from(code.iteration_cost));
            *budget = budget.saturating_sub(u32::try_from(cost).unwrap_or(u32::MAX));
        }
        vm.frame.code_block.jit_metadata.record_compiled_entry();
        self.diagnostics.compiled_entries = self.diagnostics.compiled_entries.saturating_add(1);
        for (binding_index, binding) in code.properties.iter().enumerate() {
            if !binding.writable || write_kinds[binding.scratch_register as usize] == 0 {
                continue;
            }
            let object = property_objects[binding_index]
                .as_ref()
                .expect("validated property binding has a rooted object");
            let mut object_borrowed = object.borrow_mut();
            object_borrowed.properties_mut().storage[binding.slot as usize] = JsValue::from(
                values[binding.scratch_register as usize].expect("dirty slot") as f64,
            );
        }
        match exit {
            ArithmeticExit::Completed(pc) => {
                // Preserve only alias sources, before any original register can
                // be overwritten. An arithmetic-only completion allocates nothing.
                let aliases = if code.properties.is_empty() {
                    // Only property bindings can introduce object-tagged Moves.
                    // Scalar-only code needs neither a snapshot nor a second scan.
                    Vec::new()
                } else {
                    snapshot_completed_aliases(
                        register_count,
                        &values,
                        &write_kinds,
                        |source| vm.get_register(source).clone(),
                        &mut self.diagnostics,
                    )
                };
                for (index, (&value, &write_kind)) in values
                    .iter()
                    .zip(&write_kinds)
                    .take(register_count)
                    .enumerate()
                {
                    match (value, write_kind) {
                        (Some(value), 1) => vm.set_register(index, JsValue::from(value as f64)),
                        (Some(value), 2) => vm.set_register(index, JsValue::from(value != 0)),
                        (Some(_), 3) | (_, 0) => {}
                        _ => unreachable!("emitter only writes validated arithmetic value kinds"),
                    }
                }
                for (index, value) in aliases {
                    vm.set_register(index, value);
                }
                vm.frame.pc = pc;
                true
            }
            ArithmeticExit::Bailout { pc, reason } => {
                reconstruct_deopt(
                    code,
                    vm,
                    &mut self.diagnostics,
                    &values,
                    &write_kinds,
                    pc,
                    reason,
                );
                if !code.properties.is_empty() {
                    self.diagnostics.property_bailouts =
                        self.diagnostics.property_bailouts.saturating_add(1);
                }
                true
            }
        }
    }
}

// Keep every source rooted until all scalar and alias writes have finished.
// Destinations may overwrite other alias sources, including cyclic mappings.
fn snapshot_completed_aliases(
    register_count: usize,
    values: &[Option<i64>],
    write_kinds: &[u8],
    mut read_register: impl FnMut(usize) -> JsValue,
    diagnostics: &mut ArithmeticJitDiagnostics,
) -> Vec<(usize, JsValue)> {
    let mut aliases = Vec::new();
    for (index, (&value, &kind)) in values
        .iter()
        .zip(write_kinds)
        .take(register_count)
        .enumerate()
    {
        if kind != 3 {
            continue;
        }
        let source = value.expect("object alias has a source") as usize;
        assert!(
            source < register_count,
            "object alias source is inside VM frame"
        );
        if aliases.len() == aliases.capacity() {
            diagnostics.completed_alias_allocations =
                diagnostics.completed_alias_allocations.saturating_add(1);
        }
        aliases.push((index, read_register(source)));
        diagnostics.completed_alias_copies = diagnostics.completed_alias_copies.saturating_add(1);
    }
    aliases
}

struct LoopRegion {
    first: usize,
    increment: usize,
    end: usize,
    exit: u32,
    required: Vec<u32>,
}

#[cfg(test)]
thread_local! {
    static PROPERTY_BINDING_VISITS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

#[derive(Clone, Copy)]
struct ObjectOrigin {
    register: u32,
    last_move: Option<usize>,
}

#[derive(Default)]
struct PropertyObjectOrigins {
    // Absent registers are entry values; None marks a scalar overwrite.
    registers: HashMap<u32, Option<ObjectOrigin>>,
    moves: Vec<(u32, ObjectOrigin)>,
}

impl PropertyObjectOrigins {
    fn origin(&self, register: u32) -> Option<ObjectOrigin> {
        self.registers
            .get(&register)
            .copied()
            .unwrap_or(Some(ObjectOrigin {
                register,
                last_move: None,
            }))
    }

    fn resolve(&self, register: u32, used_moves: &mut BTreeMap<u32, u32>) -> Option<u32> {
        let origin = self.origin(register)?;
        let mut previous = origin.last_move;
        while let Some(index) = previous {
            let (offset, source) = self.moves[index];
            if used_moves.insert(offset, origin.register).is_some() {
                break;
            }
            #[cfg(test)]
            PROPERTY_BINDING_VISITS.with(|count| {
                let (instructions, moves) = count.get();
                count.set((instructions, moves + 1));
            });
            previous = source.last_move;
        }
        Some(origin.register)
    }

    fn record(&mut self, instruction: &crate::vm::BytecodeInstruction) {
        let Some(dst) = unsigned(instruction, "dst").and_then(|v| u32::try_from(v).ok()) else {
            return;
        };
        let origin = if instruction.name == "Move" {
            unsigned(instruction, "src")
                .and_then(|v| u32::try_from(v).ok())
                .and_then(|register| self.origin(register))
                .map(|source| {
                    let index = self.moves.len();
                    self.moves.push((instruction.offset, source));
                    ObjectOrigin {
                        register: source.register,
                        last_move: Some(index),
                    }
                })
        } else {
            None
        };
        self.registers.insert(dst, origin);
    }
}

fn property_bindings(
    snapshot: &BytecodeContractSnapshot,
    inline_caches: &[InlineCache],
    region: &LoopRegion,
) -> Option<(Vec<PropertyBinding>, BTreeMap<u32, u32>)> {
    let mut bindings = Vec::<PropertyBinding>::new();
    let mut by_slot = HashMap::new();
    let mut ic_sites = HashSet::new();
    let mut unique_slots = 0u32;
    let mut object_move_offsets = BTreeMap::new();
    let mut origins = PropertyObjectOrigins::default();
    let instructions = &snapshot.instructions[region.first..region.end];
    let mut has_properties = false;
    for instruction in instructions {
        #[cfg(test)]
        PROPERTY_BINDING_VISITS.with(|count| {
            let (i, m) = count.get();
            count.set((i + 1, m));
        });
        if matches!(instruction.name, "GetPropertyByName" | "SetPropertyByName") {
            has_properties = true;
            break;
        }
    }
    if !has_properties {
        return Some((bindings, object_move_offsets));
    }
    let mut written = HashSet::new();
    for instruction in instructions {
        #[cfg(test)]
        PROPERTY_BINDING_VISITS.with(|count| {
            let (i, m) = count.get();
            count.set((i + 1, m));
        });
        if let Some(dst) = unsigned(instruction, "dst") {
            written.insert(dst);
        }
        if instruction.name == "AddAssignLocal"
            && let Some(value) = unsigned(instruction, "value")
        {
            written.insert(value);
        }
    }
    for instruction in instructions {
        #[cfg(test)]
        PROPERTY_BINDING_VISITS.with(|count| {
            let (i, m) = count.get();
            count.set((i + 1, m));
        });
        let property = match instruction.name {
            "GetPropertyByName" => Some(("value", false)),
            "SetPropertyByName" => Some(("object", true)),
            _ => None,
        };
        if let Some((object_operand, is_write)) = property {
            if unsigned(instruction, "receiver")? != unsigned(instruction, object_operand)? {
                return None;
            }
            let ic_index = u32::try_from(unsigned(instruction, "ic_index")?).ok()?;
            let temporary_object = u32::try_from(unsigned(instruction, object_operand)?).ok()?;
            let object_register = origins.resolve(temporary_object, &mut object_move_offsets)?;
            // Alias payloads refer to stable VM entry registers, never scalar writes.
            if written.contains(&u64::from(object_register)) {
                return None;
            }
            let (shape, slot) = inline_caches
                .get(ic_index as usize)?
                .monomorphic_own_data_slot()?;
            if is_write && !slot.attributes.contains(SlotAttributes::WRITABLE) {
                return None;
            }
            let key = (object_register, shape, slot.index);
            let scratch_register = if let Some(&index) = by_slot.get(&key) {
                let binding: &mut PropertyBinding = &mut bindings[index];
                binding.writable |= is_write;
                binding.scratch_register
            } else {
                let scratch_register = snapshot.register_count.checked_add(unique_slots)?;
                unique_slots = unique_slots.checked_add(1)?;
                by_slot.insert(key, bindings.len());
                bindings.push(PropertyBinding {
                    ic_index,
                    object_register,
                    shape,
                    slot: slot.index,
                    scratch_register,
                    writable: is_write,
                });
                ic_sites.insert(ic_index);
                scratch_register
            };
            // Retain a separate guard for each IC, even when its slot is shared.
            if ic_sites.insert(ic_index) {
                bindings.push(PropertyBinding {
                    ic_index,
                    object_register,
                    shape,
                    slot: slot.index,
                    scratch_register,
                    writable: false,
                });
            }
        }
        // Resolve property inputs before recording this instruction's destination.
        origins.record(instruction);
    }
    let roots: HashSet<_> = bindings
        .iter()
        .map(|binding| binding.object_register)
        .collect();
    // Validate every operand once, rather than repeating the walk for each binding.
    for instruction in instructions {
        #[cfg(test)]
        PROPERTY_BINDING_VISITS.with(|count| {
            let (i, m) = count.get();
            count.set((i + 1, m));
        });
        for operand in &instruction.operands {
            if operand.name == "dst"
                || !register_operand(instruction.name, operand.name, operand.value)
                    .is_some_and(|register| roots.contains(&register))
            {
                continue;
            }
            let object_use = match instruction.name {
                "Move" => object_move_offsets.contains_key(&instruction.offset),
                "GetPropertyByName" => matches!(operand.name, "value" | "receiver"),
                "SetPropertyByName" => matches!(operand.name, "object" | "receiver"),
                _ => false,
            };
            if !object_use {
                return None;
            }
        }
    }
    Some((bindings, object_move_offsets))
}

impl LoopRegion {
    fn find(snapshot: &BytecodeContractSnapshot, bytecode_resume: u32) -> Option<Self> {
        let increment = snapshot.instructions.iter().position(|instruction| {
            instruction.name == "IncrementLoopIteration"
                && instruction.next_offset == bytecode_resume
        })?;
        let increment_offset = snapshot.instructions[increment].offset;
        let by_offset = snapshot
            .instructions
            .iter()
            .enumerate()
            .map(|(index, instruction)| (instruction.offset, index))
            .collect::<BTreeMap<_, _>>();
        let (backedge, first) = snapshot.instructions[increment..]
            .iter()
            .enumerate()
            .find_map(|(relative, instruction)| {
                if instruction.name != "Jump" {
                    return None;
                }
                let target = u32::try_from(unsigned(instruction, "address")?).ok()?;
                let &first = by_offset.get(&target)?;
                (target <= increment_offset).then_some((increment + relative, first))
            })?;
        let exit = snapshot.instructions[backedge].next_offset;
        if !snapshot.instructions[first..=backedge].iter().any(|i| {
            matches!(i.name, "JumpIfTrue" | "JumpIfFalse")
                && unsigned(i, "address") == Some(u64::from(exit))
        }) {
            return None;
        }
        let supported = [
            "IncrementLoopIteration",
            "Move",
            "PushZero",
            "PushOne",
            "PushInt8",
            "PushInt16",
            "PushInt32",
            "Inc",
            "Add",
            "AddAssignLocal",
            "Sub",
            "Mul",
            "Mod",
            "LessThan",
            "LessThanOrEq",
            "GreaterThan",
            "GreaterThanOrEq",
            "StrictEq",
            "StrictNotEq",
            "GetPropertyByName",
            "SetPropertyByName",
            "Jump",
            "JumpIfTrue",
            "JumpIfFalse",
        ];
        if snapshot.instructions[first..=backedge]
            .iter()
            .any(|i| !supported.contains(&i.name))
        {
            return None;
        }
        let mut written = BTreeSet::new();
        for i in &snapshot.instructions[first..=backedge] {
            if let Some(dst) = unsigned(i, "dst").map(|v| v as u32) {
                written.insert(dst);
            }
        }
        let required = required_registers(&snapshot.instructions[first..=backedge])?;
        if required
            .iter()
            .chain(written.iter())
            .any(|&r| r >= snapshot.register_count)
        {
            return None;
        }
        Some(Self {
            first,
            increment,
            end: backedge + 1,
            exit,
            required: required.into_iter().collect(),
        })
    }
}

fn required_registers(instructions: &[crate::vm::BytecodeInstruction]) -> Option<BTreeSet<u32>> {
    let by_offset = instructions
        .iter()
        .enumerate()
        .map(|(index, instruction)| (instruction.offset, index))
        .collect::<BTreeMap<_, _>>();
    let mut definitely_written = vec![None::<BTreeSet<u32>>; instructions.len()];
    definitely_written[0] = Some(BTreeSet::new());
    let mut queue = VecDeque::from([0_usize]);
    while let Some(index) = queue.pop_front() {
        let instruction = &instructions[index];
        let mut outgoing = definitely_written[index].clone()?;
        if let Some(dst) = unsigned(instruction, "dst").and_then(|v| u32::try_from(v).ok()) {
            outgoing.insert(dst);
        }
        let mut successors = Vec::with_capacity(2);
        if matches!(instruction.name, "Jump" | "JumpIfTrue" | "JumpIfFalse")
            && let Some(target) =
                unsigned(instruction, "address").and_then(|v| u32::try_from(v).ok())
            && let Some(&target_index) = by_offset.get(&target)
        {
            successors.push(target_index);
        }
        if instruction.name != "Jump" && index + 1 < instructions.len() {
            successors.push(index + 1);
        }
        successors.sort_unstable();
        successors.dedup();
        for successor in successors {
            let merged = definitely_written[successor].as_ref().map_or_else(
                || outgoing.clone(),
                |current| current.intersection(&outgoing).copied().collect(),
            );
            if definitely_written[successor].as_ref() != Some(&merged) {
                definitely_written[successor] = Some(merged);
                queue.push_back(successor);
            }
        }
    }

    let mut required = BTreeSet::new();
    for (instruction, initialized) in instructions.iter().zip(definitely_written) {
        let initialized = initialized?;
        for operand in &instruction.operands {
            let Some(register) = register_operand(instruction.name, operand.name, operand.value)
            else {
                continue;
            };
            if operand.name != "dst" && !initialized.contains(&register) {
                required.insert(register);
            }
        }
    }
    Some(required)
}

fn unsigned(i: &crate::vm::BytecodeInstruction, name: &str) -> Option<u64> {
    i.operands
        .iter()
        .find(|o| o.name == name)
        .and_then(|o| match o.value {
            crate::vm::BytecodeOperandValue::Unsigned(v) => Some(v),
            _ => None,
        })
}
fn signed(i: &crate::vm::BytecodeInstruction, name: &str) -> Option<i64> {
    i.operands
        .iter()
        .find(|o| o.name == name)
        .and_then(|o| match o.value {
            crate::vm::BytecodeOperandValue::Signed(v) => Some(v),
            _ => None,
        })
}
fn register_operand(
    name: &str,
    operand: &str,
    value: crate::vm::BytecodeOperandValue,
) -> Option<u32> {
    if !crate::vm::bytecode_contract::is_register_operand(name, operand) {
        return None;
    }
    match value {
        crate::vm::BytecodeOperandValue::Unsigned(v) => u32::try_from(v).ok(),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Label {
    Bytecode(u32),
    Bailout(u32, DeoptReason),
    Internal(u32, u8),
}
#[cfg(all(
    test,
    any(target_arch = "x86_64", target_arch = "aarch64"),
    any(target_os = "linux", target_os = "macos")
))]
mod tests {
    use crate::{
        Context, Script,
        vm::{BytecodeConstant, Constant, JitCompilationState},
    };
    use boa_parser::Source;
    use futures_lite::future;

    use super::*;

    #[test]
    fn property_binding_walk_is_bounded_and_keeps_every_inline_cache_guard() {
        for (properties, repeats) in [(1, 1), (8, 1), (32, 1), (8, 4)] {
            let object = (0..properties)
                .map(|i| format!("p{i}:{}", i + 1))
                .collect::<Vec<_>>()
                .join(",");
            let accesses =
                (0..repeats)
                    .flat_map(|_| 0..properties)
                    .fold(String::new(), |mut output, i| {
                        std::fmt::Write::write_fmt(&mut output, format_args!("s+=o.p{i};"))
                            .expect("writing to a String cannot fail");
                        output
                    });
            let source = format!(
                "function f(o,n){{var s=0;for(var i=0;i<n;i++){{{accesses}}}return s}}var o={{{object}}};f(o,200)"
            );
            let expected = 200 * repeats * properties * (properties + 1) / 2;
            let mut context = Context::default();
            context.set_baseline_jit_enabled(false);
            let script = Script::parse(Source::from_bytes(&source), None, &mut context).unwrap();
            let outer = script.codeblock(&mut context).unwrap();
            let function_index = outer
                .constants
                .iter()
                .position(|c| matches!(c, Constant::Function(_)))
                .unwrap();
            let function = outer.constant_function(function_index);
            assert_eq!(
                script.evaluate(&mut context).unwrap().as_number(),
                Some(expected as f64)
            );
            let snapshot = function.bytecode_contract().verify().unwrap();
            let resume = snapshot
                .instructions
                .iter()
                .find(|i| i.name == "IncrementLoopIteration")
                .unwrap()
                .next_offset;
            let region = LoopRegion::find(&snapshot, resume).unwrap();
            let instruction_count = region.end - region.first;
            PROPERTY_BINDING_VISITS.with(|count| count.set((0, 0)));
            let (bindings, moves) = property_bindings(&snapshot, &function.ic, &region).unwrap();
            let (visits, move_visits) = PROPERTY_BINDING_VISITS.with(std::cell::Cell::get);
            assert!(visits <= 4 * instruction_count);
            assert_eq!(move_visits, moves.len());
            assert!(move_visits <= instruction_count);
            assert_eq!(bindings.len(), properties * repeats);
            assert_eq!(
                bindings
                    .iter()
                    .map(|b| b.ic_index)
                    .collect::<HashSet<_>>()
                    .len(),
                properties * repeats
            );
            assert_eq!(
                bindings
                    .iter()
                    .map(|b| b.scratch_register)
                    .collect::<HashSet<_>>()
                    .len(),
                properties
            );
            eprintln!(
                "properties={properties} repeats={repeats} instructions={instruction_count} visits={visits} moves={move_visits} guards={}",
                bindings.len()
            );
            context.set_baseline_jit_enabled(true);
            assert_eq!(
                context
                    .eval(Source::from_bytes("f(o,200)"))
                    .unwrap()
                    .as_number(),
                Some(expected as f64)
            );
            assert!(context.arithmetic_jit_diagnostics().compiled_entries > 0);
            // Relink a later site of the same slot: it must still have its own guard.
            if repeats > 1 {
                let index = bindings[properties].ic_index as usize;
                let ic = &function.ic[index];
                // A bad slot is repaired by the initial interpreter iteration.
                // Instead retain a second live shape at just this later site;
                // generic reads remain valid but its native contract is stale.
                let alternate = context
                    .eval(Source::from_bytes("({p0:1,extra:0})"))
                    .unwrap();
                let alternate_object = alternate.as_object().unwrap();
                let alternate_shape = alternate_object.borrow().shape_edge().clone();
                ic.set(&alternate_shape, ic.slot());
                assert!(ic.monomorphic_own_data_slot().is_none());
                assert_eq!(
                    context
                        .eval(Source::from_bytes("f(o,200)"))
                        .unwrap()
                        .as_number(),
                    Some(expected as f64)
                );
                assert!(context.arithmetic_jit_diagnostics().property_guard_misses > 0);
            }
        }
    }

    #[test]
    fn object_origin_index_preserves_move_history_across_register_overwrites() {
        let mut origins = PropertyObjectOrigins::default();
        let mut used = BTreeMap::new();
        origins.record(&instruction(
            0,
            4,
            "Move",
            vec![register("dst", 2), register("src", 0)],
        ));
        origins.record(&instruction(
            4,
            8,
            "Move",
            vec![register("dst", 3), register("src", 2)],
        ));
        origins.record(&instruction(
            8,
            12,
            "Move",
            vec![register("dst", 2), register("src", 1)],
        ));
        assert_eq!(origins.resolve(3, &mut used), Some(0));
        assert_eq!(used, BTreeMap::from([(0, 0), (4, 0)]));
        assert_eq!(origins.resolve(2, &mut used), Some(1));
        assert_eq!(used.get(&8), Some(&1));
        origins.record(&instruction(12, 16, "PushZero", vec![register("dst", 2)]));
        assert_eq!(origins.resolve(2, &mut used), None);
        assert_eq!(origins.resolve(3, &mut used), Some(0));
    }

    #[test]
    #[should_panic(expected = "object alias source is inside VM frame")]
    fn completed_alias_snapshot_rejects_sources_outside_the_vm_frame() {
        snapshot_completed_aliases(
            1,
            &[Some(1)],
            &[3],
            |_| panic!("must fail before reading a VM register"),
            &mut ArithmeticJitDiagnostics::default(),
        );
    }

    #[test]
    fn completed_alias_snapshot_ignores_unused_and_scalar_registers() {
        for count in [8, 64, 1024] {
            let values = vec![Some(7); count];
            let kinds = vec![1; count];
            let mut diagnostics = ArithmeticJitDiagnostics::default();
            let aliases = snapshot_completed_aliases(
                count,
                &values,
                &kinds,
                |_| panic!("arithmetic completion must not read original registers"),
                &mut diagnostics,
            );
            assert!(aliases.is_empty());
            assert_eq!(aliases.capacity(), 0);
            assert_eq!(diagnostics.completed_alias_copies, 0);
            assert_eq!(diagnostics.completed_alias_allocations, 0);
        }
    }

    #[test]
    fn completed_alias_snapshot_preserves_cycles_and_survives_collection() {
        let first = JsValue::from(JsObject::with_null_proto());
        let second = JsValue::from(JsObject::with_null_proto());
        let mut registers = [first.clone(), second.clone(), JsValue::from(7)];
        let values = [Some(1), Some(0), Some(42), Some(999)];
        let kinds = [3, 3, 1, 3]; // Last entry is outside the VM register frame.
        let mut diagnostics = ArithmeticJitDiagnostics::default();
        let aliases = snapshot_completed_aliases(
            3,
            &values,
            &kinds,
            |source| registers[source].clone(),
            &mut diagnostics,
        );
        registers.fill(JsValue::undefined());
        boa_gc::force_collect();
        for (index, value) in aliases {
            registers[index] = value;
        }
        assert_eq!(registers[0], second);
        assert_eq!(registers[1], first);
        assert_eq!(diagnostics.completed_alias_copies, 2);
        assert_eq!(diagnostics.completed_alias_allocations, 1);
    }

    #[test]
    fn pure_arithmetic_native_entries_do_not_copy_original_registers() {
        let mut context = Context::default();
        let value = context
            .eval(Source::from_bytes(
                "function f(n){var s=0;for(var i=0;i<n;i++)s+=i;return s}f(200);f(200)",
            ))
            .unwrap();
        assert_eq!(value.as_number(), Some(19900.0));
        let diagnostics = context.arithmetic_jit_diagnostics();
        assert!(diagnostics.compiled_entries >= 2, "{diagnostics:?}");
        assert_eq!(diagnostics.completed_alias_copies, 0);
        assert_eq!(diagnostics.completed_alias_allocations, 0);
    }

    fn register(name: &'static str, value: u64) -> crate::vm::BytecodeOperand {
        crate::vm::BytecodeOperand {
            name,
            value: crate::vm::BytecodeOperandValue::Unsigned(value),
        }
    }

    fn instruction(
        offset: u32,
        next_offset: u32,
        name: &'static str,
        operands: Vec<crate::vm::BytecodeOperand>,
    ) -> crate::vm::BytecodeInstruction {
        crate::vm::BytecodeInstruction {
            offset,
            next_offset,
            opcode: 0,
            name,
            operands,
            source_line: None,
            source_column: None,
        }
    }

    fn arithmetic_contract(source: &str) -> BytecodeContractSnapshot {
        let mut context = Context::default();
        let outer = Script::parse(Source::from_bytes(source), None, &mut context)
            .unwrap()
            .codeblock(&mut context)
            .unwrap()
            .bytecode_contract()
            .verify()
            .unwrap();
        outer
            .constants
            .into_iter()
            .find_map(|constant| match constant {
                BytecodeConstant::Function { contract, .. } => Some(*contract),
                _ => None,
            })
            .unwrap()
    }

    fn compile_arithmetic(contract: &BytecodeContractSnapshot) -> ArithmeticCode {
        let resume = contract
            .instructions
            .iter()
            .find(|instruction| instruction.name == "IncrementLoopIteration")
            .unwrap()
            .next_offset;
        ArithmeticCode::compile(contract, &[], resume)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn runtime_cache_evicts_the_oldest_loop_site_at_its_bound() {
        let mut runtime = ArithmeticRuntime::default();
        for code_id in 0..=ArithmeticRuntime::MAX_CACHE_ENTRIES as u64 {
            runtime.ensure_entry((code_id, 1));
        }
        assert_eq!(runtime.entries.len(), ArithmeticRuntime::MAX_CACHE_ENTRIES);
        assert!(!runtime.entries.contains_key(&(0, 1)));
        assert!(
            runtime
                .entries
                .contains_key(&(ArithmeticRuntime::MAX_CACHE_ENTRIES as u64, 1))
        );
        assert_eq!(runtime.diagnostics.cache_evictions, 1);
    }

    #[test]
    fn emitted_machine_code_executes_gate3_arithmetic_loop() {
        let contract = arithmetic_contract(
            "(function(n){var s=1;for(var i=0;i<n;i++)s=(s+i*3)%1000003;return s})(8)",
        );
        let code = compile_arithmetic(&contract);
        let mut values = vec![Some(1), Some(8), Some(0), None, None, None];
        let mut iterations = 1;
        assert_eq!(
            code.execute_after_increment(&mut values, &mut iterations, u64::MAX),
            Some(ArithmeticExit::Completed(121))
        );
        assert_eq!(values[0], Some(85));
        assert_eq!(values[2], Some(8));
        assert_eq!(iterations, 8);
        assert!(code.code_map.entries().len() > 8);
        assert!(
            code.frame_descriptor
                .safepoints()
                .iter()
                .any(|point| point.kind == SafepointKind::LoopBackedge)
        );
        assert!(
            code.frame_descriptor
                .safepoints()
                .iter()
                .any(|point| point.kind == SafepointKind::Bailout)
        );
        assert!(
            code.frame_descriptor
                .safepoints()
                .iter()
                .all(|point| point.stack_map.live_values().is_empty())
        );
    }

    #[test]
    fn arithmetic_on_off_corpus_covers_number_boundaries_and_all_comparisons() {
        let sources = [
            "function f(n,s,a){for(var i=0;i<n;i++)s=s+a;return s} \
             f(200,1,2); [f(100,9007199254740950,3),f(100,-9007199254740950,-3),\
             f(100,NaN,1),f(100,Infinity,1),f(100,-0,0),f(100,1,'2')]",
            "function f(n,s,a){for(var i=0;i<n;i++)s=s*a;return s} \
             f(200,1,1); [f(100,9007199254740991,9007199254740991),\
             f(100,0,-1),f(100,-1,0),f(100,-9007199254740991,9007199254740991)]",
            "function f(n,s,a){for(var i=0;i<n;i++)s=s%a;return s} \
             f(200,7,3); [f(100,-6,3),f(100,5,0),f(100,-7,3),f(100,7,-3)]",
            "function f(n){var s=0;for(var i=0;i<n;i++){\
             if(i<50)s+=1;if(i<=50)s+=2;if(i>50)s+=3;if(i>=50)s+=4;\
             if(i===50)s+=5;if(i!==50)s+=6}return s} [f(2000)]",
            "function f(n){var b=false;for(var i=0;i<n;i++)b=i<3;return b} [f(200)]",
            "function f(n){var b=false;for(var i=0;i<n;i++)b=(i<3)===0;return b} [f(200)]",
            "function f(n){var b=false;for(var i=0;i<n;i++)b=(i<3)!==0;return b} [f(200)]",
            "function f(n){var b=false;for(var i=0;i<n;i++)b=0===(i<3);return b} [f(200)]",
            "function f(n){var b=false;for(var i=0;i<n;i++)b=0!==(i<3);return b} [f(200)]",
            "function f(n){var b=false;for(var i=0;i<n;i++)b=(i<300)===(i<400);return b} [f(200)]",
        ];
        for source in sources {
            let mut results = Vec::new();
            for enabled in [false, true] {
                let mut context = Context::default();
                context.set_baseline_jit_enabled(enabled);
                let result = context.eval(Source::from_bytes(source)).unwrap();
                context
                    .register_global_property(
                        crate::js_string!("results"),
                        result,
                        crate::property::Attribute::all(),
                    )
                    .unwrap();
                let rendered = context
                    .eval(Source::from_bytes(
                        "results.map(x=>typeof x+':'+(Object.is(x,-0)?'-0':String(x))).join('|')",
                    ))
                    .unwrap();
                let diagnostics = context.arithmetic_jit_diagnostics();
                if enabled {
                    assert!(
                        diagnostics.compiled_entries > 0,
                        "{source}: {diagnostics:?}"
                    );
                } else {
                    assert_eq!(diagnostics.compiled_entries, 0);
                }
                results.push(rendered.display().to_string());
            }
            assert_eq!(results[0], results[1], "{source}");
        }
    }

    #[test]
    #[allow(clippy::print_stderr)]
    fn issue305_native_and_interpreter_measurements_share_a_checksum() {
        const ITERATIONS: i64 = 200_000;
        let expected = (0..ITERATIONS).fold(1_i64, |sum, i| (sum + i * 3) % 1_000_003);
        for enabled in [false, true] {
            let mut context = Context::default();
            context.set_baseline_jit_enabled(enabled);
            context.eval(Source::from_bytes(
                "function measured(n){var s=1;for(var i=0;i<n;i++)s=(s+i*3)%1000003;return s}measured(200)"
            )).unwrap();
            let before = context.arithmetic_jit_diagnostics().compiled_entries;
            let start = Instant::now();
            let value = context
                .eval(Source::from_bytes("measured(200000)"))
                .unwrap();
            let elapsed = start.elapsed();
            assert_eq!(value.as_number(), Some(expected as f64));
            let entries = context.arithmetic_jit_diagnostics().compiled_entries - before;
            assert_eq!(entries > 0, enabled);
            if std::env::var_os("BOA_JIT_DIAGNOSTICS").is_some() {
                eprintln!(
                    "issue305-arith architecture={:?} jit={enabled} iterations={ITERATIONS} elapsed_ns={} checksum={expected} native_entries={entries}",
                    super::super::JitArchitecture::host(),
                    elapsed.as_nanos()
                );
            }
        }
    }

    #[test]
    fn branch_skipped_writes_remain_required_on_native_entry() {
        let instructions = [
            instruction(0, 4, "Move", vec![register("dst", 1), register("src", 0)]),
            instruction(
                4,
                8,
                "JumpIfFalse",
                vec![register("address", 12), register("value", 2)],
            ),
            instruction(8, 12, "Move", vec![register("dst", 3), register("src", 1)]),
            instruction(
                12,
                17,
                "Add",
                vec![register("dst", 4), register("lhs", 3), register("rhs", 1)],
            ),
        ];
        assert_eq!(
            required_registers(&instructions).unwrap(),
            BTreeSet::from([0, 2, 3])
        );
    }

    #[test]
    fn overflow_and_negative_zero_resume_at_exact_operation() {
        let contract = arithmetic_contract(
            "(function(n){var s=1;for(var i=0;i<n;i++)s=(s+i*3)%1000003;return s})(8)",
        );
        let code = compile_arithmetic(&contract);
        let mut values = vec![
            Some(9_007_199_254_740_991_i64),
            Some(3),
            Some(0),
            None,
            None,
            None,
        ];
        let mut iterations = 1;
        let exit = code
            .execute_after_increment(&mut values, &mut iterations, u64::MAX)
            .unwrap();
        assert!(matches!(
            exit,
            ArithmeticExit::Bailout {
                reason: DeoptReason::ArithmeticGuard,
                ..
            }
        ));

        let mut values = vec![Some(-1_000_006), Some(2), Some(0), None, None, None];
        let mut iterations = 1;
        assert!(matches!(
            code.execute_after_increment(&mut values, &mut iterations, u64::MAX),
            Some(ArithmeticExit::Bailout {
                reason: DeoptReason::ArithmeticGuard,
                ..
            })
        ));
    }

    #[test]
    fn multiplication_negative_zero_and_comparison_types_remain_observable() {
        let mut context = Context::default();
        let result = Script::parse(
            Source::from_bytes(
                "(function(n){var z=1,b=false,zero=0,neg=-1;for(var i=0;i<n;i++){z=zero*neg;b=i<3}\
                 return Object.is(z,-0) && typeof b === 'boolean'})(200)",
            ),
            None,
            &mut context,
        )
        .unwrap()
        .evaluate(&mut context)
        .unwrap();
        assert_eq!(result.as_boolean(), Some(true));
        let diagnostics = context.arithmetic_jit_diagnostics();
        assert_eq!(diagnostics.successful_compilations, 1);
        assert!(diagnostics.compiled_entries >= 1);
        assert!(diagnostics.bailouts >= 1);
        assert!(diagnostics.arithmetic_deopts >= 1);
    }

    #[test]
    fn synchronous_vm_dispatches_hot_loop_through_generated_code() {
        let mut context = Context::default();
        let instruction_count = context.vm.instruction_count.clone();
        let script = Script::parse(
            Source::from_bytes(
                "(function(n){var s=1;for(var i=0;i<n;i++)s=(s+i*3)%1000003;return s})(2000)",
            ),
            None,
            &mut context,
        )
        .unwrap();
        let outer = script.codeblock(&mut context).unwrap();
        let function_index = outer
            .constants
            .iter()
            .position(|constant| matches!(constant, Constant::Function(_)))
            .unwrap();
        let function = outer.constant_function(function_index);
        let result = script.evaluate(&mut context).unwrap();
        let expected = (0..2000_i64).fold(1_i64, |sum, i| (sum + i * 3) % 1_000_003);
        assert_eq!(result.as_number(), Some(expected as f64));
        // Interpreting the 19-op loop takes over 30k dispatches. This bound also
        // proves that the hot remainder crossed the native entry in one call.
        assert!(
            instruction_count.get() < 1_000,
            "hot loop stayed interpreted"
        );
        let runtime_diagnostics = context.arithmetic_jit_diagnostics();
        assert_eq!(runtime_diagnostics.compile_requests, 1);
        assert_eq!(runtime_diagnostics.successful_compilations, 1);
        assert!(runtime_diagnostics.total_compile_time_ns > 0);
        assert!(runtime_diagnostics.generated_code_bytes > 0);
        assert!(runtime_diagnostics.compiled_entries >= 1);
        let diagnostics = function.jit_metadata();
        assert_eq!(diagnostics.state, JitCompilationState::Compiled);
        assert_eq!(diagnostics.compile_requests, 1);
        assert!(diagnostics.compiled_entries >= 1);
    }

    #[test]
    fn monomorphic_property_read_and_write_run_in_generated_loop() {
        let mut context = Context::default();
        let instruction_count = context.vm.instruction_count.clone();
        let result = Script::parse(
            Source::from_bytes(
                "(function(n){let o={x:1},s=0;for(let i=0;i<n;i++){s=s+o.x;o.x=o.x+1}\
                 return s+o.x})(2000)",
            ),
            None,
            &mut context,
        )
        .unwrap()
        .evaluate(&mut context)
        .unwrap();
        assert_eq!(result.as_number(), Some(2_003_001.0));
        assert!(
            instruction_count.get() < 2_000,
            "property loop stayed interpreted"
        );
        let diagnostics = context.arithmetic_jit_diagnostics();
        assert_eq!(diagnostics.successful_compilations, 1);
        assert!(diagnostics.property_guard_hits >= 1);
        assert_eq!(diagnostics.property_guard_misses, 0);
    }

    #[test]
    fn property_store_preserves_a_boolean_created_after_native_entry() {
        const SOURCE: &str = "function f(o,n){for(var i=0;i<n;i++){var v=i;\
            if(i>=500)v=i<900;o.x=v;}return typeof o.x+':'+o.x}f({x:0},2000)";
        let mut results = Vec::new();
        for enabled in [false, true] {
            let mut context = Context::default();
            context.set_baseline_jit_enabled(enabled);
            let value = context.eval(Source::from_bytes(SOURCE)).unwrap();
            assert_eq!(value, JsValue::from(crate::js_string!("boolean:false")));
            results.push(value.display().to_string());
            let diagnostics = context.arithmetic_jit_diagnostics();
            if enabled {
                assert!(diagnostics.compiled_entries > 0, "{diagnostics:?}");
                assert!(diagnostics.type_deopts > 0, "{diagnostics:?}");
                assert!(diagnostics.property_bailouts > 0, "{diagnostics:?}");
            } else {
                assert_eq!(diagnostics.compiled_entries, 0);
            }
        }
        assert_eq!(results[0], results[1]);
    }

    #[test]
    fn issue305_prop_mono_shape_stays_in_generated_loop_past_i32() {
        let mut context = Context::default();
        let instruction_count = context.vm.instruction_count.clone();
        let result = Script::parse(
            Source::from_bytes(
                "(function(n){var o={a:1,b:2,c:3},s=0;for(var i=0;i<n;i++){o.b=o.a+i;s+=o.b+o.c}return s})(1000000)",
            ),
            None,
            &mut context,
        )
        .unwrap()
        .evaluate(&mut context)
        .unwrap();
        assert_eq!(result.as_number(), Some(500_003_500_000.0));
        let diagnostics = context.arithmetic_jit_diagnostics();
        assert!(
            instruction_count.get() < 2_000,
            "issue #305 property shape stayed interpreted: {diagnostics:?}"
        );
        assert_eq!(diagnostics.successful_compilations, 1);
        assert!(diagnostics.property_guard_hits >= 1);
        assert_eq!(diagnostics.property_bailouts, 0);
    }

    #[test]
    fn shape_transition_invalidates_property_machine_code() {
        let mut context = Context::default();
        let result = Script::parse(
            Source::from_bytes(
                "function f(o,n){let s=0;for(let i=0;i<n;i++){s=s+o.x;o.x=o.x+1}return s+o.x}\
                 let a={x:1}; f(a,200); let b={pad:0,x:10}; f(b,2000)",
            ),
            None,
            &mut context,
        )
        .unwrap()
        .evaluate(&mut context)
        .unwrap();
        assert_eq!(result.as_number(), Some(2_021_010.0));
        let diagnostics = context.arithmetic_jit_diagnostics();
        assert!(diagnostics.property_guard_hits >= 1);
        assert!(diagnostics.property_guard_misses >= 1);
        assert!(diagnostics.shape_deopts >= 1);
        assert!(diagnostics.compile_rejections >= 1);
    }

    #[test]
    fn delete_redefine_and_accessor_objects_never_reuse_stale_property_code() {
        let mut context = Context::default();
        let result = Script::parse(
            Source::from_bytes(
                "function f(o,n){let s=0;for(let i=0;i<n;i++)s=s+o.x;return s}\
                 let a={x:2}; f(a,200); delete a.x; Object.defineProperty(a,'x',{value:7});\
                 let r1=f(a,1000); let b={get x(){return 11}}; let r2=f(b,1000); r1+r2",
            ),
            None,
            &mut context,
        )
        .unwrap()
        .evaluate(&mut context)
        .unwrap();
        assert_eq!(result.as_number(), Some(18_000.0));
        let diagnostics = context.arithmetic_jit_diagnostics();
        assert!(diagnostics.property_guard_misses >= 1);
        assert!(diagnostics.compile_rejections >= 1);
    }

    #[test]
    fn forged_stale_ic_slot_is_invalidated_before_interpreter_resume() {
        let mut context = Context::default();
        let script = Script::parse(
            Source::from_bytes(
                "var target=Object.create(null);target.x=4;function f(o,n){let s=0;for(let i=0;i<n;i++)s=s+o.x;return s}f(target,200)",
            ),
            None,
            &mut context,
        )
        .unwrap();
        let outer = script.codeblock(&mut context).unwrap();
        let function_index = outer
            .constants
            .iter()
            .position(|constant| matches!(constant, Constant::Function(_)))
            .unwrap();
        let function = outer.constant_function(function_index);
        assert_eq!(
            script.evaluate(&mut context).unwrap().as_number(),
            Some(800.0)
        );
        let ic = function.ic.first().expect("property IC");
        let mut forged = ic.slot();
        forged.index = forged.index.saturating_add(100);
        forged.attributes |= SlotAttributes::PROTOTYPE;
        ic.slot.set(forged);

        let result = Script::parse(Source::from_bytes("f(target,100)"), None, &mut context)
            .unwrap()
            .evaluate(&mut context)
            .unwrap();
        assert_eq!(result.as_number(), Some(400.0));
        assert_eq!(
            ic.slot().index,
            0,
            "generic lookup must repair the stale IC"
        );
        assert!(!ic.slot().attributes.contains(SlotAttributes::PROTOTYPE));
    }

    #[test]
    fn prototype_property_and_prototype_mutation_stay_in_interpreter() {
        let mut context = Context::default();
        let result = Script::parse(
            Source::from_bytes(
                "function f(o,n){let s=0;for(let i=0;i<n;i++)s=s+o.x;return s}\
                 let p={x:3},o=Object.create(p); let a=f(o,200); p.x=5; a+f(o,200)",
            ),
            None,
            &mut context,
        )
        .unwrap()
        .evaluate(&mut context)
        .unwrap();
        assert_eq!(result.as_number(), Some(1_600.0));
        let diagnostics = context.arithmetic_jit_diagnostics();
        assert_eq!(diagnostics.successful_compilations, 0);
        assert!(diagnostics.compile_rejections >= 1);
    }

    #[test]
    fn property_loop_matches_jit_suppressed_execution() {
        const SOURCE: &str = "(function(n){let o={x:3},s=1;for(let i=0;i<n;i++){s=(s+o.x*3)%1000003;o.x=o.x+1}return s+o.x})(2000)";
        let mut interpreted = Context::default();
        interpreted.set_baseline_jit_enabled(false);
        let expected = Script::parse(Source::from_bytes(SOURCE), None, &mut interpreted)
            .unwrap()
            .evaluate(&mut interpreted)
            .unwrap();

        let mut compiled = Context::default();
        let actual = Script::parse(Source::from_bytes(SOURCE), None, &mut compiled)
            .unwrap()
            .evaluate(&mut compiled)
            .unwrap();
        assert_eq!(actual, expected);
        assert!(compiled.arithmetic_jit_diagnostics().property_guard_hits >= 1);
    }

    #[test]
    fn property_write_is_committed_before_exact_arithmetic_bailout() {
        let mut context = Context::default();
        let result = Script::parse(
            Source::from_bytes(
                "function f(o,n,start){let s=start;for(let i=0;i<n;i++){o.x=o.x+1;s=s+o.x}return o.x}\
                 let o={x:0}; f(o,200,0); o.x=0; f(o,100,9007199254740980)",
            ),
            None,
            &mut context,
        )
        .unwrap()
        .evaluate(&mut context)
        .unwrap();
        assert_eq!(result.as_number(), Some(100.0));
        let diagnostics = context.arithmetic_jit_diagnostics();
        assert!(diagnostics.property_bailouts >= 1);
        assert!(diagnostics.arithmetic_deopts >= 1);
    }

    #[test]
    fn budgeted_async_dispatch_yields_without_entering_generated_code() {
        let mut context = Context::default();
        let script = Script::parse(
            Source::from_bytes(
                "(function(n){var s=1;for(var i=0;i<n;i++)s=(s+i*3)%1000003;return s})(2000)",
            ),
            None,
            &mut context,
        )
        .unwrap();
        let mut evaluation = Box::pin(script.evaluate_async_with_budget(&mut context, 1));
        assert!(future::block_on(future::poll_once(evaluation.as_mut())).is_none());
        let result = future::block_on(evaluation).unwrap();
        let expected = (0..2000_i64).fold(1_i64, |sum, i| (sum + i * 3) % 1_000_003);
        assert_eq!(result.as_number(), Some(expected as f64));

        let diagnostics = context.arithmetic_jit_diagnostics();
        assert_eq!(diagnostics.compile_requests, 0);
        assert_eq!(diagnostics.compiled_entries, 0);
    }

    #[test]
    fn budgeted_generated_loops_yield_with_exact_property_state_and_gc_roots() {
        for enabled in [false, true] {
            let mut context = Context::default();
            context.set_baseline_jit_enabled(enabled);
            let script = Script::parse(
                Source::from_bytes("function f(o,n){let s=1;for(let i=0;i<n;i++){o.x=o.x+1;s=(s+i*3)%1000003}return s+o.x} f({x:0},2000)"),
                None, &mut context,
            ).unwrap();
            let mut evaluation = Box::pin(script.evaluate_async_with_budget(&mut context, 1024));
            let mut yields = 0;
            let actual = loop {
                if let Some(result) = future::block_on(future::poll_once(evaluation.as_mut())) {
                    break result.unwrap();
                }
                yields += 1;
                boa_gc::force_minor_collect();
                if yields % 16 == 0 {
                    boa_gc::force_collect();
                }
            };
            drop(evaluation);
            let expected = (0..2000_i64).fold(1_i64, |sum, i| (sum + i * 3) % 1_000_003) + 2000;
            assert_eq!(actual.as_number(), Some(expected as f64));
            assert!(yields > 10);
            let diagnostics = context.arithmetic_jit_diagnostics();
            if enabled {
                assert!(diagnostics.compiled_entries > 10, "{diagnostics:?}");
                assert!(diagnostics.property_guard_hits > 0, "{diagnostics:?}");
                assert!(diagnostics.interrupt_deopts > 10, "{diagnostics:?}");
            } else {
                assert_eq!(diagnostics.compiled_entries, 0);
            }
            assert_eq!(context.vm.arithmetic_jit_budget, None);
        }
    }

    #[test]
    fn failure_snapshot_copies_live_code_and_metadata_without_changing_execution() {
        let mut context = Context::default();
        context.eval(Source::from_bytes("function f(o,n){for(let i=0;i<n;i++)o.x=o.x+1;return o.x}function helper(){return {x:1}}for(let i=0;i<40;i++)helper();f({x:0},200)")).unwrap();
        let before = context.arithmetic_jit_diagnostics();
        let first = context.jit_debug_snapshot();
        assert!(first.contains("tier=arithmetic"));
        assert!(first.contains("tier=runtime-helper"));
        assert!(first.contains("StackMap"));
        assert!(first.contains("DeoptRecipe"));
        assert!(first.contains("bytecode_entry_pc="));
        #[cfg(target_arch = "x86_64")]
        assert!(first.contains("48 83 ec 08 48 8b 07 ff d0"));
        #[cfg(target_arch = "aarch64")]
        assert!(first.contains("fd 7b bf a9 fd 03 00 91"));
        assert_eq!(context.jit_debug_snapshot(), first);
        boa_gc::force_collect();
        assert_eq!(context.arithmetic_jit_diagnostics(), before);
        assert_eq!(context.jit_debug_snapshot(), first);
        drop(context);
        // The artifact owns copied text and remains readable after RX unmapping.
        assert!(first.starts_with("jit-debug-v1"));
    }

    #[test]
    fn async_native_panic_restores_the_shared_budget_and_deadline() {
        let mut context = Context::default();
        context
            .register_global_callable(
                crate::js_string!("panicNative"),
                0,
                crate::NativeFunction::from_copy_closure(|_, _, _| panic!("host panic")),
            )
            .unwrap();
        let script = Script::parse(Source::from_bytes("function f(fail){if(fail)panicNative();return {}}for(let i=0;i<40;i++)f(false);f(true)"), None, &mut context).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            future::block_on(async {
                let mut scope = context
                    .enter_runtime_deadline(Instant::now() + std::time::Duration::from_secs(60));
                script.evaluate_async_with_budget(&mut scope, 1024).await
            })
        }));
        assert!(result.is_err());
        assert_eq!(context.vm.arithmetic_jit_budget, None);
        assert_eq!(context.runtime_limits().deadline(), None);
        assert_eq!(context.jit_exception_diagnostics().active_frames, 0);
        assert_eq!(
            context.eval(Source::from_bytes("6*7")).unwrap().as_number(),
            Some(42.0)
        );
    }

    #[test]
    fn generated_loop_observes_deadline_and_recovers_the_vm() {
        for enabled in [false, true] {
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut context = Context::default();
                context.set_baseline_jit_enabled(enabled);
                context.eval(Source::from_bytes(
                    "function timed(n){let s=1;for(let i=0;i<n;i++)s=(s+i*3)%1000003;return s}timed(200)",
                )).unwrap();
                let before = context.arithmetic_jit_diagnostics().compiled_entries;
                context
                    .runtime_limits_mut()
                    .set_deadline(Some(Instant::now() + std::time::Duration::from_millis(50)));
                let error = context
                    .eval(Source::from_bytes("timed(1000000000000)"))
                    .unwrap_err();
                let native = error.as_native().unwrap();
                assert!(native.is_runtime_limit());
                assert_eq!(native.message(), crate::vm::WALL_CLOCK_TIMEOUT_MESSAGE);
                if enabled {
                    assert!(context.arithmetic_jit_diagnostics().compiled_entries > before);
                }
                assert_eq!(context.jit_exception_diagnostics().active_frames, 0);
                context.runtime_limits_mut().set_deadline(None);
                assert_eq!(
                    context.eval(Source::from_bytes("6*7")).unwrap().as_number(),
                    Some(42.0)
                );
                sender.send(()).unwrap();
            });
            receiver
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("execution must observe its deadline while generated code is running");
        }
    }

    #[test]
    fn compiled_entry_falls_back_for_a_later_non_integer_call() {
        let mut context = Context::default();
        let result = Script::parse(
            Source::from_bytes(
                "function f(n){var s=1;for(var i=0;i<n;i++)s=(s+i*3)%1000003;return s}\
                 f(200); f('40')",
            ),
            None,
            &mut context,
        )
        .unwrap()
        .evaluate(&mut context)
        .unwrap();
        let expected = (0..40_i64).fold(1_i64, |sum, i| (sum + i * 3) % 1_000_003);
        assert_eq!(result.as_number(), Some(expected as f64));
        assert!(context.arithmetic_jit_diagnostics().type_deopts >= 1);
    }

    #[test]
    fn vm_restarts_dispatch_at_the_arithmetic_bailout_pc() {
        let mut context = Context::default();
        let result = Script::parse(
            Source::from_bytes(
                "function f(n,s){for(var i=0;i<n;i++)s=s+i*3;return s}\
                 f(200,1); f(100,2147483640)",
            ),
            None,
            &mut context,
        )
        .unwrap()
        .evaluate(&mut context)
        .unwrap();
        assert_eq!(result.as_number(), Some(2_147_498_490.0));
    }

    #[test]
    fn native_backedge_preserves_the_loop_iteration_limit() {
        let mut context = Context::default();
        context.runtime_limits_mut().set_loop_iteration_limit(100);
        let result = Script::parse(
            Source::from_bytes(
                "(function(n){var s=1;for(var i=0;i<n;i++)s=(s+i*3)%1000003;return s})(200)",
            ),
            None,
            &mut context,
        )
        .unwrap()
        .evaluate(&mut context);
        assert!(result.is_err());
        assert!(context.arithmetic_jit_diagnostics().interrupt_deopts >= 1);
    }

    #[test]
    fn internal_conditional_branch_stays_in_generated_loop() {
        let mut context = Context::default();
        let instruction_count = context.vm.instruction_count.clone();
        let result = Script::parse(
            Source::from_bytes(
                "(function(n){var s=0;for(var i=0;i<n;i++){if(i<50)s=s+2;else s=s-1}return s})(2000)",
            ),
            None,
            &mut context,
        )
        .unwrap()
        .evaluate(&mut context)
        .unwrap();
        assert_eq!(result.as_number(), Some(-1850.0));
        assert!(
            instruction_count.get() < 1_500,
            "branch loop stayed interpreted"
        );
    }

    #[test]
    fn property_and_arithmetic_loops_compile_independently_in_one_function() {
        let mut context = Context::default();
        let result = Script::parse(
            Source::from_bytes(
                "(function(n){var o={x:0};for(var i=0;i<n;i++)o.x=i;\
                 var s=1;for(var j=0;j<n;j++)s=(s+j*3)%1000003;return s+o.x})(2000)",
            ),
            None,
            &mut context,
        )
        .unwrap()
        .evaluate(&mut context)
        .unwrap();
        let arithmetic = (0..2000_i64).fold(1_i64, |sum, i| (sum + i * 3) % 1_000_003);
        assert_eq!(result.as_number(), Some((arithmetic + 1999) as f64));
        let diagnostics = context.arithmetic_jit_diagnostics();
        assert_eq!(diagnostics.compile_rejections, 0);
        assert_eq!(diagnostics.successful_compilations, 2);
        assert!(diagnostics.property_guard_hits >= 1);
        assert!(diagnostics.compiled_entries >= 1);
    }

    #[test]
    fn mixed_jit_interpreter_exception_preserves_handler_finally_rethrow_and_stack() {
        const SOURCE: &str = "let log=[];\
            function leaf(n,start,fail){let s=start;for(let i=0;i<n;i++)s=s+i*3;\
                if(fail)throw new TypeError('boom:'+s);return s}\
            function middle(fail){try{return leaf(100,9007199254740980,fail)}\
                finally{log.push('finally')}}\
            function top(){try{middle(true)}catch(error){log.push(error.name);\
                log.push(error.message);log.push(String(error.stack).includes('leaf'));\
                try{throw error}catch(same){log.push(same===error)}}return log.join('|')}\
            leaf(200,1,false);top()";

        fn run(source: &str, jit_enabled: bool) -> (JsValue, ArithmeticJitDiagnostics) {
            let mut context = Context::default();
            context.set_baseline_jit_enabled(jit_enabled);
            let value = context.eval(Source::from_bytes(source)).unwrap();
            (value, context.arithmetic_jit_diagnostics())
        }

        let (expected, _) = run(SOURCE, false);
        let (actual, diagnostics) = run(SOURCE, true);
        assert_eq!(actual, expected);
        let rendered = actual.display().to_string();
        assert!(rendered.starts_with("\"finally|TypeError|boom:"));
        assert!(rendered.ends_with("|true\""));
        assert!(diagnostics.compiled_entries >= 1);

        let contract = arithmetic_contract(
            "function f(n){try{let s=0;for(let i=0;i<n;i++)s+=i;throw Error(s)}\
             catch(error){return error.message}}f(100)",
        );
        let code = compile_arithmetic(&contract);
        assert!(
            !code
                .frame_descriptor
                .exception_metadata()
                .handlers()
                .is_empty()
        );
        assert!(code.frame_descriptor.safepoints().iter().all(|point| {
            code.frame_descriptor
                .exception_metadata()
                .source_location(point.bytecode_offset)
                .is_some()
        }));
        let handler = code.frame_descriptor.exception_metadata().handlers()[0];
        let safepoint = code
            .frame_descriptor
            .safepoints()
            .iter()
            .find(|point| handler.contains(point.bytecode_offset))
            .expect("compiled protected loop has an exact exception safepoint");
        let code_start = code.memory.as_ptr() as usize;
        let descriptor = std::sync::Arc::new(code.frame_descriptor.clone());
        let mut pc_table = crate::jit::JitPcTable::default();
        pc_table
            .install(code_start, std::sync::Arc::clone(&descriptor))
            .unwrap();
        let mut chain = crate::jit::JitFrameChain::default();
        chain
            .push(crate::jit::ActiveJitFrame {
                header: JitFrameHeader {
                    frame_id: 99,
                    descriptor_id: descriptor.id(),
                    caller: FrameCaller::Interpreter { frame_depth: 2 },
                },
                safepoint_pc: code_start + safepoint.machine_offset as usize,
            })
            .unwrap();
        let plan = crate::jit::JitExceptionUnwindPlan::build(&chain, &pc_table).unwrap();
        assert_eq!(
            plan.target(),
            crate::jit::JitExceptionUnwindTarget::Handler {
                frame_id: 99,
                bytecode_offset: handler.handler,
                environment_count: handler.environment_count,
            }
        );
        assert!(plan.popped_frame_ids().is_empty());
    }
}
