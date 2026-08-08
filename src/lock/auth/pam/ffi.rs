// Derived from sudo-rs 0.2.14 src/pam/sys_linuxpam.rs under the MIT license.
// The source bindings were generated from Linux-PAM headers. This file intentionally keeps only
// the ABI required by the lock-screen authentication transaction.
// See THIRD_PARTY_NOTICES.md and licenses/sudo-rs-LICENSE-MIT.

use std::ffi::{c_char, c_int, c_void};

#[repr(C)]
pub struct PamHandle {
    _private: [u8; 0],
}

#[repr(C)]
pub struct PamMessage {
    pub msg_style: c_int,
    pub msg: *const c_char,
}

#[repr(C)]
pub struct PamResponse {
    pub resp: *mut c_char,
    pub resp_retcode: c_int,
}

pub type ConversationFn = unsafe extern "C" fn(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    response: *mut *mut PamResponse,
    appdata_ptr: *mut c_void,
) -> c_int;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PamConv {
    pub conv: Option<ConversationFn>,
    pub appdata_ptr: *mut c_void,
}

pub const PAM_SUCCESS: c_int = 0;
pub const PAM_OPEN_ERR: c_int = 1;
pub const PAM_SYMBOL_ERR: c_int = 2;
pub const PAM_SERVICE_ERR: c_int = 3;
pub const PAM_SYSTEM_ERR: c_int = 4;
pub const PAM_BUF_ERR: c_int = 5;
pub const PAM_PERM_DENIED: c_int = 6;
pub const PAM_AUTH_ERR: c_int = 7;
pub const PAM_CRED_INSUFFICIENT: c_int = 8;
pub const PAM_AUTHINFO_UNAVAIL: c_int = 9;
pub const PAM_USER_UNKNOWN: c_int = 10;
pub const PAM_MAXTRIES: c_int = 11;
pub const PAM_NEW_AUTHTOK_REQD: c_int = 12;
pub const PAM_ACCT_EXPIRED: c_int = 13;
pub const PAM_SESSION_ERR: c_int = 14;
pub const PAM_CRED_UNAVAIL: c_int = 15;
pub const PAM_CRED_EXPIRED: c_int = 16;
pub const PAM_CRED_ERR: c_int = 17;
pub const PAM_NO_MODULE_DATA: c_int = 18;
pub const PAM_CONV_ERR: c_int = 19;
pub const PAM_AUTHTOK_ERR: c_int = 20;
pub const PAM_AUTHTOK_RECOVERY_ERR: c_int = 21;
pub const PAM_AUTHTOK_LOCK_BUSY: c_int = 22;
pub const PAM_AUTHTOK_DISABLE_AGING: c_int = 23;
pub const PAM_TRY_AGAIN: c_int = 24;
pub const PAM_IGNORE: c_int = 25;
pub const PAM_ABORT: c_int = 26;
pub const PAM_AUTHTOK_EXPIRED: c_int = 27;
pub const PAM_MODULE_UNKNOWN: c_int = 28;
pub const PAM_BAD_ITEM: c_int = 29;
pub const PAM_CONV_AGAIN: c_int = 30;
pub const PAM_INCOMPLETE: c_int = 31;

pub const PAM_USER: c_int = 2;

pub const PAM_PROMPT_ECHO_OFF: c_int = 1;
pub const PAM_PROMPT_ECHO_ON: c_int = 2;
pub const PAM_ERROR_MSG: c_int = 3;
pub const PAM_TEXT_INFO: c_int = 4;

pub const PAM_MAX_NUM_MSG: usize = 32;
pub const PAM_MAX_RESP_SIZE: usize = 512;

#[link(name = "pam")]
unsafe extern "C" {
    pub fn pam_start(
        service_name: *const c_char,
        user: *const c_char,
        pam_conversation: *const PamConv,
        pamh: *mut *mut PamHandle,
    ) -> c_int;
    pub fn pam_end(pamh: *mut PamHandle, pam_status: c_int) -> c_int;
    pub fn pam_authenticate(pamh: *mut PamHandle, flags: c_int) -> c_int;
    pub fn pam_acct_mgmt(pamh: *mut PamHandle, flags: c_int) -> c_int;
    pub fn pam_get_item(
        pamh: *const PamHandle,
        item_type: c_int,
        item: *mut *const c_void,
    ) -> c_int;
}

pub trait PamApi {
    unsafe fn start(
        &self,
        service_name: *const c_char,
        user: *const c_char,
        conversation: *const PamConv,
        handle: *mut *mut PamHandle,
    ) -> c_int;

    unsafe fn end(&self, handle: *mut PamHandle, status: c_int) -> c_int;

    unsafe fn authenticate(&self, handle: *mut PamHandle, flags: c_int) -> c_int;

    unsafe fn account_management(&self, handle: *mut PamHandle, flags: c_int) -> c_int;

    unsafe fn get_item(
        &self,
        handle: *const PamHandle,
        item_type: c_int,
        item: *mut *const c_void,
    ) -> c_int;
}

pub struct SystemPam;

impl PamApi for SystemPam {
    unsafe fn start(
        &self,
        service_name: *const c_char,
        user: *const c_char,
        conversation: *const PamConv,
        handle: *mut *mut PamHandle,
    ) -> c_int {
        // SAFETY: forwarded from the PamApi caller with the same contract.
        unsafe { pam_start(service_name, user, conversation, handle) }
    }

    unsafe fn end(&self, handle: *mut PamHandle, status: c_int) -> c_int {
        // SAFETY: forwarded from the PamApi caller with the same contract.
        unsafe { pam_end(handle, status) }
    }

    unsafe fn authenticate(&self, handle: *mut PamHandle, flags: c_int) -> c_int {
        // SAFETY: forwarded from the PamApi caller with the same contract.
        unsafe { pam_authenticate(handle, flags) }
    }

    unsafe fn account_management(&self, handle: *mut PamHandle, flags: c_int) -> c_int {
        // SAFETY: forwarded from the PamApi caller with the same contract.
        unsafe { pam_acct_mgmt(handle, flags) }
    }

    unsafe fn get_item(
        &self,
        handle: *const PamHandle,
        item_type: c_int,
        item: *mut *const c_void,
    ) -> c_int {
        // SAFETY: forwarded from the PamApi caller with the same contract.
        unsafe { pam_get_item(handle, item_type, item) }
    }
}
