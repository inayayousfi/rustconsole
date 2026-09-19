use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use rustconsole_media::{AudioFormat, AudioSamples, MediaTimestampMicros};
use rustconsole_player_core::{
    AudioDecodePlanner, AudioPlaybackEvent, AudioPlaybackQueue, DecodedAudioEvent,
    DecodedAudioSamples, LatestVideoQueue, StreamTransportStatistics, VideoPlaybackClock,
    VideoStreamSample, encode_reliable_input,
};
use rustconsole_protocol::InputEvent;
use rustconsole_protocol::audio::AudioPacket;
use rustconsole_session::video_datagram::{VideoAssemblyStats, VideoFramePayload};
use std::hint::black_box;
use std::time::{Duration, Instant};

fn audio_packet(sequence: u64) -> AudioPlaybackEvent {
    let now = Instant::now();
    AudioPlaybackEvent::Packet {
        packet: AudioPacket {
            generation: 1,
            sequence,
            captured_at_micros: sequence * 10_000,
            decoded_samples: 480,
            skip_start_samples: 0,
            skip_end_samples: 0,
            payload: vec![42; 256],
        },
        assembled_at: now,
        released_at: now,
        assembled_at_micros: sequence * 10_000,
        assembled_payload_sha256: None,
    }
}

fn decoded_audio(sequence: u64) -> DecodedAudioSamples {
    let now = Instant::now();
    DecodedAudioSamples {
        generation: 1,
        sequence,
        diagnostics: false,
        assembled_at: now,
        assembled_at_micros: sequence * 10_000,
        decoded_at: now,
        ordered_playout_duration: None,
        decoder_queue_duration: None,
        decode_duration: Duration::ZERO,
        encoded_bytes: 256,
        concealed_packets: 0,
        decoder_input_hash_duration: Duration::ZERO,
        assembly_to_decoder_matched: None,
        samples: AudioSamples {
            captured_at: MediaTimestampMicros(sequence * 10_000),
            format: AudioFormat {
                sample_rate: 48_000,
                channels: 2,
            },
            interleaved: vec![0.0; 960],
        },
    }
}

fn benchmark_audio_policy(criterion: &mut Criterion) {
    criterion.bench_function("player_audio/decode_plan_100_packets", |bench| {
        bench.iter_batched(
            || {
                (
                    AudioDecodePlanner::default(),
                    (0..100).map(audio_packet).collect::<Vec<_>>(),
                )
            },
            |(mut planner, packets)| {
                for packet in packets {
                    black_box(planner.plan(packet));
                }
            },
            BatchSize::SmallInput,
        );
    });

    criterion.bench_function("player_audio/queue_sync_8_packets", |bench| {
        bench.iter_batched(
            || {
                (
                    AudioPlaybackQueue::default(),
                    (0..8).map(decoded_audio).collect::<Vec<_>>(),
                )
            },
            |(queue, packets)| {
                for packet in packets {
                    queue.push(DecodedAudioEvent::Samples(packet));
                }
                for timestamp in (0..8).map(|value| value * 10_000) {
                    black_box(queue.pop_for_video(Some(timestamp)));
                }
            },
            BatchSize::SmallInput,
        );
    });
}

fn benchmark_video_policy(criterion: &mut Criterion) {
    #[derive(Debug)]
    struct FakeDecodedFrame {
        sequence: u64,
        bytes: usize,
    }

    let size = 1024 * 1024;
    let mut group = criterion.benchmark_group("player_video");
    group.bench_function("fake_decode_to_present", |bench| {
        bench.iter_batched(
            || VideoFramePayload {
                sequence: 7,
                captured_at_micros: 10,
                encoded_at_micros: 20,
                packetized_at_micros: 30,
                input_sequence: 4,
                keyframe: true,
                target_bitrate_bits_per_second: 40_000_000,
                estimated_capacity_bits_per_second: 50_000_000,
                soft_ceiling_bits_per_second: Some(35_000_000),
                payload: vec![42; size],
            },
            |frame| {
                let transport = StreamTransportStatistics {
                    round_trip_time: Duration::from_millis(5),
                    assembly: VideoAssemblyStats {
                        completed_frames: 1,
                        completed_payload_bytes: frame.payload.len() as u64,
                        ..VideoAssemblyStats::default()
                    },
                    assembled_at: Some(Instant::now()),
                    assembled_at_micros: 40,
                    assembly_duration: Duration::from_millis(1),
                    assembled_payload_sha256: None,
                };
                black_box(VideoStreamSample::from_frame(&frame, transport));
                let decoded = FakeDecodedFrame {
                    sequence: frame.sequence,
                    bytes: frame.payload.len(),
                };
                let queue = LatestVideoQueue::default();
                queue.push(decoded);
                let (presented, _) = queue.take_latest();
                let presented = black_box(presented.unwrap());
                black_box((presented.sequence, presented.bytes));
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();

    criterion.bench_function("player_video/latest_queue_and_clock_120_frames", |bench| {
        bench.iter_batched(
            || (LatestVideoQueue::default(), Instant::now()),
            |(queue, presented_at)| {
                for sequence in 0..120 {
                    queue.push(FakeDecodedFrame {
                        sequence,
                        bytes: 64 * 1024,
                    });
                    black_box(queue.take_latest());
                    black_box(
                        VideoPlaybackClock::new(sequence * 8_333, presented_at)
                            .timestamp_at(presented_at + Duration::from_millis(1)),
                    );
                }
            },
            BatchSize::SmallInput,
        );
    });
}

fn benchmark_input_policy(criterion: &mut Criterion) {
    let events = (0..64)
        .map(|index| InputEvent::PointerMotion {
            delta_x: index,
            delta_y: -index,
        })
        .collect::<Vec<_>>();
    criterion.bench_function("player_input/encode_64_reliable_transitions", |bench| {
        bench.iter(|| {
            for (index, event) in events.iter().copied().enumerate() {
                black_box(encode_reliable_input(event, 1, index as u64 + 1, 10));
            }
        });
    });
}

criterion_group!(
    benches,
    benchmark_audio_policy,
    benchmark_video_policy,
    benchmark_input_policy
);
criterion_main!(benches);
