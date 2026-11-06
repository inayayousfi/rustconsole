//! Host-owned delay budgets, calibration, and latency/bitrate preferences.

use rustconsole_protocol::latency::{LatencyBudgetFailure, LatencyControlStatus, LatencyReport};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const BASELINE_SAMPLES: usize = 5;
const SETTLE: Duration = Duration::from_secs(1);
const FAILURE_TIME: Duration = Duration::from_secs(5);
const PROBE_TIME: Duration = Duration::from_secs(5);
/// Above this share of the delay allowance the bitrate holds instead of rising.
const INCREASE_PRESSURE_LIMIT: f64 = 0.5;
/// Cuts this close together answer one incident, so only the first describes the link.
const INCIDENT_TIME: Duration = Duration::from_secs(10);
/// A first incident this close to the limit steps down gently instead of cutting hard.
const NEAR_LIMIT_PERCENT: u64 = 90;
const EDGE_CUT_PERCENT: u64 = 96;

#[derive(Debug, Default)]
struct Channel {
    calibration: VecDeque<u64>,
    baseline: Option<u64>,
    excessive_since: Option<Instant>,
}

#[derive(Debug)]
pub struct LatencyPolicy {
    absolute: u64,
    channels: [Channel; 3],
    changed_at: Instant,
    pressure_streak: u8,
    ceiling: Option<u64>,
    retry_seconds: u64,
    last_cut_at: Option<Instant>,
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
            ceiling: None,
            retry_seconds: 5,
            last_cut_at: None,
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
        let mut estimates = false;
        for index in 0..3 {
            let channel = &mut self.channels[index];
            if !active[index] {
                channel.excessive_since = None;
                continue;
            }
            let Some(value) = values[index].filter(|value| value.valid()) else {
                ready = false;
                channel.excessive_since = None;
                continue;
            };
            estimates |= value.estimated || value.uncertainty_micros != 0;
            if settled {
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
                let new_incident = self
                    .last_cut_at
                    .is_none_or(|at| now.saturating_duration_since(at) >= INCIDENT_TIME);
                let near_limit = self
                    .ceiling
                    .is_some_and(|ceiling| bitrate >= ceiling / 100 * NEAR_LIMIT_PERCENT);
                self.last_cut_at = Some(now);
                result.reduce_to = Some(if new_incident && near_limit {
                    bitrate / 100 * EDGE_CUT_PERCENT
                } else {
                    bitrate * 3 / 4
                });
                let hold_proven =
                    now.saturating_duration_since(self.changed_at) >= crate::PROVEN_HOLD;
                if new_incident && !hold_proven {
                    self.learn_ceiling(bitrate);
                }
            }
            return result;
        }
        self.pressure_streak = 0;
        if !ready {
            return result;
        }
        if self.ceiling.is_some_and(|ceiling| bitrate > ceiling)
            && now.saturating_duration_since(self.changed_at) >= PROBE_TIME
        {
            self.ceiling = Some(bitrate);
            self.status.ceiling_bits_per_second = self.ceiling;
            self.retry_seconds = 5;
        }
        result.allow_increase = pressure < INCREASE_PRESSURE_LIMIT;
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
    }

    pub fn bitrate_changed(&mut self, now: Instant) {
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
