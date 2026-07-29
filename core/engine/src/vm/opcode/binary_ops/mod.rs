use super::VaryingOperand;
use crate::{Context, JsResult, error::JsNativeError, string::JsString, vm::opcode::Operation};

pub(crate) mod logical;
pub(crate) mod macro_defined;

pub(crate) use logical::*;
pub(crate) use macro_defined::*;

/// `AddAssignLocal` implements the Opcode Operation for `Opcode::AddAssignLocal`
///
/// Operation:
///  - Binary `+` operator for `local += rhs`, where `local` is a register-held local.
///
/// Exists for the string case. `s += x` otherwise allocates a fresh string and copies
/// the whole prefix on every iteration, so building a string that way costs time
/// quadratic in its length. Writing the suffix into the string's own spare capacity is
/// amortized-linear instead, but mutating a live string is only sound with an
/// exclusive claim on it.
///
/// Reaching that claim is why this needs its own opcode. The compound assignment
/// compiles to `Move local -> value; <op> value, value, rhs; Move value -> local`, so
/// at the operation the string is held by *both* registers and its reference count is
/// two, never one. `Add` sees only `value` and cannot do anything about the other
/// holder; being given `local` as well, this can drop that reference first.
///
/// Clearing `local` is sound because its contents are dead either way: the compound
/// assignment is about to overwrite the binding. It is deliberately done only after
/// both operands are known to be strings, because concatenating two strings runs no
/// user code — any other combination goes through `ToPrimitive`, which can run a
/// `toString` that reads the binding, and can throw, and in both cases the binding
/// must still hold its value.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AddAssignLocal;

impl AddAssignLocal {
    #[inline(always)]
    pub(crate) fn operation(
        (local, value, rhs): (VaryingOperand, VaryingOperand, VaryingOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        if context.vm.get_register(value.into()).is_string()
            && context.vm.get_register(rhs.into()).is_string()
        {
            let suffix = context
                .vm
                .get_register(rhs.into())
                .as_string()
                .expect("checked that the register holds a string");

            // The binding's own reference. Dropping it is what can bring the count
            // down to one, and its value is dead: either it is the same string that
            // `value` holds, or the right-hand side assigned over the binding, and
            // that assignment is itself about to be overwritten by this one.
            drop(context.vm.take_register(local.into()));

            let taken = context.vm.take_register(value.into());
            let target = taken
                .as_string()
                .expect("checked that the register holds a string");

            // `as_string` cloned out of `taken`, so dropping it leaves `target` as the
            // only holder of a string nothing else was keeping alive.
            drop(taken);

            let appended = target
                .try_append(suffix.as_str())
                .unwrap_or_else(|target| JsString::concat(target.as_str(), suffix.as_str()));

            context.vm.set_register(value.into(), appended.into());
            return Ok(());
        }

        // `local` is deliberately left alone here: `add` can run user code that reads
        // the binding, and can throw, after which the binding must be unchanged.
        let lhs = context.vm.get_register(value.into()).clone();
        let rhs = context.vm.get_register(rhs.into()).clone();
        let sum = lhs.add(&rhs, context)?;
        context.vm.set_register(value.into(), sum);
        Ok(())
    }
}

impl Operation for AddAssignLocal {
    const NAME: &'static str = "AddAssignLocal";
    const INSTRUCTION: &'static str = "INST - AddAssignLocal";
    const COST: u8 = 2;
}

/// `NotEq` implements the Opcode Operation for `Opcode::NotEq`
///
/// Operation:
///  - Binary `!=` operation
#[derive(Debug, Clone, Copy)]
pub(crate) struct NotEq;

impl NotEq {
    #[allow(clippy::needless_pass_by_value)]
    #[inline(always)]
    pub(super) fn operation(
        (dst, lhs, rhs): (VaryingOperand, VaryingOperand, VaryingOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        let lhs = context.vm.get_register(lhs.into()).clone();
        let rhs = context.vm.get_register(rhs.into()).clone();
        let value = !lhs.equals(&rhs, context)?;
        context.vm.set_register(dst.into(), value.into());
        Ok(())
    }
}

impl Operation for NotEq {
    const NAME: &'static str = "NotEq";
    const INSTRUCTION: &'static str = "INST - NotEq";
    const COST: u8 = 2;
}

/// `StrictEq` implements the Opcode Operation for `Opcode::StrictEq`
///
/// Operation:
///  - Binary `===` operation
#[derive(Debug, Clone, Copy)]
pub(crate) struct StrictEq;

impl StrictEq {
    #[inline(always)]
    pub(super) fn operation(
        (dst, lhs, rhs): (VaryingOperand, VaryingOperand, VaryingOperand),
        context: &mut Context,
    ) {
        let lhs = context.vm.get_register(lhs.into());
        let rhs = context.vm.get_register(rhs.into());
        let value = lhs.strict_equals(rhs);
        context.vm.set_register(dst.into(), value.into());
    }
}

impl Operation for StrictEq {
    const NAME: &'static str = "StrictEq";
    const INSTRUCTION: &'static str = "INST - StrictEq";
    const COST: u8 = 2;
}

/// `StrictNotEq` implements the Opcode Operation for `Opcode::StrictNotEq`
///
/// Operation:
///  - Binary `!==` operation
#[derive(Debug, Clone, Copy)]
pub(crate) struct StrictNotEq;

impl StrictNotEq {
    #[inline(always)]
    pub(super) fn operation(
        (dst, lhs, rhs): (VaryingOperand, VaryingOperand, VaryingOperand),
        context: &mut Context,
    ) {
        let lhs = context.vm.get_register(lhs.into());
        let rhs = context.vm.get_register(rhs.into());
        let value = !lhs.strict_equals(rhs);
        context.vm.set_register(dst.into(), value.into());
    }
}

impl Operation for StrictNotEq {
    const NAME: &'static str = "StrictNotEq";
    const INSTRUCTION: &'static str = "INST - StrictNotEq";
    const COST: u8 = 2;
}

/// `In` implements the Opcode Operation for `Opcode::In`
///
/// Operation:
///  - Binary `in` operation
#[derive(Debug, Clone, Copy)]
pub(crate) struct In;

impl In {
    #[inline(always)]
    pub(super) fn operation(
        (dst, lhs, rhs): (VaryingOperand, VaryingOperand, VaryingOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        let rhs = context.vm.get_register(rhs.into()).clone();
        let Some(rhs) = rhs.as_object() else {
            return Err(JsNativeError::typ()
                .with_message(format!(
                    "right-hand side of 'in' should be an object, got `{}`",
                    rhs.type_of()
                ))
                .into());
        };
        let lhs = context.vm.get_register(lhs.into()).clone();
        let key = lhs.to_property_key(context)?;
        let value = rhs.has_property(key, context)?;
        context.vm.set_register(dst.into(), value.into());
        Ok(())
    }
}

impl Operation for In {
    const NAME: &'static str = "In";
    const INSTRUCTION: &'static str = "INST - In";
    const COST: u8 = 3;
}

/// `InPrivate` implements the Opcode Operation for `Opcode::InPrivate`
///
/// Operation:
///  - Binary `in` operation for private names.
#[derive(Debug, Clone, Copy)]
pub(crate) struct InPrivate;

impl InPrivate {
    #[inline(always)]
    pub(super) fn operation(
        (dst, index, rhs): (VaryingOperand, VaryingOperand, VaryingOperand),
        context: &mut Context,
    ) -> JsResult<()> {
        let name = context
            .vm
            .frame()
            .code_block()
            .constant_string(index.into());
        let rhs = context.vm.get_register(rhs.into());

        let Some(rhs) = rhs.as_object() else {
            return Err(JsNativeError::typ()
                .with_message(format!(
                    "right-hand side of 'in' should be an object, got `{}`",
                    rhs.type_of()
                ))
                .into());
        };

        let name = context
            .vm
            .environments
            .resolve_private_identifier(name)
            .expect("private name must be in environment");

        let value = rhs.private_element_find(&name, true, true).is_some();

        context.vm.set_register(dst.into(), value.into());
        Ok(())
    }
}

impl Operation for InPrivate {
    const NAME: &'static str = "InPrivate";
    const INSTRUCTION: &'static str = "INST - InPrivate";
    const COST: u8 = 4;
}
