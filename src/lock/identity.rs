use std::{ffi::CString, fmt, mem::MaybeUninit};

const DEFAULT_NSS_BUFFER: usize = 16 * 1024;
const MAX_NSS_BUFFER: usize = 1024 * 1024;
const MAX_USERNAME_BYTES: usize = 4096;

pub struct TrustedIdentity {
    uid: libc::uid_t,
    username: CString,
}

impl TrustedIdentity {
    /// Resolve the authentication identity from the real UID, never environment variables.
    pub fn discover() -> Result<Self, IdentityError> {
        // SAFETY: the ID accessors have no pointer arguments or memory-safety preconditions.
        let (uid, effective_uid, gid, effective_gid) = unsafe {
            (
                libc::getuid(),
                libc::geteuid(),
                libc::getgid(),
                libc::getegid(),
            )
        };
        validate_process_ids(uid, effective_uid, gid, effective_gid)?;
        let username = lookup_username(uid)?;
        Ok(Self { uid, username })
    }

    pub fn into_username(self) -> CString {
        self.username
    }

    #[allow(dead_code)]
    pub fn uid(&self) -> libc::uid_t {
        self.uid
    }
}

#[derive(Debug)]
pub enum IdentityError {
    CredentialMismatch,
    BufferLimit,
    Lookup(i32),
    UserNotFound,
    InvalidRecord,
}

impl fmt::Display for IdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CredentialMismatch => {
                formatter.write_str("real and effective process credentials differ")
            }
            Self::BufferLimit => formatter.write_str("NSS passwd lookup exceeded its buffer limit"),
            Self::Lookup(code) => write!(formatter, "NSS passwd lookup failed with code {code}"),
            Self::UserNotFound => formatter.write_str("real UID has no passwd record"),
            Self::InvalidRecord => formatter.write_str("NSS returned an invalid passwd record"),
        }
    }
}

impl std::error::Error for IdentityError {}

fn validate_process_ids(
    uid: libc::uid_t,
    effective_uid: libc::uid_t,
    gid: libc::gid_t,
    effective_gid: libc::gid_t,
) -> Result<(), IdentityError> {
    if uid != effective_uid || gid != effective_gid {
        Err(IdentityError::CredentialMismatch)
    } else {
        Ok(())
    }
}

fn lookup_username(uid: libc::uid_t) -> Result<CString, IdentityError> {
    // SAFETY: sysconf has no pointer arguments. A negative/implausible suggestion uses a bound.
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut size = if suggested > 0 {
        usize::try_from(suggested)
            .unwrap_or(DEFAULT_NSS_BUFFER)
            .clamp(1024, MAX_NSS_BUFFER)
    } else {
        DEFAULT_NSS_BUFFER
    };

    loop {
        let mut buffer = vec![0_u8; size];
        let mut record = MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: all output pointers and the scratch buffer are valid for this call.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                record.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE {
            if size >= MAX_NSS_BUFFER {
                return Err(IdentityError::BufferLimit);
            }
            size = size.saturating_mul(2).min(MAX_NSS_BUFFER);
            continue;
        }
        if status != 0 {
            return Err(IdentityError::Lookup(status));
        }
        if result.is_null() {
            return Err(IdentityError::UserNotFound);
        }

        // SAFETY: a successful getpwuid_r with non-null result initialized the record.
        let record = unsafe { record.assume_init() };
        if record.pw_uid != uid || record.pw_name.is_null() {
            return Err(IdentityError::InvalidRecord);
        }
        // SAFETY: getpwuid_r guarantees pw_name is a NUL-terminated passwd field. strnlen bounds
        // the application copy and rejects implausibly long names.
        let name_len = unsafe { libc::strnlen(record.pw_name, MAX_USERNAME_BYTES + 1) };
        if name_len == 0 || name_len > MAX_USERNAME_BYTES {
            return Err(IdentityError::InvalidRecord);
        }
        // SAFETY: strnlen established that these bytes are readable before the terminating NUL.
        let bytes = unsafe { std::slice::from_raw_parts(record.pw_name.cast::<u8>(), name_len) };
        return CString::new(bytes).map_err(|_| IdentityError::InvalidRecord);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_uid_or_gid_mismatch() {
        assert!(validate_process_ids(1000, 1001, 1000, 1000).is_err());
        assert!(validate_process_ids(1000, 1000, 1000, 1001).is_err());
        assert!(validate_process_ids(1000, 1000, 1000, 1000).is_ok());
    }
}
