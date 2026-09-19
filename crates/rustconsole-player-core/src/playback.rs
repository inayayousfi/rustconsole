use rustconsole_media::AudioSamples;
use rustconsole_protocol::audio::{AudioPacket, PACKET_DURATION_MICROS};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const VIDEO_QUEUE_CAPACITY: usize = 2;
const AUDIO_QUEUE_CAPACITY: usize = 8;
const AUDIO_VIDEO_SYNC_TOLERANCE_MICROS: u64 = 40_000;
const MAX_CONCEALED_AUDIO_PACKETS: u64 = 4;

pub struct EncodedAudioDecodeInput {
    pub packet: AudioPacket,
    pub assembled_at: Instant,
    pub released_at: Instant,
    pub assembled_at_micros: u64,
    pub assembled_payload_sha256: Option<[u8; 32]>,
}

pub enum AudioDecodeAction {
    Reset {
        generation: u64,
    },
    Conceal {
        generation: u64,
        sequence: u64,
        captured_at_micros: u64,
        packets: u64,
        timing: Option<(Instant, Instant, u64)>,
    },
    Decode(EncodedAudioDecodeInput),
}

#[derive(Default)]
pub struct AudioDecodePlanner {
    generation: u64,
    expected_sequence: Option<u64>,
    failed_generation: Option<u64>,
}

impl AudioDecodePlanner {
    pub fn plan(&mut self, event: crate::AudioPlaybackEvent) -> Vec<AudioDecodeAction> {
        match event {
            crate::AudioPlaybackEvent::Packet {
                packet,
                assembled_at,
                released_at,
                assembled_at_micros,
                assembled_payload_sha256,
            } => self.packet(EncodedAudioDecodeInput {
                packet,
                assembled_at,
                released_at,
                assembled_at_micros,
                assembled_payload_sha256,
            }),
            crate::AudioPlaybackEvent::Missing {
                generation,
                sequence,
                captured_at_micros,
                missing_packets,
            } => self.missing(generation, sequence, captured_at_micros, missing_packets),
        }
    }

    pub fn decoder_failed(&mut self, generation: u64) {
        if generation == self.generation {
            self.failed_generation = Some(generation);
        }
    }

    fn packet(&mut self, input: EncodedAudioDecodeInput) -> Vec<AudioDecodeAction> {
        let generation = input.packet.generation;
        if generation < self.generation {
            return Vec::new();
        }
        let mut actions = self.prepare_generation(generation);
        if self.failed_generation == Some(generation)
            || self
                .expected_sequence
                .is_some_and(|expected| input.packet.sequence < expected)
        {
            return actions;
        }
        if let Some(expected) = self.expected_sequence
            && input.packet.sequence > expected
        {
            let missing = (input.packet.sequence - expected).min(MAX_CONCEALED_AUDIO_PACKETS);
            actions.push(AudioDecodeAction::Reset { generation });
            actions.push(AudioDecodeAction::Conceal {
                generation,
                sequence: expected,
                captured_at_micros: input
                    .packet
                    .captured_at_micros
                    .saturating_sub(missing * PACKET_DURATION_MICROS),
                packets: missing,
                timing: Some((
                    input.assembled_at,
                    input.released_at,
                    input.assembled_at_micros,
                )),
            });
        }
        self.expected_sequence = input.packet.sequence.checked_add(1);
        actions.push(AudioDecodeAction::Decode(input));
        actions
    }

    fn missing(
        &mut self,
        generation: u64,
        sequence: u64,
        captured_at_micros: u64,
        missing_packets: u64,
    ) -> Vec<AudioDecodeAction> {
        if generation < self.generation || self.failed_generation == Some(generation) {
            return Vec::new();
        }
        let mut actions = self.prepare_generation(generation);
        if !matches!(
            actions.last(),
            Some(AudioDecodeAction::Reset { generation: current }) if *current == generation
        ) {
            actions.push(AudioDecodeAction::Reset { generation });
        }
        let packets = missing_packets.min(MAX_CONCEALED_AUDIO_PACKETS);
        actions.push(AudioDecodeAction::Conceal {
            generation,
            sequence,
            captured_at_micros,
            packets,
            timing: None,
        });
        self.expected_sequence = sequence.checked_add(missing_packets);
        actions
    }

    fn prepare_generation(&mut self, generation: u64) -> Vec<AudioDecodeAction> {
        if generation <= self.generation {
            return Vec::new();
        }
        self.generation = generation;
        self.expected_sequence = None;
        self.failed_generation = None;
        vec![AudioDecodeAction::Reset { generation }]
    }
}

#[derive(Debug)]
pub struct DecodedAudioSamples {
    pub generation: u64,
    pub sequence: u64,
    pub diagnostics: bool,
    pub assembled_at: Instant,
    pub assembled_at_micros: u64,
    pub decoded_at: Instant,
    pub ordered_playout_duration: Option<Duration>,
    pub decoder_queue_duration: Option<Duration>,
    pub decode_duration: Duration,
    pub encoded_bytes: usize,
    pub concealed_packets: u64,
    pub decoder_input_hash_duration: Duration,
    pub assembly_to_decoder_matched: Option<bool>,
    pub samples: AudioSamples,
}

#[derive(Debug)]
pub enum DecodedAudioEvent {
    Reset { generation: u64 },
    Samples(DecodedAudioSamples),
    Failed { generation: u64, detail: String },
}

#[derive(Debug)]
pub enum AudioPlaybackDecision {
    Samples {
        decoded: DecodedAudioSamples,
        queue_duration: Option<Duration>,
        sync_hold_duration: Option<Duration>,
    },
    Late {
        sequence: u64,
        queue_duration: Option<Duration>,
        lateness_micros: u64,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AudioPlaybackQueueSnapshot {
    pub pending_packets: usize,
    pub queue_drops: u64,
    pub late_drops: u64,
    pub resets: u64,
    pub decoder_failures: u64,
    pub detail: String,
}

#[derive(Default)]
struct AudioQueueState {
    pending: VecDeque<PendingAudio>,
    reset: Option<u64>,
    queue_drops: u64,
    late_drops: u64,
    resets: u64,
    decoder_failures: u64,
    detail: String,
}

struct PendingAudio {
    decoded: DecodedAudioSamples,
    queued_at: Option<Instant>,
    sync_hold_started: Option<Instant>,
}

#[derive(Clone, Default)]
pub struct AudioPlaybackQueue {
    state: Arc<Mutex<AudioQueueState>>,
}

impl AudioPlaybackQueue {
    pub fn push(&self, event: DecodedAudioEvent) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match event {
            DecodedAudioEvent::Reset { generation } => {
                state.pending.clear();
                state.reset = Some(generation);
                state.resets = state.resets.saturating_add(1);
                state.detail.clear();
            }
            DecodedAudioEvent::Samples(samples) => {
                if state.pending.len() == AUDIO_QUEUE_CAPACITY {
                    state.pending.pop_front();
                    state.queue_drops = state.queue_drops.saturating_add(1);
                }
                let queued_at = samples.diagnostics.then(Instant::now);
                state.pending.push_back(PendingAudio {
                    decoded: samples,
                    queued_at,
                    sync_hold_started: None,
                });
            }
            DecodedAudioEvent::Failed { detail, .. } => {
                state.pending.clear();
                state.decoder_failures = state.decoder_failures.saturating_add(1);
                state.detail = detail;
            }
        }
    }

    pub fn take_reset(&self) -> Option<u64> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .reset
            .take()
    }

    pub fn pop_for_video(
        &self,
        video_timestamp_micros: Option<u64>,
    ) -> Option<AudioPlaybackDecision> {
        let video_timestamp_micros = video_timestamp_micros?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let now = state
            .pending
            .front()
            .is_some_and(|pending| pending.queued_at.is_some())
            .then(Instant::now);
        let pending = state.pending.front_mut()?;
        let audio_timestamp = pending.decoded.samples.captured_at.0;
        if audio_timestamp
            > video_timestamp_micros.saturating_add(AUDIO_VIDEO_SYNC_TOLERANCE_MICROS)
        {
            if let Some(now) = now {
                pending.sync_hold_started.get_or_insert(now);
            }
            return None;
        }
        let pending = state.pending.pop_front().unwrap();
        let queue_duration = now
            .zip(pending.queued_at)
            .map(|(now, queued_at)| now.saturating_duration_since(queued_at));
        if audio_timestamp.saturating_add(AUDIO_VIDEO_SYNC_TOLERANCE_MICROS)
            < video_timestamp_micros
        {
            state.late_drops = state.late_drops.saturating_add(1);
            return Some(AudioPlaybackDecision::Late {
                sequence: pending.decoded.sequence,
                queue_duration,
                lateness_micros: video_timestamp_micros.saturating_sub(audio_timestamp),
            });
        }
        Some(AudioPlaybackDecision::Samples {
            decoded: pending.decoded,
            queue_duration,
            sync_hold_duration: pending
                .sync_hold_started
                .zip(now)
                .map(|(started, now)| now.saturating_duration_since(started)),
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> AudioPlaybackQueueSnapshot {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        AudioPlaybackQueueSnapshot {
            pending_packets: state.pending.len(),
            queue_drops: state.queue_drops,
            late_drops: state.late_drops,
            resets: state.resets,
            decoder_failures: state.decoder_failures,
            detail: state.detail.clone(),
        }
    }
}

pub struct LatestVideoQueue<T> {
    frames: Mutex<VecDeque<T>>,
    dropped: AtomicU64,
}

impl<T> Default for LatestVideoQueue<T> {
    fn default() -> Self {
        Self {
            frames: Mutex::new(VecDeque::with_capacity(VIDEO_QUEUE_CAPACITY)),
            dropped: AtomicU64::new(0),
        }
    }
}

impl<T> LatestVideoQueue<T> {
    pub fn push(&self, frame: T) {
        let mut frames = self
            .frames
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        while frames.len() >= VIDEO_QUEUE_CAPACITY {
            frames.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        frames.push_back(frame);
    }

    pub fn take_latest(&self) -> (Option<T>, usize) {
        let mut frames = self
            .frames
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let depth = frames.len();
        let latest = frames.pop_back();
        self.dropped
            .fetch_add(frames.len() as u64, Ordering::Relaxed);
        frames.clear();
        (latest, depth)
    }

    pub fn clear(&self) {
        self.frames
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_empty()
    }

    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct VideoPlaybackClock {
    captured_at_micros: u64,
    presented_at: Instant,
}

impl VideoPlaybackClock {
    #[must_use]
    pub const fn new(captured_at_micros: u64, presented_at: Instant) -> Self {
        Self {
            captured_at_micros,
            presented_at,
        }
    }

    #[must_use]
    pub fn timestamp_at(self, now: Instant) -> u64 {
        self.captured_at_micros.saturating_add(
            u64::try_from(now.saturating_duration_since(self.presented_at).as_micros())
                .unwrap_or(u64::MAX),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustconsole_media::{AudioFormat, MediaTimestampMicros};
    use rustconsole_session::video_datagram::{VideoAssemblyStats, VideoFramePayload};

    fn packet(generation: u64, sequence: u64) -> crate::AudioPlaybackEvent {
        let now = Instant::now();
        crate::AudioPlaybackEvent::Packet {
            packet: AudioPacket {
                generation,
                sequence,
                captured_at_micros: sequence * PACKET_DURATION_MICROS,
                decoded_samples: 480,
                skip_start_samples: 0,
                skip_end_samples: 0,
                payload: vec![1],
            },
            assembled_at: now,
            released_at: now,
            assembled_at_micros: 0,
            assembled_payload_sha256: None,
        }
    }

    fn samples(sequence: u64, timestamp: u64) -> DecodedAudioSamples {
        DecodedAudioSamples {
            generation: 1,
            sequence,
            diagnostics: false,
            assembled_at: Instant::now(),
            assembled_at_micros: timestamp,
            decoded_at: Instant::now(),
            ordered_playout_duration: None,
            decoder_queue_duration: None,
            decode_duration: Duration::ZERO,
            encoded_bytes: 1,
            concealed_packets: 0,
            decoder_input_hash_duration: Duration::ZERO,
            assembly_to_decoder_matched: None,
            samples: AudioSamples {
                captured_at: MediaTimestampMicros(timestamp),
                format: AudioFormat {
                    sample_rate: 48_000,
                    channels: 2,
                },
                interleaved: vec![0.0; 960],
            },
        }
    }

    #[test]
    fn latest_video_queue_bounds_latency_and_reports_drops() {
        let queue = LatestVideoQueue::default();
        queue.push(1);
        queue.push(2);
        queue.push(3);
        assert_eq!(queue.take_latest(), (Some(3), 2));
        assert_eq!(queue.dropped(), 2);
    }

    #[test]
    fn audio_waits_when_ahead_and_drops_when_late() {
        let queue = AudioPlaybackQueue::default();
        queue.push(DecodedAudioEvent::Samples(samples(1, 100_000)));
        assert!(queue.pop_for_video(Some(50_000)).is_none());
        assert!(matches!(
            queue.pop_for_video(Some(100_000)),
            Some(AudioPlaybackDecision::Samples { .. })
        ));
        queue.push(DecodedAudioEvent::Samples(samples(2, 100_000)));
        assert!(matches!(
            queue.pop_for_video(Some(150_000)),
            Some(AudioPlaybackDecision::Late { sequence: 2, .. })
        ));
    }

    #[test]
    fn audio_queue_is_bounded_and_reset_clears_pending_samples() {
        let queue = AudioPlaybackQueue::default();
        for sequence in 0..=AUDIO_QUEUE_CAPACITY as u64 {
            queue.push(DecodedAudioEvent::Samples(samples(
                sequence,
                sequence * 10_000,
            )));
        }
        assert_eq!(queue.snapshot().pending_packets, AUDIO_QUEUE_CAPACITY);
        assert_eq!(queue.snapshot().queue_drops, 1);
        queue.push(DecodedAudioEvent::Reset { generation: 2 });
        assert_eq!(queue.take_reset(), Some(2));
        assert_eq!(queue.snapshot().pending_packets, 0);
        assert_eq!(queue.snapshot().resets, 1);
    }

    #[test]
    fn audio_queue_reports_a_completed_video_sync_hold() {
        let queue = AudioPlaybackQueue::default();
        let mut decoded = samples(1, 100_001);
        decoded.diagnostics = true;
        queue.push(DecodedAudioEvent::Samples(decoded));
        assert!(queue.pop_for_video(Some(60_000)).is_none());
        assert!(matches!(
            queue.pop_for_video(Some(60_001)),
            Some(AudioPlaybackDecision::Samples {
                sync_hold_duration: Some(_),
                ..
            })
        ));
    }

    #[test]
    fn audio_decode_planner_resets_generations_and_bounds_gap_concealment() {
        let mut planner = AudioDecodePlanner::default();
        assert!(matches!(
            planner.plan(packet(1, 0)).as_slice(),
            [
                AudioDecodeAction::Reset { generation: 1 },
                AudioDecodeAction::Decode(_)
            ]
        ));
        let actions = planner.plan(packet(1, 10));
        assert!(matches!(
            actions.as_slice(),
            [
                AudioDecodeAction::Reset { generation: 1 },
                AudioDecodeAction::Conceal {
                    sequence: 1,
                    packets: 4,
                    ..
                },
                AudioDecodeAction::Decode(_)
            ]
        ));
    }

    #[test]
    fn audio_decode_planner_discards_stale_generations_after_recovery() {
        let mut planner = AudioDecodePlanner::default();
        planner.plan(packet(2, 0));
        assert!(planner.plan(packet(1, 1)).is_empty());
        planner.decoder_failed(2);
        assert!(planner.plan(packet(2, 1)).is_empty());
        assert!(matches!(
            planner.plan(packet(3, 0)).as_slice(),
            [
                AudioDecodeAction::Reset { generation: 3 },
                AudioDecodeAction::Decode(_)
            ]
        ));
    }

    #[test]
    fn fake_audio_decode_reaches_the_playback_queue() {
        let mut planner = AudioDecodePlanner::default();
        let actions = planner.plan(packet(1, 7));
        let encoded = actions
            .into_iter()
            .find_map(|action| match action {
                AudioDecodeAction::Decode(input) => Some(input),
                _ => None,
            })
            .unwrap();
        let queue = AudioPlaybackQueue::default();
        queue.push(DecodedAudioEvent::Samples(samples(
            encoded.packet.sequence,
            encoded.packet.captured_at_micros,
        )));

        assert!(matches!(
            queue.pop_for_video(Some(70_000)),
            Some(AudioPlaybackDecision::Samples { decoded, .. })
                if decoded.sequence == 7 && decoded.samples.interleaved.len() == 960
        ));
    }

    #[test]
    fn video_clock_advances_from_the_presented_source_timestamp() {
        let presented_at = Instant::now();
        let clock = VideoPlaybackClock::new(5_000, presented_at);
        assert_eq!(
            clock.timestamp_at(presented_at + Duration::from_millis(100)),
            105_000
        );
    }

    #[test]
    fn fake_received_frame_reaches_the_presentation_queue() {
        #[derive(Debug, Eq, PartialEq)]
        struct FakeDecodedFrame {
            sequence: u64,
            payload: Vec<u8>,
        }

        let frame = VideoFramePayload {
            sequence: 7,
            captured_at_micros: 10,
            encoded_at_micros: 20,
            packetized_at_micros: 30,
            input_sequence: 4,
            keyframe: true,
            target_bitrate_bits_per_second: 40_000_000,
            estimated_capacity_bits_per_second: 50_000_000,
            soft_ceiling_bits_per_second: Some(35_000_000),
            payload: vec![42; 64 * 1024],
        };
        let transport = crate::StreamTransportStatistics {
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
        let sample = crate::VideoStreamSample::from_frame(&frame, transport);

        // This is the codec boundary: a deterministic fake decoder preserves the
        // sequence and makes the encoded payload available as a fake frame.
        let decoded = FakeDecodedFrame {
            sequence: frame.sequence,
            payload: frame.payload,
        };
        let queue = LatestVideoQueue::default();
        queue.push(decoded);
        let (presented, depth) = queue.take_latest();

        assert_eq!(depth, 1);
        assert_eq!(sample.encoded_frame_bytes, 64 * 1024);
        assert_eq!(
            presented,
            Some(FakeDecodedFrame {
                sequence: 7,
                payload: vec![42; 64 * 1024],
            })
        );
    }
}
