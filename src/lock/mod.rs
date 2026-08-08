#[cfg(not(target_os = "linux"))]
compile_error!("the session-lock authentication implementation requires Linux-PAM");

#[cfg(target_os = "linux")]
pub mod auth;
#[cfg(target_os = "linux")]
pub mod secret;
