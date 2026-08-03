//! Versioned, read-only bytecode boundary for the bounded JIT prototype.
//!
//! The compact interpreter encoding remains private. Consumers receive owned
//! instruction and metadata snapshots only after the code block has been
//! structurally verified, so a future compiled entry never has to depend on
//! `ByteCompiler` or decode unchecked bytes itself.
use std::{
    cell::Cell,
    collections::BTreeSet,
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
};

use boa_gc::Finalize;

use super::{
    CodeBlock, Constant,
    opcode::{InstructionIterator, VaryingOperand},
};

/// Increment whenever the meaning or encoding of a contract field changes.
pub const BYTECODE_CONTRACT_VERSION: u32 = 1;

/// A scalar operand copied out of the private instruction representation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BytecodeOperandValue {
    /// An unsigned integer operand.
    Unsigned(u64),
    /// A signed integer operand.
    Signed(i64),
    /// A floating-point operand.
    Float(f64),
}

/// A named operand. Repeated operands retain the same name and source order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BytecodeOperand {
    /// The field name in the private instruction representation.
    pub name: &'static str,
    /// The copied scalar value.
    pub value: BytecodeOperandValue,
}

/// One verified instruction in the stable read-only view.
#[derive(Debug, Clone, PartialEq)]
pub struct BytecodeInstruction {
    /// Byte offset at which the instruction starts.
    pub offset: u32,
    /// Byte offset immediately after the instruction.
    pub next_offset: u32,
    /// Numeric opcode in this contract version.
    pub opcode: u8,
    /// Human-readable opcode name.
    pub name: &'static str,
    /// Operands in their encoded order.
    pub operands: Vec<BytecodeOperand>,
    /// One-based source line, when source information is available.
    pub source_line: Option<u32>,
    /// One-based source column, when source information is available.
    pub source_column: Option<u32>,
}

/// Exception-handler state needed to reconstruct interpreter fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BytecodeHandler {
    /// First protected bytecode offset.
    pub start: u32,
    /// Exclusive protected-range end. This is also Boa's handler entry offset.
    pub end: u32,
    /// Number of environments to restore before entering the handler.
    pub environment_count: u32,
}

/// Constant metadata copied without exposing compiler-owned storage.
#[derive(Debug, Clone, PartialEq)]
pub enum BytecodeConstant {
    /// A string literal or property name.
    String(crate::JsString),
    /// A nested function and its independently verified contract.
    Function {
        /// Display name of the nested function.
        name: String,
        /// Verified nested function contract.
        contract: Box<BytecodeContractSnapshot>,
    },
    /// A `BigInt` literal.
    BigInt(crate::JsBigInt),
    /// Scope metadata referenced by bytecode.
    Scope {
        /// Index of the scope in the compile-time scope table.
        scope_index: u32,
        /// Number of bindings declared by the scope.
        bindings: u32,
    },
}

/// Fully verified snapshot used by Gate 2/3 adapters.
#[derive(Debug, Clone, PartialEq)]
pub struct BytecodeContractSnapshot {
    /// Contract schema and semantic version.
    pub version: u32,
    /// Encoded bytecode length.
    pub byte_len: u32,
    /// Number of VM registers addressable by this code block.
    pub register_count: u32,
    /// Number of binding locators addressable by this code block.
    pub binding_count: u32,
    /// Number of inline-cache slots addressable by this code block.
    pub inline_cache_count: u32,
    /// Fully decoded and verified instructions.
    pub instructions: Vec<BytecodeInstruction>,
    /// Constants referenced by the instructions.
    pub constants: Vec<BytecodeConstant>,
    /// Verified exception handlers.
    pub handlers: Vec<BytecodeHandler>,
}

/// Validation failure. Invalid code must fall back before any compiled entry.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BytecodeContractError {
    /// Instruction decoding failed at an offset.
    Decode {
        /// Offset that could not be decoded.
        offset: u32,
    },
    /// A reserved opcode appeared in executable bytecode.
    ReservedOpcode {
        /// Offset of the reserved opcode.
        offset: u32,
        /// Numeric opcode value.
        opcode: u8,
    },
    /// A register operand exceeded the code block's register file.
    RegisterOutOfBounds {
        /// Instruction offset.
        offset: u32,
        /// Invalid register index.
        register: u32,
        /// Available register count.
        count: u32,
    },
    /// A constant operand exceeded the constant table.
    ConstantOutOfBounds {
        /// Instruction offset.
        offset: u32,
        /// Invalid constant index.
        constant: u32,
        /// Available constant count.
        count: u32,
    },
    /// A binding operand exceeded the binding table.
    BindingOutOfBounds {
        /// Instruction offset.
        offset: u32,
        /// Invalid binding index.
        binding: u32,
        /// Available binding count.
        count: u32,
    },
    /// An inline-cache operand exceeded the cache table.
    InlineCacheOutOfBounds {
        /// Instruction offset.
        offset: u32,
        /// Invalid cache index.
        cache: u32,
        /// Available cache count.
        count: u32,
    },
    /// A branch target did not land on an instruction boundary.
    InvalidJumpTarget {
        /// Branch instruction offset.
        offset: u32,
        /// Invalid target offset.
        target: u32,
    },
    /// An exception handler did not describe valid bytecode boundaries.
    InvalidHandler {
        /// Index in the handler table.
        index: u32,
    },
}

impl fmt::Display for BytecodeContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid bytecode contract: {self:?}")
    }
}

impl std::error::Error for BytecodeContractError {}

/// Borrowed entry point; the returned snapshot owns all adapter-visible data.
#[derive(Debug, Clone, Copy)]
pub struct BytecodeContract<'a> {
    code: &'a CodeBlock,
}

impl CodeBlock {
    /// Returns the versioned read-only bytecode boundary.
    #[must_use]
    pub const fn bytecode_contract(&self) -> BytecodeContract<'_> {
        BytecodeContract { code: self }
    }

    /// Returns hotness and compiled-entry state without exposing mutable VM data.
    #[must_use]
    pub fn jit_metadata(&self) -> JitMetadataSnapshot {
        self.jit_metadata.snapshot()
    }

    /// Returns diagnostics for each stable bytecode inline-cache slot.
    #[must_use]
    pub fn inline_cache_metadata(&self) -> Vec<super::InlineCacheMetadataSnapshot> {
        self.ic
            .iter()
            .enumerate()
            .map(|(index, cache)| cache.metadata(index as u32))
            .collect()
    }

    /// Enables or disables diagnostic hit/miss/install counters for this block.
    ///
    /// Counters are disabled by default so production property accesses do not
    /// pay for saturating counter updates. Existing counts are retained.
    pub fn set_inline_cache_telemetry_enabled(&self, enabled: bool) {
        for cache in &self.ic {
            cache.set_telemetry_enabled(enabled);
        }
    }

    /// Resets opt-in hit/miss/install counters without changing cache guards.
    pub fn reset_inline_cache_telemetry(&self) {
        for cache in &self.ic {
            cache.reset_telemetry();
        }
    }
}

impl BytecodeContract<'_> {
    /// Decode and validate all instruction, register, index, jump and handler data.
    pub fn verify(self) -> Result<BytecodeContractSnapshot, BytecodeContractError> {
        let byte_len = self.code.bytecode.bytecode.len() as u32;
        let mut iterator = InstructionIterator::new(&self.code.bytecode);
        let mut decoded = Vec::new();
        let mut boundaries = BTreeSet::new();

        while iterator.pc() < byte_len as usize {
            let offset = iterator.pc() as u32;
            boundaries.insert(offset);
            let item = catch_unwind(AssertUnwindSafe(|| iterator.next()))
                .map_err(|_| BytecodeContractError::Decode { offset })?
                .ok_or(BytecodeContractError::Decode { offset })?;
            let (start, opcode, instruction) = item;
            let next = iterator.pc();
            if start as u32 != offset || next <= start || next > byte_len as usize {
                return Err(BytecodeContractError::Decode { offset });
            }
            if opcode.as_str() == "Reserved" {
                return Err(BytecodeContractError::ReservedOpcode {
                    offset,
                    opcode: opcode as u8,
                });
            }
            decoded.push((offset, next as u32, opcode, instruction));
        }
        let register_count = self.code.register_count;
        let constant_count = self.code.constants.len() as u32;
        let binding_count = self.code.bindings.len() as u32;
        let inline_cache_count = self.code.ic.len() as u32;
        let mut instructions = Vec::with_capacity(decoded.len());

        for (offset, next_offset, opcode, instruction) in decoded {
            let operands = instruction.contract_operands();
            for operand in &operands {
                let BytecodeOperandValue::Unsigned(value) = operand.value else {
                    continue;
                };
                let value = value as u32;
                if is_register_operand(opcode.as_str(), operand.name) && value >= register_count {
                    return Err(BytecodeContractError::RegisterOutOfBounds {
                        offset,
                        register: value,
                        count: register_count,
                    });
                }
                if is_constant_operand(opcode.as_str(), operand.name) && value >= constant_count {
                    return Err(BytecodeContractError::ConstantOutOfBounds {
                        offset,
                        constant: value,
                        count: constant_count,
                    });
                }
                if operand.name == "binding_index" && value >= binding_count {
                    return Err(BytecodeContractError::BindingOutOfBounds {
                        offset,
                        binding: value,
                        count: binding_count,
                    });
                }
                if opcode.as_str() == "ThisForObjectEnvironmentName"
                    && operand.name == "index"
                    && value >= binding_count
                {
                    return Err(BytecodeContractError::BindingOutOfBounds {
                        offset,
                        binding: value,
                        count: binding_count,
                    });
                }
                if operand.name == "ic_index" && value >= inline_cache_count {
                    return Err(BytecodeContractError::InlineCacheOutOfBounds {
                        offset,
                        cache: value,
                        count: inline_cache_count,
                    });
                }
                if is_jump_operand(operand.name) && !boundaries.contains(&value) {
                    return Err(BytecodeContractError::InvalidJumpTarget {
                        offset,
                        target: value,
                    });
                }
            }
            let position = self.code.source_info.map().find(offset);
            instructions.push(BytecodeInstruction {
                offset,
                next_offset,
                opcode: opcode as u8,
                name: opcode.as_str(),
                operands,
                source_line: position.map(boa_ast::Position::line_number),
                source_column: position.map(boa_ast::Position::column_number),
            });
        }

        let handlers = self
            .code
            .handlers
            .iter()
            .enumerate()
            .map(|(index, handler)| {
                if handler.start >= handler.end
                    || !boundaries.contains(&handler.start)
                    || !boundaries.contains(&handler.end)
                {
                    return Err(BytecodeContractError::InvalidHandler {
                        index: index as u32,
                    });
                }
                Ok(BytecodeHandler {
                    start: handler.start,
                    end: handler.end,
                    environment_count: handler.environment_count,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let constants = self
            .code
            .constants
            .iter()
            .map(|constant| {
                Ok(match constant {
                    Constant::String(value) => BytecodeConstant::String(value.clone()),
                    Constant::Function(code) => BytecodeConstant::Function {
                        name: code.name().to_std_string_escaped(),
                        contract: Box::new(code.bytecode_contract().verify()?),
                    },
                    Constant::BigInt(value) => BytecodeConstant::BigInt(value.clone()),
                    Constant::Scope(scope) => BytecodeConstant::Scope {
                        scope_index: scope.scope_index(),
                        bindings: scope.num_bindings(),
                    },
                })
            })
            .collect::<Result<Vec<_>, BytecodeContractError>>()?;

        Ok(BytecodeContractSnapshot {
            version: BYTECODE_CONTRACT_VERSION,
            byte_len,
            register_count,
            binding_count,
            inline_cache_count,
            instructions,
            constants,
            handlers,
        })
    }

    /// Deterministic text form for golden tests and contract reviews.
    pub fn dump(self) -> Result<String, BytecodeContractError> {
        let snapshot = self.verify()?;
        let mut output = format!(
            "contract-v{} bytes={} registers={} constants={} bindings={} ic={}\n",
            snapshot.version,
            snapshot.byte_len,
            snapshot.register_count,
            snapshot.constants.len(),
            snapshot.binding_count,
            snapshot.inline_cache_count,
        );
        for instruction in snapshot.instructions {
            use std::fmt::Write as _;
            let _ = write!(
                output,
                "{:06}..{:06} {:03} {}",
                instruction.offset, instruction.next_offset, instruction.opcode, instruction.name
            );
            for operand in instruction.operands {
                let _ = write!(output, " {}={:?}", operand.name, operand.value);
            }
            output.push('\n');
        }
        Ok(output)
    }
}

fn is_jump_operand(name: &str) -> bool {
    matches!(
        name,
        "address"
            | "default"
            | "addresses"
            | "throw_method_undefined"
            | "return_method_undefined"
            | "return"
            | "r#return"
            | "exit"
    )
}

fn is_register_operand(opcode: &str, name: &str) -> bool {
    matches!(
        name,
        "dst"
            | "src"
            | "lhs"
            | "rhs"
            | "value"
            | "values"
            | "object"
            | "source"
            | "key"
            | "function"
            | "home"
            | "prototype"
            | "class"
            | "superclass"
            | "array"
            | "condition"
            | "exception"
            | "has_exception"
            | "receiver"
            | "proto"
            | "local"
            | "called"
            | "resume_kind"
            | "excluded_keys"
            | "is_return"
            | "name"
    ) || (opcode == "JumpTable" && name == "index")
}

fn is_constant_operand(opcode: &str, name: &str) -> bool {
    matches!(
        name,
        "name_index" | "pattern_index" | "flags_index" | "scope_index" | "message"
    ) || (name == "index"
        && matches!(
            opcode,
            "PushLiteral"
                | "GetFunction"
                | "InPrivate"
                | "ThrowMutateImmutable"
                | "HasRestrictedGlobalProperty"
                | "CanDeclareGlobalFunction"
                | "CanDeclareGlobalVar"
        ))
        || (opcode == "PushPrivateEnvironment" && name == "name_indices")
}

pub(crate) trait ContractArgument {
    fn append_contract(&self, name: &'static str, output: &mut Vec<BytecodeOperand>);
}

macro_rules! scalar_argument {
    ($type:ty, $variant:ident, $cast:ty) => {
        impl ContractArgument for $type {
            fn append_contract(&self, name: &'static str, output: &mut Vec<BytecodeOperand>) {
                output.push(BytecodeOperand {
                    name,
                    value: BytecodeOperandValue::$variant(*self as $cast),
                });
            }
        }
    };
}

scalar_argument!(u8, Unsigned, u64);
scalar_argument!(u16, Unsigned, u64);
scalar_argument!(u32, Unsigned, u64);
scalar_argument!(i8, Signed, i64);
scalar_argument!(i16, Signed, i64);
scalar_argument!(i32, Signed, i64);
scalar_argument!(f32, Float, f64);

impl ContractArgument for u64 {
    fn append_contract(&self, name: &'static str, output: &mut Vec<BytecodeOperand>) {
        output.push(BytecodeOperand {
            name,
            value: BytecodeOperandValue::Unsigned(*self),
        });
    }
}

impl ContractArgument for f64 {
    fn append_contract(&self, name: &'static str, output: &mut Vec<BytecodeOperand>) {
        output.push(BytecodeOperand {
            name,
            value: BytecodeOperandValue::Float(*self),
        });
    }
}

impl ContractArgument for VaryingOperand {
    fn append_contract(&self, name: &'static str, output: &mut Vec<BytecodeOperand>) {
        output.push(BytecodeOperand {
            name,
            value: BytecodeOperandValue::Unsigned(self.value().into()),
        });
    }
}

impl ContractArgument for thin_vec::ThinVec<VaryingOperand> {
    fn append_contract(&self, name: &'static str, output: &mut Vec<BytecodeOperand>) {
        for value in self {
            value.append_contract(name, output);
        }
    }
}

impl ContractArgument for thin_vec::ThinVec<u32> {
    fn append_contract(&self, name: &'static str, output: &mut Vec<BytecodeOperand>) {
        for value in self {
            value.append_contract(name, output);
        }
    }
}

/// State visible to a future compiled-entry dispatcher. No code pointer is
/// stored in Gate 2; executable memory remains Gate 3 work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitCompilationState {
    /// Execute through the interpreter.
    Interpreter,
    /// Compilation has been requested but no entry is installed.
    Queued,
    /// A compiled entry matching `compiled_contract_version` is installed.
    Compiled,
    /// Compilation is disabled for this code block.
    Disabled,
}

/// Copyable telemetry snapshot; mutation remains inside the Boa VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitMetadataSnapshot {
    /// Number of times this code block has entered the interpreter.
    pub interpreter_entries: u64,
    /// Number of compiled-entry fallbacks to the interpreter.
    pub fallback_entries: u64,
    /// Current compilation lifecycle state.
    pub state: JitCompilationState,
    /// Contract version used by the installed compiled entry, if any.
    pub compiled_contract_version: Option<u32>,
}

#[derive(Debug, Clone, Finalize)]
pub(crate) struct JitMetadata {
    entries: Cell<u64>,
    fallbacks: Cell<u64>,
    state: Cell<JitCompilationState>,
    compiled_contract_version: Cell<Option<u32>>,
}

impl Default for JitMetadata {
    fn default() -> Self {
        Self {
            entries: Cell::new(0),
            fallbacks: Cell::new(0),
            state: Cell::new(JitCompilationState::Interpreter),
            compiled_contract_version: Cell::new(None),
        }
    }
}

impl JitMetadata {
    pub(crate) fn record_interpreter_entry(&self) {
        self.entries.set(self.entries.get().saturating_add(1));
    }

    pub(crate) fn snapshot(&self) -> JitMetadataSnapshot {
        JitMetadataSnapshot {
            interpreter_entries: self.entries.get(),
            fallback_entries: self.fallbacks.get(),
            state: self.state.get(),
            compiled_contract_version: self.compiled_contract_version.get(),
        }
    }

    #[allow(
        dead_code,
        reason = "Gate 3 owns the dispatcher that drives these lifecycle transitions"
    )]
    pub(crate) fn mark_queued(&self) {
        self.state.set(JitCompilationState::Queued);
        self.compiled_contract_version.set(None);
    }

    #[allow(
        dead_code,
        reason = "Gate 3 owns the dispatcher that drives these lifecycle transitions"
    )]
    pub(crate) fn mark_compiled(&self, version: u32) {
        self.state.set(JitCompilationState::Compiled);
        self.compiled_contract_version.set(Some(version));
    }

    #[allow(
        dead_code,
        reason = "Gate 3 owns the dispatcher that drives these lifecycle transitions"
    )]
    pub(crate) fn record_fallback(&self) {
        self.fallbacks.set(self.fallbacks.get().saturating_add(1));
        self.state.set(JitCompilationState::Interpreter);
        self.compiled_contract_version.set(None);
    }

    #[allow(
        dead_code,
        reason = "Gate 3 owns the dispatcher that drives these lifecycle transitions"
    )]
    pub(crate) fn disable(&self) {
        self.state.set(JitCompilationState::Disabled);
        self.compiled_contract_version.set(None);
    }
}

#[cfg(test)]
mod tests {
    use boa_parser::Source;
    use thin_vec::thin_vec;

    use crate::{Context, Script};

    use super::*;

    fn compile(source: &str) -> boa_gc::Rooted<CodeBlock> {
        let mut context = Context::default();
        let script = Script::parse(Source::from_bytes(source), None, &mut context).unwrap();
        script.codeblock(&mut context).unwrap()
    }

    fn contract_fingerprint(dump: &str) -> u64 {
        dump.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }

    #[test]
    fn contract_dump_is_deterministic_and_covers_gate2_shapes() {
        let workloads = [
            (
                "arith",
                "var s=1; for(var i=0;i<8;i++) s=(s+i*3)%17; s",
                0xb6ec_e611_8fec_3eb5,
            ),
            (
                "prop-mono",
                "var o={a:1,b:2}; for(var i=0;i<8;i++){o.b=o.a+i} o.b",
                0x0e98_78cc_443b_5bdd,
            ),
            (
                "prop-mega",
                "var a=[{x:1},{y:0,x:2}]; var s=0; for(var i=0;i<8;i++)s+=a[i&1].x; s",
                0x0fb3_bfc1_9d53_53b2,
            ),
            (
                "call",
                "function add(a,b){return a+b} var s=0; for(var i=0;i<8;i++)s=add(s,1); s",
                0xd01c_e0d3_8345_6d4e,
            ),
            (
                "closure-alloc",
                "var s=0; for(var i=0;i<8;i++){var f=function(x){return x+i};s=f(s)} s",
                0x6563_5534_faea_09d3,
            ),
            (
                "object-alloc",
                "var s=0; for(var i=0;i<8;i++){var o={x:i,y:i+1};s+=o.x+o.y} s",
                0xd650_c3ea_b635_e953,
            ),
            (
                "string-concat",
                "var s=''; for(var i=0;i<8;i++)s+='ab'; s.length",
                0xa1dc_85c4_4aa5_a2d4,
            ),
            (
                "array",
                "var a=[]; for(var i=0;i<8;i++)a.push(i); a[3]",
                0xb73c_68e0_0591_3c25,
            ),
            (
                "primitive-string-property",
                "var s=0; for(var i=0;i<8;i++)s+='abc'.length; s",
                0x9d47_6119_c095_a937,
            ),
            (
                "primitive-string-method",
                "var s=0; for(var i=0;i<8;i++)s+='abc'.charCodeAt(i&2); s",
                0xa66f_4ea2_9d6b_9024,
            ),
            (
                "proto-method",
                "function B(){} B.prototype.at=function(i){return i&3}; var o=new B(); o.at(2)",
                0x80b0_8f3e_3f3f_0f9a,
            ),
        ];
        for (name, source, expected_fingerprint) in workloads {
            let first = compile(source).bytecode_contract().dump().unwrap();
            let second = compile(source).bytecode_contract().dump().unwrap();
            assert_eq!(first, second, "non-deterministic dump for {name}");
            assert_eq!(
                contract_fingerprint(&first),
                expected_fingerprint,
                "bytecode contract changed for {name}; review the change and bump \
                 BYTECODE_CONTRACT_VERSION when its encoding or meaning changed"
            );
        }

        let source = "var o={a:1}; for(var i=0;i<8;i++){o.a=o.a+i} o.a";
        let dump = compile(source).bytecode_contract().dump().unwrap();
        assert!(dump.contains("Add"));
        assert!(dump.contains("GetPropertyByName"));
        assert!(dump.contains("SetPropertyByName"));
        assert!(dump.contains("Jump"));
    }

    #[test]
    fn contract_rejects_invalid_register_constant_and_jump() {
        let mut code = CodeBlock::new(crate::JsString::default(), 0, false);
        let mut emitter = super::super::opcode::ByteCodeEmitter::new();
        emitter.emit_push_literal(VaryingOperand::new(1), VaryingOperand::new(0));
        code.bytecode = emitter.into_bytecode();
        code.register_count = 1;
        assert!(matches!(
            code.bytecode_contract().verify(),
            Err(BytecodeContractError::RegisterOutOfBounds { .. })
        ));

        let mut emitter = super::super::opcode::ByteCodeEmitter::new();
        emitter.emit_push_literal(VaryingOperand::new(0), VaryingOperand::new(1));
        code.bytecode = emitter.into_bytecode();
        assert!(matches!(
            code.bytecode_contract().verify(),
            Err(BytecodeContractError::ConstantOutOfBounds { .. })
        ));

        let mut emitter = super::super::opcode::ByteCodeEmitter::new();
        emitter.emit_jump(2);
        code.bytecode = emitter.into_bytecode();
        assert!(matches!(
            code.bytecode_contract().verify(),
            Err(BytecodeContractError::InvalidJumpTarget { .. })
        ));

        let mut emitter = super::super::opcode::ByteCodeEmitter::new();
        emitter.emit_jump(5);
        code.bytecode = emitter.into_bytecode();
        assert_eq!(code.bytecode.bytecode.len(), 5);
        assert!(matches!(
            code.bytecode_contract().verify(),
            Err(BytecodeContractError::InvalidJumpTarget { target: 5, .. })
        ));

        let mut emitter = super::super::opcode::ByteCodeEmitter::new();
        emitter.emit_jump_table(2, 0, thin_vec![]);
        code.bytecode = emitter.into_bytecode();
        assert!(matches!(
            code.bytecode_contract().verify(),
            Err(BytecodeContractError::RegisterOutOfBounds { .. })
        ));

        let mut emitter = super::super::opcode::ByteCodeEmitter::new();
        emitter.emit_push_private_environment(VaryingOperand::new(0), thin_vec![0]);
        code.bytecode = emitter.into_bytecode();
        assert!(matches!(
            code.bytecode_contract().verify(),
            Err(BytecodeContractError::ConstantOutOfBounds { .. })
        ));
    }

    #[test]
    fn contract_rejects_opcode_specific_register_index_and_control_operands() {
        let mut code = CodeBlock::new(crate::JsString::default(), 0, false);
        code.register_count = 3;

        let mut emitter = super::super::opcode::ByteCodeEmitter::new();
        emitter.emit_copy_data_properties(
            VaryingOperand::new(0),
            VaryingOperand::new(1),
            thin_vec![VaryingOperand::new(3)],
        );
        code.bytecode = emitter.into_bytecode();
        assert!(matches!(
            code.bytecode_contract().verify(),
            Err(BytecodeContractError::RegisterOutOfBounds { .. })
        ));

        let mut emitter = super::super::opcode::ByteCodeEmitter::new();
        emitter.emit_generator_delegate_next(
            1,
            0,
            VaryingOperand::new(0),
            VaryingOperand::new(1),
            VaryingOperand::new(2),
        );
        code.bytecode = emitter.into_bytecode();
        assert!(matches!(
            code.bytecode_contract().verify(),
            Err(BytecodeContractError::InvalidJumpTarget { .. })
        ));

        let mut emitter = super::super::opcode::ByteCodeEmitter::new();
        emitter.emit_throw_mutate_immutable(VaryingOperand::new(0));
        code.bytecode = emitter.into_bytecode();
        assert!(matches!(
            code.bytecode_contract().verify(),
            Err(BytecodeContractError::ConstantOutOfBounds { .. })
        ));

        let mut emitter = super::super::opcode::ByteCodeEmitter::new();
        emitter
            .emit_this_for_object_environment_name(VaryingOperand::new(0), VaryingOperand::new(0));
        code.bytecode = emitter.into_bytecode();
        assert!(matches!(
            code.bytecode_contract().verify(),
            Err(BytecodeContractError::BindingOutOfBounds { .. })
        ));
    }

    #[test]
    fn interpreter_entry_and_fallback_metadata_are_observable() {
        let code = compile("1 + 1");
        assert_eq!(code.jit_metadata().interpreter_entries, 0);
        code.jit_metadata.record_interpreter_entry();
        code.jit_metadata.mark_queued();
        assert_eq!(code.jit_metadata().state, JitCompilationState::Queued);
        code.jit_metadata.mark_compiled(BYTECODE_CONTRACT_VERSION);
        assert_eq!(code.jit_metadata().state, JitCompilationState::Compiled);
        code.jit_metadata.record_fallback();
        let snapshot = code.jit_metadata();
        assert_eq!(snapshot.interpreter_entries, 1);
        assert_eq!(snapshot.fallback_entries, 1);
        assert_eq!(snapshot.state, JitCompilationState::Interpreter);
        code.jit_metadata.disable();
        assert_eq!(code.jit_metadata().state, JitCompilationState::Disabled);
    }
}
