use rustconsole_player_core::ClockOffsetEstimate;
use rustconsole_player_core::latency::{LatencyMeasurements, capture_player_at};
use std::time::{Duration, Instant};

fn aligned(now: Instant) -> LatencyMeasurements {
    let measurements = LatencyMeasurements::default();
    measurements.set_clock(
        ClockOffsetEstimate {
            offset_micros: 1_000_000,
            uncertainty_micros: 1_000,
        },
        now,
    );
    measurements
}

#[test]
fn normal_play_reports_video_and_audio_without_full_diagnostics() {
    let now = Instant::now();
    let measurements = aligned(now);
    let captured =
        capture_player_at(measurements.clock(now).unwrap(), 1_050_000, 100_000, now).unwrap();
    measurements.video_presentation(1_050_000, Some(captured), now, 5_000_000, false);
    measurements.audio_playback(1_050_000, 100_000, now, now, 20_000);
    let report = measurements.take_report(now);
    assert!(report.valid());
    assert_eq!(report.video.unwrap().delay_micros, 50_000);
    assert_eq!(report.audio.unwrap().delay_micros, 70_000);
    assert!(report.audio.unwrap().estimated);
    assert_eq!(report.presented_bitrate_bits_per_second, 5_000_000);
}

#[test]
fn repeated_source_captures_do_not_turn_static_pictures_into_growing_delay() {
    let now = Instant::now();
    let measurements = aligned(now);
    measurements.video_presentation(
        10,
        Some(now - Duration::from_millis(40)),
        now,
        1_000_000,
        true,
    );
    assert_eq!(
        measurements.take_report(now).video.unwrap().delay_micros,
        40_000
    );
    measurements.video_presentation(
        10,
        Some(now - Duration::from_millis(40)),
        now + Duration::from_secs(1),
        2_000_000,
        true,
    );
    let repeated = measurements.take_report(now + Duration::from_secs(1));
    assert!(repeated.video.is_none());
    assert_eq!(repeated.presented_bitrate_bits_per_second, 2_000_000);
    measurements.video_presentation(
        11,
        Some(now + Duration::from_millis(960)),
        now + Duration::from_secs(1),
        2_000_000,
        true,
    );
    assert_eq!(
        measurements
            .take_report(now + Duration::from_secs(1))
            .video
            .unwrap()
            .delay_micros,
        40_000
    );
}

#[test]
fn input_delay_includes_local_queueing_and_unacknowledged_input_is_not_zero() {
    let now = Instant::now();
    let measurements = LatencyMeasurements::default();
    measurements.input_queued(1, now);
    let pending = measurements.take_report(now + Duration::from_millis(70));
    assert!(pending.input_active);
    assert_eq!(pending.input.unwrap().delay_micros, 70_000);
    assert!(pending.input.unwrap().estimated);
    measurements.input_acknowledged(1, now + Duration::from_millis(80));
    let ack = measurements.take_report(now + Duration::from_millis(90));
    assert_eq!(ack.input.unwrap().delay_micros, 80_000);
    assert!(!ack.input.unwrap().estimated);
    let idle = measurements.take_report(now + Duration::from_secs(2));
    assert!(!idle.input_active);
    assert!(idle.input.is_none());
}

#[test]
fn stale_clocks_and_inactive_audio_are_unavailable_not_healthy_zeroes() {
    let now = Instant::now();
    let measurements = aligned(now);
    let later = now + Duration::from_secs(6);
    assert!(measurements.clock(later).is_none());
    measurements.video_presentation(1, Some(now), later, 1_000_000, true);
    measurements.audio_playback(1_000_000, 0, now, later, 0);
    measurements.set_audio_active(false);
    let report = measurements.take_report(later);
    assert!(report.video.is_none());
    assert!(report.audio.is_none());
    assert!(!report.audio_active);
}

#[test]
fn estimated_presentation_label_survives_repeated_captures() {
    let now = Instant::now();
    let mut presentation = rustconsole_player_core::latency::VideoPresentationLatency::default();
    presentation.observe(1, Some(Duration::from_millis(40)), true, now);
    presentation.observe(
        1,
        Some(Duration::from_secs(1)),
        false,
        now + Duration::from_secs(1),
    );
    assert!(presentation.estimated());
    assert_eq!(presentation.latest(), Some(Duration::from_millis(40)));
    assert!(!presentation.stalled(now + Duration::from_secs(1), Duration::from_millis(500)));
}
