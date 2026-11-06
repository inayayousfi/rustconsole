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

const MAX_SOURCE_DELAY_SAMPLES: usize = 512;

/// Delay this host added before sending, beyond its usual capture-to-encoded time.
///
/// A capture or encoder stall reaches the player as extra delay, but a lower bitrate cannot remove
/// it. The bitrate control therefore sees the player's delay without this excess.
#[derive(Default)]
struct SourceDelay {
    last_capture_micros: u64,
    delays_micros: Vec<u64>,
    usual_micros: Option<u64>,
    previous_excess_micros: u64,
}

impl SourceDelay {
    fn observe(&mut self, captured_at_micros: u64, encoded_at_micros: u64) {
        // A repeated capture keeps its original timestamp, so its age says nothing about a stall.
        if captured_at_micros == self.last_capture_micros {
            return;
        }
        self.last_capture_micros = captured_at_micros;
        if self.delays_micros.len() < MAX_SOURCE_DELAY_SAMPLES {
            self.delays_micros
                .push(encoded_at_micros.saturating_sub(captured_at_micros));
        }
    }

    /// Closes one report interval. A stall near its end can reach the player in the next report,
    /// so the excess of the previous interval still counts.
    fn take_excess_micros(&mut self) -> u64 {
        let mut excess = 0;
        self.delays_micros.sort_unstable();
        if let Some(typical) = self
            .delays_micros
            .get(self.delays_micros.len() * 3 / 4)
            .copied()
        {
            let usual = self
                .usual_micros
                .map_or(typical, |usual| usual.min(typical));
            self.usual_micros = Some(usual);
            excess = typical - usual;
        }
        self.delays_micros.clear();
        excess.max(std::mem::replace(&mut self.previous_excess_micros, excess))
    }
}

/// Platform-neutral decisions for one encoded-video delivery path.
pub struct HostVideoStreamPolicy {
    bitrate: AdaptiveBitrateController,
    recovery: VideoRecovery,
    latency: Option<crate::latency::LatencyPolicy>,
    source_delay: SourceDelay,
}

impl HostVideoStreamPolicy {
    #[must_use]
    pub fn new(maximum_bits_per_second: u64) -> Self {
        Self {
            bitrate: AdaptiveBitrateController::new(maximum_bits_per_second),
            recovery: VideoRecovery::default(),
            latency: None,
            source_delay: SourceDelay::default(),
        }
    }

    #[must_use]
    pub fn from_startup_probe(
        maximum_bits_per_second: u64,
        measured_capacity_bits_per_second: u64,
    ) -> Self {
        Self {
            bitrate: AdaptiveBitrateController::from_startup_probe(
                maximum_bits_per_second,
                measured_capacity_bits_per_second,
            ),
            recovery: VideoRecovery::default(),
            latency: None,
            source_delay: SourceDelay::default(),
        }
    }

    pub fn observe_receiver(
        &mut self,
        path: VideoPathReport,
        delivery: VideoDeliveryReport,
    ) -> Option<BitrateChange> {
        self.bitrate.observe(path, delivery)
    }

    pub fn enable_latency_control(
        &mut self,
        maximum_delay_micros: u64,
        network_baseline_micros: u64,
        now: Instant,
    ) {
        self.latency = Some(crate::latency::LatencyPolicy::new(
            maximum_delay_micros,
            network_baseline_micros,
            now,
        ));
    }

    pub fn observe_receiver_with_latency(
        &mut self,
        path: VideoPathReport,
        delivery: VideoDeliveryReport,
        report: Option<rustconsole_protocol::latency::LatencyReport>,
        now: Instant,
    ) -> Result<Option<BitrateChange>, rustconsole_protocol::latency::LatencyBudgetFailure> {
        let reference = self
            .bitrate
            .delivery_ceiling_bits_per_second()
            .unwrap_or(self.bitrate.maximum_bits_per_second)
            .min(self.bitrate.maximum_bits_per_second);
        let previous = self.target_bits_per_second();
        let source_excess = self.source_delay.take_excess_micros();
        let report = report.map(|mut report| {
            if let Some(video) = report.video.as_mut() {
                video.typical_delay_micros = video.typical().saturating_sub(source_excess).max(1);
            }
            report
        });
        let Some(latency) = self.latency.as_mut() else {
            return Ok(self.bitrate.observe(path, delivery));
        };
        let evaluation = latency.observe(
            report,
            u64::try_from(path.round_trip_time.as_micros()).unwrap_or(u64::MAX),
            previous,
            reference,
            now,
        );
        if let Some(failure) = evaluation.failure {
            return Err(failure);
        }
        if self.bitrate.returning_after_collapse() {
            latency.forget_ceiling();
        }
        self.bitrate.configure_latency_ceiling(latency.ceiling());
        let mut change =
            self.bitrate
                .observe_with_recovery(path, delivery, evaluation.allow_increase);
        if let Some(target) = evaluation.reduce_to {
            let target = target.min(self.bitrate.target_bits_per_second());
            let latency_change = self
                .bitrate
                .set_latency_target(target, latency.retry_delay_micros());
            if latency_change.is_some() {
                change = latency_change;
            }
        }
        if change.is_some() {
            latency.bitrate_changed(now);
        }
        Ok(change)
    }

    pub fn latency_status(&self) -> Option<rustconsole_protocol::latency::LatencyControlStatus> {
        self.latency
            .as_ref()
            .map(crate::latency::LatencyPolicy::status)
    }

    pub fn observe_worker_queue_drop(&mut self) {
        self.recovery.require_keyframe();
        self.bitrate.observe_sender_pressure();
    }

    pub fn observe_send_deadline_expired(&mut self) -> Option<BitrateChange> {
        self.recovery.require_keyframe();
        let change = self.bitrate.observe_sender_congestion();
        if change.is_some()
            && let Some(latency) = self.latency.as_mut()
        {
            latency.bitrate_changed(Instant::now());
        }
        change
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
        &mut self,
        frame: HostEncodedVideoFrame,
        maximum_datagram_size: usize,
        video_datagram_version: u32,
    ) -> Result<Vec<Vec<u8>>, VideoDatagramError> {
        self.source_delay
            .observe(frame.captured_at_micros, frame.encoded_at_micros);
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
        policy.observe_worker_queue_drop();
        assert_eq!(policy.target_bits_per_second(), 50_000_000);
        assert!(policy.keyframe_request_due(now));
        assert!(!policy.accept_encoded_frame(0, false));
        assert!(policy.accept_encoded_frame(1, true));
    }

    #[test]
    fn repeated_worker_queue_drops_reduce_bitrate_as_sender_pressure() {
        let mut policy = HostVideoStreamPolicy::new(100_000_000);
        let path = VideoPathReport {
            round_trip_time: Duration::from_millis(10),
            congestion_window_bytes: 250_000,
            lost_packets: 0,
        };
        policy.observe_worker_queue_drop();
        assert_eq!(
            policy.observe_receiver(
                path,
                VideoDeliveryReport {
                    completed_payload_bytes: 625_000,
                    measurement_interval_micros: 500_000,
                    ..VideoDeliveryReport::default()
                }
            ),
            None
        );
        policy.observe_worker_queue_drop();

        let change = policy
            .observe_receiver(
                path,
                VideoDeliveryReport {
                    completed_payload_bytes: 1_250_000,
                    measurement_interval_micros: 500_000,
                    ..VideoDeliveryReport::default()
                },
            )
            .unwrap();

        assert_eq!(change.target_bits_per_second, 45_000_000);
        assert_eq!(
            change.cause,
            crate::VideoBitrateChangeCause::SenderCongestion
        );
    }

    #[test]
    fn fake_encoder_frame_reaches_the_receiver_assembler() {
        let mut policy = HostVideoStreamPolicy::new(40_000_000);
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
        assert_eq!(assembled.target_bitrate_bits_per_second, 20_000_000);
    }
}
