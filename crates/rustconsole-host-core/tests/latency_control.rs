use rustconsole_host_core::latency::{LatencyPolicy, delay_budget, preference_score};
use rustconsole_host_core::video_stream::HostVideoStreamPolicy;
use rustconsole_host_core::{VideoDeliveryReport, VideoPathReport};
use rustconsole_protocol::latency::{LatencyChannel, LatencyMeasurement, LatencyReport};
use std::time::{Duration, Instant};

fn measurement(delay_micros: u64) -> LatencyMeasurement {
    LatencyMeasurement {
        delay_micros,
        uncertainty_micros: 1_000,
        estimated: true,
        samples: 10,
    }
}

fn report(video: u64, bitrate: u64) -> LatencyReport {
    LatencyReport {
        video: Some(measurement(video)),
        presented_bitrate_bits_per_second: bitrate,
        ..Default::default()
    }
}

fn calibrate(policy: &mut LatencyPolicy, start: Instant) {
    for half_second in 2..=6 {
        policy.observe(
            Some(report(40_000, 1_000_000)),
            10_000,
            1_000_000,
            20_000_000,
            start + Duration::from_millis(half_second * 500),
        );
    }
}

#[test]
fn combined_budget_allows_twice_baseline_with_a_variation_floor_and_absolute_cap() {
    assert_eq!(delay_budget(5_000, 100_000), 25_000);
    assert_eq!(delay_budget(40_000, 100_000), 80_000);
    assert_eq!(delay_budget(70_000, 100_000), 100_000);
}

#[test]
fn quality_has_diminishing_rewards_and_delay_has_more_weight() {
    let quarter = preference_score(0.0, 25, 100);
    let half = preference_score(0.0, 50, 100);
    let three_quarters = preference_score(0.0, 75, 100);
    assert!(half - quarter > three_quarters - half);
    assert!(preference_score(0.0, 40, 100) > preference_score(0.4, 80, 100));
}

#[test]
fn ordinary_feedback_calibrates_at_minimum_then_increases_quality() {
    let start = Instant::now();
    let mut policy = HostVideoStreamPolicy::from_startup_probe(20_000_000, 20_000_000);
    policy.enable_latency_control(100_000, 10_000, start);
    assert_eq!(policy.target_bits_per_second(), 1_000_000);
    for index in 1..=12 {
        let target = policy.target_bits_per_second();
        policy
            .observe_receiver_with_latency(
                VideoPathReport {
                    round_trip_time: Duration::from_millis(10),
                    congestion_window_bytes: 1_000_000,
                    lost_packets: 0,
                },
                VideoDeliveryReport {
                    received_chunks: index * 100,
                    completed_payload_bytes: index * 62_500,
                    measurement_interval_micros: 500_000,
                    ..Default::default()
                },
                Some(report(40_000, target)),
                start + Duration::from_millis(index * 500),
            )
            .unwrap();
        if index <= 4 {
            assert_eq!(policy.target_bits_per_second(), 1_000_000);
        }
    }
    assert!(policy.target_bits_per_second() > 1_000_000);
}

#[test]
fn sustained_delay_cuts_twenty_five_percent_and_learns_a_separate_ceiling() {
    let start = Instant::now();
    let mut policy = LatencyPolicy::new(100_000, 10_000, start);
    calibrate(&mut policy, start);
    let first = policy.observe(
        Some(report(90_000, 4_000_000)),
        10_000,
        4_000_000,
        20_000_000,
        start + Duration::from_secs(4),
    );
    assert!(!first.allow_increase);
    assert_eq!(first.reduce_to, None);
    let second = policy.observe(
        Some(report(90_000, 4_000_000)),
        10_000,
        4_000_000,
        20_000_000,
        start + Duration::from_millis(4_500),
    );
    assert_eq!(second.reduce_to, Some(3_000_000));
    assert_eq!(policy.ceiling(), Some(3_000_000));
}

#[test]
fn delay_after_an_increase_restores_the_previous_bitrate() {
    let start = Instant::now();
    let mut policy = LatencyPolicy::new(100_000, 10_000, start);
    calibrate(&mut policy, start);
    policy.bitrate_changed(
        1_000_000,
        1_250_000,
        20_000_000,
        start + Duration::from_secs(3),
    );
    for offset in [4_000, 4_500] {
        let result = policy.observe(
            Some(report(90_000, 1_250_000)),
            10_000,
            1_250_000,
            20_000_000,
            start + Duration::from_millis(offset),
        );
        if offset == 4_500 {
            assert_eq!(result.reduce_to, Some(1_000_000));
        }
    }
}

#[test]
fn a_worse_sixty_forty_score_rolls_back_even_inside_delay_limits() {
    let start = Instant::now();
    let mut policy = LatencyPolicy::new(100_000, 10_000, start);
    calibrate(&mut policy, start);
    policy.bitrate_changed(
        1_000_000,
        1_250_000,
        20_000_000,
        start + Duration::from_secs(3),
    );
    let mut reduced = None;
    for offset in 8..=18 {
        let result = policy.observe(
            Some(report(70_000, 1_250_000)),
            10_000,
            1_250_000,
            20_000_000,
            start + Duration::from_millis(offset * 500),
        );
        reduced = reduced.or(result.reduce_to);
    }
    assert_eq!(reduced, Some(1_000_000));
}

#[test]
fn one_bad_channel_cannot_be_hidden_by_healthy_video_and_network() {
    let start = Instant::now();
    let mut policy = LatencyPolicy::new(100_000, 10_000, start);
    for index in 2..=6 {
        let mut feedback = report(40_000, 1_000_000);
        feedback.audio_active = true;
        feedback.audio = Some(measurement(60_000));
        feedback.input_active = true;
        feedback.input = Some(measurement(10_000));
        policy.observe(
            Some(feedback),
            10_000,
            1_000_000,
            20_000_000,
            start + Duration::from_millis(index * 500),
        );
    }
    let mut failure = None;
    for index in 7..=17 {
        let mut feedback = report(40_000, 1_000_000);
        feedback.audio_active = true;
        feedback.audio = Some(measurement(60_000));
        feedback.input_active = true;
        feedback.input = Some(measurement(50_000));
        let result = policy.observe(
            Some(feedback),
            10_000,
            1_000_000,
            20_000_000,
            start + Duration::from_millis(index * 500),
        );
        if index < 17 {
            assert!(result.failure.is_none());
        }
        failure = result.failure;
    }
    let failure = failure.unwrap();
    assert_eq!(failure.channel, LatencyChannel::Input as i32);
    assert_eq!(failure.budget_micros, 30_000);
    assert!(failure.valid());
}

#[test]
fn missing_feedback_and_report_gaps_do_not_count_as_continuous_failure() {
    let start = Instant::now();
    let mut policy = LatencyPolicy::new(100_000, 10_000, start);
    calibrate(&mut policy, start);
    policy.observe(
        Some(report(90_000, 1_000_000)),
        10_000,
        1_000_000,
        20_000_000,
        start + Duration::from_secs(4),
    );
    let missing = policy.observe(
        None,
        10_000,
        1_000_000,
        20_000_000,
        start + Duration::from_secs(5),
    );
    assert!(!missing.allow_increase);
    let resumed = policy.observe(
        Some(report(90_000, 1_000_000)),
        10_000,
        1_000_000,
        20_000_000,
        start + Duration::from_secs(10),
    );
    assert!(resumed.failure.is_none());
}

#[test]
fn old_bitrate_feedback_cannot_start_the_minimum_bitrate_failure_timer() {
    let start = Instant::now();
    let mut policy = LatencyPolicy::new(100_000, 10_000, start);
    calibrate(&mut policy, start);
    policy.bitrate_changed(
        4_000_000,
        1_000_000,
        20_000_000,
        start + Duration::from_secs(4),
    );
    for index in 9..=25 {
        let result = policy.observe(
            Some(report(90_000, 4_000_000)),
            10_000,
            1_000_000,
            20_000_000,
            start + Duration::from_millis(index * 500),
        );
        assert!(result.failure.is_none());
        assert!(!result.allow_increase);
    }
}

#[test]
fn late_active_audio_requires_a_minimum_bitrate_baseline() {
    let start = Instant::now();
    let mut policy = LatencyPolicy::new(100_000, 10_000, start);
    calibrate(&mut policy, start);
    let mut feedback = report(40_000, 4_000_000);
    feedback.audio_active = true;
    feedback.audio = Some(measurement(60_000));
    let result = policy.observe(
        Some(feedback),
        10_000,
        4_000_000,
        20_000_000,
        start + Duration::from_secs(4),
    );
    assert_eq!(result.reduce_to, Some(1_000_000));
    assert!(!result.allow_increase);
}

#[test]
fn latency_ceiling_does_not_replace_the_delivery_quality_reference() {
    let mut controller = rustconsole_host_core::AdaptiveBitrateController::from_startup_probe(
        20_000_000, 12_000_000,
    );
    let delivery_ceiling = controller.delivery_ceiling_bits_per_second();
    assert!(delivery_ceiling.is_some());
    controller.configure_latency_ceiling(Some(2_000_000));
    controller.set_latency_target(1_000_000, 5_000_000);
    assert_eq!(
        controller.delivery_ceiling_bits_per_second(),
        delivery_ceiling
    );
    assert_eq!(controller.soft_ceiling_bits_per_second(), None);
}

#[test]
fn network_delay_alone_cannot_end_the_session_under_application_budgets() {
    let start = Instant::now();
    let mut policy = LatencyPolicy::new(100_000, 10_000, start);
    calibrate(&mut policy, start);
    for index in 7..=25 {
        let result = policy.observe(
            Some(report(40_000, 1_000_000)),
            500_000,
            1_000_000,
            20_000_000,
            start + Duration::from_millis(index * 500),
        );
        assert!(result.failure.is_none());
        assert!(result.reduce_to.is_none());
    }
}
