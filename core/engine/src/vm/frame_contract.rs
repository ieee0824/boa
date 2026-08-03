//! Interpreter frame observation and restoration boundary for JIT fallback.
//!
//! Gate 2 does not create a second interpreter or a native JIT frame. This
//! module snapshots the current Boa frame while the VM stack still owns every
//! GC edge, validates any changed program counter and register file, and can
//! restore that state into the same active frame before interpreter fallback.
#![allow(dead_code, reason = "Gate 3 consumes this Gate 2 fallback contract")]

use std::{error::Error, fmt};

use boa_gc::Rooted;

use crate::{JsError, JsValue};

use super::{
    ActiveRunnable, CodeBlock, Vm,
    bytecode_contract::{BYTECODE_CONTRACT_VERSION, BytecodeContractError},
    call_frame::CallFrameFlags,
};

/// A failure to capture or restore an interpreter fallback frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FrameContractError {
    Bytecode(BytecodeContractError),
    StackOutOfBounds,
    UnsupportedFrameState,
    FrameChanged,
    InvalidProgramCounter { pc: u32 },
    InvalidRegisterCount { expected: u32, actual: u32 },
    InvalidEnvironmentDepth { base: u32, depth: u32, current: u32 },
}

/// Control transfer the interpreter must perform after restoring a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterpreterResumeKind {
    Continue,
    Return,
    Throw,
}

impl fmt::Display for FrameContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid interpreter fallback frame: {self:?}")
    }
}

impl Error for FrameContractError {}

impl From<BytecodeContractError> for FrameContractError {
    fn from(value: BytecodeContractError) -> Self {
        Self::Bytecode(value)
    }
}

/// Verified, reusable layout for one code block's fallback frames.
#[derive(Debug)]
pub(crate) struct InterpreterFrameLayout {
    contract_version: u32,
    code_block: Rooted<CodeBlock>,
    valid_program_counters: Box<[u32]>,
    register_count: u32,
}

impl InterpreterFrameLayout {
    pub(crate) const fn register_count(&self) -> u32 {
        self.register_count
    }
}

/// Owned state for the currently active interpreter frame.
///
/// The token is tied by identity and stack layout to one active frame. It is
/// deliberately crate-private: Gate 3 may update scalar/register values on a
/// no-allocation fast path, then must consume the token to re-enter Boa. Gate 4
/// must add an independently rooted JIT-frame representation before a token
/// may survive a GC safepoint.
#[derive(Debug)]
pub(crate) struct InterpreterFrameState<'layout, 'registers> {
    layout: &'layout InterpreterFrameLayout,
    frame_depth: usize,
    register_pointer: u32,
    argument_count: u32,
    environment_base: u32,
    environment_depth: u32,
    flags: CallFrameFlags,
    pc: u32,
    registers: &'registers mut [JsValue],
    loop_iteration_count: u64,
    return_value: JsValue,
    pending_exception: Option<JsError>,
    resume_kind: InterpreterResumeKind,
}

impl InterpreterFrameState<'_, '_> {
    pub(crate) const fn program_counter(&self) -> u32 {
        self.pc
    }

    pub(crate) fn set_program_counter(&mut self, pc: u32) {
        self.pc = pc;
    }

    pub(crate) fn registers(&self) -> &[JsValue] {
        self.registers
    }

    pub(crate) fn registers_mut(&mut self) -> &mut [JsValue] {
        self.registers
    }

    pub(crate) const fn environment_depth(&self) -> u32 {
        self.environment_depth
    }

    pub(crate) fn truncate_environments(&mut self, depth: u32) {
        self.environment_depth = depth;
    }

    pub(crate) fn set_return_value(&mut self, value: JsValue) {
        self.return_value = value;
    }

    pub(crate) fn set_pending_exception(&mut self, exception: Option<JsError>) {
        self.pending_exception = exception;
    }

    pub(crate) fn complete_with_return(&mut self, value: JsValue) {
        self.return_value = value;
        self.pending_exception = None;
        self.resume_kind = InterpreterResumeKind::Return;
    }

    pub(crate) fn complete_with_throw(&mut self, exception: JsError) {
        self.pending_exception = Some(exception);
        self.resume_kind = InterpreterResumeKind::Throw;
    }
}

impl Vm {
    fn has_unsupported_interpreter_frame_state(&self) -> bool {
        self.pending_native_call.is_some()
            || !self.native_call_continuations.is_empty()
            || self.native_call_continuation_active
            || self.native_active_function.is_some()
            || !self.frame.iterators.is_empty()
            || !self.frame.binding_stack.is_empty()
            || self.frame.construct()
            || self.frame.code_block.is_async()
            || self.frame.code_block.is_generator()
            || matches!(self.frame.active_runnable, Some(ActiveRunnable::Module(_)))
    }

    /// Verifies the current code block once for reuse by compiled entries.
    pub(crate) fn verify_interpreter_frame_layout(
        &self,
    ) -> Result<InterpreterFrameLayout, FrameContractError> {
        let contract = self.frame.code_block.bytecode_contract().verify()?;
        let valid_program_counters = if contract.instructions.is_empty() {
            vec![0].into_boxed_slice()
        } else {
            contract
                .instructions
                .iter()
                .map(|instruction| instruction.offset)
                .collect()
        };
        Ok(InterpreterFrameLayout {
            contract_version: BYTECODE_CONTRACT_VERSION,
            code_block: self.frame.code_block.clone(),
            valid_program_counters,
            register_count: contract.register_count,
        })
    }

    /// Captures the current frame for a bounded, no-safepoint compiled path.
    pub(crate) fn capture_interpreter_frame<'layout, 'registers>(
        &self,
        layout: &'layout InterpreterFrameLayout,
        registers: &'registers mut [JsValue],
    ) -> Result<InterpreterFrameState<'layout, 'registers>, FrameContractError> {
        if self.has_unsupported_interpreter_frame_state() {
            return Err(FrameContractError::UnsupportedFrameState);
        }
        if layout.contract_version != BYTECODE_CONTRACT_VERSION
            || !Rooted::ptr_eq(&self.frame.code_block, &layout.code_block)
            || self.frame.code_block.register_count != layout.register_count
        {
            return Err(FrameContractError::FrameChanged);
        }
        if layout
            .valid_program_counters
            .binary_search(&self.frame.pc)
            .is_err()
        {
            return Err(FrameContractError::InvalidProgramCounter { pc: self.frame.pc });
        }
        if registers.len() != layout.register_count as usize {
            return Err(FrameContractError::InvalidRegisterCount {
                expected: layout.register_count,
                actual: registers.len() as u32,
            });
        }

        let start = self.frame.rp as usize;
        let end = start
            .checked_add(layout.register_count as usize)
            .ok_or(FrameContractError::StackOutOfBounds)?;
        let source = self
            .stack
            .stack
            .get(start..end)
            .ok_or(FrameContractError::StackOutOfBounds)?;
        registers.clone_from_slice(source);
        let environment_depth = u32::try_from(self.environments.len())
            .map_err(|_| FrameContractError::StackOutOfBounds)?;
        if environment_depth < self.frame.env_fp {
            return Err(FrameContractError::InvalidEnvironmentDepth {
                base: self.frame.env_fp,
                depth: environment_depth,
                current: environment_depth,
            });
        }

        Ok(InterpreterFrameState {
            layout,
            frame_depth: self.frames.len(),
            register_pointer: self.frame.rp,
            argument_count: self.frame.argument_count,
            environment_base: self.frame.env_fp,
            environment_depth,
            flags: self.frame.flags,
            pc: self.frame.pc,
            registers,
            loop_iteration_count: self.frame.loop_iteration_count,
            return_value: self.return_value.clone(),
            pending_exception: self.pending_exception.clone(),
            resume_kind: InterpreterResumeKind::Continue,
        })
    }

    /// Restores a token into the same active frame and records a JIT fallback.
    pub(crate) fn restore_interpreter_frame(
        &mut self,
        state: InterpreterFrameState<'_, '_>,
    ) -> Result<InterpreterResumeKind, FrameContractError> {
        if self.has_unsupported_interpreter_frame_state() {
            return Err(FrameContractError::UnsupportedFrameState);
        }
        if state.layout.contract_version != BYTECODE_CONTRACT_VERSION
            || self.frames.len() != state.frame_depth
            || !Rooted::ptr_eq(&self.frame.code_block, &state.layout.code_block)
            || self.frame.rp != state.register_pointer
            || self.frame.argument_count != state.argument_count
            || self.frame.env_fp != state.environment_base
            || self.frame.flags != state.flags
        {
            return Err(FrameContractError::FrameChanged);
        }

        if state
            .layout
            .valid_program_counters
            .binary_search(&state.pc)
            .is_err()
        {
            return Err(FrameContractError::InvalidProgramCounter { pc: state.pc });
        }
        if self.frame.code_block.register_count != state.layout.register_count {
            return Err(FrameContractError::FrameChanged);
        }
        if state.registers.len() != state.layout.register_count as usize {
            return Err(FrameContractError::InvalidRegisterCount {
                expected: state.layout.register_count,
                actual: state.registers.len() as u32,
            });
        }
        let current_environment_depth = u32::try_from(self.environments.len())
            .map_err(|_| FrameContractError::StackOutOfBounds)?;
        if state.environment_depth < state.environment_base
            || state.environment_depth > current_environment_depth
        {
            return Err(FrameContractError::InvalidEnvironmentDepth {
                base: state.environment_base,
                depth: state.environment_depth,
                current: current_environment_depth,
            });
        }

        let start = state.register_pointer as usize;
        let end = start
            .checked_add(state.registers.len())
            .ok_or(FrameContractError::StackOutOfBounds)?;
        let target = self
            .stack
            .stack
            .get_mut(start..end)
            .ok_or(FrameContractError::StackOutOfBounds)?;
        target.clone_from_slice(state.registers);
        self.environments.truncate(state.environment_depth as usize);
        self.frame.pc = state.pc;
        self.frame.loop_iteration_count = state.loop_iteration_count;
        self.return_value = state.return_value;
        self.pending_exception = state.pending_exception;
        self.frame.code_block.jit_metadata.record_fallback();
        Ok(state.resume_kind)
    }
}

#[cfg(test)]
mod tests {
    use boa_parser::Source;

    use crate::{Context, JsNativeError, Script, vm::CallFrame};

    use super::*;

    fn push_test_frame(context: &mut Context, register_count: u32) {
        let mut code = CodeBlock::new(crate::JsString::from("frame-contract"), 0, false);
        code.register_count = register_count;
        let environment_base = context.vm.environments.len() as u32;
        let frame = CallFrame::new_rooted(
            Rooted::new(code),
            None,
            context.vm.environments.clone(),
            context.vm.realm.clone(),
        )
        .with_env_fp(environment_base);
        context
            .vm
            .push_frame_with_stack(frame, JsValue::undefined(), JsValue::undefined());
    }

    fn evaluate_with_fallback_probe(source: &str) -> crate::JsResult<JsValue> {
        let mut context = Context::default();
        context.vm.fallback_probe = crate::vm::FallbackProbe::RoundTripOnPush;
        Script::parse(Source::from_bytes(source), None, &mut context)?.evaluate(&mut context)
    }

    #[test]
    fn frame_state_round_trips_register_control_and_exception_state() {
        let mut context = Context::default();
        push_test_frame(&mut context, 2);

        let layout = context.vm.verify_interpreter_frame_layout().unwrap();
        let mut registers = vec![JsValue::undefined(); layout.register_count() as usize];
        let mut state = context
            .vm
            .capture_interpreter_frame(&layout, &mut registers)
            .unwrap();
        assert_eq!(state.program_counter(), 0);
        assert_eq!(state.registers().len(), 2);
        state.registers_mut()[0] = 42.into();
        state.set_return_value(7.into());
        let exception = JsNativeError::typ().with_message("fallback").into();
        state.set_pending_exception(Some(exception));

        assert_eq!(
            context.vm.restore_interpreter_frame(state).unwrap(),
            InterpreterResumeKind::Continue
        );
        assert_eq!(context.vm.get_register(0), &JsValue::from(42));
        assert_eq!(context.vm.get_return_value(), JsValue::from(7));
        assert!(context.vm.pending_exception.is_some());
        assert_eq!(
            context.vm.frame.code_block.jit_metadata().fallback_entries,
            1
        );

        let mut returning = context
            .vm
            .capture_interpreter_frame(&layout, &mut registers)
            .unwrap();
        returning.complete_with_return(9.into());
        assert_eq!(
            context.vm.restore_interpreter_frame(returning).unwrap(),
            InterpreterResumeKind::Return
        );
        assert_eq!(context.vm.get_return_value(), JsValue::from(9));

        let mut throwing = context
            .vm
            .capture_interpreter_frame(&layout, &mut registers)
            .unwrap();
        throwing.complete_with_throw(JsNativeError::range().with_message("throw").into());
        assert_eq!(
            context.vm.restore_interpreter_frame(throwing).unwrap(),
            InterpreterResumeKind::Throw
        );
        assert!(context.vm.pending_exception.is_some());
    }

    #[test]
    fn restore_rejects_invalid_pc_environment_and_changed_frame() {
        let mut context = Context::default();
        push_test_frame(&mut context, 1);
        let layout = context.vm.verify_interpreter_frame_layout().unwrap();
        let mut registers = vec![JsValue::undefined(); layout.register_count() as usize];
        assert!(matches!(
            context.vm.capture_interpreter_frame(&layout, &mut []),
            Err(FrameContractError::InvalidRegisterCount {
                expected: 1,
                actual: 0,
            })
        ));

        let mut invalid_pc = context
            .vm
            .capture_interpreter_frame(&layout, &mut registers)
            .unwrap();
        invalid_pc.set_program_counter(1);
        assert_eq!(
            context.vm.restore_interpreter_frame(invalid_pc),
            Err(FrameContractError::InvalidProgramCounter { pc: 1 })
        );

        let mut invalid_environment = context
            .vm
            .capture_interpreter_frame(&layout, &mut registers)
            .unwrap();
        invalid_environment
            .truncate_environments(invalid_environment.environment_depth().saturating_add(1));
        assert!(matches!(
            context.vm.restore_interpreter_frame(invalid_environment),
            Err(FrameContractError::InvalidEnvironmentDepth { .. })
        ));

        let stale = context
            .vm
            .capture_interpreter_frame(&layout, &mut registers)
            .unwrap();
        push_test_frame(&mut context, 0);
        assert_eq!(
            context.vm.restore_interpreter_frame(stale),
            Err(FrameContractError::FrameChanged)
        );
    }

    #[test]
    fn capture_rejects_out_of_scope_constructor_frames() {
        let mut context = Context::default();
        push_test_frame(&mut context, 0);
        context.vm.frame.flags.insert(CallFrameFlags::CONSTRUCT);
        let layout = context.vm.verify_interpreter_frame_layout().unwrap();
        let mut registers = vec![JsValue::undefined(); layout.register_count() as usize];
        assert!(matches!(
            context
                .vm
                .capture_interpreter_frame(&layout, &mut registers),
            Err(FrameContractError::UnsupportedFrameState)
        ));
    }

    #[test]
    fn restore_rejects_out_of_scope_state_activated_after_capture() {
        let mut context = Context::default();
        push_test_frame(&mut context, 0);
        let layout = context.vm.verify_interpreter_frame_layout().unwrap();
        let mut registers = vec![JsValue::undefined(); layout.register_count() as usize];
        let state = context
            .vm
            .capture_interpreter_frame(&layout, &mut registers)
            .unwrap();

        context.vm.native_call_continuation_active = true;
        assert_eq!(
            context.vm.restore_interpreter_frame(state),
            Err(FrameContractError::UnsupportedFrameState)
        );
    }

    #[test]
    fn fallback_round_trip_preserves_arithmetic_calls_closures_and_control_flow() {
        let cases = [
            (
                "var s=0; for(var i=0;i<8;i++){if(i&1){s+=i}else{s-=i}} s",
                JsValue::from(4),
            ),
            (
                "function add(a,b){return a+b} function twice(x){return add(x,x)} twice(21)",
                JsValue::from(42),
            ),
            (
                "function outer(x){return function(y){return x+y}} outer(19)(23)",
                JsValue::from(42),
            ),
            (
                "try { throw 41 } catch (error) { error + 1 }",
                JsValue::from(42),
            ),
        ];
        for (source, expected) in cases {
            assert_eq!(evaluate_with_fallback_probe(source).unwrap(), expected);
        }
    }

    #[test]
    fn fallback_round_trip_preserves_uncaught_exception_and_recursion_limit() {
        let error = evaluate_with_fallback_probe("throw new TypeError('fallback')").unwrap_err();
        assert!(error.is_catchable());

        let error =
            evaluate_with_fallback_probe("function recurse(){recurse()} recurse()").unwrap_err();
        assert!(!error.is_catchable());
    }
}
