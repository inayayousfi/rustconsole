use crate::{StreamConsumers, StreamProgress, StreamTransportStatistics};
use rustconsole_protocol::InputEvent;
use rustconsole_protocol::audio::AudioPacket;
use rustconsole_protocol::wire::{
    AudioStatus, AudioStreamState, Envelope, InputTransition, KeyTransition, KeyboardLeds,
    PointerButtonTransition, PointerMode, PointerModeTransition, ReleaseAll, VideoControl,
    VideoControlKind, VideoReceiverReport, VideoReconfigurationCause, WheelTransition, envelope,
    input_transition,
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
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

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
    Packet(AudioPacket),
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
    pending: BTreeMap<u64, (AudioPacket, Instant)>,
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

    fn push(&mut self, packet: AudioPacket, now: Instant) -> Vec<AudioPlaybackEvent> {
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
        self.pending.entry(packet.sequence).or_insert((packet, now));
        self.drain(now)
    }

    fn expire(&mut self, now: Instant) -> Vec<AudioPlaybackEvent> {
        self.drain(now)
    }

    fn drain(&mut self, now: Instant) -> Vec<AudioPlaybackEvent> {
        if !self.started {
            let old_enough = self
                .pending
                .first_key_value()
                .is_some_and(|(_, (_, received))| now.duration_since(*received) >= AUDIO_WAIT);
            if self.pending.len() < AUDIO_QUEUE_PACKETS && !old_enough {
                return Vec::new();
            }
            self.started = true;
        }

        let mut ready = Vec::new();
        while let Some(expected) = self.expected {
            if let Some((packet, _)) = self.pending.remove(&expected) {
                self.last_timestamp = Some(packet.captured_at_micros);
                self.expected = expected.checked_add(1);
                ready.push(AudioPlaybackEvent::Packet(packet));
                continue;
            }
            let Some((&next_sequence, (next, received))) = self.pending.first_key_value() else {
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
    video: VideoFrameAssembler,
    audio: AudioAssembler,
    audio_enabled: bool,
    ordered_audio: OrderedAudioPackets,
    audio_events: Arc<MediaQueue<AudioPlaybackEvent>>,
    frames: Arc<MediaQueue<VideoFramePayload>>,
    snapshot: Arc<Mutex<Snapshot>>,
    recover: Arc<AtomicBool>,
}

impl Receiver {
    fn datagram(&mut self, bytes: &[u8], now: Instant, rtt: Duration) -> Result<(), String> {
        if bytes.starts_with(&rustconsole_protocol::audio::MAGIC) {
            if self.audio_enabled
                && let Some(packet) = self.audio.push(bytes, now)
            {
                let events = self.ordered_audio.push(packet, now);
                self.queue_audio(events);
            }
        } else {
            let assembled = self
                .video
                .push(bytes, now, rtt)
                .map_err(|e| e.to_string())?;
            if assembled.dependency_lost {
                self.recover.store(true, Ordering::Release);
            }
            let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
            snapshot.progress = assembled.progress;
            if let Some(frame) = assembled.frame
                && self.frames.push(frame).is_some()
            {
                snapshot.audio.video_queue_drops += 1;
                self.recover.store(true, Ordering::Release);
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

pub(super) struct ReceiveStreamParameters<Stop, Input, Progress, Audio, Video> {
    pub(super) connection: quinn::Connection,
    pub(super) control: (quinn::SendStream, quinn::RecvStream),
    pub(super) fps: u16,
    pub(super) audio_enabled: bool,
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
    Input: FnMut() -> Option<InputEvent> + Send + 'static,
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
        fps,
        audio_enabled,
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
    let frames = Arc::new(MediaQueue::new(2));
    let audio_events = Arc::new(MediaQueue::new(AUDIO_QUEUE_PACKETS + 1));
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
    let stop = Arc::new(AtomicBool::new(false));
    let mut receiver = Receiver {
        video: VideoFrameAssembler::new(fps),
        audio: AudioAssembler::default(),
        audio_enabled,
        ordered_audio: OrderedAudioPackets::default(),
        audio_events: Arc::clone(&audio_events),
        frames: Arc::clone(&frames),
        snapshot: Arc::clone(&snapshot),
        recover: Arc::clone(&recover),
    };
    let reader_stop = Arc::clone(&stop);
    let reader_request = Arc::clone(&request_keyframe);
    let reader_connection = connection.clone();
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
            let mut keyboard_leds = KeyboardLedReceiver::default();
            let end = 'stream: loop {
                // Keep a partially-read control frame alive across datagram/timer events.
                let mut control = Box::pin(read_envelope(&mut receive));
                loop {
                    tokio::select! {
                        message = &mut control => {
                            match message.map_err(|e| e.to_string())?.body {
                                Some(envelope::Body::AudioStreamState(state)) => receiver.state(state),
                                Some(envelope::Body::InputAck(_)) => {}
                                Some(envelope::Body::KeyboardLeds(state)) => {
                                    let state = keyboard_leds.push(state)?;
                                    receiver.snapshot.lock().unwrap_or_else(|e| e.into_inner()).keyboard_leds = Some(state);
                                }
                                Some(envelope::Body::VideoStreamState(state)) => {
                                    let cause = state.reconfiguration_cause().ok_or("invalid video stream state")?;
                                    break 'stream StreamEnd::ReconfigurationRequired(cause);
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
                            for _ in 0..256 {
                                let Some(event) = next_input() else { break; };
                                match event {
                                    InputEvent::PointerMotion { delta_x, delta_y } => {
                                        if pointer_mode != Some(PointerMode::Relative) {
                                            input_sequence = input_sequence.checked_add(1).ok_or("input sequence exhausted")?;
                                            write_envelope(&mut send, Envelope { body: Some(envelope::Body::InputTransition(InputTransition { generation: input_generation, sequence: input_sequence, action: Some(input_transition::Action::PointerMode(PointerModeTransition { mode: PointerMode::Relative as i32 })) })) }).await.map_err(|e| e.to_string())?;
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
                                            write_envelope(&mut send, Envelope { body: Some(envelope::Body::InputTransition(InputTransition { generation: input_generation, sequence: input_sequence, action: Some(input_transition::Action::PointerMode(PointerModeTransition { mode: PointerMode::Absolute as i32 })) })) }).await.map_err(|e| e.to_string())?;
                                            pointer_mode = Some(PointerMode::Absolute);
                                        }
                                        pointer_sequence = pointer_sequence.checked_add(1).ok_or("pointer sequence exhausted")?;
                                        let bytes = PointerSnapshot::Absolute { generation: input_generation, sequence: pointer_sequence, x, y }.encode();
                                        reader_connection.send_datagram(bytes.to_vec().into()).map_err(|e| e.to_string())?;
                                    }
                                    event => {
                                        input_sequence = input_sequence.checked_add(1).ok_or("input sequence exhausted")?;
                                        let action = match event {
                                            InputEvent::Key { hid_usage, pressed } => input_transition::Action::Key(KeyTransition { hid_usage: u32::from(hid_usage), pressed }),
                                            InputEvent::PointerButton { button, pressed } => input_transition::Action::PointerButton(PointerButtonTransition { button: u32::from(button), pressed }),
                                            InputEvent::Wheel { horizontal, vertical } => input_transition::Action::Wheel(WheelTransition { horizontal: i32::from(horizontal), vertical: i32::from(vertical) }),
                                            InputEvent::PointerMotion { .. } | InputEvent::PointerPosition { .. } => unreachable!(),
                                        };
                                        write_envelope(&mut send, Envelope { body: Some(envelope::Body::InputTransition(InputTransition { generation: input_generation, sequence: input_sequence, action: Some(action) })) }).await.map_err(|e| e.to_string())?;
                                    }
                                }
                            }
                            receiver.audio.expire(Instant::now());
                            let events = receiver.ordered_audio.expire(Instant::now());
                            receiver.queue_audio(events);
                            receiver.publish(reader_connection.rtt());
                            if (reader_request.load(Ordering::Acquire) || receiver.recover.load(Ordering::Acquire)) && last_keyframe.elapsed() >= Duration::from_secs(1) {
                                write_envelope(&mut send, Envelope { body: Some(envelope::Body::VideoControl(VideoControl { kind: VideoControlKind::RequestKeyframe as i32 })) }).await.map_err(|e| e.to_string())?;
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
            write_envelope(&mut send, Envelope { body: Some(envelope::Body::InputTransition(InputTransition { generation: input_generation, sequence: input_sequence, action: Some(input_transition::Action::ReleaseAll(ReleaseAll {})) })) }).await.map_err(|e| e.to_string())?;
            write_envelope(&mut send, Envelope { body: Some(envelope::Body::VideoControl(VideoControl { kind: VideoControlKind::Stop as i32 })) }).await.map_err(|e| e.to_string())?;
            send.finish().map_err(|e| e.to_string())?;
            Ok(end)
        }.await;
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
    loop {
        if should_stop() {
            break;
        }
        let frame = frames.pop_timeout(Duration::from_millis(2));
        while let Some(event) = audio_events.pop_timeout(Duration::ZERO) {
            consume_audio(event)?;
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
            last_progress = Instant::now();
        }
        if recover.swap(false, Ordering::AcqRel) {
            continuity.ready = false;
            request_keyframe.store(true, Ordering::Release);
        }
        if let Some(frame) = frame {
            if !continuity.accept(frame.sequence, frame.keyframe) {
                request_keyframe.store(true, Ordering::Release);
                continue;
            }
            if frame.keyframe {
                request_keyframe.store(false, Ordering::Release);
            }
            if first {
                progress(StreamProgress::FirstFrameAssembled {
                    target_bitrate_bits_per_second: frame.target_bitrate_bits_per_second,
                    estimated_capacity_bits_per_second: frame.estimated_capacity_bits_per_second,
                });
                first = false;
            }
            if !consume_video(
                frame,
                StreamTransportStatistics {
                    round_trip_time: snapshot.rtt,
                    assembly: snapshot.video,
                },
            )? {
                break;
            }
        } else if frames.is_closed() || guard.task.as_ref().unwrap().is_finished() {
            break;
        }
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
    fn audio_playout_reorders_four_packets_before_release() {
        let now = Instant::now();
        let mut ordered = OrderedAudioPackets::default();
        assert!(ordered.push(audio(1, 2), now).is_empty());
        assert!(ordered.push(audio(1, 0), now).is_empty());
        assert!(ordered.push(audio(1, 3), now).is_empty());
        let ready = ordered.push(audio(1, 1), now);
        let sequences = ready
            .into_iter()
            .map(|event| match event {
                AudioPlaybackEvent::Packet(packet) => packet.sequence,
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
            ordered.push(audio(1, sequence), now);
        }
        let ready = ordered.push(audio(1, 5), now);
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
        ordered.push(audio(1, 8), now);
        for sequence in 0..3 {
            assert!(ordered.push(audio(2, sequence), now).is_empty());
        }
        let ready = ordered.push(audio(2, 3), now);
        assert!(ready.into_iter().all(|event| matches!(
            event,
            AudioPlaybackEvent::Packet(AudioPacket { generation: 2, .. })
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
            video: VideoFrameAssembler::new(120),
            audio: AudioAssembler::default(),
            audio_enabled: true,
            ordered_audio: OrderedAudioPackets::default(),
            audio_events: Arc::new(MediaQueue::new(AUDIO_QUEUE_PACKETS)),
            frames: Arc::clone(&frames),
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
        assert_eq!(frames.pop_timeout(Duration::ZERO).unwrap().sequence, 1);
        assert_eq!(frames.pop_timeout(Duration::ZERO).unwrap().sequence, 2);
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
        let (mut server_send, mut server_receive) = server_connection.accept_bi().await.unwrap();
        server_receive.read_exact(&mut [0]).await.unwrap();
        let sender = tokio::spawn(async move {
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
        let end = receive_stream(ReceiveStreamParameters {
            connection: client_connection,
            control: (client_send, client_receive),
            fps: 120,
            audio_enabled: true,
            should_stop: || started.elapsed() >= Duration::from_millis(550),
            next_input: || None,
            progress: |event| {
                if let StreamProgress::AudioTransport(state) = event {
                    latest = Some(state);
                }
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
