// Derived in part from sudo-rs 0.2.14 src/pam/error.rs under the MIT license.
// See THIRD_PARTY_NOTICES.md and sudo-rs-LICENSE-MIT.

use std::fmt;

use super::ffi;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PamDenial {
    Credentials,
    AccountPolicy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PamErrorStage {
    Start,
    Conversation,
    Authenticate,
    UserCheck,
    Account,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PamErrorKind {
    BackendUnavailable,
    ConversationProtocol,
    ResourceFailure,
    Internal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PamError {
    stage: PamErrorStage,
    kind: PamErrorKind,
    code: Option<i32>,
}

impl PamError {
    pub fn stage(self) -> PamErrorStage {
        self.stage
    }

    pub fn kind(self) -> PamErrorKind {
        self.kind
    }

    pub fn code(self) -> Option<i32> {
        self.code
    }

    pub(super) const fn new(stage: PamErrorStage, kind: PamErrorKind, code: Option<i32>) -> Self {
        Self { stage, kind, code }
    }

    pub(super) const fn from_code(stage: PamErrorStage, code: i32) -> Self {
        Self::new(stage, classify_error_code(code), Some(code))
    }
}

impl fmt::Display for PamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "PAM {:?} failure in {:?} (code {:?})",
            self.kind, self.stage, self.code
        )
    }
}

impl std::error::Error for PamError {}

pub(super) const fn classify_authenticate(
    code: i32,
) -> Result<(), Result<PamDenial, PamErrorKind>> {
    match code {
        ffi::PAM_SUCCESS => Ok(()),
        ffi::PAM_AUTH_ERR | ffi::PAM_USER_UNKNOWN | ffi::PAM_MAXTRIES => {
            Err(Ok(PamDenial::Credentials))
        }
        _ => Err(Err(classify_error_code(code))),
    }
}

pub(super) const fn classify_account(code: i32) -> Result<(), Result<PamDenial, PamErrorKind>> {
    match code {
        ffi::PAM_SUCCESS => Ok(()),
        ffi::PAM_PERM_DENIED
        | ffi::PAM_AUTH_ERR
        | ffi::PAM_USER_UNKNOWN
        | ffi::PAM_MAXTRIES
        | ffi::PAM_NEW_AUTHTOK_REQD
        | ffi::PAM_ACCT_EXPIRED
        | ffi::PAM_CRED_EXPIRED
        | ffi::PAM_AUTHTOK_EXPIRED => Err(Ok(PamDenial::AccountPolicy)),
        _ => Err(Err(classify_error_code(code))),
    }
}

const fn classify_error_code(code: i32) -> PamErrorKind {
    match code {
        ffi::PAM_OPEN_ERR
        | ffi::PAM_SYMBOL_ERR
        | ffi::PAM_SERVICE_ERR
        | ffi::PAM_SYSTEM_ERR
        | ffi::PAM_CRED_INSUFFICIENT
        | ffi::PAM_AUTHINFO_UNAVAIL
        | ffi::PAM_CRED_UNAVAIL
        | ffi::PAM_MODULE_UNKNOWN => PamErrorKind::BackendUnavailable,
        ffi::PAM_CONV_ERR | ffi::PAM_CONV_AGAIN | ffi::PAM_INCOMPLETE => {
            PamErrorKind::ConversationProtocol
        }
        ffi::PAM_BUF_ERR => PamErrorKind::ResourceFailure,
        ffi::PAM_SESSION_ERR
        | ffi::PAM_CRED_EXPIRED
        | ffi::PAM_CRED_ERR
        | ffi::PAM_NO_MODULE_DATA
        | ffi::PAM_AUTHTOK_ERR
        | ffi::PAM_AUTHTOK_RECOVERY_ERR
        | ffi::PAM_AUTHTOK_LOCK_BUSY
        | ffi::PAM_AUTHTOK_DISABLE_AGING
        | ffi::PAM_TRY_AGAIN
        | ffi::PAM_IGNORE
        | ffi::PAM_ABORT
        | ffi::PAM_AUTHTOK_EXPIRED
        | ffi::PAM_BAD_ITEM => PamErrorKind::Internal,
        _ => PamErrorKind::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_denials_are_not_backend_errors() {
        for code in [ffi::PAM_AUTH_ERR, ffi::PAM_USER_UNKNOWN, ffi::PAM_MAXTRIES] {
            assert_eq!(classify_authenticate(code), Err(Ok(PamDenial::Credentials)));
        }
    }

    #[test]
    fn account_policy_denials_are_grouped() {
        for code in [
            ffi::PAM_PERM_DENIED,
            ffi::PAM_AUTH_ERR,
            ffi::PAM_USER_UNKNOWN,
            ffi::PAM_MAXTRIES,
            ffi::PAM_NEW_AUTHTOK_REQD,
            ffi::PAM_ACCT_EXPIRED,
            ffi::PAM_CRED_EXPIRED,
            ffi::PAM_AUTHTOK_EXPIRED,
        ] {
            assert_eq!(classify_account(code), Err(Ok(PamDenial::AccountPolicy)));
        }
    }

    #[test]
    fn explicit_non_denial_codes_have_stable_categories() {
        for code in [
            ffi::PAM_OPEN_ERR,
            ffi::PAM_SYMBOL_ERR,
            ffi::PAM_SERVICE_ERR,
            ffi::PAM_SYSTEM_ERR,
            ffi::PAM_CRED_INSUFFICIENT,
            ffi::PAM_AUTHINFO_UNAVAIL,
            ffi::PAM_CRED_UNAVAIL,
            ffi::PAM_MODULE_UNKNOWN,
        ] {
            assert_eq!(classify_error_code(code), PamErrorKind::BackendUnavailable);
        }
        for code in [ffi::PAM_CONV_ERR, ffi::PAM_CONV_AGAIN, ffi::PAM_INCOMPLETE] {
            assert_eq!(
                classify_error_code(code),
                PamErrorKind::ConversationProtocol
            );
        }
        assert_eq!(
            classify_error_code(ffi::PAM_BUF_ERR),
            PamErrorKind::ResourceFailure
        );
        for code in [
            ffi::PAM_SESSION_ERR,
            ffi::PAM_CRED_EXPIRED,
            ffi::PAM_CRED_ERR,
            ffi::PAM_NO_MODULE_DATA,
            ffi::PAM_AUTHTOK_ERR,
            ffi::PAM_AUTHTOK_RECOVERY_ERR,
            ffi::PAM_AUTHTOK_LOCK_BUSY,
            ffi::PAM_AUTHTOK_DISABLE_AGING,
            ffi::PAM_TRY_AGAIN,
            ffi::PAM_IGNORE,
            ffi::PAM_ABORT,
            ffi::PAM_AUTHTOK_EXPIRED,
            ffi::PAM_BAD_ITEM,
        ] {
            assert_eq!(classify_error_code(code), PamErrorKind::Internal);
        }
    }

    #[test]
    fn unknown_codes_fail_internal() {
        assert_eq!(
            classify_authenticate(12345),
            Err(Err(PamErrorKind::Internal))
        );
        assert_eq!(classify_account(-1), Err(Err(PamErrorKind::Internal)));
    }
}
