// Derived in part from sudo-rs 0.2.14 src/pam/converse.rs under the MIT license.
// See THIRD_PARTY_NOTICES.md and licenses/sudo-rs-LICENSE-MIT.

use std::{
    ffi::{c_int, c_void},
    marker::PhantomPinned,
    panic::{AssertUnwindSafe, catch_unwind},
};

use crate::lock::secret::LockedSecret;

use super::{
    ffi::{
        PAM_BUF_ERR, PAM_CONV_ERR, PAM_ERROR_MSG, PAM_MAX_NUM_MSG, PAM_PROMPT_ECHO_OFF,
        PAM_PROMPT_ECHO_ON, PAM_SUCCESS, PAM_TEXT_INFO, PamMessage, PamResponse,
    },
    response::ResponseArray,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CallbackFailure {
    Protocol,
    Resource,
    Panicked,
}

pub(super) struct ConversationData {
    password: Option<LockedSecret>,
    password_consumed: bool,
    failure: Option<CallbackFailure>,
    #[cfg(test)]
    panic_for_test: bool,
    _pin: PhantomPinned,
}

impl ConversationData {
    pub(super) fn new(password: LockedSecret) -> Self {
        Self {
            password: Some(password),
            password_consumed: false,
            failure: None,
            #[cfg(test)]
            panic_for_test: false,
            _pin: PhantomPinned,
        }
    }

    pub(super) fn failure(&self) -> Option<CallbackFailure> {
        self.failure
    }

    fn record_failure(&mut self, failure: CallbackFailure) -> c_int {
        self.failure.get_or_insert(failure);
        match failure {
            CallbackFailure::Resource => PAM_BUF_ERR,
            CallbackFailure::Protocol | CallbackFailure::Panicked => PAM_CONV_ERR,
        }
    }
}

/// Linux-PAM conversation callback for one preloaded password.
///
/// The callback intentionally never reads PAM message text. One ECHO_OFF prompt is allowed for the
/// entire transaction; ECHO_ON and all unknown styles fail closed.
///
/// # Safety
///
/// The caller must uphold the Linux-PAM conversation ABI: non-null pointers must designate the
/// arrays/objects described by `num_msg`, `response` must be writable, and `appdata_ptr` must point
/// to the pinned `ConversationData` retained by the active transaction.
pub(super) unsafe extern "C" fn converse(
    num_msg: c_int,
    messages: *mut *const PamMessage,
    response: *mut *mut PamResponse,
    appdata_ptr: *mut c_void,
) -> c_int {
    if response.is_null() {
        return PAM_CONV_ERR;
    }
    // SAFETY: the function contract requires a writable response slot when it is non-null.
    unsafe {
        *response = std::ptr::null_mut();
    }
    if appdata_ptr.is_null() {
        return PAM_CONV_ERR;
    }

    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the callback contract ties appdata_ptr to the pinned transaction-owned value,
        // and PAM invokes the callback synchronously on the transaction's worker thread.
        let data = unsafe { &mut *appdata_ptr.cast::<ConversationData>() };
        if data.failure.is_some() {
            return PAM_CONV_ERR;
        }

        #[cfg(test)]
        if data.panic_for_test {
            panic!("injected conversation panic");
        }

        let Ok(message_count) = usize::try_from(num_msg) else {
            return data.record_failure(CallbackFailure::Protocol);
        };
        if !(1..=PAM_MAX_NUM_MSG).contains(&message_count) || messages.is_null() {
            return data.record_failure(CallbackFailure::Protocol);
        }

        let mut password_index = None;
        for index in 0..message_count {
            // SAFETY: Linux-PAM supplies an array with message_count entries.
            let message_ptr = unsafe { *messages.add(index) };
            // SAFETY: a non-null entry in the PAM-provided pointer array designates a readable
            // PamMessage for the duration of this synchronous callback.
            let Some(message) = (unsafe { message_ptr.as_ref() }) else {
                return data.record_failure(CallbackFailure::Protocol);
            };
            match message.msg_style {
                PAM_PROMPT_ECHO_OFF => {
                    if password_index.is_some() || data.password_consumed || data.password.is_none()
                    {
                        return data.record_failure(CallbackFailure::Protocol);
                    }
                    password_index = Some(index);
                }
                PAM_ERROR_MSG | PAM_TEXT_INFO => {}
                PAM_PROMPT_ECHO_ON => {
                    return data.record_failure(CallbackFailure::Protocol);
                }
                _ => return data.record_failure(CallbackFailure::Protocol),
            }
        }

        let Some(mut responses) = ResponseArray::allocate(message_count) else {
            return data.record_failure(CallbackFailure::Resource);
        };
        if let Some(index) = password_index {
            // Validation of the complete batch finished before consuming the password.
            let mut password = data
                .password
                .take()
                .expect("validated one-shot password must still be present");
            data.password_consumed = true;
            if responses.install_password(index, &mut password).is_err() {
                return data.record_failure(CallbackFailure::Resource);
            }
            // `install_password` cleared the application-owned mapping; dropping it now unmaps the
            // empty page while the PAM-owned response follows the C ownership contract.
            drop(password);
        }

        let raw_responses = responses.into_raw();
        // SAFETY: the ABI provides a writable response slot, and ownership of the C allocations is
        // transferred to PAM by this assignment.
        unsafe {
            *response = raw_responses;
        }
        PAM_SUCCESS
    }));

    match result {
        Ok(code) => code,
        Err(_) => {
            // SAFETY: non-null appdata_ptr is guaranteed by the check above and by the callback
            // contract. No secret or PAM message is included in the recorded failure.
            let data = unsafe { &mut *appdata_ptr.cast::<ConversationData>() };
            data.record_failure(CallbackFailure::Panicked)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::auth::pam::{
        ffi::PamResponse,
        response::{fail_allocation_after, wipe_and_free_pam_responses},
    };

    fn password(value: &str) -> LockedSecret {
        let mut secret = LockedSecret::new().unwrap();
        secret.append(value).unwrap();
        secret
    }

    fn call(data: &mut ConversationData, styles: &[c_int]) -> (c_int, *mut PamResponse) {
        let messages = styles
            .iter()
            .map(|style| PamMessage {
                msg_style: *style,
                msg: std::ptr::null(),
            })
            .collect::<Vec<_>>();
        let mut pointers = messages
            .iter()
            .map(|message| message as *const _)
            .collect::<Vec<_>>();
        let mut responses = std::ptr::null_mut();
        // SAFETY: all pointers refer to live test-owned objects for the duration of the call.
        let code = unsafe {
            converse(
                i32::try_from(pointers.len()).unwrap(),
                pointers.as_mut_ptr(),
                &mut responses,
                std::ptr::from_mut(data).cast(),
            )
        };
        (code, responses)
    }

    #[test]
    fn accepts_one_hidden_prompt_and_silent_messages() {
        let mut data = ConversationData::new(password("secret"));
        let styles = [PAM_TEXT_INFO, PAM_PROMPT_ECHO_OFF, PAM_ERROR_MSG];
        let (code, responses) = call(&mut data, &styles);
        assert_eq!(code, PAM_SUCCESS);
        assert!(!responses.is_null());
        // SAFETY: the callback returned an array with one entry per requested style.
        unsafe {
            assert!((*responses.add(0)).resp.is_null());
            assert_eq!(
                std::ffi::CStr::from_ptr((*responses.add(1)).resp).to_bytes(),
                b"secret"
            );
            assert!((*responses.add(2)).resp.is_null());
            wipe_and_free_pam_responses(responses, styles.len());
        }
        assert_eq!(data.failure(), None);
    }

    #[test]
    fn accepts_an_empty_password() {
        let mut data = ConversationData::new(password(""));
        let (code, responses) = call(&mut data, &[PAM_PROMPT_ECHO_OFF]);
        assert_eq!(code, PAM_SUCCESS);
        // SAFETY: the successful callback installed one NUL-terminated response.
        unsafe {
            assert!(
                std::ffi::CStr::from_ptr((*responses).resp)
                    .to_bytes()
                    .is_empty()
            );
            wipe_and_free_pam_responses(responses, 1);
        }
    }

    #[test]
    fn rejects_echo_on_without_consuming_a_response() {
        let mut data = ConversationData::new(password("secret"));
        let (code, responses) = call(&mut data, &[PAM_PROMPT_ECHO_ON]);
        assert_eq!(code, PAM_CONV_ERR);
        assert!(responses.is_null());
        assert_eq!(data.failure(), Some(CallbackFailure::Protocol));
    }

    #[test]
    fn validates_the_whole_batch_before_consuming_password() {
        let mut data = ConversationData::new(password("secret"));
        let (code, responses) = call(&mut data, &[PAM_PROMPT_ECHO_OFF, PAM_PROMPT_ECHO_OFF]);
        assert_eq!(code, PAM_CONV_ERR);
        assert!(responses.is_null());
        assert!(data.password.is_some());
    }

    #[test]
    fn rejects_a_second_prompt_across_callbacks() {
        let mut data = ConversationData::new(password("secret"));
        let (code, responses) = call(&mut data, &[PAM_PROMPT_ECHO_OFF]);
        assert_eq!(code, PAM_SUCCESS);
        // SAFETY: this is the response returned by the first successful callback.
        unsafe { wipe_and_free_pam_responses(responses, 1) };

        let (code, responses) = call(&mut data, &[PAM_PROMPT_ECHO_OFF]);
        assert_eq!(code, PAM_CONV_ERR);
        assert!(responses.is_null());
    }

    #[test]
    fn rejects_invalid_counts_and_null_elements() {
        for count in [i32::MIN, -1, 0, 33, i32::MAX] {
            let mut data = ConversationData::new(password("secret"));
            let mut responses = std::ptr::dangling_mut::<PamResponse>();
            // SAFETY: invalid top-level inputs are intentionally supplied and rejected before
            // message-array access.
            let code = unsafe {
                converse(
                    count,
                    std::ptr::null_mut(),
                    &mut responses,
                    std::ptr::from_mut(&mut data).cast(),
                )
            };
            assert_eq!(code, PAM_CONV_ERR);
            assert!(responses.is_null());
        }

        let mut data = ConversationData::new(password("secret"));
        let mut responses = std::ptr::dangling_mut::<PamResponse>();
        let mut null_message = std::ptr::null();
        // SAFETY: the one array entry is readable but null and must be rejected.
        let code = unsafe {
            converse(
                1,
                &mut null_message,
                &mut responses,
                std::ptr::from_mut(&mut data).cast(),
            )
        };
        assert_eq!(code, PAM_CONV_ERR);
    }

    #[test]
    fn rejects_null_response_or_appdata_slots() {
        let message = PamMessage {
            msg_style: PAM_PROMPT_ECHO_OFF,
            msg: std::ptr::null(),
        };
        let mut message_pointer = &message as *const _;
        // SAFETY: a null response slot is rejected before it is written.
        let code = unsafe {
            converse(
                1,
                &mut message_pointer,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(code, PAM_CONV_ERR);

        let mut responses = std::ptr::dangling_mut::<PamResponse>();
        // SAFETY: null appdata is rejected after clearing the valid response slot.
        let code = unsafe {
            converse(
                1,
                &mut message_pointer,
                &mut responses,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(code, PAM_CONV_ERR);
        assert!(responses.is_null());
    }

    #[test]
    fn reports_response_allocation_failures_without_handing_off_memory() {
        for successful_allocations in [0, 1] {
            let mut data = ConversationData::new(password("secret"));
            fail_allocation_after(successful_allocations);
            let (code, responses) = call(&mut data, &[PAM_PROMPT_ECHO_OFF]);
            assert_eq!(code, PAM_BUF_ERR);
            assert!(responses.is_null());
            assert_eq!(data.failure(), Some(CallbackFailure::Resource));
        }
    }

    #[test]
    fn catches_callback_panics() {
        let mut data = ConversationData::new(password("secret"));
        data.panic_for_test = true;
        let (code, responses) = call(&mut data, &[PAM_PROMPT_ECHO_OFF]);
        assert_eq!(code, PAM_CONV_ERR);
        assert!(responses.is_null());
        assert_eq!(data.failure(), Some(CallbackFailure::Panicked));
    }
}
