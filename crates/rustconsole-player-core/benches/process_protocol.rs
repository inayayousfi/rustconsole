use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rustconsole_player_core::process_protocol::{
    LaunchRequest, PlayerCommand, PlayerEvent, read_command, read_event, write_event, write_launch,
};
use std::hint::black_box;
use zeroize::Zeroizing;

fn benchmark_launch_round_trip(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("process_protocol/launch_round_trip");
    for password_size in [0_usize, 32, 1_024] {
        let request = LaunchRequest {
            address: "127.0.0.1:47999".parse().unwrap(),
            password: Zeroizing::new(vec![42; password_size]),
            remember_password: true,
            maximum_bitrate_bits_per_second: 100_000_000,
            frames_per_second: 120,
            latency_diagnostics: false,
        };
        group.throughput(Throughput::Bytes(password_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(password_size),
            &password_size,
            |bench, _| {
                bench.iter(|| {
                    let mut bytes = Vec::new();
                    write_launch(&mut bytes, black_box(&request)).unwrap();
                    let command = read_command(&mut bytes.as_slice()).unwrap();
                    black_box(matches!(command, PlayerCommand::Launch(_)));
                });
            },
        );
    }
    group.finish();
}

fn benchmark_event_round_trip(criterion: &mut Criterion) {
    let event = PlayerEvent::Error("x".repeat(2_048));
    criterion.bench_function("process_protocol/error_event_round_trip", |bench| {
        bench.iter(|| {
            let mut bytes = Vec::new();
            write_event(&mut bytes, black_box(&event)).unwrap();
            black_box(read_event(&mut bytes.as_slice()).unwrap());
        });
    });
}

criterion_group!(
    benches,
    benchmark_launch_round_trip,
    benchmark_event_round_trip
);
criterion_main!(benches);
