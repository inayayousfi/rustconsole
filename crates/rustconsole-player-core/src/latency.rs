//! Clock-adjusted playback measurements and bounded feedback for host policy.

use crate::ClockOffsetEstimate;
use rustconsole_protocol::latency::{LatencyMeasurement, LatencyReport, MAX_MEASUREMENT_MICROS};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CLOCK_MAX_AGE: Duration = Duration::from_secs(5);
const INPUT_ACTIVE_TIME: Duration = Duration::from_secs(1);
const MAX_PENDING_INPUTS: usize = 1024;

pub fn budget_failure_message(
    failure: rustconsole_protocol::latency::LatencyBudgetFailure,
) -> String {
    let channel = rustconsole_protocol::latency::LatencyChannel::try_from(failure.channel).map_or(
        "unknown delay",
        rustconsole_protocol::latency::LatencyChannel::label,
    );
    format!(
        "Session ended because the delay limit could not be met. The {channel} delay was {:.1} ms{} for 5 continuous seconds after reducing video bitrate to the minimum of 1 Mbit/s. Allowed delay: {:.1} ms; client maximum: {:.1} ms; healthy baseline: {:.1} ms. Try a higher maximum delay or reduce network or device load before reconnecting.",
        failure.observed_delay_micros as f64 / 1_000.0,
        if failure.estimated {
            " (estimated)"
        } else {
            ""
        },
        failure.budget_micros as f64 / 1_000.0,
        failure.absolute_limit_micros as f64 / 1_000.0,
        failure.baseline_micros as f64 / 1_000.0,
    )
}

pub fn capture_player_at(
    offset: ClockOffsetEstimate,
    captured_at_micros: u64,
    assembled_at_micros: u64,
    assembled_at: Instant,
) -> Option<Instant> {
    let player_capture = i128::from(captured_at_micros) - i128::from(offset.offset_micros);
    let age = u64::try_from(i128::from(assembled_at_micros) - player_capture).ok()?;
    assembled_at.checked_sub(Duration::from_micros(age))
}

pub fn presentation_delay(captured: Option<Instant>, presented: Instant) -> Option<Duration> {
    captured.map(|at| presented.saturating_duration_since(at))
}

#[derive(Default)]
pub struct VideoPresentationLatency {
    last_capture: Option<u64>,
    latest: Option<Duration>,
    last_presented: Option<Instant>,
    estimated: bool,
}

impl VideoPresentationLatency {
    pub fn observe(
        &mut self,
        capture: u64,
        delay: Option<Duration>,
        estimated: bool,
        presented: Instant,
    ) -> Option<Duration> {
        self.last_presented = Some(presented);
        if self.last_capture == Some(capture) {
            return None;
        }
        self.last_capture = Some(capture);
        self.latest = delay;
        self.estimated = estimated;
        delay
    }
    pub fn latest(&self) -> Option<Duration> {
        self.latest
    }
    pub fn estimated(&self) -> bool {
        self.estimated
    }
    pub fn stalled(&self, now: Instant, timeout: Duration) -> bool {
        self.last_presented
            .is_some_and(|at| now.saturating_duration_since(at) >= timeout)
    }
}

#[derive(Default)]
struct State {
    clock: Option<(ClockOffsetEstimate, Instant)>,
    video: Option<LatencyMeasurement>,
    audio: Option<LatencyMeasurement>,
    input: Option<LatencyMeasurement>,
    audio_active: bool,
    last_input: Option<Instant>,
    pending_input: BTreeMap<u64, Instant>,
    video_presentation: VideoPresentationLatency,
    presented_bitrate: u64,
}

#[derive(Clone, Default)]
pub struct LatencyMeasurements(Arc<Mutex<State>>);

fn observe(
    slot: &mut Option<LatencyMeasurement>,
    delay: Duration,
    uncertainty: u64,
    estimated: bool,
) {
    let delay_micros = u64::try_from(delay.as_micros()).unwrap_or(u64::MAX);
    if delay_micros > MAX_MEASUREMENT_MICROS || uncertainty > MAX_MEASUREMENT_MICROS {
        return;
    }
    match slot {
        Some(value) => {
            value.delay_micros = value.delay_micros.max(delay_micros);
            value.uncertainty_micros = value.uncertainty_micros.max(uncertainty);
            value.estimated |= estimated;
            value.samples = value.samples.saturating_add(1).min(65_536);
        }
        None => {
            *slot = Some(LatencyMeasurement {
                delay_micros,
                uncertainty_micros: uncertainty,
                estimated,
                samples: 1,
            })
        }
    }
}

impl LatencyMeasurements {
    pub fn set_clock(&self, estimate: ClockOffsetEstimate, now: Instant) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.clock.is_none_or(|(current, at)| {
            now.saturating_duration_since(at) >= CLOCK_MAX_AGE
                || estimate.uncertainty_micros <= current.uncertainty_micros
        }) {
            state.clock = Some((estimate, now));
        }
    }

    pub fn clock(&self, now: Instant) -> Option<ClockOffsetEstimate> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clock
            .filter(|(_, at)| now.saturating_duration_since(*at) < CLOCK_MAX_AGE)
            .map(|(estimate, _)| estimate)
    }

    pub fn video_presentation(
        &self,
        captured: u64,
        captured_player_at: Option<Instant>,
        presented_at: Instant,
        bitrate: u64,
        estimated: bool,
    ) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.presented_bitrate = bitrate;
        let clock = state
            .clock
            .filter(|(_, at)| presented_at.saturating_duration_since(*at) < CLOCK_MAX_AGE);
        let delay = clock.and_then(|_| presentation_delay(captured_player_at, presented_at));
        let observation =
            state
                .video_presentation
                .observe(captured, delay, estimated, presented_at);
        if let (Some(delay), Some((clock, _))) = (observation, clock) {
            observe(&mut state.video, delay, clock.uncertainty_micros, estimated);
        }
    }

    pub fn audio_playback(
        &self,
        captured: u64,
        assembled_micros: u64,
        assembled_at: Instant,
        submitted_at: Instant,
        queued_micros: u64,
    ) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.audio_active = true;
        if let Some((clock, at)) = state.clock
            && submitted_at.saturating_duration_since(at) < CLOCK_MAX_AGE
            && let Some(capture) =
                capture_player_at(clock, captured, assembled_micros, assembled_at)
        {
            let delay = submitted_at
                .saturating_duration_since(capture)
                .saturating_add(Duration::from_micros(queued_micros));
            observe(&mut state.audio, delay, clock.uncertainty_micros, true);
        }
    }

    pub fn set_audio_active(&self, active: bool) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.audio_active = active;
        if !active {
            state.audio = None;
        }
    }

    pub fn input_queued(&self, sequence: u64, occurred_at: Instant) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.last_input = Some(occurred_at);
        state.pending_input.insert(sequence, occurred_at);
        while state.pending_input.len() > MAX_PENDING_INPUTS {
            state.pending_input.pop_first();
        }
    }

    pub fn input_acknowledged(&self, sequence: u64, now: Instant) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let mut oldest = None::<Instant>;
        while state
            .pending_input
            .first_key_value()
            .is_some_and(|(key, _)| *key <= sequence)
        {
            let (_, occurred_at) = state.pending_input.pop_first().unwrap();
            oldest = Some(oldest.map_or(occurred_at, |at| at.min(occurred_at)));
        }
        if let Some(at) = oldest {
            observe(
                &mut state.input,
                now.saturating_duration_since(at),
                0,
                false,
            );
        }
    }

    pub fn take_report(&self, now: Instant) -> LatencyReport {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(oldest) = state.pending_input.values().min().copied() {
            observe(
                &mut state.input,
                now.saturating_duration_since(oldest),
                0,
                true,
            );
        }
        let input_active = state.input.is_some()
            || !state.pending_input.is_empty()
            || state
                .last_input
                .is_some_and(|at| now.saturating_duration_since(at) < INPUT_ACTIVE_TIME);
        LatencyReport {
            video: state.video.take(),
            audio: state.audio.take(),
            input: state.input.take(),
            audio_active: state.audio_active,
            input_active,
            presented_bitrate_bits_per_second: state.presented_bitrate,
        }
    }
}
