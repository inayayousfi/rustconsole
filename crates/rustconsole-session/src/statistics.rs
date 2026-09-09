//! Platform-neutral snapshots for session and pipeline statistics.

use rustconsole_protocol::{ChromaSubsampling, NegotiatedFeature, ProtocolVersion, VideoBitDepth};
use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Distribution<T> {
    sample_count: NonZeroU64,
    median: T,
    percentile_95: T,
    percentile_99: T,
    worst: T,
}

impl<T: Copy + Ord> Distribution<T> {
    pub fn new(
        sample_count: u64,
        median: T,
        percentile_95: T,
        percentile_99: T,
        worst: T,
    ) -> Result<Self, InvalidDistribution> {
        let sample_count = NonZeroU64::new(sample_count).ok_or(InvalidDistribution::NoSamples)?;
        if median > percentile_95 || percentile_95 > percentile_99 || percentile_99 > worst {
            return Err(InvalidDistribution::QuantilesOutOfOrder);
        }

        Ok(Self {
            sample_count,
            median,
            percentile_95,
            percentile_99,
            worst,
        })
    }

    #[must_use]
    pub const fn sample_count(self) -> u64 {
        self.sample_count.get()
    }

    #[must_use]
    pub const fn median(self) -> T {
        self.median
    }

    #[must_use]
    pub const fn percentile_95(self) -> T {
        self.percentile_95
    }

    #[must_use]
    pub const fn percentile_99(self) -> T {
        self.percentile_99
    }

    #[must_use]
    pub const fn worst(self) -> T {
        self.worst
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidDistribution {
    NoSamples,
    QuantilesOutOfOrder,
}

impl fmt::Display for InvalidDistribution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSamples => formatter.write_str("a distribution requires at least one sample"),
            Self::QuantilesOutOfOrder => {
                formatter.write_str("distribution quantiles must be in ascending order")
            }
        }
    }
}

impl std::error::Error for InvalidDistribution {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoCodec {
    Av1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioCodec {
    Opus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NegotiatedConfiguration {
    pub protocol: ProtocolVersion,
    pub features: Vec<NegotiatedFeature>,
    pub video_codec: VideoCodec,
    pub video_width: u32,
    pub video_height: u32,
    pub video_frames_per_second: u16,
    pub video_chroma_subsampling: ChromaSubsampling,
    pub video_bit_depth: VideoBitDepth,
    pub video_maximum_bitrate_bits_per_second: u64,
    pub audio_codec: AudioCodec,
    pub audio_sample_rate_hz: u32,
    pub audio_channels: u8,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PipelineQueue {
    CaptureToColorConversion,
    ColorConversionToEncode,
    EncodeToTransport,
    ReceiveToFrameAssembly,
    FrameAssemblyToDecode,
    DecodeToPresentation,
    AudioCaptureToEncode,
    AudioReceiveToPlayback,
    InputReceiveToDriver,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct QueueStatistics {
    pub capacity_items: usize,
    pub depth_items: usize,
    pub residence_micros: Option<Distribution<u64>>,
    pub overflow_count: u64,
    pub dropped_items: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VideoStatistics {
    pub capture_interval_micros: Option<Distribution<u64>>,
    pub capture_duration_micros: Option<Distribution<u64>>,
    pub color_conversion_duration_micros: Option<Distribution<u64>>,
    pub encode_duration_micros: Option<Distribution<u64>>,
    pub encoded_frame_size_bytes: Option<Distribution<u64>>,
    pub bitrate_bits_per_second: Option<Distribution<u64>>,
    pub packetization_duration_micros: Option<Distribution<u64>>,
    pub estimated_network_transit_micros: Option<Distribution<u64>>,
    pub frame_assembly_duration_micros: Option<Distribution<u64>>,
    pub decode_duration_micros: Option<Distribution<u64>>,
    pub presentation_duration_micros: Option<Distribution<u64>>,
    pub sent_chunks: u64,
    pub lost_chunks: u64,
    pub late_chunks: u64,
    pub discarded_chunks: u64,
    pub requested_keyframes: u64,
    pub keyframe_recovery_micros: Option<Distribution<u64>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AudioStatistics {
    pub buffer_depth_micros: Option<Distribution<u64>>,
    pub underrun_count: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InputStatistics {
    pub receive_to_driver_submit_micros: Option<Distribution<u64>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SystemStatistics {
    pub cpu_load_basis_points: Option<Distribution<u32>>,
    pub gpu_load_basis_points: Option<Distribution<u32>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FullFrameCopyStatistics {
    pub copy_count: u64,
    pub copied_bytes: u64,
    pub duration_micros: Option<Distribution<u64>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockOffsetEstimate {
    pub offset_micros: i64,
    pub uncertainty_micros: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionStatistics {
    pub observed_for_micros: u64,
    pub negotiated: Option<NegotiatedConfiguration>,
    pub clock_offset: Option<ClockOffsetEstimate>,
    pub video: VideoStatistics,
    pub audio: AudioStatistics,
    pub input: InputStatistics,
    pub system: SystemStatistics,
    pub queues: BTreeMap<PipelineQueue, QueueStatistics>,
    pub full_frame_copies: BTreeMap<PipelineQueue, FullFrameCopyStatistics>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustconsole_protocol::{CURRENT_PROTOCOL_VERSION, FeatureId};

    #[test]
    fn distribution_requires_samples_and_ordered_quantiles() {
        assert_eq!(
            Distribution::new(0, 1, 2, 3, 4),
            Err(InvalidDistribution::NoSamples)
        );
        assert_eq!(
            Distribution::new(4, 1, 3, 2, 4),
            Err(InvalidDistribution::QuantilesOutOfOrder)
        );

        let distribution = Distribution::new(4, 1, 2, 3, 4).unwrap();
        assert_eq!(distribution.sample_count(), 4);
        assert_eq!(distribution.median(), 1);
        assert_eq!(distribution.percentile_95(), 2);
        assert_eq!(distribution.percentile_99(), 3);
        assert_eq!(distribution.worst(), 4);
    }

    #[test]
    fn queue_statistics_are_keyed_by_pipeline_boundary() {
        let mut statistics = SessionStatistics::default();
        statistics.queues.insert(
            PipelineQueue::DecodeToPresentation,
            QueueStatistics {
                capacity_items: 2,
                depth_items: 1,
                residence_micros: Some(Distribution::new(8, 400, 700, 900, 1_100).unwrap()),
                overflow_count: 3,
                dropped_items: 3,
            },
        );

        let queue = &statistics.queues[&PipelineQueue::DecodeToPresentation];
        assert_eq!(queue.capacity_items, 2);
        assert_eq!(queue.residence_micros.unwrap().percentile_99(), 900);
        assert_eq!(queue.overflow_count, queue.dropped_items);
    }

    #[test]
    fn snapshot_keeps_negotiation_clock_uncertainty_and_copy_boundary() {
        let mut statistics = SessionStatistics {
            negotiated: Some(NegotiatedConfiguration {
                protocol: CURRENT_PROTOCOL_VERSION,
                features: vec![NegotiatedFeature {
                    id: FeatureId::new(1).unwrap(),
                    version: 2,
                }],
                video_codec: VideoCodec::Av1,
                video_width: 2560,
                video_height: 1440,
                video_frames_per_second: 120,
                video_chroma_subsampling: ChromaSubsampling::Yuv420,
                video_bit_depth: VideoBitDepth::Ten,
                video_maximum_bitrate_bits_per_second: 80_000_000,
                audio_codec: AudioCodec::Opus,
                audio_sample_rate_hz: 48_000,
                audio_channels: 2,
            }),
            clock_offset: Some(ClockOffsetEstimate {
                offset_micros: -120,
                uncertainty_micros: 35,
            }),
            ..SessionStatistics::default()
        };
        statistics.full_frame_copies.insert(
            PipelineQueue::DecodeToPresentation,
            FullFrameCopyStatistics {
                copy_count: 3,
                copied_bytes: 33_177_600,
                duration_micros: Some(Distribution::new(3, 300, 400, 450, 500).unwrap()),
            },
        );

        let negotiated = statistics.negotiated.unwrap();
        assert_eq!(negotiated.video_frames_per_second, 120);
        assert_eq!(negotiated.video_bit_depth, VideoBitDepth::Ten);
        assert_eq!(negotiated.video_maximum_bitrate_bits_per_second, 80_000_000);
        assert_eq!(statistics.clock_offset.unwrap().uncertainty_micros, 35);
        assert_eq!(
            statistics.full_frame_copies[&PipelineQueue::DecodeToPresentation].copy_count,
            3
        );
    }
}
