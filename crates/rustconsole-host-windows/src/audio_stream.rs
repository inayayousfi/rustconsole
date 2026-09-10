use crate::audio::CableCapture;
use crate::audio_encode::{OpusAudioEncoder, OpusEncoderConfiguration};
use crate::clock::HostClock;
use crate::worker_protocol::{AudioWorkerEvent, write_audio_event};
use rustconsole_host_core::audio_transport::{AUDIO_QUEUE_PACKETS, MediaQueue, expired};
use rustconsole_host_core::{AudioCaptureEvent, SystemAudioCapture};
use rustconsole_media::AudioEncoder;
use rustconsole_protocol::audio::AudioPacket;
use rustconsole_protocol::wire::{AudioConfiguration, AudioStatus, AudioStreamState};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub struct WorkerAudio {
    stop: Arc<AtomicBool>,
    queue: Arc<MediaQueue<AudioWorkerEvent>>,
    capture: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
}

impl WorkerAudio {
    pub fn start(mut pipe: File, diagnostics: bool) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let queue = Arc::new(MediaQueue::new(AUDIO_QUEUE_PACKETS));
        let dropped = Arc::new(AtomicU64::new(0));
        let writer_stop = Arc::clone(&stop);
        let writer_queue = Arc::clone(&queue);
        let writer_dropped = Arc::clone(&dropped);
        let writer = thread::Builder::new()
            .name("audio-pipe".into())
            .spawn(move || {
                let mut generation = 1_u64;
                let result = (|| -> std::io::Result<()> {
                    let clock = HostClock::new()?;
                    loop {
                        let Some(mut event) = writer_queue.pop_timeout(Duration::from_millis(10))
                        else {
                            if writer_stop.load(Ordering::Acquire) || writer_queue.is_closed() {
                                break;
                            }
                            continue;
                        };
                        generation = generation.max(match &event {
                            AudioWorkerEvent::Packet { packet, .. } => packet.generation,
                            AudioWorkerEvent::State(state) => state.generation,
                        });
                        match &mut event {
                            AudioWorkerEvent::Packet {
                                queued_at_micros, ..
                            } if expired(*queued_at_micros, clock.now()?) => {
                                writer_dropped.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            AudioWorkerEvent::State(state) => {
                                state.dropped_packets = writer_dropped.load(Ordering::Relaxed)
                            }
                            _ => {}
                        }
                        write_audio_event(&mut pipe, &event)?;
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    let _ = write_audio_event(
                        &mut pipe,
                        &AudioWorkerEvent::State(AudioStreamState::new(
                            generation,
                            AudioStatus::Failed,
                            writer_dropped.load(Ordering::Relaxed),
                            error.to_string(),
                        )),
                    );
                    eprintln!("audio pipe: {error}");
                }
                writer_stop.store(true, Ordering::Release);
                writer_queue.close();
            })?;
        let capture_stop = Arc::clone(&stop);
        let capture_queue = Arc::clone(&queue);
        let capture = match thread::Builder::new()
            .name("system-audio".into())
            .spawn(move || {
                let mut generation = 1_u64;
                let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                    let clock = HostClock::new()?;
                    let mut capture = CableCapture::new()?;
                    let config = AudioConfiguration::INITIAL;
                    let mut encoder = OpusAudioEncoder::new(OpusEncoderConfiguration {
                        bitrate_bits_per_second: config.bitrate_bits_per_second,
                        packet_duration_micros: config.packet_duration_micros,
                    })?;
                    let mut sequence = 0_u64;
                    let mut have_input = false;
                    let mut reset_needed = false;
                    let mut capture_discontinuities = 0_u64;
                    let mut invalid_capture_timestamps = 0_u64;
                    let mut encoder_resets = 0_u64;
                    let mut status = AudioStatus::Waiting;
                    let mut reported = Instant::now() - Duration::from_secs(1);
                    while !capture_stop.load(Ordering::Acquire) {
                        let event = capture.next_samples()?;
                        let next_status = match &event {
                            AudioCaptureEvent::Samples { .. } => AudioStatus::Active,
                            AudioCaptureEvent::Unavailable => AudioStatus::Unavailable,
                            _ => status,
                        };
                        let changed = next_status != status;
                        status = next_status;
                        match event {
                            AudioCaptureEvent::Samples {
                                samples,
                                discontinuity,
                            } => {
                                let capture_buffer_frames = samples.interleaved.len() as u64 / 2;
                                if diagnostics && discontinuity {
                                    capture_discontinuities =
                                        capture_discontinuities.saturating_add(1);
                                }
                                if (discontinuity || reset_needed) && have_input {
                                    encoder.reset()?;
                                    if diagnostics {
                                        encoder_resets = encoder_resets.saturating_add(1);
                                    }
                                    generation = generation
                                        .checked_add(1)
                                        .ok_or("audio generation exhausted")?;
                                    sequence = 0;
                                }
                                reset_needed = false;
                                have_input = true;
                                let encode_started_at_micros = clock.now()?;
                                let packets = encoder.encode(samples)?;
                                let encoded_at_micros = clock.now()?;
                                for packet in packets {
                                    let packet = AudioPacket {
                                        generation,
                                        sequence,
                                        captured_at_micros: packet.captured_at.0,
                                        decoded_samples: packet.decoded_samples,
                                        skip_start_samples: packet.skip_start_samples,
                                        skip_end_samples: packet.skip_end_samples,
                                        payload: packet.payload,
                                    };
                                    let (payload_sha256, hash_duration_micros) = if diagnostics {
                                        let started = Instant::now();
                                        let digest = Sha256::digest(&packet.payload).into();
                                        (Some(digest), started.elapsed().as_micros() as u64)
                                    } else {
                                        (None, 0)
                                    };
                                    sequence = sequence
                                        .checked_add(1)
                                        .ok_or("audio sequence exhausted")?;
                                    if matches!(
                                        capture_queue.push(AudioWorkerEvent::Packet {
                                            queued_at_micros: clock.now()?,
                                            payload_sha256,
                                            hash_duration_micros,
                                            encode_started_at_micros,
                                            encoded_at_micros,
                                            capture_buffer_frames: diagnostics
                                                .then_some(capture_buffer_frames)
                                                .unwrap_or(0),
                                            capture_discontinuities,
                                            invalid_capture_timestamps,
                                            device_reopens: diagnostics
                                                .then_some(capture.device_reopens)
                                                .unwrap_or(0),
                                            encoder_resets,
                                            capture_queue_depth: diagnostics
                                                .then_some(capture_queue.depth() as u64)
                                                .unwrap_or(0),
                                            capture_queue_capacity: diagnostics
                                                .then_some(capture_queue.capacity() as u64)
                                                .unwrap_or(0),
                                            capture_queue_drops: diagnostics
                                                .then_some(dropped.load(Ordering::Relaxed))
                                                .unwrap_or(0),
                                            packet
                                        }),
                                        Some(AudioWorkerEvent::Packet { .. })
                                    ) {
                                        dropped.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            }
                            AudioCaptureEvent::Unavailable => {
                                reset_needed = true;
                                thread::sleep(Duration::from_millis(5));
                            }
                            AudioCaptureEvent::InvalidTimestamp => {
                                reset_needed = true;
                                if diagnostics {
                                    invalid_capture_timestamps =
                                        invalid_capture_timestamps.saturating_add(1);
                                }
                                thread::sleep(Duration::from_millis(5));
                            }
                            AudioCaptureEvent::Idle => thread::sleep(Duration::from_millis(2)),
                        }
                        if changed || reported.elapsed() >= Duration::from_millis(500) {
                            let detail = if status == AudioStatus::Unavailable {
                                capture.last_unavailable_reason.clone().unwrap_or_default()
                            } else {
                                String::new()
                            };
                            if matches!(
                                capture_queue.push(AudioWorkerEvent::State(AudioStreamState::new(
                                    generation,
                                    status,
                                    dropped.load(Ordering::Relaxed),
                                    detail
                                ))),
                                Some(AudioWorkerEvent::Packet { .. })
                            ) {
                                dropped.fetch_add(1, Ordering::Relaxed);
                            }
                            reported = Instant::now();
                        }
                    }
                    capture.reset()?;
                    encoder.reset()?;
                    Ok(())
                })();
                let state = match result {
                    Ok(()) => AudioStreamState::new(
                        generation,
                        AudioStatus::Stopped,
                        dropped.load(Ordering::Relaxed),
                        String::new(),
                    ),
                    Err(error) => AudioStreamState::new(
                        generation,
                        AudioStatus::Failed,
                        dropped.load(Ordering::Relaxed),
                        error.to_string(),
                    ),
                };
                capture_queue.push(AudioWorkerEvent::State(state));
                capture_queue.close();
            }) {
            Ok(capture) => capture,
            Err(error) => {
                stop.store(true, Ordering::Release);
                queue.close();
                let _ = writer.join();
                return Err(error);
            }
        };
        Ok(Self {
            stop,
            queue,
            capture: Some(capture),
            writer: Some(writer),
        })
    }
}

impl Drop for WorkerAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(capture) = self.capture.take()
            && capture.join().is_err()
        {
            eprintln!("audio capture thread panicked");
        }
        self.queue.close();
        if let Some(writer) = self.writer.take()
            && writer.join().is_err()
        {
            eprintln!("audio pipe thread panicked");
        }
    }
}
