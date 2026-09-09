//! Windows capture, audio, encoding, input, and service integration.

#[cfg(any(windows, test))]
pub mod audio;
#[cfg(any(windows, test))]
pub mod audio_encode;
#[cfg(any(windows, test))]
mod audio_policy;
#[cfg(windows)]
mod audio_stream;
#[cfg(windows)]
pub mod capture;
#[cfg(any(windows, test))]
mod capture_recovery;
#[cfg(any(windows, test))]
mod clock;
#[cfg(windows)]
mod credentials;
#[cfg(windows)]
mod desktop;
#[cfg(any(windows, test))]
mod display_mode_proof;
pub mod firewall;
#[cfg(windows)]
mod gpu_encode;
#[cfg(windows)]
mod interactive_worker;
pub mod service;
#[cfg(any(windows, test))]
mod transition_proof;
#[cfg(windows)]
mod wgc_helper;
#[cfg(windows)]
mod worker;
#[cfg(any(windows, test))]
mod worker_protocol;
