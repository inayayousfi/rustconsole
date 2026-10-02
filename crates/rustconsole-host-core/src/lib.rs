//! Platform-neutral host session orchestration.

pub mod latency;
pub mod video_pacing;
mod video_recovery;
pub mod video_stream;

use rustconsole_media::{AudioSamples, VideoFormat, VideoFrame};
use rustconsole_protocol::InputEvent;
pub use rustconsole_protocol::diagnostics::VideoBitrateChangeCause;
use rustconsole_session::{SessionLifecycle, SessionPhase, SessionTransitionError};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

pub mod authentication {
    pub use rustconsole_session::authentication::{
        AuthenticationError, OpaqueServerRecord, SessionIdentity,
    };
    pub use rustconsole_session::quic::{
        AuthenticatedConnection, AuthenticationRateLimiter, HostMetadata, QuicAuthenticationError,
        authenticate_server, ephemeral_server_config, read_envelope, send_media_datagram,
        write_envelope,
    };
}

pub mod video_transport {
    pub use rustconsole_session::video_datagram::{
        LEGACY_VIDEO_DATAGRAM_VERSION, VIDEO_DATAGRAM_VERSION, VideoFramePayload,
        assembly_deadline, packetize_video_frame, packetize_video_frame_for_version,
    };
}

pub mod audio_transport {
    pub use rustconsole_session::audio_datagram::{AUDIO_QUEUE_PACKETS, AUDIO_WAIT, packetize};
    pub use rustconsole_session::media_queue::MediaQueue;

    pub fn expired(queued_at_micros: u64, now_micros: u64) -> bool {
        now_micros
            .checked_sub(queued_at_micros)
            .is_none_or(|age| age >= AUDIO_WAIT.as_micros() as u64)
    }

    #[test]
    fn original_queue_deadline_survives_pipe_transit() {
        assert!(!expired(1000, 40_999));
        assert!(expired(1000, 41_000));
        assert!(expired(1000, 999));
    }
}

pub const VIDEO_BITRATE_BOOTSTRAP: u64 = 1_000_000;
const CONGESTION_TARGET_PERCENT: u128 = 75;
const MILD_CONGESTION_TARGET_PERCENT: u128 = 90;
const REPEATED_SENDER_CONGESTION_TARGET_PERCENT: u128 = 90;
const SAFE_PATH_PERCENT: u128 = 100;
const RECOVERY_PROBE_PERCENT: u128 = 110;
const RECOVERY_MINIMUM_STEP_PERCENT: u128 = 3;
const RECOVERY_MAXIMUM_STEP_PERCENT: u128 = 25;
const SOFT_CEILING_PROBE_PERCENT: u128 = 102;
const SOFT_CEILING_RANGE_PERCENT: u128 = 95;
const SOFT_CEILING_PROBE_INTERVAL_MICROS: u64 = 5_000_000;
const MAXIMUM_PROBE_BACKOFF_MICROS: u64 = 40_000_000;
const SOFT_CEILING_EXPIRATION_MICROS: u64 = 60_000_000;
const RECOVERY_COOLDOWN_MICROS: u64 = 5_000_000;
const FAILED_PROBE_ATTRIBUTION_MICROS: u64 = 5_000_000;
const FAILED_PROBE_AVERAGE_SAMPLES: usize = 5;
const HEALTHY_REPORTS_BEFORE_RECOVERY: u8 = 4;
const DEGRADED_REPORTS_BEFORE_REDUCTION: u8 = 2;
const RTT_PRESSURE_REPORTS_BEFORE_REDUCTION: u8 = 2;
const MINIMUM_RTT_PRESSURE_MARGIN: Duration = Duration::from_millis(20);
const SENDER_PRESSURE_REPORTS_BEFORE_REDUCTION: u8 = 2;
const SEVERE_SENDER_PRESSURE_EVENTS: u8 = 3;
const SEVERE_RECEIVER_LOSS_PERCENT: u128 = 2;
const SEVERE_LOST_CHUNKS: u64 = 4;
const SEVERE_INCOMPLETE_FRAMES: u64 = 2;
const CAPACITY_AVERAGE_REPORTS: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostSessionControlAction {
    ReleasePointerCapture,
}

pub trait HostSessionControlSource {
    fn try_next_action(&mut self) -> Result<Option<HostSessionControlAction>, String>;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VideoPathReport {
    pub round_trip_time: Duration,
    pub congestion_window_bytes: u64,
    pub lost_packets: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VideoDeliveryReport {
    pub received_chunks: u64,
    pub completed_payload_bytes: u64,
    pub measurement_interval_micros: u64,
    pub lost_chunks: u64,
    pub late_chunks: u64,
    pub assembly_overflows: u64,
    pub incomplete_frames: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BitrateChangeReason {
    HealthyDelivery,
    Congestion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BitrateChange {
    pub target_bits_per_second: u64,
    pub reason: BitrateChangeReason,
    pub cause: VideoBitrateChangeCause,
}

#[derive(Debug)]
pub struct AdaptiveBitrateController {
    maximum_bits_per_second: u64,
    target_bits_per_second: u64,
    estimated_capacity_bits_per_second: u64,
    delivery_samples: VecDeque<(u64, u64)>,
    minimum_round_trip_time: Option<Duration>,
    rtt_pressure_report_streak: u8,
    previous_delivery: VideoDeliveryReport,
    degraded_reports: u8,
    healthy_reports: u8,
    healthy_delivery_micros: u64,
    soft_ceiling_healthy_micros: u64,
    recovery_cooldown_micros: u64,
    provisional_ceiling_bits_per_second: Option<u64>,
    latency_ceiling_bits_per_second: Option<u64>,
    failed_probe_targets: VecDeque<u64>,
    active_probe: Option<ActiveBitrateProbe>,
    probe_interval_micros: u64,
    sender_pressure_events_since_report: u8,
    severe_sender_reduction_since_report: bool,
    sender_congested_report_streak: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActiveBitrateProbe {
    previous_target_bits_per_second: Option<u64>,
    target_bits_per_second: u64,
    elapsed_micros: u64,
}

impl AdaptiveBitrateController {
    #[must_use]
    pub fn new(maximum_bits_per_second: u64) -> Self {
        let maximum_bits_per_second = maximum_bits_per_second.max(VIDEO_BITRATE_BOOTSTRAP);
        let target_bits_per_second = (maximum_bits_per_second / 2).max(VIDEO_BITRATE_BOOTSTRAP);
        Self {
            maximum_bits_per_second,
            target_bits_per_second,
            estimated_capacity_bits_per_second: 0,
            delivery_samples: VecDeque::with_capacity(CAPACITY_AVERAGE_REPORTS),
            minimum_round_trip_time: None,
            rtt_pressure_report_streak: 0,
            previous_delivery: VideoDeliveryReport::default(),
            degraded_reports: 0,
            healthy_reports: 0,
            healthy_delivery_micros: 0,
            soft_ceiling_healthy_micros: 0,
            recovery_cooldown_micros: 0,
            provisional_ceiling_bits_per_second: None,
            latency_ceiling_bits_per_second: None,
            failed_probe_targets: VecDeque::with_capacity(FAILED_PROBE_AVERAGE_SAMPLES),
            active_probe: None,
            probe_interval_micros: SOFT_CEILING_PROBE_INTERVAL_MICROS,
            sender_pressure_events_since_report: 0,
            severe_sender_reduction_since_report: false,
            sender_congested_report_streak: 0,
        }
    }

    #[must_use]
    pub fn from_startup_probe(
        maximum_bits_per_second: u64,
        measured_capacity_bits_per_second: u64,
    ) -> Self {
        let mut controller = Self::new(maximum_bits_per_second);
        if measured_capacity_bits_per_second == 0 {
            controller.target_bits_per_second = VIDEO_BITRATE_BOOTSTRAP;
            return controller;
        }
        let ceiling = measured_capacity_bits_per_second.min(controller.maximum_bits_per_second);
        controller.provisional_ceiling_bits_per_second = Some(ceiling);
        controller.estimated_capacity_bits_per_second = measured_capacity_bits_per_second;
        controller.target_bits_per_second = u64::try_from(
            u128::from(VIDEO_BITRATE_BOOTSTRAP)
                .saturating_mul(u128::from(ceiling))
                .isqrt(),
        )
        .unwrap_or(u64::MAX)
        .max(VIDEO_BITRATE_BOOTSTRAP)
        .min(controller.maximum_bits_per_second);
        controller.active_probe = Some(ActiveBitrateProbe {
            previous_target_bits_per_second: None,
            target_bits_per_second: controller.target_bits_per_second,
            elapsed_micros: 0,
        });
        controller
    }

    #[must_use]
    pub const fn target_bits_per_second(&self) -> u64 {
        self.target_bits_per_second
    }

    #[must_use]
    pub const fn estimated_capacity_bits_per_second(&self) -> u64 {
        self.estimated_capacity_bits_per_second
    }

    #[must_use]
    pub fn soft_ceiling_bits_per_second(&self) -> Option<u64> {
        if self.failed_probe_targets.len() < FAILED_PROBE_AVERAGE_SAMPLES {
            return None;
        }
        let sum = self
            .failed_probe_targets
            .iter()
            .fold(0_u128, |sum, target| sum + u128::from(*target));
        Some(u64::try_from(sum / self.failed_probe_targets.len() as u128).unwrap_or(u64::MAX))
    }

    pub fn observe(
        &mut self,
        path: VideoPathReport,
        delivery: VideoDeliveryReport,
    ) -> Option<BitrateChange> {
        self.observe_with_recovery(path, delivery, true)
    }

    pub fn observe_with_recovery(
        &mut self,
        path: VideoPathReport,
        delivery: VideoDeliveryReport,
        allow_increase: bool,
    ) -> Option<BitrateChange> {
        let completed_payload_bytes = delivery
            .completed_payload_bytes
            .saturating_sub(self.previous_delivery.completed_payload_bytes);
        let received_chunks = delivery
            .received_chunks
            .saturating_sub(self.previous_delivery.received_chunks);
        let lost_chunks = delivery
            .lost_chunks
            .saturating_sub(self.previous_delivery.lost_chunks);
        let incomplete_frames = delivery
            .incomplete_frames
            .saturating_sub(self.previous_delivery.incomplete_frames);
        let assembly_overflows = delivery
            .assembly_overflows
            .saturating_sub(self.previous_delivery.assembly_overflows);
        let receiver_loss = delivery.lost_chunks > self.previous_delivery.lost_chunks
            || delivery.late_chunks > self.previous_delivery.late_chunks
            || delivery.assembly_overflows > self.previous_delivery.assembly_overflows
            || delivery.incomplete_frames > self.previous_delivery.incomplete_frames;
        self.previous_delivery = delivery;

        if delivery.measurement_interval_micros == 0 {
            return None;
        }
        let sender_pressure_events = std::mem::take(&mut self.sender_pressure_events_since_report);
        let severe_sender_reduction =
            std::mem::take(&mut self.severe_sender_reduction_since_report);
        if sender_pressure_events != 0 {
            self.sender_congested_report_streak =
                self.sender_congested_report_streak.saturating_add(1);
        } else {
            self.sender_congested_report_streak = 0;
        }
        if self.delivery_samples.len() == CAPACITY_AVERAGE_REPORTS {
            self.delivery_samples.pop_front();
        }
        self.delivery_samples.push_back((
            completed_payload_bytes,
            delivery.measurement_interval_micros,
        ));
        let (delivered_bytes, interval_micros) = self.delivery_samples.iter().fold(
            (0_u64, 0_u64),
            |(bytes, interval), (sample_bytes, sample_interval)| {
                (
                    bytes.saturating_add(*sample_bytes),
                    interval.saturating_add(*sample_interval),
                )
            },
        );
        let capacity = delivered_bits_per_second(delivered_bytes, interval_micros);
        self.estimated_capacity_bits_per_second = capacity;

        let safe_path_capacity =
            if path.round_trip_time.is_zero() || path.congestion_window_bytes == 0 {
                u64::MAX
            } else {
                let minimum_round_trip_time = self
                    .minimum_round_trip_time
                    .map_or(path.round_trip_time, |minimum| {
                        minimum.min(path.round_trip_time)
                    });
                self.minimum_round_trip_time = Some(minimum_round_trip_time);
                percentage(
                    delivered_bits_per_second(
                        path.congestion_window_bytes,
                        u64::try_from(minimum_round_trip_time.as_micros()).unwrap_or(u64::MAX),
                    ),
                    SAFE_PATH_PERCENT,
                )
            };
        let rtt_pressure = self.minimum_round_trip_time.is_some_and(|minimum| {
            let margin = MINIMUM_RTT_PRESSURE_MARGIN.max(minimum / 2);
            path.round_trip_time > minimum.saturating_add(margin)
        });
        if rtt_pressure {
            self.rtt_pressure_report_streak = self.rtt_pressure_report_streak.saturating_add(1);
        } else {
            self.rtt_pressure_report_streak = 0;
        }

        let severe_receiver_loss = assembly_overflows > 0
            || incomplete_frames >= SEVERE_INCOMPLETE_FRAMES
            || (lost_chunks >= SEVERE_LOST_CHUNKS
                && loss_percent_at_least(
                    lost_chunks,
                    received_chunks.saturating_add(lost_chunks),
                    SEVERE_RECEIVER_LOSS_PERCENT,
                ));
        let severe_path_pressure =
            safe_path_capacity < percentage(self.target_bits_per_second, CONGESTION_TARGET_PERCENT);
        if severe_receiver_loss || severe_path_pressure {
            self.degraded_reports = 0;
            let rollback_target = self.record_active_probe_failure();
            self.reset_recovery_wait();
            let cause = if severe_receiver_loss {
                VideoBitrateChangeCause::SevereReceiverLoss
            } else {
                VideoBitrateChangeCause::SeverePathPressure
            };
            if severe_receiver_loss && let Some(target) = rollback_target {
                return self.rollback_failed_probe(target, cause);
            }
            return self.set_target(
                percentage(self.target_bits_per_second, CONGESTION_TARGET_PERCENT),
                BitrateChangeReason::Congestion,
                cause,
            );
        }

        if receiver_loss || safe_path_capacity < self.target_bits_per_second {
            self.reset_recovery_wait();
            self.degraded_reports = self.degraded_reports.saturating_add(1);
            if self.degraded_reports < DEGRADED_REPORTS_BEFORE_REDUCTION {
                return None;
            }
            self.degraded_reports = 0;
            let rollback_target = self.record_active_probe_failure();
            if let Some(target) = rollback_target {
                return self
                    .rollback_failed_probe(target, VideoBitrateChangeCause::MildDegradation);
            }
            return self.set_target(
                percentage(self.target_bits_per_second, MILD_CONGESTION_TARGET_PERCENT),
                BitrateChangeReason::Congestion,
                VideoBitrateChangeCause::MildDegradation,
            );
        }
        self.degraded_reports = 0;

        if self.rtt_pressure_report_streak != 0 {
            self.reset_recovery_wait();
            if self.rtt_pressure_report_streak < RTT_PRESSURE_REPORTS_BEFORE_REDUCTION {
                return None;
            }
            self.rtt_pressure_report_streak = 0;
            let rollback_target = self.record_active_probe_failure();
            if let Some(target) = rollback_target {
                return self
                    .rollback_failed_probe(target, VideoBitrateChangeCause::MildDegradation);
            }
            return self.set_target(
                percentage(self.target_bits_per_second, MILD_CONGESTION_TARGET_PERCENT),
                BitrateChangeReason::Congestion,
                VideoBitrateChangeCause::MildDegradation,
            );
        }

        if sender_pressure_events != 0 {
            self.reset_recovery_wait();
            if severe_sender_reduction
                || self.sender_congested_report_streak < SENDER_PRESSURE_REPORTS_BEFORE_REDUCTION
            {
                return None;
            }
            self.record_active_probe_failure();
            return self.set_target(
                percentage(
                    self.target_bits_per_second,
                    REPEATED_SENDER_CONGESTION_TARGET_PERCENT,
                ),
                BitrateChangeReason::Congestion,
                VideoBitrateChangeCause::SenderCongestion,
            );
        }

        if !allow_increase {
            self.reset_recovery_wait();
            return None;
        }
        let recovery_cooldown_was_active = self.recovery_cooldown_micros != 0;
        self.recovery_cooldown_micros = self
            .recovery_cooldown_micros
            .saturating_sub(delivery.measurement_interval_micros);
        if self.recovery_cooldown_micros != 0 {
            self.reset_recovery_wait();
            return None;
        }

        self.healthy_delivery_micros = self
            .healthy_delivery_micros
            .saturating_add(delivery.measurement_interval_micros);
        if self.healthy_delivery_micros >= SOFT_CEILING_EXPIRATION_MICROS {
            self.failed_probe_targets.clear();
            self.provisional_ceiling_bits_per_second = None;
            self.probe_interval_micros = SOFT_CEILING_PROBE_INTERVAL_MICROS;
        }

        let probe_validated = self.advance_active_probe(delivery.measurement_interval_micros);
        if self.target_bits_per_second == self.maximum_bits_per_second {
            return None;
        }
        if self.active_probe.is_some() {
            self.healthy_reports = 0;
            self.soft_ceiling_healthy_micros = 0;
            return None;
        }

        if probe_validated {
            self.healthy_reports = 0;
            self.soft_ceiling_healthy_micros = 0;
            let recovery_target = if self.near_soft_ceiling() {
                percentage(self.target_bits_per_second, SOFT_CEILING_PROBE_PERCENT)
            } else {
                self.ordinary_recovery_target()
            };
            return self.set_recovery_target(recovery_target.min(safe_path_capacity));
        }

        if recovery_cooldown_was_active {
            self.healthy_reports = 0;
            self.soft_ceiling_healthy_micros = 0;
            let recovery_target = if self.near_soft_ceiling() {
                percentage(self.target_bits_per_second, SOFT_CEILING_PROBE_PERCENT)
            } else {
                self.ordinary_recovery_target()
            };
            return self.set_recovery_target(recovery_target.min(safe_path_capacity));
        }

        if self.near_soft_ceiling() {
            self.healthy_reports = 0;
            self.soft_ceiling_healthy_micros = self
                .soft_ceiling_healthy_micros
                .saturating_add(delivery.measurement_interval_micros);
            if self.soft_ceiling_healthy_micros < self.probe_interval_micros {
                return None;
            }
            self.soft_ceiling_healthy_micros = 0;
            return self.set_recovery_target(
                percentage(self.target_bits_per_second, SOFT_CEILING_PROBE_PERCENT)
                    .min(safe_path_capacity),
            );
        }

        self.soft_ceiling_healthy_micros = 0;
        self.healthy_reports = self.healthy_reports.saturating_add(1);
        if self.healthy_reports < HEALTHY_REPORTS_BEFORE_RECOVERY {
            return None;
        }
        self.healthy_reports = 0;
        let recovery_target = self.ordinary_recovery_target();
        self.set_recovery_target(recovery_target.min(safe_path_capacity))
    }

    pub fn observe_sender_congestion(&mut self) -> Option<BitrateChange> {
        self.observe_sender_pressure();
        if self.sender_pressure_events_since_report < SEVERE_SENDER_PRESSURE_EVENTS
            || self.severe_sender_reduction_since_report
        {
            return None;
        }
        self.severe_sender_reduction_since_report = true;
        self.record_active_probe_failure();
        self.set_target(
            percentage(self.target_bits_per_second, CONGESTION_TARGET_PERCENT),
            BitrateChangeReason::Congestion,
            VideoBitrateChangeCause::SenderCongestion,
        )
    }

    pub fn observe_sender_pressure(&mut self) {
        self.reset_recovery_wait();
        self.degraded_reports = 0;
        self.sender_pressure_events_since_report =
            self.sender_pressure_events_since_report.saturating_add(1);
    }

    fn near_soft_ceiling(&self) -> bool {
        self.recovery_ceiling_bits_per_second()
            .is_some_and(|ceiling| {
                self.target_bits_per_second >= percentage(ceiling, SOFT_CEILING_RANGE_PERCENT)
            })
    }

    fn recovery_ceiling_bits_per_second(&self) -> Option<u64> {
        match (
            self.delivery_ceiling_bits_per_second(),
            self.latency_ceiling_bits_per_second,
        ) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub fn delivery_ceiling_bits_per_second(&self) -> Option<u64> {
        self.soft_ceiling_bits_per_second()
            .or(self.provisional_ceiling_bits_per_second)
    }

    pub fn configure_latency_ceiling(&mut self, ceiling: Option<u64>) {
        self.latency_ceiling_bits_per_second = ceiling;
    }

    pub fn set_latency_target(
        &mut self,
        target: u64,
        cooldown_micros: u64,
    ) -> Option<BitrateChange> {
        self.active_probe = None;
        self.reset_recovery_wait();
        let change = self.set_target(
            target,
            BitrateChangeReason::Congestion,
            VideoBitrateChangeCause::LatencyPressure,
        );
        self.recovery_cooldown_micros = cooldown_micros;
        change
    }

    fn ordinary_recovery_target(&self) -> u64 {
        let Some(ceiling) = self.recovery_ceiling_bits_per_second() else {
            return percentage(self.target_bits_per_second, RECOVERY_PROBE_PERCENT);
        };
        let safe_ceiling = percentage(ceiling, SOFT_CEILING_RANGE_PERCENT);
        if self.target_bits_per_second >= safe_ceiling {
            return self.target_bits_per_second;
        }
        let gap = safe_ceiling - self.target_bits_per_second;
        let half_gap = gap.div_ceil(2);
        let minimum_step = percentage(self.target_bits_per_second, RECOVERY_MINIMUM_STEP_PERCENT);
        let maximum_step = percentage(self.target_bits_per_second, RECOVERY_MAXIMUM_STEP_PERCENT);
        self.target_bits_per_second
            .saturating_add(half_gap.clamp(minimum_step, maximum_step))
            .min(safe_ceiling)
    }

    fn set_recovery_target(&mut self, target_bits_per_second: u64) -> Option<BitrateChange> {
        let previous_target_bits_per_second = self.target_bits_per_second;
        let change = self.set_target(
            target_bits_per_second,
            BitrateChangeReason::HealthyDelivery,
            VideoBitrateChangeCause::HealthyDelivery,
        );
        if let Some(change) = change {
            self.active_probe = Some(ActiveBitrateProbe {
                previous_target_bits_per_second: Some(previous_target_bits_per_second),
                target_bits_per_second: change.target_bits_per_second,
                elapsed_micros: 0,
            });
        }
        change
    }

    fn advance_active_probe(&mut self, interval_micros: u64) -> bool {
        let Some(probe) = self.active_probe.as_mut() else {
            return false;
        };
        probe.elapsed_micros = probe.elapsed_micros.saturating_add(interval_micros);
        if probe.elapsed_micros >= FAILED_PROBE_ATTRIBUTION_MICROS {
            self.active_probe = None;
            return true;
        }
        false
    }

    fn record_active_probe_failure(&mut self) -> Option<u64> {
        let probe = self.active_probe.take()?;
        self.provisional_ceiling_bits_per_second = Some(probe.target_bits_per_second);
        if self.failed_probe_targets.len() == FAILED_PROBE_AVERAGE_SAMPLES {
            self.failed_probe_targets.pop_front();
        }
        self.failed_probe_targets
            .push_back(probe.target_bits_per_second);
        self.probe_interval_micros = self
            .probe_interval_micros
            .saturating_mul(2)
            .min(MAXIMUM_PROBE_BACKOFF_MICROS);
        probe.previous_target_bits_per_second
    }

    fn rollback_failed_probe(
        &mut self,
        target_bits_per_second: u64,
        cause: VideoBitrateChangeCause,
    ) -> Option<BitrateChange> {
        let change = self.set_target(
            target_bits_per_second,
            BitrateChangeReason::Congestion,
            cause,
        );
        self.recovery_cooldown_micros = self.probe_interval_micros;
        change
    }

    fn reset_recovery_wait(&mut self) {
        self.healthy_reports = 0;
        self.soft_ceiling_healthy_micros = 0;
        self.healthy_delivery_micros = 0;
    }

    fn set_target(
        &mut self,
        target_bits_per_second: u64,
        reason: BitrateChangeReason,
        cause: VideoBitrateChangeCause,
    ) -> Option<BitrateChange> {
        let target = target_bits_per_second
            .max(VIDEO_BITRATE_BOOTSTRAP)
            .min(self.maximum_bits_per_second);
        if target == self.target_bits_per_second {
            return None;
        }
        self.target_bits_per_second = target;
        if reason == BitrateChangeReason::Congestion {
            self.recovery_cooldown_micros = RECOVERY_COOLDOWN_MICROS;
        }
        Some(BitrateChange {
            target_bits_per_second: target,
            reason,
            cause,
        })
    }
}

fn percentage(value: u64, percent: u128) -> u64 {
    u64::try_from(u128::from(value) * percent / 100).unwrap_or(u64::MAX)
}

fn delivered_bits_per_second(bytes: u64, interval_micros: u64) -> u64 {
    if interval_micros == 0 {
        return 0;
    }
    let bits_per_second = u128::from(bytes)
        .saturating_mul(8_000_000)
        .checked_div(u128::from(interval_micros))
        .unwrap_or(0);
    u64::try_from(bits_per_second).unwrap_or(u64::MAX)
}

fn loss_percent_at_least(lost: u64, total: u64, percent: u128) -> bool {
    total > 0 && u128::from(lost).saturating_mul(100) >= u128::from(total) * percent
}

pub trait DesktopCapture {
    type Frame;
    type Error;

    fn format(&self) -> VideoFormat;

    fn next_frame(&mut self) -> Result<VideoFrame<Self::Frame>, Self::Error>;

    fn reset(&mut self) -> Result<(), Self::Error>;
}

#[derive(Debug, PartialEq)]
pub enum AudioCaptureEvent {
    Samples {
        samples: AudioSamples,
        discontinuity: bool,
    },
    Idle,
    Unavailable,
    InvalidTimestamp,
}

pub trait SystemAudioCapture {
    type Error;

    /// Poll one bounded packet without waiting for sound or a missing device.
    fn next_samples(&mut self) -> Result<AudioCaptureEvent, Self::Error>;

    fn reset(&mut self) -> Result<(), Self::Error>;
}

pub trait RemoteInputSink {
    type Error;

    fn apply(&mut self, event: InputEvent) -> Result<(), Self::Error>;

    fn release_all(&mut self) -> Result<(), Self::Error>;

    fn healthy(&self) -> bool;
}

pub struct LatestQueue<T> {
    capacity: usize,
    items: Mutex<VecDeque<T>>,
    ready: Condvar,
}

impl<T> LatestQueue<T> {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "latest queue capacity must be positive");
        Self {
            capacity,
            items: Mutex::new(VecDeque::with_capacity(capacity)),
            ready: Condvar::new(),
        }
    }

    pub fn push(&self, item: T) -> Option<T> {
        let mut items = self.items.lock().unwrap_or_else(|error| error.into_inner());
        let dropped = (items.len() == self.capacity)
            .then(|| items.pop_front())
            .flatten();
        items.push_back(item);
        self.ready.notify_one();
        dropped
    }

    pub fn pop_timeout(&self, timeout: Duration) -> Option<T> {
        let mut items = self.items.lock().unwrap_or_else(|error| error.into_inner());
        if items.is_empty() {
            let (guard, _) = self
                .ready
                .wait_timeout(items, timeout)
                .unwrap_or_else(|error| error.into_inner());
            items = guard;
        }
        items.pop_front()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SessionId(u64);

impl SessionId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug)]
struct SessionRegistryState {
    next_id: u64,
    active_stream: Option<SessionId>,
    sessions: BTreeMap<SessionId, SessionLifecycle>,
}

#[derive(Clone, Debug)]
pub struct SessionRegistry {
    state: Arc<Mutex<SessionRegistryState>>,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(SessionRegistryState {
                next_id: 1,
                active_stream: None,
                sessions: BTreeMap::new(),
            })),
        }
    }

    pub fn connect(&self) -> Result<SessionHandle, SessionRegistryError> {
        let mut state = lock_registry(&self.state)?;
        let id = SessionId(state.next_id);
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or(SessionRegistryError::IdExhausted)?;
        state.sessions.insert(id, SessionLifecycle::connected());

        Ok(SessionHandle {
            id,
            state: Arc::clone(&self.state),
        })
    }

    pub fn active_stream(&self) -> Result<Option<SessionId>, SessionRegistryError> {
        Ok(lock_registry(&self.state)?.active_stream)
    }
}

#[derive(Clone, Debug)]
pub struct SessionHandle {
    id: SessionId,
    state: Arc<Mutex<SessionRegistryState>>,
}

impl SessionHandle {
    #[must_use]
    pub const fn id(&self) -> SessionId {
        self.id
    }

    pub fn phase(&self) -> Result<SessionPhase, SessionRegistryError> {
        let state = lock_registry(&self.state)?;
        state
            .sessions
            .get(&self.id)
            .copied()
            .map(SessionLifecycle::phase)
            .ok_or(SessionRegistryError::UnknownSession(self.id))
    }

    pub fn authenticate(&self) -> Result<(), SessionRegistryError> {
        self.update_lifecycle(SessionLifecycle::authenticate)
    }

    pub fn negotiate(&self) -> Result<(), SessionRegistryError> {
        self.update_lifecycle(SessionLifecycle::negotiate)
    }

    pub fn start_streaming(&self) -> Result<(), SessionRegistryError> {
        let mut state = lock_registry(&self.state)?;
        if let Some(active) = state.active_stream {
            return Err(SessionRegistryError::ActiveStreamExists(active));
        }

        state
            .sessions
            .get_mut(&self.id)
            .ok_or(SessionRegistryError::UnknownSession(self.id))?
            .start_streaming()?;
        state.active_stream = Some(self.id);
        Ok(())
    }

    pub fn close(&self) -> Result<SessionCloseEffects, SessionRegistryError> {
        let mut state = lock_registry(&self.state)?;
        let lifecycle = state
            .sessions
            .get_mut(&self.id)
            .ok_or(SessionRegistryError::UnknownSession(self.id))?;
        let release_input = lifecycle.phase() == SessionPhase::Streaming;
        lifecycle.close();

        if state.active_stream == Some(self.id) {
            state.active_stream = None;
        }

        Ok(SessionCloseEffects { release_input })
    }

    fn update_lifecycle(
        &self,
        update: fn(&mut SessionLifecycle) -> Result<(), SessionTransitionError>,
    ) -> Result<(), SessionRegistryError> {
        let mut state = lock_registry(&self.state)?;
        let lifecycle = state
            .sessions
            .get_mut(&self.id)
            .ok_or(SessionRegistryError::UnknownSession(self.id))?;
        update(lifecycle)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionCloseEffects {
    pub release_input: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionRegistryError {
    ActiveStreamExists(SessionId),
    UnknownSession(SessionId),
    InvalidTransition(SessionTransitionError),
    IdExhausted,
    RegistryUnavailable,
}

impl From<SessionTransitionError> for SessionRegistryError {
    fn from(error: SessionTransitionError) -> Self {
        Self::InvalidTransition(error)
    }
}

impl fmt::Display for SessionRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ActiveStreamExists(id) => {
                write!(formatter, "session {} is already streaming", id.get())
            }
            Self::UnknownSession(id) => write!(formatter, "unknown session {}", id.get()),
            Self::InvalidTransition(error) => error.fmt(formatter),
            Self::IdExhausted => formatter.write_str("session identifier space exhausted"),
            Self::RegistryUnavailable => formatter.write_str("session registry is unavailable"),
        }
    }
}

fn lock_registry(
    state: &Mutex<SessionRegistryState>,
) -> Result<MutexGuard<'_, SessionRegistryState>, SessionRegistryError> {
    state
        .lock()
        .map_err(|_| SessionRegistryError::RegistryUnavailable)
}

impl std::error::Error for SessionRegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidTransition(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockInputSink {
        release_count: usize,
    }

    impl RemoteInputSink for MockInputSink {
        type Error = std::convert::Infallible;

        fn apply(&mut self, _event: InputEvent) -> Result<(), Self::Error> {
            Ok(())
        }

        fn release_all(&mut self) -> Result<(), Self::Error> {
            self.release_count += 1;
            Ok(())
        }

        fn healthy(&self) -> bool {
            true
        }
    }

    fn ready_session(registry: &SessionRegistry) -> SessionHandle {
        let session = registry.connect().unwrap();
        session.authenticate().unwrap();
        session.negotiate().unwrap();
        session
    }

    fn path(capacity_bits_per_second: u64) -> VideoPathReport {
        VideoPathReport {
            round_trip_time: Duration::from_millis(10),
            congestion_window_bytes: capacity_bits_per_second / 800,
            lost_packets: 0,
        }
    }

    fn path_with_rtt(round_trip_millis: u64) -> VideoPathReport {
        VideoPathReport {
            round_trip_time: Duration::from_millis(round_trip_millis),
            congestion_window_bytes: 25_000_000,
            lost_packets: 0,
        }
    }

    fn delivery(completed_payload_bytes: u64) -> VideoDeliveryReport {
        VideoDeliveryReport {
            completed_payload_bytes,
            measurement_interval_micros: 500_000,
            ..VideoDeliveryReport::default()
        }
    }

    #[test]
    fn bitrate_starts_at_half_the_selected_maximum() {
        let controller = AdaptiveBitrateController::new(100_000_000);

        assert_eq!(controller.target_bits_per_second(), 50_000_000);
        assert_eq!(controller.estimated_capacity_bits_per_second(), 0);
    }

    #[test]
    fn bitrate_never_exceeds_the_selected_maximum() {
        let controller = AdaptiveBitrateController::new(2_500_000);

        assert_eq!(controller.target_bits_per_second(), 1_250_000);
    }

    #[test]
    fn startup_probe_uses_the_multiplicative_middle_and_seeds_a_provisional_ceiling() {
        let controller = AdaptiveBitrateController::from_startup_probe(100_000_000, 10_000_000);

        assert_eq!(controller.target_bits_per_second(), 3_162_277);
        assert_eq!(controller.estimated_capacity_bits_per_second(), 10_000_000);
        assert_eq!(
            controller.provisional_ceiling_bits_per_second,
            Some(10_000_000)
        );
        assert_eq!(controller.soft_ceiling_bits_per_second(), None);
        assert_eq!(
            controller.active_probe,
            Some(ActiveBitrateProbe {
                previous_target_bits_per_second: None,
                target_bits_per_second: 3_162_277,
                elapsed_micros: 0,
            })
        );
    }

    #[test]
    fn failed_startup_target_replaces_the_provisional_ceiling() {
        let mut controller = AdaptiveBitrateController::from_startup_probe(100_000_000, 10_000_000);
        let mut report = delivery(1_000_000);
        report.received_chunks = 196;
        report.lost_chunks = 4;

        controller.observe(path(200_000_000), report);

        assert_eq!(controller.target_bits_per_second(), 2_371_707);
        assert_eq!(
            controller.provisional_ceiling_bits_per_second,
            Some(3_162_277)
        );
        assert_eq!(controller.failed_probe_targets, VecDeque::from([3_162_277]));
    }

    #[test]
    fn recovery_closes_half_the_gap_to_ninety_five_percent_of_the_ceiling() {
        let mut controller = AdaptiveBitrateController::from_startup_probe(100_000_000, 10_000_000);

        for report in 1..10_u64 {
            assert_eq!(
                controller.observe(path(200_000_000), delivery(report * 625_000)),
                None
            );
        }
        assert_eq!(
            controller.observe(path(200_000_000), delivery(6_250_000)),
            Some(BitrateChange {
                target_bits_per_second: 3_952_846,
                reason: BitrateChangeReason::HealthyDelivery,
                cause: VideoBitrateChangeCause::HealthyDelivery,
            })
        );
    }

    #[test]
    fn startup_probe_failure_falls_back_to_one_megabit() {
        let controller = AdaptiveBitrateController::from_startup_probe(100_000_000, 0);

        assert_eq!(controller.target_bits_per_second(), VIDEO_BITRATE_BOOTSTRAP);
        assert_eq!(controller.provisional_ceiling_bits_per_second, None);
    }

    #[test]
    fn isolated_rtt_pressure_holds_bitrate_without_reducing_it() {
        let mut controller = AdaptiveBitrateController::new(20_000_000);
        controller.observe(path_with_rtt(10), delivery(625_000));

        assert_eq!(
            controller.observe(path_with_rtt(31), delivery(1_250_000)),
            None
        );
        assert_eq!(controller.target_bits_per_second(), 10_000_000);
        assert_eq!(controller.rtt_pressure_report_streak, 1);
        assert_eq!(
            controller.observe(path_with_rtt(10), delivery(1_875_000)),
            None
        );
        assert_eq!(controller.rtt_pressure_report_streak, 0);
    }

    #[test]
    fn consecutive_rtt_pressure_reports_reduce_ten_percent() {
        let mut controller = AdaptiveBitrateController::new(20_000_000);
        controller.observe(path_with_rtt(10), delivery(625_000));
        controller.observe(path_with_rtt(31), delivery(1_250_000));

        let change = controller
            .observe(path_with_rtt(31), delivery(1_875_000))
            .unwrap();

        assert_eq!(change.target_bits_per_second, 9_000_000);
        assert_eq!(change.reason, BitrateChangeReason::Congestion);
        assert_eq!(change.cause, VideoBitrateChangeCause::MildDegradation);
        assert_eq!(controller.rtt_pressure_report_streak, 0);
    }

    #[test]
    fn consecutive_rtt_pressure_rolls_back_an_active_increase() {
        let mut controller = AdaptiveBitrateController::new(20_000_000);
        controller.observe(path_with_rtt(10), delivery(625_000));
        controller.set_recovery_target(12_000_000);
        controller.observe(path_with_rtt(31), delivery(1_250_000));

        let change = controller
            .observe(path_with_rtt(31), delivery(1_875_000))
            .unwrap();

        assert_eq!(change.target_bits_per_second, 10_000_000);
        assert_eq!(change.reason, BitrateChangeReason::Congestion);
        assert_eq!(controller.probe_interval_micros, 10_000_000);
    }

    #[test]
    fn rtt_pressure_margin_scales_with_a_slower_baseline() {
        let mut controller = AdaptiveBitrateController::new(20_000_000);
        controller.observe(path_with_rtt(100), delivery(625_000));

        controller.observe(path_with_rtt(150), delivery(1_250_000));
        assert_eq!(controller.rtt_pressure_report_streak, 0);
        controller.observe(path_with_rtt(151), delivery(1_875_000));
        assert_eq!(controller.rtt_pressure_report_streak, 1);
    }

    #[test]
    fn estimated_capacity_averages_the_latest_two_reports() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        let path = path(80_000_000_000);

        controller.observe(path, delivery(625_000));
        assert_eq!(controller.estimated_capacity_bits_per_second(), 10_000_000);
        controller.observe(path, delivery(1_875_000));
        assert_eq!(controller.estimated_capacity_bits_per_second(), 15_000_000);
        controller.observe(path, delivery(3_750_000));
        assert_eq!(controller.estimated_capacity_bits_per_second(), 25_000_000);
        assert_eq!(controller.target_bits_per_second(), 50_000_000);
    }

    #[test]
    fn repeated_mild_receiver_loss_reduces_without_using_unsaturated_goodput() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        let mut report = delivery(1_000_000);
        report.received_chunks = 100;
        report.lost_chunks = 1;
        assert_eq!(controller.observe(path(200_000_000), report), None);
        report.completed_payload_bytes = 2_000_000;
        report.received_chunks = 200;
        report.lost_chunks = 2;
        let change = controller.observe(path(200_000_000), report).unwrap();

        assert_eq!(change.reason, BitrateChangeReason::Congestion);
        assert_eq!(change.cause, VideoBitrateChangeCause::MildDegradation);
        assert_eq!(change.target_bits_per_second, 45_000_000);
        assert_eq!(controller.estimated_capacity_bits_per_second(), 16_000_000);
    }

    #[test]
    fn transport_packet_loss_alone_does_not_prevent_recovery() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        let mut completed_payload_bytes = 0;

        for lost_packets in 1..=4 {
            completed_payload_bytes += 1_250_000;
            let mut report_path = path(200_000_000);
            report_path.lost_packets = lost_packets;
            let change = controller.observe(report_path, delivery(completed_payload_bytes));
            if lost_packets < 4 {
                assert_eq!(change, None);
            } else {
                assert_eq!(
                    change,
                    Some(BitrateChange {
                        target_bits_per_second: 55_000_000,
                        reason: BitrateChangeReason::HealthyDelivery,
                        cause: VideoBitrateChangeCause::HealthyDelivery,
                    })
                );
            }
        }

        assert_eq!(controller.soft_ceiling_bits_per_second(), None);
    }

    #[test]
    fn recovery_without_a_ceiling_uses_four_reports_and_a_ten_percent_step() {
        let mut controller = AdaptiveBitrateController::new(20_000_000);
        let report_path = path(200_000_000);

        for completed_payload_bytes in [1_250_000, 2_500_000, 3_750_000] {
            assert_eq!(
                controller.observe(report_path, delivery(completed_payload_bytes)),
                None
            );
        }
        assert_eq!(
            controller.observe(report_path, delivery(5_000_000)),
            Some(BitrateChange {
                target_bits_per_second: 11_000_000,
                reason: BitrateChangeReason::HealthyDelivery,
                cause: VideoBitrateChangeCause::HealthyDelivery,
            })
        );
    }

    #[test]
    fn intermittent_transport_loss_does_not_keep_a_stale_soft_ceiling() {
        let mut controller = AdaptiveBitrateController::new(20_000_000);
        controller.maximum_bits_per_second = controller.target_bits_per_second;
        controller.failed_probe_targets = VecDeque::from([11_000_000; 5]);
        let mut completed_payload_bytes = 0;
        let mut lost_packets = 0;

        for report_index in 0..120 {
            completed_payload_bytes += 1_250_000;
            if report_index % 2 == 0 {
                lost_packets += 1;
            }
            let mut report_path = path(200_000_000);
            report_path.lost_packets = lost_packets;
            controller.observe(report_path, delivery(completed_payload_bytes));
        }

        assert_eq!(controller.soft_ceiling_bits_per_second(), None);
        assert_eq!(controller.target_bits_per_second(), 10_000_000);
    }

    #[test]
    fn severe_receiver_loss_uses_the_bounded_twenty_five_percent_cut() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        controller.target_bits_per_second = 20_000_000;
        let mut report = delivery(1_000_000);
        report.received_chunks = 196;
        report.lost_chunks = 4;

        let change = controller.observe(path(200_000_000), report).unwrap();

        assert_eq!(change.reason, BitrateChangeReason::Congestion);
        assert_eq!(change.cause, VideoBitrateChangeCause::SevereReceiverLoss);
        assert_eq!(change.target_bits_per_second, 15_000_000);
        assert_eq!(controller.estimated_capacity_bits_per_second(), 16_000_000);
    }

    #[test]
    fn severe_loss_during_a_probe_restores_the_previous_target() {
        let mut controller = AdaptiveBitrateController::new(20_000_000);
        assert_eq!(controller.target_bits_per_second(), 10_000_000);
        controller.set_recovery_target(10_200_000);
        let mut report = delivery(1_000_000);
        report.received_chunks = 196;
        report.lost_chunks = 4;

        let change = controller.observe(path(200_000_000), report).unwrap();

        assert_eq!(change.reason, BitrateChangeReason::Congestion);
        assert_eq!(change.cause, VideoBitrateChangeCause::SevereReceiverLoss);
        assert_eq!(change.target_bits_per_second, 10_000_000);
        assert_eq!(controller.probe_interval_micros, 10_000_000);
        assert_eq!(controller.recovery_cooldown_micros, 10_000_000);
    }

    #[test]
    fn persistent_severe_loss_after_a_probe_rollback_cuts_twenty_five_percent() {
        let mut controller = AdaptiveBitrateController::new(20_000_000);
        controller.set_recovery_target(10_200_000);
        let mut first = delivery(1_000_000);
        first.received_chunks = 196;
        first.lost_chunks = 4;
        controller.observe(path(200_000_000), first);
        let mut second = delivery(2_000_000);
        second.received_chunks = 392;
        second.lost_chunks = 8;

        let change = controller.observe(path(200_000_000), second).unwrap();

        assert_eq!(change.target_bits_per_second, 7_500_000);
        assert_eq!(change.cause, VideoBitrateChangeCause::SevereReceiverLoss);
    }

    #[test]
    fn failed_probe_backoff_doubles_to_forty_seconds() {
        let mut controller = AdaptiveBitrateController::new(20_000_000);

        for expected in [10_000_000, 20_000_000, 40_000_000, 40_000_000] {
            controller.active_probe = Some(ActiveBitrateProbe {
                previous_target_bits_per_second: Some(10_000_000),
                target_bits_per_second: 10_200_000,
                elapsed_micros: 0,
            });
            assert_eq!(controller.record_active_probe_failure(), Some(10_000_000));
            assert_eq!(controller.probe_interval_micros, expected);
        }
    }

    #[test]
    fn rollback_backoff_waits_ten_clean_seconds_before_another_probe() {
        let mut controller = AdaptiveBitrateController::new(20_000_000);
        controller.set_recovery_target(10_200_000);
        let mut loss = delivery(1_000_000);
        loss.received_chunks = 196;
        loss.lost_chunks = 4;
        controller.observe(path(200_000_000), loss);

        for report_index in 1..20_u64 {
            let mut clean = delivery(1_000_000 + report_index * 625_000);
            clean.received_chunks = 196;
            clean.lost_chunks = 4;
            assert_eq!(controller.observe(path(200_000_000), clean), None);
        }
        let mut clean = delivery(13_500_000);
        clean.received_chunks = 196;
        clean.lost_chunks = 4;
        assert_eq!(
            controller.observe(path(200_000_000), clean),
            Some(BitrateChange {
                target_bits_per_second: 10_200_000,
                reason: BitrateChangeReason::HealthyDelivery,
                cause: VideoBitrateChangeCause::HealthyDelivery,
            })
        );
    }

    #[test]
    fn isolated_sender_congestion_holds_the_target() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);

        assert_eq!(controller.observe_sender_congestion(), None);
        assert_eq!(controller.target_bits_per_second(), 50_000_000);
        assert_eq!(
            controller.observe(path(200_000_000), delivery(1_250_000)),
            None
        );
        assert_eq!(controller.target_bits_per_second(), 50_000_000);
    }

    #[test]
    fn bitrate_does_not_fall_below_one_megabit() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        controller.target_bits_per_second = 1_100_000;
        assert_eq!(controller.observe_sender_congestion(), None);
        assert_eq!(controller.observe_sender_congestion(), None);
        let change = controller.observe_sender_congestion().unwrap();

        assert_eq!(change.target_bits_per_second, VIDEO_BITRATE_BOOTSTRAP);
        assert_eq!(controller.target_bits_per_second(), VIDEO_BITRATE_BOOTSTRAP);
    }

    #[test]
    fn zero_measurement_interval_leaves_the_last_measurement_unchanged() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        controller.observe(path(20_000_000), delivery(625_000));
        let capacity = controller.estimated_capacity_bits_per_second();
        let mut invalid = delivery(1_250_000);
        invalid.measurement_interval_micros = 0;

        assert_eq!(controller.observe(path(20_000_000), invalid), None);
        assert_eq!(controller.estimated_capacity_bits_per_second(), capacity);
    }

    #[test]
    fn enormous_window_cannot_override_receiver_goodput() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        let enormous_path = path(80_000_000_000);

        controller.observe(enormous_path, delivery(625_000));
        controller.observe(enormous_path, delivery(1_250_000));

        assert_eq!(controller.estimated_capacity_bits_per_second(), 10_000_000);
        assert_eq!(controller.target_bits_per_second(), 50_000_000);
    }

    #[test]
    fn recovery_waits_five_clean_seconds_then_uses_a_bounded_step() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        let mut report = delivery(1_250_000);
        report.received_chunks = 196;
        report.lost_chunks = 4;
        controller.observe(path(80_000_000), report);
        assert_eq!(controller.target_bits_per_second(), 37_500_000);

        for completed_payload_bytes in (2..=10).map(|report| report * 1_250_000) {
            assert_eq!(
                controller.observe(path(80_000_000), delivery(completed_payload_bytes)),
                None
            );
        }
        assert_eq!(
            controller.observe(path(80_000_000), delivery(13_750_000)),
            Some(BitrateChange {
                target_bits_per_second: 41_250_000,
                reason: BitrateChangeReason::HealthyDelivery,
                cause: VideoBitrateChangeCause::HealthyDelivery,
            })
        );
    }

    #[test]
    fn stable_higher_rtt_does_not_prevent_full_recovery() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        let mut congested = delivery(1_250_000);
        congested.received_chunks = 196;
        congested.lost_chunks = 4;
        controller.observe(path(200_000_000), congested);
        assert_eq!(controller.target_bits_per_second(), 37_500_000);

        let higher_rtt_path = VideoPathReport {
            round_trip_time: Duration::from_millis(20),
            congestion_window_bytes: 500_000,
            lost_packets: 0,
        };
        let mut completed_payload_bytes = 1_250_000;
        for _ in 0..120 {
            completed_payload_bytes += 1_250_000;
            controller.observe(higher_rtt_path, delivery(completed_payload_bytes));
        }

        assert_eq!(controller.target_bits_per_second(), 100_000_000);
    }

    #[test]
    fn repeated_sender_stalls_cut_once_within_one_report() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        assert_eq!(controller.observe_sender_congestion(), None);
        assert_eq!(controller.observe_sender_congestion(), None);
        assert_eq!(
            controller
                .observe_sender_congestion()
                .unwrap()
                .target_bits_per_second,
            37_500_000
        );
        for _ in 0..120 {
            assert_eq!(controller.observe_sender_congestion(), None);
        }
        assert_eq!(controller.target_bits_per_second(), 37_500_000);
    }

    #[test]
    fn consecutive_sender_pressure_reports_reduce_ten_percent() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        let report_path = path(200_000_000);

        assert_eq!(controller.observe_sender_congestion(), None);
        assert_eq!(controller.observe(report_path, delivery(625_000)), None);
        assert_eq!(controller.observe_sender_congestion(), None);
        assert_eq!(
            controller
                .observe(report_path, delivery(1_250_000))
                .unwrap()
                .target_bits_per_second,
            45_000_000
        );
    }

    #[test]
    fn sender_congestion_interval_cannot_count_toward_recovery() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        controller.observe_sender_congestion();
        assert_eq!(
            controller.observe(path(200_000_000), delivery(5_000_000)),
            None
        );
        for bytes in [10_000_000, 15_000_000, 20_000_000] {
            assert_eq!(controller.observe(path(200_000_000), delivery(bytes)), None);
        }
        assert_eq!(
            controller.observe(path(200_000_000), delivery(25_000_000)),
            Some(BitrateChange {
                target_bits_per_second: 55_000_000,
                reason: BitrateChangeReason::HealthyDelivery,
                cause: VideoBitrateChangeCause::HealthyDelivery,
            })
        );
    }

    #[test]
    fn invalid_report_does_not_clear_sender_pressure() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        assert_eq!(controller.observe_sender_congestion(), None);
        controller.observe(path(200_000_000), VideoDeliveryReport::default());
        assert_eq!(controller.observe_sender_congestion(), None);
        let change = controller.observe_sender_congestion().unwrap();
        assert_eq!(change.cause, VideoBitrateChangeCause::SenderCongestion);
        assert_eq!(controller.target_bits_per_second(), 37_500_000);
    }

    #[test]
    fn one_failed_recovery_probe_does_not_establish_a_soft_ceiling() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        for _ in 0..3 {
            controller.observe_sender_congestion();
        }
        for completed_payload_bytes in (1..=14).map(|report| report * 625_000) {
            controller.observe(path(200_000_000), delivery(completed_payload_bytes));
        }
        assert_eq!(controller.target_bits_per_second(), 41_250_000);

        for _ in 0..3 {
            controller.observe_sender_congestion();
        }

        assert_eq!(controller.soft_ceiling_bits_per_second(), None);
        assert_eq!(controller.target_bits_per_second(), 30_937_500);
    }

    #[test]
    fn soft_ceiling_is_the_average_of_the_latest_five_failures() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        for target in [
            10_000_000, 20_000_000, 30_000_000, 40_000_000, 50_000_000, 60_000_000,
        ] {
            controller.active_probe = Some(ActiveBitrateProbe {
                previous_target_bits_per_second: None,
                target_bits_per_second: target,
                elapsed_micros: 0,
            });
            controller.record_active_probe_failure();
        }

        assert_eq!(controller.soft_ceiling_bits_per_second(), Some(40_000_000));
    }

    #[test]
    fn soft_ceiling_requires_five_failed_probes() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        for target in [20_000_000, 21_000_000, 22_000_000, 23_000_000] {
            controller.active_probe = Some(ActiveBitrateProbe {
                previous_target_bits_per_second: None,
                target_bits_per_second: target,
                elapsed_micros: 0,
            });
            controller.record_active_probe_failure();
            assert_eq!(controller.soft_ceiling_bits_per_second(), None);
        }
        controller.active_probe = Some(ActiveBitrateProbe {
            previous_target_bits_per_second: None,
            target_bits_per_second: 24_000_000,
            elapsed_micros: 0,
        });
        controller.record_active_probe_failure();

        assert_eq!(controller.soft_ceiling_bits_per_second(), Some(22_000_000));
    }

    #[test]
    fn recovery_near_the_soft_ceiling_waits_five_seconds_and_uses_a_small_step() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        controller.failed_probe_targets = VecDeque::from([20_000_000; 5]);
        controller.target_bits_per_second = 19_600_000;

        for report in 1..10_u64 {
            assert_eq!(
                controller.observe(path(200_000_000), delivery(report * 1_250_000)),
                None
            );
        }
        assert_eq!(
            controller.observe(path(200_000_000), delivery(12_500_000)),
            Some(BitrateChange {
                target_bits_per_second: 19_992_000,
                reason: BitrateChangeReason::HealthyDelivery,
                cause: VideoBitrateChangeCause::HealthyDelivery,
            })
        );
    }

    #[test]
    fn ordinary_recovery_does_not_jump_past_a_learned_soft_ceiling() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        controller.failed_probe_targets = VecDeque::from([20_000_000; 5]);
        controller.target_bits_per_second = 15_000_000;

        for report in 1..4_u64 {
            assert_eq!(
                controller.observe(path(200_000_000), delivery(report * 1_250_000)),
                None
            );
        }
        assert_eq!(
            controller.observe(path(200_000_000), delivery(5_000_000)),
            Some(BitrateChange {
                target_bits_per_second: 17_000_000,
                reason: BitrateChangeReason::HealthyDelivery,
                cause: VideoBitrateChangeCause::HealthyDelivery,
            })
        );
        for report in 5..14_u64 {
            assert_eq!(
                controller.observe(path(200_000_000), delivery(report * 1_250_000)),
                None
            );
        }
        assert_eq!(
            controller.observe(path(200_000_000), delivery(17_500_000)),
            Some(BitrateChange {
                target_bits_per_second: 18_000_000,
                reason: BitrateChangeReason::HealthyDelivery,
                cause: VideoBitrateChangeCause::HealthyDelivery,
            })
        );
    }

    #[test]
    fn sustained_healthy_delivery_expires_a_learned_soft_ceiling() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        controller.failed_probe_targets = VecDeque::from([20_000_000; 5]);
        controller.target_bits_per_second = 10_000_000;

        for report in 1..=120_u64 {
            controller.observe(path(200_000_000), delivery(report * 1_250_000));
        }

        assert_eq!(controller.soft_ceiling_bits_per_second(), None);
    }

    #[test]
    fn cloned_handles_share_one_session_object() {
        let registry = SessionRegistry::new();
        let session = registry.connect().unwrap();
        let clone = session.clone();

        clone.authenticate().unwrap();

        assert_eq!(session.phase().unwrap(), SessionPhase::Authenticated);
        assert_eq!(session.id(), clone.id());
    }

    #[test]
    fn second_viewer_is_rejected_while_first_streams() {
        let registry = SessionRegistry::new();
        let first = ready_session(&registry);
        let second = ready_session(&registry);
        first.start_streaming().unwrap();

        assert_eq!(
            second.start_streaming(),
            Err(SessionRegistryError::ActiveStreamExists(first.id()))
        );
        assert_eq!(second.phase().unwrap(), SessionPhase::Negotiated);
    }

    #[test]
    fn closing_active_viewer_releases_input_and_allows_waiting_viewer() {
        let registry = SessionRegistry::new();
        let first = ready_session(&registry);
        let second = ready_session(&registry);
        let mut input = MockInputSink::default();
        first.start_streaming().unwrap();

        let effects = first.close().unwrap();
        if effects.release_input {
            input.release_all().unwrap();
        }
        second.start_streaming().unwrap();

        assert_eq!(input.release_count, 1);
        assert_eq!(registry.active_stream().unwrap(), Some(second.id()));
    }

    #[test]
    fn closing_non_streaming_viewer_does_not_release_active_input() {
        let registry = SessionRegistry::new();
        let first = ready_session(&registry);
        let second = ready_session(&registry);
        first.start_streaming().unwrap();

        assert_eq!(
            second.close().unwrap(),
            SessionCloseEffects {
                release_input: false,
            }
        );
        assert_eq!(registry.active_stream().unwrap(), Some(first.id()));
    }

    #[test]
    fn poisoned_registry_is_reported_instead_of_panicking_callers() {
        let registry = SessionRegistry::new();
        let state = Arc::clone(&registry.state);
        let _ = std::thread::spawn(move || {
            let _guard = state.lock().unwrap();
            panic!("poison registry for test");
        })
        .join();

        assert_eq!(
            registry.active_stream(),
            Err(SessionRegistryError::RegistryUnavailable)
        );
        assert!(matches!(
            registry.connect(),
            Err(SessionRegistryError::RegistryUnavailable)
        ));
    }

    #[test]
    fn latest_queue_discards_the_oldest_pending_item() {
        let queue = LatestQueue::new(2);

        assert_eq!(queue.push(1), None);
        assert_eq!(queue.push(2), None);
        assert_eq!(queue.push(3), Some(1));
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.pop_timeout(Duration::ZERO), Some(2));
        assert_eq!(queue.pop_timeout(Duration::ZERO), Some(3));
        assert!(queue.is_empty());
    }
}
