use std::io::{self, Read, Write};

pub const VERSION: u16 = 12;
const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
const COMMAND_CAPTURE_PROOF: u8 = 1;
const COMMAND_STOP: u8 = 2;
const COMMAND_DESKTOP_TRANSITION_PROOF: u8 = 3;
const COMMAND_LOGIN_TRANSITION_PROOF: u8 = 4;
const COMMAND_DISPLAY_MODE_TRANSITION_PROOF: u8 = 5;
const COMMAND_ENCODE_SNAPSHOT: u8 = 6;
const COMMAND_START_VIDEO_STREAM: u8 = 7;
const COMMAND_SET_VIDEO_BITRATE: u8 = 8;
const COMMAND_REQUEST_VIDEO_KEYFRAME: u8 = 9;
const COMMAND_STOP_VIDEO_STREAM: u8 = 10;
const COMMAND_SET_VIDEO_FRAME_DIVISOR: u8 = 11;
const COMMAND_AUDIO_PROOF: u8 = 12;
const COMMAND_AUDIO_ENCODE_PROOF: u8 = 13;
const COMMAND_PREPARE_VIDEO_STREAM: u8 = 14;
const EVENT_HELLO: u8 = 1;
const EVENT_CAPTURE_REPORT: u8 = 2;
const EVENT_PROOF_PROGRESS: u8 = 3;
const EVENT_ENCODED_SNAPSHOT: u8 = 4;
const EVENT_FAILURE: u8 = 5;
const EVENT_ENCODED_VIDEO_FRAME: u8 = 6;
const EVENT_VIDEO_CONFIGURATION: u8 = 7;
const EVENT_VIDEO_RECONFIGURATION_REQUIRED: u8 = 8;
const EVENT_SYSTEM_IDENTITY_REQUIRED: u8 = 9;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum WorkerIdentity {
    ActiveUser = 1,
    LocalSystem = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum WorkerCaptureEngine {
    WindowsGraphicsCapture = 1,
    DesktopDuplication = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum WorkerVideoFormat {
    Nv12 = 1,
    P010 = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum WorkerVideoColor {
    Bt709Limited = 1,
    Bt2020PqLimited = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerVideoConfiguration {
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
    pub capture_engine: WorkerCaptureEngine,
    pub format: WorkerVideoFormat,
    pub color: WorkerVideoColor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerCommand {
    AudioProof,
    AudioEncodeProof,
    CaptureProof,
    DesktopTransitionProof,
    LoginTransitionProof,
    DisplayModeTransitionProof,
    PrepareVideoStream,
    EncodeSnapshot {
        frames_per_second: u16,
        bitrate_bits_per_second: u64,
    },
    StartVideoStream {
        frames_per_second: u16,
        bitrate_bits_per_second: u64,
        audio: bool,
    },
    SetVideoBitrate(u64),
    SetVideoFrameDivisor(u8),
    RequestVideoKeyframe,
    StopVideoStream,
    Stop,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerEvent {
    Hello {
        version: u16,
        process_id: u32,
        session_id: u32,
        identity: WorkerIdentity,
        connection_token: [u8; 16],
    },
    CaptureReport(String),
    ProofProgress(String),
    EncodedSnapshot {
        last_present_time: i64,
        accumulated_frames: u32,
        protected_content_masked: bool,
        presentation_timestamp: i64,
        keyframe: bool,
        payload: Vec<u8>,
    },
    EncodedVideoFrame {
        sequence: u64,
        last_present_time: i64,
        accumulated_frames: u32,
        protected_content_masked: bool,
        presentation_timestamp: i64,
        keyframe: bool,
        payload: Vec<u8>,
    },
    VideoConfiguration(WorkerVideoConfiguration),
    VideoReconfigurationRequired(rustconsole_protocol::wire::VideoReconfigurationCause),
    SystemIdentityRequired,
    Failure(String),
}

pub fn write_command(writer: &mut impl Write, command: WorkerCommand) -> io::Result<()> {
    let payload = match command {
        WorkerCommand::AudioProof => vec![COMMAND_AUDIO_PROOF],
        WorkerCommand::AudioEncodeProof => vec![COMMAND_AUDIO_ENCODE_PROOF],
        WorkerCommand::CaptureProof => vec![COMMAND_CAPTURE_PROOF],
        WorkerCommand::DesktopTransitionProof => vec![COMMAND_DESKTOP_TRANSITION_PROOF],
        WorkerCommand::LoginTransitionProof => vec![COMMAND_LOGIN_TRANSITION_PROOF],
        WorkerCommand::DisplayModeTransitionProof => vec![COMMAND_DISPLAY_MODE_TRANSITION_PROOF],
        WorkerCommand::PrepareVideoStream => vec![COMMAND_PREPARE_VIDEO_STREAM],
        WorkerCommand::EncodeSnapshot {
            frames_per_second,
            bitrate_bits_per_second,
        } => {
            let mut payload = vec![COMMAND_ENCODE_SNAPSHOT];
            payload.extend_from_slice(&frames_per_second.to_be_bytes());
            payload.extend_from_slice(&bitrate_bits_per_second.to_be_bytes());
            payload
        }
        WorkerCommand::StartVideoStream {
            frames_per_second,
            bitrate_bits_per_second,
            audio,
        } => {
            let mut payload = vec![COMMAND_START_VIDEO_STREAM];
            payload.extend_from_slice(&frames_per_second.to_be_bytes());
            payload.extend_from_slice(&bitrate_bits_per_second.to_be_bytes());
            payload.push(u8::from(audio));
            payload
        }
        WorkerCommand::SetVideoBitrate(bitrate_bits_per_second) => {
            let mut payload = vec![COMMAND_SET_VIDEO_BITRATE];
            payload.extend_from_slice(&bitrate_bits_per_second.to_be_bytes());
            payload
        }
        WorkerCommand::SetVideoFrameDivisor(divisor) => {
            vec![COMMAND_SET_VIDEO_FRAME_DIVISOR, divisor]
        }
        WorkerCommand::RequestVideoKeyframe => vec![COMMAND_REQUEST_VIDEO_KEYFRAME],
        WorkerCommand::StopVideoStream => vec![COMMAND_STOP_VIDEO_STREAM],
        WorkerCommand::Stop => vec![COMMAND_STOP],
    };
    write_frame(writer, &payload)
}

pub fn read_command(reader: &mut impl Read) -> io::Result<WorkerCommand> {
    let payload = read_frame(reader)?;
    match payload.as_slice() {
        [COMMAND_AUDIO_PROOF] => Ok(WorkerCommand::AudioProof),
        [COMMAND_AUDIO_ENCODE_PROOF] => Ok(WorkerCommand::AudioEncodeProof),
        [COMMAND_CAPTURE_PROOF] => Ok(WorkerCommand::CaptureProof),
        [COMMAND_DESKTOP_TRANSITION_PROOF] => Ok(WorkerCommand::DesktopTransitionProof),
        [COMMAND_LOGIN_TRANSITION_PROOF] => Ok(WorkerCommand::LoginTransitionProof),
        [COMMAND_DISPLAY_MODE_TRANSITION_PROOF] => Ok(WorkerCommand::DisplayModeTransitionProof),
        [COMMAND_PREPARE_VIDEO_STREAM] => Ok(WorkerCommand::PrepareVideoStream),
        [COMMAND_ENCODE_SNAPSHOT, rest @ ..] if rest.len() == 10 => {
            Ok(WorkerCommand::EncodeSnapshot {
                frames_per_second: u16::from_be_bytes(rest[..2].try_into().unwrap()),
                bitrate_bits_per_second: u64::from_be_bytes(rest[2..].try_into().unwrap()),
            })
        }
        [COMMAND_START_VIDEO_STREAM, rest @ ..] if rest.len() == 11 && rest[10] <= 1 => {
            Ok(WorkerCommand::StartVideoStream {
                frames_per_second: u16::from_be_bytes(rest[..2].try_into().unwrap()),
                bitrate_bits_per_second: u64::from_be_bytes(rest[2..10].try_into().unwrap()),
                audio: rest[10] != 0,
            })
        }
        [COMMAND_SET_VIDEO_BITRATE, rest @ ..] if rest.len() == 8 => Ok(
            WorkerCommand::SetVideoBitrate(u64::from_be_bytes(rest.try_into().unwrap())),
        ),
        [COMMAND_SET_VIDEO_FRAME_DIVISOR, divisor] => {
            Ok(WorkerCommand::SetVideoFrameDivisor(*divisor))
        }
        [COMMAND_REQUEST_VIDEO_KEYFRAME] => Ok(WorkerCommand::RequestVideoKeyframe),
        [COMMAND_STOP_VIDEO_STREAM] => Ok(WorkerCommand::StopVideoStream),
        [COMMAND_STOP] => Ok(WorkerCommand::Stop),
        _ => Err(invalid_data("unknown worker command")),
    }
}

pub fn write_event(writer: &mut impl Write, event: &WorkerEvent) -> io::Result<()> {
    let mut payload = Vec::new();
    match event {
        WorkerEvent::Hello {
            version,
            process_id,
            session_id,
            identity,
            connection_token,
        } => {
            payload.push(EVENT_HELLO);
            payload.extend_from_slice(&version.to_be_bytes());
            payload.extend_from_slice(&process_id.to_be_bytes());
            payload.extend_from_slice(&session_id.to_be_bytes());
            payload.push(*identity as u8);
            payload.extend_from_slice(connection_token);
        }
        WorkerEvent::CaptureReport(report) => {
            payload.push(EVENT_CAPTURE_REPORT);
            payload.extend_from_slice(report.as_bytes());
        }
        WorkerEvent::ProofProgress(report) => {
            payload.push(EVENT_PROOF_PROGRESS);
            payload.extend_from_slice(report.as_bytes());
        }
        WorkerEvent::EncodedSnapshot {
            last_present_time,
            accumulated_frames,
            protected_content_masked,
            presentation_timestamp,
            keyframe,
            payload: packet,
        } => {
            payload.push(EVENT_ENCODED_SNAPSHOT);
            payload.extend_from_slice(&last_present_time.to_be_bytes());
            payload.extend_from_slice(&accumulated_frames.to_be_bytes());
            payload.push(u8::from(*protected_content_masked));
            payload.extend_from_slice(&presentation_timestamp.to_be_bytes());
            payload.push(u8::from(*keyframe));
            payload.extend_from_slice(packet);
        }
        WorkerEvent::EncodedVideoFrame {
            sequence,
            last_present_time,
            accumulated_frames,
            protected_content_masked,
            presentation_timestamp,
            keyframe,
            payload: packet,
        } => {
            payload.push(EVENT_ENCODED_VIDEO_FRAME);
            payload.extend_from_slice(&sequence.to_be_bytes());
            payload.extend_from_slice(&last_present_time.to_be_bytes());
            payload.extend_from_slice(&accumulated_frames.to_be_bytes());
            payload.push(u8::from(*protected_content_masked));
            payload.extend_from_slice(&presentation_timestamp.to_be_bytes());
            payload.push(u8::from(*keyframe));
            payload.extend_from_slice(packet);
        }
        WorkerEvent::Failure(error) => {
            payload.push(EVENT_FAILURE);
            payload.extend_from_slice(error.as_bytes());
        }
        WorkerEvent::VideoConfiguration(configuration) => {
            payload.push(EVENT_VIDEO_CONFIGURATION);
            payload.extend_from_slice(&configuration.width.to_be_bytes());
            payload.extend_from_slice(&configuration.height.to_be_bytes());
            payload.extend_from_slice(&configuration.refresh_rate.to_be_bytes());
            payload.push(configuration.capture_engine as u8);
            payload.push(configuration.format as u8);
            payload.push(configuration.color as u8);
        }
        WorkerEvent::VideoReconfigurationRequired(cause) => {
            payload.push(EVENT_VIDEO_RECONFIGURATION_REQUIRED);
            payload.push(*cause as u8);
        }
        WorkerEvent::SystemIdentityRequired => payload.push(EVENT_SYSTEM_IDENTITY_REQUIRED),
    }
    write_frame(writer, &payload)
}

pub fn read_event(reader: &mut impl Read) -> io::Result<WorkerEvent> {
    let payload = read_frame(reader)?;
    match payload.first().copied() {
        Some(EVENT_HELLO) if payload.len() == 28 => Ok(WorkerEvent::Hello {
            version: u16::from_be_bytes(payload[1..3].try_into().unwrap()),
            process_id: u32::from_be_bytes(payload[3..7].try_into().unwrap()),
            session_id: u32::from_be_bytes(payload[7..11].try_into().unwrap()),
            identity: match payload[11] {
                1 => WorkerIdentity::ActiveUser,
                2 => WorkerIdentity::LocalSystem,
                _ => return Err(invalid_data("invalid worker identity")),
            },
            connection_token: payload[12..28].try_into().unwrap(),
        }),
        Some(EVENT_CAPTURE_REPORT) => {
            let report = String::from_utf8(payload[1..].to_vec())
                .map_err(|_| invalid_data("worker report is not UTF-8"))?;
            Ok(WorkerEvent::CaptureReport(report))
        }
        Some(EVENT_PROOF_PROGRESS) => {
            let report = String::from_utf8(payload[1..].to_vec())
                .map_err(|_| invalid_data("worker progress is not UTF-8"))?;
            Ok(WorkerEvent::ProofProgress(report))
        }
        Some(EVENT_ENCODED_SNAPSHOT) if payload.len() >= 23 => Ok(WorkerEvent::EncodedSnapshot {
            last_present_time: i64::from_be_bytes(payload[1..9].try_into().unwrap()),
            accumulated_frames: u32::from_be_bytes(payload[9..13].try_into().unwrap()),
            protected_content_masked: match payload[13] {
                0 => false,
                1 => true,
                _ => return Err(invalid_data("invalid protected-content flag")),
            },
            presentation_timestamp: i64::from_be_bytes(payload[14..22].try_into().unwrap()),
            keyframe: match payload[22] {
                0 => false,
                1 => true,
                _ => return Err(invalid_data("invalid keyframe flag")),
            },
            payload: payload[23..].to_vec(),
        }),
        Some(EVENT_FAILURE) => {
            let error = String::from_utf8(payload[1..].to_vec())
                .map_err(|_| invalid_data("worker failure is not UTF-8"))?;
            Ok(WorkerEvent::Failure(error))
        }
        Some(EVENT_VIDEO_CONFIGURATION) if payload.len() == 16 => {
            let capture_engine = match payload[13] {
                1 => WorkerCaptureEngine::WindowsGraphicsCapture,
                2 => WorkerCaptureEngine::DesktopDuplication,
                _ => return Err(invalid_data("invalid worker capture engine")),
            };
            let format = match payload[14] {
                1 => WorkerVideoFormat::Nv12,
                2 => WorkerVideoFormat::P010,
                _ => return Err(invalid_data("invalid worker video format")),
            };
            let color = match payload[15] {
                1 => WorkerVideoColor::Bt709Limited,
                2 => WorkerVideoColor::Bt2020PqLimited,
                _ => return Err(invalid_data("invalid worker video color")),
            };
            Ok(WorkerEvent::VideoConfiguration(WorkerVideoConfiguration {
                width: u32::from_be_bytes(payload[1..5].try_into().unwrap()),
                height: u32::from_be_bytes(payload[5..9].try_into().unwrap()),
                refresh_rate: u32::from_be_bytes(payload[9..13].try_into().unwrap()),
                capture_engine,
                format,
                color,
            }))
        }
        Some(EVENT_VIDEO_RECONFIGURATION_REQUIRED) if payload.len() == 2 => {
            let cause = rustconsole_protocol::wire::VideoReconfigurationCause::try_from(i32::from(
                payload[1],
            ))
            .map_err(|_| invalid_data("invalid video reconfiguration cause"))?;
            if cause == rustconsole_protocol::wire::VideoReconfigurationCause::Unspecified {
                return Err(invalid_data("unspecified video reconfiguration cause"));
            }
            Ok(WorkerEvent::VideoReconfigurationRequired(cause))
        }
        Some(EVENT_SYSTEM_IDENTITY_REQUIRED) if payload.len() == 1 => {
            Ok(WorkerEvent::SystemIdentityRequired)
        }
        Some(EVENT_ENCODED_VIDEO_FRAME) if payload.len() >= 31 => {
            Ok(WorkerEvent::EncodedVideoFrame {
                sequence: u64::from_be_bytes(payload[1..9].try_into().unwrap()),
                last_present_time: i64::from_be_bytes(payload[9..17].try_into().unwrap()),
                accumulated_frames: u32::from_be_bytes(payload[17..21].try_into().unwrap()),
                protected_content_masked: match payload[21] {
                    0 => false,
                    1 => true,
                    _ => return Err(invalid_data("invalid protected-content flag")),
                },
                presentation_timestamp: i64::from_be_bytes(payload[22..30].try_into().unwrap()),
                keyframe: match payload[30] {
                    0 => false,
                    1 => true,
                    _ => return Err(invalid_data("invalid keyframe flag")),
                },
                payload: payload[31..].to_vec(),
            })
        }
        _ => Err(invalid_data("unknown worker event")),
    }
}

fn write_frame(writer: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_PAYLOAD {
        return Err(invalid_data("worker payload exceeds 16 MiB"));
    }
    let length = u32::try_from(payload.len()).map_err(|_| invalid_data("worker payload size"))?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(payload)
}

fn read_frame(reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let length = usize::try_from(u32::from_be_bytes(length))
        .map_err(|_| invalid_data("worker payload size"))?;
    if length > MAX_PAYLOAD {
        return Err(invalid_data("worker payload exceeds 16 MiB"));
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[derive(Clone, Debug, PartialEq)]
pub enum AudioWorkerEvent {
    Packet {
        queued_at_micros: u64,
        packet: rustconsole_protocol::audio::AudioPacket,
    },
    State(rustconsole_protocol::wire::AudioStreamState),
}

pub fn write_audio_event(writer: &mut impl Write, event: &AudioWorkerEvent) -> io::Result<()> {
    use rustconsole_protocol::{audio::Header, wire};
    let mut data = Vec::new();
    match event {
        AudioWorkerEvent::Packet {
            queued_at_micros,
            packet,
        } => {
            let size = u16::try_from(packet.payload.len())
                .map_err(|_| invalid_data("audio packet too large"))?;
            let header = Header {
                generation: packet.generation,
                sequence: packet.sequence,
                captured_at_micros: packet.captured_at_micros,
                decoded_samples: packet.decoded_samples,
                skip_start_samples: packet.skip_start_samples,
                skip_end_samples: packet.skip_end_samples,
                packet_size: size,
                fragment_count: 1,
                fragment_index: 0,
                offset: 0,
                payload_size: size,
            };
            data.push(1);
            data.extend_from_slice(&queued_at_micros.to_be_bytes());
            data.extend_from_slice(&header.encode().map_err(invalid_data)?);
            data.extend_from_slice(&packet.payload);
        }
        AudioWorkerEvent::State(state) => {
            if !state.valid() {
                return Err(invalid_data("invalid audio state"));
            }
            data.push(2);
            data.extend(
                wire::encode_reliable_frame(&wire::Envelope {
                    body: Some(wire::envelope::Body::AudioStreamState(state.clone())),
                })
                .map_err(io::Error::other)?,
            );
        }
    }
    if data.len() > 8192 {
        return Err(invalid_data("audio event exceeds 8 KiB"));
    }
    write_frame(writer, &data)
}

pub fn read_audio_event(reader: &mut impl Read) -> io::Result<AudioWorkerEvent> {
    use rustconsole_protocol::{
        audio::{AudioPacket, Header},
        wire,
    };
    let mut size = [0; 4];
    reader.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    if size == 0 || size > 8192 {
        return Err(invalid_data("audio event exceeds its bound"));
    }
    let mut data = vec![0; size];
    reader.read_exact(&mut data)?;
    match data[0] {
        1 if data.len() > 9 => {
            let queued_at_micros = u64::from_be_bytes(data[1..9].try_into().unwrap());
            let (h, payload) = Header::decode(&data[9..]).map_err(invalid_data)?;
            if h.fragment_count != 1 || h.offset != 0 || h.packet_size != h.payload_size {
                return Err(invalid_data("fragmented worker audio event"));
            }
            Ok(AudioWorkerEvent::Packet {
                queued_at_micros,
                packet: AudioPacket {
                    generation: h.generation,
                    sequence: h.sequence,
                    captured_at_micros: h.captured_at_micros,
                    decoded_samples: h.decoded_samples,
                    skip_start_samples: h.skip_start_samples,
                    skip_end_samples: h.skip_end_samples,
                    payload: payload.to_vec(),
                },
            })
        }
        2 => match wire::decode_reliable_frame(&data[1..])
            .map_err(io::Error::other)?
            .body
        {
            Some(wire::envelope::Body::AudioStreamState(state)) if state.valid() => {
                Ok(AudioWorkerEvent::State(state))
            }
            _ => Err(invalid_data("invalid worker audio state")),
        },
        _ => Err(invalid_data("invalid worker audio event")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn audio_events_round_trip_and_reject_oversized_prefixes() {
        use rustconsole_protocol::{
            audio::AudioPacket,
            wire::{AudioStatus, AudioStreamState},
        };
        for event in [
            AudioWorkerEvent::Packet {
                queued_at_micros: 90,
                packet: AudioPacket {
                    generation: 1,
                    sequence: 2,
                    captured_at_micros: 80,
                    decoded_samples: 480,
                    skip_start_samples: 312,
                    skip_end_samples: 0,
                    payload: vec![42; 7657],
                },
            },
            AudioWorkerEvent::State(AudioStreamState::new(
                1,
                AudioStatus::Unavailable,
                3,
                "no device".into(),
            )),
        ] {
            let mut bytes = Vec::new();
            write_audio_event(&mut bytes, &event).unwrap();
            assert_eq!(read_audio_event(&mut Cursor::new(bytes)).unwrap(), event);
        }
        assert!(read_audio_event(&mut Cursor::new(8193_u32.to_be_bytes())).is_err());
        assert!(read_audio_event(&mut Cursor::new(0_u32.to_be_bytes())).is_err());
    }

    #[test]
    fn command_round_trip_is_framed() {
        let mut bytes = Vec::new();
        write_command(&mut bytes, WorkerCommand::CaptureProof).unwrap();
        assert_eq!(
            read_command(&mut Cursor::new(bytes)).unwrap(),
            WorkerCommand::CaptureProof
        );

        let mut bytes = Vec::new();
        write_command(&mut bytes, WorkerCommand::DesktopTransitionProof).unwrap();
        assert_eq!(
            read_command(&mut Cursor::new(bytes)).unwrap(),
            WorkerCommand::DesktopTransitionProof
        );

        let mut bytes = Vec::new();
        write_command(&mut bytes, WorkerCommand::LoginTransitionProof).unwrap();
        assert_eq!(
            read_command(&mut Cursor::new(bytes)).unwrap(),
            WorkerCommand::LoginTransitionProof
        );

        let mut bytes = Vec::new();
        write_command(&mut bytes, WorkerCommand::DisplayModeTransitionProof).unwrap();
        assert_eq!(
            read_command(&mut Cursor::new(bytes)).unwrap(),
            WorkerCommand::DisplayModeTransitionProof
        );

        for command in [
            WorkerCommand::AudioProof,
            WorkerCommand::AudioEncodeProof,
            WorkerCommand::PrepareVideoStream,
            WorkerCommand::StartVideoStream {
                audio: true,
                frames_per_second: 120,
                bitrate_bits_per_second: 20_000_000,
            },
            WorkerCommand::SetVideoBitrate(16_000_000),
            WorkerCommand::SetVideoFrameDivisor(2),
            WorkerCommand::RequestVideoKeyframe,
            WorkerCommand::StopVideoStream,
        ] {
            let mut bytes = Vec::new();
            write_command(&mut bytes, command).unwrap();
            assert_eq!(read_command(&mut Cursor::new(bytes)).unwrap(), command);
        }
    }

    #[test]
    fn hello_round_trip_keeps_identity() {
        let event = WorkerEvent::Hello {
            version: VERSION,
            process_id: 42,
            session_id: 7,
            identity: WorkerIdentity::ActiveUser,
            connection_token: [9; 16],
        };
        let mut bytes = Vec::new();
        write_event(&mut bytes, &event).unwrap();
        assert_eq!(read_event(&mut Cursor::new(bytes)).unwrap(), event);

        let mut invalid_identity = Vec::from(28_u32.to_be_bytes());
        invalid_identity.extend([EVENT_HELLO, 0, 12, 0, 0, 0, 42, 0, 0, 0, 7, 3]);
        invalid_identity.extend([0; 16]);
        assert!(read_event(&mut Cursor::new(invalid_identity)).is_err());
    }

    #[test]
    fn proof_progress_round_trip_keeps_report() {
        let event = WorkerEvent::ProofProgress("status=running\nphase=initial\n".to_owned());
        let mut bytes = Vec::new();
        write_event(&mut bytes, &event).unwrap();
        assert_eq!(read_event(&mut Cursor::new(bytes)).unwrap(), event);
    }

    #[test]
    fn encoded_snapshot_round_trip_keeps_packet_and_metadata() {
        let event = WorkerEvent::EncodedSnapshot {
            last_present_time: 91,
            accumulated_frames: 2,
            protected_content_masked: false,
            presentation_timestamp: 7,
            keyframe: true,
            payload: vec![1, 2, 3],
        };
        let mut bytes = Vec::new();
        write_event(&mut bytes, &event).unwrap();
        assert_eq!(read_event(&mut Cursor::new(bytes)).unwrap(), event);
    }

    #[test]
    fn video_configuration_and_reconfiguration_round_trip() {
        for event in [
            WorkerEvent::VideoConfiguration(WorkerVideoConfiguration {
                width: 2560,
                height: 1440,
                refresh_rate: 240,
                capture_engine: WorkerCaptureEngine::WindowsGraphicsCapture,
                format: WorkerVideoFormat::P010,
                color: WorkerVideoColor::Bt2020PqLimited,
            }),
            WorkerEvent::VideoReconfigurationRequired(
                rustconsole_protocol::wire::VideoReconfigurationCause::Color,
            ),
            WorkerEvent::SystemIdentityRequired,
        ] {
            let mut bytes = Vec::new();
            write_event(&mut bytes, &event).unwrap();
            assert_eq!(read_event(&mut Cursor::new(bytes)).unwrap(), event);
        }

        let invalid = [0, 0, 0, 2, EVENT_VIDEO_RECONFIGURATION_REQUIRED, 0];
        assert!(read_event(&mut Cursor::new(invalid)).is_err());
    }

    #[test]
    fn encoded_video_frame_round_trip_keeps_sequence_packet_and_metadata() {
        let event = WorkerEvent::EncodedVideoFrame {
            sequence: 17,
            last_present_time: 91,
            accumulated_frames: 2,
            protected_content_masked: false,
            presentation_timestamp: 7,
            keyframe: true,
            payload: vec![1, 2, 3],
        };
        let mut bytes = Vec::new();
        write_event(&mut bytes, &event).unwrap();
        assert_eq!(read_event(&mut Cursor::new(bytes)).unwrap(), event);
    }

    #[test]
    fn failure_round_trip_keeps_error() {
        let event = WorkerEvent::Failure("GPU bridge failed".to_owned());
        let mut bytes = Vec::new();
        write_event(&mut bytes, &event).unwrap();
        assert_eq!(read_event(&mut Cursor::new(bytes)).unwrap(), event);
    }

    #[test]
    fn oversized_worker_frame_is_rejected_before_allocation() {
        let mut bytes = Vec::from(u32::try_from(MAX_PAYLOAD + 1).unwrap().to_be_bytes());
        bytes.push(0);
        assert!(read_event(&mut Cursor::new(bytes)).is_err());
    }
}
