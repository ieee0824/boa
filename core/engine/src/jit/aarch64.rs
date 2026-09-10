//! Fixed-width A64 emission. Offsets are bytes relative to the instruction PC.

use std::fmt::Write as _;

use super::JitError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Label(usize);

#[derive(Debug, Clone, Copy)]
enum Relocation {
    Branch,
    Conditional,
    Literal,
}

impl Relocation {
    const fn bits(self) -> u32 {
        match self {
            Self::Branch => 26,
            Self::Conditional | Self::Literal => 19,
        }
    }

    const fn shift(self) -> u32 {
        match self {
            Self::Branch => 0,
            Self::Conditional | Self::Literal => 5,
        }
    }
}

#[derive(Debug)]
struct Fixup {
    instruction: usize,
    target: Label,
    kind: Relocation,
}

#[derive(Debug, Default)]
pub(super) struct Assembler {
    words: Vec<u32>,
    labels: Vec<Option<usize>>,
    fixups: Vec<Fixup>,
    literals: Vec<(u64, Label)>,
}

impl Assembler {
    pub(super) fn position(&self) -> usize {
        self.words.len() * 4
    }

    pub(super) fn label(&mut self) -> Label {
        let label = Label(self.labels.len());
        self.labels.push(None);
        label
    }

    pub(super) fn bind(&mut self, label: Label) -> Result<(), JitError> {
        let position = self.position();
        let target = self
            .labels
            .get_mut(label.0)
            .ok_or(JitError::InvalidAssembly("unknown A64 label"))?;
        if target.is_some() {
            return Err(JitError::InvalidAssembly("A64 label bound twice"));
        }
        *target = Some(position);
        Ok(())
    }

    pub(super) fn instruction(&mut self, word: u32) {
        self.words.push(word);
    }

    fn relocate(&mut self, instruction: u32, target: Label, kind: Relocation) {
        self.fixups.push(Fixup {
            instruction: self.words.len(),
            target,
            kind,
        });
        self.instruction(instruction);
    }

    pub(super) fn branch(&mut self, target: Label) {
        self.relocate(0x1400_0000, target, Relocation::Branch);
    }

    pub(super) fn branch_equal(&mut self, target: Label) {
        self.branch_condition(target, 0);
    }

    pub(super) fn branch_condition(&mut self, target: Label, condition: u8) {
        assert!(
            condition < 14,
            "A64 conditional branch requires a real condition"
        );
        self.relocate(
            0x5400_0000 | u32::from(condition),
            target,
            Relocation::Conditional,
        );
    }

    pub(super) fn literal_u64(&mut self, register: u8, value: u64) -> Result<(), JitError> {
        check_register(register)?;
        let target = if let Some((_, target)) = self.literals.iter().find(|(v, _)| *v == value) {
            *target
        } else {
            let target = self.label();
            self.literals.push((value, target));
            target
        };
        self.relocate(
            0x5800_0000 | u32::from(register),
            target,
            Relocation::Literal,
        );
        Ok(())
    }

    pub(super) fn load_u64(&mut self, destination: u8, base: u8) -> Result<(), JitError> {
        check_register(destination)?;
        check_register(base)?;
        self.instruction(0xf940_0000 | (u32::from(base) << 5) | u32::from(destination));
        Ok(())
    }

    pub(super) fn prologue(&mut self) {
        self.instruction(0xa9bf_7bfd); // stp x29, x30, [sp, #-16]!
        self.instruction(0x9100_03fd); // mov x29, sp
    }

    /// Returns the exact return-PC offset used by the common safepoint table.
    pub(super) fn call_register(&mut self, register: u8) -> Result<u32, JitError> {
        check_register(register)?;
        self.instruction(0xd63f_0000 | (u32::from(register) << 5));
        u32::try_from(self.position()).map_err(|_| JitError::InvalidCodeSize)
    }

    pub(super) fn epilogue(&mut self) {
        self.instruction(0xa8c1_7bfd); // ldp x29, x30, [sp], #16
        self.ret();
    }

    pub(super) fn ret(&mut self) {
        self.instruction(0xd65f_03c0);
    }

    /// Appends an aligned, deduplicated literal pool after the caller's final
    /// return/branch. Generated control flow must never fall through into it.
    pub(super) fn finish(mut self) -> Result<Vec<u8>, JitError> {
        if !self.literals.is_empty() && !self.position().is_multiple_of(8) {
            self.instruction(0xd503_201f); // nop: align the u64 pool
        }
        for (value, label) in std::mem::take(&mut self.literals) {
            self.bind(label)?;
            let bytes = value.to_le_bytes();
            self.instruction(u32::from_le_bytes(
                bytes[..4].try_into().expect("four bytes"),
            ));
            self.instruction(u32::from_le_bytes(
                bytes[4..].try_into().expect("four bytes"),
            ));
        }
        for fixup in &self.fixups {
            let target = self
                .labels
                .get(fixup.target.0)
                .copied()
                .flatten()
                .ok_or(JitError::InvalidAssembly("unbound A64 label"))?;
            let target = i64::try_from(target).map_err(|_| JitError::InvalidCodeSize)?;
            let pc = i64::try_from(fixup.instruction * 4).map_err(|_| JitError::InvalidCodeSize)?;
            self.words[fixup.instruction] |=
                displacement(target - pc, fixup.kind.bits())? << fixup.kind.shift();
        }
        Ok(self.words.into_iter().flat_map(u32::to_le_bytes).collect())
    }
}

fn check_register(register: u8) -> Result<(), JitError> {
    // x18 is platform-reserved on Apple. Keeping it reserved on Linux too makes
    // the same emitted code follow both ABIs. Register 31 is SP/ZR, not a GPR.
    if register > 30 || register == 18 {
        return Err(JitError::InvalidAssembly(
            "invalid or reserved A64 register",
        ));
    }
    Ok(())
}

fn displacement(bytes: i64, bits: u32) -> Result<u32, JitError> {
    if bytes % 4 != 0 {
        return Err(JitError::InvalidAssembly("unaligned A64 branch or literal"));
    }
    let words = bytes / 4;
    let bound = 1_i64 << (bits - 1);
    if !(-bound..bound).contains(&words) {
        return Err(JitError::InvalidAssembly(
            "A64 branch or literal out of range",
        ));
    }
    u32::try_from(words & ((1_i64 << bits) - 1)).map_err(|_| JitError::InvalidCodeSize)
}

pub(super) fn fixed_return(value: u64) -> Result<Vec<u8>, JitError> {
    let mut assembler = Assembler::default();
    assembler.literal_u64(0, value)?;
    assembler.ret();
    assembler.finish()
}

pub(super) fn runtime_call() -> Result<(Vec<u8>, u32), JitError> {
    let mut assembler = Assembler::default();
    assembler.prologue();
    assembler.load_u64(16, 0)?;
    let return_pc = assembler.call_register(16)?;
    assembler.epilogue();
    Ok((assembler.finish()?, return_pc))
}

/// Decodes the foundation emitter's instruction subset and referenced u64 pool.
pub(super) fn disassemble(bytes: &[u8], output: &mut String) {
    let words: Vec<_> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|word| u32::from_le_bytes(*word))
        .collect();
    let mut literals = std::collections::BTreeSet::new();
    for (index, &word) in words.iter().enumerate() {
        let pc = index * 4;
        if literals.contains(&pc) || (pc >= 4 && literals.contains(&(pc - 4))) {
            continue;
        }
        if word & 0xff00_0000 == 0x5800_0000 {
            let target = index as i64 * 4 + signed_immediate(word, 19, 5) * 4;
            if let Ok(target) = usize::try_from(target)
                && target.is_multiple_of(8)
                && target.checked_add(8).is_some_and(|end| end <= bytes.len())
            {
                literals.insert(target);
            }
        }
    }
    for (index, &word) in words.iter().enumerate() {
        let pc = index * 4;
        if pc >= 4 && literals.contains(&(pc - 4)) {
            continue;
        }
        let instruction = if literals.contains(&pc) {
            let value = u64::from_le_bytes(bytes[pc..pc + 8].try_into().expect("literal bounds"));
            format!(".quad 0x{value:016x}")
        } else {
            match word {
                0xd65f_03c0 => "ret".to_owned(),
                0xd503_201f => "nop".to_owned(),
                0xa9bf_7bfd => "stp x29, x30, [sp, #-16]!".to_owned(),
                0x9100_03fd => "mov x29, sp".to_owned(),
                0xa8c1_7bfd => "ldp x29, x30, [sp], #16".to_owned(),
                _ if word & 0xffff_fc1f == 0xd63f_0000 => format!("blr x{}", (word >> 5) & 31),
                _ if word & 0xff00_0000 == 0x5800_0000 => format!(
                    "ldr x{}, {:#x}",
                    word & 31,
                    pc as i64 + signed_immediate(word, 19, 5) * 4,
                ),
                _ if word & 0xffc0_0000 == 0xf940_0000 => format!(
                    "ldr x{}, [x{}, #{}]",
                    word & 31,
                    (word >> 5) & 31,
                    ((word >> 10) & 4095) * 8,
                ),
                _ if word & 0xfc00_0000 == 0x1400_0000 => {
                    format!("b {:#x}", pc as i64 + signed_immediate(word, 26, 0) * 4)
                }
                _ if word & 0xff00_001f == 0x5400_0000 => {
                    format!("b.eq {:#x}", pc as i64 + signed_immediate(word, 19, 5) * 4)
                }
                _ => format!(".inst 0x{word:08x}"),
            }
        };
        writeln!(output, "{pc:08x}: {instruction}").expect("String formatting cannot fail");
    }
}

fn signed_immediate(word: u32, bits: u32, shift: u32) -> i64 {
    let field = i64::from((word >> shift) & ((1_u32 << bits) - 1));
    (field << (64 - bits)) >> (64 - bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(bytes: &[u8]) -> Vec<u32> {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|word| u32::from_le_bytes(*word))
            .collect()
    }

    #[test]
    fn literal_pool_is_aligned_and_deduplicated() {
        let mut assembler = Assembler::default();
        assembler.literal_u64(0, 0x1234_5678_9abc_def0).unwrap();
        assembler.literal_u64(1, 0x1234_5678_9abc_def0).unwrap();
        assembler.ret();
        let bytes = assembler.finish().unwrap();
        assert_eq!(bytes.len(), 24);
        assert_eq!(
            words(&bytes[..16]),
            [0x5800_0080, 0x5800_0061, 0xd65f_03c0, 0xd503_201f]
        );
        assert_eq!(&bytes[16..], &0x1234_5678_9abc_def0_u64.to_le_bytes());
    }

    #[test]
    fn forward_backward_and_conditional_fixups_use_instruction_pc() {
        let mut assembler = Assembler::default();
        let start = assembler.label();
        let end = assembler.label();
        assembler.bind(start).unwrap();
        assembler.branch(end);
        assembler.branch_equal(end);
        assembler.branch(start);
        assembler.bind(end).unwrap();
        assembler.ret();
        assert_eq!(
            words(&assembler.finish().unwrap()),
            [0x1400_0003, 0x5400_0040, 0x17ff_fffe, 0xd65f_03c0]
        );
    }

    #[test]
    fn invalid_labels_registers_and_displacements_are_errors() {
        let mut assembler = Assembler::default();
        let label = assembler.label();
        assembler.branch(label);
        assert!(assembler.finish().is_err());
        let mut assembler = Assembler::default();
        let label = assembler.label();
        assembler.bind(label).unwrap();
        assert!(assembler.bind(label).is_err());
        assert!(assembler.bind(Label(999)).is_err());
        for register in [18, 31, 255] {
            assert!(assembler.literal_u64(register, 0).is_err());
            assert!(assembler.call_register(register).is_err());
            assert!(assembler.load_u64(0, register).is_err());
        }
        for bits in [19, 26] {
            let bytes = (1_i64 << (bits - 1)) * 4;
            assert!(displacement(-bytes, bits).is_ok());
            assert!(displacement(bytes - 4, bits).is_ok());
            assert!(displacement(-bytes - 4, bits).is_err());
            assert!(displacement(bytes, bits).is_err());
            assert!(displacement(2, bits).is_err());
        }
    }

    #[test]
    fn stubs_encode_the_abi_frame_and_exact_return_pc() {
        let (code, pc) = runtime_call().unwrap();
        assert_eq!(pc, 16);
        assert_eq!(
            words(&code),
            [
                0xa9bf_7bfd,
                0x9100_03fd,
                0xf940_0010,
                0xd63f_0200,
                0xa8c1_7bfd,
                0xd65f_03c0
            ]
        );
        let fixed = fixed_return(u64::MAX).unwrap();
        assert_eq!(words(&fixed[..8]), [0x5800_0040, 0xd65f_03c0]);
        assert_eq!(&fixed[8..], &u64::MAX.to_le_bytes());
        let mut dump = String::new();
        disassemble(&fixed, &mut dump);
        assert!(dump.contains("00000000: ldr x0, 0x8"));
        assert!(dump.contains("00000008: .quad 0xffffffffffffffff"));
        assert!(!dump.contains("0000000c:"));
        dump.clear();
        // These literal bits resemble an LDR targeting offset zero. Data must
        // not introduce a second bogus pool entry over the actual instructions.
        disassemble(&fixed_return(0x58ff_ffc0).unwrap(), &mut dump);
        assert!(dump.contains("00000000: ldr x0, 0x8"));
        assert!(dump.contains("00000008: .quad 0x0000000058ffffc0"));
        dump.clear();
        disassemble(&code, &mut dump);
        assert!(dump.contains("0000000c: blr x16"));
        assert!(dump.contains("00000010: ldp x29, x30, [sp], #16"));
    }
}

#[cfg(all(
    test,
    target_arch = "aarch64",
    any(target_os = "linux", target_os = "macos")
))]
mod native_tests {
    use super::*;
    use crate::jit::platform::{ExecutableMemory, WritableMemory};

    std::arch::global_asm!(
        include_str!("aarch64_abi_probe.S"),
        probe = sym boa_jit_abi_probe,
    );

    unsafe extern "C" {
        fn boa_jit_abi_probe(entry: *const u8, frame: *mut libc::c_void) -> u64;
    }

    #[repr(C)]
    struct ProbeFrame {
        helper: unsafe extern "C" fn(*mut ProbeFrame),
        input: u64,
        output: u64,
        stack_pointer: usize,
    }

    unsafe extern "C" fn helper(frame: *mut ProbeFrame) {
        // SAFETY: the probe provides its live stack frame through x0.
        let frame = unsafe { &mut *frame };
        let stack_pointer: usize;
        // SAFETY: reading sp changes no memory and does not modify the stack.
        unsafe {
            std::arch::asm!("mov {}, sp", out(reg) stack_pointer, options(nomem, nostack));
        }
        frame.stack_pointer = stack_pointer;
        frame.output = frame.input.wrapping_add(1);
    }

    fn publish(bytes: &[u8]) -> ExecutableMemory {
        let mut memory = WritableMemory::allocate(bytes.len()).unwrap();
        memory.write(0, bytes).unwrap();
        memory.publish().unwrap()
    }

    #[test]
    #[allow(clippy::print_stderr)]
    fn fixed_and_runtime_stubs_preserve_registers_and_aligned_stack() {
        let fixed = publish(&fixed_return(42).unwrap());
        // SAFETY: the assembly probe calls this validated zero-argument stub
        // and restores its caller's x19-x29 and d8-d15 before returning.
        assert_eq!(
            unsafe { boa_jit_abi_probe(fixed.as_ptr(), std::ptr::null_mut()) },
            1
        );
        let call = publish(&runtime_call().unwrap().0);
        if std::env::var_os("BOA_JIT_DIAGNOSTICS").is_some() {
            let mut dump = String::new();
            call.write_debug_bytes(&mut dump);
            eprintln!("ARM64 runtime-call ABI probe:\n{dump}");
        }
        let mut frame = ProbeFrame {
            helper,
            input: 41,
            output: 0,
            stack_pointer: 0,
        };
        // SAFETY: the generated stub takes this live repr(C) frame in x0 and
        // invokes its first-field helper using the same calling convention.
        assert_eq!(
            unsafe { boa_jit_abi_probe(call.as_ptr(), (&raw mut frame).cast()) },
            1
        );
        assert_eq!(frame.output, 42);
        assert_ne!(frame.stack_pointer, 0);
        assert_eq!(frame.stack_pointer % 16, 0);
    }

    #[test]
    fn newly_published_instructions_and_literals_are_visible() {
        for value in 0..128_u64 {
            let value = value.wrapping_mul(0x1234_5678_9abc_def1);
            let code = publish(&fixed_return(value).unwrap());
            // SAFETY: fixed_return emits exactly extern C fn() -> u64 and
            // publication flushes instruction caches before exposing its pointer.
            let entry: unsafe extern "C" fn() -> u64 =
                unsafe { std::mem::transmute(code.as_ptr()) };
            assert_eq!(unsafe { entry() }, value);
            // Also vary instruction bits themselves: a literal-only change
            // could pass even if an old instruction-cache line survived reuse.
            let immediate = u32::try_from(value & 0xffff).unwrap();
            let mut assembler = Assembler::default();
            assembler.instruction(0xd280_0000 | (immediate << 5)); // movz x0, #imm16
            assembler.ret();
            let immediate_code = publish(&assembler.finish().unwrap());
            // SAFETY: this second generated body is movz x0, #imm16; ret.
            let entry: unsafe extern "C" fn() -> u64 =
                unsafe { std::mem::transmute(immediate_code.as_ptr()) };
            assert_eq!(unsafe { entry() }, u64::from(immediate));
        }
    }

    #[test]
    fn native_forward_conditional_and_backward_branches_reach_their_labels() {
        let mut assembler = Assembler::default();
        let loop_start = assembler.label();
        let done = assembler.label();
        assembler.instruction(0xd280_0060); // movz x0, #3
        assembler.instruction(0xd280_0001); // movz x1, #0
        assembler.bind(loop_start).unwrap();
        assembler.instruction(0x9100_0421); // add x1, x1, #1
        assembler.instruction(0xf100_0400); // subs x0, x0, #1
        assembler.branch_equal(done);
        assembler.branch(loop_start);
        assembler.bind(done).unwrap();
        assembler.instruction(0xaa01_03e0); // mov x0, x1
        assembler.ret();
        let code = publish(&assembler.finish().unwrap());
        // SAFETY: the finite loop above has no arguments or memory accesses,
        // touches only caller-saved registers and returns its count in x0.
        let entry: unsafe extern "C" fn() -> u64 = unsafe { std::mem::transmute(code.as_ptr()) };
        assert_eq!(unsafe { entry() }, 3);
    }
}
