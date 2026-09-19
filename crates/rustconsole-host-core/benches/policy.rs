use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rustconsole_host_core::{
    AdaptiveBitrateController, LatestQueue, VideoDeliveryReport, VideoPathReport,
    video_pacing::VideoFramePacer,
    video_stream::{HostEncodedVideoFrame, HostVideoStreamPolicy},
};
use rustconsole_session::video_datagram::{VIDEO_DATAGRAM_VERSION, VideoFrameAssembler};
use std::hint::black_box;
use std::time::{Duration, Instant};

fn benchmark_adaptive_bitrate(criterion: &mut Criterion) {
    criterion.bench_function("adaptive_bitrate/100_healthy_reports", |bench| {
        bench.iter_batched(
            || AdaptiveBitrateController::new(100_000_000),
            |mut controller| {
                for report in 1..=100_u64 {
                    black_box(controller.observe(
                        VideoPathReport {
                            round_trip_time: Duration::from_millis(5),
                            congestion_window_bytes: 1_000_000,
                            lost_packets: 0,
                        },
                        VideoDeliveryReport {
                            completed_payload_bytes: report * 500_000,
                            measurement_interval_micros: 500_000,
                            ..VideoDeliveryReport::default()
                        },
                    ));
                }
            },
            BatchSize::SmallInput,
        );
    });
}

fn benchmark_latest_queue(criterion: &mut Criterion) {
    criterion.bench_function("latest_queue/push_pop_64", |bench| {
        bench.iter_batched(
            || LatestQueue::new(64),
            |queue| {
                for value in 0..64 {
                    black_box(queue.push(value));
                }
                for _ in 0..64 {
                    black_box(queue.pop_timeout(Duration::ZERO));
                }
            },
            BatchSize::SmallInput,
        );
    });
}

fn benchmark_frame_pacing(criterion: &mut Criterion) {
    criterion.bench_function("video_pacing/120_ticks", |bench| {
        bench.iter_batched(
            || {
                let now = Instant::now();
                (VideoFramePacer::new(120, now).unwrap(), now)
            },
            |(mut pacer, mut now)| {
                for _ in 0..120 {
                    black_box(pacer.wait_duration(now));
                    black_box(pacer.should_encode());
                    let completed = now + Duration::from_millis(2);
                    pacer.complete_tick(now, completed);
                    now += pacer.frame_period();
                }
            },
            BatchSize::SmallInput,
        );
    });
}

fn benchmark_fake_encoded_video_path(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("host_video/fake_encoder_to_receiver");
    for size in [64 * 1024_usize, 1024 * 1024] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |bench, size| {
            bench.iter_batched(
                || (HostVideoStreamPolicy::new(40_000_000), vec![42; *size]),
                |(policy, payload)| {
                    let datagrams = policy
                        .packetize_encoded_frame(
                            HostEncodedVideoFrame {
                                sequence: 7,
                                captured_at_micros: 10,
                                encoded_at_micros: 20,
                                packetized_at_micros: 30,
                                input_sequence: 4,
                                keyframe: true,
                                payload,
                            },
                            1_200,
                            VIDEO_DATAGRAM_VERSION,
                        )
                        .unwrap();
                    let mut assembler = VideoFrameAssembler::new(120);
                    let now = Instant::now();
                    for datagram in datagrams {
                        black_box(
                            assembler
                                .push(&datagram, now, Duration::from_millis(5))
                                .unwrap(),
                        );
                    }
                },
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

fn benchmark_video_recovery_policy(criterion: &mut Criterion) {
    criterion.bench_function("host_video/recovery_cycle", |bench| {
        bench.iter_batched(
            || HostVideoStreamPolicy::new(100_000_000),
            |mut policy| {
                black_box(policy.observe_send_deadline_expired());
                black_box(policy.keyframe_request_due(Instant::now()));
                black_box(policy.accept_encoded_frame(1, false));
                black_box(policy.accept_encoded_frame(2, true));
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(
    benches,
    benchmark_adaptive_bitrate,
    benchmark_latest_queue,
    benchmark_frame_pacing,
    benchmark_fake_encoded_video_path,
    benchmark_video_recovery_policy
);
criterion_main!(benches);
