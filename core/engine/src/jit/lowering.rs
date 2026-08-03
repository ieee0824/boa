//! Architecture-neutral control layer shared by baseline emitters.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::vm::{
    BYTECODE_CONTRACT_VERSION, BytecodeContractSnapshot, BytecodeInstruction, BytecodeOperandValue,
};

use super::JitCodeHandle;

/// A scalar operand detached from Boa's private bytecode encoding.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BaselineOperandValue {
    /// Unsigned integer.
    Unsigned(u64),
    /// Signed integer.
    Signed(i64),
    /// Floating-point value.
    Float(f64),
}

/// A named operand retained by the lowering IR.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BaselineOperand {
    /// Contract field name.
    pub name: &'static str,
    /// Copied value.
    pub value: BaselineOperandValue,
}

/// VM state that must be available on entry to or fallback from a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmState {
    /// Size of the frame register file.
    pub register_count: u32,
    /// Registers referenced by this block, in stable ascending order.
    pub live_registers: Vec<u32>,
    /// Bytecode program counter represented by this state.
    pub bytecode_offset: u32,
}

/// One architecture-neutral operation.
#[derive(Debug, Clone, PartialEq)]
pub struct BaselineInstruction {
    /// Original bytecode offset.
    pub bytecode_offset: u32,
    /// Stable opcode number from the bytecode contract.
    pub opcode: u8,
    /// Opcode name used to select an emitter implementation.
    pub name: &'static str,
    /// Verified operands.
    pub operands: Vec<BaselineOperand>,
}

/// Whether a block may be handed to a native emitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineBlockKind {
    /// Every instruction has baseline semantics available to an emitter.
    Compilable,
    /// Resume the interpreter before executing this bytecode offset.
    InterpreterFallback {
        /// First unsupported opcode.
        opcode: &'static str,
        /// Resume program counter.
        resume_offset: u32,
    },
}

/// A basic block with explicit control-flow and fallback state.
#[derive(Debug, Clone, PartialEq)]
pub struct BaselineBlock {
    /// Dense block identifier.
    pub id: u32,
    /// Inclusive bytecode start.
    pub start_offset: u32,
    /// Exclusive bytecode end.
    pub end_offset: u32,
    /// State needed by a compiled entry or interpreter fallback.
    pub entry_state: VmState,
    /// Lowered instructions. A fallback block includes the unsupported opcode
    /// for diagnostics but emitters must not execute it.
    pub instructions: Vec<BaselineInstruction>,
    /// Successor block identifiers.
    pub successors: Vec<u32>,
    /// Compilation disposition.
    pub kind: BaselineBlockKind,
}

/// Verified, architecture-neutral baseline program.
#[derive(Debug, Clone, PartialEq)]
pub struct BaselineIr {
    /// Bytecode contract version used for lowering.
    pub contract_version: u32,
    /// Original bytecode length.
    pub byte_len: u32,
    /// Lowered basic blocks.
    pub blocks: Vec<BaselineBlock>,
}

/// Lowering failure. The caller must stay in the interpreter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoweringError {
    /// The snapshot was not produced by the contract version understood here.
    ContractVersion {
        /// Version understood by this lowering implementation.
        expected: u32,
        /// Version found in the snapshot or emitted entry.
        actual: u32,
    },
    /// The supplied snapshot was not structurally self-consistent.
    MalformedSnapshot {
        /// Bytecode offset at which consistency was lost.
        offset: u32,
    },
    /// An emitter attempted to install code without an outstanding request.
    NoCompileRequest,
}

impl std::fmt::Display for LoweringError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ContractVersion { expected, actual } => write!(
                formatter,
                "baseline contract version mismatch: expected {expected}, got {actual}"
            ),
            Self::MalformedSnapshot { offset } => {
                write!(formatter, "malformed bytecode snapshot at offset {offset}")
            }
            Self::NoCompileRequest => formatter.write_str("no baseline compile request is queued"),
        }
    }
}

impl std::error::Error for LoweringError {}

impl BaselineIr {
    /// Lowers a verified contract snapshot into basic blocks.
    pub fn lower(snapshot: &BytecodeContractSnapshot) -> Result<Self, LoweringError> {
        if snapshot.version != BYTECODE_CONTRACT_VERSION {
            return Err(LoweringError::ContractVersion {
                expected: BYTECODE_CONTRACT_VERSION,
                actual: snapshot.version,
            });
        }
        if snapshot.instructions.is_empty() {
            return Ok(Self {
                contract_version: snapshot.version,
                byte_len: snapshot.byte_len,
                blocks: Vec::new(),
            });
        }

        let mut boundaries = BTreeSet::from([snapshot.instructions[0].offset]);
        let instruction_offsets = snapshot
            .instructions
            .iter()
            .map(|i| i.offset)
            .collect::<BTreeSet<_>>();
        for (index, instruction) in snapshot.instructions.iter().enumerate() {
            if instruction.next_offset <= instruction.offset
                || (index + 1 < snapshot.instructions.len()
                    && snapshot.instructions[index + 1].offset != instruction.next_offset)
                || (index + 1 == snapshot.instructions.len()
                    && instruction.next_offset != snapshot.byte_len)
            {
                return Err(LoweringError::MalformedSnapshot {
                    offset: instruction.offset,
                });
            }
            for target in branch_targets(instruction) {
                if !instruction_offsets.contains(&target) {
                    return Err(LoweringError::MalformedSnapshot {
                        offset: instruction.offset,
                    });
                }
                boundaries.insert(target);
            }
            if !is_supported(instruction.name) {
                boundaries.insert(instruction.offset);
            }
            if (is_terminator(instruction.name) || !is_supported(instruction.name))
                && instruction.next_offset < snapshot.byte_len
            {
                boundaries.insert(instruction.next_offset);
            }
        }

        let starts = boundaries.into_iter().collect::<Vec<_>>();
        let block_by_offset = starts
            .iter()
            .enumerate()
            .map(|(id, offset)| (*offset, id as u32))
            .collect::<BTreeMap<_, _>>();
        let mut blocks = Vec::with_capacity(starts.len());
        for (id, start) in starts.iter().copied().enumerate() {
            let end = starts.get(id + 1).copied().unwrap_or(snapshot.byte_len);
            let source = snapshot
                .instructions
                .iter()
                .filter(|i| i.offset >= start && i.offset < end)
                .collect::<Vec<_>>();
            if source.is_empty() {
                return Err(LoweringError::MalformedSnapshot { offset: start });
            }
            let unsupported = source.iter().find(|i| !is_supported(i.name)).copied();
            let live_registers = source
                .iter()
                .flat_map(|i| register_operands(i))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let instructions = source
                .iter()
                .map(|instruction| lower_instruction(instruction))
                .collect();
            let kind = unsupported.map_or(BaselineBlockKind::Compilable, |instruction| {
                BaselineBlockKind::InterpreterFallback {
                    opcode: instruction.name,
                    resume_offset: instruction.offset,
                }
            });
            let last = *source.last().expect("non-empty source block");
            let successors = successors(last, end, snapshot.byte_len)
                .into_iter()
                .filter_map(|offset| block_by_offset.get(&offset).copied())
                .collect();
            blocks.push(BaselineBlock {
                id: id as u32,
                start_offset: start,
                end_offset: end,
                entry_state: VmState {
                    register_count: snapshot.register_count,
                    live_registers,
                    bytecode_offset: start,
                },
                instructions,
                successors,
                kind,
            });
        }
        Ok(Self {
            contract_version: snapshot.version,
            byte_len: snapshot.byte_len,
            blocks,
        })
    }

    /// Stable lowering dump used by diagnostics and emitter golden tests.
    #[must_use]
    pub fn dump(&self) -> String {
        let mut output = format!(
            "baseline-ir-v{} bytes={} blocks={}\n",
            self.contract_version,
            self.byte_len,
            self.blocks.len()
        );
        for block in &self.blocks {
            let _ = writeln!(
                output,
                "block{} {:06}..{:06} {:?} live={:?} succ={:?}",
                block.id,
                block.start_offset,
                block.end_offset,
                block.kind,
                block.entry_state.live_registers,
                block.successors
            );
            for instruction in &block.instructions {
                let _ = write!(
                    output,
                    "  {:06} {}",
                    instruction.bytecode_offset, instruction.name
                );
                for operand in &instruction.operands {
                    let _ = write!(output, " {}={:?}", operand.name, operand.value);
                }
                output.push('\n');
            }
        }
        output
    }
}

/// One source-to-machine-code mapping emitted after lowering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BytecodeCodeMapEntry {
    /// Source bytecode offset.
    pub bytecode_offset: u32,
    /// Offset from the start of the machine-code object.
    pub machine_offset: u32,
}

/// Ordered source map shared by all architecture-specific emitters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BytecodeCodeMap(Vec<BytecodeCodeMapEntry>);

impl BytecodeCodeMap {
    /// Records a monotonically increasing source/code position.
    pub fn push(&mut self, bytecode_offset: u32, machine_offset: u32) -> Result<(), LoweringError> {
        if self.0.last().is_some_and(|last| {
            last.bytecode_offset >= bytecode_offset || last.machine_offset > machine_offset
        }) {
            return Err(LoweringError::MalformedSnapshot {
                offset: bytecode_offset,
            });
        }
        self.0.push(BytecodeCodeMapEntry {
            bytecode_offset,
            machine_offset,
        });
        Ok(())
    }

    /// Ordered entries.
    #[must_use]
    pub fn entries(&self) -> &[BytecodeCodeMapEntry] {
        &self.0
    }

    /// Stable emitter diagnostic suitable for code-dump annotations.
    #[must_use]
    pub fn dump(&self) -> String {
        let mut output = String::from("bytecode-to-machine-offset\n");
        for entry in &self.0 {
            let _ = writeln!(
                output,
                "  bc={:06} machine=+0x{:06x}",
                entry.bytecode_offset, entry.machine_offset
            );
        }
        output
    }
}

/// Installed entry and its source map.
#[derive(Debug, Clone)]
pub struct BaselineEntry {
    /// Generation-checked executable-code handle.
    pub handle: JitCodeHandle,
    /// Contract version the entry was emitted from.
    pub contract_version: u32,
    /// Source mapping produced by the emitter.
    pub code_map: BytecodeCodeMap,
}

/// Result of observing one interpreter entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompileDecision {
    /// Continue in the interpreter.
    Interpret,
    /// The hotness threshold was reached; lower and emit this code block.
    CompileNow,
    /// Dispatch through the current generation-checked compiled entry.
    EnterCompiled(JitCodeHandle),
}

/// Observable tiering and bailout counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BaselineDiagnostics {
    /// Entries that selected or continued in the interpreter.
    pub interpreter_entries: u64,
    /// One-shot requests sent to an emitter.
    pub compile_requests: u64,
    /// Dispatches selecting an installed compiled entry.
    pub compiled_entries: u64,
    /// Compiled execution paths that resumed the interpreter.
    pub bailouts: u64,
    /// Installed entries removed due to bytecode or IC changes.
    pub invalidations: u64,
}

/// Per-code-block baseline tiering controller.
#[derive(Debug)]
pub struct BaselineController {
    threshold: u64,
    queued: bool,
    entry: Option<BaselineEntry>,
    diagnostics: BaselineDiagnostics,
}

impl BaselineController {
    /// Creates a controller. A threshold of zero compiles on the first entry.
    #[must_use]
    pub fn new(threshold: u64) -> Self {
        Self {
            threshold,
            queued: false,
            entry: None,
            diagnostics: BaselineDiagnostics::default(),
        }
    }
    /// Records an interpreter/dispatch entry and requests compilation once.
    pub fn enter(&mut self) -> CompileDecision {
        if let Some(entry) = &self.entry {
            self.diagnostics.compiled_entries = self.diagnostics.compiled_entries.saturating_add(1);
            return CompileDecision::EnterCompiled(entry.handle);
        }
        self.diagnostics.interpreter_entries =
            self.diagnostics.interpreter_entries.saturating_add(1);
        if !self.queued && self.diagnostics.interpreter_entries >= self.threshold.max(1) {
            self.queued = true;
            self.diagnostics.compile_requests = self.diagnostics.compile_requests.saturating_add(1);
            CompileDecision::CompileNow
        } else {
            CompileDecision::Interpret
        }
    }
    /// Installs the entry created for the latest request.
    pub fn install(&mut self, entry: BaselineEntry) -> Result<(), LoweringError> {
        if !self.queued || entry.contract_version != BYTECODE_CONTRACT_VERSION {
            if !self.queued {
                return Err(LoweringError::NoCompileRequest);
            }
            return Err(LoweringError::ContractVersion {
                expected: BYTECODE_CONTRACT_VERSION,
                actual: entry.contract_version,
            });
        }
        self.entry = Some(entry);
        self.queued = false;
        Ok(())
    }
    /// Records a safe interpreter bailout; the installed entry remains reusable.
    pub fn bailout(&mut self) {
        self.diagnostics.bailouts = self.diagnostics.bailouts.saturating_add(1);
    }
    /// Rejects a queued compilation and returns to a retryable interpreter state.
    pub fn reject_compile(&mut self) {
        if self.queued {
            self.queued = false;
            self.diagnostics.bailouts = self.diagnostics.bailouts.saturating_add(1);
        }
    }
    /// Drops the installed generation and permits recompilation on the next entry.
    pub fn invalidate(&mut self) -> Option<JitCodeHandle> {
        self.queued = false;
        let old = self.entry.take().map(|entry| entry.handle);
        if old.is_some() {
            self.diagnostics.invalidations = self.diagnostics.invalidations.saturating_add(1);
        }
        old
    }
    /// Current counters.
    #[must_use]
    pub const fn diagnostics(&self) -> BaselineDiagnostics {
        self.diagnostics
    }
    /// Installed entry, if any.
    #[must_use]
    pub const fn entry(&self) -> Option<&BaselineEntry> {
        self.entry.as_ref()
    }
}

fn lower_instruction(instruction: &BytecodeInstruction) -> BaselineInstruction {
    BaselineInstruction {
        bytecode_offset: instruction.offset,
        opcode: instruction.opcode,
        name: instruction.name,
        operands: instruction
            .operands
            .iter()
            .map(|operand| BaselineOperand {
                name: operand.name,
                value: match operand.value {
                    BytecodeOperandValue::Unsigned(v) => BaselineOperandValue::Unsigned(v),
                    BytecodeOperandValue::Signed(v) => BaselineOperandValue::Signed(v),
                    BytecodeOperandValue::Float(v) => BaselineOperandValue::Float(v),
                },
            })
            .collect(),
    }
}

fn branch_targets(instruction: &BytecodeInstruction) -> Vec<u32> {
    instruction
        .operands
        .iter()
        .filter(|o| {
            matches!(
                o.name,
                "address"
                    | "default"
                    | "addresses"
                    | "exit"
                    | "return"
                    | "r#return"
                    | "throw_method_undefined"
                    | "return_method_undefined"
            )
        })
        .filter_map(|o| match o.value {
            BytecodeOperandValue::Unsigned(v) => Some(v as u32),
            _ => None,
        })
        .collect()
}
fn register_operands(instruction: &BytecodeInstruction) -> Vec<u32> {
    instruction
        .operands
        .iter()
        .filter(|o| {
            matches!(
                o.name,
                "dst"
                    | "src"
                    | "lhs"
                    | "rhs"
                    | "value"
                    | "object"
                    | "source"
                    | "key"
                    | "condition"
                    | "receiver"
                    | "array"
                    | "function"
                    | "local"
            )
        })
        .filter_map(|o| match o.value {
            BytecodeOperandValue::Unsigned(v) => Some(v as u32),
            _ => None,
        })
        .collect()
}
fn is_terminator(name: &str) -> bool {
    matches!(
        name,
        "Jump" | "JumpIfTrue" | "JumpIfFalse" | "LogicalAnd" | "LogicalOr" | "Return" | "Throw"
    )
}
fn successors(instruction: &BytecodeInstruction, fallthrough: u32, byte_len: u32) -> Vec<u32> {
    let mut targets = branch_targets(instruction);
    if instruction.name != "Jump"
        && !matches!(instruction.name, "Return" | "Throw")
        && fallthrough < byte_len
    {
        targets.push(fallthrough);
    }
    targets.sort_unstable();
    targets.dedup();
    targets
}
fn is_supported(name: &str) -> bool {
    matches!(
        name,
        "PushZero"
            | "PushOne"
            | "PushInt8"
            | "PushInt16"
            | "PushInt32"
            | "PushFloat"
            | "PushDouble"
            | "PushNan"
            | "PushTrue"
            | "PushFalse"
            | "PushNull"
            | "PushUndefined"
            | "Move"
            | "Add"
            | "Sub"
            | "Mul"
            | "Div"
            | "Mod"
            | "StrictEq"
            | "StrictNotEq"
            | "GreaterThan"
            | "GreaterThanOrEq"
            | "LessThan"
            | "LessThanOrEq"
            | "LogicalNot"
            | "Neg"
            | "Jump"
            | "JumpIfTrue"
            | "JumpIfFalse"
            | "Return"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::{BytecodeInstruction, BytecodeOperand};
    use crate::{Context, Script};
    use boa_parser::Source;

    fn op(
        offset: u32,
        next: u32,
        opcode: u8,
        name: &'static str,
        operands: Vec<BytecodeOperand>,
    ) -> BytecodeInstruction {
        BytecodeInstruction {
            offset,
            next_offset: next,
            opcode,
            name,
            operands,
            source_line: None,
            source_column: None,
        }
    }
    fn operand(name: &'static str, value: u64) -> BytecodeOperand {
        BytecodeOperand {
            name,
            value: BytecodeOperandValue::Unsigned(value),
        }
    }
    fn snapshot(instructions: Vec<BytecodeInstruction>, byte_len: u32) -> BytecodeContractSnapshot {
        BytecodeContractSnapshot {
            version: BYTECODE_CONTRACT_VERSION,
            byte_len,
            register_count: 4,
            binding_count: 0,
            inline_cache_count: 0,
            instructions,
            constants: Vec::new(),
            handlers: Vec::new(),
        }
    }

    fn compile(source: &str) -> BytecodeContractSnapshot {
        let mut context = Context::default();
        let script = Script::parse(Source::from_bytes(source), None, &mut context).unwrap();
        script
            .codeblock(&mut context)
            .unwrap()
            .bytecode_contract()
            .verify()
            .unwrap()
    }

    #[test]
    fn real_gate2_bytecode_lowers_deterministically_without_dropping_operations() {
        let snapshot = compile("var o={x:1}; for(var i=0;i<4;i++) o.x=o.x+i; o.x");
        let first = BaselineIr::lower(&snapshot).unwrap();
        let second = BaselineIr::lower(&snapshot).unwrap();
        assert_eq!(first.dump(), second.dump());
        assert_eq!(
            first
                .blocks
                .iter()
                .map(|block| block.instructions.len())
                .sum::<usize>(),
            snapshot.instructions.len()
        );
        assert!(
            first
                .blocks
                .iter()
                .any(|block| matches!(block.kind, BaselineBlockKind::Compilable))
        );
        assert!(
            first
                .blocks
                .iter()
                .any(|block| matches!(block.kind, BaselineBlockKind::InterpreterFallback { .. }))
        );
    }

    #[test]
    fn mixed_supported_and_unsupported_bytecode_has_safe_fallback() {
        let ir = BaselineIr::lower(&snapshot(
            vec![
                op(0, 2, 1, "PushOne", vec![operand("dst", 0)]),
                op(2, 5, 2, "GetPropertyByName", vec![operand("dst", 1)]),
                op(5, 6, 3, "Return", vec![]),
            ],
            6,
        ))
        .unwrap();
        assert_eq!(ir.blocks.len(), 3);
        assert!(matches!(ir.blocks[0].kind, BaselineBlockKind::Compilable));
        assert!(matches!(
            ir.blocks[1].kind,
            BaselineBlockKind::InterpreterFallback {
                resume_offset: 2,
                ..
            }
        ));
        assert!(ir.dump().contains("GetPropertyByName"));
    }

    #[test]
    fn branch_targets_form_basic_blocks() {
        let ir = BaselineIr::lower(&snapshot(
            vec![
                op(
                    0,
                    5,
                    1,
                    "JumpIfFalse",
                    vec![operand("address", 7), operand("value", 0)],
                ),
                op(5, 7, 2, "PushOne", vec![operand("dst", 1)]),
                op(7, 8, 3, "Return", vec![]),
            ],
            8,
        ))
        .unwrap();
        assert_eq!(
            ir.blocks.iter().map(|b| b.start_offset).collect::<Vec<_>>(),
            vec![0, 5, 7]
        );
        assert_eq!(ir.blocks[0].successors, vec![1, 2]);
    }

    #[test]
    fn source_map_rejects_ambiguous_ordering() {
        let mut map = BytecodeCodeMap::default();
        map.push(0, 0).unwrap();
        map.push(5, 3).unwrap();
        assert!(map.push(5, 4).is_err());
        assert!(map.dump().contains("machine=+0x000003"));
    }

    #[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn hotness_install_invalidation_and_recompile_are_connected() {
        use crate::jit::{JitCacheKey, JitCodeCache};

        let key = JitCacheKey {
            code_id: 7,
            version: 1,
        };
        let mut cache = JitCodeCache::new();
        let mut controller = BaselineController::new(2);
        assert_eq!(controller.enter(), CompileDecision::Interpret);
        assert_eq!(controller.enter(), CompileDecision::CompileNow);
        let first = cache.compile_fixed_return(key, 1).unwrap();
        controller
            .install(BaselineEntry {
                handle: first,
                contract_version: BYTECODE_CONTRACT_VERSION,
                code_map: BytecodeCodeMap::default(),
            })
            .unwrap();
        assert_eq!(controller.enter(), CompileDecision::EnterCompiled(first));
        assert_eq!(controller.invalidate(), Some(first));
        assert!(cache.invalidate(key));
        assert_eq!(controller.enter(), CompileDecision::CompileNow);
        let second = cache.compile_fixed_return(key, 2).unwrap();
        controller
            .install(BaselineEntry {
                handle: second,
                contract_version: BYTECODE_CONTRACT_VERSION,
                code_map: BytecodeCodeMap::default(),
            })
            .unwrap();
        assert_eq!(cache.call_fixed_return(second).unwrap(), 2);
        assert_eq!(controller.diagnostics().compile_requests, 2);
        assert_eq!(controller.diagnostics().invalidations, 1);
    }
}
