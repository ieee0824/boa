//! Bounded x86-64 integer loop emitter used by the first arithmetic baseline tier.
//!
//! Values cross this boundary as checked `i32`s in an engine-owned scratch frame.
//! Generated code cannot allocate or retain GC edges. Any operation whose exact
//! ECMAScript Number result is not representable by this tier exits before that
//! bytecode and lets the interpreter perform the operation.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use crate::{
    JsValue,
    vm::{BYTECODE_CONTRACT_VERSION, BytecodeContractSnapshot, Vm},
};

use super::{BytecodeCodeMap, ExecutableMemory, JitError, WritableMemory};

#[repr(C)]
struct NativeFrame {
    registers: *mut i64,
    dirty: *mut u8,
    loop_iterations: u64,
    loop_limit: u64,
    pc: u32,
    status: u32,
}

#[derive(Debug)]
pub(crate) struct ArithmeticCode {
    memory: ExecutableMemory,
    resumed_entry_offset: u32,
    bytecode_resume: u32,
    required: Box<[u32]>,
    pub(crate) code_map: BytecodeCodeMap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArithmeticExit {
    Completed(u32),
    Bailout(u32),
}

impl ArithmeticCode {
    pub(crate) fn compile(
        snapshot: &BytecodeContractSnapshot,
        bytecode_resume: u32,
    ) -> Result<Option<Self>, JitError> {
        #[cfg(not(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos"))))]
        {
            let _ = snapshot;
            return Ok(None);
        }
        #[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
        {
            let Some(region) = LoopRegion::find(snapshot, bytecode_resume) else {
                return Ok(None);
            };
            let mut assembler = Assembler::default();
            let resumed_entry_offset = assembler.position();
            // r10 permanently holds the checked integer register-file pointer.
            assembler.bytes(&[0x4c, 0x8b, 0x17]); // mov r10, [rdi]
            assembler.bytes(&[0x4c, 0x8b, 0x5f, 0x08]); // mov r11, [rdi + 8]
            assembler.jump(
                &[0xe9],
                Label::Bytecode(snapshot.instructions[region.first].next_offset),
            );
            let mut code_map = BytecodeCodeMap::default();
            let mut bailouts = BTreeSet::new();

            for instruction in &snapshot.instructions[region.first..region.end] {
                code_map
                    .push(instruction.offset, assembler.position())
                    .expect("verified instructions and monotonic emission");
                assembler.bind(Label::Bytecode(instruction.offset));
                emit_instruction(&mut assembler, instruction, &mut bailouts)?;
            }
            assembler.bind(Label::Bytecode(region.exit));
            emit_exit(&mut assembler, region.exit, 0);
            for pc in bailouts {
                assembler.bind(Label::Bailout(pc));
                emit_exit(&mut assembler, pc, 1);
            }
            assembler.resolve()?;
            let mut writable = WritableMemory::allocate(assembler.code.len())?;
            writable.write(0, &assembler.code)?;
            let memory = writable.publish()?;
            Ok(Some(Self {
                memory,
                resumed_entry_offset,
                bytecode_resume: snapshot.instructions[region.first].next_offset,
                required: region.required.into_boxed_slice(),
                code_map,
            }))
        }
    }

    fn execute_after_increment(
        &self,
        values: &mut [Option<i32>],
        loop_iterations: &mut u64,
        loop_limit: u64,
    ) -> Option<ArithmeticExit> {
        self.execute_at(
            self.resumed_entry_offset,
            values,
            loop_iterations,
            loop_limit,
        )
    }

    fn execute_at(
        &self,
        machine_offset: u32,
        values: &mut [Option<i32>],
        loop_iterations: &mut u64,
        loop_limit: u64,
    ) -> Option<ArithmeticExit> {
        if self.required.iter().any(|&r| values[r as usize].is_none()) {
            return None;
        }
        let mut registers = values
            .iter()
            .map(|value| i64::from(value.unwrap_or_default()))
            .collect::<Vec<_>>();
        let mut dirty = vec![0_u8; values.len()];
        let mut frame = NativeFrame {
            registers: registers.as_mut_ptr(),
            dirty: dirty.as_mut_ptr(),
            loop_iterations: *loop_iterations,
            loop_limit,
            pc: 0,
            status: 0,
        };
        // SAFETY: the emitter validates every register and branch, generated code
        // only accesses this frame and its fixed-size register allocation, and the
        // RX mapping remains owned for the duration of the call.
        let entry: unsafe extern "C" fn(*mut NativeFrame) =
            unsafe { std::mem::transmute(self.memory.as_ptr().add(machine_offset as usize)) };
        unsafe { entry(&raw mut frame) };
        *loop_iterations = frame.loop_iterations;
        for (register, &was_written) in dirty.iter().enumerate() {
            if was_written != 0 {
                values[register] = i32::try_from(registers[register]).ok();
            }
        }
        Some(if frame.status == 0 {
            ArithmeticExit::Completed(frame.pc)
        } else {
            ArithmeticExit::Bailout(frame.pc)
        })
    }
}

#[derive(Debug, Default)]
pub(crate) struct ArithmeticRuntime {
    entries: HashMap<(u64, u32), RuntimeEntry>,
    diagnostics: ArithmeticJitDiagnostics,
}

/// Runtime-wide counters for the arithmetic baseline tier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArithmeticJitDiagnostics {
    /// Hot loops submitted to the emitter.
    pub compile_requests: u64,
    /// Requests that installed generated code.
    pub successful_compilations: u64,
    /// Requests rejected as unsupported or invalid.
    pub compile_rejections: u64,
    /// Calls that entered generated machine code.
    pub compiled_entries: u64,
    /// Calls that resumed the interpreter.
    pub bailouts: u64,
}

#[derive(Debug)]
enum RuntimeEntry {
    Warming(u32),
    Compiled(ArithmeticCode),
    Unsupported,
}

impl ArithmeticRuntime {
    const HOT_LOOP_THRESHOLD: u32 = 32;

    pub(crate) const fn diagnostics(&self) -> ArithmeticJitDiagnostics {
        self.diagnostics
    }

    /// Observes a loop header and, once hot, replaces repeated bytecode dispatch
    /// with one bounded generated-code call.
    pub(crate) fn try_execute_after_increment(&mut self, vm: &mut Vm) -> bool {
        let pc = vm.frame.pc;
        let key = (vm.frame.code_block.jit_code_id, pc);
        let entry = self.entries.entry(key).or_insert(RuntimeEntry::Warming(0));
        match entry {
            RuntimeEntry::Warming(count) => {
                *count = count.saturating_add(1);
                if *count < Self::HOT_LOOP_THRESHOLD {
                    return false;
                }
                vm.frame.code_block.jit_metadata.mark_queued();
                self.diagnostics.compile_requests =
                    self.diagnostics.compile_requests.saturating_add(1);
                let snapshot = vm.frame.code_block.bytecode_contract().verify().ok();
                let single_loop = snapshot.as_ref().is_some_and(|snapshot| {
                    snapshot
                        .instructions
                        .iter()
                        .filter(|instruction| instruction.name == "IncrementLoopIteration")
                        .count()
                        == 1
                });
                let compiled = snapshot
                    .as_ref()
                    .and_then(|snapshot| ArithmeticCode::compile(snapshot, pc).ok().flatten());
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
        let register_count = vm.frame.code_block.register_count as usize;
        let mut values = (0..register_count)
            .map(|index| {
                let value = vm.get_register(index);
                let number = value.as_number()?;
                if number == 0.0 && number.is_sign_negative() {
                    return None;
                }
                value.as_i32()
            })
            .collect::<Vec<_>>();
        let Some(exit) = code.execute_after_increment(
            &mut values,
            &mut vm.frame.loop_iteration_count,
            vm.runtime_limits.loop_iteration_limit(),
        ) else {
            vm.frame.code_block.jit_metadata.record_reusable_fallback();
            self.diagnostics.bailouts = self.diagnostics.bailouts.saturating_add(1);
            return false;
        };
        vm.frame.code_block.jit_metadata.record_compiled_entry();
        self.diagnostics.compiled_entries = self.diagnostics.compiled_entries.saturating_add(1);
        for (index, value) in values.into_iter().enumerate() {
            if let Some(value) = value {
                vm.set_register(index, JsValue::from(value));
            }
        }
        match exit {
            ArithmeticExit::Completed(pc) => {
                vm.frame.pc = pc;
                true
            }
            ArithmeticExit::Bailout(pc) => {
                vm.frame.pc = pc;
                vm.frame.code_block.jit_metadata.record_reusable_fallback();
                self.diagnostics.bailouts = self.diagnostics.bailouts.saturating_add(1);
                true
            }
        }
    }
}

struct LoopRegion {
    first: usize,
    end: usize,
    exit: u32,
    required: Vec<u32>,
}

impl LoopRegion {
    fn find(snapshot: &BytecodeContractSnapshot, bytecode_resume: u32) -> Option<Self> {
        if !snapshot.handlers.is_empty() {
            return None;
        }
        let first = snapshot.instructions.iter().position(|instruction| {
            instruction.name == "IncrementLoopIteration"
                && instruction.next_offset == bytecode_resume
        })?;
        let entry = snapshot.instructions[first].offset;
        let backedge = snapshot.instructions[first..]
            .iter()
            .position(|i| i.name == "Jump" && unsigned(i, "address") == Some(u64::from(entry)))?
            + first;
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
            "Sub",
            "Mul",
            "Mod",
            "LessThan",
            "LessThanOrEq",
            "GreaterThan",
            "GreaterThanOrEq",
            "StrictEq",
            "StrictNotEq",
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
    Bailout(u32),
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
fn store_rax(a: &mut Assembler, reg: u32) {
    a.bytes(&[0x49, 0x89, 0x82]);
    a.u32(reg * 8);
    a.bytes(&[0x41, 0xc6, 0x83]);
    a.u32(reg);
    a.bytes(&[0x01]);
}
fn immediate(a: &mut Assembler, value: i64) {
    a.bytes(&[0x48, 0xb8]);
    a.bytes(&value.to_le_bytes());
}
fn bailout(a: &mut Assembler, opcode: &[u8], pc: u32, set: &mut BTreeSet<u32>) {
    set.insert(pc);
    a.jump(opcode, Label::Bailout(pc));
}

fn emit_instruction(
    a: &mut Assembler,
    i: &crate::vm::BytecodeInstruction,
    bailouts: &mut BTreeSet<u32>,
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
            bailout(a, &[0x0f, 0x87], i.offset, bailouts);
            a.bytes(&[0x48, 0x83, 0x47, 0x10, 0x01]);
        }
        "Move" => {
            load(a, src("src")?, false);
            store_rax(a, dst()?);
        }
        "PushZero" => {
            immediate(a, 0);
            store_rax(a, dst()?);
        }
        "PushOne" => {
            immediate(a, 1);
            store_rax(a, dst()?);
        }
        "PushInt8" | "PushInt16" | "PushInt32" => {
            immediate(a, signed(i, "value").ok_or(JitError::InvalidCodeSize)?);
            store_rax(a, dst()?);
        }
        "Inc" => {
            load(a, src("src")?, false);
            a.bytes(&[0x83, 0xc0, 0x01]);
            bailout(a, &[0x0f, 0x80], i.offset, bailouts);
            a.bytes(&[0x48, 0x63, 0xc0]);
            store_rax(a, dst()?);
        }
        "Add" | "Sub" | "Mul" => {
            load(a, src("lhs")?, false);
            load(a, src("rhs")?, true);
            a.bytes(match i.name {
                "Add" => &[0x01, 0xc8][..],
                "Sub" => &[0x29, 0xc8][..],
                _ => &[0x0f, 0xaf, 0xc1][..],
            });
            bailout(a, &[0x0f, 0x80], i.offset, bailouts);
            a.bytes(&[0x48, 0x63, 0xc0]);
            store_rax(a, dst()?);
        }
        "Mod" => {
            load(a, src("lhs")?, false);
            load(a, src("rhs")?, true);
            a.bytes(&[0x85, 0xc9]);
            bailout(a, &[0x0f, 0x84], i.offset, bailouts);
            // Preserve lhs in r8d; idiv's INT_MIN/-1 trap must bail out.
            a.bytes(&[0x41, 0x89, 0xc0, 0x3d, 0x00, 0x00, 0x00, 0x80]);
            let safe = Label::Internal(i.offset, 0);
            a.jump(&[0x0f, 0x85], safe);
            a.bytes(&[0x83, 0xf9, 0xff]);
            bailout(a, &[0x0f, 0x84], i.offset, bailouts);
            a.bind(safe);
            a.bytes(&[0x99, 0xf7, 0xf9]); // cdq; idiv ecx
            // A negative zero remainder needs the interpreter's f64 representation.
            a.bytes(&[0x85, 0xd2]);
            let nonzero = Label::Internal(i.offset, 1);
            a.jump(&[0x0f, 0x85], nonzero);
            a.bytes(&[0x45, 0x85, 0xc0]);
            bailout(a, &[0x0f, 0x88], i.offset, bailouts);
            a.bind(nonzero);
            a.bytes(&[0x48, 0x63, 0xc2]);
            store_rax(a, dst()?);
        }
        "LessThan" | "LessThanOrEq" | "GreaterThan" | "GreaterThanOrEq" | "StrictEq"
        | "StrictNotEq" => {
            load(a, src("lhs")?, false);
            load(a, src("rhs")?, true);
            a.bytes(&[0x39, 0xc8]);
            let cc = match i.name {
                "LessThan" => 0x9c,
                "LessThanOrEq" => 0x9e,
                "GreaterThan" => 0x9f,
                "GreaterThanOrEq" => 0x9d,
                "StrictEq" => 0x94,
                _ => 0x95,
            };
            a.bytes(&[0x0f, cc, 0xc0, 0x48, 0x0f, 0xb6, 0xc0]);
            store_rax(a, dst()?);
        }
        "Jump" => a.jump(&[0xe9], Label::Bytecode(src("address")?)),
        "JumpIfTrue" | "JumpIfFalse" => {
            load(a, src("value")?, false);
            a.bytes(&[0x48, 0x85, 0xc0]);
            let op = if i.name == "JumpIfTrue" { 0x85 } else { 0x84 };
            a.jump(&[0x0f, op], Label::Bytecode(src("address")?));
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
        ArithmeticCode::compile(contract, resume).unwrap().unwrap()
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
        let mut values = vec![Some(i32::MAX), Some(3), Some(0), None, None, None];
        let mut iterations = 1;
        let exit = code
            .execute_after_increment(&mut values, &mut iterations, u64::MAX)
            .unwrap();
        assert!(matches!(exit, ArithmeticExit::Bailout(_)));

        let mut values = vec![Some(-1_000_006), Some(2), Some(0), None, None, None];
        let mut iterations = 1;
        assert!(matches!(
            code.execute_after_increment(&mut values, &mut iterations, u64::MAX),
            Some(ArithmeticExit::Bailout(_))
        ));
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
        assert!(runtime_diagnostics.compiled_entries >= 1);
        let diagnostics = function.jit_metadata();
        assert_eq!(diagnostics.state, JitCompilationState::Compiled);
        assert_eq!(diagnostics.compile_requests, 1);
        assert!(diagnostics.compiled_entries >= 1);
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
    fn unsupported_loop_does_not_block_a_supported_loop_in_the_same_function() {
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
        assert_eq!(diagnostics.compile_rejections, 1);
        assert_eq!(diagnostics.successful_compilations, 1);
        assert!(diagnostics.compiled_entries >= 1);
    }
}
