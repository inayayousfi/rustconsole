//! Fixed, bounded pointer datagrams.

use std::fmt;

const MAGIC: [u8; 2] = *b"RI";
const VERSION: u8 = 1;
const ABSOLUTE: u8 = 1;
const RELATIVE: u8 = 2;
pub const INPUT_DATAGRAM_SIZE: usize = 36;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerSnapshot {
    Absolute {
        generation: u64,
        sequence: u64,
        x: u16,
        y: u16,
    },
    Relative {
        generation: u64,
        sequence: u64,
        cumulative_x: i64,
        cumulative_y: i64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerUpdate {
    Absolute { x: u16, y: u16 },
    Relative { delta_x: i64, delta_y: i64 },
}

#[derive(Default)]
pub struct PointerSnapshotReceiver {
    generation: Option<u64>,
    sequence: u64,
    cumulative: Option<(i64, i64)>,
}

impl PointerSnapshotReceiver {
    #[must_use]
    pub const fn generation(&self) -> Option<u64> {
        self.generation
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn push(
        &mut self,
        snapshot: PointerSnapshot,
    ) -> Result<Option<PointerUpdate>, InputDatagramError> {
        let (generation, sequence) = snapshot.identity();
        if self.generation.is_some_and(|current| generation < current) {
            return Ok(None);
        }
        if self.generation != Some(generation) {
            self.generation = Some(generation);
            self.sequence = 0;
            self.cumulative = None;
        }
        if sequence == 0 || sequence <= self.sequence {
            return Ok(None);
        }
        let (update, cumulative) = match snapshot {
            PointerSnapshot::Absolute { x, y, .. } => {
                (Some(PointerUpdate::Absolute { x, y }), None)
            }
            PointerSnapshot::Relative {
                cumulative_x,
                cumulative_y,
                ..
            } => {
                let update = self
                    .cumulative
                    .map(|(previous_x, previous_y)| {
                        Ok(PointerUpdate::Relative {
                            delta_x: cumulative_x
                                .checked_sub(previous_x)
                                .ok_or(InputDatagramError)?,
                            delta_y: cumulative_y
                                .checked_sub(previous_y)
                                .ok_or(InputDatagramError)?,
                        })
                    })
                    .transpose()?;
                (update, Some((cumulative_x, cumulative_y)))
            }
        };
        self.sequence = sequence;
        self.cumulative = cumulative;
        Ok(update)
    }
}

impl PointerSnapshot {
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.identity().0
    }

    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.identity().1
    }

    #[must_use]
    pub const fn is_absolute(self) -> bool {
        matches!(self, Self::Absolute { .. })
    }

    const fn identity(self) -> (u64, u64) {
        match self {
            Self::Absolute {
                generation,
                sequence,
                ..
            }
            | Self::Relative {
                generation,
                sequence,
                ..
            } => (generation, sequence),
        }
    }

    #[must_use]
    pub fn encode(self) -> [u8; INPUT_DATAGRAM_SIZE] {
        let mut bytes = [0; INPUT_DATAGRAM_SIZE];
        bytes[..2].copy_from_slice(&MAGIC);
        bytes[2] = VERSION;
        match self {
            Self::Absolute {
                generation,
                sequence,
                x,
                y,
            } => {
                bytes[3] = ABSOLUTE;
                bytes[4..12].copy_from_slice(&generation.to_be_bytes());
                bytes[12..20].copy_from_slice(&sequence.to_be_bytes());
                bytes[20..22].copy_from_slice(&x.to_be_bytes());
                bytes[22..24].copy_from_slice(&y.to_be_bytes());
            }
            Self::Relative {
                generation,
                sequence,
                cumulative_x,
                cumulative_y,
            } => {
                bytes[3] = RELATIVE;
                bytes[4..12].copy_from_slice(&generation.to_be_bytes());
                bytes[12..20].copy_from_slice(&sequence.to_be_bytes());
                bytes[20..28].copy_from_slice(&cumulative_x.to_be_bytes());
                bytes[28..36].copy_from_slice(&cumulative_y.to_be_bytes());
            }
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, InputDatagramError> {
        if bytes.len() != INPUT_DATAGRAM_SIZE || bytes[..2] != MAGIC || bytes[2] != VERSION {
            return Err(InputDatagramError);
        }
        let generation = u64::from_be_bytes(bytes[4..12].try_into().unwrap());
        let sequence = u64::from_be_bytes(bytes[12..20].try_into().unwrap());
        match bytes[3] {
            ABSOLUTE
                if bytes[24..].iter().all(|byte| *byte == 0)
                    && u16::from_be_bytes(bytes[20..22].try_into().unwrap()) <= 32767
                    && u16::from_be_bytes(bytes[22..24].try_into().unwrap()) <= 32767 =>
            {
                Ok(Self::Absolute {
                    generation,
                    sequence,
                    x: u16::from_be_bytes(bytes[20..22].try_into().unwrap()),
                    y: u16::from_be_bytes(bytes[22..24].try_into().unwrap()),
                })
            }
            RELATIVE => Ok(Self::Relative {
                generation,
                sequence,
                cumulative_x: i64::from_be_bytes(bytes[20..28].try_into().unwrap()),
                cumulative_y: i64::from_be_bytes(bytes[28..36].try_into().unwrap()),
            }),
            _ => Err(InputDatagramError),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputDatagramError;

impl fmt::Display for InputDatagramError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("malformed input datagram")
    }
}

impl std::error::Error for InputDatagramError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_round_trip() {
        for snapshot in [
            PointerSnapshot::Absolute {
                generation: 4,
                sequence: 9,
                x: 0,
                y: 32767,
            },
            PointerSnapshot::Relative {
                generation: 5,
                sequence: 10,
                cumulative_x: -7,
                cumulative_y: 12,
            },
        ] {
            assert_eq!(PointerSnapshot::decode(&snapshot.encode()), Ok(snapshot));
        }
    }

    #[test]
    fn rejects_wrong_size_magic_version_kind_and_absolute_padding() {
        let mut bytes = PointerSnapshot::Absolute {
            generation: 1,
            sequence: 1,
            x: 2,
            y: 3,
        }
        .encode();
        assert!(PointerSnapshot::decode(&bytes[..31]).is_err());
        bytes[0] = 0;
        assert!(PointerSnapshot::decode(&bytes).is_err());
        bytes[0] = MAGIC[0];
        bytes[2] = 2;
        assert!(PointerSnapshot::decode(&bytes).is_err());
        bytes[2] = VERSION;
        bytes[3] = 9;
        assert!(PointerSnapshot::decode(&bytes).is_err());
        bytes[3] = ABSOLUTE;
        bytes[31] = 1;
        assert!(PointerSnapshot::decode(&bytes).is_err());
        bytes[31] = 0;
        bytes[20..22].copy_from_slice(&32768u16.to_be_bytes());
        assert!(PointerSnapshot::decode(&bytes).is_err());
    }

    #[test]
    fn cumulative_relative_snapshots_recover_lost_datagrams() {
        let mut receiver = PointerSnapshotReceiver::default();
        assert_eq!(
            receiver
                .push(PointerSnapshot::Relative {
                    generation: 1,
                    sequence: 1,
                    cumulative_x: 10,
                    cumulative_y: -5,
                })
                .unwrap(),
            None
        );
        assert_eq!(
            receiver
                .push(PointerSnapshot::Relative {
                    generation: 1,
                    sequence: 3,
                    cumulative_x: 19,
                    cumulative_y: 7,
                })
                .unwrap(),
            Some(PointerUpdate::Relative {
                delta_x: 9,
                delta_y: 12,
            })
        );
        assert_eq!(
            receiver
                .push(PointerSnapshot::Relative {
                    generation: 1,
                    sequence: 2,
                    cumulative_x: 11,
                    cumulative_y: 0,
                })
                .unwrap(),
            None
        );
    }

    #[test]
    fn generation_change_resets_relative_baseline() {
        let mut receiver = PointerSnapshotReceiver::default();
        for generation in [1, 2] {
            assert_eq!(
                receiver
                    .push(PointerSnapshot::Relative {
                        generation,
                        sequence: 1,
                        cumulative_x: i64::MAX,
                        cumulative_y: i64::MIN,
                    })
                    .unwrap(),
                None
            );
        }
        assert_eq!(
            receiver
                .push(PointerSnapshot::Absolute {
                    generation: 1,
                    sequence: 99,
                    x: 0,
                    y: 0,
                })
                .unwrap(),
            None
        );
    }
}
