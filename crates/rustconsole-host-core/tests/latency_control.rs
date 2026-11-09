use rustconsole_host_core::latency::{LatencyPolicy, delay_budget, preference_score};
use rustconsole_host_core::video_stream::{HostEncodedVideoFrame, HostVideoStreamPolicy};
use rustconsole_host_core::{VideoDeliveryReport, VideoPathReport};
use rustconsole_protocol::latency::{LatencyChannel, LatencyMeasurement, LatencyReport};
use rustconsole_session::video_datagram::VIDEO_DATAGRAM_VERSION;
use std::time::{Duration, Instant};

fn measurement(delay_micros: u64) -> LatencyMeasurement {
    LatencyMeasurement {
        delay_micros,
        uncertainty_micros: 1_000,
        estimated: true,
        samples: 10,
        typical_delay_micros: 0,
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
fn a_session_starts_from_the_measured_startup_bitrate() {
    let start = Instant::now();
    let mut policy = HostVideoStreamPolicy::from_startup_probe(20_000_000, 20_000_000);
    let measured_start = policy.target_bits_per_second();
    policy.enable_latency_control(100_000, 10_000, start);
    assert!(measured_start > 1_000_000);
    assert_eq!(policy.target_bits_per_second(), measured_start);
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
    assert_eq!(policy.ceiling(), Some(4_000_000));
}

#[test]
fn repeated_cuts_in_one_incident_keep_the_first_limit() {
    let start = Instant::now();
    let mut policy = LatencyPolicy::new(100_000, 10_000, start);
    calibrate(&mut policy, start);
    let mut bitrate = 40_000_000;
    let mut report_at = Duration::from_secs(4);
    for delay in [85_000, 100_000, 120_000] {
        policy.bitrate_changed(start + report_at - Duration::from_secs(2));
        for _ in 0..2 {
            let result = policy.observe(
                Some(report(delay, bitrate)),
                10_000,
                bitrate,
                100_000_000,
                start + report_at,
            );
            bitrate = result.reduce_to.unwrap_or(bitrate);
            report_at += Duration::from_millis(500);
        }
    }
    assert_eq!(bitrate, 16_875_000);
    assert_eq!(policy.ceiling(), Some(40_000_000));
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
        feedback.input = Some(measurement(110_000));
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
    policy.bitrate_changed(start + Duration::from_secs(4));
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
fn late_active_audio_holds_bitrate_until_it_has_a_baseline() {
    let start = Instant::now();
    let mut policy = LatencyPolicy::new(100_000, 10_000, start);
    calibrate(&mut policy, start);
    let mut allowed = Vec::new();
    for index in 8..=13 {
        let mut feedback = report(40_000, 1_000_000);
        feedback.audio_active = true;
        feedback.audio = Some(measurement(60_000));
        let result = policy.observe(
            Some(feedback),
            10_000,
            1_000_000,
            20_000_000,
            start + Duration::from_millis(index * 500),
        );
        assert_eq!(result.reduce_to, None);
        allowed.push(result.allow_increase);
    }
    assert!(!allowed[0]);
    assert!(*allowed.last().unwrap());
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

/// Sends one frame with the given capture-to-encoded time, then one player report.
fn report_interval(
    policy: &mut HostVideoStreamPolicy,
    half_second: u64,
    source_delay: u64,
    player_delay: u64,
    start: Instant,
) {
    report_interval_with_loss(policy, half_second, source_delay, player_delay, 0, start);
}

/// Like `report_interval`, with `lost_chunks` counted since the start of the stream.
fn report_interval_with_loss(
    policy: &mut HostVideoStreamPolicy,
    half_second: u64,
    source_delay: u64,
    player_delay: u64,
    lost_chunks: u64,
    start: Instant,
) {
    let captured_at_micros = half_second * 500_000;
    policy
        .packetize_encoded_frame(
            HostEncodedVideoFrame {
                sequence: half_second,
                captured_at_micros,
                encoded_at_micros: captured_at_micros + source_delay,
                packetized_at_micros: captured_at_micros + source_delay,
                input_sequence: 0,
                keyframe: true,
                payload: vec![0; 64],
            },
            1_200,
            VIDEO_DATAGRAM_VERSION,
        )
        .unwrap();
    let target = policy.target_bits_per_second();
    let _ = policy.observe_receiver_with_latency(
        VideoPathReport {
            round_trip_time: Duration::from_millis(3),
            congestion_window_bytes: 250_000,
            lost_packets: 0,
        },
        VideoDeliveryReport {
            completed_payload_bytes: target / 16,
            measurement_interval_micros: 500_000,
            lost_chunks,
            ..VideoDeliveryReport::default()
        },
        Some(report(player_delay, target)),
        start + Duration::from_millis(half_second * 500),
    );
}

fn calibrated_stream(start: Instant) -> HostVideoStreamPolicy {
    let mut policy = HostVideoStreamPolicy::from_startup_probe(100_000_000, 50_000_000);
    policy.enable_latency_control(100_000, 3_000, start);
    for half_second in 1..=12 {
        report_interval(&mut policy, half_second, 13_000, 20_000, start);
    }
    policy
}

#[test]
fn a_host_stall_does_not_cut_bitrate_but_the_same_delay_from_the_network_does() {
    let start = Instant::now();
    let mut stalled = calibrated_stream(start);
    let before_stall = stalled.target_bits_per_second();
    for half_second in 13..=18 {
        report_interval(&mut stalled, half_second, 73_000, 80_000, start);
    }
    assert!(stalled.target_bits_per_second() >= before_stall);

    let mut congested = calibrated_stream(start);
    let before_congestion = congested.target_bits_per_second();
    for half_second in 13..=18 {
        report_interval(&mut congested, half_second, 13_000, 80_000, start);
    }
    assert!(congested.target_bits_per_second() < before_congestion);
}

#[test]
fn delay_while_the_link_loses_data_neither_cuts_nor_sets_a_limit() {
    let start = Instant::now();
    let run = |player_delay: u64| {
        let mut policy = calibrated_stream(start);
        for half_second in 13..=18 {
            report_interval_with_loss(
                &mut policy,
                half_second,
                13_000,
                player_delay,
                half_second * 10,
                start,
            );
        }
        policy
    };
    let delayed = run(80_000);
    let undelayed = run(20_000);
    assert_eq!(
        delayed.target_bits_per_second(),
        undelayed.target_bits_per_second()
    );
    assert_eq!(
        delayed.latency_status().unwrap().ceiling_bits_per_second,
        None
    );
}

#[test]
fn a_passing_incident_after_a_proven_hold_keeps_the_limit_but_a_recent_rate_is_blamed() {
    let start = Instant::now();
    let mut policy = LatencyPolicy::new(100_000, 10_000, start);
    calibrate(&mut policy, start);
    let bad = |policy: &mut LatencyPolicy, bitrate: u64, seconds: u64| {
        for half_second in 0..2_u64 {
            let evaluation = policy.observe(
                Some(report(90_000, bitrate)),
                10_000,
                bitrate,
                20_000_000,
                start + Duration::from_millis(seconds * 1_000 + half_second * 500),
            );
            if evaluation.reduce_to.is_some() {
                return evaluation.reduce_to;
            }
        }
        None
    };
    assert!(bad(&mut policy, 4_000_000, 4).is_some());
    assert_eq!(policy.ceiling(), Some(4_000_000));

    let cut_after_hold = start + Duration::from_secs(5);
    policy.bitrate_changed(cut_after_hold);
    assert_eq!(bad(&mut policy, 3_800_000, 70), Some(3_648_000));
    assert_eq!(policy.ceiling(), Some(4_000_000));

    policy.bitrate_changed(start + Duration::from_secs(75));
    assert!(bad(&mut policy, 3_800_000, 90).is_some());
    assert_eq!(policy.ceiling(), Some(3_800_000));
}

#[test]
fn brief_spikes_in_every_report_leave_the_bitrate_and_the_session_alone() {
    let start = Instant::now();
    let mut policy = LatencyPolicy::new(100_000, 10_000, start);
    calibrate(&mut policy, start);
    for half_second in 8..=80 {
        let mut spiky = report(95_000, 1_000_000);
        spiky.video.as_mut().unwrap().typical_delay_micros = 40_000;
        let evaluation = policy.observe(
            Some(spiky),
            10_000,
            1_000_000,
            20_000_000,
            start + Duration::from_millis(half_second * 500),
        );
        assert_eq!(evaluation.reduce_to, None);
        assert_eq!(evaluation.failure, None);
    }
}

#[test]
fn delay_on_most_frames_still_ends_a_session_stuck_at_the_minimum_bitrate() {
    let start = Instant::now();
    let mut policy = LatencyPolicy::new(100_000, 10_000, start);
    calibrate(&mut policy, start);
    let failed = (8..=24).any(|half_second| {
        let mut congested = report(125_000, 1_000_000);
        congested.video.as_mut().unwrap().typical_delay_micros = 120_000;
        policy
            .observe(
                Some(congested),
                10_000,
                1_000_000,
                20_000_000,
                start + Duration::from_millis(half_second * 500),
            )
            .failure
            .is_some()
    });
    assert!(failed);
}

#[test]
fn delay_over_the_budget_but_within_the_client_limit_never_ends_a_session() {
    let start = Instant::now();
    let mut policy = LatencyPolicy::new(100_000, 10_000, start);
    calibrate(&mut policy, start);
    for half_second in 8..=60 {
        let evaluation = policy.observe(
            Some(report(60_000, 1_000_000)),
            10_000,
            1_000_000,
            20_000_000,
            start + Duration::from_millis(half_second * 500),
        );
        assert_eq!(evaluation.failure, None);
    }
}

/// Reports the same delay every half second and returns each bitrate cut with its time.
fn cuts_under_steady_delay(seconds: u64) -> Vec<(Duration, u64)> {
    let start = Instant::now();
    let mut policy = LatencyPolicy::new(100_000, 10_000, start);
    calibrate(&mut policy, start);
    let mut bitrate = 40_000_000;
    let mut cuts = Vec::new();
    for half_second in 8..8 + seconds * 2 {
        let at = Duration::from_millis(half_second * 500);
        policy.bitrate_changed(start + at - Duration::from_secs(2));
        let result = policy.observe(
            Some(report(90_000, bitrate)),
            10_000,
            bitrate,
            100_000_000,
            start + at,
        );
        if let Some(target) = result.reduce_to {
            bitrate = target;
            cuts.push((at, target));
        }
    }
    cuts
}

#[test]
fn delay_that_a_cut_does_not_change_is_cut_only_every_eight_seconds() {
    let cuts = cuts_under_steady_delay(18);
    assert_eq!(cuts.len(), 3);
    assert!(
        cuts.windows(2)
            .all(|pair| pair[1].0 - pair[0].0 >= Duration::from_secs(8))
    );
}
