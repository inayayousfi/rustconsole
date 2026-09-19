use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use prost::Message;
use rustconsole_protocol::audio::{HEADER_SIZE, Header};
use rustconsole_protocol::wire::{
    InputPack, InputTransition, PointerMotionTransition, input_transition,
};
use rustconsole_protocol::{
    FeatureId, FeatureOffer, FeatureRequirement, FeatureVersionRange, negotiate_features,
};
use std::hint::black_box;

fn feature_offers(count: u16) -> Vec<FeatureOffer> {
    (1..=count)
        .map(|id| FeatureOffer {
            id: FeatureId::new(id).unwrap(),
            versions: FeatureVersionRange::new(1, 4).unwrap(),
            requirement: FeatureRequirement::Optional,
        })
        .collect()
}

fn benchmark_feature_negotiation(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("feature_negotiation");
    for count in [8_u16, 64, 256] {
        let local = feature_offers(count);
        let peer = feature_offers(count);
        group.throughput(Throughput::Elements(u64::from(count)));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |bench, _| {
            bench.iter(|| negotiate_features(black_box(&local), black_box(&peer)).unwrap());
        });
    }
    group.finish();
}

fn benchmark_audio_header(criterion: &mut Criterion) {
    let header = Header {
        generation: 1,
        sequence: 2,
        captured_at_micros: 3,
        decoded_samples: 480,
        skip_start_samples: 312,
        skip_end_samples: 0,
        packet_size: 1_275,
        fragment_index: 0,
        fragment_count: 2,
        offset: 0,
        payload_size: 1_152,
    };
    criterion.bench_function("audio_header/encode", |bench| {
        bench.iter(|| black_box(header).encode().unwrap());
    });

    let mut datagram = Vec::with_capacity(HEADER_SIZE + usize::from(header.payload_size));
    datagram.extend_from_slice(&header.encode().unwrap());
    datagram.resize(datagram.capacity(), 42);
    criterion.bench_function("audio_header/decode", |bench| {
        bench.iter(|| Header::decode(black_box(&datagram)).unwrap());
    });
}

fn benchmark_reliable_input_pack(criterion: &mut Criterion) {
    let pack = InputPack {
        transitions: (1..=64)
            .map(|sequence| InputTransition {
                generation: 1,
                sequence,
                action: Some(input_transition::Action::PointerMotion(
                    PointerMotionTransition {
                        delta_x: sequence as i32,
                        delta_y: -(sequence as i32),
                    },
                )),
                player_sent_at_micros: 10,
            })
            .collect(),
    };
    criterion.bench_function("reliable_input_pack/64_round_trip", |bench| {
        bench.iter(|| {
            let encoded = black_box(&pack).encode_to_vec();
            black_box(InputPack::decode(encoded.as_slice()).unwrap())
        });
    });
}

criterion_group!(
    benches,
    benchmark_feature_negotiation,
    benchmark_audio_header,
    benchmark_reliable_input_pack
);
criterion_main!(benches);
