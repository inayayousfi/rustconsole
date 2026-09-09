//! Platform-neutral player video backend and overlay contracts.

use rustconsole_protocol::Av1HardwareCapability;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct OverlayStatistics {
    pub frames_per_second: f64,
    pub encoded_megabits_per_second: f64,
    pub round_trip_time: Duration,
    pub lost_chunks: u64,
    pub late_chunks: u64,
    pub assembly_overflows: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoConfiguration {
    pub width: u32,
    pub height: u32,
    pub frames_per_second: u16,
}

pub trait PlayerVideoBackend {
    type Error;

    fn decoder_capabilities(&self) -> &[Av1HardwareCapability];

    fn configure(&mut self, configuration: VideoConfiguration) -> Result<(), Self::Error>;

    fn submit_packet(
        &mut self,
        encoded_av1: &[u8],
        overlay: OverlayStatistics,
    ) -> Result<bool, Self::Error>;

    fn resize(&mut self, width: u32, height: u32) -> Result<(), Self::Error>;

    fn redraw(&mut self, overlay: OverlayStatistics) -> Result<(), Self::Error>;

    fn suspend(&mut self) -> Result<(), Self::Error>;

    fn resume(&mut self) -> Result<(), Self::Error>;

    fn recover(&mut self) -> Result<(), Self::Error>;

    fn shutdown(&mut self) -> Result<(), Self::Error>;
}
