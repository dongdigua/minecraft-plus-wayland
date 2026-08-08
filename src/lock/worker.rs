use std::{
    ffi::{CStr, CString},
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::mpsc::{self, SyncSender, TrySendError},
    thread,
};

use smithay_client_toolkit::reexports::calloop::channel;

use super::{
    auth::pam::{PamAuthenticator, PamOutcome},
    secret::LockedSecret,
    state::{AttemptId, AuthDecision},
};

pub struct AuthRequest {
    pub attempt: AttemptId,
    pub password: LockedSecret,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthReply {
    pub attempt: AttemptId,
    pub decision: AuthDecision,
}

pub struct AuthWorker {
    requests: SyncSender<AuthRequest>,
}

impl AuthWorker {
    pub fn spawn_pam(
        username: CString,
        authenticator: PamAuthenticator,
        replies: channel::SyncSender<AuthReply>,
    ) -> io::Result<Self> {
        spawn_worker(username, authenticator, replies)
    }

    /// A full/disconnected queue is a systemic fault; the returned request still owns and wipes
    /// its password when dropped by the caller.
    pub fn try_authenticate(&self, request: AuthRequest) -> Result<(), TrySendError<AuthRequest>> {
        self.requests.try_send(request)
    }
}

trait Authenticator: Send + 'static {
    fn authenticate(&self, username: &CStr, password: LockedSecret) -> AuthDecision;
}

impl Authenticator for PamAuthenticator {
    fn authenticate(&self, username: &CStr, password: LockedSecret) -> AuthDecision {
        match PamAuthenticator::authenticate(self, username, password) {
            Ok(PamOutcome::Authenticated) => AuthDecision::Authenticated,
            Ok(PamOutcome::Denied(_)) => AuthDecision::Denied,
            Err(error) => {
                log::error!(
                    target: "minecraft_plus_wayland::auth",
                    "PAM systemic failure: stage={:?}, kind={:?}, code={:?}",
                    error.stage(),
                    error.kind(),
                    error.code(),
                );
                AuthDecision::SystemFailure
            }
        }
    }
}

fn spawn_worker<A: Authenticator>(
    username: CString,
    authenticator: A,
    replies: channel::SyncSender<AuthReply>,
) -> io::Result<AuthWorker> {
    let (requests, receiver) = mpsc::sync_channel::<AuthRequest>(1);
    thread::Builder::new()
        .name("minecraft-plus-pam".into())
        .spawn(move || {
            while let Ok(request) = receiver.recv() {
                let attempt = request.attempt;
                let result = catch_unwind(AssertUnwindSafe(|| {
                    authenticator.authenticate(&username, request.password)
                }));
                let decision = match result {
                    Ok(decision) => decision,
                    Err(_) => {
                        log::error!(
                            target: "minecraft_plus_wayland::auth",
                            "authentication worker panicked: attempt={attempt:?}",
                        );
                        let _ = replies.send(AuthReply {
                            attempt,
                            decision: AuthDecision::SystemFailure,
                        });
                        break;
                    }
                };
                if replies.send(AuthReply { attempt, decision }).is_err() {
                    break;
                }
            }
        })?;
    Ok(AuthWorker { requests })
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    struct FakeAuthenticator {
        calls: Arc<AtomicUsize>,
        decision: AuthDecision,
        panic: bool,
    }

    impl Authenticator for FakeAuthenticator {
        fn authenticate(&self, _username: &CStr, _password: LockedSecret) -> AuthDecision {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panic, "injected backend panic");
            self.decision
        }
    }

    fn password() -> LockedSecret {
        let mut password = LockedSecret::new().unwrap();
        password.append("secret").unwrap();
        password
    }

    #[test]
    fn worker_tags_one_coarse_result() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (reply_tx, reply_rx) = channel::sync_channel(1);
        let worker = spawn_worker(
            c"alice".to_owned(),
            FakeAuthenticator {
                calls: calls.clone(),
                decision: AuthDecision::Authenticated,
                panic: false,
            },
            reply_tx,
        )
        .unwrap();
        let attempt = AttemptId::new(7);
        assert!(
            worker
                .try_authenticate(AuthRequest {
                    attempt,
                    password: password(),
                })
                .is_ok()
        );
        assert_eq!(
            reply_rx.recv().unwrap(),
            AuthReply {
                attempt,
                decision: AuthDecision::Authenticated
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn backend_panic_becomes_system_failure_and_closes_worker() {
        let (reply_tx, reply_rx) = channel::sync_channel(1);
        let worker = spawn_worker(
            c"alice".to_owned(),
            FakeAuthenticator {
                calls: Arc::new(AtomicUsize::new(0)),
                decision: AuthDecision::Authenticated,
                panic: true,
            },
            reply_tx,
        )
        .unwrap();
        let attempt = AttemptId::new(8);
        assert!(
            worker
                .try_authenticate(AuthRequest {
                    attempt,
                    password: password(),
                })
                .is_ok()
        );
        assert_eq!(
            reply_rx.recv().unwrap(),
            AuthReply {
                attempt,
                decision: AuthDecision::SystemFailure
            }
        );
        assert!(reply_rx.recv().is_err());
    }
}
