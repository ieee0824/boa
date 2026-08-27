//! Experimental baseline JIT lowering, dispatch, code-memory, and entry ABI.
//!
//! This is an opt-in public embedding contract through `baseline-jit`; enabling
//! the synchronous VM uses generated x86-64 code for verified hot integer
//! arithmetic loops. The common lowering layer makes every unsupported
//! instruction and unrepresentable Number result an explicit interpreter
//! fallback. The API remains experimental and may evolve between fork revisions
//! while Gate 3 is in progress.

mod arithmetic;
mod deopt;
mod exception;
mod lowering;
mod platform;
mod runtime_call;
mod stack_map;

pub use arithmetic::ArithmeticJitDiagnostics;
pub(crate) use arithmetic::ArithmeticRuntime;

pub use deopt::{
    DeoptEnvironment, DeoptFrameLayout, DeoptMaterialization, DeoptMaterializationError,
    DeoptMetadataError, DeoptPendingCall, DeoptReason, DeoptRecipe, DeoptResumePoint,
    DeoptSourceValue, DeoptValueRepresentation,
};
pub use exception::{
    JitExceptionHandler, JitExceptionMetadata, JitExceptionMetadataError, JitExceptionTraceFrame,
    JitExceptionUnwindPlan, JitExceptionUnwindTarget, JitSourceLocation,
};

pub use lowering::{
    BaselineBlock, BaselineBlockKind, BaselineController, BaselineDiagnostics, BaselineEntry,
    BaselineInstruction, BaselineIr, BaselineOperand, BaselineOperandValue, BytecodeCodeMap,
    BytecodeCodeMapEntry, CompileDecision, LoweringError, VmState,
};
pub use runtime_call::{
    JitAllocationKind, JitRuntimeCall, JitRuntimeCallDiagnostics, RuntimeCallError,
};
pub use stack_map::{
    ActiveJitFrame, FrameCaller, FrameMetadataError, JitFrameChain, JitFrameDescriptor,
    JitFrameDescriptorId, JitFrameHeader, JitPcLookup, JitPcTable, Safepoint, SafepointKind,
    StackMap, ValueLocation,
};

use std::{
    collections::HashMap,
    fmt, io,
    sync::atomic::{AtomicU64, Ordering},
};

use platform::{ExecutableMemory, WritableMemory};

/// The calling convention used by the first baseline JIT entry stub.
pub const JIT_ABI: &str = "System V AMD64: extern C fn() -> u64";

/// Errors produced while allocating, publishing, or looking up JIT code.
#[derive(Debug)]
pub enum JitError {
    /// This experimental backend only exists on supported `x86_64` Unix hosts.
    UnsupportedPlatform,
    /// The requested code object had no bytes or overflowed page rounding.
    InvalidCodeSize,
    /// An operating-system memory operation failed.
    Os(io::Error),
    /// A handle no longer names the current cache generation.
    StaleCodeHandle,
    /// An emitter produced an invalid frame descriptor or stack map.
    FrameMetadata(FrameMetadataError),
    /// An emitter produced an invalid interpreter reconstruction recipe.
    DeoptMetadata(DeoptMetadataError),
    /// An emitter produced invalid exception handler or source metadata.
    ExceptionMetadata(JitExceptionMetadataError),
}

impl fmt::Display for JitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                formatter.write_str("baseline JIT is unsupported on this platform")
            }
            Self::InvalidCodeSize => formatter.write_str("invalid JIT code size"),
            Self::Os(error) => write!(formatter, "JIT code memory operation failed: {error}"),
            Self::StaleCodeHandle => formatter.write_str("JIT code handle is stale or invalidated"),
            Self::FrameMetadata(error) => error.fmt(formatter),
            Self::DeoptMetadata(error) => error.fmt(formatter),
            Self::ExceptionMetadata(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for JitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Os(error) => Some(error),
            Self::FrameMetadata(error) => Some(error),
            Self::DeoptMetadata(error) => Some(error),
            Self::ExceptionMetadata(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for JitError {
    fn from(error: io::Error) -> Self {
        Self::Os(error)
    }
}

impl From<FrameMetadataError> for JitError {
    fn from(error: FrameMetadataError) -> Self {
        Self::FrameMetadata(error)
    }
}

impl From<DeoptMetadataError> for JitError {
    fn from(error: DeoptMetadataError) -> Self {
        Self::DeoptMetadata(error)
    }
}

impl From<JitExceptionMetadataError> for JitError {
    fn from(error: JitExceptionMetadataError) -> Self {
        Self::ExceptionMetadata(error)
    }
}

/// Stable identity for compiled bytecode plus its invalidation version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JitCacheKey {
    /// Engine-owned identity of the source code block.
    pub code_id: u64,
    /// Bytecode/IC version that the machine code was compiled against.
    pub version: u32,
}

/// Non-owning capability for one cache generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitCodeHandle {
    cache_id: u64,
    key: JitCacheKey,
    generation: u64,
}

/// Observable memory state for diagnostics and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodePermission {
    /// The object is writable and cannot execute.
    ReadWrite,
    /// The object is executable and cannot be written.
    ReadExecute,
}

#[derive(Debug)]
struct FixedReturnCode {
    memory: ExecutableMemory,
    value: u64,
}

impl FixedReturnCode {
    fn compile(value: u64) -> Result<Self, JitError> {
        // System V AMD64: movabs rax, imm64; ret.
        let mut bytes = [0_u8; 11];
        bytes[0] = 0x48;
        bytes[1] = 0xB8;
        bytes[2..10].copy_from_slice(&value.to_le_bytes());
        bytes[10] = 0xC3;

        let mut writable = WritableMemory::allocate(bytes.len())?;
        writable.write(0, &bytes)?;
        let memory = writable.publish()?;
        Ok(Self { memory, value })
    }

    fn call(&self) -> u64 {
        // SAFETY: `compile` is the only constructor and emits exactly
        // `movabs rax, imm64; ret`, matching JIT_ABI without touching the stack
        // or any callee-saved register. The mapping remains alive for the call.
        let entry: unsafe extern "C" fn() -> u64 =
            unsafe { std::mem::transmute(self.memory.as_ptr()) };
        // SAFETY: The generated function has no arguments and its body/ABI were
        // validated by construction above.
        unsafe { entry() }
    }
}

#[derive(Debug)]
struct CacheEntry {
    generation: u64,
    code: FixedReturnCode,
}

/// Runtime-local code cache. Separate instances never share executable code.
#[derive(Debug)]
pub struct JitCodeCache {
    cache_id: u64,
    entries: HashMap<JitCacheKey, CacheEntry>,
    next_generation: u64,
}

static NEXT_CACHE_ID: AtomicU64 = AtomicU64::new(1);

impl Default for JitCodeCache {
    fn default() -> Self {
        Self {
            cache_id: NEXT_CACHE_ID.fetch_add(1, Ordering::Relaxed),
            entries: HashMap::new(),
            next_generation: 0,
        }
    }
}

impl JitCodeCache {
    /// Creates an empty runtime-local cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Compiles and inserts a validated fixed-return ABI probe.
    pub fn compile_fixed_return(
        &mut self,
        key: JitCacheKey,
        value: u64,
    ) -> Result<JitCodeHandle, JitError> {
        let code = FixedReturnCode::compile(value)?;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        self.entries.insert(key, CacheEntry { generation, code });
        Ok(JitCodeHandle {
            cache_id: self.cache_id,
            key,
            generation,
        })
    }

    /// Calls a current fixed-return probe, rejecting replaced/invalidated handles.
    pub fn call_fixed_return(&self, handle: JitCodeHandle) -> Result<u64, JitError> {
        if handle.cache_id != self.cache_id {
            return Err(JitError::StaleCodeHandle);
        }
        let entry = self
            .entries
            .get(&handle.key)
            .filter(|entry| entry.generation == handle.generation)
            .ok_or(JitError::StaleCodeHandle)?;
        let result = entry.code.call();
        debug_assert_eq!(result, entry.code.value);
        Ok(result)
    }

    /// Removes the current object for `key`; dropping it unmaps its RX pages.
    pub fn invalidate(&mut self, key: JitCacheKey) -> bool {
        self.entries.remove(&key).is_some()
    }

    /// Returns the current permission state and mapped size for diagnostics.
    #[must_use]
    pub fn diagnostics(&self, handle: JitCodeHandle) -> Option<(CodePermission, usize)> {
        if handle.cache_id != self.cache_id {
            return None;
        }
        self.entries
            .get(&handle.key)
            .filter(|entry| entry.generation == handle.generation)
            .map(|entry| (CodePermission::ReadExecute, entry.code.memory.mapped_len()))
    }
}

#[cfg(all(
    test,
    target_arch = "x86_64",
    any(target_os = "linux", target_os = "macos")
))]
mod tests {
    use super::*;

    fn key(code_id: u64) -> JitCacheKey {
        JitCacheKey {
            code_id,
            version: 1,
        }
    }

    #[test]
    fn fixed_stub_enters_and_returns_with_rx_permissions() {
        let mut cache = JitCodeCache::new();
        let handle = cache
            .compile_fixed_return(key(1), 0xFEDC_BA98_7654_3210)
            .expect("compile stub");
        assert_eq!(
            cache.call_fixed_return(handle).expect("call stub"),
            0xFEDC_BA98_7654_3210
        );
        let (permission, mapped_len) = cache.diagnostics(handle).expect("diagnostics");
        assert_eq!(permission, CodePermission::ReadExecute);
        assert!(mapped_len >= 11);
    }

    #[test]
    fn replacement_and_invalidation_reject_stale_handles() {
        let mut cache = JitCodeCache::new();
        let old = cache.compile_fixed_return(key(2), 10).expect("old stub");
        let new = cache.compile_fixed_return(key(2), 20).expect("new stub");
        assert!(matches!(
            cache.call_fixed_return(old),
            Err(JitError::StaleCodeHandle)
        ));
        assert_eq!(cache.call_fixed_return(new).expect("new call"), 20);
        assert!(cache.invalidate(key(2)));
        assert!(matches!(
            cache.call_fixed_return(new),
            Err(JitError::StaleCodeHandle)
        ));
    }

    #[test]
    fn caches_are_runtime_local() {
        let mut first = JitCodeCache::new();
        let mut second = JitCodeCache::new();
        let first_handle = first.compile_fixed_return(key(3), 30).expect("first stub");
        let second_handle = second
            .compile_fixed_return(key(3), 40)
            .expect("second stub");
        assert_eq!(
            first.call_fixed_return(first_handle).expect("first call"),
            30
        );
        assert_eq!(
            second
                .call_fixed_return(second_handle)
                .expect("second call"),
            40
        );
        assert!(matches!(
            first.call_fixed_return(second_handle),
            Err(JitError::StaleCodeHandle)
        ));
        assert!(matches!(
            second.call_fixed_return(first_handle),
            Err(JitError::StaleCodeHandle)
        ));
        first.invalidate(key(3));
        assert_eq!(
            second
                .call_fixed_return(second_handle)
                .expect("second survives"),
            40
        );
    }
}
