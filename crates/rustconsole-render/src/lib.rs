//! Platform-neutral decoded-frame rendering and overlay contracts.

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
pub enum DecodedVideoColor {
    Bt709Limited,
    Bt2020PqLimited,
}

pub trait PlayerVideoBackend<Frame> {
    type Error;

    fn present_frame(
        &mut self,
        frame: &Frame,
        color: DecodedVideoColor,
        width: u32,
        height: u32,
        overlay_text: &str,
    ) -> Result<(), Self::Error>;

    fn present_loading(
        &mut self,
        width: u32,
        height: u32,
        elapsed_seconds: f32,
        status: &str,
    ) -> Result<(), Self::Error>;
}
