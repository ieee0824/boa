//! AAPCS64 leaf loops. x0 owns the frame pointer, x9/x10 the scratch values/tags,
//! x11/x12 the operands, and x13/x14 temporary values/addresses. Every bytecode
//! result is spilled before the next instruction or native exit. No callee-saved
//! register, x18, native stack slot, or floating-point register is modified.

use std::{
    collections::{BTreeMap, BTreeSet},
    mem::offset_of,
};

use super::{
    DeoptReason, JitError, Label, NativeFrame, PropertyBinding, register_operand, signed, unsigned,
};
use crate::jit::aarch64;

const FRAME: u8 = 0;
const VALUES: u8 = 9;
const TAGS: u8 = 10;
const LEFT: u8 = 11;
const RIGHT: u8 = 12;
const TEMP: u8 = 13;
const ADDRESS: u8 = 14;
const EQ: u8 = 0;
const NE: u8 = 1;
const MI: u8 = 4;
const VS: u8 = 6;
const HI: u8 = 8;
const GE: u8 = 10;
const LT: u8 = 11;
const GT: u8 = 12;
const LE: u8 = 13;

#[derive(Default)]
pub(super) struct Assembler {
    native: aarch64::Assembler,
    labels: BTreeMap<Label, aarch64::Label>,
    pub(super) code: Vec<u8>,
}

impl Assembler {
    pub(super) fn position(&self) -> u32 {
        self.native.position() as u32
    }

    fn label(&mut self, label: Label) -> aarch64::Label {
        *self
            .labels
            .entry(label)
            .or_insert_with(|| self.native.label())
    }

    pub(super) fn bind(&mut self, label: Label) {
        let label = self.label(label);
        self.native
            .bind(label)
            .expect("lowering binds each label once");
    }

    fn jump(&mut self, condition: Option<u8>, label: Label) {
        let label = self.label(label);
        match condition {
            None => self.native.branch(label),
            Some(EQ) => self.native.branch_equal(label),
            Some(condition) => self.native.branch_condition(label, condition),
        }
    }

    pub(super) fn entry(&mut self, resume: u32) {
        self.memory(
            true,
            8,
            VALUES,
            FRAME,
            offset_of!(NativeFrame, registers) as u64,
        );
        self.memory(true, 8, TAGS, FRAME, offset_of!(NativeFrame, dirty) as u64);
        self.jump(None, Label::Bytecode(resume));
    }

    pub(super) fn resolve(&mut self) -> Result<(), JitError> {
        self.code = std::mem::take(&mut self.native).finish()?;
        Ok(())
    }

    fn word(&mut self, instruction: u32) {
        self.native.instruction(instruction);
    }

    fn binary(&mut self, opcode: u32, destination: u8, lhs: u8, rhs: u8) {
        self.word(opcode | (u32::from(rhs) << 16) | (u32::from(lhs) << 5) | u32::from(destination));
    }

    fn immediate(&mut self, register: u8, value: u64) {
        self.word(0xd280_0000 | ((value as u32 & 0xffff) << 5) | u32::from(register));
        for half in 1..4 {
            let bits = ((value >> (16 * half)) & 0xffff) as u32;
            if bits != 0 {
                self.word(0xf280_0000 | (half << 21) | (bits << 5) | u32::from(register));
            }
        }
    }

    fn compare(&mut self, lhs: u8, rhs: u8) {
        self.binary(0xeb00_0000, 31, lhs, rhs); // subs xzr, lhs, rhs
    }

    fn compare_zero(&mut self, register: u8) {
        self.word(0xf100_001f | (u32::from(register) << 5));
    }

    fn memory(&mut self, load: bool, width: u64, register: u8, base: u8, offset: u64) {
        let (base, offset) = if offset.is_multiple_of(width) && offset / width < 4096 {
            (base, offset / width)
        } else {
            // Frame-register indices can exceed the immediate addressing range.
            self.immediate(ADDRESS, offset);
            self.binary(0x8b00_0000, ADDRESS, base, ADDRESS);
            (ADDRESS, 0)
        };
        let opcode = match (load, width) {
            (true, 8) => 0xf940_0000,
            (false, 8) => 0xf900_0000,
            (true, 1) => 0x3940_0000,
            (false, 1) => 0x3900_0000,
            (false, 4) => 0xb900_0000,
            _ => unreachable!("the loop frame only needs i64 values, u8 tags and u32 exits"),
        };
        self.word(opcode | ((offset as u32) << 10) | (u32::from(base) << 5) | u32::from(register));
    }

    fn load(&mut self, register: u32, native: u8) {
        self.memory(true, 8, native, VALUES, u64::from(register) * 8);
    }

    fn store(&mut self, register: u32, tag: u8) {
        self.memory(false, 8, LEFT, VALUES, u64::from(register) * 8);
        self.immediate(TEMP, u64::from(tag));
        self.memory(false, 1, TEMP, TAGS, u64::from(register));
    }

    fn bailout(
        &mut self,
        condition: u8,
        pc: u32,
        reason: DeoptReason,
        bailouts: &mut BTreeSet<(u32, DeoptReason)>,
    ) {
        bailouts.insert((pc, reason));
        self.jump(Some(condition), Label::Bailout(pc, reason));
    }

    fn guard_safe_integer(&mut self, pc: u32, bailouts: &mut BTreeSet<(u32, DeoptReason)>) {
        self.immediate(TEMP, 9_007_199_254_740_991);
        self.compare(LEFT, TEMP);
        self.bailout(GT, pc, DeoptReason::ArithmeticGuard, bailouts);
        self.immediate(TEMP, (-9_007_199_254_740_991_i64) as u64);
        self.compare(LEFT, TEMP);
        self.bailout(LT, pc, DeoptReason::ArithmeticGuard, bailouts);
    }
}

pub(super) fn emit_instruction(
    a: &mut Assembler,
    i: &crate::vm::BytecodeInstruction,
    properties: &[PropertyBinding],
    object_move_offsets: &BTreeMap<u32, u32>,
    bailouts: &mut BTreeSet<(u32, DeoptReason)>,
) -> Result<(), JitError> {
    // Opaque object aliases must return to the VM before scalar operations
    // perform JavaScript coercion or compare object identities.
    if !matches!(i.name, "Move" | "GetPropertyByName" | "SetPropertyByName") {
        for operand in &i.operands {
            if operand.name != "dst"
                && let Some(source) = register_operand(i.name, operand.name, operand.value)
            {
                a.memory(true, 1, TEMP, TAGS, u64::from(source));
                a.word(0xf100_0c1f | (u32::from(TEMP) << 5)); // cmp temp, #3
                a.bailout(EQ, i.offset, DeoptReason::TypeGuard, bailouts);
            }
        }
    }
    let operand = |name| {
        unsigned(i, name)
            .and_then(|v| u32::try_from(v).ok())
            .ok_or(JitError::InvalidCodeSize)
    };
    let guard = DeoptReason::ArithmeticGuard;
    match i.name {
        "IncrementLoopIteration" => {
            let poll = offset_of!(NativeFrame, poll_remaining) as u64;
            a.memory(true, 8, LEFT, FRAME, poll);
            a.compare_zero(LEFT);
            a.bailout(EQ, i.offset, DeoptReason::Interrupt, bailouts);
            a.word(0xd100_0400 | (u32::from(LEFT) << 5) | u32::from(LEFT)); // sub left, left, #1
            a.memory(false, 8, LEFT, FRAME, poll);
            a.memory(
                true,
                8,
                LEFT,
                FRAME,
                offset_of!(NativeFrame, loop_iterations) as u64,
            );
            a.memory(
                true,
                8,
                RIGHT,
                FRAME,
                offset_of!(NativeFrame, loop_limit) as u64,
            );
            a.compare(LEFT, RIGHT);
            a.bailout(HI, i.offset, DeoptReason::Interrupt, bailouts);
            a.word(0x9100_0400 | (u32::from(LEFT) << 5) | u32::from(LEFT)); // add left, left, #1
            a.memory(
                false,
                8,
                LEFT,
                FRAME,
                offset_of!(NativeFrame, loop_iterations) as u64,
            );
        }
        "Move" => {
            // Keep the rooted VM source register as an opaque alias payload so
            // every exit can reconstruct an overwritten temporary correctly.
            if let Some(&source) = object_move_offsets.get(&i.offset) {
                a.immediate(LEFT, u64::from(source));
                a.store(operand("dst")?, 3);
                return Ok(());
            }
            let source = operand("src")?;
            let destination = operand("dst")?;
            a.load(source, LEFT);
            a.memory(true, 1, RIGHT, TAGS, u64::from(source));
            a.compare_zero(RIGHT);
            let typed = Label::Internal(i.offset, 4);
            a.jump(Some(NE), typed);
            a.immediate(RIGHT, 1);
            a.bind(typed);
            a.memory(false, 8, LEFT, VALUES, u64::from(destination) * 8);
            a.memory(false, 1, RIGHT, TAGS, u64::from(destination));
        }
        "PushZero" | "PushOne" | "PushInt8" | "PushInt16" | "PushInt32" => {
            let value = match i.name {
                "PushZero" => 0,
                "PushOne" => 1,
                _ => signed(i, "value").ok_or(JitError::InvalidCodeSize)?,
            };
            a.immediate(LEFT, value as u64);
            a.store(operand("dst")?, 1);
        }
        "Inc" => {
            a.load(operand("src")?, LEFT);
            a.word(0xb100_0400 | (u32::from(LEFT) << 5) | u32::from(LEFT)); // adds left, left, #1
            a.bailout(VS, i.offset, guard, bailouts);
            a.guard_safe_integer(i.offset, bailouts);
            a.store(operand("dst")?, 1);
        }
        "Add" | "AddAssignLocal" | "Sub" | "Mul" => {
            let output = operand(if i.name == "AddAssignLocal" {
                "value"
            } else {
                "dst"
            })?;
            a.load(
                operand(if i.name == "AddAssignLocal" {
                    "value"
                } else {
                    "lhs"
                })?,
                LEFT,
            );
            a.load(operand("rhs")?, RIGHT);
            if i.name == "Mul" {
                let lhs_nonzero = Label::Internal(i.offset, 2);
                let safe = Label::Internal(i.offset, 3);
                a.compare_zero(LEFT);
                a.jump(Some(NE), lhs_nonzero);
                a.compare_zero(RIGHT);
                a.bailout(MI, i.offset, guard, bailouts);
                a.jump(None, safe);
                a.bind(lhs_nonzero);
                a.compare_zero(RIGHT);
                a.jump(Some(NE), safe);
                a.compare_zero(LEFT);
                a.bailout(MI, i.offset, guard, bailouts);
                a.bind(safe);
                a.binary(0x9b40_7c00, TEMP, LEFT, RIGHT); // smulh: signed high half
                a.binary(0x9b00_7c00, LEFT, LEFT, RIGHT); // mul: low half
                a.word(0x937f_fc00 | (u32::from(LEFT) << 5) | u32::from(RIGHT)); // asr right, left, #63
                a.compare(TEMP, RIGHT);
                a.bailout(NE, i.offset, guard, bailouts);
            } else {
                a.binary(
                    if i.name == "Sub" {
                        0xeb00_0000
                    } else {
                        0xab00_0000
                    },
                    LEFT,
                    LEFT,
                    RIGHT,
                );
                a.bailout(VS, i.offset, guard, bailouts);
            }
            a.guard_safe_integer(i.offset, bailouts);
            a.store(output, 1);
        }
        "Mod" => {
            a.load(operand("lhs")?, LEFT);
            a.load(operand("rhs")?, RIGHT);
            a.compare_zero(RIGHT);
            a.bailout(EQ, i.offset, guard, bailouts);
            // Bounded safe-integer inputs exclude i64::MIN / -1.
            a.binary(0x9ac0_0c00, TEMP, LEFT, RIGHT); // sdiv temp, left, right
            a.word(
                0x9b00_8000
                    | (u32::from(RIGHT) << 16)
                    | (u32::from(LEFT) << 10)
                    | (u32::from(TEMP) << 5)
                    | u32::from(TEMP),
            ); // msub temp, temp, right, left
            a.compare_zero(TEMP);
            let nonzero = Label::Internal(i.offset, 1);
            a.jump(Some(NE), nonzero);
            a.compare_zero(LEFT);
            a.bailout(MI, i.offset, guard, bailouts);
            a.bind(nonzero);
            a.binary(0xaa00_0000, LEFT, 31, TEMP); // mov left, temp
            a.store(operand("dst")?, 1);
        }
        "LessThan" | "LessThanOrEq" | "GreaterThan" | "GreaterThanOrEq" | "StrictEq"
        | "StrictNotEq" => {
            if matches!(i.name, "StrictEq" | "StrictNotEq") {
                // A comparison produced inside this loop can be Boolean even
                // though native-entry inputs were guarded as Numbers. Resume
                // at this bytecode with its tags intact instead of equating
                // false/true with numeric zero/one.
                for name in ["lhs", "rhs"] {
                    a.memory(true, 1, TEMP, TAGS, u64::from(operand(name)?));
                    a.word(0xf100_081f | (u32::from(TEMP) << 5)); // cmp temp, #2
                    a.bailout(EQ, i.offset, DeoptReason::TypeGuard, bailouts);
                }
            }
            a.load(operand("lhs")?, LEFT);
            a.load(operand("rhs")?, RIGHT);
            a.compare(LEFT, RIGHT);
            let condition = match i.name {
                "LessThan" => LT,
                "LessThanOrEq" => LE,
                "GreaterThan" => GT,
                "GreaterThanOrEq" => GE,
                "StrictEq" => EQ,
                _ => NE,
            };
            a.word(0x9a9f_07e0 | (u32::from(condition ^ 1) << 12) | u32::from(LEFT)); // cset left, condition
            a.store(operand("dst")?, 2);
        }
        "Jump" => a.jump(None, Label::Bytecode(operand("address")?)),
        "JumpIfTrue" | "JumpIfFalse" => {
            a.load(operand("value")?, LEFT);
            a.compare_zero(LEFT);
            a.jump(
                Some(if i.name == "JumpIfTrue" { NE } else { EQ }),
                Label::Bytecode(operand("address")?),
            );
        }
        "GetPropertyByName" | "SetPropertyByName" => {
            let ic_index = operand("ic_index")?;
            let binding = properties
                .iter()
                .find(|binding| binding.ic_index == ic_index)
                .ok_or(JitError::InvalidCodeSize)?;
            if i.name == "GetPropertyByName" {
                a.load(binding.scratch_register, LEFT);
                a.store(operand("dst")?, 1);
            } else {
                let value = operand("value")?;
                // A Boolean generated inside the loop cannot be committed to
                // an object as a Number. Deopt before this property operation.
                a.memory(true, 1, TEMP, TAGS, u64::from(value));
                a.word(0xf100_081f | (u32::from(TEMP) << 5)); // cmp temp, #2
                a.bailout(GE, i.offset, DeoptReason::TypeGuard, bailouts);
                a.load(value, LEFT);
                a.store(binding.scratch_register, 1);
            }
        }
        _ => return Err(JitError::InvalidCodeSize),
    }
    Ok(())
}

pub(super) fn emit_exit(a: &mut Assembler, pc: u32, status: u32) {
    a.immediate(LEFT, u64::from(pc));
    a.memory(false, 4, LEFT, FRAME, offset_of!(NativeFrame, pc) as u64);
    a.immediate(LEFT, u64::from(status));
    a.memory(
        false,
        4,
        LEFT,
        FRAME,
        offset_of!(NativeFrame, status) as u64,
    );
    a.native.ret();
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::jit::{FrameCaller, JitFrameDescriptorId, JitFrameHeader, WritableMemory};

    #[test]
    fn large_register_indices_spill_values_and_boolean_tags() {
        const SOURCE: u32 = 5000;
        const DESTINATION: u32 = 5001;
        unsafe extern "C" {
            // Defined by the foundation's test-only assembly probe. It checks
            // x19-x29, d8-d15 and native stack alignment around this leaf body.
            fn boa_jit_abi_probe(entry: *const u8, frame: *mut libc::c_void) -> u64;
        }
        let mut a = Assembler::default();
        let entry = a.position();
        a.memory(
            true,
            8,
            VALUES,
            FRAME,
            offset_of!(NativeFrame, registers) as u64,
        );
        a.memory(true, 8, TAGS, FRAME, offset_of!(NativeFrame, dirty) as u64);
        a.load(SOURCE, LEFT);
        a.store(DESTINATION, 2);
        emit_exit(&mut a, 123, 0);
        a.resolve().unwrap();
        let mut memory = WritableMemory::allocate(a.code.len()).unwrap();
        memory.write(0, &a.code).unwrap();
        let memory = memory.publish().unwrap();
        let mut values = vec![0_i64; DESTINATION as usize + 1];
        values[SOURCE as usize] = 1;
        let mut tags = vec![0_u8; values.len()];
        let mut frame = NativeFrame {
            registers: values.as_mut_ptr(),
            dirty: tags.as_mut_ptr(),
            loop_iterations: 0,
            loop_limit: u64::MAX,
            pc: 0,
            status: 0,
            header: JitFrameHeader {
                frame_id: 1,
                descriptor_id: JitFrameDescriptorId(1),
                caller: FrameCaller::Interpreter { frame_depth: 0 },
            },
            poll_remaining: u64::MAX,
        };
        // SAFETY: the validated leaf uses the bounded buffers above and the
        // probe preserves its own caller's registers after checking sentinels.
        assert_eq!(
            unsafe {
                boa_jit_abi_probe(memory.as_ptr().add(entry as usize), (&raw mut frame).cast())
            },
            1
        );
        assert_eq!(values[DESTINATION as usize], 1);
        assert_eq!(tags[DESTINATION as usize], 2);
        assert_eq!(tags[SOURCE as usize], 0);
        assert_eq!(frame.pc, 123);
        assert_eq!(frame.status, 0);
    }
}
