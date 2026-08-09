// Derived in part from sudo-rs 0.2.14 src/pam/converse.rs and securemem.rs under
// the MIT license. See THIRD_PARTY_NOTICES.md and sudo-rs-LICENSE-MIT.

use std::ptr::NonNull;

use zeroize::Zeroize;

use super::secret::LockedSecret;

use super::ffi::{PAM_MAX_RESP_SIZE, PamResponse};

#[cfg(test)]
thread_local! {
    static ALLOCATIONS_BEFORE_FAILURE: std::cell::Cell<Option<usize>> = const {
        std::cell::Cell::new(None)
    };
}

pub(super) struct ResponseArray {
    pointer: NonNull<PamResponse>,
    len: usize,
}

impl ResponseArray {
    pub(super) fn allocate(len: usize) -> Option<Self> {
        // SAFETY: pam_calloc either returns a suitably aligned, zeroed allocation or null.
        let pointer = unsafe { pam_calloc(len, std::mem::size_of::<PamResponse>()) };
        NonNull::new(pointer.cast()).map(|pointer| Self { pointer, len })
    }

    pub(super) fn install_password(
        &mut self,
        index: usize,
        secret: &mut LockedSecret,
    ) -> Result<(), ()> {
        if index >= self.len {
            return Err(());
        }

        // Allocate the traditional Linux-PAM response capacity. The application payload is capped
        // at 511 bytes, leaving a zero terminator in this calloc-initialized allocation.
        // SAFETY: pam_calloc either returns a zeroed allocation or null.
        let raw = unsafe { pam_calloc(PAM_MAX_RESP_SIZE, 1) };
        let Some(password) = NonNull::new(raw.cast::<u8>()) else {
            return Err(());
        };

        let bytes = secret.as_bytes();
        debug_assert!(bytes.len() < PAM_MAX_RESP_SIZE);
        // SAFETY: the destination owns PAM_MAX_RESP_SIZE bytes, and LockedSecret enforces the
        // payload limit. The two independent allocations cannot overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), password.as_ptr(), bytes.len());
            let response = self.pointer.as_ptr().add(index);
            (*response).resp = password.as_ptr().cast();
            (*response).resp_retcode = 0;
        }
        secret.clear();
        Ok(())
    }

    pub(super) fn into_raw(self) -> *mut PamResponse {
        let pointer = self.pointer.as_ptr();
        std::mem::forget(self);
        pointer
    }
}

impl Drop for ResponseArray {
    fn drop(&mut self) {
        for index in 0..self.len {
            // SAFETY: every index is within the calloc allocation owned by this object.
            let response = unsafe { &mut *self.pointer.as_ptr().add(index) };
            if let Some(password) = NonNull::new(response.resp.cast::<u8>()) {
                // Every non-null response installed by this builder owns exactly
                // PAM_MAX_RESP_SIZE bytes and has not yet been handed to PAM.
                // SAFETY: the pointer came from this type's fixed-size calloc allocation.
                unsafe {
                    std::slice::from_raw_parts_mut(password.as_ptr(), PAM_MAX_RESP_SIZE).zeroize();
                    libc::free(password.as_ptr().cast());
                }
                response.resp = std::ptr::null_mut();
            }
        }
        // SAFETY: `pointer` came from calloc and ownership has not been transferred.
        unsafe {
            libc::free(self.pointer.as_ptr().cast());
        }
    }
}

unsafe fn pam_calloc(count: usize, size: usize) -> *mut libc::c_void {
    #[cfg(test)]
    {
        let fail = ALLOCATIONS_BEFORE_FAILURE.with(|remaining| match remaining.get() {
            Some(0) => {
                remaining.set(None);
                true
            }
            Some(value) => {
                remaining.set(Some(value - 1));
                false
            }
            None => false,
        });
        if fail {
            return std::ptr::null_mut();
        }
    }

    // SAFETY: forwarded to C calloc; callers check for a null result before use.
    unsafe { libc::calloc(count, size) }
}

#[cfg(test)]
pub(super) fn fail_allocation_after(successful_allocations: usize) {
    ALLOCATIONS_BEFORE_FAILURE.with(|remaining| remaining.set(Some(successful_allocations)));
}

#[cfg(test)]
pub(super) unsafe fn wipe_and_free_pam_responses(pointer: *mut PamResponse, len: usize) {
    if pointer.is_null() {
        return;
    }
    for index in 0..len {
        // SAFETY: the test fake calls this with the array returned by the conversation callback.
        let response = unsafe { &mut *pointer.add(index) };
        if !response.resp.is_null() {
            // SAFETY: the callback allocates every non-null response with this fixed capacity.
            unsafe {
                std::slice::from_raw_parts_mut(response.resp.cast::<u8>(), PAM_MAX_RESP_SIZE)
                    .zeroize();
                libc::free(response.resp.cast());
            }
        }
    }
    // SAFETY: the response array was allocated with calloc by ResponseArray.
    unsafe {
        libc::free(pointer.cast());
    }
}
