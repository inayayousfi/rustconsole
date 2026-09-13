use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use rustconsole_codec_ffmpeg::opus::{
    OpusDecoder, OpusEncoder, OpusEncoderConfiguration, OpusPacket,
};
use std::hint::black_box;

const FRAMES: usize = 480;

fn configuration() -> OpusEncoderConfiguration {
    OpusEncoderConfiguration {
        bitrate_bits_per_second: 128_000,
        packet_duration_micros: 10_000,
    }
}

fn samples() -> Vec<f32> {
    (0..FRAMES)
        .flat_map(|frame| {
            [440.0_f32, 660.0].map(|frequency| {
                0.1 * (frame as f32 * frequency * std::f32::consts::TAU / 48_000.0).sin()
            })
        })
        .collect()
}

fn encoded_packet() -> OpusPacket {
    let mut encoder = OpusEncoder::open(configuration()).unwrap();
    let packet = encoder.encode(1_000_000, samples()).unwrap().remove(0);
    encoder.finish().unwrap();
    packet
}

fn benchmark_opus_encode(criterion: &mut Criterion) {
    let input = samples();
    let mut encoder = OpusEncoder::open(configuration()).unwrap();
    let mut timestamp = 0_u64;
    let mut group = criterion.benchmark_group("opus");
    group.throughput(Throughput::Elements(FRAMES as u64));
    group.bench_function("encode_10ms_stereo", |bench| {
        bench.iter_batched(
            || {
                timestamp += 10_000;
                (timestamp, input.clone())
            },
            |(timestamp, input)| black_box(encoder.encode(timestamp, input).unwrap()),
            BatchSize::SmallInput,
        );
    });
    group.finish();
    encoder.finish().unwrap();
}

fn benchmark_opus_decode(criterion: &mut Criterion) {
    let packet = encoded_packet();
    let mut decoder = OpusDecoder::open().unwrap();
    let mut group = criterion.benchmark_group("opus");
    group.throughput(Throughput::Elements(FRAMES as u64));
    group.bench_function("decode_10ms_stereo", |bench| {
        bench.iter(|| black_box(decoder.decode(black_box(&packet)).unwrap()));
    });
    group.finish();
}

criterion_group!(benches, benchmark_opus_encode, benchmark_opus_decode);
criterion_main!(benches);
