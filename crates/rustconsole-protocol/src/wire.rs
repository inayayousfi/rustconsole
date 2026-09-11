//! Bounded Protobuf messages used on reliable QUIC streams.

use prost::Message;
use std::fmt;

/// Maximum encoded Protobuf payload accepted from one reliable frame.
pub const MAX_RELIABLE_MESSAGE_SIZE: usize = 64 * 1024;

/// Maximum encoded AV1 packet accepted by the one-frame transport proof.
pub const MAX_ENCODED_VIDEO_PACKET_SIZE: usize = 16 * 1024 * 1024;

/// Number of bytes in the big-endian reliable-frame length prefix.
pub const RELIABLE_FRAME_PREFIX_SIZE: usize = size_of::<u32>();

#[derive(Clone, PartialEq, Message)]
pub struct Envelope {
    #[prost(
        oneof = "envelope::Body",
        tags = "1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21"
    )]
    pub body: Option<envelope::Body>,
}

pub mod envelope {
    use super::{
        AuthenticationResult, Av1CapabilityOffer, HostIdentityOffer, HostIdentityRequest,
        OpaqueCredentialFinalization, OpaqueCredentialRequest, OpaqueCredentialResponse,
        SelectedAv1Configuration, SessionAvailabilityProbe, SessionAvailabilityResult,
        VersionOffer, VideoControl, VideoReceiverReport,
    };
    use prost::Oneof;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Body {
        #[prost(message, tag = "14")]
        AudioStreamState(super::AudioStreamState),
        #[prost(message, tag = "15")]
        InputPack(super::InputPack),
        #[prost(message, tag = "16")]
        InputAck(super::InputAck),
        #[prost(message, tag = "17")]
        KeyboardLeds(super::KeyboardLeds),
        #[prost(message, tag = "18")]
        VideoStreamState(super::VideoStreamState),
        #[prost(message, tag = "19")]
        ClockPing(super::ClockPing),
        #[prost(message, tag = "20")]
        ClockPong(super::ClockPong),
        #[prost(message, tag = "21")]
        HostSessionControl(super::HostSessionControl),
        #[prost(message, tag = "1")]
        VersionOffer(VersionOffer),
        #[prost(message, tag = "2")]
        Av1CapabilityOffer(Av1CapabilityOffer),
        #[prost(message, tag = "3")]
        SelectedAv1Configuration(SelectedAv1Configuration),
        #[prost(message, tag = "4")]
        OpaqueCredentialRequest(OpaqueCredentialRequest),
        #[prost(message, tag = "5")]
        OpaqueCredentialResponse(OpaqueCredentialResponse),
        #[prost(message, tag = "6")]
        OpaqueCredentialFinalization(OpaqueCredentialFinalization),
        #[prost(message, tag = "7")]
        AuthenticationResult(AuthenticationResult),
        #[prost(message, tag = "8")]
        VideoControl(VideoControl),
        #[prost(message, tag = "9")]
        VideoReceiverReport(VideoReceiverReport),
        #[prost(message, tag = "10")]
        HostIdentityOffer(HostIdentityOffer),
        #[prost(message, tag = "11")]
        HostIdentityRequest(HostIdentityRequest),
        #[prost(message, tag = "12")]
        SessionAvailabilityProbe(SessionAvailabilityProbe),
        #[prost(message, tag = "13")]
        SessionAvailabilityResult(SessionAvailabilityResult),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct InputTransition {
    #[prost(uint64, tag = "1")]
    pub generation: u64,
    #[prost(uint64, tag = "2")]
    pub sequence: u64,
    #[prost(oneof = "input_transition::Action", tags = "3, 4, 5, 6, 7, 9, 10")]
    pub action: Option<input_transition::Action>,
    #[prost(uint64, tag = "8")]
    pub player_sent_at_micros: u64,
}

pub mod input_transition {
    use prost::Oneof;

    #[derive(Clone, Copy, PartialEq, Oneof)]
    pub enum Action {
        #[prost(message, tag = "3")]
        Key(super::KeyTransition),
        #[prost(message, tag = "4")]
        PointerButton(super::PointerButtonTransition),
        #[prost(message, tag = "5")]
        Wheel(super::WheelTransition),
        #[prost(message, tag = "6")]
        PointerMode(super::PointerModeTransition),
        #[prost(message, tag = "7")]
        ReleaseAll(super::ReleaseAll),
        #[prost(message, tag = "9")]
        PointerMotion(super::PointerMotionTransition),
        #[prost(message, tag = "10")]
        PointerPosition(super::PointerPositionTransition),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct InputPack {
    #[prost(message, repeated, tag = "1")]
    pub transitions: Vec<InputTransition>,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct KeyTransition {
    #[prost(uint32, tag = "1")]
    pub hid_usage: u32,
    #[prost(bool, tag = "2")]
    pub pressed: bool,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct PointerButtonTransition {
    #[prost(uint32, tag = "1")]
    pub button: u32,
    #[prost(bool, tag = "2")]
    pub pressed: bool,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct PointerMotionTransition {
    #[prost(sint32, tag = "1")]
    pub delta_x: i32,
    #[prost(sint32, tag = "2")]
    pub delta_y: i32,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct PointerPositionTransition {
    #[prost(uint32, tag = "1")]
    pub x: u32,
    #[prost(uint32, tag = "2")]
    pub y: u32,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct WheelTransition {
    #[prost(sint32, tag = "1")]
    pub horizontal: i32,
    #[prost(sint32, tag = "2")]
    pub vertical: i32,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct PointerModeTransition {
    #[prost(enumeration = "PointerMode", tag = "1")]
    pub mode: i32,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct ReleaseAll {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, prost::Enumeration)]
#[repr(i32)]
pub enum PointerMode {
    Unspecified = 0,
    Absolute = 1,
    Relative = 2,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct InputAck {
    #[prost(uint64, tag = "1")]
    pub generation: u64,
    #[prost(uint64, tag = "2")]
    pub through_sequence: u64,
    #[prost(uint64, tag = "3")]
    pub player_sent_at_micros: u64,
    #[prost(uint64, tag = "4")]
    pub host_received_at_micros: u64,
    #[prost(uint64, tag = "5")]
    pub host_submitted_at_micros: u64,
    #[prost(uint64, tag = "6")]
    pub pointer_datagrams_received: u64,
    #[prost(uint64, tag = "7")]
    pub pointer_updates_applied: u64,
    #[prost(uint64, tag = "8")]
    pub pointer_updates_ignored: u64,
    #[prost(uint64, tag = "9")]
    pub mouse_reports_published: u64,
    #[prost(uint64, tag = "10")]
    pub keyboard_reports_published: u64,
    #[prost(uint64, tag = "11")]
    pub reliable_transitions_received: u64,
    #[prost(uint64, tag = "12")]
    pub reliable_transitions_applied: u64,
    #[prost(uint64, tag = "13")]
    pub reliable_transitions_rejected: u64,
    #[prost(uint64, tag = "14")]
    pub reliable_transitions_missing: u64,
    #[prost(uint64, tag = "15")]
    pub reliable_transitions_duplicate_or_late: u64,
    #[prost(uint64, tag = "16")]
    pub release_all_transitions: u64,
    #[prost(uint64, tag = "17")]
    pub pointer_missing_datagrams: u64,
    #[prost(uint64, tag = "18")]
    pub pointer_stale_generations: u64,
    #[prost(uint64, tag = "19")]
    pub pointer_duplicate_or_late: u64,
    #[prost(uint64, tag = "20")]
    pub pointer_mode_rejections: u64,
    #[prost(uint64, tag = "21")]
    pub pointer_relative_baselines: u64,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct ClockPing {
    #[prost(uint64, tag = "1")]
    pub sequence: u64,
    #[prost(uint64, tag = "2")]
    pub player_sent_at_micros: u64,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct ClockPong {
    #[prost(uint64, tag = "1")]
    pub sequence: u64,
    #[prost(uint64, tag = "2")]
    pub player_sent_at_micros: u64,
    #[prost(uint64, tag = "3")]
    pub host_received_at_micros: u64,
    #[prost(uint64, tag = "4")]
    pub host_sent_at_micros: u64,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct KeyboardLeds {
    #[prost(uint64, tag = "1")]
    pub generation: u64,
    #[prost(uint64, tag = "2")]
    pub sequence: u64,
    #[prost(uint32, tag = "3")]
    pub mask: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct OpaqueCredentialRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub credential_identifier: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub message: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct OpaqueCredentialResponse {
    #[prost(bytes = "vec", tag = "1")]
    pub message: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct OpaqueCredentialFinalization {
    #[prost(bytes = "vec", tag = "1")]
    pub message: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AuthenticationResult {
    #[prost(bool, tag = "1")]
    pub accepted: bool,
    #[prost(bytes = "vec", tag = "2")]
    pub host_identity: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct HostIdentityOffer {
    #[prost(bytes = "vec", tag = "1")]
    pub host_identity: Vec<u8>,
    #[prost(string, tag = "2")]
    pub display_name: String,
    #[prost(enumeration = "HostOperatingSystem", tag = "3")]
    pub operating_system: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, prost::Enumeration)]
#[repr(i32)]
pub enum HostOperatingSystem {
    Unknown = 0,
    Windows = 1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, prost::Enumeration)]
#[repr(i32)]
pub enum HostFirewallStatus {
    Unknown = 0,
    Missing = 1,
    PrivateLocalSubnet = 2,
    AllProfilesLocalSubnet = 3,
    AllAddresses = 4,
    CheckFailed = 5,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct HostIdentityRequest {}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct SessionAvailabilityProbe {}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct SessionAvailabilityResult {
    #[prost(enumeration = "SessionAvailability", tag = "1")]
    pub availability: i32,
    #[prost(enumeration = "VbCableStatus", tag = "2")]
    pub vb_cable_status: i32,
    #[prost(enumeration = "HostFirewallStatus", tag = "3")]
    pub firewall_status: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, prost::Enumeration)]
#[repr(i32)]
pub enum SessionAvailability {
    Unspecified = 0,
    Available = 1,
    Busy = 2,
    DesktopSessionUnavailable = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, prost::Enumeration)]
#[repr(i32)]
pub enum VbCableStatus {
    Unspecified = 0,
    Ready = 1,
    Unavailable = 2,
    CheckFailed = 3,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct VideoControl {
    #[prost(enumeration = "VideoControlKind", tag = "1")]
    pub kind: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, prost::Enumeration)]
#[repr(i32)]
pub enum VideoControlKind {
    Unspecified = 0,
    RequestKeyframe = 1,
    Stop = 2,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct VideoStreamState {
    #[prost(enumeration = "VideoStreamStatus", tag = "1")]
    pub status: i32,
    #[prost(enumeration = "VideoReconfigurationCause", tag = "2")]
    pub cause: i32,
}

impl VideoStreamState {
    #[must_use]
    pub fn reconfiguration_required(cause: VideoReconfigurationCause) -> Self {
        Self {
            status: VideoStreamStatus::ReconfigurationRequired as i32,
            cause: cause as i32,
        }
    }

    #[must_use]
    pub fn reconfiguration_cause(self) -> Option<VideoReconfigurationCause> {
        (VideoStreamStatus::try_from(self.status) == Ok(VideoStreamStatus::ReconfigurationRequired))
            .then(|| VideoReconfigurationCause::try_from(self.cause).ok())
            .flatten()
            .filter(|cause| *cause != VideoReconfigurationCause::Unspecified)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, prost::Enumeration)]
#[repr(i32)]
pub enum VideoStreamStatus {
    Unspecified = 0,
    ReconfigurationRequired = 1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, prost::Enumeration)]
#[repr(i32)]
pub enum VideoReconfigurationCause {
    Unspecified = 0,
    CaptureEngine = 1,
    Dimensions = 2,
    RefreshRate = 3,
    PixelFormat = 4,
    Color = 5,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct VideoReceiverReport {
    #[prost(uint64, tag = "1")]
    pub newest_sequence: u64,
    #[prost(uint64, tag = "2")]
    pub received_chunks: u64,
    #[prost(uint64, tag = "3")]
    pub lost_chunks: u64,
    #[prost(uint64, tag = "4")]
    pub late_chunks: u64,
    #[prost(uint64, tag = "5")]
    pub assembly_overflows: u64,
    #[prost(uint64, tag = "6")]
    pub completed_frames: u64,
    #[prost(uint64, tag = "7")]
    pub incomplete_frames: u64,
    #[prost(uint64, tag = "8")]
    pub last_completed_assembly_micros: u64,
    #[prost(uint64, tag = "9")]
    pub last_assembly_budget_micros: u64,
    #[prost(uint64, tag = "10")]
    pub completed_payload_bytes: u64,
    #[prost(uint64, tag = "11")]
    pub measurement_interval_micros: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Av1CapabilityOffer {
    #[prost(message, repeated, tag = "1")]
    pub encoder_capabilities: Vec<Av1HardwareCapability>,
    #[prost(message, repeated, tag = "2")]
    pub decoder_capabilities: Vec<Av1HardwareCapability>,
    #[prost(message, optional, tag = "3")]
    pub viewer_settings: Option<Av1ViewerSettings>,
    #[prost(message, optional, tag = "4")]
    pub audio_transport: Option<AudioConfiguration>,
    #[prost(bool, tag = "5")]
    pub full_diagnostics: bool,
    #[prost(bool, tag = "6")]
    pub host_pointer_release: bool,
    #[prost(bool, tag = "7")]
    pub dedicated_input_stream: bool,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct Av1HardwareCapability {
    #[prost(enumeration = "ChromaSubsampling", tag = "1")]
    pub chroma_subsampling: i32,
    #[prost(enumeration = "VideoBitDepth", tag = "2")]
    pub bit_depth: i32,
    #[prost(uint32, tag = "3")]
    pub maximum_width: u32,
    #[prost(uint32, tag = "4")]
    pub maximum_height: u32,
    #[prost(uint32, tag = "5")]
    pub maximum_frames_per_second: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Av1ViewerSettings {
    #[prost(uint32, tag = "1")]
    pub width: u32,
    #[prost(uint32, tag = "2")]
    pub height: u32,
    #[prost(uint32, tag = "3")]
    pub frames_per_second: u32,
    #[prost(message, repeated, tag = "4")]
    pub mode_preferences: Vec<Av1Mode>,
    #[prost(uint64, tag = "5")]
    pub maximum_bitrate_bits_per_second: u64,
    // Field 6 was the minimum bitrate and must not be reused.
}

#[derive(Clone, Copy, Eq, PartialEq, Message)]
pub struct AudioConfiguration {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(uint32, tag = "2")]
    pub codec: u32,
    #[prost(uint32, tag = "3")]
    pub sample_rate: u32,
    #[prost(uint32, tag = "4")]
    pub channels: u32,
    #[prost(uint32, tag = "5")]
    pub packet_duration_micros: u32,
    #[prost(uint32, tag = "6")]
    pub bitrate_bits_per_second: u32,
}

impl AudioConfiguration {
    pub const INITIAL: Self = Self {
        version: 1,
        codec: 1,
        sample_rate: 48_000,
        channels: 2,
        packet_duration_micros: 10_000,
        bitrate_bits_per_second: 128_000,
    };

    pub fn supported(self) -> bool {
        self == Self::INITIAL
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct AudioStreamState {
    #[prost(uint64, tag = "1")]
    pub generation: u64,
    #[prost(enumeration = "AudioStatus", tag = "2")]
    pub status: i32,
    #[prost(uint64, tag = "3")]
    pub dropped_packets: u64,
    #[prost(string, tag = "4")]
    pub detail: String,
}

impl AudioStreamState {
    pub fn new(
        generation: u64,
        status: AudioStatus,
        dropped_packets: u64,
        mut detail: String,
    ) -> Self {
        let mut end = detail.len().min(256);
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
        Self {
            generation,
            status: status as i32,
            dropped_packets,
            detail,
        }
    }

    pub fn valid(&self) -> bool {
        self.detail.len() <= 256 && AudioStatus::try_from(self.status).is_ok()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, prost::Enumeration)]
#[repr(i32)]
pub enum AudioStatus {
    NotNegotiated = 0,
    Waiting = 1,
    Active = 2,
    Unavailable = 3,
    Failed = 4,
    Stopped = 5,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct Av1Mode {
    #[prost(enumeration = "ChromaSubsampling", tag = "1")]
    pub chroma_subsampling: i32,
    #[prost(enumeration = "VideoBitDepth", tag = "2")]
    pub bit_depth: i32,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct SelectedAv1Configuration {
    #[prost(uint32, tag = "1")]
    pub width: u32,
    #[prost(uint32, tag = "2")]
    pub height: u32,
    #[prost(uint32, tag = "3")]
    pub frames_per_second: u32,
    #[prost(message, optional, tag = "4")]
    pub mode: Option<Av1Mode>,
    #[prost(uint64, tag = "5")]
    pub maximum_bitrate_bits_per_second: u64,
    // Field 6 was the minimum bitrate and must not be reused.
    #[prost(message, optional, tag = "7")]
    pub audio_transport: Option<AudioConfiguration>,
    #[prost(bool, tag = "8")]
    pub full_diagnostics: bool,
    #[prost(bool, tag = "9")]
    pub host_pointer_release: bool,
    #[prost(bool, tag = "10")]
    pub dedicated_input_stream: bool,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct HostSessionControl {
    #[prost(enumeration = "HostSessionControlKind", tag = "1")]
    pub kind: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, prost::Enumeration)]
#[repr(i32)]
pub enum HostSessionControlKind {
    Unspecified = 0,
    ReleasePointerCapture = 1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, prost::Enumeration)]
#[repr(i32)]
pub enum ChromaSubsampling {
    Unspecified = 0,
    Yuv420 = 1,
    Yuv422 = 2,
    Yuv444 = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, prost::Enumeration)]
#[repr(i32)]
pub enum VideoBitDepth {
    Unspecified = 0,
    Eight = 1,
    Ten = 2,
}

#[derive(Clone, PartialEq, Message)]
pub struct EncodedVideoPacket {
    #[prost(uint64, tag = "1")]
    pub sequence: u64,
    #[prost(uint64, tag = "2")]
    pub captured_at_micros: u64,
    #[prost(bool, tag = "3")]
    pub keyframe: bool,
    #[prost(bytes = "vec", tag = "4")]
    pub payload: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct VersionOffer {
    #[prost(uint32, tag = "1")]
    pub protocol_major: u32,
    #[prost(uint32, tag = "2")]
    pub protocol_minor: u32,
    #[prost(message, repeated, tag = "3")]
    pub features: Vec<FeatureOffer>,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct FeatureOffer {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(uint32, tag = "2")]
    pub oldest_version: u32,
    #[prost(uint32, tag = "3")]
    pub newest_version: u32,
    #[prost(bool, tag = "4")]
    pub required: bool,
}

#[derive(Debug)]
pub enum ReliableFrameError {
    PayloadTooLarge { size: usize, maximum: usize },
    TruncatedPrefix { size: usize },
    LengthMismatch { declared: usize, actual: usize },
    Encode(prost::EncodeError),
    Decode(prost::DecodeError),
}

impl fmt::Display for ReliableFrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadTooLarge { size, maximum } => {
                write!(
                    formatter,
                    "reliable payload is {size} bytes; maximum is {maximum}"
                )
            }
            Self::TruncatedPrefix { size } => write!(
                formatter,
                "reliable frame has {size} prefix bytes; expected {RELIABLE_FRAME_PREFIX_SIZE}"
            ),
            Self::LengthMismatch { declared, actual } => write!(
                formatter,
                "reliable frame declares {declared} payload bytes but contains {actual}"
            ),
            Self::Encode(error) => write!(formatter, "failed to encode reliable payload: {error}"),
            Self::Decode(error) => write!(formatter, "failed to decode reliable payload: {error}"),
        }
    }
}

impl std::error::Error for ReliableFrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Encode(error) => Some(error),
            Self::Decode(error) => Some(error),
            _ => None,
        }
    }
}

/// Encodes one complete reliable frame with its bounded length prefix.
pub fn encode_reliable_frame(envelope: &Envelope) -> Result<Vec<u8>, ReliableFrameError> {
    let payload_size = envelope.encoded_len();
    if payload_size > MAX_RELIABLE_MESSAGE_SIZE {
        return Err(ReliableFrameError::PayloadTooLarge {
            size: payload_size,
            maximum: MAX_RELIABLE_MESSAGE_SIZE,
        });
    }

    let mut frame = Vec::with_capacity(RELIABLE_FRAME_PREFIX_SIZE + payload_size);
    frame.extend_from_slice(&(payload_size as u32).to_be_bytes());
    envelope
        .encode(&mut frame)
        .map_err(ReliableFrameError::Encode)?;
    Ok(frame)
}

/// Decodes one complete reliable frame after enforcing its outer size bound.
pub fn decode_reliable_frame(frame: &[u8]) -> Result<Envelope, ReliableFrameError> {
    let prefix: [u8; RELIABLE_FRAME_PREFIX_SIZE] = frame
        .get(..RELIABLE_FRAME_PREFIX_SIZE)
        .ok_or(ReliableFrameError::TruncatedPrefix { size: frame.len() })?
        .try_into()
        .expect("slice length was checked");
    let declared = u32::from_be_bytes(prefix) as usize;

    if declared > MAX_RELIABLE_MESSAGE_SIZE {
        return Err(ReliableFrameError::PayloadTooLarge {
            size: declared,
            maximum: MAX_RELIABLE_MESSAGE_SIZE,
        });
    }

    let payload = &frame[RELIABLE_FRAME_PREFIX_SIZE..];
    if declared != payload.len() {
        return Err(ReliableFrameError::LengthMismatch {
            declared,
            actual: payload.len(),
        });
    }

    Envelope::decode(payload).map_err(ReliableFrameError::Decode)
}

pub fn encode_video_packet_frame(
    packet: &EncodedVideoPacket,
) -> Result<Vec<u8>, ReliableFrameError> {
    let payload_size = packet.encoded_len();
    if payload_size > MAX_ENCODED_VIDEO_PACKET_SIZE {
        return Err(ReliableFrameError::PayloadTooLarge {
            size: payload_size,
            maximum: MAX_ENCODED_VIDEO_PACKET_SIZE,
        });
    }
    let mut frame = Vec::with_capacity(RELIABLE_FRAME_PREFIX_SIZE + payload_size);
    frame.extend_from_slice(&(payload_size as u32).to_be_bytes());
    packet
        .encode(&mut frame)
        .map_err(ReliableFrameError::Encode)?;
    Ok(frame)
}

pub fn decode_video_packet_frame(frame: &[u8]) -> Result<EncodedVideoPacket, ReliableFrameError> {
    let prefix: [u8; RELIABLE_FRAME_PREFIX_SIZE] = frame
        .get(..RELIABLE_FRAME_PREFIX_SIZE)
        .ok_or(ReliableFrameError::TruncatedPrefix { size: frame.len() })?
        .try_into()
        .expect("slice length was checked");
    let declared = u32::from_be_bytes(prefix) as usize;
    if declared > MAX_ENCODED_VIDEO_PACKET_SIZE {
        return Err(ReliableFrameError::PayloadTooLarge {
            size: declared,
            maximum: MAX_ENCODED_VIDEO_PACKET_SIZE,
        });
    }
    let payload = &frame[RELIABLE_FRAME_PREFIX_SIZE..];
    if declared != payload.len() {
        return Err(ReliableFrameError::LengthMismatch {
            declared,
            actual: payload.len(),
        });
    }
    EncodedVideoPacket::decode(payload).map_err(ReliableFrameError::Decode)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERSION_OFFER_FIXTURE: [u8; 18] = [
        0x00, 0x00, 0x00, 0x0e, 0x0a, 0x0c, 0x08, 0x01, 0x1a, 0x08, 0x08, 0x07, 0x10, 0x01, 0x18,
        0x03, 0x20, 0x01,
    ];
    const AV1_VIEWER_SETTINGS_FIXTURE: [u8; 19] = [
        0x08, 0x80, 0x14, 0x10, 0xa0, 0x0b, 0x18, 0x78, 0x22, 0x04, 0x08, 0x01, 0x10, 0x01, 0x28,
        0x80, 0xc2, 0xd7, 0x2f,
    ];
    const SELECTED_AV1_CONFIGURATION_FIXTURE: [u8; 19] = AV1_VIEWER_SETTINGS_FIXTURE;

    fn version_offer() -> Envelope {
        Envelope {
            body: Some(envelope::Body::VersionOffer(VersionOffer {
                protocol_major: 1,
                protocol_minor: 0,
                features: vec![FeatureOffer {
                    id: 7,
                    oldest_version: 1,
                    newest_version: 3,
                    required: true,
                }],
            })),
        }
    }

    #[test]
    fn version_offer_matches_permanent_wire_fixture() {
        let encoded = encode_reliable_frame(&version_offer()).unwrap();

        assert_eq!(encoded, VERSION_OFFER_FIXTURE);
        assert_eq!(decode_reliable_frame(&encoded).unwrap(), version_offer());
    }

    #[test]
    fn maximum_only_av1_messages_match_permanent_wire_fixtures() {
        let mode = Av1Mode {
            chroma_subsampling: ChromaSubsampling::Yuv420 as i32,
            bit_depth: VideoBitDepth::Eight as i32,
        };
        let settings = Av1ViewerSettings {
            width: 2_560,
            height: 1_440,
            frames_per_second: 120,
            mode_preferences: vec![mode],
            maximum_bitrate_bits_per_second: 100_000_000,
        };
        let selected = SelectedAv1Configuration {
            dedicated_input_stream: false,
            host_pointer_release: false,
            full_diagnostics: false,
            audio_transport: None,
            width: 2_560,
            height: 1_440,
            frames_per_second: 120,
            mode: Some(mode),
            maximum_bitrate_bits_per_second: 100_000_000,
        };

        assert_eq!(settings.encode_to_vec(), AV1_VIEWER_SETTINGS_FIXTURE);
        assert_eq!(selected.encode_to_vec(), SELECTED_AV1_CONFIGURATION_FIXTURE);
        assert_eq!(
            Av1ViewerSettings::decode(AV1_VIEWER_SETTINGS_FIXTURE.as_slice()).unwrap(),
            settings
        );
        assert_eq!(
            SelectedAv1Configuration::decode(SELECTED_AV1_CONFIGURATION_FIXTURE.as_slice())
                .unwrap(),
            selected
        );
    }

    #[test]
    fn reliable_input_pack_preserves_order_and_pointer_motion() {
        let envelope = Envelope {
            body: Some(envelope::Body::InputPack(InputPack {
                transitions: vec![
                    InputTransition {
                        generation: 3,
                        sequence: 8,
                        action: Some(input_transition::Action::Key(KeyTransition {
                            hid_usage: 4,
                            pressed: true,
                        })),
                        player_sent_at_micros: 100,
                    },
                    InputTransition {
                        generation: 3,
                        sequence: 9,
                        action: Some(input_transition::Action::PointerMotion(
                            PointerMotionTransition {
                                delta_x: -5,
                                delta_y: 7,
                            },
                        )),
                        player_sent_at_micros: 100,
                    },
                ],
            })),
        };

        assert_eq!(
            decode_reliable_frame(&encode_reliable_frame(&envelope).unwrap()).unwrap(),
            envelope
        );
    }

    #[test]
    fn unknown_optional_protobuf_field_is_ignored() {
        let mut payload = VERSION_OFFER_FIXTURE[RELIABLE_FRAME_PREFIX_SIZE..].to_vec();
        payload.extend_from_slice(&[0x98, 0x06, 0x01]);
        let mut frame = Vec::from((payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);

        assert_eq!(decode_reliable_frame(&frame).unwrap(), version_offer());
    }

    #[test]
    fn oversized_declared_payload_is_rejected_before_decode() {
        let frame = ((MAX_RELIABLE_MESSAGE_SIZE + 1) as u32).to_be_bytes();

        assert!(matches!(
            decode_reliable_frame(&frame),
            Err(ReliableFrameError::PayloadTooLarge {
                size,
                maximum: MAX_RELIABLE_MESSAGE_SIZE,
            }) if size == MAX_RELIABLE_MESSAGE_SIZE + 1
        ));
    }

    #[test]
    fn truncated_length_prefix_is_rejected() {
        assert!(matches!(
            decode_reliable_frame(&[0, 0, 0]),
            Err(ReliableFrameError::TruncatedPrefix { size: 3 })
        ));
    }

    #[test]
    fn mismatched_payload_length_is_rejected() {
        let frame = [0, 0, 0, 2, 0];

        assert!(matches!(
            decode_reliable_frame(&frame),
            Err(ReliableFrameError::LengthMismatch {
                declared: 2,
                actual: 1,
            })
        ));
    }

    #[test]
    fn encoded_video_packet_has_an_independent_bound() {
        let packet = EncodedVideoPacket {
            sequence: 7,
            captured_at_micros: 42,
            keyframe: true,
            payload: vec![0x12; MAX_RELIABLE_MESSAGE_SIZE],
        };
        let frame = encode_video_packet_frame(&packet).unwrap();

        assert_eq!(decode_video_packet_frame(&frame).unwrap(), packet);
        assert!(matches!(
            encode_video_packet_frame(&EncodedVideoPacket {
                sequence: 0,
                captured_at_micros: 0,
                keyframe: false,
                payload: vec![0; MAX_ENCODED_VIDEO_PACKET_SIZE],
            }),
            Err(ReliableFrameError::PayloadTooLarge { maximum, .. })
                if maximum == MAX_ENCODED_VIDEO_PACKET_SIZE
        ));
    }

    #[test]
    fn opaque_messages_are_bounded_by_reliable_framing() {
        let request = Envelope {
            body: Some(envelope::Body::OpaqueCredentialRequest(
                OpaqueCredentialRequest {
                    credential_identifier: b"default".to_vec(),
                    message: vec![0x55; 512],
                },
            )),
        };

        let encoded = encode_reliable_frame(&request).unwrap();
        assert_eq!(decode_reliable_frame(&encoded).unwrap(), request);

        let oversized = Envelope {
            body: Some(envelope::Body::OpaqueCredentialResponse(
                OpaqueCredentialResponse {
                    message: vec![0; MAX_RELIABLE_MESSAGE_SIZE],
                },
            )),
        };
        assert!(matches!(
            encode_reliable_frame(&oversized),
            Err(ReliableFrameError::PayloadTooLarge { maximum, .. })
                if maximum == MAX_RELIABLE_MESSAGE_SIZE
        ));
    }

    #[test]
    fn video_control_and_receiver_report_round_trip() {
        for body in [
            envelope::Body::VideoControl(VideoControl {
                kind: VideoControlKind::RequestKeyframe as i32,
            }),
            envelope::Body::VideoReceiverReport(VideoReceiverReport {
                newest_sequence: 90,
                received_chunks: 400,
                lost_chunks: 3,
                late_chunks: 2,
                assembly_overflows: 1,
                completed_frames: 30,
                incomplete_frames: 2,
                last_completed_assembly_micros: 7_000,
                last_assembly_budget_micros: 25_000,
                completed_payload_bytes: 1_500_000,
                measurement_interval_micros: 500_000,
            }),
        ] {
            let envelope = Envelope { body: Some(body) };
            let encoded = encode_reliable_frame(&envelope).unwrap();
            assert_eq!(decode_reliable_frame(&encoded).unwrap(), envelope);
        }
    }

    #[test]
    fn video_reconfiguration_state_round_trips_and_requires_a_cause() {
        let state =
            VideoStreamState::reconfiguration_required(VideoReconfigurationCause::PixelFormat);
        let envelope = Envelope {
            body: Some(envelope::Body::VideoStreamState(state)),
        };
        let encoded = encode_reliable_frame(&envelope).unwrap();
        assert_eq!(decode_reliable_frame(&encoded).unwrap(), envelope);
        assert_eq!(
            state.reconfiguration_cause(),
            Some(VideoReconfigurationCause::PixelFormat)
        );
        assert_eq!(VideoStreamState::default().reconfiguration_cause(), None);
    }

    #[test]
    fn session_availability_messages_round_trip() {
        for body in [
            envelope::Body::SessionAvailabilityProbe(SessionAvailabilityProbe {}),
            envelope::Body::SessionAvailabilityResult(SessionAvailabilityResult {
                availability: SessionAvailability::DesktopSessionUnavailable as i32,
                vb_cable_status: VbCableStatus::Unavailable as i32,
                firewall_status: HostFirewallStatus::AllProfilesLocalSubnet as i32,
            }),
        ] {
            let envelope = Envelope { body: Some(body) };
            let encoded = encode_reliable_frame(&envelope).unwrap();
            assert_eq!(decode_reliable_frame(&encoded).unwrap(), envelope);
        }
    }

    #[test]
    fn older_availability_response_has_an_unspecified_vb_cable_status() {
        #[derive(Clone, PartialEq, Message)]
        struct OldSessionAvailabilityResult {
            #[prost(enumeration = "SessionAvailability", tag = "1")]
            availability: i32,
        }

        let old = OldSessionAvailabilityResult {
            availability: SessionAvailability::Available as i32,
        };
        let current = SessionAvailabilityResult::decode(old.encode_to_vec().as_slice()).unwrap();
        assert_eq!(current.availability, SessionAvailability::Available as i32);
        assert_eq!(current.vb_cable_status, VbCableStatus::Unspecified as i32);
        assert_eq!(current.firewall_status, HostFirewallStatus::Unknown as i32);
    }

    #[test]
    fn older_identity_offer_has_unknown_metadata() {
        #[derive(Clone, PartialEq, Message)]
        struct OldHostIdentityOffer {
            #[prost(bytes = "vec", tag = "1")]
            host_identity: Vec<u8>,
        }

        let identity = vec![0x5a; 32];
        let old = OldHostIdentityOffer {
            host_identity: identity.clone(),
        };
        let current = HostIdentityOffer::decode(old.encode_to_vec().as_slice()).unwrap();
        assert_eq!(current.host_identity, identity);
        assert!(current.display_name.is_empty());
        assert_eq!(
            current.operating_system,
            HostOperatingSystem::Unknown as i32
        );
    }
}
#[test]
fn audio_offer_has_a_fixed_fixture_and_is_optional_to_older_peers() {
    let offer = Av1CapabilityOffer {
        dedicated_input_stream: false,
        host_pointer_release: false,
        full_diagnostics: false,
        encoder_capabilities: Vec::new(),
        decoder_capabilities: Vec::new(),
        viewer_settings: None,
        audio_transport: Some(AudioConfiguration::INITIAL),
    };
    let frame = encode_reliable_frame(&Envelope {
        body: Some(envelope::Body::Av1CapabilityOffer(offer.clone())),
    })
    .unwrap();
    assert_eq!(
        frame,
        [
            0, 0, 0, 21, 18, 19, 34, 17, 8, 1, 16, 1, 24, 128, 247, 2, 32, 2, 40, 144, 78, 48, 128,
            232, 7
        ]
    );
    #[derive(Clone, PartialEq, Message)]
    struct OldOffer {
        #[prost(message, repeated, tag = "1")]
        encoder: Vec<Av1HardwareCapability>,
        #[prost(message, repeated, tag = "2")]
        decoder: Vec<Av1HardwareCapability>,
        #[prost(message, optional, tag = "3")]
        settings: Option<Av1ViewerSettings>,
    }
    let old = OldOffer::decode(offer.encode_to_vec().as_slice()).unwrap();
    assert!(old.encoder.is_empty() && old.decoder.is_empty() && old.settings.is_none());
    let old_bytes = old.encode_to_vec();
    assert_eq!(
        Av1CapabilityOffer::decode(old_bytes.as_slice())
            .unwrap()
            .audio_transport,
        None
    );
    assert!(AudioConfiguration::INITIAL.supported());
    assert!(
        !AudioConfiguration {
            version: 2,
            ..AudioConfiguration::INITIAL
        }
        .supported()
    );
}

#[test]
fn host_pointer_release_is_negotiated_as_an_optional_field() {
    let offer = Av1CapabilityOffer {
        dedicated_input_stream: false,
        host_pointer_release: true,
        full_diagnostics: false,
        encoder_capabilities: Vec::new(),
        decoder_capabilities: Vec::new(),
        viewer_settings: None,
        audio_transport: None,
    };
    assert!(
        Av1CapabilityOffer::decode(offer.encode_to_vec().as_slice())
            .unwrap()
            .host_pointer_release
    );

    #[derive(Clone, PartialEq, Message)]
    struct OldOffer {
        #[prost(message, repeated, tag = "1")]
        encoder: Vec<Av1HardwareCapability>,
    }
    let old = OldOffer::decode(offer.encode_to_vec().as_slice()).unwrap();
    let decoded = Av1CapabilityOffer::decode(old.encode_to_vec().as_slice()).unwrap();
    assert!(!decoded.host_pointer_release);

    let envelope = Envelope {
        body: Some(envelope::Body::HostSessionControl(HostSessionControl {
            kind: HostSessionControlKind::ReleasePointerCapture as i32,
        })),
    };
    let frame = encode_reliable_frame(&envelope).unwrap();
    assert_eq!(frame, [0, 0, 0, 5, 0xaa, 0x01, 0x02, 0x08, 0x01]);
    assert_eq!(decode_reliable_frame(&frame).unwrap(), envelope);
}

#[test]
fn dedicated_input_stream_is_negotiated_as_an_optional_field() {
    let offer = Av1CapabilityOffer {
        dedicated_input_stream: true,
        host_pointer_release: false,
        full_diagnostics: false,
        encoder_capabilities: Vec::new(),
        decoder_capabilities: Vec::new(),
        viewer_settings: None,
        audio_transport: None,
    };
    assert!(
        Av1CapabilityOffer::decode(offer.encode_to_vec().as_slice())
            .unwrap()
            .dedicated_input_stream
    );

    #[derive(Clone, PartialEq, Message)]
    struct OldOffer {
        #[prost(message, repeated, tag = "1")]
        encoder: Vec<Av1HardwareCapability>,
    }
    let old = OldOffer::decode(offer.encode_to_vec().as_slice()).unwrap();
    let decoded = Av1CapabilityOffer::decode(old.encode_to_vec().as_slice()).unwrap();
    assert!(!decoded.dedicated_input_stream);
}

#[test]
fn audio_state_bounds_unicode_and_rejects_unknown_status() {
    let mut state = AudioStreamState::new(7, AudioStatus::Failed, 2, "\u{20ac}".repeat(200));
    assert!(state.valid());
    assert!(state.detail.len() <= 256);
    let envelope = Envelope {
        body: Some(envelope::Body::AudioStreamState(state.clone())),
    };
    assert_eq!(
        decode_reliable_frame(&encode_reliable_frame(&envelope).unwrap()).unwrap(),
        envelope
    );
    state.status = 999;
    assert!(!state.valid());
}
