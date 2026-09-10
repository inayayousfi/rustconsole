use std::fmt;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use zeroize::Zeroizing;

pub const VERSION: u16 = 3;
pub const MAX_MESSAGE_SIZE: usize = 4 * 1024;
pub const MAX_PASSWORD_SIZE: usize = 1024;
pub const MAX_ERROR_SIZE: usize = 2048;
pub const MINIMUM_MAXIMUM_BITRATE_BITS_PER_SECOND: u64 = 5_000_000;

const LAUNCH: u8 = 1;
const STOP: u8 = 2;
const RECONNECT: u8 = 3;
const AUTHENTICATED: u8 = 1;
const STARTED: u8 = 2;
const ENDED: u8 = 3;
const ERROR: u8 = 4;

pub struct LaunchRequest {
    pub address: SocketAddr,
    pub password: Zeroizing<Vec<u8>>,
    pub remember_password: bool,
    pub maximum_bitrate_bits_per_second: u64,
    pub latency_diagnostics: bool,
}

pub enum PlayerCommand {
    Launch(LaunchRequest),
    Stop,
    Reconnect,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlayerEvent {
    Authenticated { host_identity: [u8; 32] },
    Started,
    Ended,
    Error(String),
}

#[derive(Debug)]
pub enum ProcessProtocolError {
    Io(io::Error),
    MessageTooLarge { size: usize, maximum: usize },
    InvalidMessage(&'static str),
}

impl fmt::Display for ProcessProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "player process pipe failed: {error}"),
            Self::MessageTooLarge { size, maximum } => {
                write!(
                    formatter,
                    "player process message is {size} bytes; maximum is {maximum}"
                )
            }
            Self::InvalidMessage(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ProcessProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ProcessProtocolError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub fn write_launch(
    writer: &mut impl Write,
    request: &LaunchRequest,
) -> Result<(), ProcessProtocolError> {
    if request.password.len() > MAX_PASSWORD_SIZE {
        return Err(ProcessProtocolError::MessageTooLarge {
            size: request.password.len(),
            maximum: MAX_PASSWORD_SIZE,
        });
    }
    validate_maximum_bitrate(request.maximum_bitrate_bits_per_second)?;
    let address = request.address.to_string();
    let address_length = u16::try_from(address.len())
        .map_err(|_| ProcessProtocolError::InvalidMessage("player address is too long"))?;
    let password_length = u16::try_from(request.password.len()).expect("password bound fits u16");
    let mut payload = Vec::with_capacity(16 + address.len() + request.password.len());
    payload.push(LAUNCH);
    payload.extend_from_slice(&VERSION.to_be_bytes());
    payload.extend_from_slice(&address_length.to_be_bytes());
    payload.extend_from_slice(address.as_bytes());
    payload.extend_from_slice(&request.maximum_bitrate_bits_per_second.to_be_bytes());
    payload.push(u8::from(request.remember_password));
    payload.push(u8::from(request.latency_diagnostics));
    payload.extend_from_slice(&password_length.to_be_bytes());
    payload.extend_from_slice(request.password.as_slice());
    write_frame(writer, &payload)
}

pub fn write_stop(writer: &mut impl Write) -> Result<(), ProcessProtocolError> {
    write_frame(writer, &[STOP])
}

pub fn write_reconnect(writer: &mut impl Write) -> Result<(), ProcessProtocolError> {
    write_frame(writer, &[RECONNECT])
}

pub fn read_command(reader: &mut impl Read) -> Result<PlayerCommand, ProcessProtocolError> {
    let payload = read_frame(reader)?;
    let Some((&kind, rest)) = payload.split_first() else {
        return Err(ProcessProtocolError::InvalidMessage("empty player command"));
    };
    match kind {
        LAUNCH => decode_launch(rest).map(PlayerCommand::Launch),
        STOP if rest.is_empty() => Ok(PlayerCommand::Stop),
        STOP => Err(ProcessProtocolError::InvalidMessage(
            "stop command contains trailing data",
        )),
        RECONNECT if rest.is_empty() => Ok(PlayerCommand::Reconnect),
        RECONNECT => Err(ProcessProtocolError::InvalidMessage(
            "reconnect command contains trailing data",
        )),
        _ => Err(ProcessProtocolError::InvalidMessage(
            "unknown player command",
        )),
    }
}

pub fn write_event(
    writer: &mut impl Write,
    event: &PlayerEvent,
) -> Result<(), ProcessProtocolError> {
    let payload = match event {
        PlayerEvent::Authenticated { host_identity } => {
            let mut payload = Vec::with_capacity(33);
            payload.push(AUTHENTICATED);
            payload.extend_from_slice(host_identity);
            payload
        }
        PlayerEvent::Started => vec![STARTED],
        PlayerEvent::Ended => vec![ENDED],
        PlayerEvent::Error(message) => {
            if message.len() > MAX_ERROR_SIZE {
                return Err(ProcessProtocolError::MessageTooLarge {
                    size: message.len(),
                    maximum: MAX_ERROR_SIZE,
                });
            }
            let mut payload = Vec::with_capacity(message.len() + 1);
            payload.push(ERROR);
            payload.extend_from_slice(message.as_bytes());
            payload
        }
    };
    write_frame(writer, &payload)
}

pub fn read_event(reader: &mut impl Read) -> Result<PlayerEvent, ProcessProtocolError> {
    let payload = read_frame(reader)?;
    let Some((&kind, rest)) = payload.split_first() else {
        return Err(ProcessProtocolError::InvalidMessage("empty player event"));
    };
    match kind {
        AUTHENTICATED if rest.len() == 32 => Ok(PlayerEvent::Authenticated {
            host_identity: rest.try_into().expect("identity length was checked"),
        }),
        STARTED if rest.is_empty() => Ok(PlayerEvent::Started),
        ENDED if rest.is_empty() => Ok(PlayerEvent::Ended),
        ERROR if rest.len() <= MAX_ERROR_SIZE => String::from_utf8(rest.to_vec())
            .map(PlayerEvent::Error)
            .map_err(|_| ProcessProtocolError::InvalidMessage("player error is not UTF-8")),
        AUTHENTICATED | STARTED | ENDED | ERROR => Err(ProcessProtocolError::InvalidMessage(
            "player event has an invalid payload",
        )),
        _ => Err(ProcessProtocolError::InvalidMessage("unknown player event")),
    }
}

fn decode_launch(mut payload: &[u8]) -> Result<LaunchRequest, ProcessProtocolError> {
    let version = take_u16(&mut payload)?;
    if version != VERSION {
        return Err(ProcessProtocolError::InvalidMessage(
            "unsupported player process protocol version",
        ));
    }
    let address_length = usize::from(take_u16(&mut payload)?);
    let address = take(&mut payload, address_length)?;
    let address = std::str::from_utf8(address)
        .map_err(|_| ProcessProtocolError::InvalidMessage("player address is not UTF-8"))?
        .parse()
        .map_err(|_| ProcessProtocolError::InvalidMessage("player address is invalid"))?;
    let maximum_bitrate_bits_per_second = take_u64(&mut payload)?;
    validate_maximum_bitrate(maximum_bitrate_bits_per_second)?;
    let remember_password = match take(&mut payload, 1)?[0] {
        0 => false,
        1 => true,
        _ => {
            return Err(ProcessProtocolError::InvalidMessage(
                "remember-password flag is invalid",
            ));
        }
    };
    let latency_diagnostics = match take(&mut payload, 1)?[0] {
        0 => false,
        1 => true,
        _ => {
            return Err(ProcessProtocolError::InvalidMessage(
                "latency-diagnostics flag is invalid",
            ));
        }
    };
    let password_length = usize::from(take_u16(&mut payload)?);
    if password_length > MAX_PASSWORD_SIZE {
        return Err(ProcessProtocolError::MessageTooLarge {
            size: password_length,
            maximum: MAX_PASSWORD_SIZE,
        });
    }
    let password = Zeroizing::new(take(&mut payload, password_length)?.to_vec());
    if !payload.is_empty() {
        return Err(ProcessProtocolError::InvalidMessage(
            "launch command contains trailing data",
        ));
    }
    Ok(LaunchRequest {
        address,
        password,
        remember_password,
        maximum_bitrate_bits_per_second,
        latency_diagnostics,
    })
}

fn validate_maximum_bitrate(value: u64) -> Result<(), ProcessProtocolError> {
    if !(MINIMUM_MAXIMUM_BITRATE_BITS_PER_SECOND
        ..=rustconsole_protocol::av1::MAXIMUM_VIDEO_BITRATE_BITS_PER_SECOND)
        .contains(&value)
    {
        return Err(ProcessProtocolError::InvalidMessage(
            "player maximum bitrate is outside 5-100 Mbit/s",
        ));
    }
    Ok(())
}

fn write_frame(writer: &mut impl Write, payload: &[u8]) -> Result<(), ProcessProtocolError> {
    if payload.len() > MAX_MESSAGE_SIZE {
        return Err(ProcessProtocolError::MessageTooLarge {
            size: payload.len(),
            maximum: MAX_MESSAGE_SIZE,
        });
    }
    writer.write_all(&(payload.len() as u32).to_be_bytes())?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

fn read_frame(reader: &mut impl Read) -> Result<Vec<u8>, ProcessProtocolError> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_MESSAGE_SIZE {
        return Err(ProcessProtocolError::MessageTooLarge {
            size: length,
            maximum: MAX_MESSAGE_SIZE,
        });
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

fn take<'a>(payload: &mut &'a [u8], length: usize) -> Result<&'a [u8], ProcessProtocolError> {
    if payload.len() < length {
        return Err(ProcessProtocolError::InvalidMessage(
            "player process message is truncated",
        ));
    }
    let (value, rest) = payload.split_at(length);
    *payload = rest;
    Ok(value)
}

fn take_u16(payload: &mut &[u8]) -> Result<u16, ProcessProtocolError> {
    Ok(u16::from_be_bytes(
        take(payload, 2)?.try_into().expect("length was checked"),
    ))
}

fn take_u64(payload: &mut &[u8]) -> Result<u64, ProcessProtocolError> {
    Ok(u64::from_be_bytes(
        take(payload, 8)?.try_into().expect("length was checked"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch() -> LaunchRequest {
        LaunchRequest {
            address: "127.0.0.1:47999".parse().unwrap(),
            password: Zeroizing::new(b"not logged".to_vec()),
            remember_password: true,
            maximum_bitrate_bits_per_second: 100_000_000,
            latency_diagnostics: true,
        }
    }

    #[test]
    fn launch_round_trip_preserves_bounded_fields() {
        let expected = launch();
        let mut bytes = Vec::new();
        write_launch(&mut bytes, &expected).unwrap();

        let PlayerCommand::Launch(actual) = read_command(&mut bytes.as_slice()).unwrap() else {
            panic!("expected launch command");
        };
        assert_eq!(actual.address, expected.address);
        assert_eq!(actual.password.as_slice(), expected.password.as_slice());
        assert_eq!(actual.remember_password, expected.remember_password);
        assert_eq!(actual.latency_diagnostics, expected.latency_diagnostics);
        assert_eq!(
            actual.maximum_bitrate_bits_per_second,
            expected.maximum_bitrate_bits_per_second
        );
    }

    #[test]
    fn launch_rejects_maximum_bitrate_outside_ui_range() {
        for maximum_bitrate_bits_per_second in [4_999_999, 100_000_001] {
            let request = LaunchRequest {
                maximum_bitrate_bits_per_second,
                ..launch()
            };
            assert!(matches!(
                write_launch(&mut Vec::new(), &request),
                Err(ProcessProtocolError::InvalidMessage(_))
            ));
        }
    }

    #[test]
    fn events_and_control_commands_round_trip() {
        for expected in [
            PlayerEvent::Authenticated {
                host_identity: [7; 32],
            },
            PlayerEvent::Started,
            PlayerEvent::Ended,
            PlayerEvent::Error("renderer stopped".into()),
        ] {
            let mut bytes = Vec::new();
            write_event(&mut bytes, &expected).unwrap();
            assert_eq!(read_event(&mut bytes.as_slice()).unwrap(), expected);
        }

        let mut bytes = Vec::new();
        write_stop(&mut bytes).unwrap();
        assert!(matches!(
            read_command(&mut bytes.as_slice()).unwrap(),
            PlayerCommand::Stop
        ));

        let mut bytes = Vec::new();
        write_reconnect(&mut bytes).unwrap();
        assert!(matches!(
            read_command(&mut bytes.as_slice()).unwrap(),
            PlayerCommand::Reconnect
        ));
    }

    #[test]
    fn outer_bound_is_checked_before_allocation() {
        let bytes = ((MAX_MESSAGE_SIZE + 1) as u32).to_be_bytes();
        assert!(matches!(
            read_event(&mut bytes.as_slice()),
            Err(ProcessProtocolError::MessageTooLarge { maximum, .. })
                if maximum == MAX_MESSAGE_SIZE
        ));
    }

    #[test]
    fn password_bound_is_enforced() {
        let request = LaunchRequest {
            password: Zeroizing::new(vec![0; MAX_PASSWORD_SIZE + 1]),
            ..launch()
        };
        assert!(matches!(
            write_launch(&mut Vec::new(), &request),
            Err(ProcessProtocolError::MessageTooLarge { maximum, .. })
                if maximum == MAX_PASSWORD_SIZE
        ));
    }
}
