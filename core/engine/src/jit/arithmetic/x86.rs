//! System V AMD64 safe-integer loop emission.

use std::{
    collections::{BTreeMap, BTreeSet},
    mem::offset_of,
};

use super::{
    DeoptReason, JitError, Label, NativeFrame, PropertyBinding, register_operand, signed, unsigned,
};

#[derive(Default)]
pub(super) struct Assembler {
    pub(super) code: Vec<u8>,
    labels: BTreeMap<Label, u32>,
    fixups: Vec<(usize, Label)>,
}
impl Assembler {
    pub(super) fn entry(&mut self, resume: u32) {
        // r10 is the checked integer register file; r11 contains its dirty tags.
        self.bytes(&[0x4c, 0x8b, 0x17]);
        self.bytes(&[0x4c, 0x8b, 0x5f, 0x08]);
        self.jump(&[0xe9], Label::Bytecode(resume));
    }

    pub(super) fn position(&self) -> u32 {
        self.code.len() as u32
    }
    fn bytes(&mut self, bytes: &[u8]) {
        self.code.extend_from_slice(bytes);
    }
    fn u32(&mut self, value: u32) {
        self.bytes(&value.to_le_bytes());
    }
    pub(super) fn bind(&mut self, label: Label) {
        self.labels.insert(label, self.position());
    }
    fn jump(&mut self, opcode: &[u8], label: Label) {
        self.bytes(opcode);
        let at = self.code.len();
        self.u32(0);
        self.fixups.push((at, label));
    }
    pub(super) fn resolve(&mut self) -> Result<(), JitError> {
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

pub(super) fn emit_instruction(
    a: &mut Assembler,
    i: &crate::vm::BytecodeInstruction,
    properties: &[PropertyBinding],
    object_move_offsets: &BTreeMap<u32, u32>,
    bailouts: &mut BTreeSet<(u32, DeoptReason)>,
) -> Result<(), JitError> {
    if !matches!(i.name, "Move" | "GetPropertyByName" | "SetPropertyByName") {
        for operand in &i.operands {
            if operand.name != "dst"
                && let Some(source) = register_operand(i.name, operand.name, operand.value)
            {
                a.bytes(&[0x41, 0x80, 0xbb]); // cmp byte [r11 + source], 3
                a.u32(source);
                a.bytes(&[3]);
                bailout(a, &[0x0f, 0x84], i.offset, DeoptReason::TypeGuard, bailouts);
            }
        }
    }
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
            let poll_offset = u8::try_from(offset_of!(NativeFrame, poll_remaining))
                .map_err(|_| JitError::InvalidCodeSize)?;
            a.bytes(&[0x48, 0x83, 0x7f, poll_offset, 0x00]); // cmp [rdi + poll], 0
            bailout(a, &[0x0f, 0x84], i.offset, DeoptReason::Interrupt, bailouts);
            a.bytes(&[0x48, 0x83, 0x6f, poll_offset, 0x01]); // sub [rdi + poll], 1
            a.bytes(&[0x48, 0x8b, 0x47, 0x10, 0x48, 0x3b, 0x47, 0x18]);
            bailout(a, &[0x0f, 0x87], i.offset, DeoptReason::Interrupt, bailouts);
            a.bytes(&[0x48, 0x83, 0x47, 0x10, 0x01]);
        }
        "Move" => {
            if let Some(&source) = object_move_offsets.get(&i.offset) {
                immediate(a, i64::from(source));
                store_rax(a, dst()?, 3);
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
            if matches!(i.name, "StrictEq" | "StrictNotEq") {
                // Preserve strict type semantics for Booleans produced after
                // the Number guards at native entry.
                for name in ["lhs", "rhs"] {
                    a.bytes(&[0x41, 0x80, 0xbb]); // cmp byte [r11 + register], 2
                    a.u32(src(name)?);
                    a.bytes(&[2]);
                    bailout(a, &[0x0f, 0x84], i.offset, DeoptReason::TypeGuard, bailouts);
                }
            }
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
                a.bytes(&[0x41, 0x80, 0xbb]); // cmp byte [r11 + value], 2
                a.u32(src("value")?);
                a.bytes(&[2]);
                bailout(a, &[0x0f, 0x83], i.offset, DeoptReason::TypeGuard, bailouts);
                load(a, src("value")?, false);
                store_rax(a, binding.scratch_register, 1);
            }
        }
        _ => return Err(JitError::InvalidCodeSize),
    }
    Ok(())
}

pub(super) fn emit_exit(a: &mut Assembler, pc: u32, status: u32) {
    a.bytes(&[0xc7, 0x47, 0x20]);
    a.u32(pc);
    a.bytes(&[0xc7, 0x47, 0x24]);
    a.u32(status);
    a.bytes(&[0xc3]);
}
