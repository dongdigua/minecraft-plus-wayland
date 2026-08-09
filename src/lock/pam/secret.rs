// Derived in part from sudo-rs 0.2.14 src/pam/securemem.rs under the MIT license.
// See THIRD_PARTY_NOTICES.md and sudo-rs-LICENSE-MIT.

use std::{fmt, io, ptr::NonNull, str};

use zeroize::Zeroize;

pub const MAX_PASSWORD_BYTES: usize = 511;

const MLOCK_EAGAIN_RETRIES: usize = 5;

/// A fixed-capacity password buffer backed by a dedicated anonymous mapping.
///
/// The type intentionally does not implement `Clone`, `Debug`, or `Display`.
/// Moving it transfers ownership of the mapping without copying its contents.
pub struct LockedSecret {
    mapping: NonNull<u8>,
    mapping_len: usize,
    len: usize,
    locked: bool,
}

/// Failures that can occur while allocating or editing an application-owned secret.
#[derive(Debug)]
pub enum SecretError {
    PageSizeUnavailable,
    Mapping(io::Error),
    DontDump(io::Error),
    Lock(io::Error),
    TooLong,
    ContainsNul,
}

impl fmt::Display for SecretError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PageSizeUnavailable => formatter.write_str("system page size is unavailable"),
            Self::Mapping(error) => write!(formatter, "failed to allocate secret memory: {error}"),
            Self::DontDump(error) => {
                write!(
                    formatter,
                    "failed to exclude secret memory from core dumps: {error}"
                )
            }
            Self::Lock(error) => write!(formatter, "failed to lock secret memory: {error}"),
            Self::TooLong => formatter.write_str("password exceeds the configured byte limit"),
            Self::ContainsNul => formatter.write_str("password input contains a NUL byte"),
        }
    }
}

impl std::error::Error for SecretError {}

impl LockedSecret {
    pub fn new() -> Result<Self, SecretError> {
        let page_size = page_size().ok_or(SecretError::PageSizeUnavailable)?;
        if page_size < MAX_PASSWORD_BYTES + 1 {
            return Err(SecretError::PageSizeUnavailable);
        }

        // SAFETY: the arguments request a new private anonymous read/write mapping.
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                page_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        let Some(mapping) = NonNull::new(raw.cast::<u8>()).filter(|_| raw != libc::MAP_FAILED)
        else {
            return Err(SecretError::Mapping(io::Error::last_os_error()));
        };

        // SAFETY: `mapping` references the full live mapping created above.
        if unsafe { libc::madvise(mapping.as_ptr().cast(), page_size, libc::MADV_DONTDUMP) } != 0 {
            let error = io::Error::last_os_error();
            // SAFETY: `mapping` still denotes the mapping returned by mmap.
            unsafe {
                libc::munmap(mapping.as_ptr().cast(), page_size);
            }
            return Err(SecretError::DontDump(error));
        }

        let locked = match lock_mapping(mapping, page_size) {
            Ok(locked) => locked,
            Err(error) => {
                // The mapping has never held a password, but clear it before releasing it so this
                // failure path keeps the same ownership discipline as all later paths.
                // SAFETY: `mapping` references `page_size` initialized bytes.
                unsafe {
                    std::slice::from_raw_parts_mut(mapping.as_ptr(), page_size).zeroize();
                    libc::munmap(mapping.as_ptr().cast(), page_size);
                }
                return Err(SecretError::Lock(error));
            }
        };

        Ok(Self {
            mapping,
            mapping_len: page_size,
            len: 0,
            locked,
        })
    }

    pub fn append(&mut self, text: &str) -> Result<(), SecretError> {
        let bytes = text.as_bytes();
        if bytes.contains(&0) {
            return Err(SecretError::ContainsNul);
        }
        let new_len = self
            .len
            .checked_add(bytes.len())
            .ok_or(SecretError::TooLong)?;
        if new_len > MAX_PASSWORD_BYTES {
            return Err(SecretError::TooLong);
        }

        // SAFETY: the capacity check above proves the destination lies within the mapping, and
        // `text` cannot alias this private mapping through the safe API.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.mapping.as_ptr().add(self.len),
                bytes.len(),
            );
            *self.mapping.as_ptr().add(new_len) = 0;
        }
        self.len = new_len;
        Ok(())
    }

    pub fn delete_last_scalar(&mut self) -> bool {
        if self.len == 0 {
            return false;
        }
        let current = str::from_utf8(self.as_bytes())
            .expect("LockedSecret only accepts valid UTF-8 through append");
        let scalar_len = current
            .chars()
            .next_back()
            .expect("a non-empty UTF-8 string contains a scalar")
            .len_utf8();
        let new_len = self.len - scalar_len;

        // Clear the removed scalar immediately rather than waiting for a later full-buffer clear.
        // SAFETY: `new_len..self.len` is within the initialized mapping.
        unsafe {
            std::slice::from_raw_parts_mut(self.mapping.as_ptr().add(new_len), scalar_len)
                .zeroize();
            *self.mapping.as_ptr().add(new_len) = 0;
        }
        self.len = new_len;
        true
    }

    pub fn clear(&mut self) {
        // Clear the full mapping, not only the logical password length.
        // SAFETY: `mapping` owns `mapping_len` initialized bytes for this object's lifetime.
        unsafe {
            std::slice::from_raw_parts_mut(self.mapping.as_ptr(), self.mapping_len).zeroize();
        }
        self.len = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        // SAFETY: `len` is maintained at or below MAX_PASSWORD_BYTES, which fits in the mapping.
        unsafe { std::slice::from_raw_parts(self.mapping.as_ptr(), self.len) }
    }
}

// SAFETY: `LockedSecret` uniquely owns its mapping, has no thread-affine OS state, and exposes no
// shared references to the mapping that can outlive a borrow. Moving it to the single PAM worker
// transfers ownership without copying the password. It is deliberately not `Sync`.
unsafe impl Send for LockedSecret {}

impl Drop for LockedSecret {
    fn drop(&mut self) {
        self.clear();
        if self.locked {
            // SAFETY: the mapping is still live and was successfully locked by this object.
            if unsafe { libc::munlock(self.mapping.as_ptr().cast(), self.mapping_len) } != 0 {
                log::warn!(
                    target: "minecraft_plus_wayland::auth",
                    "failed to unlock cleared password memory: {}",
                    io::Error::last_os_error(),
                );
            }
        }
        // SAFETY: this object uniquely owns the live mapping returned by mmap.
        if unsafe { libc::munmap(self.mapping.as_ptr().cast(), self.mapping_len) } != 0 {
            log::warn!(
                target: "minecraft_plus_wayland::auth",
                "failed to release cleared password memory: {}",
                io::Error::last_os_error(),
            );
        }
    }
}

/// Proof that process core dumps were disabled before constructing the PAM backend.
///
/// The value is intentionally non-cloneable and can only be created by a successful
/// [`disable_process_dumps`] call.
#[must_use]
pub struct ProcessDumpProtection {
    _private: (),
}

/// Disable process core dumps before requesting a session lock.
pub fn disable_process_dumps() -> io::Result<ProcessDumpProtection> {
    // SAFETY: PR_SET_DUMPABLE accepts an integer value and no pointer arguments.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } == 0 {
        Ok(ProcessDumpProtection { _private: () })
    } else {
        Err(io::Error::last_os_error())
    }
}

fn page_size() -> Option<usize> {
    // SAFETY: sysconf has no pointer arguments and `_SC_PAGESIZE` is a valid query.
    let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(value).ok().filter(|value| *value > 0)
}

fn lock_mapping(mapping: NonNull<u8>, len: usize) -> io::Result<bool> {
    let mut eagain_failures = 0;
    loop {
        // SAFETY: `mapping` denotes a live mapping of `len` bytes.
        if unsafe { libc::mlock(mapping.as_ptr().cast(), len) } == 0 {
            return Ok(true);
        }

        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EPERM | libc::ENOSYS) => {
                log::warn!(
                    target: "minecraft_plus_wayland::auth",
                    "password memory locking is unavailable; continuing without mlock: {error}",
                );
                return Ok(false);
            }
            Some(libc::EAGAIN) if eagain_failures + 1 < MLOCK_EAGAIN_RETRIES => {
                eagain_failures += 1;
            }
            _ => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_utf8_without_reallocation() {
        let mut secret = LockedSecret::new().expect("secret allocation should succeed");
        secret.append("a界").unwrap();
        assert_eq!(secret.len(), 4);
        assert!(secret.delete_last_scalar());
        assert_eq!(secret.as_bytes(), b"a");
        assert!(secret.delete_last_scalar());
        assert!(secret.is_empty());
        assert!(!secret.delete_last_scalar());
    }

    #[test]
    fn rejects_nul_and_over_limit_without_partial_append() {
        let mut secret = LockedSecret::new().expect("secret allocation should succeed");
        assert!(matches!(
            secret.append("a\0b"),
            Err(SecretError::ContainsNul)
        ));
        assert!(secret.is_empty());

        let maximum = "x".repeat(MAX_PASSWORD_BYTES);
        secret.append(&maximum).unwrap();
        assert_eq!(secret.len(), MAX_PASSWORD_BYTES);
        secret.clear();

        let oversized = "x".repeat(MAX_PASSWORD_BYTES + 1);
        assert!(matches!(
            secret.append(&oversized),
            Err(SecretError::TooLong)
        ));
        assert!(secret.is_empty());
    }

    #[test]
    fn clear_wipes_the_full_mapping() {
        let mut secret = LockedSecret::new().expect("secret allocation should succeed");
        secret.append("sensitive").unwrap();
        secret.clear();
        assert!(secret.is_empty());
        // SAFETY: the test holds the live owner of this mapping.
        let mapping =
            unsafe { std::slice::from_raw_parts(secret.mapping.as_ptr(), secret.mapping_len) };
        assert!(mapping.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn secret_can_move_to_a_worker_thread() {
        let mut secret = LockedSecret::new().expect("secret allocation should succeed");
        secret.append("password").unwrap();
        std::thread::spawn(move || {
            assert_eq!(secret.as_bytes(), b"password");
        })
        .join()
        .unwrap();
    }
}
