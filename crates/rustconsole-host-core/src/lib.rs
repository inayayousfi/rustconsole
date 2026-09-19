//! Platform-neutral host session orchestration.

pub mod video_recovery;

use rustconsole_media::{AudioSamples, VideoFormat, VideoFrame};
use rustconsole_protocol::InputEvent;
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
const SAFE_PATH_PERCENT: u128 = 100;
const RECOVERY_PROBE_PERCENT: u128 = 110;
const SOFT_CEILING_PROBE_PERCENT: u128 = 102;
const SOFT_CEILING_RANGE_PERCENT: u128 = 98;
const SOFT_CEILING_PROBE_INTERVAL_MICROS: u64 = 5_000_000;
const FAILED_PROBE_ATTRIBUTION_MICROS: u64 = 2_000_000;
const FAILED_PROBE_AVERAGE_SAMPLES: usize = 5;
const CAPACITY_SAMPLE_SATURATION_PERCENT: u128 = 80;
const HEALTHY_REPORTS_BEFORE_RECOVERY: u8 = 2;
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
}

#[derive(Debug)]
pub struct AdaptiveBitrateController {
    maximum_bits_per_second: u64,
    target_bits_per_second: u64,
    estimated_capacity_bits_per_second: u64,
    delivery_samples: VecDeque<(u64, u64)>,
    minimum_round_trip_time: Option<Duration>,
    previous_delivery: VideoDeliveryReport,
    previous_lost_packets: u64,
    healthy_reports: u8,
    soft_ceiling_healthy_micros: u64,
    failed_probe_targets: VecDeque<u64>,
    active_probe: Option<(u64, u64)>,
    sender_congested_since_report: bool,
}

impl AdaptiveBitrateController {
    #[must_use]
    pub fn new(maximum_bits_per_second: u64) -> Self {
        let maximum_bits_per_second = maximum_bits_per_second.max(VIDEO_BITRATE_BOOTSTRAP);
        Self {
            maximum_bits_per_second,
            target_bits_per_second: maximum_bits_per_second,
            estimated_capacity_bits_per_second: 0,
            delivery_samples: VecDeque::with_capacity(CAPACITY_AVERAGE_REPORTS),
            minimum_round_trip_time: None,
            previous_delivery: VideoDeliveryReport::default(),
            previous_lost_packets: 0,
            healthy_reports: 0,
            soft_ceiling_healthy_micros: 0,
            failed_probe_targets: VecDeque::with_capacity(FAILED_PROBE_AVERAGE_SAMPLES),
            active_probe: None,
            sender_congested_since_report: false,
        }
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
        let completed_payload_bytes = delivery
            .completed_payload_bytes
            .saturating_sub(self.previous_delivery.completed_payload_bytes);
        let receiver_loss = delivery.lost_chunks > self.previous_delivery.lost_chunks
            || delivery.late_chunks > self.previous_delivery.late_chunks
            || delivery.assembly_overflows > self.previous_delivery.assembly_overflows
            || delivery.incomplete_frames > self.previous_delivery.incomplete_frames;
        let packet_loss = path.lost_packets > self.previous_lost_packets;
        self.previous_delivery = delivery;
        self.previous_lost_packets = path.lost_packets;

        if delivery.measurement_interval_micros == 0 {
            return None;
        }
        let sender_congested = std::mem::take(&mut self.sender_congested_since_report);
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

        if receiver_loss || packet_loss {
            self.record_active_probe_failure();
            self.reset_recovery_wait();
            let mut target = percentage(self.target_bits_per_second, CONGESTION_TARGET_PERCENT)
                .min(safe_path_capacity);
            if capacity
                >= percentage(
                    self.target_bits_per_second,
                    CAPACITY_SAMPLE_SATURATION_PERCENT,
                )
            {
                target = target.min(congestion_target(capacity));
            }
            return self.set_target(target, BitrateChangeReason::Congestion);
        }

        if safe_path_capacity < self.target_bits_per_second {
            self.record_active_probe_failure();
            self.reset_recovery_wait();
            return self.set_target(safe_path_capacity, BitrateChangeReason::Congestion);
        }

        if sender_congested {
            self.reset_recovery_wait();
            return None;
        }

        self.advance_active_probe(delivery.measurement_interval_micros);
        if self.target_bits_per_second == self.maximum_bits_per_second {
            return None;
        }

        if self.near_soft_ceiling() {
            self.healthy_reports = 0;
            self.soft_ceiling_healthy_micros = self
                .soft_ceiling_healthy_micros
                .saturating_add(delivery.measurement_interval_micros);
            if self.soft_ceiling_healthy_micros < SOFT_CEILING_PROBE_INTERVAL_MICROS {
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
        self.set_recovery_target(
            percentage(self.target_bits_per_second, RECOVERY_PROBE_PERCENT).min(safe_path_capacity),
        )
    }

    pub fn observe_sender_congestion(&mut self) -> Option<BitrateChange> {
        self.reset_recovery_wait();
        // A brief drain between stalled frames is not evidence that congestion has cleared.
        if self.sender_congested_since_report {
            return None;
        }
        self.sender_congested_since_report = true;
        self.record_active_probe_failure();
        self.set_target(
            percentage(self.target_bits_per_second, CONGESTION_TARGET_PERCENT),
            BitrateChangeReason::Congestion,
        )
    }

    fn near_soft_ceiling(&self) -> bool {
        self.soft_ceiling_bits_per_second().is_some_and(|ceiling| {
            self.target_bits_per_second >= percentage(ceiling, SOFT_CEILING_RANGE_PERCENT)
        })
    }

    fn set_recovery_target(&mut self, target_bits_per_second: u64) -> Option<BitrateChange> {
        let change = self.set_target(target_bits_per_second, BitrateChangeReason::HealthyDelivery);
        if let Some(change) = change {
            self.active_probe = Some((change.target_bits_per_second, 0));
        }
        change
    }

    fn advance_active_probe(&mut self, interval_micros: u64) {
        let Some((_, elapsed_micros)) = self.active_probe.as_mut() else {
            return;
        };
        *elapsed_micros = elapsed_micros.saturating_add(interval_micros);
        if *elapsed_micros > FAILED_PROBE_ATTRIBUTION_MICROS {
            self.active_probe = None;
        }
    }

    fn record_active_probe_failure(&mut self) {
        let Some((target, _)) = self.active_probe.take() else {
            return;
        };
        if self.failed_probe_targets.len() == FAILED_PROBE_AVERAGE_SAMPLES {
            self.failed_probe_targets.pop_front();
        }
        self.failed_probe_targets.push_back(target);
    }

    fn reset_recovery_wait(&mut self) {
        self.healthy_reports = 0;
        self.soft_ceiling_healthy_micros = 0;
    }

    fn set_target(
        &mut self,
        target_bits_per_second: u64,
        reason: BitrateChangeReason,
    ) -> Option<BitrateChange> {
        let target = target_bits_per_second
            .max(VIDEO_BITRATE_BOOTSTRAP)
            .min(self.maximum_bits_per_second);
        if target == self.target_bits_per_second {
            return None;
        }
        self.target_bits_per_second = target;
        Some(BitrateChange {
            target_bits_per_second: target,
            reason,
        })
    }
}

fn congestion_target(capacity_bits_per_second: u64) -> u64 {
    percentage(capacity_bits_per_second, CONGESTION_TARGET_PERCENT)
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

    fn delivery(completed_payload_bytes: u64) -> VideoDeliveryReport {
        VideoDeliveryReport {
            completed_payload_bytes,
            measurement_interval_micros: 500_000,
            ..VideoDeliveryReport::default()
        }
    }

    #[test]
    fn bitrate_starts_at_the_selected_maximum() {
        let controller = AdaptiveBitrateController::new(100_000_000);

        assert_eq!(controller.target_bits_per_second(), 100_000_000);
        assert_eq!(controller.estimated_capacity_bits_per_second(), 0);
    }

    #[test]
    fn bitrate_never_exceeds_the_selected_maximum() {
        let controller = AdaptiveBitrateController::new(2_500_000);

        assert_eq!(controller.target_bits_per_second(), 2_500_000);
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
        assert_eq!(controller.target_bits_per_second(), 100_000_000);
    }

    #[test]
    fn receiver_loss_does_not_treat_unsaturated_output_as_path_capacity() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        let mut report = delivery(1_000_000);
        report.lost_chunks = 1;
        let change = controller.observe(path(200_000_000), report).unwrap();

        assert_eq!(change.reason, BitrateChangeReason::Congestion);
        assert_eq!(change.target_bits_per_second, 75_000_000);
        assert_eq!(controller.estimated_capacity_bits_per_second(), 16_000_000);
    }

    #[test]
    fn receiver_loss_uses_goodput_when_the_encoder_was_saturating_the_path() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        controller.target_bits_per_second = 20_000_000;
        let mut report = delivery(1_000_000);
        report.lost_chunks = 1;

        let change = controller.observe(path(200_000_000), report).unwrap();

        assert_eq!(change.reason, BitrateChangeReason::Congestion);
        assert_eq!(change.target_bits_per_second, 12_000_000);
        assert_eq!(controller.estimated_capacity_bits_per_second(), 16_000_000);
    }

    #[test]
    fn sender_congestion_reduces_the_target_immediately() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);

        let change = controller.observe_sender_congestion().unwrap();

        assert_eq!(change.reason, BitrateChangeReason::Congestion);
        assert_eq!(change.target_bits_per_second, 75_000_000);
    }

    #[test]
    fn bitrate_does_not_fall_below_one_megabit() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        let mut report = delivery(1);
        report.incomplete_frames = 1;
        let change = controller.observe(path(500_000), report).unwrap();

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
        assert_eq!(controller.target_bits_per_second(), 100_000_000);
    }

    #[test]
    fn recovery_requires_two_clean_reports_and_uses_a_small_step() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        let mut report = delivery(1_250_000);
        report.lost_chunks = 1;
        controller.observe(path(80_000_000), report);
        assert_eq!(controller.target_bits_per_second(), 75_000_000);

        assert_eq!(
            controller.observe(path(80_000_000), delivery(2_500_000)),
            None
        );
        assert_eq!(
            controller.observe(path(80_000_000), delivery(3_750_000)),
            Some(BitrateChange {
                target_bits_per_second: 80_000_000,
                reason: BitrateChangeReason::HealthyDelivery,
            })
        );
    }

    #[test]
    fn stable_higher_rtt_does_not_prevent_full_recovery() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        let mut congested = delivery(1_250_000);
        congested.lost_chunks = 1;
        controller.observe(path(200_000_000), congested);
        assert_eq!(controller.target_bits_per_second(), 75_000_000);

        let higher_rtt_path = VideoPathReport {
            round_trip_time: Duration::from_millis(20),
            congestion_window_bytes: 500_000,
            lost_packets: 0,
        };
        let mut completed_payload_bytes = 1_250_000;
        for _ in 0..40 {
            completed_payload_bytes += 1_250_000;
            controller.observe(higher_rtt_path, delivery(completed_payload_bytes));
        }

        assert_eq!(controller.target_bits_per_second(), 100_000_000);
    }

    #[test]
    fn repeated_sender_stalls_cannot_collapse_bitrate_within_one_report() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        controller.observe_sender_congestion().unwrap();
        for _ in 0..120 {
            assert_eq!(controller.observe_sender_congestion(), None);
        }
        assert_eq!(controller.target_bits_per_second(), 75_000_000);
        assert_eq!(
            controller.observe(path(200_000_000), delivery(5_000_000)),
            None
        );
        assert_eq!(controller.target_bits_per_second(), 75_000_000);
        assert_eq!(
            controller
                .observe_sender_congestion()
                .unwrap()
                .target_bits_per_second,
            56_250_000
        );
    }

    #[test]
    fn sender_congestion_interval_cannot_count_toward_recovery() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        controller.observe_sender_congestion();
        for bytes in [5_000_000, 10_000_000] {
            assert_eq!(controller.observe(path(200_000_000), delivery(bytes)), None);
        }
        let change = controller
            .observe(path(200_000_000), delivery(15_000_000))
            .unwrap();
        assert_eq!(change.target_bits_per_second, 82_500_000);
        assert_eq!(change.reason, BitrateChangeReason::HealthyDelivery);
    }

    #[test]
    fn invalid_report_does_not_rearm_sender_bitrate_cuts() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        controller.observe_sender_congestion();
        controller.observe(path(200_000_000), VideoDeliveryReport::default());
        assert_eq!(controller.observe_sender_congestion(), None);
        assert_eq!(controller.target_bits_per_second(), 75_000_000);
    }

    #[test]
    fn one_failed_recovery_probe_does_not_establish_a_soft_ceiling() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        controller.observe_sender_congestion();
        for completed_payload_bytes in [5_000_000, 10_000_000, 15_000_000] {
            controller.observe(path(200_000_000), delivery(completed_payload_bytes));
        }
        assert_eq!(controller.target_bits_per_second(), 82_500_000);

        controller.observe_sender_congestion();

        assert_eq!(controller.soft_ceiling_bits_per_second(), None);
        assert_eq!(controller.target_bits_per_second(), 61_875_000);
    }

    #[test]
    fn soft_ceiling_is_the_average_of_the_latest_five_failures() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        for target in [
            10_000_000, 20_000_000, 30_000_000, 40_000_000, 50_000_000, 60_000_000,
        ] {
            controller.active_probe = Some((target, 0));
            controller.record_active_probe_failure();
        }

        assert_eq!(controller.soft_ceiling_bits_per_second(), Some(40_000_000));
    }

    #[test]
    fn soft_ceiling_requires_five_failed_probes() {
        let mut controller = AdaptiveBitrateController::new(100_000_000);
        for target in [20_000_000, 21_000_000, 22_000_000, 23_000_000] {
            controller.active_probe = Some((target, 0));
            controller.record_active_probe_failure();
            assert_eq!(controller.soft_ceiling_bits_per_second(), None);
        }
        controller.active_probe = Some((24_000_000, 0));
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
            })
        );
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
