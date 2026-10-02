//! Host-owned delay budgets, calibration, and latency/bitrate preferences.

use rustconsole_protocol::latency::{LatencyBudgetFailure, LatencyControlStatus, LatencyReport};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const BASELINE_SAMPLES: usize = 5;
const SETTLE: Duration = Duration::from_secs(1);
const FAILURE_TIME: Duration = Duration::from_secs(5);
const PROBE_TIME: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
struct Channel {
    calibration: VecDeque<u64>,
    baseline: Option<u64>,
    excessive_since: Option<Instant>,
}

#[derive(Debug)]
struct Trial {
    previous_bitrate: u64,
    reference: u64,
    previous_score: f64,
    mask: u8,
    started: Instant,
    scores: f64,
    reports: u32,
}

#[derive(Debug)]
pub struct LatencyPolicy {
    absolute: u64,
    channels: [Channel; 3],
    changed_at: Instant,
    pressure_streak: u8,
    trial: Option<Trial>,
    ceiling: Option<u64>,
    retry_seconds: u64,
    last_pressure: f64,
    last_mask: u8,
    last_report_at: Option<Instant>,
    status: LatencyControlStatus,
}

pub struct Evaluation {
    pub allow_increase: bool,
    pub reduce_to: Option<u64>,
    pub failure: Option<LatencyBudgetFailure>,
}

pub fn delay_budget(baseline: u64, absolute: u64) -> u64 {
    baseline.saturating_add(baseline.max(20_000)).min(absolute)
}

pub fn preference_score(pressure: f64, bitrate: u64, reference: u64) -> f64 {
    let latency = 1.0 - pressure.clamp(0.0, 1.0);
    let quality = (bitrate as f64 / reference.max(1) as f64)
        .clamp(0.0, 1.0)
        .sqrt();
    0.6 * latency + 0.4 * quality
}

impl LatencyPolicy {
    pub fn new(absolute: u64, _network_baseline: u64, now: Instant) -> Self {
        let channels = std::array::from_fn(|_| Channel::default());
        Self {
            absolute,
            channels,
            changed_at: now,
            pressure_streak: 0,
            trial: None,
            ceiling: None,
            retry_seconds: 5,
            last_pressure: 0.0,
            last_mask: 0,
            last_report_at: None,
            status: LatencyControlStatus::default(),
        }
    }

    pub fn observe(
        &mut self,
        report: Option<LatencyReport>,
        _network_micros: u64,
        bitrate: u64,
        reference: u64,
        now: Instant,
    ) -> Evaluation {
        let mut result = Evaluation {
            allow_increase: false,
            reduce_to: None,
            failure: None,
        };
        if self
            .last_report_at
            .is_some_and(|at| now.saturating_duration_since(at) > Duration::from_secs(2))
        {
            self.reset_observation();
        }
        self.last_report_at = Some(now);
        let Some(report) = report.filter(|report| report.valid()) else {
            self.reset_observation();
            return result;
        };
        let settled = now.saturating_duration_since(self.changed_at) >= SETTLE
            && report.presented_bitrate_bits_per_second == bitrate;
        let values = [report.video, report.audio, report.input];
        let active = [true, report.audio_active, report.input_active];
        let mut ready = settled;
        let mut pressure = 0.0_f64;
        let mut excessive = false;
        let mut mask = 0;
        let mut estimates = false;
        for index in 0..3 {
            let channel = &mut self.channels[index];
            if !active[index] {
                channel.excessive_since = None;
                continue;
            }
            mask |= 1 << index;
            let Some(value) = values[index].filter(|value| value.valid()) else {
                ready = false;
                channel.excessive_since = None;
                continue;
            };
            estimates |= value.estimated || value.uncertainty_micros != 0;
            if settled && bitrate == crate::VIDEO_BITRATE_BOOTSTRAP {
                if channel.calibration.len() == BASELINE_SAMPLES {
                    channel.calibration.pop_front();
                }
                channel.calibration.push_back(value.delay_micros);
                if channel.calibration.len() == BASELINE_SAMPLES {
                    let mut samples: Vec<_> = channel.calibration.iter().copied().collect();
                    samples.sort_unstable();
                    let baseline = samples[1];
                    channel.baseline =
                        Some(channel.baseline.map_or(baseline, |old| old.min(baseline)));
                }
            }
            let Some(baseline) = channel.baseline else {
                ready = false;
                if settled && bitrate > crate::VIDEO_BITRATE_BOOTSTRAP {
                    result.reduce_to = Some(crate::VIDEO_BITRATE_BOOTSTRAP);
                }
                continue;
            };
            let budget = delay_budget(baseline, self.absolute);
            // Clock uncertainty is retained in the report; compare the estimate,
            // rather than turn uncertainty itself into an observed delay failure.
            let current_pressure = if budget > baseline {
                value.delay_micros.saturating_sub(baseline) as f64 / (budget - baseline) as f64
            } else {
                value.delay_micros as f64 / budget.max(1) as f64
            };
            pressure = pressure.max(current_pressure);
            if value.delay_micros > budget {
                excessive = true;
                if settled && bitrate == crate::VIDEO_BITRATE_BOOTSTRAP {
                    let since = *channel.excessive_since.get_or_insert(now);
                    if now.saturating_duration_since(since) >= FAILURE_TIME
                        && result.failure.is_none()
                    {
                        result.failure = Some(LatencyBudgetFailure {
                            channel: index as i32 + 1,
                            observed_delay_micros: value.delay_micros,
                            budget_micros: budget,
                            absolute_limit_micros: self.absolute,
                            baseline_micros: baseline,
                            estimated: value.estimated || value.uncertainty_micros != 0,
                        });
                    }
                } else {
                    channel.excessive_since = None;
                }
            } else {
                channel.excessive_since = None;
            }
        }
        self.last_pressure = pressure;
        self.last_mask = mask;
        self.status = LatencyControlStatus {
            ceiling_bits_per_second: self.ceiling,
            highest_pressure_ppm: (pressure * 1_000_000.0).min(u32::MAX as f64) as u32,
            score_ppm: (preference_score(pressure, bitrate, reference) * 1_000_000.0) as u32,
            measurements_ready: ready,
            uses_estimates: estimates,
        };
        if !settled {
            self.pressure_streak = 0;
            return result;
        }
        if excessive {
            self.pressure_streak = self.pressure_streak.saturating_add(1);
            if self.pressure_streak >= 2 && bitrate > crate::VIDEO_BITRATE_BOOTSTRAP {
                self.pressure_streak = 0;
                let target = self
                    .trial
                    .take()
                    .map_or(bitrate * 3 / 4, |trial| trial.previous_bitrate);
                self.learn_ceiling(target.max(crate::VIDEO_BITRATE_BOOTSTRAP));
                result.reduce_to = Some(target);
            }
            return result;
        }
        self.pressure_streak = 0;
        if !ready {
            // Missing measurements cannot validate an intentional increase.
            if let Some(trial) = self.trial.as_mut() {
                trial.started = now;
                trial.scores = 0.0;
                trial.reports = 0;
            }
            return result;
        }
        if self.trial.as_ref().is_some_and(|trial| trial.mask != mask) {
            // Changed active channels make the before/after scores incomparable.
            self.trial = None;
        }
        if let Some(trial) = self.trial.as_mut() {
            trial.scores += preference_score(pressure, bitrate, trial.reference);
            trial.reports += 1;
            if now.saturating_duration_since(trial.started) >= PROBE_TIME && trial.reports >= 5 {
                let trial = self.trial.take().unwrap();
                if trial.scores / f64::from(trial.reports) < trial.previous_score {
                    self.learn_ceiling(trial.previous_bitrate);
                    result.reduce_to = Some(trial.previous_bitrate);
                    return result;
                }
                if self.ceiling.is_some_and(|ceiling| bitrate > ceiling) {
                    self.ceiling = Some(bitrate);
                    self.status.ceiling_bits_per_second = self.ceiling;
                    self.retry_seconds = 5;
                }
            }
        }
        result.allow_increase = self.trial.is_none();
        result
    }

    fn learn_ceiling(&mut self, bitrate: u64) {
        self.ceiling = Some(bitrate);
        self.status.ceiling_bits_per_second = self.ceiling;
        self.retry_seconds = (self.retry_seconds * 2).min(40);
    }

    fn reset_observation(&mut self) {
        self.status.measurements_ready = false;
        self.pressure_streak = 0;
        for channel in &mut self.channels {
            channel.excessive_since = None;
        }
        self.trial = None;
    }

    pub fn bitrate_changed(&mut self, previous: u64, target: u64, reference: u64, now: Instant) {
        if target > previous {
            self.trial = Some(Trial {
                previous_bitrate: previous,
                reference,
                previous_score: preference_score(self.last_pressure, previous, reference),
                mask: self.last_mask,
                started: now + SETTLE,
                scores: 0.0,
                reports: 0,
            });
        } else {
            self.trial = None;
        }
        self.changed_at = now;
        for channel in &mut self.channels {
            channel.excessive_since = None;
        }
    }

    pub fn status(&self) -> LatencyControlStatus {
        self.status
    }
    pub fn ceiling(&self) -> Option<u64> {
        self.ceiling
    }
    pub fn retry_delay_micros(&self) -> u64 {
        self.retry_seconds * 1_000_000
    }
}
