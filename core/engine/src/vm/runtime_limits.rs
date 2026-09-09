use std::time::Instant;

use crate::{JsNativeError, JsResult};

/// Stable error message for an expired cooperative execution deadline.
pub const WALL_CLOCK_TIMEOUT_MESSAGE: &str = "JavaScript evaluation exceeded wall-clock timeout";

/// Represents the limits of different runtime operations.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeLimits {
    /// Max stack size before an error is thrown.
    stack_size: usize,

    /// Max loop iterations before an error is thrown.
    loop_iteration: u64,

    /// Max backtrace count in exception.
    backtrace_limit: usize,

    /// Max function recursion limit
    resursion: usize,

    /// Absolute deadline, preserved across nested calls and native suspension.
    deadline: Option<Instant>,
}

impl Default for RuntimeLimits {
    #[inline]
    fn default() -> Self {
        Self {
            loop_iteration: u64::MAX,
            resursion: 512,
            backtrace_limit: 50,
            stack_size: 1024 * 10,
            deadline: None,
        }
    }
}

impl RuntimeLimits {
    /// Returns the absolute cooperative execution deadline, if configured.
    #[must_use]
    pub const fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Sets an absolute deadline shared by interpreter and generated execution.
    ///
    /// `None` disables the time limit. Expiry is checked at execution polls and
    /// call boundaries; it does not preempt a blocking native host function.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
    }

    pub(crate) fn check_deadline(&self) -> JsResult<()> {
        if self.deadline.is_none() {
            return Ok(());
        }
        self.check_deadline_at(Instant::now())
    }

    fn check_deadline_at(&self, now: Instant) -> JsResult<()> {
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            return Err(JsNativeError::runtime_limit()
                .with_message(WALL_CLOCK_TIMEOUT_MESSAGE)
                .into());
        }
        Ok(())
    }

    /// Return the loop iteration limit.
    ///
    /// If the limit is exceeded in a loop it will throw and errror.
    ///
    /// The limit value [`u64::MAX`] means that there is no limit.
    #[inline]
    #[must_use]
    pub const fn loop_iteration_limit(&self) -> u64 {
        self.loop_iteration
    }

    /// Set the loop iteration limit.
    ///
    /// If the limit is exceeded in a loop it will throw and errror.
    ///
    /// Setting the limit to [`u64::MAX`] means that there is no limit.
    #[inline]
    pub fn set_loop_iteration_limit(&mut self, value: u64) {
        self.loop_iteration = value;
    }

    /// Disable loop iteration limit.
    #[inline]
    pub fn disable_loop_iteration_limit(&mut self) {
        self.loop_iteration = u64::MAX;
    }

    /// Get max backtrace limit for an exception.
    ///
    /// Default is 50.
    #[inline]
    #[must_use]
    pub const fn backtrace_limit(&self) -> usize {
        self.backtrace_limit
    }

    /// Set max backtrace limit for an exception.
    #[inline]
    pub fn set_backtrace_limit(&mut self, value: usize) {
        self.backtrace_limit = value;
    }

    /// Get max stack size.
    #[inline]
    #[must_use]
    pub const fn stack_size_limit(&self) -> usize {
        self.stack_size
    }

    /// Set max stack size before an error is thrown.
    #[inline]
    pub fn set_stack_size_limit(&mut self, value: usize) {
        self.stack_size = value;
    }

    /// Get recursion limit.
    #[inline]
    #[must_use]
    pub const fn recursion_limit(&self) -> usize {
        self.resursion
    }

    /// Set recursion limit before an error is thrown.
    #[inline]
    pub fn set_recursion_limit(&mut self, value: usize) {
        self.resursion = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn deadline_boundary_is_deterministic_and_uncatchable() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut limits = RuntimeLimits::default();
        limits.set_deadline(Some(deadline));
        assert!(
            limits
                .check_deadline_at(deadline.checked_sub(Duration::from_nanos(1)).unwrap())
                .is_ok()
        );
        for now in [deadline, deadline + Duration::from_nanos(1)] {
            let error = limits.check_deadline_at(now).unwrap_err();
            assert!(!error.is_catchable());
            let native = error.as_native().unwrap();
            assert!(native.is_runtime_limit());
            assert_eq!(native.message(), WALL_CLOCK_TIMEOUT_MESSAGE);
        }
        limits.set_deadline(None);
        assert!(
            limits
                .check_deadline_at(deadline + Duration::from_secs(10))
                .is_ok()
        );
    }
}
