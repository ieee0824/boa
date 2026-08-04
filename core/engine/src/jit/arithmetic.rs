//! Bounded x86-64 integer loop emitter used by the first arithmetic baseline tier.
//!
//! Values cross this boundary as checked safe integers in an engine-owned scratch frame.
//! Generated code cannot allocate or retain GC edges. Any operation whose exact
//! ECMAScript Number result is not representable by this tier exits before that
//! bytecode and lets the interpreter perform the operation.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    mem::size_of,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

#[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
use std::mem::offset_of;

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
}

#[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
static_assertions::const_assert!(offset_of!(NativeFrame, registers) == 0x00);
#[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
static_assertions::const_assert!(offset_of!(NativeFrame, dirty) == 0x08);
#[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
static_assertions::const_assert!(offset_of!(NativeFrame, loop_iterations) == 0x10);
#[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
static_assertions::const_assert!(offset_of!(NativeFrame, loop_limit) == 0x18);
#[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
static_assertions::const_assert!(offset_of!(NativeFrame, pc) == 0x20);
#[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
static_assertions::const_assert!(offset_of!(NativeFrame, status) == 0x24);
#[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
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
        #[cfg(not(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos"))))]
        {
            let _ = (snapshot, inline_caches, bytecode_resume);
            return Ok(None);
        }
        #[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
        {
            let Some(mut region) = LoopRegion::find(snapshot, bytecode_resume) else {
                return Ok(None);
            };
            let Some((properties, object_move_offsets)) =
                property_bindings(snapshot, inline_caches, &region)
            else {
                return Ok(None);
            };
            for property in &properties {
                region
                    .required
                    .retain(|register| *register != property.object_register);
            }
            let mut assembler = Assembler::default();
            let resumed_entry_offset = assembler.position();
            // r10 permanently holds the checked integer register-file pointer.
            assembler.bytes(&[0x4c, 0x8b, 0x17]); // mov r10, [rdi]
            assembler.bytes(&[0x4c, 0x8b, 0x5f, 0x08]); // mov r11, [rdi + 8]
            assembler.jump(
                &[0xe9],
                Label::Bytecode(snapshot.instructions[region.increment].next_offset),
            );
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
            let frame_descriptor = JitFrameDescriptor::new(
                JitFrameDescriptorId(NEXT_FRAME_DESCRIPTOR_ID.fetch_add(1, Ordering::Relaxed)),
                u32::try_from(assembler.code.len()).map_err(|_| JitError::InvalidCodeSize)?,
                u32::try_from(size_of::<NativeFrame>()).map_err(|_| JitError::InvalidCodeSize)?,
                snapshot.register_count,
                safepoints,
            )?;
            let valid_program_counters = snapshot
                .instructions
                .iter()
                .map(|instruction| instruction.offset)
                .collect::<Vec<_>>();
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
            Ok(Some(Self {
                memory,
                resumed_entry_offset,
                bytecode_resume: snapshot.instructions[region.increment].next_offset,
                required: region.required.into_boxed_slice(),
                properties: properties.into_boxed_slice(),
                code_map,
                frame_descriptor,
                deopt_recipes,
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
        self.execute_after_increment_typed(values, &mut write_kinds, loop_iterations, loop_limit, 0)
    }

    fn execute_after_increment_typed(
        &self,
        values: &mut [Option<i64>],
        write_kinds: &mut [u8],
        loop_iterations: &mut u64,
        loop_limit: u64,
        interpreter_frame_depth: usize,
    ) -> Option<ArithmeticExit> {
        self.execute_at(
            self.resumed_entry_offset,
            values,
            write_kinds,
            loop_iterations,
            loop_limit,
            interpreter_frame_depth,
        )
    }

    fn execute_at(
        &self,
        machine_offset: u32,
        values: &mut [Option<i64>],
        write_kinds: &mut [u8],
        loop_iterations: &mut u64,
        loop_limit: u64,
        interpreter_frame_depth: usize,
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
            loop_limit,
            pc: 0,
            status: 0,
            header: JitFrameHeader {
                frame_id: NEXT_ACTIVE_FRAME_ID.fetch_add(1, Ordering::Relaxed),
                descriptor_id: self.frame_descriptor.id(),
                caller: FrameCaller::Interpreter {
                    frame_depth: interpreter_frame_depth,
                },
            },
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
    /// Deopts caused by values outside the baseline representation contract.
    pub type_deopts: u64,
    /// Deopts caused by arithmetic overflow, negative zero, or another Number edge case.
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
        let Some(exit) = code.execute_after_increment_typed(
            &mut values,
            &mut write_kinds,
            &mut vm.frame.loop_iteration_count,
            vm.runtime_limits.loop_iteration_limit(),
            interpreter_frame_depth,
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
                for (index, (&value, &write_kind)) in values
                    .iter()
                    .zip(&write_kinds)
                    .take(register_count)
                    .enumerate()
                {
                    match (value, write_kind) {
                        (Some(value), 1) => vm.set_register(index, JsValue::from(value as f64)),
                        (Some(value), 2) => vm.set_register(index, JsValue::from(value != 0)),
                        (_, 0) => {}
                        _ => unreachable!("emitter only writes validated arithmetic value kinds"),
                    }
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

struct LoopRegion {
    first: usize,
    increment: usize,
    end: usize,
    exit: u32,
    required: Vec<u32>,
}

fn property_bindings(
    snapshot: &BytecodeContractSnapshot,
    inline_caches: &[InlineCache],
    region: &LoopRegion,
) -> Option<(Vec<PropertyBinding>, BTreeSet<u32>)> {
    let mut bindings = Vec::<PropertyBinding>::new();
    let mut object_move_offsets = BTreeSet::new();
    let instructions = &snapshot.instructions[region.first..region.end];
    for (instruction_index, instruction) in instructions.iter().enumerate() {
        let (object_operand, is_write) = match instruction.name {
            "GetPropertyByName" => {
                if unsigned(instruction, "receiver")? != unsigned(instruction, "value")? {
                    return None;
                }
                ("value", false)
            }
            "SetPropertyByName" => {
                if unsigned(instruction, "receiver")? != unsigned(instruction, "object")? {
                    return None;
                }
                ("object", true)
            }
            _ => continue,
        };
        let ic_index = u32::try_from(unsigned(instruction, "ic_index")?).ok()?;
        let temporary_object = u32::try_from(unsigned(instruction, object_operand)?).ok()?;
        let object_register = resolve_property_object_register(
            instructions,
            instruction_index,
            temporary_object,
            &mut object_move_offsets,
        )?;
        let (shape, slot) = inline_caches
            .get(ic_index as usize)?
            .monomorphic_own_data_slot()?;
        if is_write && !slot.attributes.contains(SlotAttributes::WRITABLE) {
            return None;
        }
        let key = (object_register, shape, slot.index);
        let scratch_register = if let Some(binding) = bindings
            .iter_mut()
            .find(|binding| (binding.object_register, binding.shape, binding.slot) == key)
        {
            binding.writable |= is_write;
            binding.scratch_register
        } else {
            let unique_slots = bindings
                .iter()
                .map(|binding| binding.scratch_register)
                .collect::<BTreeSet<_>>()
                .len();
            let scratch_register = snapshot
                .register_count
                .checked_add(u32::try_from(unique_slots).ok()?)?;
            bindings.push(PropertyBinding {
                ic_index,
                object_register,
                shape,
                slot: slot.index,
                scratch_register,
                writable: is_write,
            });
            scratch_register
        };
        // Every IC site is independently guarded even when two sites alias the
        // same object slot. Keep a zero-width guard-only binding for that site.
        if !bindings.iter().any(|binding| binding.ic_index == ic_index) {
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
    Some((bindings, object_move_offsets))
}

fn resolve_property_object_register(
    instructions: &[crate::vm::BytecodeInstruction],
    before: usize,
    register: u32,
    object_move_offsets: &mut BTreeSet<u32>,
) -> Option<u32> {
    for (index, instruction) in instructions[..before].iter().enumerate().rev() {
        if unsigned(instruction, "dst").and_then(|value| u32::try_from(value).ok())
            != Some(register)
        {
            continue;
        }
        if instruction.name != "Move" {
            return None;
        }
        object_move_offsets.insert(instruction.offset);
        let source = u32::try_from(unsigned(instruction, "src")?).ok()?;
        return resolve_property_object_register(instructions, index, source, object_move_offsets);
    }
    Some(register)
}

impl LoopRegion {
    fn find(snapshot: &BytecodeContractSnapshot, bytecode_resume: u32) -> Option<Self> {
        if !snapshot.handlers.is_empty() {
            return None;
        }
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
#[derive(Default)]
struct Assembler {
    code: Vec<u8>,
    labels: BTreeMap<Label, u32>,
    fixups: Vec<(usize, Label)>,
}
impl Assembler {
    fn position(&self) -> u32 {
        self.code.len() as u32
    }
    fn bytes(&mut self, bytes: &[u8]) {
        self.code.extend_from_slice(bytes);
    }
    fn u32(&mut self, value: u32) {
        self.bytes(&value.to_le_bytes());
    }
    fn bind(&mut self, label: Label) {
        self.labels.insert(label, self.position());
    }
    fn jump(&mut self, opcode: &[u8], label: Label) {
        self.bytes(opcode);
        let at = self.code.len();
        self.u32(0);
        self.fixups.push((at, label));
    }
    fn resolve(&mut self) -> Result<(), JitError> {
        for (at, label) in &self.fixups {
            let target = i64::from(*self.labels.get(label).ok_or(JitError::InvalidCodeSize)?);
            let after = (*at + 4) as i64;
            let rel = i32::try_from(target - after).map_err(|_| JitError::InvalidCodeSize)?;
            self.code[*at..*at + 4].copy_from_slice(&rel.to_le_bytes());
        }
        Ok(())
    }
}

fn load(a: &mut Assembler, reg: u32, rcx: bool) {
    a.bytes(if rcx {
        &[0x49, 0x8b, 0x8a]
    } else {
        &[0x49, 0x8b, 0x82]
    });
    a.u32(reg * 8);
}
fn store_rax(a: &mut Assembler, reg: u32, kind: u8) {
    a.bytes(&[0x49, 0x89, 0x82]);
    a.u32(reg * 8);
    a.bytes(&[0x41, 0xc6, 0x83]);
    a.u32(reg);
    a.bytes(&[kind]);
}
fn move_rax(a: &mut Assembler, src: u32, dst: u32, pc: u32) {
    // Preserve a comparison's boolean tag when Move forwards it. Untagged
    // native-entry inputs are known numeric values.
    a.bytes(&[0x41, 0x8a, 0x8b]);
    a.u32(src);
    a.bytes(&[0x84, 0xc9]);
    let typed = Label::Internal(pc, 4);
    a.jump(&[0x0f, 0x85], typed);
    a.bytes(&[0xb1, 0x01]);
    a.bind(typed);
    a.bytes(&[0x49, 0x89, 0x82]);
    a.u32(dst * 8);
    a.bytes(&[0x41, 0x88, 0x8b]);
    a.u32(dst);
}
fn immediate(a: &mut Assembler, value: i64) {
    a.bytes(&[0x48, 0xb8]);
    a.bytes(&value.to_le_bytes());
}
fn bailout(
    a: &mut Assembler,
    opcode: &[u8],
    pc: u32,
    reason: DeoptReason,
    set: &mut BTreeSet<(u32, DeoptReason)>,
) {
    set.insert((pc, reason));
    a.jump(opcode, Label::Bailout(pc, reason));
}

fn guard_safe_integer(a: &mut Assembler, pc: u32, bailouts: &mut BTreeSet<(u32, DeoptReason)>) {
    a.bytes(&[0x49, 0x89, 0xc0]); // mov r8, rax
    immediate(a, 9_007_199_254_740_991);
    a.bytes(&[0x49, 0x39, 0xc0]); // cmp r8, rax
    bailout(a, &[0x0f, 0x8f], pc, DeoptReason::ArithmeticGuard, bailouts);
    immediate(a, -9_007_199_254_740_991);
    a.bytes(&[0x49, 0x39, 0xc0]); // cmp r8, rax
    bailout(a, &[0x0f, 0x8c], pc, DeoptReason::ArithmeticGuard, bailouts);
    a.bytes(&[0x4c, 0x89, 0xc0]); // mov rax, r8
}

fn emit_instruction(
    a: &mut Assembler,
    i: &crate::vm::BytecodeInstruction,
    properties: &[PropertyBinding],
    object_move_offsets: &BTreeSet<u32>,
    bailouts: &mut BTreeSet<(u32, DeoptReason)>,
) -> Result<(), JitError> {
    let dst = || {
        unsigned(i, "dst")
            .and_then(|v| u32::try_from(v).ok())
            .ok_or(JitError::InvalidCodeSize)
    };
    let src = |n| {
        unsigned(i, n)
            .and_then(|v| u32::try_from(v).ok())
            .ok_or(JitError::InvalidCodeSize)
    };
    match i.name {
        "IncrementLoopIteration" => {
            a.bytes(&[0x48, 0x8b, 0x47, 0x10, 0x48, 0x3b, 0x47, 0x18]);
            bailout(a, &[0x0f, 0x87], i.offset, DeoptReason::Interrupt, bailouts);
            a.bytes(&[0x48, 0x83, 0x47, 0x10, 0x01]);
        }
        "Move" => {
            if object_move_offsets.contains(&i.offset) {
                return Ok(());
            }
            let source = src("src")?;
            load(a, source, false);
            move_rax(a, source, dst()?, i.offset);
        }
        "PushZero" => {
            immediate(a, 0);
            store_rax(a, dst()?, 1);
        }
        "PushOne" => {
            immediate(a, 1);
            store_rax(a, dst()?, 1);
        }
        "PushInt8" | "PushInt16" | "PushInt32" => {
            immediate(a, signed(i, "value").ok_or(JitError::InvalidCodeSize)?);
            store_rax(a, dst()?, 1);
        }
        "Inc" => {
            load(a, src("src")?, false);
            a.bytes(&[0x48, 0x83, 0xc0, 0x01]);
            bailout(
                a,
                &[0x0f, 0x80],
                i.offset,
                DeoptReason::ArithmeticGuard,
                bailouts,
            );
            guard_safe_integer(a, i.offset, bailouts);
            store_rax(a, dst()?, 1);
        }
        "Add" | "AddAssignLocal" | "Sub" | "Mul" => {
            let output = if i.name == "AddAssignLocal" {
                src("value")?
            } else {
                dst()?
            };
            load(
                a,
                src(if i.name == "AddAssignLocal" {
                    "value"
                } else {
                    "lhs"
                })?,
                false,
            );
            load(a, src("rhs")?, true);
            if i.name == "Mul" {
                // A zero multiplied by a value with the opposite sign is -0,
                // which this safe-integer tier cannot represent.
                a.bytes(&[0x48, 0x85, 0xc0]);
                let lhs_nonzero = Label::Internal(i.offset, 2);
                a.jump(&[0x0f, 0x85], lhs_nonzero);
                a.bytes(&[0x48, 0x85, 0xc9]);
                bailout(
                    a,
                    &[0x0f, 0x88],
                    i.offset,
                    DeoptReason::ArithmeticGuard,
                    bailouts,
                );
                let safe = Label::Internal(i.offset, 3);
                a.jump(&[0xe9], safe);
                a.bind(lhs_nonzero);
                a.bytes(&[0x48, 0x85, 0xc9]);
                a.jump(&[0x0f, 0x85], safe);
                a.bytes(&[0x48, 0x85, 0xc0]);
                bailout(
                    a,
                    &[0x0f, 0x88],
                    i.offset,
                    DeoptReason::ArithmeticGuard,
                    bailouts,
                );
                a.bind(safe);
            }
            a.bytes(match i.name {
                "Add" | "AddAssignLocal" => &[0x48, 0x01, 0xc8][..],
                "Sub" => &[0x48, 0x29, 0xc8][..],
                _ => &[0x48, 0x0f, 0xaf, 0xc1][..],
            });
            bailout(
                a,
                &[0x0f, 0x80],
                i.offset,
                DeoptReason::ArithmeticGuard,
                bailouts,
            );
            guard_safe_integer(a, i.offset, bailouts);
            store_rax(a, output, 1);
        }
        "Mod" => {
            load(a, src("lhs")?, false);
            load(a, src("rhs")?, true);
            a.bytes(&[0x48, 0x85, 0xc9]);
            bailout(
                a,
                &[0x0f, 0x84],
                i.offset,
                DeoptReason::ArithmeticGuard,
                bailouts,
            );
            // Inputs are bounded to safe integers, so signed division cannot
            // encounter the i64::MIN / -1 hardware trap.
            a.bytes(&[0x49, 0x89, 0xc0, 0x48, 0x99, 0x48, 0xf7, 0xf9]);
            // A negative zero remainder needs the interpreter's f64 representation.
            a.bytes(&[0x48, 0x85, 0xd2]);
            let nonzero = Label::Internal(i.offset, 1);
            a.jump(&[0x0f, 0x85], nonzero);
            a.bytes(&[0x4d, 0x85, 0xc0]);
            bailout(
                a,
                &[0x0f, 0x88],
                i.offset,
                DeoptReason::ArithmeticGuard,
                bailouts,
            );
            a.bind(nonzero);
            a.bytes(&[0x48, 0x89, 0xd0]);
            store_rax(a, dst()?, 1);
        }
        "LessThan" | "LessThanOrEq" | "GreaterThan" | "GreaterThanOrEq" | "StrictEq"
        | "StrictNotEq" => {
            load(a, src("lhs")?, false);
            load(a, src("rhs")?, true);
            a.bytes(&[0x48, 0x39, 0xc8]);
            let cc = match i.name {
                "LessThan" => 0x9c,
                "LessThanOrEq" => 0x9e,
                "GreaterThan" => 0x9f,
                "GreaterThanOrEq" => 0x9d,
                "StrictEq" => 0x94,
                _ => 0x95,
            };
            a.bytes(&[0x0f, cc, 0xc0, 0x48, 0x0f, 0xb6, 0xc0]);
            store_rax(a, dst()?, 2);
        }
        "Jump" => a.jump(&[0xe9], Label::Bytecode(src("address")?)),
        "JumpIfTrue" | "JumpIfFalse" => {
            load(a, src("value")?, false);
            a.bytes(&[0x48, 0x85, 0xc0]);
            let op = if i.name == "JumpIfTrue" { 0x85 } else { 0x84 };
            a.jump(&[0x0f, op], Label::Bytecode(src("address")?));
        }
        "GetPropertyByName" | "SetPropertyByName" => {
            let ic_index = src("ic_index")?;
            let binding = properties
                .iter()
                .find(|binding| binding.ic_index == ic_index)
                .ok_or(JitError::InvalidCodeSize)?;
            if i.name == "GetPropertyByName" {
                load(a, binding.scratch_register, false);
                store_rax(a, dst()?, 1);
            } else {
                load(a, src("value")?, false);
                store_rax(a, binding.scratch_register, 1);
            }
        }
        _ => return Err(JitError::InvalidCodeSize),
    }
    Ok(())
}

fn emit_exit(a: &mut Assembler, pc: u32, status: u32) {
    a.bytes(&[0xc7, 0x47, 0x20]);
    a.u32(pc);
    a.bytes(&[0xc7, 0x47, 0x24]);
    a.u32(status);
    a.bytes(&[0xc3]);
}

#[cfg(all(
    test,
    target_arch = "x86_64",
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
        interpreted.vm.arithmetic_jit_suppression_depth = 1;
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
}
