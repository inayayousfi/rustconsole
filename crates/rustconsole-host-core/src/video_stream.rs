use crate::video_recovery::VideoRecovery;
use crate::{AdaptiveBitrateController, BitrateChange, VideoDeliveryReport, VideoPathReport};
use rustconsole_session::video_datagram::{
    VideoDatagramError, VideoFramePayload, packetize_video_frame_for_version,
};
use std::time::Instant;

pub struct HostEncodedVideoFrame {
    pub sequence: u64,
    pub captured_at_micros: u64,
    pub encoded_at_micros: u64,
    pub packetized_at_micros: u64,
    pub input_sequence: u64,
    pub keyframe: bool,
    pub payload: Vec<u8>,
}

/// Platform-neutral decisions for one encoded-video delivery path.
pub struct HostVideoStreamPolicy {
    bitrate: AdaptiveBitrateController,
    recovery: VideoRecovery,
}

impl HostVideoStreamPolicy {
    #[must_use]
    pub fn new(maximum_bits_per_second: u64) -> Self {
        Self {
            bitrate: AdaptiveBitrateController::new(maximum_bits_per_second),
            recovery: VideoRecovery::default(),
        }
    }

    pub fn observe_receiver(
        &mut self,
        path: VideoPathReport,
        delivery: VideoDeliveryReport,
    ) -> Option<BitrateChange> {
        self.bitrate.observe(path, delivery)
    }

    pub fn observe_worker_queue_drop(&mut self) -> Option<BitrateChange> {
        self.recovery.require_keyframe();
        self.bitrate.observe_sender_congestion()
    }

    pub fn observe_send_deadline_expired(&mut self) -> Option<BitrateChange> {
        self.recovery.require_keyframe();
        self.bitrate.observe_sender_congestion()
    }

    pub fn require_keyframe(&mut self) {
        self.recovery.require_keyframe();
    }

    #[must_use]
    pub fn keyframe_request_due(&mut self, now: Instant) -> bool {
        self.recovery.request_due(now)
    }

    pub fn accept_encoded_frame(&mut self, sequence: u64, keyframe: bool) -> bool {
        self.recovery.accept(sequence, keyframe)
    }

    pub fn packetize_encoded_frame(
        &self,
        frame: HostEncodedVideoFrame,
        maximum_datagram_size: usize,
        video_datagram_version: u32,
    ) -> Result<Vec<Vec<u8>>, VideoDatagramError> {
        packetize_video_frame_for_version(
            &VideoFramePayload {
                sequence: frame.sequence,
                captured_at_micros: frame.captured_at_micros,
                encoded_at_micros: frame.encoded_at_micros,
                packetized_at_micros: frame.packetized_at_micros,
                input_sequence: frame.input_sequence,
                keyframe: frame.keyframe,
                target_bitrate_bits_per_second: self.target_bits_per_second(),
                estimated_capacity_bits_per_second: self.estimated_capacity_bits_per_second(),
                soft_ceiling_bits_per_second: self.soft_ceiling_bits_per_second(),
                payload: frame.payload,
            },
            maximum_datagram_size,
            video_datagram_version,
        )
    }

    #[must_use]
    pub const fn target_bits_per_second(&self) -> u64 {
        self.bitrate.target_bits_per_second()
    }

    #[must_use]
    pub const fn estimated_capacity_bits_per_second(&self) -> u64 {
        self.bitrate.estimated_capacity_bits_per_second()
    }

    #[must_use]
    pub fn soft_ceiling_bits_per_second(&self) -> Option<u64> {
        self.bitrate.soft_ceiling_bits_per_second()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustconsole_session::video_datagram::{VIDEO_DATAGRAM_VERSION, VideoFrameAssembler};
    use std::time::Duration;

    #[test]
    fn delivery_failures_couple_congestion_response_to_keyframe_recovery() {
        let now = Instant::now();
        let mut policy = HostVideoStreamPolicy::new(100_000_000);
        let change = policy.observe_worker_queue_drop().unwrap();
        assert_eq!(change.target_bits_per_second, 75_000_000);
        assert!(policy.keyframe_request_due(now));
        assert!(!policy.accept_encoded_frame(0, false));
        assert!(policy.accept_encoded_frame(1, true));
    }

    #[test]
    fn fake_encoder_frame_reaches_the_receiver_assembler() {
        let policy = HostVideoStreamPolicy::new(40_000_000);
        let payload = vec![42; 64 * 1024];
        let datagrams = policy
            .packetize_encoded_frame(
                HostEncodedVideoFrame {
                    sequence: 7,
                    captured_at_micros: 10,
                    encoded_at_micros: 20,
                    packetized_at_micros: 30,
                    input_sequence: 4,
                    keyframe: true,
                    payload: payload.clone(),
                },
                1_200,
                VIDEO_DATAGRAM_VERSION,
            )
            .unwrap();
        let mut assembler = VideoFrameAssembler::new(120);
        let now = Instant::now();
        let assembled = datagrams
            .iter()
            .filter_map(|datagram| {
                assembler
                    .push(datagram, now, Duration::from_millis(5))
                    .unwrap()
                    .frame
            })
            .next()
            .unwrap();
        assert_eq!(assembled.sequence, 7);
        assert_eq!(assembled.input_sequence, 4);
        assert_eq!(assembled.payload, payload);
        assert_eq!(assembled.target_bitrate_bits_per_second, 40_000_000);
    }
}
