//! Verified exception metadata and mixed JIT/interpreter unwind planning.

use std::{collections::BTreeSet, error::Error, fmt};

use super::{FrameCaller, JitFrameChain, JitFrameDescriptorId, JitPcTable};

/// A source position attached to an exact bytecode boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitSourceLocation {
    /// Bytecode offset represented by this location.
    pub bytecode_offset: u32,
    /// One-based source line when present in the bytecode contract.
    pub line: Option<u32>,
    /// One-based source column when present in the bytecode contract.
    pub column: Option<u32>,
}

/// One verified protected range and its interpreter handler entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitExceptionHandler {
    /// First protected bytecode offset.
    pub start: u32,
    /// Exclusive protected-range end.
    pub end: u32,
    /// Bytecode offset at which catch/finally dispatch resumes.
    pub handler: u32,
    /// Number of environments retained relative to the frame base.
    pub environment_count: u32,
}

impl JitExceptionHandler {
    /// Returns whether `pc` lies in this protected range.
    #[must_use]
    pub const fn contains(self, pc: u32) -> bool {
        self.start <= pc && pc < self.end
    }
}

/// Invalid exception or source metadata rejected before code installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitExceptionMetadataError {
    /// A protected range is empty or names a non-boundary offset.
    InvalidHandler {
        /// Index of the rejected handler in compiler order.
        index: u32,
    },
    /// Source locations must be unique valid bytecode boundaries.
    InvalidSourceLocation {
        /// Duplicate or non-boundary bytecode offset.
        bytecode_offset: u32,
    },
}

impl fmt::Display for JitExceptionMetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid JIT exception metadata: {self:?}")
    }
}

impl Error for JitExceptionMetadataError {}

/// Immutable handler and source map copied from a verified bytecode contract.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JitExceptionMetadata {
    handlers: Box<[JitExceptionHandler]>,
    source_locations: Box<[JitSourceLocation]>,
}

impl JitExceptionMetadata {
    /// Validates handler ranges and source locations against bytecode boundaries.
    pub fn new(
        valid_program_counters: impl IntoIterator<Item = u32>,
        handlers: impl IntoIterator<Item = JitExceptionHandler>,
        source_locations: impl IntoIterator<Item = JitSourceLocation>,
    ) -> Result<Self, JitExceptionMetadataError> {
        let valid = valid_program_counters.into_iter().collect::<BTreeSet<_>>();
        let handlers = handlers.into_iter().collect::<Box<[_]>>();
        for (index, handler) in handlers.iter().enumerate() {
            if handler.start >= handler.end
                || !valid.contains(&handler.start)
                || !valid.contains(&handler.end)
                || !valid.contains(&handler.handler)
            {
                return Err(JitExceptionMetadataError::InvalidHandler {
                    index: index as u32,
                });
            }
        }
        let mut source_locations = source_locations.into_iter().collect::<Vec<_>>();
        source_locations.sort_unstable_by_key(|location| location.bytecode_offset);
        for (index, location) in source_locations.iter().enumerate() {
            if !valid.contains(&location.bytecode_offset)
                || index > 0
                    && source_locations[index - 1].bytecode_offset == location.bytecode_offset
            {
                return Err(JitExceptionMetadataError::InvalidSourceLocation {
                    bytecode_offset: location.bytecode_offset,
                });
            }
        }
        Ok(Self {
            handlers,
            source_locations: source_locations.into_boxed_slice(),
        })
    }

    /// Returns the innermost handler selected by Boa interpreter ordering.
    #[must_use]
    pub fn handler_at(&self, pc: u32) -> Option<JitExceptionHandler> {
        self.handlers
            .iter()
            .rev()
            .copied()
            .find(|handler| handler.contains(pc))
    }

    /// Returns source information for an exact bytecode offset.
    #[must_use]
    pub fn source_location(&self, pc: u32) -> Option<JitSourceLocation> {
        self.source_locations
            .binary_search_by_key(&pc, |location| location.bytecode_offset)
            .ok()
            .map(|index| self.source_locations[index])
    }

    /// Enumerates verified handlers in compiler order.
    #[must_use]
    pub fn handlers(&self) -> &[JitExceptionHandler] {
        &self.handlers
    }
}

/// One generated frame retained in an exception stack trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitExceptionTraceFrame {
    /// Runtime identity of the generated frame.
    pub frame_id: u64,
    /// Descriptor owning the generated frame.
    pub descriptor_id: JitFrameDescriptorId,
    /// Exact interpreter offset reconstructed for the throw.
    pub bytecode_offset: u32,
    /// Source location when the compiler supplied one.
    pub source: Option<JitSourceLocation>,
}

/// Destination selected after walking generated frames from inner to outer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitExceptionUnwindTarget {
    /// Reconstruct this generated frame and enter its bytecode catch/finally handler.
    Handler {
        /// Generated frame retained for interpreter reconstruction.
        frame_id: u64,
        /// Catch/finally bytecode entry.
        bytecode_offset: u32,
        /// Environment depth relative to the frame base.
        environment_count: u32,
    },
    /// No generated handler matched; continue in the owning interpreter frame.
    Interpreter {
        /// Interpreter frame depth recorded by the outermost JIT frame.
        frame_depth: usize,
    },
}

/// Deterministic cleanup and handler-selection result for one thrown completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitExceptionUnwindPlan {
    trace: Box<[JitExceptionTraceFrame]>,
    popped_frame_ids: Box<[u64]>,
    target: JitExceptionUnwindTarget,
}

impl JitExceptionUnwindPlan {
    /// Resolves exact safepoints, preserving the stack trace and selecting the
    /// first innermost generated handler or the owning interpreter frame.
    pub fn build(
        chain: &JitFrameChain,
        pc_table: &JitPcTable,
    ) -> Result<Self, super::FrameMetadataError> {
        if chain.frames().is_empty() {
            return Err(super::FrameMetadataError::BrokenCallerChain);
        }
        let lookups = chain.resolve_safepoints(pc_table)?;
        let mut trace = Vec::with_capacity(lookups.len());
        for (frame, lookup) in chain.frames().iter().zip(&lookups) {
            let pc = lookup.safepoint.bytecode_offset;
            trace.push(JitExceptionTraceFrame {
                frame_id: frame.header.frame_id,
                descriptor_id: frame.header.descriptor_id,
                bytecode_offset: pc,
                source: lookup.descriptor.exception_metadata().source_location(pc),
            });
        }

        let mut popped = Vec::new();
        for (frame, lookup) in chain.frames().iter().zip(&lookups).rev() {
            if let Some(handler) = lookup
                .descriptor
                .exception_metadata()
                .handler_at(lookup.safepoint.bytecode_offset)
            {
                return Ok(Self {
                    trace: trace.into_boxed_slice(),
                    popped_frame_ids: popped.into_boxed_slice(),
                    target: JitExceptionUnwindTarget::Handler {
                        frame_id: frame.header.frame_id,
                        bytecode_offset: handler.handler,
                        environment_count: handler.environment_count,
                    },
                });
            }
            popped.push(frame.header.frame_id);
        }
        let frame_depth = match chain
            .frames()
            .first()
            .expect("a resolved exception chain is non-empty")
            .header
            .caller
        {
            FrameCaller::Interpreter { frame_depth } => frame_depth,
            FrameCaller::Jit { .. } => unreachable!("JIT frame chains validate their outer caller"),
        };
        Ok(Self {
            trace: trace.into_boxed_slice(),
            popped_frame_ids: popped.into_boxed_slice(),
            target: JitExceptionUnwindTarget::Interpreter { frame_depth },
        })
    }

    /// Generated frames in outer-to-inner stack-trace order.
    #[must_use]
    pub fn trace(&self) -> &[JitExceptionTraceFrame] {
        &self.trace
    }

    /// Generated frames removed from inner to outer before reaching the target.
    #[must_use]
    pub fn popped_frame_ids(&self) -> &[u64] {
        &self.popped_frame_ids
    }

    /// Selected generated handler or interpreter continuation.
    #[must_use]
    pub const fn target(&self) -> JitExceptionUnwindTarget {
        self.target
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::jit::{
        ActiveJitFrame, JitFrameDescriptor, JitFrameHeader, Safepoint, SafepointKind, StackMap,
    };

    fn descriptor(
        id: u64,
        bytecode_offset: u32,
        handler: Option<JitExceptionHandler>,
    ) -> Arc<JitFrameDescriptor> {
        let metadata = JitExceptionMetadata::new(
            [0, 10, 20, 30],
            handler,
            [JitSourceLocation {
                bytecode_offset,
                line: Some(id as u32),
                column: Some(7),
            }],
        )
        .unwrap();
        Arc::new(
            JitFrameDescriptor::new(
                JitFrameDescriptorId(id),
                16,
                32,
                0,
                [Safepoint {
                    machine_offset: 4,
                    bytecode_offset,
                    kind: SafepointKind::Call,
                    stack_map: StackMap::new([]),
                }],
            )
            .unwrap()
            .with_exception_metadata(metadata),
        )
    }

    #[test]
    fn metadata_rejects_non_boundary_handlers_and_duplicate_sources() {
        assert_eq!(
            JitExceptionMetadata::new(
                [0, 10, 20],
                [JitExceptionHandler {
                    start: 1,
                    end: 20,
                    handler: 20,
                    environment_count: 0,
                }],
                [],
            ),
            Err(JitExceptionMetadataError::InvalidHandler { index: 0 })
        );
        assert_eq!(
            JitExceptionMetadata::new(
                [0, 10],
                [],
                [
                    JitSourceLocation {
                        bytecode_offset: 10,
                        line: None,
                        column: None,
                    },
                    JitSourceLocation {
                        bytecode_offset: 10,
                        line: Some(1),
                        column: Some(1),
                    },
                ],
            ),
            Err(JitExceptionMetadataError::InvalidSourceLocation {
                bytecode_offset: 10
            })
        );
    }

    #[test]
    fn nested_generated_frames_select_outer_handler_and_preserve_trace() {
        let outer = descriptor(
            1,
            20,
            Some(JitExceptionHandler {
                start: 10,
                end: 30,
                handler: 30,
                environment_count: 2,
            }),
        );
        let inner = descriptor(2, 10, None);
        let mut table = JitPcTable::default();
        table.install(0x1000, Arc::clone(&outer)).unwrap();
        table.install(0x2000, Arc::clone(&inner)).unwrap();
        let mut chain = JitFrameChain::default();
        chain
            .push(ActiveJitFrame {
                header: JitFrameHeader {
                    frame_id: 11,
                    descriptor_id: outer.id(),
                    caller: FrameCaller::Interpreter { frame_depth: 3 },
                },
                safepoint_pc: 0x1004,
            })
            .unwrap();
        chain
            .push(ActiveJitFrame {
                header: JitFrameHeader {
                    frame_id: 12,
                    descriptor_id: inner.id(),
                    caller: FrameCaller::Jit { frame_id: 11 },
                },
                safepoint_pc: 0x2004,
            })
            .unwrap();

        let plan = JitExceptionUnwindPlan::build(&chain, &table).unwrap();
        assert_eq!(plan.popped_frame_ids(), &[12]);
        assert_eq!(
            plan.target(),
            JitExceptionUnwindTarget::Handler {
                frame_id: 11,
                bytecode_offset: 30,
                environment_count: 2,
            }
        );
        assert_eq!(plan.trace().len(), 2);
        assert_eq!(plan.trace()[0].source.unwrap().line, Some(1));
        assert_eq!(plan.trace()[1].source.unwrap().line, Some(2));
    }

    #[test]
    fn uncaught_generated_exception_returns_to_recorded_interpreter_depth() {
        let frame = descriptor(3, 20, None);
        let mut table = JitPcTable::default();
        table.install(0x3000, Arc::clone(&frame)).unwrap();
        let mut chain = JitFrameChain::default();
        chain
            .push(ActiveJitFrame {
                header: JitFrameHeader {
                    frame_id: 21,
                    descriptor_id: frame.id(),
                    caller: FrameCaller::Interpreter { frame_depth: 5 },
                },
                safepoint_pc: 0x3004,
            })
            .unwrap();
        let plan = JitExceptionUnwindPlan::build(&chain, &table).unwrap();
        assert_eq!(plan.popped_frame_ids(), &[21]);
        assert_eq!(
            plan.target(),
            JitExceptionUnwindTarget::Interpreter { frame_depth: 5 }
        );
    }
}
