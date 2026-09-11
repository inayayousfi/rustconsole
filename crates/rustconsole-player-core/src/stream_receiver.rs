use crate::{
    PayloadIntegrityCounters, PayloadIntegritySample, StreamConsumers, StreamProgress,
    StreamTransportStatistics, TimedInputEvent,
};
use rustconsole_protocol::InputEvent;
use rustconsole_protocol::audio::AudioPacket;
use rustconsole_protocol::diagnostics::{MediaKind, PayloadDigest, RECORD_SIZE, STREAM_PREAMBLE};
use rustconsole_protocol::wire::{
    AudioStatus, AudioStreamState, ClockPing, ClockPong, Envelope, HostSessionControlKind,
    InputTransition, KeyTransition, KeyboardLeds, PointerButtonTransition, PointerMode,
    PointerModeTransition, ReleaseAll, VideoControl, VideoControlKind, VideoReceiverReport,
    VideoReconfigurationCause, WheelTransition, envelope, input_transition,
};
use rustconsole_session::audio_datagram::{
    AUDIO_QUEUE_PACKETS, AUDIO_WAIT, AudioAssembler, AudioReceiveStatistics,
};
use rustconsole_session::input_datagram::PointerSnapshot;
use rustconsole_session::media_queue::MediaQueue;
use rustconsole_session::quic::{read_envelope, write_envelope};
use rustconsole_session::video_datagram::{
    FrameAssemblyProgress, VideoAssemblyStats, VideoFrameAssembler, VideoFramePayload,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

enum DiagnosticEvent {
    ReleasePointerCapture,
    Clock(crate::ClockOffsetEstimate),
    InputAck {
        sequence: u64,
        player_sent_at_micros: u64,
        player_received_at_micros: u64,
        host_received_at_micros: u64,
        host_submitted_at_micros: u64,
        pointer_datagrams_received: u64,
        pointer_updates_applied: u64,
        pointer_updates_ignored: u64,
        mouse_reports_published: u64,
        keyboard_reports_published: u64,
        reliable_transitions_received: u64,
        reliable_transitions_applied: u64,
        reliable_transitions_rejected: u64,
        reliable_transitions_missing: u64,
        reliable_transitions_duplicate_or_late: u64,
        release_all_transitions: u64,
        pointer_missing_datagrams: u64,
        pointer_stale_generations: u64,
        pointer_duplicate_or_late: u64,
        pointer_mode_rejections: u64,
        pointer_relative_baselines: u64,
    },
    InputSent {
        sequence: u64,
        occurred_at: Instant,
        sent_at: Instant,
        send_completed_at: Instant,
        correlates_test_marker: bool,
    },
}

#[derive(Clone, Copy)]
struct LocalPayloadDigest {
    kind: MediaKind,
    generation: u64,
    sequence: u64,
    payload_size: u64,
    hash_duration_micros: u64,
    sha256: [u8; 32],
    assembled_at_micros: u64,
}

enum PayloadDigestEvent {
    Host(PayloadDigest),
    Local(LocalPayloadDigest),
}

#[derive(Default)]
struct PayloadDigestMatcher {
    host: BTreeMap<(u8, u64, u64), PayloadDigest>,
    local: BTreeMap<(u8, u64, u64), LocalPayloadDigest>,
    counters: PayloadIntegrityCounters,
}

impl PayloadDigestMatcher {
    fn push(&mut self, event: PayloadDigestEvent) -> Option<PayloadIntegritySample> {
        const MAX_PENDING: usize = 4_096;
        let key = match event {
            PayloadDigestEvent::Host(host) => {
                let key = (host.kind as u8, host.generation, host.sequence);
                if self.host.insert(key, host).is_some() {
                    self.counters.duplicate_host_records += 1;
                }
                key
            }
            PayloadDigestEvent::Local(local) => {
                let key = (local.kind as u8, local.generation, local.sequence);
                if self.local.insert(key, local).is_some() {
                    self.counters.duplicate_player_payloads += 1;
                }
                key
            }
        };
        while self.host.len() > MAX_PENDING {
            self.host.pop_first();
            self.counters.unmatched_host_records += 1;
        }
        while self.local.len() > MAX_PENDING {
            self.local.pop_first();
            self.counters.unmatched_player_payloads += 1;
        }
        if !self.host.contains_key(&key) || !self.local.contains_key(&key) {
            return None;
        }
        let host = self.host.remove(&key).unwrap();
        let local = self.local.remove(&key).unwrap();
        Some(PayloadIntegritySample {
            kind: host.kind,
            generation: host.generation,
            sequence: host.sequence,
            payload_size: host.payload_size,
            matched: host.payload_size == local.payload_size && host.sha256 == local.sha256,
            producer_hash_duration_micros: host.producer_hash_duration_micros,
            host_hash_duration_micros: host.boundary_hash_duration_micros,
            player_hash_duration_micros: local.hash_duration_micros,
            host_dropped_records: host.producer_dropped_records,
            worker_to_service_matched: host.boundary_matched,
            encode_started_at_micros: host.encode_started_at_micros,
            encoded_at_micros: host.encoded_at_micros,
            worker_queued_at_micros: host.worker_queued_at_micros,
            service_received_at_micros: host.service_received_at_micros,
            packetized_at_micros: host.packetized_at_micros,
            assembled_at_micros: local.assembled_at_micros,
            captured_at_micros: host.captured_at_micros,
            mirror_decode_micros: host.mirror_decode_micros,
            quality_present: host.quality_present,
            quality_presentation_timestamp: host.quality_presentation_timestamp,
            source_readback_micros: host.source_readback_micros,
            decoded_readback_micros: host.decoded_readback_micros,
            scoring_micros: host.scoring_micros,
            readback_bytes: host.readback_bytes,
            luma_psnr_millidecibels: host.luma_psnr_millidecibels,
            luma_mean_absolute_error_ppm: host.luma_mean_absolute_error_ppm,
            packetization_completed_at_micros: host.packetization_completed_at_micros,
            first_send_attempt_at_micros: host.first_send_attempt_at_micros,
            last_send_completed_at_micros: host.last_send_completed_at_micros,
            capture_acquisition_micros: host.capture_acquisition_micros,
            cross_adapter_copy_micros: host.cross_adapter_copy_micros,
            color_conversion_micros: host.color_conversion_micros,
            encoder_call_micros: host.encoder_call_micros,
            audio_capture_buffer_frames: host.audio_capture_buffer_frames,
            audio_capture_discontinuities: host.audio_capture_discontinuities,
            audio_invalid_capture_timestamps: host.audio_invalid_capture_timestamps,
            audio_device_reopens: host.audio_device_reopens,
            audio_encoder_resets: host.audio_encoder_resets,
            audio_capture_queue_depth: host.audio_capture_queue_depth,
            audio_capture_queue_capacity: host.audio_capture_queue_capacity,
            audio_capture_queue_drops: host.audio_capture_queue_drops,
        })
    }

    fn counters(&self) -> PayloadIntegrityCounters {
        PayloadIntegrityCounters {
            pending_host_records: self.host.len() as u64,
            pending_player_payloads: self.local.len() as u64,
            ..self.counters
        }
    }
}

fn local_payload_digest(
    kind: MediaKind,
    generation: u64,
    sequence: u64,
    payload: &[u8],
    assembled_at_micros: u64,
) -> LocalPayloadDigest {
    let started = Instant::now();
    let sha256 = Sha256::digest(payload).into();
    LocalPayloadDigest {
        kind,
        generation,
        sequence,
        payload_size: payload.len() as u64,
        hash_duration_micros: started.elapsed().as_micros() as u64,
        sha256,
        assembled_at_micros,
    }
}

fn elapsed_micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn clock_offset(
    pong: ClockPong,
    player_received_at_micros: u64,
) -> Option<crate::ClockOffsetEstimate> {
    if pong.player_sent_at_micros > player_received_at_micros
        || pong.host_received_at_micros > pong.host_sent_at_micros
    {
        return None;
    }
    let round_trip = player_received_at_micros - pong.player_sent_at_micros;
    let host_processing = pong.host_sent_at_micros - pong.host_received_at_micros;
    if host_processing > round_trip {
        return None;
    }
    let offset = ((i128::from(pong.host_received_at_micros)
        - i128::from(pong.player_sent_at_micros))
        + (i128::from(pong.host_sent_at_micros) - i128::from(player_received_at_micros)))
        / 2;
    Some(crate::ClockOffsetEstimate {
        offset_micros: i64::try_from(offset).ok()?,
        uncertainty_micros: (round_trip - host_processing).div_ceil(2),
    })
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioTransportSnapshot {
    pub receive: AudioReceiveStatistics,
    pub host: AudioStreamState,
    pub video_queue_drops: u64,
    pub audio_queue_drops: u64,
}

impl Eq for AudioTransportSnapshot {}

impl AudioTransportSnapshot {
    pub fn stopped(&self) -> bool {
        matches!(
            AudioStatus::try_from(self.host.status),
            Ok(AudioStatus::Unavailable | AudioStatus::Failed | AudioStatus::Stopped)
        )
    }

    pub fn negotiated(&self) -> bool {
        self.host.status != AudioStatus::NotNegotiated as i32
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioPlaybackEvent {
    Packet {
        packet: AudioPacket,
        assembled_at: Instant,
        released_at: Instant,
        assembled_at_micros: u64,
        assembled_payload_sha256: Option<[u8; 32]>,
    },
    Missing {
        generation: u64,
        sequence: u64,
        captured_at_micros: u64,
        missing_packets: u64,
    },
}

#[derive(Default)]
struct OrderedAudioPackets {
    generation: u64,
    expected: Option<u64>,
    pending: BTreeMap<u64, (AudioPacket, Instant, u64, Option<[u8; 32]>)>,
    started: bool,
    last_timestamp: Option<u64>,
}

impl OrderedAudioPackets {
    fn generation(&mut self, generation: u64) {
        if generation > self.generation {
            self.generation = generation;
            self.expected = None;
            self.pending.clear();
            self.started = false;
            self.last_timestamp = None;
        }
    }

    fn push(
        &mut self,
        packet: AudioPacket,
        now: Instant,
        assembled_at_micros: u64,
        assembled_payload_sha256: Option<[u8; 32]>,
    ) -> Vec<AudioPlaybackEvent> {
        if packet.generation < self.generation {
            return Vec::new();
        }
        self.generation(packet.generation);
        if self.started
            && self
                .expected
                .is_some_and(|expected| packet.sequence < expected)
        {
            return Vec::new();
        }
        if !self.started {
            self.expected = Some(
                self.expected
                    .map_or(packet.sequence, |expected| expected.min(packet.sequence)),
            );
        }
        self.pending.entry(packet.sequence).or_insert((
            packet,
            now,
            assembled_at_micros,
            assembled_payload_sha256,
        ));
        self.drain(now)
    }

    fn expire(&mut self, now: Instant) -> Vec<AudioPlaybackEvent> {
        self.drain(now)
    }

    fn drain(&mut self, now: Instant) -> Vec<AudioPlaybackEvent> {
        if !self.started {
            let old_enough =
                self.pending
                    .first_key_value()
                    .is_some_and(|(_, (_, received, _, _))| {
                        now.duration_since(*received) >= AUDIO_WAIT
                    });
            if self.pending.len() < AUDIO_QUEUE_PACKETS && !old_enough {
                return Vec::new();
            }
            self.started = true;
        }

        let mut ready = Vec::new();
        while let Some(expected) = self.expected {
            if let Some((packet, assembled_at, assembled_at_micros, assembled_payload_sha256)) =
                self.pending.remove(&expected)
            {
                self.last_timestamp = Some(packet.captured_at_micros);
                self.expected = expected.checked_add(1);
                ready.push(AudioPlaybackEvent::Packet {
                    packet,
                    assembled_at,
                    released_at: now,
                    assembled_at_micros,
                    assembled_payload_sha256,
                });
                continue;
            }
            let Some((&next_sequence, (next, received, _, _))) = self.pending.first_key_value()
            else {
                break;
            };
            let missing_is_confirmed = self.pending.len() >= AUDIO_QUEUE_PACKETS
                || now.duration_since(*received) >= AUDIO_WAIT;
            if !missing_is_confirmed {
                break;
            }
            let missing_packets = next_sequence.saturating_sub(expected);
            let captured_at_micros = self
                .last_timestamp
                .and_then(|timestamp| {
                    timestamp.checked_add(rustconsole_protocol::audio::PACKET_DURATION_MICROS)
                })
                .or_else(|| {
                    next_sequence
                        .checked_sub(expected)
                        .and_then(|distance| {
                            distance
                                .checked_mul(rustconsole_protocol::audio::PACKET_DURATION_MICROS)
                        })
                        .and_then(|distance| next.captured_at_micros.checked_sub(distance))
                })
                .unwrap_or(next.captured_at_micros);
            self.last_timestamp = next
                .captured_at_micros
                .checked_sub(rustconsole_protocol::audio::PACKET_DURATION_MICROS);
            self.expected = Some(next_sequence);
            ready.push(AudioPlaybackEvent::Missing {
                generation: self.generation,
                sequence: expected,
                captured_at_micros,
                missing_packets,
            });
        }
        ready
    }
}

#[derive(Clone)]
struct Snapshot {
    audio: AudioTransportSnapshot,
    video: VideoAssemblyStats,
    progress: Option<FrameAssemblyProgress>,
    rtt: Duration,
    keyboard_leds: Option<KeyboardLedSnapshot>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct KeyboardLedSnapshot {
    generation: u64,
    sequence: u64,
    mask: u8,
}

#[derive(Default)]
struct KeyboardLedReceiver {
    last: Option<KeyboardLedSnapshot>,
}

impl KeyboardLedReceiver {
    fn push(&mut self, state: KeyboardLeds) -> Result<KeyboardLedSnapshot, &'static str> {
        let mask = u8::try_from(state.mask).map_err(|_| "keyboard LED mask is out of range")?;
        if state.generation == 0 || state.sequence == 0 || mask & !0x1f != 0 {
            return Err("invalid keyboard LED state");
        }
        if let Some(last) = self.last {
            if state.generation < last.generation {
                return Err("stale keyboard LED generation");
            }
            if state.generation == last.generation
                && last.sequence.checked_add(1) != Some(state.sequence)
            {
                return Err("out-of-order keyboard LED sequence");
            }
        }
        let next = KeyboardLedSnapshot {
            generation: state.generation,
            sequence: state.sequence,
            mask,
        };
        self.last = Some(next);
        Ok(next)
    }
}

struct Receiver {
    started: Instant,
    video: VideoFrameAssembler,
    audio: AudioAssembler,
    audio_enabled: bool,
    ordered_audio: OrderedAudioPackets,
    audio_events: Arc<MediaQueue<AudioPlaybackEvent>>,
    frames: Arc<MediaQueue<AssembledVideoFrame>>,
    payload_digests: Option<Arc<MediaQueue<PayloadDigestEvent>>>,
    snapshot: Arc<Mutex<Snapshot>>,
    recover: Arc<AtomicBool>,
}

struct AssembledVideoFrame {
    frame: VideoFramePayload,
    payload_sha256: Option<[u8; 32]>,
}

impl Receiver {
    fn datagram(&mut self, bytes: &[u8], now: Instant, rtt: Duration) -> Result<(), String> {
        if bytes.starts_with(&rustconsole_protocol::audio::MAGIC) {
            if self.audio_enabled
                && let Some(packet) = self.audio.push(bytes, now)
            {
                let digest = self.payload_digests.as_ref().map(|digests| {
                    let digest = local_payload_digest(
                        MediaKind::Audio,
                        packet.generation,
                        packet.sequence,
                        &packet.payload,
                        elapsed_micros(self.started),
                    );
                    digests.push(PayloadDigestEvent::Local(digest));
                    digest
                });
                let events = self.ordered_audio.push(
                    packet,
                    now,
                    elapsed_micros(self.started),
                    digest.map(|digest| digest.sha256),
                );
                self.queue_audio(events);
            }
        } else {
            let assembled = self.video.push(bytes, now, rtt).map_err(|error| {
                format!(
                    "{error}: length={} prefix={:02x?}",
                    bytes.len(),
                    &bytes[..bytes.len().min(4)]
                )
            })?;
            if assembled.dependency_lost {
                self.recover.store(true, Ordering::Release);
            }
            let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
            snapshot.progress = assembled.progress;
            if let Some(frame) = assembled.frame {
                let digest = self.payload_digests.as_ref().map(|digests| {
                    let digest = local_payload_digest(
                        MediaKind::Video,
                        1,
                        frame.sequence,
                        &frame.payload,
                        elapsed_micros(self.started),
                    );
                    digests.push(PayloadDigestEvent::Local(digest));
                    digest
                });
                if self
                    .frames
                    .push(AssembledVideoFrame {
                        frame,
                        payload_sha256: digest.map(|digest| digest.sha256),
                    })
                    .is_some()
                {
                    snapshot.audio.video_queue_drops += 1;
                    self.recover.store(true, Ordering::Release);
                }
            }
        }
        self.publish(rtt);
        Ok(())
    }

    fn queue_audio(&self, events: Vec<AudioPlaybackEvent>) {
        for event in events {
            if self.audio_events.push(event).is_some() {
                self.snapshot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .audio
                    .audio_queue_drops += 1;
            }
        }
    }

    fn publish(&self, rtt: Duration) {
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        snapshot.video = self.video.stats();
        snapshot.rtt = rtt;
        snapshot.audio.receive = self.audio.statistics();
        if snapshot.audio.receive.generation > snapshot.audio.host.generation {
            snapshot.audio.host.generation = snapshot.audio.receive.generation;
            snapshot.audio.host.status = AudioStatus::Active as i32;
            snapshot.audio.host.detail.clear();
        }
    }

    fn state(&mut self, state: AudioStreamState) {
        if !self.audio_enabled
            || !state.valid()
            || state.generation < self.audio.statistics().generation
        {
            return;
        }
        self.audio.generation(state.generation);
        self.ordered_audio.generation(state.generation);
        self.snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .audio
            .host = state;
    }
}

#[derive(Default)]
struct DecodeContinuity {
    last: Option<u64>,
    ready: bool,
}

impl DecodeContinuity {
    fn accept(&mut self, sequence: u64, keyframe: bool) -> bool {
        if self
            .last
            .is_some_and(|last| last.checked_add(1) != Some(sequence))
        {
            self.ready = false;
        }
        self.last = Some(sequence);
        if keyframe {
            self.ready = true;
        }
        self.ready
    }
}

struct ReaderGuard {
    connection: quinn::Connection,
    stop: Arc<AtomicBool>,
    task: Option<tokio::task::JoinHandle<Result<StreamEnd, String>>>,
}

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.connection.close(0_u32.into(), b"player stopped");
    }
}

pub(super) fn close_with_stream_error(connection: &quinn::Connection, error: &str) {
    let mut end = error.len().min(1_024);
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    connection.close(0x202_u32.into(), &error.as_bytes()[..end]);
}

pub(super) struct ReceiveStreamParameters<Stop, Input, Progress, Audio, Video> {
    pub(super) connection: quinn::Connection,
    pub(super) control: (quinn::SendStream, quinn::RecvStream),
    pub(super) input_stream: Option<quinn::SendStream>,
    pub(super) fps: u16,
    pub(super) audio_enabled: bool,
    pub(super) host_pointer_release: bool,
    pub(super) diagnostic_stream: Option<quinn::RecvStream>,
    pub(super) should_stop: Stop,
    pub(super) next_input: Input,
    pub(super) progress: Progress,
    pub(super) consumers: StreamConsumers<Audio, Video>,
}

pub(super) async fn receive_stream<Stop, Input, Progress, Audio, Video>(
    parameters: ReceiveStreamParameters<Stop, Input, Progress, Audio, Video>,
) -> Result<StreamEnd, Box<dyn std::error::Error>>
where
    Stop: Fn() -> bool,
    Input: FnMut() -> Option<TimedInputEvent> + Send + 'static,
    Progress: FnMut(StreamProgress),
    Audio: FnMut(AudioPlaybackEvent) -> Result<(), Box<dyn std::error::Error>>,
    Video: FnMut(
        VideoFramePayload,
        StreamTransportStatistics,
    ) -> Result<bool, Box<dyn std::error::Error>>,
{
    let ReceiveStreamParameters {
        connection,
        control,
        input_stream,
        fps,
        audio_enabled,
        host_pointer_release,
        diagnostic_stream,
        should_stop,
        mut next_input,
        mut progress,
        consumers,
    } = parameters;
    let StreamConsumers {
        audio: mut consume_audio,
        video: mut consume_video,
    } = consumers;
    let (mut send, mut receive) = control;
    let mut input_send = input_stream;
    let frames = Arc::new(MediaQueue::new(2));
    let audio_events = Arc::new(MediaQueue::new(AUDIO_QUEUE_PACKETS + 1));
    let diagnostic_events = Arc::new(MediaQueue::new(1024));
    let payload_digests = diagnostic_stream
        .as_ref()
        .map(|_| Arc::new(MediaQueue::new(4_096)));
    let full_diagnostics = payload_digests.is_some();
    let snapshot = Arc::new(Mutex::new(Snapshot {
        audio: AudioTransportSnapshot {
            receive: AudioReceiveStatistics::default(),
            host: AudioStreamState::new(
                0,
                if audio_enabled {
                    AudioStatus::Waiting
                } else {
                    AudioStatus::NotNegotiated
                },
                0,
                String::new(),
            ),
            video_queue_drops: 0,
            audio_queue_drops: 0,
        },
        video: VideoAssemblyStats::default(),
        progress: None,
        rtt: connection.rtt(),
        keyboard_leds: None,
    }));
    let recover = Arc::new(AtomicBool::new(false));
    let request_keyframe = Arc::new(AtomicBool::new(false));
    let keyframe_requests = Arc::new(AtomicU64::new(0));
    let keyframe_recovery_started = Arc::new(Mutex::new(None::<Instant>));
    let stop = Arc::new(AtomicBool::new(false));
    let session_started = Instant::now();
    let mut receiver = Receiver {
        started: session_started,
        video: VideoFrameAssembler::new(fps),
        audio: AudioAssembler::default(),
        audio_enabled,
        ordered_audio: OrderedAudioPackets::default(),
        audio_events: Arc::clone(&audio_events),
        frames: Arc::clone(&frames),
        payload_digests: payload_digests.as_ref().map(Arc::clone),
        snapshot: Arc::clone(&snapshot),
        recover: Arc::clone(&recover),
    };
    let reader_stop = Arc::clone(&stop);
    let reader_request = Arc::clone(&request_keyframe);
    let reader_keyframe_requests = Arc::clone(&keyframe_requests);
    let reader_keyframe_recovery_started = Arc::clone(&keyframe_recovery_started);
    let reader_connection = connection.clone();
    let reader_diagnostics = Arc::clone(&diagnostic_events);
    let reader_started = session_started;
    let reader_full_diagnostics = full_diagnostics;
    let digest_task = diagnostic_stream
        .zip(payload_digests.as_ref())
        .map(|(mut stream, queue)| {
            let queue = Arc::clone(queue);
            tokio::spawn(async move {
                let mut preamble = [0; STREAM_PREAMBLE.len()];
                stream
                    .read_exact(&mut preamble)
                    .await
                    .map_err(|error| error.to_string())?;
                if preamble != STREAM_PREAMBLE {
                    return Err("invalid diagnostic stream preamble".to_owned());
                }
                loop {
                    let mut bytes = [0; RECORD_SIZE];
                    match stream.read_exact(&mut bytes).await {
                        Ok(_) => {
                            let record = PayloadDigest::decode(&bytes).map_err(str::to_owned)?;
                            queue.push(PayloadDigestEvent::Host(record));
                        }
                        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(()),
                        Err(error) => return Err(error.to_string()),
                    }
                }
            })
        });
    let task = tokio::spawn(async move {
        let result = async {
            let mut ticks = tokio::time::interval(Duration::from_millis(5));
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_report = Instant::now();
            let mut last_keyframe = Instant::now() - Duration::from_secs(1);
            let input_generation = 1;
            let mut input_sequence = 0u64;
            let mut pointer_sequence = 0u64;
            let mut cumulative_x = 0i64;
            let mut cumulative_y = 0i64;
            let mut pointer_mode = None;
            let mut clock_sequence = 0_u64;
            let mut next_clock_sync = Instant::now();
            let mut keyboard_leds = KeyboardLedReceiver::default();
            let end = 'stream: loop {
                // Keep a partially-read control frame alive across datagram/timer events.
                let mut control = Box::pin(read_envelope(&mut receive));
                loop {
                    tokio::select! {
                        message = &mut control => {
                            match message.map_err(|e| e.to_string())?.body {
                                Some(envelope::Body::AudioStreamState(state)) => receiver.state(state),
                                Some(envelope::Body::InputAck(ack)) => {
                                    if reader_full_diagnostics {
                                    reader_diagnostics.push(DiagnosticEvent::InputAck {
                                        sequence: ack.through_sequence,
                                        player_sent_at_micros: ack.player_sent_at_micros,
                                        player_received_at_micros: elapsed_micros(reader_started),
                                        host_received_at_micros: ack.host_received_at_micros,
                                        host_submitted_at_micros: ack.host_submitted_at_micros,
                                        pointer_datagrams_received: ack.pointer_datagrams_received,
                                        pointer_updates_applied: ack.pointer_updates_applied,
                                        pointer_updates_ignored: ack.pointer_updates_ignored,
                                        mouse_reports_published: ack.mouse_reports_published,
                                        keyboard_reports_published: ack.keyboard_reports_published,
                                        reliable_transitions_received: ack.reliable_transitions_received,
                                        reliable_transitions_applied: ack.reliable_transitions_applied,
                                        reliable_transitions_rejected: ack.reliable_transitions_rejected,
                                        reliable_transitions_missing: ack.reliable_transitions_missing,
                                        reliable_transitions_duplicate_or_late: ack.reliable_transitions_duplicate_or_late,
                                        release_all_transitions: ack.release_all_transitions,
                                        pointer_missing_datagrams: ack.pointer_missing_datagrams,
                                        pointer_stale_generations: ack.pointer_stale_generations,
                                        pointer_duplicate_or_late: ack.pointer_duplicate_or_late,
                                        pointer_mode_rejections: ack.pointer_mode_rejections,
                                        pointer_relative_baselines: ack.pointer_relative_baselines,
                                    });
                                    }
                                }
                                Some(envelope::Body::ClockPong(pong)) => {
                                    if let Some(estimate) = clock_offset(pong, elapsed_micros(reader_started)) {
                                        reader_diagnostics.push(DiagnosticEvent::Clock(estimate));
                                    }
                                }
                                Some(envelope::Body::KeyboardLeds(state)) => {
                                    let state = keyboard_leds.push(state)?;
                                    receiver.snapshot.lock().unwrap_or_else(|e| e.into_inner()).keyboard_leds = Some(state);
                                }
                                Some(envelope::Body::VideoStreamState(state)) => {
                                    let cause = state.reconfiguration_cause().ok_or("invalid video stream state")?;
                                    break 'stream StreamEnd::ReconfigurationRequired(cause);
                                }
                                Some(envelope::Body::HostSessionControl(control)) => {
                                    if !host_pointer_release {
                                        return Err("host sent an unnegotiated session control".to_owned());
                                    }
                                    match HostSessionControlKind::try_from(control.kind) {
                                        Ok(HostSessionControlKind::ReleasePointerCapture) => {
                                            reader_diagnostics.push(DiagnosticEvent::ReleasePointerCapture);
                                        }
                                        _ => return Err("host sent an invalid session control".to_owned()),
                                    }
                                }
                                _ => return Err("unexpected streaming control response".to_owned()),
                            }
                            break;
                        }
                        datagram = reader_connection.read_datagram() => {
                            receiver.datagram(&datagram.map_err(|e| e.to_string())?, Instant::now(), reader_connection.rtt())?;
                        }
                        _ = ticks.tick() => {
                            if reader_stop.load(Ordering::Acquire) { break 'stream StreamEnd::Stopped; }
                            if reader_full_diagnostics && Instant::now() >= next_clock_sync {
                                clock_sequence = clock_sequence.checked_add(1).ok_or("clock sequence exhausted")?;
                                let player_sent_at_micros = elapsed_micros(reader_started);
                                write_envelope(&mut send, Envelope { body: Some(envelope::Body::ClockPing(ClockPing { sequence: clock_sequence, player_sent_at_micros })) }).await.map_err(|e| e.to_string())?;
                                next_clock_sync = Instant::now() + Duration::from_millis(500);
                            }
                            for _ in 0..256 {
                                let Some(TimedInputEvent { event, occurred_at }) = next_input() else { break; };
                                match event {
                                    InputEvent::PointerMotion { delta_x, delta_y } => {
                                        if pointer_mode != Some(PointerMode::Relative) {
                                            input_sequence = input_sequence.checked_add(1).ok_or("input sequence exhausted")?;
                                            let player_sent_at_micros = reader_full_diagnostics.then(|| elapsed_micros(reader_started)).unwrap_or(0);
                                            write_envelope(input_send.as_mut().unwrap_or(&mut send), Envelope { body: Some(envelope::Body::InputTransition(InputTransition { generation: input_generation, sequence: input_sequence, action: Some(input_transition::Action::PointerMode(PointerModeTransition { mode: PointerMode::Relative as i32 })), player_sent_at_micros })) }).await.map_err(|e| e.to_string())?;
                                            pointer_mode = Some(PointerMode::Relative);
                                        }
                                        cumulative_x = cumulative_x.checked_add(i64::from(delta_x)).ok_or("relative pointer x counter overflow")?;
                                        cumulative_y = cumulative_y.checked_add(i64::from(delta_y)).ok_or("relative pointer y counter overflow")?;
                                        pointer_sequence = pointer_sequence.checked_add(1).ok_or("pointer sequence exhausted")?;
                                        let bytes = PointerSnapshot::Relative { generation: input_generation, sequence: pointer_sequence, cumulative_x, cumulative_y }.encode();
                                        reader_connection.send_datagram(bytes.to_vec().into()).map_err(|e| e.to_string())?;
                                    }
                                    InputEvent::PointerPosition { x, y } => {
                                        if pointer_mode != Some(PointerMode::Absolute) {
                                            input_sequence = input_sequence.checked_add(1).ok_or("input sequence exhausted")?;
                                            let player_sent_at_micros = reader_full_diagnostics.then(|| elapsed_micros(reader_started)).unwrap_or(0);
                                            write_envelope(input_send.as_mut().unwrap_or(&mut send), Envelope { body: Some(envelope::Body::InputTransition(InputTransition { generation: input_generation, sequence: input_sequence, action: Some(input_transition::Action::PointerMode(PointerModeTransition { mode: PointerMode::Absolute as i32 })), player_sent_at_micros })) }).await.map_err(|e| e.to_string())?;
                                            pointer_mode = Some(PointerMode::Absolute);
                                        }
                                        pointer_sequence = pointer_sequence.checked_add(1).ok_or("pointer sequence exhausted")?;
                                        let bytes = PointerSnapshot::Absolute { generation: input_generation, sequence: pointer_sequence, x, y }.encode();
                                        reader_connection.send_datagram(bytes.to_vec().into()).map_err(|e| e.to_string())?;
                                    }
                                    event => {
                                        input_sequence = input_sequence.checked_add(1).ok_or("input sequence exhausted")?;
                                        let correlates_test_marker = matches!(event, InputEvent::PointerButton { pressed: true, .. });
                                        let action = match event {
                                            InputEvent::Key { hid_usage, pressed } => input_transition::Action::Key(KeyTransition { hid_usage: u32::from(hid_usage), pressed }),
                                            InputEvent::ReleaseAll => input_transition::Action::ReleaseAll(ReleaseAll {}),
                                            InputEvent::PointerButton { button, pressed } => input_transition::Action::PointerButton(PointerButtonTransition { button: u32::from(button), pressed }),
                                            InputEvent::Wheel { horizontal, vertical } => input_transition::Action::Wheel(WheelTransition { horizontal: i32::from(horizontal), vertical: i32::from(vertical) }),
                                            InputEvent::PointerMotion { .. } | InputEvent::PointerPosition { .. } => unreachable!(),
                                        };
                                        let sent_at = reader_full_diagnostics.then(Instant::now);
                                        let player_sent_at_micros = reader_full_diagnostics.then(|| elapsed_micros(reader_started)).unwrap_or(0);
                                        write_envelope(input_send.as_mut().unwrap_or(&mut send), Envelope { body: Some(envelope::Body::InputTransition(InputTransition { generation: input_generation, sequence: input_sequence, action: Some(action), player_sent_at_micros })) }).await.map_err(|e| e.to_string())?;
                                        if let Some(sent_at) = sent_at {
                                            reader_diagnostics.push(DiagnosticEvent::InputSent { sequence: input_sequence, occurred_at, sent_at, send_completed_at: Instant::now(), correlates_test_marker });
                                        }
                                    }
                                }
                            }
                            receiver.audio.expire(Instant::now());
                            let events = receiver.ordered_audio.expire(Instant::now());
                            receiver.queue_audio(events);
                            receiver.publish(reader_connection.rtt());
                            if (reader_request.load(Ordering::Acquire) || receiver.recover.load(Ordering::Acquire)) && last_keyframe.elapsed() >= Duration::from_secs(1) {
                                write_envelope(&mut send, Envelope { body: Some(envelope::Body::VideoControl(VideoControl { kind: VideoControlKind::RequestKeyframe as i32 })) }).await.map_err(|e| e.to_string())?;
                                reader_keyframe_requests.fetch_add(1, Ordering::Relaxed);
                                reader_keyframe_recovery_started
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .get_or_insert_with(Instant::now);
                                reader_request.store(false, Ordering::Release);
                                last_keyframe = Instant::now();
                            }
                            if last_report.elapsed() >= Duration::from_millis(500) {
                                let stats = receiver.video.stats();
                                let report = VideoReceiverReport {
                                    newest_sequence: receiver.snapshot.lock().unwrap_or_else(|e| e.into_inner()).progress.map_or(0, |p| p.sequence),
                                    received_chunks: stats.received_chunks, lost_chunks: stats.lost_chunks,
                                    late_chunks: stats.late_chunks, assembly_overflows: stats.assembly_overflows,
                                    completed_frames: stats.completed_frames, incomplete_frames: stats.incomplete_frames,
                                    last_completed_assembly_micros: stats.last_completed_assembly_micros,
                                    last_assembly_budget_micros: stats.last_assembly_budget_micros,
                                    completed_payload_bytes: stats.completed_payload_bytes,
                                    measurement_interval_micros: last_report.elapsed().as_micros() as u64,
                                };
                                write_envelope(&mut send, Envelope { body: Some(envelope::Body::VideoReceiverReport(report)) }).await.map_err(|e| e.to_string())?;
                                last_report = Instant::now();
                            }
                        }
                    }
                }
            };
            input_sequence = input_sequence.checked_add(1).ok_or("input sequence exhausted")?;
            write_envelope(input_send.as_mut().unwrap_or(&mut send), Envelope { body: Some(envelope::Body::InputTransition(InputTransition { generation: input_generation, sequence: input_sequence, action: Some(input_transition::Action::ReleaseAll(ReleaseAll {})), player_sent_at_micros: 0 })) }).await.map_err(|e| e.to_string())?;
            write_envelope(&mut send, Envelope { body: Some(envelope::Body::VideoControl(VideoControl { kind: VideoControlKind::Stop as i32 })) }).await.map_err(|e| e.to_string())?;
            send.finish().map_err(|e| e.to_string())?;
            Ok(end)
        }.await;
        if let Err(error) = &result {
            close_with_stream_error(&reader_connection, error);
        }
        receiver.frames.close();
        receiver.audio_events.close();
        result
    });
    let mut guard = ReaderGuard {
        connection,
        stop,
        task: Some(task),
    };
    let mut continuity = DecodeContinuity::default();
    let mut first = true;
    let mut last_progress = Instant::now() - Duration::from_secs(1);
    let mut reported_keyboard_leds = None;
    let mut digest_matcher = PayloadDigestMatcher::default();
    loop {
        if should_stop() {
            break;
        }
        let frame = frames.pop_timeout(Duration::from_millis(2));
        while let Some(event) = audio_events.pop_timeout(Duration::ZERO) {
            if let Err(error) = consume_audio(event) {
                close_with_stream_error(&guard.connection, &error.to_string());
                return Err(error);
            }
        }
        while let Some(event) = diagnostic_events.pop_timeout(Duration::ZERO) {
            match event {
                DiagnosticEvent::ReleasePointerCapture => {
                    progress(StreamProgress::ReleasePointerCapture)
                }
                DiagnosticEvent::Clock(estimate) => progress(StreamProgress::ClockOffset(estimate)),
                DiagnosticEvent::InputAck {
                    sequence,
                    player_sent_at_micros,
                    player_received_at_micros,
                    host_received_at_micros,
                    host_submitted_at_micros,
                    pointer_datagrams_received,
                    pointer_updates_applied,
                    pointer_updates_ignored,
                    mouse_reports_published,
                    keyboard_reports_published,
                    reliable_transitions_received,
                    reliable_transitions_applied,
                    reliable_transitions_rejected,
                    reliable_transitions_missing,
                    reliable_transitions_duplicate_or_late,
                    release_all_transitions,
                    pointer_missing_datagrams,
                    pointer_stale_generations,
                    pointer_duplicate_or_late,
                    pointer_mode_rejections,
                    pointer_relative_baselines,
                } => progress(StreamProgress::InputAcknowledged {
                    sequence,
                    player_sent_at_micros,
                    player_received_at_micros,
                    host_received_at_micros,
                    host_submitted_at_micros,
                    pointer_datagrams_received,
                    pointer_updates_applied,
                    pointer_updates_ignored,
                    mouse_reports_published,
                    keyboard_reports_published,
                    reliable_transitions_received,
                    reliable_transitions_applied,
                    reliable_transitions_rejected,
                    reliable_transitions_missing,
                    reliable_transitions_duplicate_or_late,
                    release_all_transitions,
                    pointer_missing_datagrams,
                    pointer_stale_generations,
                    pointer_duplicate_or_late,
                    pointer_mode_rejections,
                    pointer_relative_baselines,
                }),
                DiagnosticEvent::InputSent {
                    sequence,
                    occurred_at,
                    sent_at,
                    send_completed_at,
                    correlates_test_marker,
                } => progress(StreamProgress::InputSent {
                    sequence,
                    occurred_at,
                    sent_at,
                    send_completed_at,
                    correlates_test_marker,
                }),
            }
        }
        if let Some(payload_digests) = &payload_digests {
            while let Some(event) = payload_digests.pop_timeout(Duration::ZERO) {
                if let Some(sample) = digest_matcher.push(event) {
                    progress(StreamProgress::PayloadIntegrity(sample));
                }
            }
        }
        let snapshot = snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if snapshot.keyboard_leds != reported_keyboard_leds {
            if let Some(state) = snapshot.keyboard_leds {
                progress(StreamProgress::KeyboardLeds {
                    generation: state.generation,
                    sequence: state.sequence,
                    mask: state.mask,
                });
            }
            reported_keyboard_leds = snapshot.keyboard_leds;
        }
        if last_progress.elapsed() >= Duration::from_millis(250) {
            if let Some(frame) = snapshot.progress {
                progress(StreamProgress::ReceivingVideoPackets {
                    frame,
                    totals: snapshot.video,
                });
            }
            progress(StreamProgress::AudioTransport(snapshot.audio.clone()));
            if payload_digests.is_some() {
                progress(StreamProgress::PayloadIntegrityCounters(
                    digest_matcher.counters(),
                ));
                progress(StreamProgress::DiagnosticQueues(
                    crate::DiagnosticQueueSnapshot {
                        video_depth: frames.depth() as u64,
                        video_drops: frames.dropped(),
                        audio_depth: audio_events.depth() as u64,
                        audio_drops: audio_events.dropped(),
                        event_depth: diagnostic_events.depth() as u64,
                        event_drops: diagnostic_events.dropped(),
                        digest_depth: payload_digests
                            .as_ref()
                            .map_or(0, |queue| queue.depth() as u64),
                        digest_drops: payload_digests.as_ref().map_or(0, |queue| queue.dropped()),
                        keyframe_requests: keyframe_requests.load(Ordering::Relaxed),
                    },
                ));
            }
            last_progress = Instant::now();
        }
        if recover.swap(false, Ordering::AcqRel) {
            continuity.ready = false;
            request_keyframe.store(true, Ordering::Release);
        }
        if let Some(AssembledVideoFrame {
            frame,
            payload_sha256,
        }) = frame
        {
            if !continuity.accept(frame.sequence, frame.keyframe) {
                request_keyframe.store(true, Ordering::Release);
                continue;
            }
            if frame.keyframe {
                request_keyframe.store(false, Ordering::Release);
                if let Some(started) = keyframe_recovery_started
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take()
                {
                    progress(StreamProgress::KeyframeRecovered {
                        duration: started.elapsed(),
                    });
                }
            }
            if first {
                progress(StreamProgress::FirstFrameAssembled {
                    target_bitrate_bits_per_second: frame.target_bitrate_bits_per_second,
                    estimated_capacity_bits_per_second: frame.estimated_capacity_bits_per_second,
                });
                first = false;
            }
            let keep_streaming = consume_video(
                frame,
                StreamTransportStatistics {
                    round_trip_time: snapshot.rtt,
                    assembly: snapshot.video,
                    assembled_at: payload_digests.as_ref().map(|_| Instant::now()),
                    assembled_at_micros: elapsed_micros(session_started),
                    assembly_duration: Duration::from_micros(
                        snapshot.video.last_completed_assembly_micros,
                    ),
                    assembled_payload_sha256: payload_sha256,
                },
            );
            let keep_streaming = match keep_streaming {
                Ok(keep_streaming) => keep_streaming,
                Err(error) => {
                    close_with_stream_error(&guard.connection, &error.to_string());
                    return Err(error);
                }
            };
            if !keep_streaming {
                break;
            }
        } else if frames.is_closed() || guard.task.as_ref().unwrap().is_finished() {
            break;
        }
    }
    if let Some(task) = digest_task {
        task.abort();
    }
    guard.stop.store(true, Ordering::Release);
    let result = tokio::time::timeout(Duration::from_secs(3), guard.task.as_mut().unwrap()).await;
    match result {
        Ok(result) => {
            guard.task.take();
            result
                .map_err(|_| "stream receiver panicked")?
                .map_err(Into::into)
        }
        Err(_) => Err("stream receiver shutdown timed out".into()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamEnd {
    Stopped,
    ReconfigurationRequired(VideoReconfigurationCause),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(sequence: u64) -> VideoFramePayload {
        VideoFramePayload {
            sequence,
            captured_at_micros: 1000,
            encoded_at_micros: 1100,
            packetized_at_micros: 1200,
            input_sequence: 0,
            keyframe: sequence == 0,
            target_bitrate_bits_per_second: 1_000_000,
            estimated_capacity_bits_per_second: 2_000_000,
            payload: vec![42; 100],
        }
    }

    fn audio(generation: u64, sequence: u64) -> AudioPacket {
        AudioPacket {
            generation,
            sequence,
            captured_at_micros: 1_000 + sequence * 10_000,
            decoded_samples: 480,
            skip_start_samples: 0,
            skip_end_samples: 0,
            payload: vec![42],
        }
    }

    #[test]
    fn payload_digest_matcher_reports_identity_and_mismatch() {
        let local = local_payload_digest(MediaKind::Audio, 2, 3, b"payload", 20);
        let mut host = PayloadDigest {
            kind: MediaKind::Audio,
            generation: 2,
            sequence: 3,
            payload_size: local.payload_size,
            hashed_at_micros: 10,
            producer_hash_duration_micros: 10,
            boundary_hash_duration_micros: 11,
            producer_dropped_records: 12,
            boundary_matched: true,
            sha256: local.sha256,
            encode_started_at_micros: 1,
            encoded_at_micros: 2,
            worker_queued_at_micros: 3,
            service_received_at_micros: 4,
            packetized_at_micros: 5,
            captured_at_micros: 0,
            mirror_decode_micros: 6,
            quality_present: false,
            quality_presentation_timestamp: 0,
            source_readback_micros: 0,
            decoded_readback_micros: 0,
            scoring_micros: 0,
            readback_bytes: 0,
            luma_psnr_millidecibels: 0,
            luma_mean_absolute_error_ppm: 0,
            packetization_completed_at_micros: 0,
            first_send_attempt_at_micros: 0,
            last_send_completed_at_micros: 0,
            capture_acquisition_micros: 0,
            cross_adapter_copy_micros: 0,
            color_conversion_micros: 0,
            encoder_call_micros: 0,
            audio_capture_buffer_frames: 0,
            audio_capture_discontinuities: 0,
            audio_invalid_capture_timestamps: 0,
            audio_device_reopens: 0,
            audio_encoder_resets: 0,
            audio_capture_queue_depth: 0,
            audio_capture_queue_capacity: 0,
            audio_capture_queue_drops: 0,
        };
        let mut matcher = PayloadDigestMatcher::default();
        assert!(matcher.push(PayloadDigestEvent::Host(host)).is_none());
        let matched = matcher
            .push(PayloadDigestEvent::Local(local))
            .expect("matching sides should produce a sample");
        assert!(matched.matched);
        assert_eq!(matched.host_dropped_records, 12);
        host.sequence = 4;
        matcher.push(PayloadDigestEvent::Host(host));
        let mut changed = local;
        changed.sequence = 4;
        changed.sha256[0] ^= 1;
        assert!(
            !matcher
                .push(PayloadDigestEvent::Local(changed))
                .unwrap()
                .matched
        );
        assert_eq!(matcher.counters().pending_host_records, 0);
    }

    #[test]
    fn audio_playout_reorders_four_packets_before_release() {
        let now = Instant::now();
        let mut ordered = OrderedAudioPackets::default();
        assert!(ordered.push(audio(1, 2), now, 2, None).is_empty());
        assert!(ordered.push(audio(1, 0), now, 0, None).is_empty());
        assert!(ordered.push(audio(1, 3), now, 3, None).is_empty());
        let ready = ordered.push(audio(1, 1), now, 1, None);
        let sequences = ready
            .into_iter()
            .map(|event| match event {
                AudioPlaybackEvent::Packet { packet, .. } => packet.sequence,
                AudioPlaybackEvent::Missing { .. } => panic!("unexpected missing packet"),
            })
            .collect::<Vec<_>>();
        assert_eq!(sequences, [0, 1, 2, 3]);
    }

    #[test]
    fn audio_playout_reports_a_confirmed_gap_once() {
        let now = Instant::now();
        let mut ordered = OrderedAudioPackets::default();
        for sequence in [0, 2, 3, 4] {
            ordered.push(audio(1, sequence), now, sequence, None);
        }
        let ready = ordered.push(audio(1, 5), now, 5, None);
        assert!(matches!(
            ready.first(),
            Some(AudioPlaybackEvent::Missing {
                generation: 1,
                sequence: 1,
                captured_at_micros: 11_000,
                missing_packets: 1,
            })
        ));
        assert_eq!(ready.len(), 5);
    }

    #[test]
    fn audio_playout_generation_discards_old_pending_packets() {
        let now = Instant::now();
        let mut ordered = OrderedAudioPackets::default();
        ordered.push(audio(1, 8), now, 8, None);
        for sequence in 0..3 {
            assert!(
                ordered
                    .push(audio(2, sequence), now, sequence, None)
                    .is_empty()
            );
        }
        let ready = ordered.push(audio(2, 3), now, 3, None);
        assert!(ready.into_iter().all(|event| matches!(
            event,
            AudioPlaybackEvent::Packet {
                packet: AudioPacket { generation: 2, .. },
                ..
            }
        )));
    }

    #[test]
    fn stalled_video_consumer_does_not_block_audio_or_audio_failure_state() {
        let frames = Arc::new(MediaQueue::new(2));
        let snapshot = Arc::new(Mutex::new(Snapshot {
            audio: AudioTransportSnapshot {
                receive: AudioReceiveStatistics::default(),
                host: AudioStreamState::default(),
                video_queue_drops: 0,
                audio_queue_drops: 0,
            },
            video: VideoAssemblyStats::default(),
            progress: None,
            rtt: Duration::ZERO,
            keyboard_leds: None,
        }));
        let mut receiver = Receiver {
            started: Instant::now(),
            video: VideoFrameAssembler::new(120),
            audio: AudioAssembler::default(),
            audio_enabled: true,
            ordered_audio: OrderedAudioPackets::default(),
            audio_events: Arc::new(MediaQueue::new(AUDIO_QUEUE_PACKETS)),
            frames: Arc::clone(&frames),
            payload_digests: None,
            snapshot: Arc::clone(&snapshot),
            recover: Arc::new(AtomicBool::new(false)),
        };
        let now = Instant::now();
        for sequence in 0..3 {
            for data in
                rustconsole_session::video_datagram::packetize_video_frame(&video(sequence), 1200)
                    .unwrap()
            {
                receiver.datagram(&data, now, Duration::ZERO).unwrap();
            }
        }
        let packet = rustconsole_protocol::audio::AudioPacket {
            generation: 1,
            sequence: 0,
            captured_at_micros: 1000,
            decoded_samples: 480,
            skip_start_samples: 312,
            skip_end_samples: 0,
            payload: vec![9; 100],
        };
        for data in rustconsole_session::audio_datagram::packetize(&packet, 80)
            .unwrap()
            .into_iter()
            .rev()
        {
            receiver.datagram(&data, now, Duration::ZERO).unwrap();
        }
        receiver
            .datagram(b"RA broken", now, Duration::ZERO)
            .unwrap();
        receiver.state(AudioStreamState::new(
            1,
            AudioStatus::Failed,
            0,
            "test failure".into(),
        ));
        let stats = snapshot.lock().unwrap().clone();
        assert_eq!(stats.audio.receive.completed_packets, 1);
        assert_eq!(stats.audio.receive.malformed_fragments, 1);
        assert_eq!(stats.audio.host.status, AudioStatus::Failed as i32);
        assert_eq!(stats.audio.video_queue_drops, 1);
        assert_eq!(
            frames.pop_timeout(Duration::ZERO).unwrap().frame.sequence,
            1
        );
        assert_eq!(
            frames.pop_timeout(Duration::ZERO).unwrap().frame.sequence,
            2
        );
    }

    #[test]
    fn keyboard_led_receiver_requires_valid_monotonic_states() {
        let mut receiver = KeyboardLedReceiver::default();
        let state = |generation, sequence, mask| KeyboardLeds {
            generation,
            sequence,
            mask,
        };
        assert_eq!(
            receiver.push(state(9, 4, 0x07)).unwrap(),
            KeyboardLedSnapshot {
                generation: 9,
                sequence: 4,
                mask: 0x07,
            }
        );
        assert!(receiver.push(state(9, 6, 0)).is_err());
        assert!(receiver.push(state(8, 5, 0)).is_err());
        assert!(receiver.push(state(9, 5, 0x20)).is_err());
        assert!(receiver.push(state(10, 1, 0x10)).is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mixed_loopback_keeps_partial_reads_and_returns_reconfiguration() {
        use rustconsole_session::quic::{ephemeral_server_config, opaque_client_config};
        let server = quinn::Endpoint::server(
            ephemeral_server_config().unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        client.set_default_client_config(opaque_client_config().unwrap());
        let connecting = client
            .connect(server.local_addr().unwrap(), "rustconsole.invalid")
            .unwrap();
        let (client_connection, server_connection) =
            tokio::join!(async { connecting.await.unwrap() }, async {
                server.accept().await.unwrap().await.unwrap()
            },);
        let (mut client_send, client_receive) = client_connection.open_bi().await.unwrap();
        client_send.write_all(&[0]).await.unwrap();
        let mut client_input = client_connection.open_uni().await.unwrap();
        client_input
            .write_all(&rustconsole_protocol::input::STREAM_PREAMBLE)
            .await
            .unwrap();
        let (mut server_send, mut server_receive) = server_connection.accept_bi().await.unwrap();
        server_receive.read_exact(&mut [0]).await.unwrap();
        let sender = tokio::spawn(async move {
            let mut server_input = server_connection.accept_uni().await.unwrap();
            let mut input_preamble = [0; rustconsole_protocol::input::STREAM_PREAMBLE.len()];
            server_input.read_exact(&mut input_preamble).await.unwrap();
            assert_eq!(input_preamble, rustconsole_protocol::input::STREAM_PREAMBLE);
            let input = read_envelope(&mut server_input).await.unwrap();
            assert!(matches!(
                input.body,
                Some(envelope::Body::InputTransition(InputTransition {
                    action: Some(input_transition::Action::Key(KeyTransition {
                        hid_usage: 4,
                        pressed: false,
                    })),
                    ..
                }))
            ));
            let state =
                AudioStreamState::new(1, AudioStatus::Failed, 0, "audio-only test failure".into());
            let bytes = rustconsole_protocol::wire::encode_reliable_frame(&Envelope {
                body: Some(envelope::Body::AudioStreamState(state)),
            })
            .unwrap();
            server_send.write_all(&bytes[..2]).await.unwrap();
            let packet = rustconsole_protocol::audio::AudioPacket {
                generation: 1,
                sequence: 0,
                captured_at_micros: 1000,
                decoded_samples: 480,
                skip_start_samples: 312,
                skip_end_samples: 0,
                payload: vec![9; 100],
            };
            for data in rustconsole_session::audio_datagram::packetize(&packet, 80).unwrap() {
                server_connection
                    .send_datagram_wait(data.into())
                    .await
                    .unwrap();
            }
            server_connection
                .send_datagram_wait(b"RA invalid".to_vec().into())
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            server_send.write_all(&bytes[2..]).await.unwrap();
            write_envelope(
                &mut server_send,
                Envelope {
                    body: Some(envelope::Body::HostSessionControl(
                        rustconsole_protocol::wire::HostSessionControl {
                            kind: HostSessionControlKind::ReleasePointerCapture as i32,
                        },
                    )),
                },
            )
            .await
            .unwrap();
            for sequence in 0..2 {
                for data in rustconsole_session::video_datagram::packetize_video_frame(
                    &video(sequence),
                    1200,
                )
                .unwrap()
                {
                    server_connection
                        .send_datagram_wait(data.into())
                        .await
                        .unwrap();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            write_envelope(
                &mut server_send,
                Envelope {
                    body: Some(envelope::Body::VideoStreamState(
                        rustconsole_protocol::wire::VideoStreamState::reconfiguration_required(
                            VideoReconfigurationCause::Color,
                        ),
                    )),
                },
            )
            .await
            .unwrap();
            server_connection.closed().await;
        });
        let started = Instant::now();
        let mut count = 0;
        let mut audio_count = 0;
        let mut latest = None;
        let mut release_pointer_capture = false;
        let mut input = Some(TimedInputEvent {
            event: InputEvent::Key {
                hid_usage: 4,
                pressed: false,
            },
            occurred_at: Instant::now(),
        });
        let end = receive_stream(ReceiveStreamParameters {
            connection: client_connection,
            control: (client_send, client_receive),
            input_stream: Some(client_input),
            fps: 120,
            audio_enabled: true,
            host_pointer_release: true,
            diagnostic_stream: None,
            should_stop: || started.elapsed() >= Duration::from_millis(550),
            next_input: move || input.take(),
            progress: |event| match event {
                StreamProgress::AudioTransport(state) => latest = Some(state),
                StreamProgress::ReleasePointerCapture => release_pointer_capture = true,
                _ => {}
            },
            consumers: StreamConsumers {
                audio: |_| {
                    audio_count += 1;
                    Ok(())
                },
                video: |_, _| {
                    count += 1;
                    Ok(true)
                },
            },
        })
        .await
        .unwrap();
        sender.await.unwrap();
        let latest = latest.unwrap();
        assert_eq!(count, 2);
        assert_eq!(audio_count, 1);
        assert_eq!(latest.receive.completed_packets, 1);
        assert_eq!(latest.receive.malformed_fragments, 1);
        assert_eq!(latest.host.status, AudioStatus::Failed as i32);
        assert!(release_pointer_capture);
        assert_eq!(
            end,
            StreamEnd::ReconfigurationRequired(VideoReconfigurationCause::Color)
        );
    }
    #[test]
    fn dropped_video_requires_a_keyframe() {
        let mut state = DecodeContinuity::default();
        assert!(!state.accept(1, false));
        assert!(state.accept(2, true));
        assert!(state.accept(3, false));
        assert!(!state.accept(5, false));
        assert!(!state.accept(6, false));
        assert!(state.accept(7, true));
    }
}
