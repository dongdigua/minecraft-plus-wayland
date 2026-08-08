#[cfg(not(target_os = "linux"))]
compile_error!("the session-lock authentication implementation requires Linux-PAM");

#[cfg(target_os = "linux")]
pub mod animations;
#[cfg(target_os = "linux")]
pub mod auth;
#[cfg(target_os = "linux")]
pub mod identity;
#[cfg(target_os = "linux")]
pub mod secret;
#[cfg(target_os = "linux")]
pub mod state;
#[cfg(target_os = "linux")]
pub mod worker;
