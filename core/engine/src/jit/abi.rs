//! Architecture register numbers shared by emitters and safepoint metadata.

/// Architecture used to interpret a generated frame's machine-register numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitArchitecture {
    /// System V AMD64, using DWARF general-purpose register numbers.
    X86_64,
    /// AAPCS64 / Apple arm64, using x0 through x30.
    Aarch64,
    /// No native backend is available for the compilation target.
    Unsupported,
}

impl JitArchitecture {
    /// Returns the architecture of this compiled engine.
    #[must_use]
    pub const fn host() -> Self {
        if cfg!(target_arch = "x86_64") {
            Self::X86_64
        } else if cfg!(target_arch = "aarch64") {
            Self::Aarch64
        } else {
            Self::Unsupported
        }
    }

    /// Returns the documented general-purpose register map for this backend.
    #[must_use]
    pub const fn registers(self) -> JitRegisterMap {
        match self {
            Self::X86_64 => JitRegisterMap {
                architecture: self,
                arguments: &[5, 4, 1, 2, 8, 9],
                result: 0,
                frame_pointer: 6,
                stack_pointer: 7,
                callee_saved: &[3, 6, 12, 13, 14, 15],
            },
            Self::Aarch64 => JitRegisterMap {
                architecture: self,
                arguments: &[0, 1, 2, 3, 4, 5, 6, 7],
                result: 0,
                frame_pointer: 29,
                stack_pointer: 31,
                callee_saved: &[19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29],
            },
            Self::Unsupported => JitRegisterMap {
                architecture: self,
                arguments: &[],
                result: 0,
                frame_pointer: 0,
                stack_pointer: 0,
                callee_saved: &[],
            },
        }
    }
}

/// General-purpose register convention used by a frame descriptor.
///
/// ARM64 additionally preserves the low 64 bits of v8-v15. Those registers
/// carry unboxed numbers, never GC pointers, and do not occur in a `StackMap`.
/// Both backends require a 16-byte aligned native stack at a call boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitRegisterMap {
    /// Architecture whose register numbers this map describes.
    pub architecture: JitArchitecture,
    /// Integer/pointer argument registers in ABI order.
    pub arguments: &'static [u8],
    /// Integer/pointer result register.
    pub result: u8,
    /// Register containing the native frame pointer.
    pub frame_pointer: u8,
    /// Register containing the native stack pointer; never a GC-value register.
    pub stack_pointer: u8,
    /// General-purpose registers a callee must preserve.
    pub callee_saved: &'static [u8],
}

impl JitRegisterMap {
    /// Returns the assembly name for a register number, including ABI-reserved registers.
    #[must_use]
    pub fn name(self, register: u8) -> Option<&'static str> {
        const X86: [&str; 16] = [
            "rax", "rdx", "rcx", "rbx", "rsi", "rdi", "rbp", "rsp", "r8", "r9", "r10", "r11",
            "r12", "r13", "r14", "r15",
        ];
        const ARM: [&str; 32] = [
            "x0", "x1", "x2", "x3", "x4", "x5", "x6", "x7", "x8", "x9", "x10", "x11", "x12", "x13",
            "x14", "x15", "x16", "x17", "x18", "x19", "x20", "x21", "x22", "x23", "x24", "x25",
            "x26", "x27", "x28", "x29", "x30", "sp",
        ];
        match self.architecture {
            JitArchitecture::X86_64 => X86.get(usize::from(register)).copied(),
            JitArchitecture::Aarch64 => ARM.get(usize::from(register)).copied(),
            JitArchitecture::Unsupported => None,
        }
    }

    /// Whether a register can hold a described machine value at a safepoint.
    /// ARM64 x18 is reserved on both targets so emitted code obeys Apple's ABI.
    #[must_use]
    pub fn is_value_register(self, register: u8) -> bool {
        self.name(register).is_some()
            && register != self.stack_pointer
            && !(self.architecture == JitArchitecture::Aarch64 && register == 18)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_numbers_match_both_native_calling_conventions() {
        let arm = JitArchitecture::Aarch64.registers();
        assert_eq!(arm.arguments, &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(arm.name(arm.result), Some("x0"));
        assert_eq!(arm.name(arm.frame_pointer), Some("x29"));
        assert_eq!(arm.name(arm.stack_pointer), Some("sp"));
        assert_eq!(
            arm.callee_saved,
            &[19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29]
        );
        for register in [18, 31, 32, 255] {
            assert!(!arm.is_value_register(register));
        }
        assert!(arm.is_value_register(19));
        let x86 = JitArchitecture::X86_64.registers();
        let argument_names: Vec<_> = x86
            .arguments
            .iter()
            .map(|&r| x86.name(r).unwrap())
            .collect();
        assert_eq!(argument_names, ["rdi", "rsi", "rdx", "rcx", "r8", "r9"]);
        assert!(!x86.is_value_register(7));
        assert!(!x86.is_value_register(16));
        assert!(x86.is_value_register(3));
    }
}
