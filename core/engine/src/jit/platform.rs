use std::{io, marker::PhantomData, ptr::NonNull, rc::Rc};

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
    #[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
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
        // No mapping is writable and executable at the same time: bytes are
        // emitted into RW pages and publication performs a one-way RX transition.
        // SAFETY: parameters request a fresh anonymous private mapping. The
        // returned pointer is checked before constructing the owner.
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                mapped_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if pointer == libc::MAP_FAILED {
            return Err(io::Error::last_os_error().into());
        }
        let Some(pointer) = NonNull::new(pointer.cast::<u8>()) else {
            // SAFETY: even an unexpected null result still denotes the mapping
            // returned above and must be released before reporting failure.
            unsafe { libc::munmap(pointer, mapped_len) };
            return Err(io::Error::other("mmap returned null").into());
        };
        Ok(Self {
            pointer,
            requested_len,
            mapped_len,
            _not_send_or_sync: PhantomData,
        })
    }

    #[cfg(not(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos"))))]
    fn allocate(_requested_len: usize) -> Result<Self, JitError> {
        Err(JitError::UnsupportedPlatform)
    }

    #[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
    fn make_executable(&self) -> Result<(), JitError> {
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
        // x86_64 has coherent instruction/data caches, so no explicit cache
        // invalidation is required after the permission transition.
        Ok(())
    }

    #[cfg(not(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos"))))]
    fn make_executable(&self) -> Result<(), JitError> {
        Err(JitError::UnsupportedPlatform)
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        #[cfg(all(target_arch = "x86_64", any(target_os = "linux", target_os = "macos")))]
        {
            // SAFETY: this owner drops exactly once and stores the original
            // mapping base and rounded length.
            let result = unsafe { libc::munmap(self.pointer.as_ptr().cast(), self.mapped_len) };
            debug_assert_eq!(result, 0, "munmap failed: {}", io::Error::last_os_error());
        }
    }
}
