use crate::JsNativeError;
use crate::{Context, JsResult, vm::opcode::Operation};

/// `IncrementLoopIteration` implements the Opcode Operation for `Opcode::IncrementLoopIteration`.
///
/// Operation:
///  - Increment loop iteration count.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IncrementLoopIteration;

impl IncrementLoopIteration {
    #[inline(always)]
    pub(crate) fn operation((): (), context: &mut Context) -> JsResult<()> {
        let max = context.vm.runtime_limits.loop_iteration_limit();
        let frame = context.vm.frame_mut();
        let previous_iteration_count = frame.loop_iteration_count;

        if previous_iteration_count > max {
            return Err(JsNativeError::runtime_limit()
                .with_message(format!("Maximum loop iteration limit {max} exceeded"))
                .into());
        }

        frame.loop_iteration_count = previous_iteration_count.wrapping_add(1);

        #[cfg(feature = "baseline-jit")]
        if context.vm.arithmetic_jit_allowed
            && !context.vm.frame.code_block.jit_metadata.is_disabled()
        {
            let mut arithmetic_jit = std::mem::take(&mut context.vm.arithmetic_jit);
            arithmetic_jit.try_execute_after_increment(&mut context.vm);
            context.vm.arithmetic_jit = arithmetic_jit;
        }
        Ok(())
    }
}

impl Operation for IncrementLoopIteration {
    const NAME: &'static str = "IncrementLoopIteration";
    const INSTRUCTION: &'static str = "INST - IncrementLoopIteration";
    const COST: u8 = 3;
}
