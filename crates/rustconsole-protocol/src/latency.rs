//! Small control measurements, independent of detailed diagnostics.

use prost::Message;

pub const DEFAULT_MAXIMUM_DELAY_MICROS: u64 = 100_000;
pub const MAX_MEASUREMENT_MICROS: u64 = 60_000_000;
pub const MAX_REPORT_INTERVAL_MICROS: u64 = 2_000_000;

pub fn valid_maximum_delay(value: u64) -> bool {
    value > 0 && value <= u64::from(u32::MAX) * 1_000
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct LatencyProbeReport {
    #[prost(uint64, tag = "1")]
    pub baseline_round_trip_micros: u64,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct LatencyMeasurement {
    #[prost(uint64, tag = "1")]
    pub delay_micros: u64,
    #[prost(uint64, tag = "2")]
    pub uncertainty_micros: u64,
    #[prost(bool, tag = "3")]
    pub estimated: bool,
    #[prost(uint32, tag = "4")]
    pub samples: u32,
}

impl LatencyMeasurement {
    pub fn valid(self) -> bool {
        self.samples > 0
            && self.samples <= 65_536
            && self.delay_micros <= MAX_MEASUREMENT_MICROS
            && self.uncertainty_micros <= MAX_MEASUREMENT_MICROS
    }
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct LatencyReport {
    #[prost(message, optional, tag = "1")]
    pub video: Option<LatencyMeasurement>,
    #[prost(message, optional, tag = "2")]
    pub audio: Option<LatencyMeasurement>,
    #[prost(message, optional, tag = "3")]
    pub input: Option<LatencyMeasurement>,
    #[prost(bool, tag = "4")]
    pub audio_active: bool,
    #[prost(bool, tag = "5")]
    pub input_active: bool,
    #[prost(uint64, tag = "6")]
    pub presented_bitrate_bits_per_second: u64,
}

impl LatencyReport {
    pub fn valid(self) -> bool {
        [self.video, self.audio, self.input]
            .into_iter()
            .flatten()
            .all(LatencyMeasurement::valid)
            && (self.audio_active || self.audio.is_none())
            && (self.input_active || self.input.is_none())
            && self.presented_bitrate_bits_per_second
                <= crate::av1::MAXIMUM_VIDEO_BITRATE_BITS_PER_SECOND
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, prost::Enumeration)]
#[repr(i32)]
pub enum LatencyChannel {
    Network = 0,
    Video = 1,
    Audio = 2,
    Input = 3,
}

impl LatencyChannel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Network => "network round-trip",
            Self::Video => "video capture to presentation",
            Self::Audio => "audio capture to estimated playback",
            Self::Input => "input acknowledgement",
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Message)]
pub struct LatencyControlStatus {
    #[prost(uint64, optional, tag = "1")]
    pub ceiling_bits_per_second: Option<u64>,
    #[prost(uint32, tag = "2")]
    pub highest_pressure_ppm: u32,
    #[prost(uint32, tag = "3")]
    pub score_ppm: u32,
    #[prost(bool, tag = "4")]
    pub measurements_ready: bool,
    #[prost(bool, tag = "5")]
    pub uses_estimates: bool,
}

impl LatencyControlStatus {
    pub fn valid(self) -> bool {
        self.ceiling_bits_per_second.is_none_or(|value| {
            value > 0 && value <= crate::av1::MAXIMUM_VIDEO_BITRATE_BITS_PER_SECOND
        }) && self.score_ppm <= 1_000_000
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Message)]
pub struct LatencyBudgetFailure {
    #[prost(enumeration = "LatencyChannel", tag = "1")]
    pub channel: i32,
    #[prost(uint64, tag = "2")]
    pub observed_delay_micros: u64,
    #[prost(uint64, tag = "3")]
    pub budget_micros: u64,
    #[prost(uint64, tag = "4")]
    pub absolute_limit_micros: u64,
    #[prost(uint64, tag = "5")]
    pub baseline_micros: u64,
    #[prost(bool, tag = "6")]
    pub estimated: bool,
}

impl LatencyBudgetFailure {
    pub fn valid(self) -> bool {
        LatencyChannel::try_from(self.channel).is_ok()
            && valid_maximum_delay(self.absolute_limit_micros)
            && self.budget_micros > 0
            && self.budget_micros <= self.absolute_limit_micros
            && self.observed_delay_micros > self.budget_micros
            && self.observed_delay_micros <= MAX_MEASUREMENT_MICROS
            && self.baseline_micros <= MAX_MEASUREMENT_MICROS
    }
}
