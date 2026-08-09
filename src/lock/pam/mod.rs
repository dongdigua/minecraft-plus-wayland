// Derived in part from sudo-rs 0.2.14 src/pam under the MIT license.
// See THIRD_PARTY_NOTICES.md and sudo-rs-LICENSE-MIT.

#![deny(unsafe_op_in_unsafe_fn)]

mod conversation;
mod error;
mod ffi;
mod response;
mod secret;

use std::{
    ffi::{CStr, c_void},
    marker::PhantomData,
    pin::Pin,
    ptr::NonNull,
    rc::Rc,
};

use conversation::{CallbackFailure, ConversationData};
pub use error::{PamDenial, PamError, PamErrorKind, PamErrorStage};
use error::{classify_account, classify_authenticate};
use ffi::{PAM_SUCCESS, PAM_USER, PamApi, PamConv, PamHandle, SystemPam};
pub use secret::{LockedSecret, ProcessDumpProtection, SecretError, disable_process_dumps};

const LOGIN_SERVICE: &CStr = c"login";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PamOutcome {
    Authenticated,
    Denied(PamDenial),
}

pub struct PamAuthenticator {
    _dump_protection: ProcessDumpProtection,
}

impl PamAuthenticator {
    /// Construct the PAM backend only after process-wide core dumps were disabled.
    pub fn new(dump_protection: ProcessDumpProtection) -> Self {
        Self {
            _dump_protection: dump_protection,
        }
    }

    /// Perform one blocking Linux-PAM authentication transaction.
    ///
    /// The caller must supply the username obtained from the trusted real-UID lookup and transfer
    /// ownership of the application password buffer. This function is intended to run inside the
    /// single authentication worker, never the Wayland event thread. The constructor's required
    /// proof token ensures process dump protection was enabled before PAM can own a password copy.
    pub fn authenticate(
        &self,
        expected_user: &CStr,
        password: LockedSecret,
    ) -> Result<PamOutcome, PamError> {
        authenticate_with(&SystemPam, expected_user, password)
    }
}

fn authenticate_with<A: PamApi>(
    api: &A,
    expected_user: &CStr,
    password: LockedSecret,
) -> Result<PamOutcome, PamError> {
    if expected_user.to_bytes().is_empty() {
        return Err(PamError::new(
            PamErrorStage::Start,
            PamErrorKind::Internal,
            None,
        ));
    }

    let mut transaction = PamTransaction::start(api, expected_user, password)?;
    let candidate = transaction.run(expected_user);
    let finish = transaction.finish();

    match finish {
        Ok(()) => candidate,
        Err(error) => Err(error),
    }
}

struct PamTransaction<'api, A: PamApi> {
    api: &'api A,
    handle: Option<NonNull<PamHandle>>,
    conversation: Pin<Box<ConversationData>>,
    last_status: i32,
    // Explicitly keep a PAM transaction thread-local even if pointer auto-traits change later.
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl<'api, A: PamApi> PamTransaction<'api, A> {
    fn start(api: &'api A, expected_user: &CStr, password: LockedSecret) -> Result<Self, PamError> {
        let mut conversation = Box::pin(ConversationData::new(password));
        // SAFETY: the pinned Box remains owned by the transaction through pam_end, so this address
        // is stable for the complete PAM transaction.
        let appdata_ptr = unsafe {
            Pin::as_mut(&mut conversation).get_unchecked_mut() as *mut ConversationData
                as *mut c_void
        };
        let pam_conversation = PamConv {
            conv: Some(conversation::converse),
            appdata_ptr,
        };
        let mut raw_handle = std::ptr::null_mut();
        // SAFETY: all C strings and the conversation descriptor remain live for pam_start; PAM
        // copies the service/user items and retains only the callback/appdata values whose pointee
        // is pinned above.
        let status = unsafe {
            api.start(
                LOGIN_SERVICE.as_ptr(),
                expected_user.as_ptr(),
                &pam_conversation,
                &mut raw_handle,
            )
        };
        if status != PAM_SUCCESS {
            return Err(PamError::from_code(PamErrorStage::Start, status));
        }
        let Some(handle) = NonNull::new(raw_handle) else {
            return Err(PamError::new(
                PamErrorStage::Start,
                PamErrorKind::Internal,
                Some(status),
            ));
        };

        Ok(Self {
            api,
            handle: Some(handle),
            conversation,
            last_status: status,
            _not_send_or_sync: PhantomData,
        })
    }

    fn run(&mut self, expected_user: &CStr) -> Result<PamOutcome, PamError> {
        let auth_status = self.call_authenticate();
        if let Some(error) = self.callback_error() {
            return Err(error);
        }
        match classify_authenticate(auth_status) {
            Ok(()) => {}
            Err(Ok(denial)) => return Ok(PamOutcome::Denied(denial)),
            Err(Err(kind)) => {
                return Err(PamError::new(
                    PamErrorStage::Authenticate,
                    kind,
                    Some(auth_status),
                ));
            }
        }

        self.check_user(expected_user)?;

        let account_status = self.call_account_management();
        if let Some(error) = self.callback_error() {
            return Err(error);
        }
        match classify_account(account_status) {
            Ok(()) => {}
            Err(Ok(denial)) => return Ok(PamOutcome::Denied(denial)),
            Err(Err(kind)) => {
                return Err(PamError::new(
                    PamErrorStage::Account,
                    kind,
                    Some(account_status),
                ));
            }
        }

        self.check_user(expected_user)?;
        Ok(PamOutcome::Authenticated)
    }

    fn call_authenticate(&mut self) -> i32 {
        let handle = self.handle.expect("active PAM transaction has a handle");
        // SAFETY: the handle is live, belongs to this transaction, and is used synchronously on
        // one worker thread. Empty authentication tokens are intentionally allowed, so flags are 0.
        let status = unsafe { self.api.authenticate(handle.as_ptr(), 0) };
        self.last_status = status;
        status
    }

    fn call_account_management(&mut self) -> i32 {
        let handle = self.handle.expect("active PAM transaction has a handle");
        // SAFETY: the handle remains live and account management is the next synchronous PAM call.
        let status = unsafe { self.api.account_management(handle.as_ptr(), 0) };
        self.last_status = status;
        status
    }

    fn check_user(&mut self, expected_user: &CStr) -> Result<(), PamError> {
        let handle = self.handle.expect("active PAM transaction has a handle");
        let mut item = std::ptr::null();
        // SAFETY: the handle is live and `item` is a writable output slot for pam_get_item.
        let status = unsafe { self.api.get_item(handle.as_ptr(), PAM_USER, &mut item) };
        self.last_status = status;
        if status != PAM_SUCCESS || item.is_null() {
            return Err(PamError::new(
                PamErrorStage::UserCheck,
                PamErrorKind::Internal,
                Some(status),
            ));
        }

        // SAFETY: a successful PAM_USER lookup returns a PAM-owned NUL-terminated string valid
        // until a later PAM call. It is compared immediately without allocation.
        let actual_user = unsafe { CStr::from_ptr(item.cast()) };
        if actual_user.to_bytes() != expected_user.to_bytes() {
            return Err(PamError::new(
                PamErrorStage::UserCheck,
                PamErrorKind::Internal,
                Some(status),
            ));
        }
        Ok(())
    }

    fn callback_error(&self) -> Option<PamError> {
        match self.conversation.as_ref().get_ref().failure()? {
            CallbackFailure::Protocol => Some(PamError::new(
                PamErrorStage::Conversation,
                PamErrorKind::ConversationProtocol,
                None,
            )),
            CallbackFailure::Resource => Some(PamError::new(
                PamErrorStage::Conversation,
                PamErrorKind::ResourceFailure,
                None,
            )),
            CallbackFailure::Panicked => Some(PamError::new(
                PamErrorStage::Conversation,
                PamErrorKind::Internal,
                None,
            )),
        }
    }

    fn finish(mut self) -> Result<(), PamError> {
        let handle = self
            .handle
            .take()
            .expect("active PAM transaction has a handle");
        // Marking the handle consumed before calling pam_end prevents Drop from retrying: PAM
        // invalidates the handle regardless of the return code.
        // SAFETY: the handle is valid and this is its sole pam_end call. ConversationData remains
        // pinned and live until this method returns and self is dropped.
        let status = unsafe { self.api.end(handle.as_ptr(), self.last_status) };
        if status != PAM_SUCCESS {
            return Err(PamError::new(
                PamErrorStage::End,
                PamErrorKind::Internal,
                Some(status),
            ));
        }
        // A PAM module cleanup hook may converse during pam_end. Such a protocol/resource/panic
        // failure must override a candidate authentication success even if pam_end returned success.
        if let Some(error) = self.callback_error() {
            return Err(error);
        }
        Ok(())
    }
}

impl<A: PamApi> Drop for PamTransaction<'_, A> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            // SAFETY: this is the best-effort, at-most-once cleanup for an unwinding or otherwise
            // unfinished transaction. Drop can never turn this path into authentication success.
            unsafe {
                self.api.end(handle.as_ptr(), self.last_status);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        ffi::{CString, c_char},
    };

    use super::*;
    use crate::lock::pam::{
        ffi::{
            PAM_ACCT_EXPIRED, PAM_AUTH_ERR, PAM_CONV_ERR, PAM_PROMPT_ECHO_OFF, PamMessage,
            PamResponse,
        },
        response::{fail_allocation_after, wipe_and_free_pam_responses},
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Call {
        Start,
        Authenticate(i32),
        GetUser,
        Account(i32),
        End(i32),
    }

    struct FakePam {
        start_status: i32,
        auth_status: i32,
        account_status: i32,
        end_status: i32,
        start_null_handle: bool,
        auth_prompts: Vec<i32>,
        account_prompts: Vec<i32>,
        end_prompts: Vec<i32>,
        panic_in_authenticate: bool,
        users: Vec<CString>,
        user_statuses: Vec<i32>,
        user_index: Cell<usize>,
        conversation: Cell<Option<PamConv>>,
        calls: RefCell<Vec<Call>>,
        captured_passwords: RefCell<VecDeque<Vec<u8>>>,
    }

    impl FakePam {
        fn successful(user: &CStr) -> Self {
            Self {
                start_status: PAM_SUCCESS,
                auth_status: PAM_SUCCESS,
                account_status: PAM_SUCCESS,
                end_status: PAM_SUCCESS,
                start_null_handle: false,
                auth_prompts: vec![PAM_PROMPT_ECHO_OFF],
                account_prompts: Vec::new(),
                end_prompts: Vec::new(),
                panic_in_authenticate: false,
                users: vec![user.to_owned(), user.to_owned()],
                user_statuses: vec![PAM_SUCCESS, PAM_SUCCESS],
                user_index: Cell::new(0),
                conversation: Cell::new(None),
                calls: RefCell::new(Vec::new()),
                captured_passwords: RefCell::new(VecDeque::new()),
            }
        }

        fn invoke_conversation(&self, prompts: &[i32]) -> i32 {
            let Some(conversation) = self.conversation.get() else {
                return PAM_CONV_ERR;
            };
            if prompts.is_empty() {
                return PAM_SUCCESS;
            }
            let messages = prompts
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
            let mut responses = std::ptr::null_mut::<PamResponse>();
            // SAFETY: all callback arguments point to live fake-owned values for this call.
            let code = unsafe {
                conversation.conv.expect("conversation callback")(
                    i32::try_from(pointers.len()).unwrap(),
                    pointers.as_mut_ptr(),
                    &mut responses,
                    conversation.appdata_ptr,
                )
            };
            if code == PAM_SUCCESS {
                for (index, style) in prompts.iter().enumerate() {
                    // SAFETY: a successful callback returned one response per message.
                    let response = unsafe { &*responses.add(index) };
                    assert_eq!(response.resp_retcode, 0);
                    if *style == PAM_PROMPT_ECHO_OFF {
                        // SAFETY: ECHO_OFF success provides a NUL-terminated password response.
                        let bytes = unsafe { CStr::from_ptr(response.resp) }.to_bytes().to_vec();
                        self.captured_passwords.borrow_mut().push_back(bytes);
                    } else {
                        assert!(response.resp.is_null());
                    }
                }
                // SAFETY: simulate PAM taking ownership and freeing the successful response.
                unsafe { wipe_and_free_pam_responses(responses, prompts.len()) };
            } else {
                assert!(responses.is_null());
            }
            code
        }
    }

    impl PamApi for FakePam {
        unsafe fn start(
            &self,
            service_name: *const c_char,
            user: *const c_char,
            conversation: *const PamConv,
            handle: *mut *mut PamHandle,
        ) -> i32 {
            self.calls.borrow_mut().push(Call::Start);
            // SAFETY: the transaction supplies valid C strings and writable pointers.
            assert_eq!(unsafe { CStr::from_ptr(service_name) }, LOGIN_SERVICE);
            assert!(!unsafe { CStr::from_ptr(user) }.to_bytes().is_empty());
            // SAFETY: conversation points to the live descriptor passed by PamTransaction::start.
            self.conversation.set(Some(unsafe { *conversation }));
            if self.start_status == PAM_SUCCESS && !self.start_null_handle {
                // A non-null, never-dereferenced sentinel recognized only by this fake.
                // SAFETY: handle is a writable output slot.
                unsafe { *handle = NonNull::<u8>::dangling().as_ptr().cast() };
            }
            self.start_status
        }

        unsafe fn end(&self, _handle: *mut PamHandle, status: i32) -> i32 {
            self.calls.borrow_mut().push(Call::End(status));
            let _ = self.invoke_conversation(&self.end_prompts);
            self.end_status
        }

        unsafe fn authenticate(&self, _handle: *mut PamHandle, flags: i32) -> i32 {
            self.calls.borrow_mut().push(Call::Authenticate(flags));
            if self.panic_in_authenticate {
                panic!("injected PAM API panic");
            }
            let conversation_status = self.invoke_conversation(&self.auth_prompts);
            if conversation_status == PAM_SUCCESS {
                self.auth_status
            } else {
                PAM_AUTH_ERR
            }
        }

        unsafe fn account_management(&self, _handle: *mut PamHandle, flags: i32) -> i32 {
            self.calls.borrow_mut().push(Call::Account(flags));
            let conversation_status = self.invoke_conversation(&self.account_prompts);
            if conversation_status == PAM_SUCCESS {
                self.account_status
            } else {
                PAM_AUTH_ERR
            }
        }

        unsafe fn get_item(
            &self,
            _handle: *const PamHandle,
            item_type: i32,
            item: *mut *const c_void,
        ) -> i32 {
            self.calls.borrow_mut().push(Call::GetUser);
            assert_eq!(item_type, PAM_USER);
            let index = self.user_index.get();
            self.user_index.set(index + 1);
            let status = *self
                .user_statuses
                .get(index)
                .expect("scripted PAM user status");
            if status == PAM_SUCCESS {
                let user = self.users.get(index).expect("scripted PAM user");
                // SAFETY: item is a writable output slot; CString allocations stay alive in self.
                unsafe { *item = user.as_ptr().cast() };
            }
            status
        }
    }

    fn password(value: &str) -> LockedSecret {
        let mut secret = LockedSecret::new().unwrap();
        secret.append(value).unwrap();
        secret
    }

    #[test]
    fn successful_pipeline_finishes_before_returning_authenticated() {
        let user = c"alice";
        let fake = FakePam::successful(user);
        let outcome = authenticate_with(&fake, user, password("secret")).unwrap();
        assert_eq!(outcome, PamOutcome::Authenticated);
        assert_eq!(
            *fake.calls.borrow(),
            [
                Call::Start,
                Call::Authenticate(0),
                Call::GetUser,
                Call::Account(0),
                Call::GetUser,
                Call::End(PAM_SUCCESS),
            ]
        );
        assert_eq!(
            fake.captured_passwords.borrow_mut().pop_front().unwrap(),
            b"secret"
        );
    }

    #[test]
    fn accepts_an_empty_password() {
        let user = c"alice";
        let fake = FakePam::successful(user);
        assert_eq!(
            authenticate_with(&fake, user, password("")).unwrap(),
            PamOutcome::Authenticated
        );
        assert!(
            fake.captured_passwords
                .borrow_mut()
                .pop_front()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn authentication_denial_still_finishes_with_raw_status() {
        let user = c"alice";
        let mut fake = FakePam::successful(user);
        fake.auth_status = PAM_AUTH_ERR;
        let outcome = authenticate_with(&fake, user, password("wrong")).unwrap();
        assert_eq!(outcome, PamOutcome::Denied(PamDenial::Credentials));
        assert_eq!(fake.calls.borrow().last(), Some(&Call::End(PAM_AUTH_ERR)));
    }

    #[test]
    fn account_denial_does_not_change_authentication_token() {
        let user = c"alice";
        let mut fake = FakePam::successful(user);
        fake.account_status = PAM_ACCT_EXPIRED;
        let outcome = authenticate_with(&fake, user, password("secret")).unwrap();
        assert_eq!(outcome, PamOutcome::Denied(PamDenial::AccountPolicy));
        assert_eq!(
            fake.calls.borrow().last(),
            Some(&Call::End(PAM_ACCT_EXPIRED))
        );
    }

    #[test]
    fn pam_end_failure_overrides_success_or_denial() {
        let user = c"alice";
        for auth_status in [PAM_SUCCESS, PAM_AUTH_ERR] {
            let mut fake = FakePam::successful(user);
            fake.auth_status = auth_status;
            fake.end_status = ffi::PAM_SYSTEM_ERR;
            let error = authenticate_with(&fake, user, password("secret")).unwrap_err();
            assert_eq!(error.stage(), PamErrorStage::End);
            assert_eq!(error.kind(), PamErrorKind::Internal);
            assert_eq!(
                fake.calls
                    .borrow()
                    .iter()
                    .filter(|call| matches!(call, Call::End(_)))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn start_failure_or_null_success_handle_fails_closed() {
        let user = c"alice";
        let mut failed = FakePam::successful(user);
        failed.start_status = ffi::PAM_SERVICE_ERR;
        let error = authenticate_with(&failed, user, password("secret")).unwrap_err();
        assert_eq!(error.stage(), PamErrorStage::Start);
        assert!(
            !failed
                .calls
                .borrow()
                .iter()
                .any(|call| matches!(call, Call::End(_)))
        );

        let mut null = FakePam::successful(user);
        null.start_null_handle = true;
        let error = authenticate_with(&null, user, password("secret")).unwrap_err();
        assert_eq!(error.kind(), PamErrorKind::Internal);
        assert!(
            !null
                .calls
                .borrow()
                .iter()
                .any(|call| matches!(call, Call::End(_)))
        );
    }

    #[test]
    fn changed_user_fails_before_account_or_after_account() {
        let expected = c"alice";
        for users in [
            vec![CString::new("mallory").unwrap(), expected.to_owned()],
            vec![expected.to_owned(), CString::new("mallory").unwrap()],
        ] {
            let mut fake = FakePam::successful(expected);
            fake.users = users;
            let error = authenticate_with(&fake, expected, password("secret")).unwrap_err();
            assert_eq!(error.stage(), PamErrorStage::UserCheck);
            assert_eq!(fake.calls.borrow().last(), Some(&Call::End(PAM_SUCCESS)));
        }
    }

    #[test]
    fn get_user_failure_finishes_with_its_raw_status() {
        let user = c"alice";
        for failing_index in 0..2 {
            let mut fake = FakePam::successful(user);
            fake.user_statuses[failing_index] = ffi::PAM_SYSTEM_ERR;
            let error = authenticate_with(&fake, user, password("secret")).unwrap_err();
            assert_eq!(error.stage(), PamErrorStage::UserCheck);
            assert_eq!(
                fake.calls.borrow().last(),
                Some(&Call::End(ffi::PAM_SYSTEM_ERR))
            );
        }
    }

    #[test]
    fn callback_protocol_failure_is_not_downgraded_to_bad_password() {
        let user = c"alice";
        let mut fake = FakePam::successful(user);
        fake.auth_prompts = vec![ffi::PAM_PROMPT_ECHO_ON];
        let error = authenticate_with(&fake, user, password("secret")).unwrap_err();
        assert_eq!(error.stage(), PamErrorStage::Conversation);
        assert_eq!(error.kind(), PamErrorKind::ConversationProtocol);
    }

    #[test]
    fn account_or_end_conversation_cannot_request_another_password() {
        let user = c"alice";
        for during_end in [false, true] {
            let mut fake = FakePam::successful(user);
            if during_end {
                fake.end_prompts = vec![PAM_PROMPT_ECHO_OFF];
            } else {
                fake.account_prompts = vec![PAM_PROMPT_ECHO_OFF];
            }
            let error = authenticate_with(&fake, user, password("secret")).unwrap_err();
            assert_eq!(error.stage(), PamErrorStage::Conversation);
            assert_eq!(error.kind(), PamErrorKind::ConversationProtocol);
            assert_eq!(
                fake.calls
                    .borrow()
                    .iter()
                    .filter(|call| matches!(call, Call::End(_)))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn end_echo_on_failure_overrides_candidate_success() {
        let user = c"alice";
        let mut fake = FakePam::successful(user);
        fake.end_prompts = vec![ffi::PAM_PROMPT_ECHO_ON];
        let error = authenticate_with(&fake, user, password("secret")).unwrap_err();
        assert_eq!(error.stage(), PamErrorStage::Conversation);
        assert_eq!(error.kind(), PamErrorKind::ConversationProtocol);
        assert_eq!(fake.calls.borrow().last(), Some(&Call::End(PAM_SUCCESS)));
    }

    #[test]
    fn response_allocation_failure_remains_systemic_through_transaction() {
        let user = c"alice";
        for successful_allocations in [0, 1] {
            let fake = FakePam::successful(user);
            fail_allocation_after(successful_allocations);
            let error = authenticate_with(&fake, user, password("secret")).unwrap_err();
            assert_eq!(error.stage(), PamErrorStage::Conversation);
            assert_eq!(error.kind(), PamErrorKind::ResourceFailure);
            assert_eq!(fake.calls.borrow().last(), Some(&Call::End(PAM_AUTH_ERR)));
        }
    }

    #[test]
    fn unwind_uses_drop_to_end_the_transaction_once() {
        let user = c"alice";
        let mut fake = FakePam::successful(user);
        fake.panic_in_authenticate = true;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = authenticate_with(&fake, user, password("secret"));
        }));
        assert!(result.is_err());
        assert_eq!(
            fake.calls
                .borrow()
                .iter()
                .filter(|call| matches!(call, Call::End(_)))
                .count(),
            1
        );
        assert_eq!(fake.calls.borrow().last(), Some(&Call::End(PAM_SUCCESS)));
    }
}
