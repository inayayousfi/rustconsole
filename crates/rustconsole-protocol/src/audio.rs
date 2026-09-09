//! Versioned Opus fragment headers, independent of transport and codec libraries.

pub const HEADER_SIZE: usize = 48;
pub const MAX_PACKET_SIZE: usize = 7_657;
pub const MAX_FRAGMENTS: usize = 64;
pub const MAGIC: [u8; 2] = *b"RA";
pub const PACKET_DURATION_MICROS: u64 = 10_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioPacket {
    pub generation: u64,
    pub sequence: u64,
    pub captured_at_micros: u64,
    pub decoded_samples: u16,
    pub skip_start_samples: u16,
    pub skip_end_samples: u16,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    pub generation: u64,
    pub sequence: u64,
    pub captured_at_micros: u64,
    pub decoded_samples: u16,
    pub skip_start_samples: u16,
    pub skip_end_samples: u16,
    pub packet_size: u16,
    pub fragment_index: u16,
    pub fragment_count: u16,
    pub offset: u16,
    pub payload_size: u16,
}

impl Header {
    pub fn valid(self) -> bool {
        self.generation != 0
            && self.decoded_samples == 480
            && u32::from(self.skip_start_samples) + u32::from(self.skip_end_samples)
                <= u32::from(self.decoded_samples)
            && (1..=MAX_PACKET_SIZE).contains(&usize::from(self.packet_size))
            && (1..=MAX_FRAGMENTS).contains(&usize::from(self.fragment_count))
            && self.fragment_index < self.fragment_count
            && self.payload_size != 0
            && u32::from(self.offset) + u32::from(self.payload_size) <= u32::from(self.packet_size)
    }

    pub fn encode(self) -> Result<[u8; HEADER_SIZE], &'static str> {
        if !self.valid() {
            return Err("invalid audio fragment header");
        }
        let mut out = [0; HEADER_SIZE];
        out[..2].copy_from_slice(&MAGIC);
        out[2] = 1;
        out[3] = 2; // Stereo layout, left then right.
        out[4..12].copy_from_slice(&self.generation.to_be_bytes());
        out[12..20].copy_from_slice(&self.sequence.to_be_bytes());
        out[20..28].copy_from_slice(&self.captured_at_micros.to_be_bytes());
        out[28..32].copy_from_slice(&48_000_u32.to_be_bytes());
        for (index, value) in [
            self.decoded_samples,
            self.skip_start_samples,
            self.skip_end_samples,
            self.packet_size,
            self.fragment_index,
            self.fragment_count,
            self.offset,
            self.payload_size,
        ]
        .into_iter()
        .enumerate()
        {
            out[32 + index * 2..34 + index * 2].copy_from_slice(&value.to_be_bytes());
        }
        Ok(out)
    }

    pub fn decode(data: &[u8]) -> Result<(Self, &[u8]), &'static str> {
        if data.len() <= HEADER_SIZE
            || data[..2] != MAGIC
            || data[2] != 1
            || data[3] != 2
            || data[28..32] != 48_000_u32.to_be_bytes()
        {
            return Err("invalid audio datagram");
        }
        let word = |offset| u16::from_be_bytes(data[offset..offset + 2].try_into().unwrap());
        let header = Self {
            generation: u64::from_be_bytes(data[4..12].try_into().unwrap()),
            sequence: u64::from_be_bytes(data[12..20].try_into().unwrap()),
            captured_at_micros: u64::from_be_bytes(data[20..28].try_into().unwrap()),
            decoded_samples: word(32),
            skip_start_samples: word(34),
            skip_end_samples: word(36),
            packet_size: word(38),
            fragment_index: word(40),
            fragment_count: word(42),
            offset: word(44),
            payload_size: word(46),
        };
        if !header.valid() || data.len() != HEADER_SIZE + usize::from(header.payload_size) {
            return Err("invalid audio fragment size");
        }
        Ok((header, &data[HEADER_SIZE..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_matches_fixed_network_fixture() {
        let h = Header {
            generation: 1,
            sequence: 2,
            captured_at_micros: 3,
            decoded_samples: 480,
            skip_start_samples: 312,
            skip_end_samples: 0,
            packet_size: 1,
            fragment_index: 0,
            fragment_count: 1,
            offset: 0,
            payload_size: 1,
        };
        let expected = [
            82, 65, 1, 2, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 3,
            0, 0, 187, 128, 1, 224, 1, 56, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1,
        ];
        assert_eq!(h.encode().unwrap(), expected);
        let mut bytes = expected.to_vec();
        bytes.push(42);
        assert_eq!(Header::decode(&bytes).unwrap(), (h, &[42][..]));
        for index in [2, 3, 28, 38, 42, 46] {
            let mut bad = bytes.clone();
            bad[index] = 255;
            assert!(Header::decode(&bad).is_err());
        }
        assert!(Header::decode(&bytes[..HEADER_SIZE]).is_err());
    }
}
