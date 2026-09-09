use std::{fmt::Write as _, io, marker::PhantomData, ptr::NonNull, rc::Rc};

use super::JitError;

#[derive(Debug)]
pub(super) struct WritableMemory {
    mapping: Mapping,
}

impl WritableMemory {
    pub(super) fn allocate(code_len: usize) -> Result<Self, JitError> {
        Mapping::allocate(code_len).map(|mapping| Self { mapping })
    }

    pub(super) fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<(), JitError> {
        let end = offset
            .checked_add(bytes.len())
            .ok_or(JitError::InvalidCodeSize)?;
        if end > self.mapping.requested_len {
            return Err(JitError::InvalidCodeSize);
        }
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            apple::write(&self.mapping, offset, bytes)
        }
        #[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
        {
            // SAFETY: bounds were checked against requested_len, which is no larger
            // than mapped_len; this type exclusively owns a writable mapping.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    self.mapping.pointer.as_ptr().add(offset),
                    bytes.len(),
                );
            }
            Ok(())
        }
    }

    pub(super) fn publish(self) -> Result<ExecutableMemory, JitError> {
        self.mapping.make_executable()?;
        Ok(ExecutableMemory {
            mapping: self.mapping,
        })
    }
}

#[derive(Debug)]
pub(super) struct ExecutableMemory {
    mapping: Mapping,
}

impl ExecutableMemory {
    pub(super) fn write_debug_bytes(&self, output: &mut String) {
        // SAFETY: publication made this live, exclusively owned mapping
        // readable/executable; requested_len bounds the initialized code.
        // Borrowing self keeps it mapped while the bytes are copied to text.
        let bytes = unsafe { std::slice::from_raw_parts(self.as_ptr(), self.requested_len()) };
        for (index, chunk) in bytes.chunks(16).enumerate() {
            write!(output, "{:08x}:", index * 16).expect("String formatting cannot fail");
            for byte in chunk {
                write!(output, " {byte:02x}").expect("String formatting cannot fail");
            }
            output.push('\n');
        }
        #[cfg(target_arch = "aarch64")]
        {
            output.push_str("aarch64 disassembly:\n");
            super::aarch64::disassemble(bytes, output);
        }
    }

    pub(super) fn as_ptr(&self) -> *const u8 {
        self.mapping.pointer.as_ptr()
    }

    pub(super) fn mapped_len(&self) -> usize {
        self.mapping.mapped_len
    }

    pub(super) fn requested_len(&self) -> usize {
        self.mapping.requested_len
    }
}

#[derive(Debug)]
struct Mapping {
    pointer: NonNull<u8>,
    requested_len: usize,
    mapped_len: usize,
    // Executable mappings are runtime-local and deliberately not transferable.
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl Mapping {
    #[cfg(all(
        any(target_arch = "x86_64", target_arch = "aarch64"),
        any(target_os = "linux", target_os = "macos")
    ))]
    fn allocate(requested_len: usize) -> Result<Self, JitError> {
        if requested_len == 0 {
            return Err(JitError::InvalidCodeSize);
        }
        // SAFETY: sysconf has no memory-safety preconditions.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size <= 0 {
            let os_error = io::Error::last_os_error();
            return Err(if os_error.raw_os_error().is_some_and(|code| code != 0) {
                os_error.into()
            } else {
                io::Error::other("sysconf(_SC_PAGESIZE) returned a non-positive value").into()
            });
        }
        let page_size = usize::try_from(page_size).map_err(|_| JitError::InvalidCodeSize)?;
        let mapped_len = requested_len
            .checked_add(page_size - 1)
            .ok_or(JitError::InvalidCodeSize)?
            / page_size
            * page_size;
        // Unix publication uses one-way page permissions. Apple ARM64 instead
        // requires MAP_JIT and per-thread W^X through an allowlisted callback.
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        let (protection, flags) = (
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
        );
        #[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
        let (protection, flags) = (
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
        );
        // SAFETY: parameters request a fresh anonymous private mapping. The
        // returned pointer is checked before constructing the owner.
        let pointer =
            unsafe { libc::mmap(std::ptr::null_mut(), mapped_len, protection, flags, -1, 0) };
        if pointer == libc::MAP_FAILED {
            return Err(io::Error::last_os_error().into());
        }
        let Some(pointer) = NonNull::new(pointer.cast::<u8>()) else {
            // SAFETY: even an unexpected null result still denotes the mapping
            // returned above and must be released before reporting failure.
            unsafe { libc::munmap(pointer, mapped_len) };
            return Err(io::Error::other("mmap returned null").into());
        };
        let mapping = Self {
            pointer,
            requested_len,
            mapped_len,
            _not_send_or_sync: PhantomData,
        };
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        if !apple::thread_protection_supported() {
            // Some ARM64 virtual machines have no per-thread JIT protection.
            // Their callback is a plain copy. Remove execute permission before
            // any generated bytes are written, then publish with mprotect.
            mapping.protect(libc::PROT_READ | libc::PROT_WRITE)?;
        }
        Ok(mapping)
    }

    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    fn protect(&self, protection: libc::c_int) -> Result<(), JitError> {
        // SAFETY: this exclusively owned mapping is live and page-aligned.
        if unsafe { libc::mprotect(self.pointer.as_ptr().cast(), self.mapped_len, protection) } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    #[cfg(not(all(
        any(target_arch = "x86_64", target_arch = "aarch64"),
        any(target_os = "linux", target_os = "macos")
    )))]
    fn allocate(_requested_len: usize) -> Result<Self, JitError> {
        Err(JitError::UnsupportedPlatform)
    }

    #[cfg(all(
        any(target_arch = "x86_64", target_arch = "aarch64"),
        any(target_os = "linux", target_os = "macos")
    ))]
    fn make_executable(&self) -> Result<(), JitError> {
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            // Hardware-supported callbacks already restored per-thread RX.
            // VM hosts without that capability need a real page transition.
            if !apple::thread_protection_supported() {
                self.protect(libc::PROT_READ | libc::PROT_EXEC)?;
            }
            // SAFETY: the mapping is live and requested_len bounds its bytes.
            unsafe {
                apple::sys_icache_invalidate(self.pointer.as_ptr().cast(), self.requested_len)
            };
            Ok(())
        }
        #[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
        {
            // SAFETY: pointer and length describe the live mapping owned by self.
            let result = unsafe {
                libc::mprotect(
                    self.pointer.as_ptr().cast(),
                    self.mapped_len,
                    libc::PROT_READ | libc::PROT_EXEC,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error().into());
            }
            #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
            // SAFETY: both endpoints delimit the live mapping, without dereferencing
            // the one-past-end pointer. libgcc handles cache line sizes and barriers.
            unsafe {
                clear_cache(
                    self.pointer.as_ptr().cast(),
                    self.pointer.as_ptr().add(self.requested_len).cast(),
                );
            }
            // x86_64 has coherent instruction/data caches, so no explicit cache
            // invalidation is required after the permission transition.
            Ok(())
        }
    }

    #[cfg(not(all(
        any(target_arch = "x86_64", target_arch = "aarch64"),
        any(target_os = "linux", target_os = "macos")
    )))]
    fn make_executable(&self) -> Result<(), JitError> {
        Err(JitError::UnsupportedPlatform)
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        #[cfg(all(
            any(target_arch = "x86_64", target_arch = "aarch64"),
            any(target_os = "linux", target_os = "macos")
        ))]
        {
            // SAFETY: this owner drops exactly once and stores the original
            // mapping base and rounded length.
            let result = unsafe { libc::munmap(self.pointer.as_ptr().cast(), self.mapped_len) };
            debug_assert_eq!(result, 0, "munmap failed: {}", io::Error::last_os_error());
        }
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
unsafe extern "C" {
    #[link_name = "__clear_cache"]
    fn clear_cache(start: *mut libc::c_void, end: *mut libc::c_void);
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
mod apple {
    use super::{JitError, Mapping, io};

    pub(super) fn thread_protection_supported() -> bool {
        // SAFETY: this capability query takes no arguments or memory access.
        unsafe { libc::pthread_jit_write_protect_supported_np() != 0 }
    }

    struct WriteRequest<'a> {
        mapping: &'a Mapping,
        offset: usize,
        bytes: &'a [u8],
    }

    // This is Apple's PTHREAD_JIT_WRITE_ALLOW_CALLBACKS_NP layout. The null
    // terminator is required. The callback never allocates or invokes JIT code.
    #[used]
    #[unsafe(link_section = "__DATA_CONST,__pth_jit_func")]
    static CALLBACKS: [libc::pthread_jit_write_callback_t; 2] = [Some(write_callback), None];

    extern "C" fn write_callback(context: *mut libc::c_void) -> libc::c_int {
        if context.is_null() {
            return libc::EINVAL;
        }
        // SAFETY: only write() invokes this callback, synchronously, with a
        // live WriteRequest. Its borrow keeps the unique mapping and bytes live.
        let request = unsafe { &*context.cast::<WriteRequest<'_>>() };
        let Some(end) = request.offset.checked_add(request.bytes.len()) else {
            return libc::EINVAL;
        };
        if end > request.mapping.requested_len {
            return libc::EINVAL;
        }
        // SAFETY: requested bounds were rechecked in the write-enabled callback;
        // MAP_JIT makes these pages writable and non-executable on this thread.
        unsafe {
            std::ptr::copy_nonoverlapping(
                request.bytes.as_ptr(),
                request.mapping.pointer.as_ptr().add(request.offset),
                request.bytes.len(),
            );
        }
        0
    }

    pub(super) fn write(mapping: &Mapping, offset: usize, bytes: &[u8]) -> Result<(), JitError> {
        let mut request = WriteRequest {
            mapping,
            offset,
            bytes,
        };
        // Keep a real reference to the allowlist section even with linker dead
        // stripping/LTO. #[used] alone only guarantees compiler-level retention.
        // SAFETY: the immutable static contains this callback followed by null.
        let callback = unsafe { std::ptr::read_volatile(&raw const CALLBACKS[0]) };
        // SAFETY: the callback is statically allowlisted above. No Rust panic
        // or control-flow escape can bypass the OS restoring execute permission.
        let result = unsafe {
            libc::pthread_jit_write_with_callback_np(callback, (&raw mut request).cast())
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result).into());
        }
        Ok(())
    }

    unsafe extern "C" {
        pub(super) fn sys_icache_invalidate(start: *mut libc::c_void, length: usize);
    }
}

#[cfg(all(
    test,
    any(target_arch = "x86_64", target_arch = "aarch64"),
    any(target_os = "linux", target_os = "macos")
))]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;

    use crate::jit::FixedReturnCode;

    #[cfg(target_os = "macos")]
    fn assert_mapped(pointer: *const u8, expected: bool) {
        // Darwin mincore reports residency, including zero for holes. Query
        // the actual VM region before and after Drop instead. BASIC_INFO_64
        // returns the next region if the supplied address is no longer mapped.
        unsafe extern "C" {
            static mach_task_self_: u32;
            fn mach_vm_region(
                task: u32,
                address: *mut u64,
                size: *mut u64,
                flavor: i32,
                info: *mut i32,
                count: *mut u32,
                object: *mut u32,
            ) -> i32;
            fn mach_port_deallocate(task: u32, name: u32) -> i32;
        }
        let original = pointer as u64;
        let mut address = original;
        let mut size = 0;
        let mut info = [0_i32; 32];
        let mut count = info.len() as u32;
        let mut object = 0;
        // SAFETY: the system initializes the current-task port at startup. All
        // output buffers are live; info has ample space for BASIC_INFO_64.
        let result = unsafe {
            mach_vm_region(
                mach_task_self_,
                &raw mut address,
                &raw mut size,
                9,
                info.as_mut_ptr(),
                &raw mut count,
                &raw mut object,
            )
        };
        if object != 0 {
            // SAFETY: release the send right returned by this query, if any.
            assert_eq!(unsafe { mach_port_deallocate(mach_task_self_, object) }, 0);
        }
        if result == 1 {
            // KERN_INVALID_ADDRESS: there is no mapping at or after this address.
            assert!(!expected, "live code must have a VM region");
            return;
        }
        assert_eq!(result, 0, "mach_vm_region failed");
        assert_eq!(
            address <= original && original < address.checked_add(size).unwrap(),
            expected,
            "code mapping presence must follow its owner lifetime"
        );
    }

    #[test]
    #[allow(clippy::print_stderr)]
    fn publication_permissions_and_unmap_are_enforced() {
        const CHILD_MODE: &str = "BOA_TEST_CODE_MEMORY_CHILD";
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        eprintln!(
            "macOS per-thread JIT protection supported: {}",
            super::apple::thread_protection_supported()
        );
        if let Some(mode) = std::env::var_os(CHILD_MODE) {
            let limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: lowering the child's core-file limit needs no privilege;
            // the expected permission fault must not leave a large core file.
            assert_eq!(
                unsafe { libc::setrlimit(libc::RLIMIT_CORE, &raw const limit) },
                0
            );
            let code = FixedReturnCode::compile(42).unwrap();
            assert_eq!(code.call(), 42);
            let pointer = code.memory.as_ptr();
            #[cfg(target_os = "linux")]
            let length = code.memory.mapped_len();
            eprintln!("published code executed successfully");
            if mode == "write" {
                // This isolated child intentionally attempts a forbidden write
                // after proving the mapping is live and executable. Its parent
                // requires the OS to reject the write with SIGSEGV or SIGBUS.
                unsafe { std::ptr::write_volatile(pointer.cast_mut(), 0) };
                panic!("published code was writable");
            }
            assert_eq!(mode, "unmap");
            #[cfg(target_os = "macos")]
            assert_mapped(pointer, true);
            drop(code);
            #[cfg(target_os = "macos")]
            assert_mapped(pointer, false);
            #[cfg(target_os = "linux")]
            {
                // No other tests execute in this child, so another code allocator
                // cannot reuse the just-released range before mincore observes it.
                let mut residency = 0_u8;
                // SAFETY: mincore queries address-space metadata; it does not
                // dereference the retired pointer. This code object was one page.
                let result = unsafe {
                    libc::mincore(
                        pointer.cast_mut().cast(),
                        length,
                        (&raw mut residency).cast(),
                    )
                };
                assert_eq!(result, -1, "retired code pages must be unmapped");
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ENOMEM)
                );
            }
            return;
        }

        let name = "jit::platform::tests::publication_permissions_and_unmap_are_enforced";
        for mode in ["write", "unmap"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--nocapture"])
                .env(CHILD_MODE, mode)
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("published code executed successfully"),
                "{stderr}"
            );
            if mode == "write" {
                assert!(
                    matches!(output.status.signal(), Some(libc::SIGSEGV | libc::SIGBUS)),
                    "{stderr}"
                );
            } else {
                assert!(output.status.success(), "{stderr}");
            }
        }
    }
}
