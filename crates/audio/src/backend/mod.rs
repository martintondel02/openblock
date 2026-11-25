//! Platform output backends.
//!
//! Linux uses the PulseAudio *simple* API, loaded at runtime with `dlopen` and
//! `dlsym`, so the crate builds without ALSA/PulseAudio development headers or
//! any build script. PipeWire's PulseAudio compatibility socket is enough at
//! runtime. Every other platform compiles the silent stub: [`AudioEngine::new`]
//! reports that no output backend is available and the client continues without
//! sound.
//!
//! [`AudioEngine::new`]: crate::AudioEngine::new

// The PulseAudio FFI and dynamic loader are the crate's only unsafe code. The
// module is exempted from the crate-level `deny(unsafe_code)`; every unsafe
// block carries a SAFETY comment and all FFI is confined to this file.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod pulse;
#[cfg(not(target_os = "linux"))]
pub(crate) mod stub;

#[cfg(target_os = "linux")]
pub(crate) use pulse::{Output, start};
