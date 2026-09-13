use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use rustconsole_host_core::{
    AdaptiveBitrateController, LatestQueue, VideoDeliveryReport, VideoPathReport,
};
use std::hint::black_box;
use std::time::Duration;

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

criterion_group!(benches, benchmark_adaptive_bitrate, benchmark_latest_queue);
criterion_main!(benches);
