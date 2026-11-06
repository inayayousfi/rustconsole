//! Runs the host bitrate policy against a simulated link with noisy delay. The simulation
//! stands in for the encoder and network, so these tests establish control behavior only.

use rustconsole_host_core::video_stream::HostVideoStreamPolicy;
use rustconsole_host_core::{VideoDeliveryReport, VideoPathReport};
use rustconsole_protocol::latency::{LatencyMeasurement, LatencyReport};
use std::time::{Duration, Instant};

const STEP_SECONDS: f64 = 0.5;
const CHUNK_BYTES: f64 = 1_200.0;
const LINK_BUFFER_SECONDS: f64 = 0.05;
const MAXIMUM_BITRATE: u64 = 100_000_000;

struct Link {
    random: u64,
    capacity: f64,
    queued_bits: f64,
    now: Instant,
    policy: HostVideoStreamPolicy,
    delivery: VideoDeliveryReport,
    delay_micros: u64,
    source_stall_steps: u32,
}

impl Link {
    fn new(capacity_megabits: f64) -> Self {
        let now = Instant::now();
        let capacity = capacity_megabits * 1e6;
        let mut policy =
            HostVideoStreamPolicy::from_startup_probe(MAXIMUM_BITRATE, capacity as u64);
        policy.enable_latency_control(100_000, 2_000, now);
        Self {
            random: 0x9E37_79B9_7F4A_7C15,
            capacity,
            queued_bits: 0.0,
            now,
            policy,
            delivery: VideoDeliveryReport::default(),
            delay_micros: 0,
            source_stall_steps: 0,
        }
    }

    fn noise(&mut self) -> f64 {
        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        (self.random >> 11) as f64 / (1_u64 << 53) as f64
    }

    /// Advances half a second and returns the bitrate the host then requests.
    fn step(&mut self) -> u64 {
        let target = self.policy.target_bits_per_second();
        let encoded = target as f64 * STEP_SECONDS * (0.97 + 0.06 * self.noise());
        let capacity_bits = self.capacity * STEP_SECONDS;
        let offered = self.queued_bits + encoded;
        let delivered = offered.min(capacity_bits);
        let waiting = offered - delivered;
        let buffer = self.capacity * LINK_BUFFER_SECONDS;
        let dropped = (waiting - buffer).max(0.0);
        self.queued_bits = waiting.min(buffer);
        let queue_seconds = self.queued_bits / self.capacity;
        let jitter = 8_000.0 * self.noise();
        self.delay_micros = (25_000.0 + jitter + queue_seconds * 1e6) as u64;
        if self.source_stall_steps != 0 {
            self.source_stall_steps -= 1;
            self.delay_micros += 400_000;
        }

        self.delivery.completed_payload_bytes += (delivered / 8.0) as u64;
        self.delivery.received_chunks += (delivered / 8.0 / CHUNK_BYTES) as u64;
        self.delivery.lost_chunks += (dropped / 8.0 / CHUNK_BYTES) as u64;
        self.delivery.measurement_interval_micros = 500_000;
        self.now += Duration::from_millis(500);
        let report = LatencyReport {
            video: Some(LatencyMeasurement {
                delay_micros: self.delay_micros,
                uncertainty_micros: 700,
                estimated: true,
                samples: 60,
                typical_delay_micros: 0,
            }),
            presented_bitrate_bits_per_second: target,
            ..LatencyReport::default()
        };
        let path = VideoPathReport {
            round_trip_time: Duration::from_secs_f64(0.002 + queue_seconds),
            congestion_window_bytes: 100_000_000,
            lost_packets: 0,
        };
        self.policy
            .observe_receiver_with_latency(path, self.delivery, Some(report), self.now)
            .expect("the simulated link stays above the minimum bitrate");
        self.policy.target_bits_per_second()
    }

    fn run(&mut self, seconds: u64) -> Vec<u64> {
        (0..seconds * 2).map(|_| self.step()).collect()
    }
}

#[test]
fn a_steady_link_settles_near_its_limit_without_a_repeating_cycle() {
    let mut link = Link::new(80.0);
    link.run(300);
    let targets = link.run(1_800);
    let drops = targets.windows(2).filter(|pair| pair[1] < pair[0]).count();
    let lowest = *targets.iter().min().unwrap() as f64 / 1e6;
    let highest = *targets.iter().max().unwrap() as f64 / 1e6;
    assert!(drops <= 3, "{drops} drops in 30 minutes");
    assert!(lowest >= 74.0, "bitrate fell to {lowest}");
    assert!(highest <= 82.0, "bitrate reached {highest}");
}

#[test]
fn a_capacity_collapse_is_answered_within_about_a_second() {
    let mut link = Link::new(80.0);
    link.run(120);
    link.capacity = 30e6;
    let targets = link.run(30);
    let first_cut = targets.iter().position(|target| *target < 60_000_000);
    assert!(first_cut.is_some_and(|step| step <= 2), "{first_cut:?}");
    assert!(*targets.last().unwrap() <= 30_000_000);
    assert!(link.delay_micros < 40_000, "{}", link.delay_micros);
}

#[test]
fn a_brief_stall_at_the_source_does_not_lose_the_learned_bitrate() {
    let mut link = Link::new(80.0);
    link.run(120);
    let before = link.policy.target_bits_per_second();
    link.source_stall_steps = 6;
    let targets = link.run(60);
    assert!(*targets.iter().min().unwrap() < before);
    assert!(*targets.last().unwrap() as f64 >= before as f64 * 0.95);
}

#[test]
fn a_better_connection_is_found_without_a_manual_setting() {
    let mut link = Link::new(30.0);
    link.run(300);
    link.capacity = 80e6;
    let targets = link.run(1_200);
    assert!(
        *targets.last().unwrap() >= 70_000_000,
        "{}",
        targets.last().unwrap()
    );
}
