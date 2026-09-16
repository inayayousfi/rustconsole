//! Windows capture, audio, encoding, input, and service integration.

#[cfg(any(windows, test))]
pub mod audio;
#[cfg(any(windows, test))]
pub mod audio_encode;
#[cfg(any(windows, test))]
mod audio_policy;
#[cfg(windows)]
mod audio_stream;
#[cfg(any(windows, test))]
mod clock;
#[cfg(windows)]
mod credentials;
#[cfg(windows)]
mod desktop;
#[cfg(windows)]
pub mod display;
pub mod firewall;
#[cfg(windows)]
pub mod gpu_encode;
#[cfg(windows)]
mod installation;
#[cfg(windows)]
mod interactive_worker;
pub mod service;
#[cfg(windows)]
mod session_controls;
pub mod video_configuration;
#[cfg(windows)]
mod wgc_helper;
#[cfg(windows)]
mod worker;
#[cfg(any(windows, test))]
pub(crate) mod worker_protocol;
