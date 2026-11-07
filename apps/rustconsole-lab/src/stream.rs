//! Headless player: real authentication, transport, and hardware decoding, with decoded
//! frames discarded. Decode completion stands in for presentation, which needs a display.

use rustconsole_player_core::latency::{LatencyMeasurements, capture_player_at};
use rustconsole_player_core::{DecodedAudioEvent, StreamProgress, VideoStreamSample};
use rustconsole_player_linux::{DecodedVideoFrame, StreamCallbacks, StreamConfiguration};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_millis(500);

pub struct Options {
    pub address: SocketAddr,
    pub seconds: u64,
    pub output: PathBuf,
    pub maximum_bitrate_bits_per_second: u64,
    pub maximum_delay_micros: u64,
    pub frames_per_second: u16,
}

#[derive(Default)]
struct Interval {
    frames: u64,
    fresh_frames: u64,
    keyframes: u64,
    longest_frame_gap: u64,
    bytes: u64,
    capture_to_decoded: Vec<u64>,
    capture_to_encoded: Vec<u64>,
    packetized_to_assembled: Vec<u64>,
    decode: Vec<u64>,
    decoder_queue: Vec<u64>,
}

struct Recorder {
    output: BufWriter<File>,
    started: Instant,
    flushed: Instant,
    interval: Interval,
    target: u64,
    latest: Option<VideoStreamSample>,
    pressure_ppm: u32,
    latency_ceiling: u64,
    clock_uncertainty: u64,
    last_capture: u64,
    last_decoded: Option<Instant>,
}

fn percentile(values: &mut [u64], percent: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[(values.len() - 1) * percent / 100]
}

impl Recorder {
    fn flush_if_due(&mut self, now: Instant) -> std::io::Result<()> {
        let elapsed = now.saturating_duration_since(self.flushed);
        if elapsed < INTERVAL {
            return Ok(());
        }
        self.flushed = now;
        let mut interval = std::mem::take(&mut self.interval);
        let sample = self.latest;
        let bits_per_second =
            u128::from(interval.bytes) * 8 * 1_000_000 / elapsed.as_micros().max(1);
        writeln!(
            self.output,
            concat!(
                "{{\"t\":{:.3},\"target\":{},\"encoded\":{},\"frames\":{},\"fresh\":{},\"keyframes\":{},\"frame_gap_max\":{},",
                "\"delay_p50\":{},\"delay_p99\":{},\"delay_max\":{},",
                "\"encode_p50\":{},\"network_p50\":{},\"network_p99\":{},\"decode_p50\":{},\"decode_max\":{},\"decoder_queue_max\":{},",
                "\"rtt\":{},\"capacity\":{},\"ceiling\":{},\"latency_ceiling\":{},",
                "\"pressure_ppm\":{},\"lost\":{},\"late\":{},\"incomplete\":{},",
                "\"overflows\":{},\"clock_uncertainty\":{}}}"
            ),
            now.saturating_duration_since(self.started).as_secs_f64(),
            self.target,
            bits_per_second,
            interval.frames,
            interval.fresh_frames,
            interval.keyframes,
            interval.longest_frame_gap,
            percentile(&mut interval.capture_to_decoded, 50),
            percentile(&mut interval.capture_to_decoded, 99),
            percentile(&mut interval.capture_to_decoded, 100),
            percentile(&mut interval.capture_to_encoded, 50),
            percentile(&mut interval.packetized_to_assembled, 50),
            percentile(&mut interval.packetized_to_assembled, 99),
            percentile(&mut interval.decode, 50),
            percentile(&mut interval.decode, 100),
            percentile(&mut interval.decoder_queue, 100),
            sample.map_or(0, |sample| sample.round_trip_time.as_micros()),
            sample.map_or(0, |sample| sample.estimated_capacity_bits_per_second),
            sample
                .and_then(|sample| sample.soft_ceiling_bits_per_second)
                .unwrap_or(0),
            self.latency_ceiling,
            self.pressure_ppm,
            sample.map_or(0, |sample| sample.lost_chunks),
            sample.map_or(0, |sample| sample.late_chunks),
            sample.map_or(0, |sample| sample.incomplete_frames),
            sample.map_or(0, |sample| sample.assembly_overflows),
            self.clock_uncertainty,
        )?;
        self.output.flush()
    }

    fn observe_frame(
        &mut self,
        frame: &DecodedVideoFrame,
        measurements: &LatencyMeasurements,
        decoded_at: Instant,
    ) -> std::io::Result<()> {
        self.target = frame.target_bitrate_bits_per_second;
        self.interval.frames += 1;
        if let Some(last) = self.last_decoded.replace(decoded_at) {
            let gap = decoded_at.saturating_duration_since(last).as_micros() as u64;
            self.interval.longest_frame_gap = self.interval.longest_frame_gap.max(gap);
        }
        self.interval.bytes += frame.encoded_frame_bytes as u64;
        // A repeated capture of an unchanged screen carries its original timestamp, which
        // is a frame age, not a delay.
        let new_capture = frame.captured_at_micros != self.last_capture;
        self.last_capture = frame.captured_at_micros;
        self.interval.fresh_frames += u64::from(new_capture);
        if new_capture {
            self.interval.capture_to_encoded.push(
                frame
                    .encoded_at_micros
                    .saturating_sub(frame.captured_at_micros),
            );
        }
        self.interval
            .decode
            .push(frame.decode_duration.as_micros() as u64);
        if let Some(queue) = frame.decoder_queue_duration {
            self.interval.decoder_queue.push(queue.as_micros() as u64);
        }
        let clock = measurements.clock(decoded_at);
        let captured = clock.zip(frame.assembled_at).and_then(|(clock, at)| {
            capture_player_at(
                clock,
                frame.captured_at_micros,
                frame.assembled_at_micros,
                at,
            )
        });
        if let Some(captured) = captured.filter(|_| new_capture) {
            self.interval
                .capture_to_decoded
                .push(decoded_at.saturating_duration_since(captured).as_micros() as u64);
        }
        if let Some(clock) = clock {
            let packetized =
                i128::from(frame.packetized_at_micros) - i128::from(clock.offset_micros);
            let network = i128::from(frame.assembled_at_micros) - packetized;
            self.interval
                .packetized_to_assembled
                .push(u64::try_from(network).unwrap_or(0));
            self.clock_uncertainty = clock.uncertainty_micros;
        }
        measurements.video_presentation(
            frame.captured_at_micros,
            captured,
            decoded_at,
            frame.target_bitrate_bits_per_second,
            true,
        );
        self.flush_if_due(decoded_at)
    }
}

pub fn run(options: Options) -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(options.seconds);
    let recorder = Arc::new(Mutex::new(Recorder {
        output: BufWriter::new(File::create(&options.output)?),
        started,
        flushed: started,
        interval: Interval::default(),
        target: 0,
        latest: None,
        pressure_ppm: 0,
        latency_ceiling: 0,
        clock_uncertainty: 0,
        last_capture: 0,
        last_decoded: None,
    }));
    let measurements = LatencyMeasurements::default();
    let audio_measurements = measurements.clone();
    let video_measurements = measurements.clone();
    let progress_recorder = Arc::clone(&recorder);
    let statistics_recorder = Arc::clone(&recorder);
    let video_recorder = Arc::clone(&recorder);
    // The sender stays alive so the session sees an idle input channel, not a closed one.
    let (_input, input_rx) = rustconsole_player_core::input_channel();
    let result = rustconsole_player_linux::stream_quic_video(
        StreamConfiguration {
            maximum_delay_micros: options.maximum_delay_micros,
            latency_measurements: measurements,
            display: None,
            address: options.address.to_string(),
            password: None,
            remember_password: false,
            maximum_bitrate_bits_per_second: options.maximum_bitrate_bits_per_second,
            frames_per_second: options.frames_per_second,
            latency_diagnostics: false,
            diagnostic_probe_sequence: Arc::new(AtomicU64::new(0)),
        },
        move || Instant::now() >= deadline,
        input_rx,
        StreamCallbacks {
            authenticated: |_| eprintln!("authenticated"),
            progress: move |progress| {
                if let StreamProgress::LatencyStatus(status) = progress {
                    let mut recorder = progress_recorder
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    recorder.pressure_ppm = status.highest_pressure_ppm;
                    recorder.latency_ceiling = status.ceiling_bits_per_second.unwrap_or(0);
                }
            },
            statistics: move |sample: rustconsole_player_core::VideoStreamSample| {
                let mut recorder = statistics_recorder
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                recorder.interval.keyframes += u64::from(sample.keyframe);
                recorder.latest = Some(sample);
            },
            audio: move |event| {
                if let DecodedAudioEvent::Samples(decoded) = event {
                    audio_measurements.audio_playback(
                        decoded.samples.captured_at.0,
                        decoded.assembled_at_micros,
                        decoded.assembled_at,
                        Instant::now(),
                        0,
                    );
                }
                Ok(())
            },
            video: move |frame: DecodedVideoFrame| {
                let decoded_at = Instant::now();
                video_recorder
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .observe_frame(&frame, &video_measurements, decoded_at)?;
                Ok(true)
            },
        },
    )?;
    eprintln!("stream ended: {:?}", result.end);
    Ok(())
}
