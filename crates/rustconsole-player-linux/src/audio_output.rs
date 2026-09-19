use rustconsole_media::AudioSamples;
use rustconsole_player_core::AudioPlaybackQueue;
use sdl3::audio::{AudioFormat, AudioSpec, AudioStreamOwner};
use std::mem::size_of;
use std::path::Path;
use std::time::{Duration, Instant};

const PACKET_FRAMES: usize = 480;
const CHANNELS: usize = 2;
const PREBUFFER_PACKETS: usize = 4;
const MAX_DEVICE_PACKETS: usize = 8;
const PREBUFFER_BYTES: usize = PACKET_FRAMES * CHANNELS * PREBUFFER_PACKETS * size_of::<f32>();
const MAX_SDL_BYTES: usize = PACKET_FRAMES * CHANNELS * MAX_DEVICE_PACKETS * size_of::<f32>();
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
        let queue = queue.snapshot();
        let mut snapshot = AudioPlaybackSnapshot {
            pending_packets: queue.pending_packets,
            queued_micros: self.queued_micros,
            queue_drops: queue.queue_drops,
            late_drops: queue.late_drops,
            device_drops: self.device_drops,
            invalid_format_drops: self.invalid_format_drops,
            unavailable_device_drops: self.unavailable_device_drops,
            device_query_failures: self.device_query_failures,
            software_capacity_drops: self.software_capacity_drops,
            device_submission_failures: self.device_submission_failures,
            underruns: self.underruns,
            resets: queue.resets,
            decoder_failures: queue.decoder_failures,
            device_name: self.device_name.clone(),
            detail: queue.detail,
        };
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

    #[test]
    fn byte_depth_uses_stereo_float_frames() {
        assert_eq!(bytes_to_micros(480 * 2 * size_of::<f32>()), 10_000);
        assert_eq!(bytes_to_micros(PREBUFFER_BYTES), 40_000);
        assert_eq!(bytes_to_micros(MAX_SDL_BYTES), 80_000);
    }
}
