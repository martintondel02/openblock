//! Silent output stub for platforms without a native backend in 1.0.
//!
//! Windows and macOS still expose the complete [`AudioEngine`] API, but
//! [`AudioEngine::new`] reports [`AudioError::BackendDisabled`]. The caller
//! treats that as "run silently": synthesis is available through
//! [`AudioEngine::headless`] and every gameplay call remains a no-op. No system
//! audio library is linked or loaded on these platforms.
//!
//! [`AudioEngine`]: crate::AudioEngine
//! [`AudioEngine::new`]: crate::AudioEngine::new
//! [`AudioEngine::headless`]: crate::AudioEngine::headless
//! [`AudioError::BackendDisabled`]: crate::AudioError::BackendDisabled

use crate::AudioError;

/// Why this platform has no real-time output in the 1.0 slice.
pub(crate) fn error() -> AudioError {
    AudioError::BackendDisabled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_reports_backend_disabled_and_does_not_link_a_device() {
        assert_eq!(error(), AudioError::BackendDisabled);
        assert_eq!(
            error().to_string(),
            "no real-time audio backend on this platform (silent stub)"
        );
    }
}
