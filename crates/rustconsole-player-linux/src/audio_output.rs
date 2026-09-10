use crate::{DecodedAudioEvent, DecodedAudioSamples};
use rustconsole_media::AudioSamples;
use sdl3::audio::{AudioFormat, AudioSpec, AudioStreamOwner};
use std::collections::VecDeque;
use std::mem::size_of;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PACKET_FRAMES: usize = 480;
const CHANNELS: usize = 2;
const PREBUFFER_PACKETS: usize = 4;
const MAX_PACKETS: usize = 8;
const MAX_SYNC_MICROS: u64 = 40_000;
const PREBUFFER_BYTES: usize = PACKET_FRAMES * CHANNELS * PREBUFFER_PACKETS * size_of::<f32>();
const MAX_SDL_BYTES: usize = PACKET_FRAMES * CHANNELS * MAX_PACKETS * size_of::<f32>();
const DEVICE_RETRY: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AudioPlaybackSnapshot {
    pub pending_packets: usize,
    pub queued_micros: u32,
    pub queue_drops: u64,
    pub late_drops: u64,
    pub device_drops: u64,
    pub invalid_format_drops: u64,
    pub unavailable_device_drops: u64,
    pub device_query_failures: u64,
    pub software_capacity_drops: u64,
    pub device_submission_failures: u64,
    pub underruns: u64,
    pub resets: u64,
    pub decoder_failures: u64,
    pub device_name: String,
    pub detail: String,
}

#[derive(Default)]
struct QueueState {
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

#[derive(Clone, Default)]
pub struct AudioPlaybackQueue {
    state: Arc<Mutex<QueueState>>,
}

impl AudioPlaybackQueue {
    pub fn push(&self, event: DecodedAudioEvent) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match event {
            DecodedAudioEvent::Reset { generation } => {
                state.pending.clear();
                state.reset = Some(generation);
                state.resets += 1;
                state.detail.clear();
            }
            DecodedAudioEvent::Samples(samples) => {
                if state.pending.len() == MAX_PACKETS {
                    state.pending.pop_front();
                    state.queue_drops += 1;
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
        if audio_timestamp > video_timestamp_micros.saturating_add(MAX_SYNC_MICROS) {
            if let Some(now) = now {
                pending.sync_hold_started.get_or_insert(now);
            }
            return None;
        }
        let pending = state.pending.pop_front().unwrap();
        let queue_duration = now
            .zip(pending.queued_at)
            .map(|(now, queued_at)| now.saturating_duration_since(queued_at));
        if audio_timestamp.saturating_add(MAX_SYNC_MICROS) < video_timestamp_micros {
            state.late_drops += 1;
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

    pub fn snapshot(&self) -> AudioPlaybackSnapshot {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        AudioPlaybackSnapshot {
            pending_packets: state.pending.len(),
            queue_drops: state.queue_drops,
            late_drops: state.late_drops,
            resets: state.resets,
            decoder_failures: state.decoder_failures,
            detail: state.detail.clone(),
            ..AudioPlaybackSnapshot::default()
        }
    }
}

pub struct SdlAudioOutput {
    sdl: sdl3::Sdl,
    audio: Option<sdl3::AudioSubsystem>,
    stream: Option<AudioStreamOwner>,
    retry_at: Instant,
    started: bool,
    empty: bool,
    queued_micros: u32,
    device_drops: u64,
    invalid_format_drops: u64,
    unavailable_device_drops: u64,
    device_query_failures: u64,
    software_capacity_drops: u64,
    device_submission_failures: u64,
    underruns: u64,
    device_name: String,
    detail: String,
}

impl SdlAudioOutput {
    pub fn new(sdl: &sdl3::Sdl) -> Self {
        Self {
            sdl: sdl.clone(),
            audio: None,
            stream: None,
            retry_at: Instant::now(),
            started: false,
            empty: true,
            queued_micros: 0,
            device_drops: 0,
            invalid_format_drops: 0,
            unavailable_device_drops: 0,
            device_query_failures: 0,
            software_capacity_drops: 0,
            device_submission_failures: 0,
            underruns: 0,
            device_name: String::new(),
            detail: String::new(),
        }
    }

    pub fn reset(&mut self) {
        if let Some(stream) = &self.stream
            && let Err(error) = stream.clear()
        {
            self.fail(error.to_string());
        }
        if let Some(stream) = &self.stream
            && let Err(error) = stream.pause()
        {
            self.fail(error.to_string());
        }
        self.started = false;
        self.empty = true;
        self.queued_micros = 0;
    }

    pub fn play(&mut self, samples: &AudioSamples) {
        if samples.format.sample_rate != 48_000
            || samples.format.channels != 2
            || samples.interleaved.is_empty()
            || !samples.interleaved.len().is_multiple_of(CHANNELS)
        {
            self.device_drops += 1;
            self.invalid_format_drops += 1;
            self.detail = "decoded audio has an invalid format".into();
            return;
        }
        self.poll();
        let Some(stream) = &self.stream else {
            self.device_drops += 1;
            self.unavailable_device_drops += 1;
            return;
        };
        let bytes = samples.interleaved.len() * size_of::<f32>();
        let queued = match stream.queued_bytes() {
            Ok(queued) if queued >= 0 => queued as usize,
            Ok(_) => {
                self.fail("SDL reported a negative audio queue size".into());
                self.device_drops += 1;
                self.device_query_failures += 1;
                return;
            }
            Err(error) => {
                self.fail(error.to_string());
                self.device_drops += 1;
                self.device_query_failures += 1;
                return;
            }
        };
        if queued.saturating_add(bytes) > MAX_SDL_BYTES {
            self.device_drops += 1;
            self.software_capacity_drops += 1;
            return;
        }
        if let Err(error) = stream.put_data_f32(&samples.interleaved) {
            self.fail(error.to_string());
            self.device_drops += 1;
            self.device_submission_failures += 1;
            return;
        }
        let queued = queued + bytes;
        self.queued_micros = bytes_to_micros(queued);
        if !self.started && queued >= PREBUFFER_BYTES {
            if let Err(error) = stream.resume() {
                self.fail(error.to_string());
                self.device_drops += 1;
                self.device_submission_failures += 1;
                return;
            }
            self.started = true;
        }
        if self.started {
            self.empty = false;
        }
    }

    pub fn poll(&mut self) {
        if self.stream.is_none() {
            if Instant::now() >= self.retry_at {
                self.open();
            }
            return;
        }
        let queued = match self.stream.as_ref().unwrap().queued_bytes() {
            Ok(queued) if queued >= 0 => queued as usize,
            Ok(_) => {
                self.fail("SDL reported a negative audio queue size".into());
                return;
            }
            Err(error) => {
                self.fail(error.to_string());
                return;
            }
        };
        self.queued_micros = bytes_to_micros(queued);
        if self.started && queued == 0 && !self.empty {
            self.underruns += 1;
            self.empty = true;
            if let Err(error) = self.stream.as_ref().unwrap().pause() {
                self.fail(error.to_string());
                return;
            }
            self.started = false;
        }
    }

    pub fn snapshot(&self, queue: &AudioPlaybackQueue) -> AudioPlaybackSnapshot {
        let mut snapshot = queue.snapshot();
        snapshot.queued_micros = self.queued_micros;
        snapshot.device_drops = self.device_drops;
        snapshot.invalid_format_drops = self.invalid_format_drops;
        snapshot.unavailable_device_drops = self.unavailable_device_drops;
        snapshot.device_query_failures = self.device_query_failures;
        snapshot.software_capacity_drops = self.software_capacity_drops;
        snapshot.device_submission_failures = self.device_submission_failures;
        snapshot.underruns = self.underruns;
        snapshot.device_name.clone_from(&self.device_name);
        if !self.detail.is_empty() {
            snapshot.detail.clone_from(&self.detail);
        }
        snapshot
    }

    pub const fn queued_micros(&self) -> u32 {
        self.queued_micros
    }

    fn open(&mut self) {
        if self.audio.is_none() {
            match self.sdl.audio() {
                Ok(audio) => self.audio = Some(audio),
                Err(error) => {
                    self.fail(error.to_string());
                    return;
                }
            }
        }
        let spec = AudioSpec {
            freq: Some(48_000),
            channels: Some(2),
            format: Some(AudioFormat::f32_sys()),
        };
        let result = self
            .audio
            .as_ref()
            .unwrap()
            .open_playback_device(&spec)
            .and_then(|device| device.open_device_stream(Some(&spec)));
        match result {
            Ok(stream) => {
                self.device_name = stream
                    .device_name()
                    .unwrap_or_else(|| "default SDL output".into());
                self.stream = Some(stream);
                self.detail.clear();
                self.started = false;
                self.empty = true;
            }
            Err(error) => self.fail(error.to_string()),
        }
    }

    fn fail(&mut self, detail: String) {
        self.stream = None;
        self.queued_micros = 0;
        self.device_name.clear();
        self.detail = detail;
        self.retry_at = Instant::now() + DEVICE_RETRY;
        self.started = false;
        self.empty = true;
    }
}

fn bytes_to_micros(bytes: usize) -> u32 {
    ((bytes / (CHANNELS * size_of::<f32>())) as u64 * 1_000_000 / 48_000) as u32
}

pub fn run_sdl_audio_proof(
    sdl: &sdl3::Sdl,
    report_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let queue = AudioPlaybackQueue::default();
    let mut output = SdlAudioOutput::new(sdl);
    for packet in 0..PREBUFFER_PACKETS - 1 {
        play_proof_packet(&mut output, packet);
    }
    let prebuffering = output.snapshot(&queue);
    if output.started || prebuffering.queued_micros != 30_000 {
        return Err(format!(
            "SDL audio output started before its reserve was full: {prebuffering:?}"
        )
        .into());
    }
    play_proof_packet(&mut output, PREBUFFER_PACKETS - 1);
    let started = output.snapshot(&queue);
    if !output.started || started.queued_micros != 40_000 {
        return Err(
            format!("SDL audio output did not start with a full reserve: {started:?}").into(),
        );
    }
    std::thread::sleep(Duration::from_millis(80));
    output.poll();
    let drained = output.snapshot(&queue);
    if output.started || drained.queued_micros != 0 || drained.underruns != 1 {
        return Err(format!(
            "SDL audio output did not enter rebuffering after draining: {drained:?}"
        )
        .into());
    }
    for packet in PREBUFFER_PACKETS..PREBUFFER_PACKETS * 2 {
        play_proof_packet(&mut output, packet);
    }
    let rebuffered = output.snapshot(&queue);
    if !output.started || rebuffered.queued_micros != 40_000 {
        return Err(
            format!("SDL audio output did not resume with a full reserve: {rebuffered:?}").into(),
        );
    }
    std::fs::write(
        report_path,
        format!(
            "status=ok\nformat=f32-stereo-48000\ndevice={}\nprebuffered_micros={}\nstarted_micros={}\nforced_underruns={}\nrebuffered_micros={}\nmaximum_queued_micros={}\n",
            rebuffered.device_name,
            prebuffering.queued_micros,
            started.queued_micros,
            drained.underruns,
            rebuffered.queued_micros,
            bytes_to_micros(MAX_SDL_BYTES),
        ),
    )?;
    Ok(())
}

fn play_proof_packet(output: &mut SdlAudioOutput, packet: usize) {
    let start = packet * PACKET_FRAMES;
    let interleaved = (start..start + PACKET_FRAMES)
        .flat_map(|frame| {
            let sample = (frame as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.025;
            [sample, sample]
        })
        .collect();
    output.play(&AudioSamples {
        captured_at: rustconsole_media::MediaTimestampMicros(packet as u64 * 10_000),
        format: rustconsole_media::AudioFormat {
            sample_rate: 48_000,
            channels: 2,
        },
        interleaved,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustconsole_media::{AudioFormat as MediaAudioFormat, MediaTimestampMicros};

    fn samples(timestamp: u64) -> DecodedAudioSamples {
        DecodedAudioSamples {
            generation: 1,
            sequence: timestamp / 10_000,
            diagnostics: true,
            assembled_at: Instant::now(),
            assembled_at_micros: timestamp,
            decoded_at: Instant::now(),
            decode_duration: Duration::ZERO,
            encoded_bytes: 1,
            concealed_packets: 0,
            decoder_input_hash_duration: Duration::ZERO,
            assembly_to_decoder_matched: None,
            ordered_playout_duration: None,
            decoder_queue_duration: None,
            samples: AudioSamples {
                captured_at: MediaTimestampMicros(timestamp),
                format: MediaAudioFormat {
                    sample_rate: 48_000,
                    channels: 2,
                },
                interleaved: vec![0.0; 960],
            },
        }
    }

    #[test]
    fn playback_queue_is_bounded_and_keeps_the_latest_audio() {
        let queue = AudioPlaybackQueue::default();
        for timestamp in (0..=80_000).step_by(10_000) {
            queue.push(DecodedAudioEvent::Samples(samples(timestamp)));
        }
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.pending_packets, 8);
        assert_eq!(snapshot.queue_drops, 1);
        assert!(matches!(
            queue
                .pop_for_video(Some(10_000))
                .unwrap(),
            AudioPlaybackDecision::Samples { decoded, .. }
                if decoded.samples.captured_at.0 == 10_000
        ));
    }

    #[test]
    fn playback_queue_holds_early_audio_and_drops_late_audio() {
        let queue = AudioPlaybackQueue::default();
        queue.push(DecodedAudioEvent::Samples(samples(100_001)));
        assert!(queue.pop_for_video(Some(60_000)).is_none());
        assert_eq!(queue.snapshot().pending_packets, 1);
        assert!(matches!(
            queue.pop_for_video(Some(140_002)),
            Some(AudioPlaybackDecision::Late {
                lateness_micros: 40_001,
                ..
            })
        ));
        assert_eq!(queue.snapshot().late_drops, 1);
    }

    #[test]
    fn playback_queue_reports_a_completed_video_sync_hold() {
        let queue = AudioPlaybackQueue::default();
        queue.push(DecodedAudioEvent::Samples(samples(100_001)));
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
    fn reset_clears_pending_audio() {
        let queue = AudioPlaybackQueue::default();
        queue.push(DecodedAudioEvent::Samples(samples(0)));
        queue.push(DecodedAudioEvent::Reset { generation: 2 });
        assert_eq!(queue.take_reset(), Some(2));
        assert_eq!(queue.snapshot().pending_packets, 0);
        assert_eq!(queue.snapshot().resets, 1);
    }

    #[test]
    fn byte_depth_uses_stereo_float_frames() {
        assert_eq!(bytes_to_micros(480 * 2 * size_of::<f32>()), 10_000);
        assert_eq!(bytes_to_micros(PREBUFFER_BYTES), 40_000);
        assert_eq!(bytes_to_micros(MAX_SDL_BYTES), 80_000);
    }
}
