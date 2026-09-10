//! Native-stack sampling for the recursive parser's shared resource budget.

/// Reads the native stack position without depending on local-variable placement.
///
/// ASAN can move address-taken locals to its fake stack, where their numeric
/// distance bears no relationship to native stack consumption. The supported
/// distribution architectures therefore read SP directly. This is not `pure`:
/// successive invocations can observe different stack positions.
#[inline(always)]
#[allow(clippy::inline_always)] // Sample the production's frame even in unoptimized builds.
pub(super) fn current_stack_address() -> usize {
    #[cfg(all(not(miri), any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let address: usize;
        // SAFETY: Copies SP to an output register without dereferencing memory,
        // modifying SP, writing the red zone, or changing condition flags.
        unsafe {
            #[cfg(target_arch = "x86_64")]
            core::arch::asm!(
                "mov {}, rsp",
                out(reg) address,
                options(nomem, nostack, preserves_flags)
            );
            #[cfg(target_arch = "aarch64")]
            core::arch::asm!(
                "mov {}, sp",
                out(reg) address,
                options(nomem, nostack, preserves_flags)
            );
        }
        address
    }
    #[cfg(any(miri, not(any(target_arch = "x86_64", target_arch = "aarch64"))))]
    {
        // Preserve the existing non-instrumented fallback elsewhere, including
        // interpreters without inline assembly. ASAN fake-stack support is
        // verified only on the two native architectures above.
        let marker = 0_u8;
        std::ptr::from_ref(&marker) as usize
    }
}
