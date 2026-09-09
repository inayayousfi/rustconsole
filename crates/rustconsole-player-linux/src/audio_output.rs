use crate::DecodedAudioEvent;
use rustconsole_media::AudioSamples;
use sdl3::audio::{AudioFormat, AudioSpec, AudioStreamOwner};
use std::collections::VecDeque;
use std::mem::size_of;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PACKET_FRAMES: usize = 480;
const CHANNELS: usize = 2;
const MAX_PACKETS: usize = 4;
const MAX_SYNC_MICROS: u64 = 40_000;
const MAX_SDL_BYTES: usize = PACKET_FRAMES * CHANNELS * MAX_PACKETS * size_of::<f32>();
const DEVICE_RETRY: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AudioPlaybackSnapshot {
    pub pending_packets: usize,
    pub queued_micros: u32,
    pub queue_drops: u64,
    pub late_drops: u64,
    pub device_drops: u64,
    pub underruns: u64,
    pub resets: u64,
    pub device_name: String,
    pub detail: String,
}

#[derive(Default)]
struct QueueState {
    pending: VecDeque<AudioSamples>,
    reset: Option<u64>,
    queue_drops: u64,
    late_drops: u64,
    resets: u64,
    detail: String,
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
                state.pending.push_back(samples);
            }
            DecodedAudioEvent::Failed { detail, .. } => {
                state.pending.clear();
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

    pub fn pop_for_video(&self, video_timestamp_micros: Option<u64>) -> Option<AudioSamples> {
        let video_timestamp_micros = video_timestamp_micros?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            let audio_timestamp = state.pending.front()?.captured_at.0;
            if audio_timestamp > video_timestamp_micros.saturating_add(MAX_SYNC_MICROS) {
                return None;
            }
            if audio_timestamp.saturating_add(MAX_SYNC_MICROS) < video_timestamp_micros {
                state.pending.pop_front();
                state.late_drops += 1;
                continue;
            }
            return state.pending.pop_front();
        }
    }

    pub fn snapshot(&self) -> AudioPlaybackSnapshot {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        AudioPlaybackSnapshot {
            pending_packets: state.pending.len(),
            queue_drops: state.queue_drops,
            late_drops: state.late_drops,
            resets: state.resets,
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
        self.started = false;
        self.empty = true;
        self.queued_micros = 0;
    }

    pub fn play(&mut self, samples: AudioSamples) {
        if samples.format.sample_rate != 48_000
            || samples.format.channels != 2
            || samples.interleaved.is_empty()
            || !samples.interleaved.len().is_multiple_of(CHANNELS)
        {
            self.device_drops += 1;
            self.detail = "decoded audio has an invalid format".into();
            return;
        }
        self.poll();
        let Some(stream) = &self.stream else {
            self.device_drops += 1;
            return;
        };
        let bytes = samples.interleaved.len() * size_of::<f32>();
        let queued = match stream.queued_bytes() {
            Ok(queued) if queued >= 0 => queued as usize,
            Ok(_) => {
                self.fail("SDL reported a negative audio queue size".into());
                self.device_drops += 1;
                return;
            }
            Err(error) => {
                self.fail(error.to_string());
                self.device_drops += 1;
                return;
            }
        };
        if queued.saturating_add(bytes) > MAX_SDL_BYTES {
            self.device_drops += 1;
            return;
        }
        if let Err(error) = stream.put_data_f32(&samples.interleaved) {
            self.fail(error.to_string());
            self.device_drops += 1;
            return;
        }
        self.started = true;
        self.empty = false;
        self.queued_micros = bytes_to_micros(queued + bytes);
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
        }
    }

    pub fn snapshot(&self, queue: &AudioPlaybackQueue) -> AudioPlaybackSnapshot {
        let mut snapshot = queue.snapshot();
        snapshot.queued_micros = self.queued_micros;
        snapshot.device_drops = self.device_drops;
        snapshot.underruns = self.underruns;
        snapshot.device_name.clone_from(&self.device_name);
        if !self.detail.is_empty() {
            snapshot.detail.clone_from(&self.detail);
        }
        snapshot
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
            .and_then(|device| device.open_device_stream(Some(&spec)))
            .and_then(|stream| {
                stream.resume()?;
                Ok(stream)
            });
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
    for packet in 0..MAX_PACKETS {
        let start = packet * PACKET_FRAMES;
        let interleaved = (start..start + PACKET_FRAMES)
            .flat_map(|frame| {
                let sample =
                    (frame as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.025;
                [sample, sample]
            })
            .collect();
        output.play(AudioSamples {
            captured_at: rustconsole_media::MediaTimestampMicros(packet as u64 * 10_000),
            format: rustconsole_media::AudioFormat {
                sample_rate: 48_000,
                channels: 2,
            },
            interleaved,
        });
    }
    let initial = output.snapshot(&queue);
    if initial.device_name.is_empty()
        || initial.queued_micros == 0
        || initial.queued_micros > MAX_SYNC_MICROS as u32
    {
        return Err(format!("SDL audio output did not queue a bounded signal: {initial:?}").into());
    }
    std::thread::sleep(Duration::from_millis(80));
    output.poll();
    let final_snapshot = output.snapshot(&queue);
    if final_snapshot.queued_micros >= initial.queued_micros {
        return Err("SDL audio queue did not drain".into());
    }
    std::fs::write(
        report_path,
        format!(
            "status=ok\nformat=f32-stereo-48000\ndevice={}\ninitial_queued_micros={}\nfinal_queued_micros={}\nmaximum_queued_micros={}\n",
            initial.device_name,
            initial.queued_micros,
            final_snapshot.queued_micros,
            MAX_SYNC_MICROS,
        ),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustconsole_media::{AudioFormat as MediaAudioFormat, MediaTimestampMicros};

    fn samples(timestamp: u64) -> AudioSamples {
        AudioSamples {
            captured_at: MediaTimestampMicros(timestamp),
            format: MediaAudioFormat {
                sample_rate: 48_000,
                channels: 2,
            },
            interleaved: vec![0.0; 960],
        }
    }

    #[test]
    fn playback_queue_is_bounded_and_keeps_the_latest_audio() {
        let queue = AudioPlaybackQueue::default();
        for timestamp in [0, 10_000, 20_000, 30_000, 40_000] {
            queue.push(DecodedAudioEvent::Samples(samples(timestamp)));
        }
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.pending_packets, 4);
        assert_eq!(snapshot.queue_drops, 1);
        assert_eq!(
            queue.pop_for_video(Some(10_000)).unwrap().captured_at.0,
            10_000
        );
    }

    #[test]
    fn playback_queue_holds_early_audio_and_drops_late_audio() {
        let queue = AudioPlaybackQueue::default();
        queue.push(DecodedAudioEvent::Samples(samples(100_001)));
        assert!(queue.pop_for_video(Some(60_000)).is_none());
        assert_eq!(queue.snapshot().pending_packets, 1);
        assert!(queue.pop_for_video(Some(140_002)).is_none());
        assert_eq!(queue.snapshot().late_drops, 1);
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
        assert_eq!(bytes_to_micros(MAX_SDL_BYTES), 40_000);
    }
}
