use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rustconsole_protocol::audio::AudioPacket;
use rustconsole_session::audio_datagram::{AudioAssembler, packetize as packetize_audio};
use rustconsole_session::input_datagram::PointerSnapshot;
use rustconsole_session::media_queue::MediaQueue;
use rustconsole_session::video_datagram::{
    VideoFrameAssembler, VideoFramePayload, packetize_video_frame,
};
use std::hint::black_box;
use std::time::{Duration, Instant};

fn video_frame(size: usize) -> VideoFramePayload {
    VideoFramePayload {
        sequence: 1,
        captured_at_micros: 10,
        encoded_at_micros: 20,
        packetized_at_micros: 30,
        input_sequence: 4,
        keyframe: false,
        target_bitrate_bits_per_second: 40_000_000,
        soft_ceiling_bits_per_second: Some(35_000_000),
        estimated_capacity_bits_per_second: 50_000_000,
        payload: vec![42; size],
    }
}

fn audio_packet(size: usize) -> AudioPacket {
    AudioPacket {
        generation: 1,
        sequence: 1,
        captured_at_micros: 10,
        decoded_samples: 480,
        skip_start_samples: 0,
        skip_end_samples: 0,
        payload: vec![42; size],
    }
}

fn benchmark_video_datagrams(criterion: &mut Criterion) {
    let mut packetize = criterion.benchmark_group("video_datagram/packetize");
    for size in [64 * 1024_usize, 1024 * 1024] {
        let frame = video_frame(size);
        packetize.throughput(Throughput::Bytes(size as u64));
        packetize.bench_with_input(BenchmarkId::from_parameter(size), &size, |bench, _| {
            bench.iter(|| packetize_video_frame(black_box(&frame), 1_200).unwrap());
        });
    }
    packetize.finish();

    let mut assemble = criterion.benchmark_group("video_datagram/assemble");
    for size in [64 * 1024_usize, 1024 * 1024] {
        let datagrams = packetize_video_frame(&video_frame(size), 1_200).unwrap();
        assemble.throughput(Throughput::Bytes(size as u64));
        assemble.bench_with_input(BenchmarkId::from_parameter(size), &size, |bench, _| {
            bench.iter_batched(
                || VideoFrameAssembler::new(120),
                |mut assembler| {
                    let now = Instant::now();
                    for datagram in &datagrams {
                        black_box(
                            assembler
                                .push(datagram, now, Duration::from_millis(5))
                                .unwrap(),
                        );
                    }
                },
                BatchSize::SmallInput,
            );
        });
    }
    assemble.finish();
}

fn benchmark_audio_datagrams(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("audio_datagram");
    for size in [400_usize, 1_275, 7_657] {
        let packet = audio_packet(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("packetize", size), &size, |bench, _| {
            bench.iter(|| packetize_audio(black_box(&packet), 1_200).unwrap());
        });

        let datagrams = packetize_audio(&packet, 1_200).unwrap();
        group.bench_with_input(BenchmarkId::new("assemble", size), &size, |bench, _| {
            bench.iter_batched(
                AudioAssembler::default,
                |mut assembler| {
                    let now = Instant::now();
                    for datagram in &datagrams {
                        black_box(assembler.push(datagram, now));
                    }
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn benchmark_input_datagram(criterion: &mut Criterion) {
    let snapshot = PointerSnapshot::Relative {
        generation: 7,
        sequence: 42,
        cumulative_x: -100,
        cumulative_y: 200,
    };
    criterion.bench_function("input_datagram/round_trip", |bench| {
        bench.iter(|| PointerSnapshot::decode(black_box(&snapshot.encode())).unwrap());
    });
}

fn benchmark_media_queue(criterion: &mut Criterion) {
    criterion.bench_function("media_queue/push_pop_64", |bench| {
        bench.iter_batched(
            || MediaQueue::new(64),
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

criterion_group!(
    benches,
    benchmark_video_datagrams,
    benchmark_audio_datagrams,
    benchmark_input_datagram,
    benchmark_media_queue
);
criterion_main!(benches);
