//! Linux decoding, rendering, audio, input, and secret-store integration.

#[cfg(target_os = "linux")]
mod audio_output;
#[cfg(target_os = "linux")]
mod opengl_renderer;
#[cfg(target_os = "linux")]
mod renderer_benchmark;

#[cfg(target_os = "linux")]
pub use audio_output::{
    AudioPlaybackDecision, AudioPlaybackQueue, AudioPlaybackSnapshot, SdlAudioOutput,
    run_sdl_audio_proof,
};
#[cfg(target_os = "linux")]
pub use opengl_renderer::{NativeVideoSink, NativeVideoSurface, NativeVideoSurfaceStatus};
pub use rustconsole_codec_ffmpeg::Av1ColorDescription;
#[cfg(target_os = "linux")]
pub use rustconsole_codec_ffmpeg::DmaBufFrameFormat;
#[cfg(target_os = "linux")]
pub use rustconsole_codec_ffmpeg::MappedDmaBufFrame as NativeDmaBufFrame;

use rustconsole_codec_ffmpeg::opus::{OpusDecoder, OpusPacket};
use rustconsole_codec_ffmpeg::{
    Av1VaApiDecoder, DecodedAv1Frame, HardwareDevice, HardwareDeviceType,
};
use rustconsole_media::{AudioFormat, AudioSamples, MediaTimestampMicros};
use rustconsole_player_core::AudioPlaybackEvent;
use rustconsole_protocol::wire::{
    self, Av1CapabilityOffer, Av1HardwareCapability, Av1Mode, Av1ViewerSettings, ChromaSubsampling,
    EncodedVideoPacket, Envelope, SelectedAv1Configuration, VideoBitDepth, envelope,
};
use rustconsole_protocol::{
    Av1HardwareCapability as DomainCapability, Av1Mode as DomainMode,
    Av1ViewerSettings as DomainSettings, ChromaSubsampling as DomainChroma,
    VideoBitDepth as DomainDepth, negotiate_av1_configuration,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
#[cfg(target_os = "linux")]
use zeroize::Zeroizing;

const WIDTH: u32 = 2560;
const HEIGHT: u32 = 1440;
const FRAMES_PER_SECOND: u16 = 120;
const DEFAULT_MAXIMUM_BITRATE: u64 = 20_000_000;
const VAAPI_DEVICE: &str = "/dev/dri/renderD128";
const MAX_DIAGNOSTIC_MARKER_PROBES_PER_INPUT: u8 = 8;
const CAPABILITY_FIXTURE: &[u8] = include_bytes!("../test-data/av1-2560x1440-420-8.obu");

#[cfg(target_os = "linux")]
pub struct LinuxSecretStore;

#[derive(Clone, Copy, Debug)]
pub struct VideoStreamSample {
    pub encoded_frame_bytes: usize,
    pub target_bitrate_bits_per_second: u64,
    pub estimated_capacity_bits_per_second: u64,
    pub round_trip_time: Duration,
    pub lost_chunks: u64,
    pub late_chunks: u64,
    pub assembly_overflows: u64,
    pub completed_frames: u64,
    pub incomplete_frames: u64,
}

pub struct DecodedVideoFrame {
    pub sequence: u64,
    pub encoded_frame_bytes: usize,
    pub captured_at_micros: u64,
    pub encoded_at_micros: u64,
    pub packetized_at_micros: u64,
    pub input_sequence: u64,
    pub assembled_at: Option<Instant>,
    pub assembled_at_micros: u64,
    pub assembly_duration: Duration,
    pub decoder_queue_duration: Option<Duration>,
    pub decode_duration: Duration,
    pub decoded_at: Option<Instant>,
    pub decoder_input_hash_duration: Duration,
    pub assembly_to_decoder_matched: Option<bool>,
    pub diagnostic_marker_input_sequence: Option<u64>,
    pub diagnostic_copy_duration: Option<Duration>,
    pub diagnostic_copy_bytes: u64,
    pub color_description: Av1ColorDescription,
    pub frame: DecodedAv1Frame,
}

pub struct StreamCallbacks<Authenticated, Progress, Statistics, Audio, Video> {
    pub authenticated: Authenticated,
    pub progress: Progress,
    pub statistics: Statistics,
    pub audio: Audio,
    pub video: Video,
}

#[derive(Debug)]
pub enum DecodedAudioEvent {
    Reset { generation: u64 },
    Samples(DecodedAudioSamples),
    Failed { generation: u64, detail: String },
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

#[derive(Default)]
struct StreamAudioDecoder {
    decoder: Option<OpusDecoder>,
    generation: u64,
    expected: Option<u64>,
    failed_generation: Option<u64>,
    diagnostics: bool,
}

impl StreamAudioDecoder {
    fn event(&mut self, event: AudioPlaybackEvent) -> Vec<DecodedAudioEvent> {
        match event {
            AudioPlaybackEvent::Packet {
                packet,
                assembled_at,
                released_at,
                assembled_at_micros,
                assembled_payload_sha256,
            } => self.packet(
                packet,
                assembled_at,
                released_at,
                assembled_at_micros,
                assembled_payload_sha256,
            ),
            AudioPlaybackEvent::Missing {
                generation,
                sequence,
                captured_at_micros,
                missing_packets,
            } => {
                let mut output = self.prepare_generation(generation);
                if let Some(decoder) = self.decoder.as_mut() {
                    decoder.reset();
                }
                if !matches!(
                    output.last(),
                    Some(DecodedAudioEvent::Reset {
                        generation: current
                    }) if *current == generation
                ) {
                    output.push(DecodedAudioEvent::Reset { generation });
                }
                let count = missing_packets.min(4);
                output.push(DecodedAudioEvent::Samples(DecodedAudioSamples {
                    generation,
                    sequence,
                    diagnostics: self.diagnostics,
                    assembled_at: Instant::now(),
                    assembled_at_micros: 0,
                    decoded_at: Instant::now(),
                    ordered_playout_duration: None,
                    decoder_queue_duration: None,
                    decode_duration: Duration::ZERO,
                    encoded_bytes: 0,
                    concealed_packets: count,
                    decoder_input_hash_duration: Duration::ZERO,
                    assembly_to_decoder_matched: None,
                    samples: AudioSamples {
                        captured_at: MediaTimestampMicros(captured_at_micros),
                        format: AudioFormat {
                            sample_rate: 48_000,
                            channels: 2,
                        },
                        interleaved: vec![0.0; count as usize * 480 * 2],
                    },
                }));
                self.expected = sequence.checked_add(missing_packets);
                output
            }
        }
    }

    fn packet(
        &mut self,
        packet: rustconsole_protocol::audio::AudioPacket,
        assembled_at: Instant,
        released_at: Instant,
        assembled_at_micros: u64,
        assembled_payload_sha256: Option<[u8; 32]>,
    ) -> Vec<DecodedAudioEvent> {
        let mut output = self.prepare_generation(packet.generation);
        if self.failed_generation == Some(packet.generation) {
            return output;
        }
        if self
            .expected
            .is_some_and(|expected| packet.sequence < expected)
        {
            return output;
        }
        if let Some(expected) = self.expected
            && packet.sequence > expected
        {
            if let Some(decoder) = self.decoder.as_mut() {
                decoder.reset();
            }
            output.push(DecodedAudioEvent::Reset {
                generation: packet.generation,
            });
            let missing = (packet.sequence - expected).min(4);
            let captured_at = packet
                .captured_at_micros
                .saturating_sub(missing * rustconsole_protocol::audio::PACKET_DURATION_MICROS);
            output.push(DecodedAudioEvent::Samples(DecodedAudioSamples {
                generation: packet.generation,
                sequence: expected,
                diagnostics: self.diagnostics,
                assembled_at,
                assembled_at_micros,
                decoded_at: Instant::now(),
                ordered_playout_duration: self
                    .diagnostics
                    .then(|| released_at.saturating_duration_since(assembled_at)),
                decoder_queue_duration: None,
                decode_duration: Duration::ZERO,
                encoded_bytes: 0,
                concealed_packets: missing,
                decoder_input_hash_duration: Duration::ZERO,
                assembly_to_decoder_matched: None,
                samples: AudioSamples {
                    captured_at: MediaTimestampMicros(captured_at),
                    format: AudioFormat {
                        sample_rate: 48_000,
                        channels: 2,
                    },
                    interleaved: vec![0.0; missing as usize * 480 * 2],
                },
            }));
        }
        if self.decoder.is_none() {
            match OpusDecoder::open() {
                Ok(decoder) => self.decoder = Some(decoder),
                Err(error) => {
                    self.failed_generation = Some(packet.generation);
                    output.push(DecodedAudioEvent::Failed {
                        generation: packet.generation,
                        detail: error.to_string(),
                    });
                    return output;
                }
            }
        }
        let encoded_bytes = packet.payload.len();
        let (decoder_input_hash_duration, assembly_to_decoder_matched) =
            if let Some(assembled_sha256) = assembled_payload_sha256 {
                let started = Instant::now();
                let decoder_sha256: [u8; 32] = Sha256::digest(&packet.payload).into();
                (started.elapsed(), Some(decoder_sha256 == assembled_sha256))
            } else {
                (Duration::ZERO, None)
            };
        let encoded = OpusPacket {
            captured_at_micros: packet.captured_at_micros,
            decoded_samples: packet.decoded_samples,
            skip_start_samples: packet.skip_start_samples,
            skip_end_samples: packet.skip_end_samples,
            payload: packet.payload,
        };
        let decode_started = Instant::now();
        let decoder_queue_duration = self
            .diagnostics
            .then(|| decode_started.saturating_duration_since(released_at));
        match self.decoder.as_mut().unwrap().decode(&encoded) {
            Ok(interleaved) => {
                let decode_duration = decode_started.elapsed();
                self.expected = packet.sequence.checked_add(1);
                output.push(DecodedAudioEvent::Samples(DecodedAudioSamples {
                    generation: packet.generation,
                    sequence: packet.sequence,
                    diagnostics: self.diagnostics,
                    assembled_at,
                    assembled_at_micros,
                    decoded_at: Instant::now(),
                    ordered_playout_duration: self
                        .diagnostics
                        .then(|| released_at.saturating_duration_since(assembled_at)),
                    decoder_queue_duration,
                    decode_duration,
                    encoded_bytes,
                    concealed_packets: 0,
                    decoder_input_hash_duration,
                    assembly_to_decoder_matched,
                    samples: AudioSamples {
                        captured_at: MediaTimestampMicros(encoded.captured_at_micros),
                        format: AudioFormat {
                            sample_rate: 48_000,
                            channels: 2,
                        },
                        interleaved,
                    },
                }));
            }
            Err(error) => {
                self.decoder = None;
                self.failed_generation = Some(packet.generation);
                output.push(DecodedAudioEvent::Failed {
                    generation: packet.generation,
                    detail: error.to_string(),
                });
            }
        }
        output
    }

    fn prepare_generation(&mut self, generation: u64) -> Vec<DecodedAudioEvent> {
        if generation <= self.generation {
            return Vec::new();
        }
        self.generation = generation;
        self.expected = None;
        self.failed_generation = None;
        if let Some(decoder) = self.decoder.as_mut() {
            decoder.reset();
        }
        vec![DecodedAudioEvent::Reset { generation }]
    }
}

#[cfg(target_os = "linux")]
impl LinuxSecretStore {
    pub fn available() -> Result<(), &'static keyring::Error> {
        keyring::Entry::store_status().as_ref().copied()
    }

    fn entry(host_identity: &str) -> Result<keyring::Entry, keyring::Error> {
        keyring::Entry::new("org.rustconsole.viewer", host_identity)
    }
}

#[cfg(target_os = "linux")]
impl rustconsole_player_core::SecretStore for LinuxSecretStore {
    type Error = keyring::Error;

    fn load(&self, host_identity: &str) -> Result<Option<Vec<u8>>, Self::Error> {
        match Self::entry(host_identity)?.get_secret() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn store(&mut self, host_identity: &str, secret: &[u8]) -> Result<(), Self::Error> {
        Self::entry(host_identity)?.set_secret(secret)
    }

    fn remove(&mut self, host_identity: &str) -> Result<(), Self::Error> {
        match Self::entry(host_identity)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

pub fn run_authentication_probe(
    address: &str,
    password: Vec<u8>,
) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    let address = address.parse::<SocketAddr>()?;
    let identity = rustconsole_player_core::authenticate_host(address, password)?;
    Ok(*identity.as_bytes())
}

pub fn stream_quic_video(
    address: &str,
    password: Option<Vec<u8>>,
    remember_password: bool,
    maximum_bitrate_bits_per_second: u64,
    latency_diagnostics: bool,
    diagnostic_probe_sequence: Arc<AtomicU64>,
    should_stop: impl Fn() -> bool,
    next_input: impl FnMut() -> Option<rustconsole_player_core::TimedInputEvent> + Send + 'static,
    callbacks: StreamCallbacks<
        impl FnOnce([u8; 32]),
        impl FnMut(rustconsole_player_core::StreamProgress),
        impl FnMut(VideoStreamSample),
        impl FnMut(DecodedAudioEvent) -> Result<(), Box<dyn std::error::Error>>,
        impl FnMut(DecodedVideoFrame) -> Result<bool, Box<dyn std::error::Error>>,
    >,
) -> Result<rustconsole_player_core::StreamHostResult, Box<dyn std::error::Error>> {
    let StreamCallbacks {
        authenticated: observe_authenticated,
        progress: mut observe_progress,
        statistics: mut observe_statistics,
        audio: mut consume_audio,
        video: mut consume_video,
    } = callbacks;
    let address = address.parse::<SocketAddr>()?;
    let capability_8 = DomainCapability {
        mode: DomainMode {
            chroma_subsampling: DomainChroma::Yuv420,
            bit_depth: DomainDepth::Eight,
        },
        maximum_width: WIDTH,
        maximum_height: HEIGHT,
        maximum_frames_per_second: FRAMES_PER_SECOND,
    };
    let capability_10 = DomainCapability {
        mode: DomainMode {
            chroma_subsampling: DomainChroma::Yuv420,
            bit_depth: DomainDepth::Ten,
        },
        maximum_width: WIDTH,
        maximum_height: HEIGHT,
        maximum_frames_per_second: FRAMES_PER_SECOND,
    };
    let settings = DomainSettings {
        width: WIDTH,
        height: HEIGHT,
        frames_per_second: FRAMES_PER_SECOND,
        mode_preferences: vec![capability_10.mode, capability_8.mode],
        maximum_bitrate_bits_per_second,
    };
    let device = HardwareDevice::open(HardwareDeviceType::VaApi, Some(VAAPI_DEVICE))?;
    let mut decoder = Av1VaApiDecoder::open(&device)?;
    let mut audio_decoder = StreamAudioDecoder {
        diagnostics: latency_diagnostics,
        ..StreamAudioDecoder::default()
    };
    let mut diagnostic_marker = None;
    let mut diagnostic_baseline_attempted = false;
    let mut last_diagnostic_probe_sequence = 0;
    let mut active_diagnostic_probe_sequence = 0;
    let mut diagnostic_probe_attempts = 0;
    let selected_depth = Arc::new(Mutex::new(None));
    let progress_depth = Arc::clone(&selected_depth);
    let video_depth = Arc::clone(&selected_depth);
    let supplied_password = password.map(Zeroizing::new);
    let password_to_store = supplied_password.clone();
    let identity = rustconsole_player_core::stream_host(rustconsole_player_core::StreamHostParameters {
        address,
        password_for: move |host_identity: rustconsole_player_core::HostIdentity| {
            supplied_password
                .as_ref()
                .map(|password| password.to_vec())
                .or_else(|| {
                    use rustconsole_player_core::SecretStore;
                    LinuxSecretStore
                        .load(&host_identity_key(host_identity.as_bytes()))
                        .ok()
                        .flatten()
                })
        },
        on_authenticated: move |host_identity: rustconsole_player_core::HostIdentity| {
            if remember_password && let Some(password) = &password_to_store {
                use rustconsole_player_core::SecretStore;
                LinuxSecretStore.store(
                    &host_identity_key(host_identity.as_bytes()),
                    password.as_slice(),
                )?;
            }
            observe_authenticated(*host_identity.as_bytes());
            Ok(())
        },
        on_progress: move |progress: rustconsole_player_core::StreamProgress| {
            if let rustconsole_player_core::StreamProgress::VideoNegotiated(configuration) = &progress {
                *progress_depth.lock().unwrap_or_else(|error| error.into_inner()) =
                    Some(configuration.mode.bit_depth);
            }
            observe_progress(progress);
        },
        full_diagnostics: latency_diagnostics,
        video: (vec![capability_10, capability_8], settings),
        should_stop,
        next_input,
        consumers: rustconsole_player_core::StreamConsumers {
            audio: move |event| {
                for event in audio_decoder.event(event) {
                    consume_audio(event)?;
                }
                Ok(())
            },
            video: move |
                frame: rustconsole_player_core::VideoFramePayload,
                transport: rustconsole_player_core::StreamTransportStatistics,
            | {
                observe_statistics(VideoStreamSample {
                    encoded_frame_bytes: frame.payload.len(),
                    target_bitrate_bits_per_second: frame.target_bitrate_bits_per_second,
                    estimated_capacity_bits_per_second: frame.estimated_capacity_bits_per_second,
                    round_trip_time: transport.round_trip_time,
                    lost_chunks: transport.assembly.lost_chunks,
                    late_chunks: transport.assembly.late_chunks,
                    assembly_overflows: transport.assembly.assembly_overflows,
                    completed_frames: transport.assembly.completed_frames,
                    incomplete_frames: transport.assembly.incomplete_frames,
                });
                let (decoder_input_hash_duration, assembly_to_decoder_matched) =
                    if let Some(assembled_sha256) = transport.assembled_payload_sha256 {
                        let started = Instant::now();
                        let decoder_sha256: [u8; 32] = Sha256::digest(&frame.payload).into();
                        (started.elapsed(), Some(decoder_sha256 == assembled_sha256))
                    } else {
                        (Duration::ZERO, None)
                    };
                let decode_started = std::time::Instant::now();
                let decoder_queue_duration = transport
                    .assembled_at
                    .map(|assembled_at| decode_started.saturating_duration_since(assembled_at));
                let decoded = decoder.decode_packet(&frame.payload)?;
                let decode_duration = decode_started.elapsed();
                let decoded_at = latency_diagnostics.then(Instant::now);
                match decoded {
                    Some(decoded) => {
                        let color_description = decoded
                            .color_description()
                            .ok_or("decoded AV1 frame has unsupported color metadata")?;
                        let expected_depth = *video_depth
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        if !matches!(
                            (expected_depth, color_description),
                            (Some(DomainDepth::Eight), Av1ColorDescription::Bt709Limited)
                                | (Some(DomainDepth::Ten), Av1ColorDescription::Bt2020PqLimited)
                        ) {
                            return Err("decoded AV1 color metadata contradicts the negotiated bit depth".into());
                        }
                        let mut diagnostic_marker_input_sequence = None;
                        let mut diagnostic_copy_duration = None;
                        let mut diagnostic_copy_bytes = 0;
                        let requested_probe_sequence =
                            diagnostic_probe_sequence.load(Ordering::Acquire);
                        if take_diagnostic_marker_probe(
                            latency_diagnostics,
                            diagnostic_baseline_attempted,
                            frame.input_sequence,
                            requested_probe_sequence,
                            &mut last_diagnostic_probe_sequence,
                            &mut active_diagnostic_probe_sequence,
                            &mut diagnostic_probe_attempts,
                        ) {
                            diagnostic_baseline_attempted = true;
                            let copy_started = std::time::Instant::now();
                            let (samples, bit_depth, transferred_bytes) = decoded
                                .download_luma_samples(&[(24, 32), (56, 32), (96, 32)])?;
                            diagnostic_copy_duration = Some(copy_started.elapsed());
                            diagnostic_copy_bytes = transferred_bytes;
                            let marker = diagnostic_marker_state(&samples, bit_depth);
                            if let Some(marker) = marker {
                                let previous_marker = diagnostic_marker;
                                if previous_marker.is_some_and(|previous| previous != marker) {
                                    diagnostic_marker_input_sequence =
                                        Some(requested_probe_sequence);
                                    last_diagnostic_probe_sequence = requested_probe_sequence;
                                    active_diagnostic_probe_sequence = 0;
                                    diagnostic_probe_attempts = 0;
                                } else if previous_marker.is_none() {
                                    diagnostic_probe_attempts = 0;
                                }
                                diagnostic_marker = Some(marker);
                            }
                            if diagnostic_marker_input_sequence.is_none()
                                && diagnostic_probe_attempts
                                    >= MAX_DIAGNOSTIC_MARKER_PROBES_PER_INPUT
                                && requested_probe_sequence != 0
                            {
                                last_diagnostic_probe_sequence = requested_probe_sequence;
                                active_diagnostic_probe_sequence = 0;
                                diagnostic_probe_attempts = 0;
                            }
                        }
                        consume_video(DecodedVideoFrame {
                            sequence: frame.sequence,
                            encoded_frame_bytes: frame.payload.len(),
                            captured_at_micros: frame.captured_at_micros,
                            encoded_at_micros: frame.encoded_at_micros,
                            packetized_at_micros: frame.packetized_at_micros,
                            input_sequence: frame.input_sequence,
                            assembled_at: transport.assembled_at,
                            assembled_at_micros: transport.assembled_at_micros,
                            assembly_duration: transport.assembly_duration,
                            decoder_queue_duration,
                            decode_duration,
                            decoded_at,
                            decoder_input_hash_duration,
                            assembly_to_decoder_matched,
                            diagnostic_marker_input_sequence,
                            diagnostic_copy_duration,
                            diagnostic_copy_bytes,
                            color_description,
                            frame: decoded,
                        })
                    }
                    None => Ok(true),
                }
            },
        },
    })?;
    Ok(identity)
}

fn host_identity_key(identity: &[u8; 32]) -> String {
    identity.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn diagnostic_marker_state(samples: &[u16], bit_depth: u16) -> Option<bool> {
    let &[white, black, state] = samples else {
        return None;
    };
    let shift = bit_depth.checked_sub(2)?;
    let dark = 1_u16.checked_shl(u32::from(shift))?;
    let bright = 3_u16.checked_shl(u32::from(shift))?;
    (white >= bright && black <= dark && (state <= dark || state >= bright))
        .then_some(state >= bright)
}

fn take_diagnostic_marker_probe(
    latency_diagnostics: bool,
    baseline_attempted: bool,
    frame_input_sequence: u64,
    requested_sequence: u64,
    last_probe_sequence: &mut u64,
    active_probe_sequence: &mut u64,
    probe_attempts: &mut u8,
) -> bool {
    if !latency_diagnostics {
        return false;
    }
    if !baseline_attempted {
        return true;
    }
    if requested_sequence == 0
        || requested_sequence <= *last_probe_sequence
        || frame_input_sequence < requested_sequence
    {
        return false;
    }
    if *active_probe_sequence != requested_sequence {
        *active_probe_sequence = requested_sequence;
        *probe_attempts = 0;
    }
    if *probe_attempts >= MAX_DIAGNOSTIC_MARKER_PROBES_PER_INPUT {
        *last_probe_sequence = requested_sequence;
        *active_probe_sequence = 0;
        *probe_attempts = 0;
        return false;
    }
    *probe_attempts += 1;
    true
}

pub fn run_one_frame_proof(
    address: &str,
    report_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let validation_device = HardwareDevice::open(HardwareDeviceType::VaApi, Some(VAAPI_DEVICE))?;
    let mut validation_decoder = Av1VaApiDecoder::open(&validation_device)?;
    let validation_frame = validation_decoder.decode_one_packet(CAPABILITY_FIXTURE)?;
    if validation_frame.width() != WIDTH || validation_frame.height() != HEIGHT {
        return Err("VA-API capability fixture returned unexpected dimensions".into());
    }

    let local_capability = DomainCapability {
        mode: DomainMode {
            chroma_subsampling: DomainChroma::Yuv420,
            bit_depth: DomainDepth::Eight,
        },
        maximum_width: WIDTH,
        maximum_height: HEIGHT,
        maximum_frames_per_second: FRAMES_PER_SECOND,
    };
    let settings = DomainSettings {
        width: WIDTH,
        height: HEIGHT,
        frames_per_second: FRAMES_PER_SECOND,
        mode_preferences: vec![local_capability.mode],
        maximum_bitrate_bits_per_second: DEFAULT_MAXIMUM_BITRATE,
    };
    let offer = Envelope {
        body: Some(envelope::Body::Av1CapabilityOffer(Av1CapabilityOffer {
            dedicated_input_stream: false,
            host_pointer_release: false,
            full_diagnostics: false,
            audio_transport: None,
            encoder_capabilities: Vec::new(),
            decoder_capabilities: vec![wire_capability(local_capability)],
            viewer_settings: Some(wire_settings(&settings)),
        })),
    };

    let mut stream = TcpStream::connect(address)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    write_all_frame(&mut stream, &wire::encode_reliable_frame(&offer)?)?;

    let host_offer = read_control(&mut stream)?;
    let host_capabilities = match host_offer.body {
        Some(envelope::Body::Av1CapabilityOffer(offer))
            if offer.decoder_capabilities.is_empty() && offer.viewer_settings.is_none() =>
        {
            offer
                .encoder_capabilities
                .iter()
                .map(domain_capability)
                .collect::<Result<Vec<_>, _>>()?
        }
        _ => return Err("host sent an invalid AV1 capability offer".into()),
    };
    let selected = negotiate_av1_configuration(&host_capabilities, &[local_capability], &settings)?;
    let selected_wire = wire_selected(selected);
    write_all_frame(
        &mut stream,
        &wire::encode_reliable_frame(&Envelope {
            body: Some(envelope::Body::SelectedAv1Configuration(selected_wire)),
        })?,
    )?;

    let host_selected = read_control(&mut stream)?;
    match host_selected.body {
        Some(envelope::Body::SelectedAv1Configuration(configuration))
            if configuration == selected_wire => {}
        _ => return Err("host selected a different AV1 configuration".into()),
    }

    let packet_frame = read_frame(&mut stream, wire::MAX_ENCODED_VIDEO_PACKET_SIZE)?;
    let packet: EncodedVideoPacket = wire::decode_video_packet_frame(&packet_frame)?;
    if !packet.keyframe || packet.payload.is_empty() {
        return Err("host packet is not a non-empty AV1 keyframe".into());
    }
    let decode_device = HardwareDevice::open(HardwareDeviceType::VaApi, Some(VAAPI_DEVICE))?;
    let mut decoder = Av1VaApiDecoder::open(&decode_device)?;
    let decoded = decoder.decode_one_packet(&packet.payload)?;
    if decoded.width() != WIDTH || decoded.height() != HEIGHT {
        return Err("decoded Windows frame has unexpected dimensions".into());
    }
    let nv12 = decoded.download_nv12()?;
    let checksum = nv12
        .y_plane
        .iter()
        .chain(&nv12.uv_plane)
        .fold(0_u64, |sum, byte| sum.wrapping_add(u64::from(*byte)));
    let image_path = report_path.with_extension("pgm");
    let mut image = format!("P5\n{} {}\n255\n", nv12.width, nv12.height).into_bytes();
    image.extend_from_slice(&nv12.y_plane);
    fs::write(&image_path, image)?;
    fs::write(
        report_path,
        format!(
            "status=ok\ntransport=tcp-proof-only\nconfiguration=2560x1440@120-yuv420-8bit\npacket_bytes={}\nsequence={}\ncaptured_at_micros={}\nkeyframe={}\ndecoder_frame=vaapi\ndecoded_width={}\ndecoded_height={}\nnv12_checksum={}\nluma_image={}\n",
            packet.payload.len(),
            packet.sequence,
            packet.captured_at_micros,
            packet.keyframe,
            nv12.width,
            nv12.height,
            checksum,
            image_path.display(),
        ),
    )?;
    Ok(())
}

pub fn run_dma_buf_proof(report_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    run_dma_buf_packet_proof(CAPABILITY_FIXTURE, report_path)
}

pub fn run_dma_buf_packet_proof(
    packet: &[u8],
    report_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let device = HardwareDevice::open(HardwareDeviceType::VaApi, Some(VAAPI_DEVICE))?;
    let mut decoder = Av1VaApiDecoder::open(&device)?;
    let decoded = decoder.decode_one_packet(packet)?;
    let mapped = decoded.map_dma_buf()?;
    let egl_import_time = renderer_benchmark::benchmark_egl_import(&mapped, 1_000)?;
    let vulkan_import_time = renderer_benchmark::benchmark_vulkan_import(&mapped, 1_000)?;
    let object_summary = mapped
        .objects()
        .iter()
        .enumerate()
        .map(|(index, object)| {
            format!(
                "object_{index}=size:{},modifier:{:#x}",
                object.size, object.format_modifier
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let layer_summary = mapped
        .layers()
        .iter()
        .enumerate()
        .map(|(index, layer)| {
            format!(
                "layer_{index}=format:{:#x},planes:{}",
                layer.format,
                layer.planes.len()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        report_path,
        format!(
            "status=ok\nwidth={}\nheight={}\nobjects={}\nlayers={}\nimport_repetitions=1000\negl_import_total_micros={}\nvulkan_import_total_micros={}\n{object_summary}\n{layer_summary}\n",
            mapped.width(),
            mapped.height(),
            mapped.objects().len(),
            mapped.layers().len(),
            egl_import_time.as_micros(),
            vulkan_import_time.as_micros(),
        ),
    )?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn decode_fixture_dma_buf() -> Result<NativeDmaBufFrame, Box<dyn std::error::Error>> {
    let device = HardwareDevice::open(HardwareDeviceType::VaApi, Some(VAAPI_DEVICE))?;
    let mut decoder = Av1VaApiDecoder::open(&device)?;
    let decoded = decoder.decode_one_packet(CAPABILITY_FIXTURE)?;
    Ok(decoded.map_dma_buf()?)
}

fn wire_capability(capability: DomainCapability) -> Av1HardwareCapability {
    Av1HardwareCapability {
        chroma_subsampling: ChromaSubsampling::Yuv420 as i32,
        bit_depth: VideoBitDepth::Eight as i32,
        maximum_width: capability.maximum_width,
        maximum_height: capability.maximum_height,
        maximum_frames_per_second: u32::from(capability.maximum_frames_per_second),
    }
}

fn domain_capability(
    capability: &Av1HardwareCapability,
) -> Result<DomainCapability, Box<dyn std::error::Error>> {
    if ChromaSubsampling::try_from(capability.chroma_subsampling) != Ok(ChromaSubsampling::Yuv420)
        || VideoBitDepth::try_from(capability.bit_depth) != Ok(VideoBitDepth::Eight)
    {
        return Err("host advertised an unsupported AV1 mode".into());
    }
    Ok(DomainCapability {
        mode: DomainMode {
            chroma_subsampling: DomainChroma::Yuv420,
            bit_depth: DomainDepth::Eight,
        },
        maximum_width: capability.maximum_width,
        maximum_height: capability.maximum_height,
        maximum_frames_per_second: u16::try_from(capability.maximum_frames_per_second)?,
    })
}

fn wire_settings(settings: &DomainSettings) -> Av1ViewerSettings {
    Av1ViewerSettings {
        width: settings.width,
        height: settings.height,
        frames_per_second: u32::from(settings.frames_per_second),
        mode_preferences: vec![Av1Mode {
            chroma_subsampling: ChromaSubsampling::Yuv420 as i32,
            bit_depth: VideoBitDepth::Eight as i32,
        }],
        maximum_bitrate_bits_per_second: settings.maximum_bitrate_bits_per_second,
    }
}

fn wire_selected(
    selected: rustconsole_protocol::NegotiatedAv1Configuration,
) -> SelectedAv1Configuration {
    SelectedAv1Configuration {
        dedicated_input_stream: false,
        host_pointer_release: false,
        full_diagnostics: false,
        audio_transport: None,
        width: selected.width,
        height: selected.height,
        frames_per_second: u32::from(selected.frames_per_second),
        mode: Some(Av1Mode {
            chroma_subsampling: ChromaSubsampling::Yuv420 as i32,
            bit_depth: VideoBitDepth::Eight as i32,
        }),
        maximum_bitrate_bits_per_second: selected.maximum_bitrate_bits_per_second,
    }
}

fn read_control(stream: &mut TcpStream) -> Result<Envelope, Box<dyn std::error::Error>> {
    let frame = read_frame(stream, wire::MAX_RELIABLE_MESSAGE_SIZE)?;
    Ok(wire::decode_reliable_frame(&frame)?)
}

fn read_frame(
    stream: &mut TcpStream,
    maximum: usize,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut prefix = [0_u8; wire::RELIABLE_FRAME_PREFIX_SIZE];
    stream.read_exact(&mut prefix)?;
    let size = u32::from_be_bytes(prefix) as usize;
    if size > maximum {
        return Err(format!("TCP frame declares {size} bytes; maximum is {maximum}").into());
    }
    let mut frame = Vec::with_capacity(prefix.len() + size);
    frame.extend_from_slice(&prefix);
    frame.resize(prefix.len() + size, 0);
    stream.read_exact(&mut frame[prefix.len()..])?;
    Ok(frame)
}

fn write_all_frame(stream: &mut TcpStream, frame: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    stream.write_all(frame)?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustconsole_codec_ffmpeg::opus::{OpusEncoder, OpusEncoderConfiguration};

    fn encoded_packet(generation: u64, sequence: u64) -> rustconsole_protocol::audio::AudioPacket {
        let mut encoder = OpusEncoder::open(OpusEncoderConfiguration {
            bitrate_bits_per_second: 128_000,
            packet_duration_micros: 10_000,
        })
        .unwrap();
        let samples = (0..480)
            .flat_map(|frame| {
                let value = (frame as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.1;
                [value, value]
            })
            .collect();
        let packet = encoder.encode(1_000_000, samples).unwrap().remove(0);
        rustconsole_protocol::audio::AudioPacket {
            generation,
            sequence,
            captured_at_micros: packet.captured_at_micros,
            decoded_samples: packet.decoded_samples,
            skip_start_samples: packet.skip_start_samples,
            skip_end_samples: packet.skip_end_samples,
            payload: packet.payload,
        }
    }

    #[test]
    fn stream_decoder_emits_reset_and_real_opus_samples() {
        let mut decoder = StreamAudioDecoder {
            diagnostics: true,
            ..StreamAudioDecoder::default()
        };
        let events = decoder.event(AudioPlaybackEvent::Packet {
            packet: encoded_packet(1, 0),
            assembled_at: Instant::now(),
            released_at: Instant::now(),
            assembled_at_micros: 10,
            assembled_payload_sha256: None,
        });
        assert!(matches!(
            events.first(),
            Some(DecodedAudioEvent::Reset { generation: 1 })
        ));
        let DecodedAudioEvent::Samples(samples) = &events[1] else {
            panic!("expected decoded samples");
        };
        assert_eq!(samples.samples.format.sample_rate, 48_000);
        assert_eq!(samples.samples.format.channels, 2);
        assert_eq!(samples.samples.interleaved.len(), 336);
        assert!(samples.ordered_playout_duration.is_some());
        assert!(samples.decoder_queue_duration.is_some());
        assert!(
            samples
                .samples
                .interleaved
                .iter()
                .any(|sample| *sample != 0.0)
        );
    }

    #[test]
    fn stream_decoder_bounds_missing_audio_and_contains_codec_failure() {
        let mut decoder = StreamAudioDecoder::default();
        let missing = decoder.event(AudioPlaybackEvent::Missing {
            generation: 2,
            sequence: 4,
            captured_at_micros: 20_000,
            missing_packets: 20,
        });
        let DecodedAudioEvent::Samples(silence) = &missing[1] else {
            panic!("expected bounded silence");
        };
        assert_eq!(silence.samples.interleaved.len(), 4 * 480 * 2);
        assert!(
            silence
                .samples
                .interleaved
                .iter()
                .all(|sample| *sample == 0.0)
        );

        let mut invalid = encoded_packet(3, 0);
        invalid.payload.clear();
        let failed = decoder.event(AudioPlaybackEvent::Packet {
            packet: invalid,
            assembled_at: Instant::now(),
            released_at: Instant::now(),
            assembled_at_micros: 20,
            assembled_payload_sha256: None,
        });
        assert!(matches!(
            failed.last(),
            Some(DecodedAudioEvent::Failed { generation: 3, .. })
        ));
    }

    #[test]
    fn diagnostic_marker_requires_anchors_and_a_binary_state() {
        assert_eq!(diagnostic_marker_state(&[255, 0, 255], 8), Some(true));
        assert_eq!(diagnostic_marker_state(&[940, 64, 64], 10), Some(false));
        assert_eq!(diagnostic_marker_state(&[255, 255, 0], 8), None);
        assert_eq!(diagnostic_marker_state(&[255, 0, 128], 8), None);
        assert_eq!(diagnostic_marker_state(&[255, 0], 8), None);
    }

    #[test]
    fn diagnostic_marker_probe_waits_for_the_applied_input_sequence() {
        let mut last_probe_sequence = 0;
        let mut active_probe_sequence = 0;
        let mut probe_attempts = 0;
        assert!(take_diagnostic_marker_probe(
            true,
            false,
            0,
            0,
            &mut last_probe_sequence,
            &mut active_probe_sequence,
            &mut probe_attempts,
        ));
        assert!(!take_diagnostic_marker_probe(
            true,
            true,
            6,
            7,
            &mut last_probe_sequence,
            &mut active_probe_sequence,
            &mut probe_attempts,
        ));
        assert!(take_diagnostic_marker_probe(
            true,
            true,
            7,
            7,
            &mut last_probe_sequence,
            &mut active_probe_sequence,
            &mut probe_attempts,
        ));
        assert!(take_diagnostic_marker_probe(
            true,
            true,
            8,
            7,
            &mut last_probe_sequence,
            &mut active_probe_sequence,
            &mut probe_attempts,
        ));
        last_probe_sequence = 7;
        assert!(!take_diagnostic_marker_probe(
            true,
            true,
            9,
            7,
            &mut last_probe_sequence,
            &mut active_probe_sequence,
            &mut probe_attempts,
        ));
    }
}
