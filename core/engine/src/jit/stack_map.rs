//! GC-safe frame and program-counter metadata shared by JIT runtimes.
//!
//! The metadata is deliberately independent of the current arithmetic emitter:
//! later tiers can describe machine registers and native stack slots while the
//! arithmetic tier records that its unboxed integer scratch registers are not
//! GC roots.

use std::{collections::BTreeSet, error::Error, fmt, sync::Arc};

/// Stable identity of one installed JIT frame descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JitFrameDescriptorId(pub u64);

/// A location that contains a live garbage-collected value at a safepoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValueLocation {
    /// Architecture register number in the emitter's documented register set.
    MachineRegister(u8),
    /// Signed byte offset from the JIT frame pointer.
    StackSlot(i32),
    /// Slot in the engine-owned frame register file.
    FrameRegister(u32),
}

/// Why generated execution may stop at a machine-code position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SafepointKind {
    /// A call into generated or runtime code.
    Call,
    /// An operation that may allocate a GC-managed value.
    Allocation,
    /// A backwards control-flow edge.
    LoopBackedge,
    /// An exit that restores interpreter execution.
    Bailout,
}

/// Sorted, duplicate-free live GC-value locations for one safepoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackMap(Box<[ValueLocation]>);

impl StackMap {
    /// Encodes live locations in deterministic order.
    #[must_use]
    pub fn new(locations: impl IntoIterator<Item = ValueLocation>) -> Self {
        Self(
            locations
                .into_iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        )
    }

    /// Enumerates only locations that are live at this safepoint.
    #[must_use]
    pub fn live_values(&self) -> &[ValueLocation] {
        &self.0
    }
}

/// One exact machine-PC stop point and its interpreter reconstruction PC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Safepoint {
    /// Offset from the beginning of the generated code object.
    pub machine_offset: u32,
    /// Interpreter bytecode offset represented by this stop point.
    pub bytecode_offset: u32,
    /// Operation that makes this position a safepoint.
    pub kind: SafepointKind,
    /// GC-value locations live at this exact position.
    pub stack_map: StackMap,
}

/// Metadata validation failure. Invalid metadata must never be installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameMetadataError {
    /// A descriptor cannot identify an empty code object.
    EmptyCode,
    /// A safepoint lies outside its code object.
    MachineOffsetOutOfBounds {
        /// Rejected machine-code offset.
        offset: u32,
    },
    /// Safepoints are not strictly ordered by machine offset.
    SafepointsOutOfOrder {
        /// Duplicate or decreasing machine-code offset.
        offset: u32,
    },
    /// A map names a register beyond the frame register file.
    FrameRegisterOutOfBounds {
        /// Rejected frame-register index.
        register: u32,
    },
    /// A map names a native stack slot beyond the frame allocation.
    StackSlotOutOfBounds {
        /// Rejected frame-pointer-relative byte offset.
        offset: i32,
    },
    /// A PC table already contains the descriptor identity.
    DuplicateDescriptorId,
    /// Installed generated-code address ranges overlap or overflow.
    CodeRangesOverlap,
    /// A runtime frame does not link to the immediately enclosing frame.
    BrokenCallerChain,
    /// An active frame's PC or descriptor identity is absent from the PC table.
    UnknownSafepoint,
}

impl fmt::Display for FrameMetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid JIT frame metadata: {self:?}")
    }
}

impl Error for FrameMetadataError {}

/// Immutable layout and safepoint table for one generated code object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitFrameDescriptor {
    id: JitFrameDescriptorId,
    code_size: u32,
    frame_size: u32,
    frame_register_count: u32,
    safepoints: Box<[Safepoint]>,
}

impl JitFrameDescriptor {
    /// Builds and validates immutable metadata before it can be installed.
    pub fn new(
        id: JitFrameDescriptorId,
        code_size: u32,
        frame_size: u32,
        frame_register_count: u32,
        safepoints: impl IntoIterator<Item = Safepoint>,
    ) -> Result<Self, FrameMetadataError> {
        if code_size == 0 {
            return Err(FrameMetadataError::EmptyCode);
        }
        let safepoints = safepoints.into_iter().collect::<Box<[_]>>();
        let descriptor = Self {
            id,
            code_size,
            frame_size,
            frame_register_count,
            safepoints,
        };
        descriptor.validate()?;
        debug_assert!(descriptor.validate().is_ok());
        Ok(descriptor)
    }

    fn validate(&self) -> Result<(), FrameMetadataError> {
        let mut previous = None;
        for safepoint in &self.safepoints {
            if safepoint.machine_offset >= self.code_size {
                return Err(FrameMetadataError::MachineOffsetOutOfBounds {
                    offset: safepoint.machine_offset,
                });
            }
            if previous.is_some_and(|offset| offset >= safepoint.machine_offset) {
                return Err(FrameMetadataError::SafepointsOutOfOrder {
                    offset: safepoint.machine_offset,
                });
            }
            previous = Some(safepoint.machine_offset);
            for location in safepoint.stack_map.live_values() {
                match *location {
                    ValueLocation::FrameRegister(register)
                        if register >= self.frame_register_count =>
                    {
                        return Err(FrameMetadataError::FrameRegisterOutOfBounds { register });
                    }
                    ValueLocation::StackSlot(offset)
                        if offset.unsigned_abs() >= self.frame_size =>
                    {
                        return Err(FrameMetadataError::StackSlotOutOfBounds { offset });
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    #[must_use]
    /// Returns this descriptor's stable identity.
    pub const fn id(&self) -> JitFrameDescriptorId {
        self.id
    }
    #[must_use]
    /// Returns the generated code object's size in bytes.
    pub const fn code_size(&self) -> u32 {
        self.code_size
    }

    /// Returns the native frame allocation size in bytes.
    #[must_use]
    pub const fn frame_size(&self) -> u32 {
        self.frame_size
    }

    /// Returns the number of engine-owned frame-register slots.
    #[must_use]
    pub const fn frame_register_count(&self) -> u32 {
        self.frame_register_count
    }
    #[must_use]
    /// Returns the ordered exact-PC safepoints.
    pub const fn safepoints(&self) -> &[Safepoint] {
        &self.safepoints
    }
    #[must_use]
    /// Resolves an exact machine offset within this code object.
    pub fn lookup(&self, machine_offset: u32) -> Option<&Safepoint> {
        self.safepoints
            .binary_search_by_key(&machine_offset, |point| point.machine_offset)
            .ok()
            .map(|index| &self.safepoints[index])
    }
}

#[derive(Debug, Clone)]
struct InstalledDescriptor {
    code_start: usize,
    descriptor: Arc<JitFrameDescriptor>,
}

/// Installed machine-PC to frame/safepoint lookup table.
#[derive(Debug, Clone, Default)]
pub struct JitPcTable(Vec<InstalledDescriptor>);

/// Result of resolving an exact safepoint machine PC.
#[derive(Debug, Clone, Copy)]
pub struct JitPcLookup<'a> {
    /// Descriptor owning the resolved machine PC.
    pub descriptor: &'a JitFrameDescriptor,
    /// Exact safepoint at the resolved machine PC.
    pub safepoint: &'a Safepoint,
}

impl JitPcTable {
    /// Installs a non-overlapping generated-code range.
    pub fn install(
        &mut self,
        code_start: usize,
        descriptor: Arc<JitFrameDescriptor>,
    ) -> Result<(), FrameMetadataError> {
        if self
            .0
            .iter()
            .any(|entry| entry.descriptor.id == descriptor.id)
        {
            return Err(FrameMetadataError::DuplicateDescriptorId);
        }
        let end = code_start
            .checked_add(descriptor.code_size as usize)
            .ok_or(FrameMetadataError::CodeRangesOverlap)?;
        if self.0.iter().any(|entry| {
            let entry_end = entry.code_start + entry.descriptor.code_size as usize;
            code_start < entry_end && entry.code_start < end
        }) {
            return Err(FrameMetadataError::CodeRangesOverlap);
        }
        self.0.push(InstalledDescriptor {
            code_start,
            descriptor,
        });
        self.0.sort_unstable_by_key(|entry| entry.code_start);
        Ok(())
    }

    #[must_use]
    /// Resolves an absolute machine PC only when it is an exact safepoint.
    pub fn lookup(&self, machine_pc: usize) -> Option<JitPcLookup<'_>> {
        let index = self
            .0
            .partition_point(|entry| entry.code_start <= machine_pc);
        let entry = self.0.get(index.checked_sub(1)?)?;
        let offset = machine_pc.checked_sub(entry.code_start)?;
        let offset = u32::try_from(offset).ok()?;
        let safepoint = entry.descriptor.lookup(offset)?;
        Some(JitPcLookup {
            descriptor: &entry.descriptor,
            safepoint,
        })
    }
}

/// Link from a JIT frame to its immediate caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub enum FrameCaller {
    /// The outermost JIT frame was entered by an interpreter frame.
    Interpreter {
        /// Depth of the owning frame in the interpreter frame stack.
        frame_depth: usize,
    },
    /// A nested JIT frame was entered by the identified JIT frame.
    Jit {
        /// Runtime identity of the immediate JIT caller.
        frame_id: u64,
    },
}

/// Runtime header carried by every active JIT frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct JitFrameHeader {
    /// Runtime-unique identity of this active frame.
    pub frame_id: u64,
    /// Metadata descriptor used to interpret the frame.
    pub descriptor_id: JitFrameDescriptorId,
    /// Immediate caller link.
    pub caller: FrameCaller,
}

/// Observable state of one JIT frame stopped at a safepoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveJitFrame {
    /// Header physically owned by the JIT frame.
    pub header: JitFrameHeader,
    /// Exact machine PC captured when generated execution stopped.
    pub safepoint_pc: usize,
}

/// Validated outer-to-inner active JIT frame chain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JitFrameChain(Vec<ActiveJitFrame>);

impl JitFrameChain {
    /// Appends an inner frame after validating its immediate caller link.
    pub fn push(&mut self, frame: ActiveJitFrame) -> Result<(), FrameMetadataError> {
        let valid = match (self.0.last(), frame.header.caller) {
            (None, FrameCaller::Interpreter { .. }) => true,
            (Some(parent), FrameCaller::Jit { frame_id }) => parent.header.frame_id == frame_id,
            _ => false,
        };
        if !valid
            || self
                .0
                .iter()
                .any(|active| active.header.frame_id == frame.header.frame_id)
        {
            return Err(FrameMetadataError::BrokenCallerChain);
        }
        self.0.push(frame);
        debug_assert!(self.validate());
        Ok(())
    }

    fn validate(&self) -> bool {
        self.0
            .iter()
            .enumerate()
            .all(|(index, frame)| match (index, frame.header.caller) {
                (0, FrameCaller::Interpreter { .. }) => true,
                (0, FrameCaller::Jit { .. }) | (_, FrameCaller::Interpreter { .. }) => false,
                (_, FrameCaller::Jit { frame_id }) => self.0[index - 1].header.frame_id == frame_id,
            })
    }

    #[must_use]
    /// Enumerates active frames from outermost to innermost.
    pub fn frames(&self) -> &[ActiveJitFrame] {
        &self.0
    }

    /// Resolves every active frame to matching descriptor and stack-map data.
    pub fn resolve_safepoints<'a>(
        &self,
        table: &'a JitPcTable,
    ) -> Result<Vec<JitPcLookup<'a>>, FrameMetadataError> {
        self.0
            .iter()
            .map(|frame| {
                let lookup = table
                    .lookup(frame.safepoint_pc)
                    .ok_or(FrameMetadataError::UnknownSafepoint)?;
                if lookup.descriptor.id() != frame.header.descriptor_id {
                    return Err(FrameMetadataError::UnknownSafepoint);
                }
                Ok(lookup)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(
        machine_offset: u32,
        bytecode_offset: u32,
        kind: SafepointKind,
        live: &[ValueLocation],
    ) -> Safepoint {
        Safepoint {
            machine_offset,
            bytecode_offset,
            kind,
            stack_map: StackMap::new(live.iter().copied()),
        }
    }

    #[test]
    fn nested_jit_and_interpreter_frames_resolve_exact_live_slots() {
        let outer = Arc::new(
            JitFrameDescriptor::new(
                JitFrameDescriptorId(1),
                64,
                32,
                4,
                [point(
                    8,
                    10,
                    SafepointKind::Call,
                    &[
                        ValueLocation::FrameRegister(0),
                        ValueLocation::StackSlot(-8),
                    ],
                )],
            )
            .unwrap(),
        );
        let inner = Arc::new(
            JitFrameDescriptor::new(
                JitFrameDescriptorId(2),
                48,
                16,
                3,
                [point(
                    12,
                    22,
                    SafepointKind::Allocation,
                    &[
                        ValueLocation::MachineRegister(3),
                        ValueLocation::FrameRegister(2),
                    ],
                )],
            )
            .unwrap(),
        );
        let mut table = JitPcTable::default();
        table.install(0x1000, outer.clone()).unwrap();
        table.install(0x2000, inner.clone()).unwrap();
        let mut chain = JitFrameChain::default();
        chain
            .push(ActiveJitFrame {
                safepoint_pc: 0x1008,
                header: JitFrameHeader {
                    frame_id: 10,
                    descriptor_id: outer.id(),
                    caller: FrameCaller::Interpreter { frame_depth: 2 },
                },
            })
            .unwrap();
        chain
            .push(ActiveJitFrame {
                safepoint_pc: 0x200c,
                header: JitFrameHeader {
                    frame_id: 11,
                    descriptor_id: inner.id(),
                    caller: FrameCaller::Jit { frame_id: 10 },
                },
            })
            .unwrap();

        assert_eq!(chain.frames().len(), 2);
        let resolved = chain.resolve_safepoints(&table).unwrap();
        assert_eq!(resolved[0].safepoint.bytecode_offset, 10);
        assert_eq!(resolved[1].safepoint.bytecode_offset, 22);
        let found = table.lookup(0x200c).unwrap();
        assert_eq!(found.safepoint.bytecode_offset, 22);
        assert_eq!(
            found.safepoint.stack_map.live_values(),
            &[
                ValueLocation::MachineRegister(3),
                ValueLocation::FrameRegister(2)
            ]
        );
        assert!(
            !found
                .safepoint
                .stack_map
                .live_values()
                .contains(&ValueLocation::FrameRegister(1))
        );
        assert!(table.lookup(0x200d).is_none());
    }

    #[test]
    fn all_stop_kinds_and_invalid_maps_are_covered() {
        let descriptor = JitFrameDescriptor::new(
            JitFrameDescriptorId(3),
            32,
            16,
            1,
            [
                point(1, 1, SafepointKind::Call, &[]),
                point(2, 2, SafepointKind::Allocation, &[]),
                point(3, 3, SafepointKind::LoopBackedge, &[]),
                point(4, 4, SafepointKind::Bailout, &[]),
            ],
        )
        .unwrap();
        assert_eq!(descriptor.safepoints().len(), 4);
        assert!(matches!(
            JitFrameDescriptor::new(
                JitFrameDescriptorId(4),
                8,
                8,
                1,
                [point(
                    1,
                    0,
                    SafepointKind::Call,
                    &[ValueLocation::FrameRegister(1)]
                )],
            ),
            Err(FrameMetadataError::FrameRegisterOutOfBounds { .. })
        ));
    }
}
