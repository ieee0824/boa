use crate::{
    Context, JsObject, JsResult,
    object::{internal_methods::InternalMethodPropertyContext, shape::slot::SlotAttributes},
    property::PropertyKey,
    value::PrimitiveLookup,
    vm::opcode::{Operation, VaryingOperand},
};

/// Resolves what a property lookup on a **non-object** receiver should run
/// against, without materializing the receiver's wrapper (see
/// [`crate::JsValue::primitive_property_lookup`]).
///
/// Callers must take the object case themselves, so that the far more common
/// object receiver keeps its original code path and pays nothing for this.
fn primitive_lookup_target(
    receiver: &crate::JsValue,
    key: &PropertyKey,
    context: &mut Context,
) -> JsResult<LookupTarget> {
    Ok(match receiver.primitive_property_lookup(key, context) {
        PrimitiveLookup::Value(value) => LookupTarget::Resolved(value),
        PrimitiveLookup::Prototype(prototype) => LookupTarget::Object(prototype),
        // `null`/`undefined`: let `to_object` raise the TypeError.
        PrimitiveLookup::NotPrimitive => LookupTarget::Object(receiver.to_object(context)?),
    })
}

enum LookupTarget {
    Object(JsObject),
    Resolved(crate::JsValue),
}

/// `GetPropertyByName` implements the Opcode Operation for `Opcode::GetPropertyByName`
///
/// Operation:
///  - Get a property by name from an object an push it on the stack.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GetPropertyByName;

impl GetPropertyByName {
    #[inline(always)]
    pub(crate) fn operation(
        (dst, receiver, value, index): (
            VaryingOperand,
            VaryingOperand,
            VaryingOperand,
            VaryingOperand,
        ),
        context: &mut Context,
    ) -> JsResult<()> {
        let receiver = context.vm.get_register(receiver.into()).clone();
        let value = context.vm.get_register(value.into()).clone();
        // The object case is deliberately first and self-contained: building the
        // `PropertyKey` is only needed to answer a primitive, and the inline
        // cache below usually hits without one.
        let object = if let Some(object) = value.as_object() {
            object.clone()
        } else {
            let key: PropertyKey = context.vm.frame().code_block().ic[usize::from(index)]
                .name
                .clone()
                .into();
            match primitive_lookup_target(&value, &key, context)? {
                LookupTarget::Resolved(value) => {
                    context.vm.set_register(dst.into(), value);
                    return Ok(());
                }
                LookupTarget::Object(object) => object,
            }
        };

        let ic = &context.vm.frame().code_block().ic[usize::from(index)];
        let object_borrowed = object.borrow();
        let shape = object_borrowed.shape_edge();
        if let Some(slot) = ic.match_or_reset(shape) {
            let mut result = if slot.attributes.contains(SlotAttributes::PROTOTYPE) {
                let prototype = shape.prototype().expect("prototype should have value");
                let prototype = prototype.borrow();
                prototype.properties().storage[slot.index as usize].clone()
            } else {
                object_borrowed.properties().storage[slot.index as usize].clone()
            };

            drop(object_borrowed);
            if slot.attributes.has_get() && result.is_object() {
                result = result.as_object().expect("should contain getter").call(
                    &receiver,
                    &[],
                    context,
                )?;
            }
            context.vm.set_register(dst.into(), result);
            return Ok(());
        }

        drop(object_borrowed);

        let key: PropertyKey = ic.name.clone().into();

        let context = &mut InternalMethodPropertyContext::new(context);
        let result = object.__get__(&key, receiver.clone(), context)?;

        // Cache the property.
        let slot = *context.slot();
        if slot.is_cachable() {
            let ic = &context.vm.frame().code_block.ic[usize::from(index)];
            let object_borrowed = object.borrow();
            let shape = object_borrowed.shape_edge();
            ic.set(shape, slot);
        }

        context.vm.set_register(dst.into(), result);
        Ok(())
    }
}

impl Operation for GetPropertyByName {
    const NAME: &'static str = "GetPropertyByName";
    const INSTRUCTION: &'static str = "INST - GetPropertyByName";
    const COST: u8 = 4;
}

/// `GetPropertyByValue` implements the Opcode Operation for `Opcode::GetPropertyByValue`
///
/// Operation:
///  - Get a property by value from an object an push it on the stack.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GetPropertyByValue;

impl GetPropertyByValue {
    #[inline(always)]
    pub(crate) fn operation(
        (dst, key, receiver, object): (
            VaryingOperand,
            VaryingOperand,
            VaryingOperand,
            VaryingOperand,
        ),
        context: &mut Context,
    ) -> JsResult<()> {
        let key = context.vm.get_register(key.into()).clone();
        let object = context.vm.get_register(object.into()).clone();
        let key = key.to_property_key(context)?;
        let object = if let Some(object) = object.as_object() {
            object.clone()
        } else {
            match primitive_lookup_target(&object, &key, context)? {
                LookupTarget::Resolved(value) => {
                    context.vm.set_register(dst.into(), value);
                    return Ok(());
                }
                LookupTarget::Object(object) => object,
            }
        };

        // Fast Path
        if object.is_array()
            && let PropertyKey::Index(index) = &key
        {
            let object_borrowed = object.borrow();
            if let Some(element) = object_borrowed.properties().get_dense_property(index.get()) {
                context.vm.set_register(dst.into(), element);
                return Ok(());
            }
        }

        let receiver = context.vm.get_register(receiver.into());

        // Slow path:
        let result = object.__get__(
            &key,
            receiver.clone(),
            &mut InternalMethodPropertyContext::new(context),
        )?;

        context.vm.set_register(dst.into(), result);
        Ok(())
    }
}

impl Operation for GetPropertyByValue {
    const NAME: &'static str = "GetPropertyByValue";
    const INSTRUCTION: &'static str = "INST - GetPropertyByValue";
    const COST: u8 = 4;
}

/// `GetPropertyByValuePush` implements the Opcode Operation for `Opcode::GetPropertyByValuePush`
///
/// Operation:
///  - Get a property by value from an object an push the key and value on the stack.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GetPropertyByValuePush;

impl GetPropertyByValuePush {
    #[inline(always)]
    pub(crate) fn operation(
        (dst, key, receiver, object): (
            VaryingOperand,
            VaryingOperand,
            VaryingOperand,
            VaryingOperand,
        ),
        context: &mut Context,
    ) -> JsResult<()> {
        let key_value = context.vm.get_register(key.into()).clone();
        let object = context.vm.get_register(object.into()).clone();
        let key_value = key_value.to_property_key(context)?;
        let object = if let Some(object) = object.as_object() {
            object.clone()
        } else {
            match primitive_lookup_target(&object, &key_value, context)? {
                LookupTarget::Resolved(value) => {
                    context.vm.set_register(key.into(), key_value.into());
                    context.vm.set_register(dst.into(), value);
                    return Ok(());
                }
                LookupTarget::Object(object) => object,
            }
        };

        // Fast Path
        if object.is_array()
            && let PropertyKey::Index(index) = &key_value
        {
            let object_borrowed = object.borrow();
            if let Some(element) = object_borrowed.properties().get_dense_property(index.get()) {
                context.vm.set_register(key.into(), key_value.into());
                context.vm.set_register(dst.into(), element);
                return Ok(());
            }
        }

        let receiver = context.vm.get_register(receiver.into());

        // Slow path:
        let result = object.__get__(
            &key_value,
            receiver.clone(),
            &mut InternalMethodPropertyContext::new(context),
        )?;

        context.vm.set_register(key.into(), key_value.into());
        context.vm.set_register(dst.into(), result);
        Ok(())
    }
}

impl Operation for GetPropertyByValuePush {
    const NAME: &'static str = "GetPropertyByValuePush";
    const INSTRUCTION: &'static str = "INST - GetPropertyByValuePush";
    const COST: u8 = 4;
}
