//! Platform-neutral decoded-frame rendering and overlay contracts.

use std::time::Duration;
use std::time::Instant;

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

#[derive(Clone, Copy, Debug)]
pub struct PresentationSubmission {
    pub id: u64,
    pub queued_at: Instant,
    pub feedback_available: bool,
    pub diagnostics: Option<PresentationDiagnostics>,
}

#[derive(Clone, Copy, Debug)]
pub struct PresentationDiagnostics {
    pub pre_import: Duration,
    pub native_frame_import: Duration,
    pub command_preparation: Duration,
    pub queue_submission: Duration,
    pub presentation_queueing: Duration,
}

#[derive(Clone, Copy, Debug)]
pub struct PresentationFeedback {
    pub id: u64,
    pub presented_at: Instant,
}

pub trait PlayerVideoBackend<Frame, GuiFrame> {
    type Error;

    fn present_frame(
        &mut self,
        frame: &Frame,
        color: DecodedVideoColor,
        width: u32,
        height: u32,
        diagnostics: bool,
        gui: GuiFrame,
    ) -> Result<PresentationSubmission, Self::Error>;

    fn take_presentation_feedback(&mut self) -> Option<PresentationFeedback>;

    fn present_loading(
        &mut self,
        width: u32,
        height: u32,
        elapsed_seconds: f32,
        gui: GuiFrame,
    ) -> Result<(), Self::Error>;
}
