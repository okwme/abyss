//! macOS Secure Event Input while the password field has focus: other
//! processes can no longer observe keystrokes through event taps.
//!
//! `EnableSecureEventInput` is reference counted per process, so each enable
//! must be paired with exactly one disable. [`SecureInputGuard`] makes the
//! pairing structural: one enable in `new`, one disable in `Drop`.

#[cfg(all(target_os = "macos", not(test)))]
mod sys {
    // Carbon's HIToolbox. Both calls take no arguments and return an `OSStatus`.
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn EnableSecureEventInput() -> i32;
        fn DisableSecureEventInput() -> i32;
    }

    pub(super) fn enable() {
        // SAFETY: a parameterless Carbon call with no preconditions; it only
        // increments the process's secure-input count.
        let status = unsafe { EnableSecureEventInput() };
        if status != 0 {
            tracing::warn!(status, "EnableSecureEventInput failed");
        }
    }

    pub(super) fn disable() {
        // SAFETY: a parameterless Carbon call; it is only reached from the
        // guard's `Drop`, so it is paired with the one successful `enable`.
        let status = unsafe { DisableSecureEventInput() };
        if status != 0 {
            tracing::warn!(status, "DisableSecureEventInput failed");
        }
    }
}

/// Tests count calls instead of toggling the real, system-wide state.
#[cfg(test)]
pub(crate) mod sys {
    use std::sync::atomic::{AtomicIsize, Ordering};

    pub(crate) static ACTIVE: AtomicIsize = AtomicIsize::new(0);

    pub(super) fn enable() {
        ACTIVE.fetch_add(1, Ordering::SeqCst);
    }

    pub(super) fn disable() {
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(all(not(target_os = "macos"), not(test)))]
mod sys {
    pub(super) fn enable() {}
    pub(super) fn disable() {}
}

/// Secure Event Input is on for as long as this guard lives.
#[derive(Debug)]
pub(crate) struct SecureInputGuard {
    _private: (),
}

impl SecureInputGuard {
    pub(crate) fn new() -> Self {
        sys::enable();
        Self { _private: () }
    }
}

impl Drop for SecureInputGuard {
    fn drop(&mut self) {
        sys::disable();
    }
}
