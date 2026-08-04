//! Verified metadata for reconstructing interpreter state from a JIT exit.

use std::{collections::BTreeSet, error::Error, fmt};

use crate::JsValue;

use super::ValueLocation;

/// Why generated execution returned to the interpreter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
#[repr(u32)]
pub enum DeoptReason {
    /// A property shape or inline-cache identity changed.
    ShapeGuard = 1,
    /// A value no longer has the representation required by generated code.
    TypeGuard = 2,
    /// Integer arithmetic overflowed or produced an unsupported Number value.
    ArithmeticGuard = 3,
    /// Cooperative execution interruption was requested at a safepoint.
    Interrupt = 4,
    /// An exception must resume through interpreter unwinding.
    Exception = 5,
    /// A runtime or test requested interpreter reconstruction explicitly.
    Explicit = 6,
}

impl DeoptReason {
    pub(crate) const fn status(self) -> u32 {
        self as u32
    }

    pub(crate) const fn from_status(status: u32) -> Option<Self> {
        match status {
            1 => Some(Self::ShapeGuard),
            2 => Some(Self::TypeGuard),
            3 => Some(Self::ArithmeticGuard),
            4 => Some(Self::Interrupt),
            5 => Some(Self::Exception),
            6 => Some(Self::Explicit),
            _ => None,
        }
    }
}

/// Whether interpreter dispatch repeats or skips the operation at the recipe PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeoptResumePoint {
    /// Resume before the named bytecode because it has not performed side effects.
    BeforeOperation,
    /// Resume after the named bytecode because its side effects are already committed.
    AfterOperation,
}

/// Conversion from a machine/frame location into an interpreter `JsValue`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeoptValueRepresentation {
    /// The source already contains a tagged JavaScript value.
    Tagged,
    /// Convert a checked signed integer into an ECMAScript Number.
    SafeInteger,
    /// Convert zero/non-zero into an ECMAScript Boolean.
    Boolean,
    /// Read the generated frame's side tag to choose Number or Boolean.
    NativeTagged,
}

/// Raw value read from a generated frame before recipe-directed conversion.
#[derive(Debug, Clone, PartialEq)]
pub enum DeoptSourceValue {
    /// A fully tagged JavaScript value kept live by the JIT stack map.
    Tagged(JsValue),
    /// A checked integer held in a machine register or native stack slot.
    SafeInteger(i64),
    /// A generated zero/non-zero Boolean payload.
    Boolean(bool),
    /// A generated payload accompanied by the arithmetic tier's side tag.
    NativeTagged {
        /// Generated integer payload.
        value: i64,
        /// Whether the payload is a Boolean side tag instead of a Number.
        is_boolean: bool,
    },
    /// The generated operation did not modify this interpreter destination.
    Preserve,
}

/// Failure while applying a verified recipe to a generated-frame snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeoptMaterializationError {
    /// The generated-frame snapshot did not contain a recipe source.
    MissingSource {
        /// Location absent from the generated-frame snapshot.
        source: ValueLocation,
    },
    /// The source payload did not match the representation declared by the recipe.
    RepresentationMismatch {
        /// Location whose payload did not match the recipe.
        source: ValueLocation,
    },
    /// The destination frame does not match the verified recipe layout.
    DestinationOutOfBounds {
        /// Invalid interpreter destination register.
        register: u32,
    },
}

impl fmt::Display for DeoptMaterializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "cannot materialize JIT deopt value: {self:?}")
    }
}

impl Error for DeoptMaterializationError {}

/// One interpreter register reconstructed from generated-frame state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeoptMaterialization {
    /// Interpreter register receiving the reconstructed value.
    pub destination: u32,
    /// Stack-map location containing the live value.
    pub source: ValueLocation,
    /// Representation conversion applied to the source.
    pub representation: DeoptValueRepresentation,
}

/// Environment-stack operation required before interpreter resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeoptEnvironment {
    /// Preserve the environment depth captured at generated entry.
    Preserve,
    /// Truncate to a verified depth relative to the active frame.
    TruncateTo(u32),
}

/// Pending native-call state reconstructed at the deopt boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeoptPendingCall {
    /// No call was started by the operation and existing state is preserved.
    Preserve,
    /// The operation completed its call before the exit; clear its pending slot.
    ClearCompleted,
    /// Resume interpreter call dispatch at the recipe bytecode offset.
    ResumeInterpreter,
}

/// Invalid deoptimization metadata rejected before code installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeoptMetadataError {
    /// The recipe PC is not a verified bytecode boundary.
    InvalidProgramCounter {
        /// Rejected bytecode offset.
        pc: u32,
    },
    /// A destination lies outside the interpreter register file.
    DestinationOutOfBounds {
        /// Rejected interpreter register.
        register: u32,
    },
    /// Two materializations write the same interpreter register.
    DuplicateDestination {
        /// Repeated interpreter register.
        register: u32,
    },
    /// A frame-register source lies outside the generated register file.
    FrameRegisterOutOfBounds {
        /// Rejected generated-frame register.
        register: u32,
    },
    /// A native stack source lies outside the generated frame allocation.
    StackSlotOutOfBounds {
        /// Rejected frame-pointer-relative offset.
        offset: i32,
    },
    /// An architecture register number is outside the portable snapshot table.
    MachineRegisterOutOfBounds {
        /// Rejected architecture register number.
        register: u8,
    },
    /// An environment truncation would remove the active frame's base environment.
    EnvironmentBelowFrameBase {
        /// Active frame's minimum environment depth.
        base: u32,
        /// Rejected truncation depth.
        depth: u32,
    },
}

impl fmt::Display for DeoptMetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid JIT deopt metadata: {self:?}")
    }
}

impl Error for DeoptMetadataError {}

/// Verified interpreter and generated-frame bounds shared by deopt recipes.
#[derive(Debug, Clone, Copy)]
pub struct DeoptFrameLayout<'a> {
    valid_program_counters: &'a [u32],
    interpreter_register_count: u32,
    frame_register_count: u32,
    machine_register_count: u8,
    frame_size: u32,
    frame_environment_base: u32,
}

impl<'a> DeoptFrameLayout<'a> {
    /// Describes one code object's interpreter and generated-frame bounds.
    #[must_use]
    pub fn new(
        valid_program_counters: &'a [u32],
        interpreter_register_count: u32,
        frame_register_count: u32,
        machine_register_count: u8,
        frame_size: u32,
        frame_environment_base: u32,
    ) -> Self {
        Self {
            valid_program_counters,
            interpreter_register_count,
            frame_register_count,
            machine_register_count,
            frame_size,
            frame_environment_base,
        }
    }
}

/// Immutable, validated interpreter reconstruction recipe for one JIT exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeoptRecipe {
    bytecode_offset: u32,
    resume: DeoptResumePoint,
    materializations: Box<[DeoptMaterialization]>,
    environment: DeoptEnvironment,
    pending_call: DeoptPendingCall,
}

impl DeoptRecipe {
    /// Validates a recipe against interpreter and generated-frame layouts.
    pub fn new(
        bytecode_offset: u32,
        layout: DeoptFrameLayout<'_>,
        resume: DeoptResumePoint,
        materializations: impl IntoIterator<Item = DeoptMaterialization>,
        environment: DeoptEnvironment,
        pending_call: DeoptPendingCall,
    ) -> Result<Self, DeoptMetadataError> {
        if !layout.valid_program_counters.contains(&bytecode_offset) {
            return Err(DeoptMetadataError::InvalidProgramCounter {
                pc: bytecode_offset,
            });
        }
        if let DeoptEnvironment::TruncateTo(depth) = environment
            && depth < layout.frame_environment_base
        {
            return Err(DeoptMetadataError::EnvironmentBelowFrameBase {
                base: layout.frame_environment_base,
                depth,
            });
        }
        let materializations = materializations.into_iter().collect::<Box<[_]>>();
        let mut destinations = BTreeSet::new();
        for materialization in &materializations {
            if materialization.destination >= layout.interpreter_register_count {
                return Err(DeoptMetadataError::DestinationOutOfBounds {
                    register: materialization.destination,
                });
            }
            if !destinations.insert(materialization.destination) {
                return Err(DeoptMetadataError::DuplicateDestination {
                    register: materialization.destination,
                });
            }
            match materialization.source {
                ValueLocation::FrameRegister(register)
                    if register >= layout.frame_register_count =>
                {
                    return Err(DeoptMetadataError::FrameRegisterOutOfBounds { register });
                }
                ValueLocation::StackSlot(offset) if offset.unsigned_abs() >= layout.frame_size => {
                    return Err(DeoptMetadataError::StackSlotOutOfBounds { offset });
                }
                ValueLocation::MachineRegister(register)
                    if register >= layout.machine_register_count =>
                {
                    return Err(DeoptMetadataError::MachineRegisterOutOfBounds { register });
                }
                _ => {}
            }
        }
        Ok(Self {
            bytecode_offset,
            resume,
            materializations,
            environment,
            pending_call,
        })
    }

    /// Bytecode operation associated with this exit.
    #[must_use]
    pub const fn bytecode_offset(&self) -> u32 {
        self.bytecode_offset
    }

    /// Whether the operation is repeated or skipped after reconstruction.
    #[must_use]
    pub const fn resume_point(&self) -> DeoptResumePoint {
        self.resume
    }

    /// Ordered interpreter-register reconstruction operations.
    #[must_use]
    pub const fn materializations(&self) -> &[DeoptMaterialization] {
        &self.materializations
    }

    /// Environment-stack reconstruction operation.
    #[must_use]
    pub const fn environment(&self) -> DeoptEnvironment {
        self.environment
    }

    /// Pending-call reconstruction operation.
    #[must_use]
    pub const fn pending_call(&self) -> DeoptPendingCall {
        self.pending_call
    }

    /// Reconstructs interpreter registers from machine-register, stack-slot, or
    /// generated-frame sources. The caller owns the architecture-specific read;
    /// this method owns all representation conversion and destination writes.
    pub fn materialize(
        &self,
        destinations: &mut [JsValue],
        mut read: impl FnMut(ValueLocation) -> Option<DeoptSourceValue>,
    ) -> Result<(), DeoptMaterializationError> {
        for operation in &self.materializations {
            let destination = destinations.get_mut(operation.destination as usize).ok_or(
                DeoptMaterializationError::DestinationOutOfBounds {
                    register: operation.destination,
                },
            )?;
            let source =
                read(operation.source).ok_or(DeoptMaterializationError::MissingSource {
                    source: operation.source,
                })?;
            let value = match (operation.representation, source) {
                (_, DeoptSourceValue::Preserve) => continue,
                (DeoptValueRepresentation::Tagged, DeoptSourceValue::Tagged(value)) => value,
                (DeoptValueRepresentation::SafeInteger, DeoptSourceValue::SafeInteger(value)) => {
                    JsValue::from(value as f64)
                }
                (DeoptValueRepresentation::Boolean, DeoptSourceValue::Boolean(value)) => {
                    JsValue::from(value)
                }
                (
                    DeoptValueRepresentation::NativeTagged,
                    DeoptSourceValue::NativeTagged { value, is_boolean },
                ) => {
                    if is_boolean {
                        JsValue::from(value != 0)
                    } else {
                        JsValue::from(value as f64)
                    }
                }
                _ => {
                    return Err(DeoptMaterializationError::RepresentationMismatch {
                        source: operation.source,
                    });
                }
            };
            *destination = value;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn materialization(destination: u32, source: ValueLocation) -> DeoptMaterialization {
        DeoptMaterialization {
            destination,
            source,
            representation: DeoptValueRepresentation::Tagged,
        }
    }

    #[test]
    fn recipe_preserves_exact_resume_and_reconstruction_contract() {
        let recipe = DeoptRecipe::new(
            12,
            DeoptFrameLayout::new(&[0, 4, 12, 20], 4, 3, 4, 64, 2),
            DeoptResumePoint::AfterOperation,
            [
                materialization(0, ValueLocation::MachineRegister(1)),
                materialization(1, ValueLocation::StackSlot(-8)),
                materialization(2, ValueLocation::FrameRegister(2)),
            ],
            DeoptEnvironment::TruncateTo(2),
            DeoptPendingCall::ClearCompleted,
        )
        .unwrap();
        assert_eq!(recipe.bytecode_offset(), 12);
        assert_eq!(recipe.resume_point(), DeoptResumePoint::AfterOperation);
        assert_eq!(recipe.materializations().len(), 3);
        assert_eq!(recipe.environment(), DeoptEnvironment::TruncateTo(2));
        assert_eq!(recipe.pending_call(), DeoptPendingCall::ClearCompleted);
    }

    #[test]
    fn malformed_recipe_is_rejected_before_installation() {
        let duplicate = DeoptRecipe::new(
            4,
            DeoptFrameLayout::new(&[4], 2, 1, 0, 16, 1),
            DeoptResumePoint::BeforeOperation,
            [
                materialization(0, ValueLocation::FrameRegister(0)),
                materialization(0, ValueLocation::StackSlot(8)),
            ],
            DeoptEnvironment::Preserve,
            DeoptPendingCall::Preserve,
        );
        assert!(matches!(
            duplicate,
            Err(DeoptMetadataError::DuplicateDestination { register: 0 })
        ));

        let invalid_source = DeoptRecipe::new(
            4,
            DeoptFrameLayout::new(&[4], 2, 1, 0, 16, 1),
            DeoptResumePoint::BeforeOperation,
            [materialization(1, ValueLocation::FrameRegister(1))],
            DeoptEnvironment::Preserve,
            DeoptPendingCall::Preserve,
        );
        assert!(matches!(
            invalid_source,
            Err(DeoptMetadataError::FrameRegisterOutOfBounds { register: 1 })
        ));

        let invalid_environment = DeoptRecipe::new(
            4,
            DeoptFrameLayout::new(&[4], 2, 1, 0, 16, 2),
            DeoptResumePoint::BeforeOperation,
            [],
            DeoptEnvironment::TruncateTo(1),
            DeoptPendingCall::Preserve,
        );
        assert!(matches!(
            invalid_environment,
            Err(DeoptMetadataError::EnvironmentBelowFrameBase { base: 2, depth: 1 })
        ));
    }

    #[test]
    fn machine_register_and_stack_values_materialize_into_interpreter_registers() {
        let recipe = DeoptRecipe::new(
            8,
            DeoptFrameLayout::new(&[8], 3, 1, 3, 64, 0),
            DeoptResumePoint::BeforeOperation,
            [
                DeoptMaterialization {
                    destination: 0,
                    source: ValueLocation::MachineRegister(2),
                    representation: DeoptValueRepresentation::SafeInteger,
                },
                DeoptMaterialization {
                    destination: 1,
                    source: ValueLocation::StackSlot(-16),
                    representation: DeoptValueRepresentation::Boolean,
                },
                DeoptMaterialization {
                    destination: 2,
                    source: ValueLocation::FrameRegister(0),
                    representation: DeoptValueRepresentation::NativeTagged,
                },
            ],
            DeoptEnvironment::Preserve,
            DeoptPendingCall::Preserve,
        )
        .unwrap();
        let mut registers = vec![JsValue::undefined(); 3];
        recipe
            .materialize(&mut registers, |source| match source {
                ValueLocation::MachineRegister(2) => Some(DeoptSourceValue::SafeInteger(42)),
                ValueLocation::StackSlot(-16) => Some(DeoptSourceValue::Boolean(true)),
                ValueLocation::FrameRegister(0) => Some(DeoptSourceValue::NativeTagged {
                    value: 7,
                    is_boolean: false,
                }),
                _ => None,
            })
            .unwrap();
        assert_eq!(registers[0].as_number(), Some(42.0));
        assert_eq!(registers[1].as_boolean(), Some(true));
        assert_eq!(registers[2].as_number(), Some(7.0));
    }
}
