//! Fixed-size records carried on the optional diagnostics QUIC stream.

pub const STREAM_PREAMBLE: [u8; 6] = *b"RCDG\0\x02";
pub const RECORD_SIZE: usize = 322;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MediaKind {
    Audio = 1,
    Video = 2,
}

impl TryFrom<u8> for MediaKind {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Audio),
            2 => Ok(Self::Video),
            _ => Err("unknown diagnostic media kind"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PayloadDigest {
    pub kind: MediaKind,
    pub generation: u64,
    pub sequence: u64,
    pub payload_size: u64,
    pub hashed_at_micros: u64,
    pub producer_hash_duration_micros: u64,
    pub boundary_hash_duration_micros: u64,
    pub producer_dropped_records: u64,
    pub boundary_matched: bool,
    pub sha256: [u8; 32],
    pub encode_started_at_micros: u64,
    pub encoded_at_micros: u64,
    pub worker_queued_at_micros: u64,
    pub service_received_at_micros: u64,
    pub packetized_at_micros: u64,
    pub captured_at_micros: u64,
    pub mirror_decode_micros: u64,
    pub quality_present: bool,
    pub quality_presentation_timestamp: i64,
    pub source_readback_micros: u64,
    pub decoded_readback_micros: u64,
    pub scoring_micros: u64,
    pub readback_bytes: u64,
    pub luma_psnr_millidecibels: u64,
    pub luma_mean_absolute_error_ppm: u64,
    pub packetization_completed_at_micros: u64,
    pub first_send_attempt_at_micros: u64,
    pub last_send_completed_at_micros: u64,
    pub capture_acquisition_micros: u64,
    pub cross_adapter_copy_micros: u64,
    pub color_conversion_micros: u64,
    pub encoder_call_micros: u64,
    pub audio_capture_buffer_frames: u64,
    pub audio_capture_discontinuities: u64,
    pub audio_invalid_capture_timestamps: u64,
    pub audio_device_reopens: u64,
    pub audio_encoder_resets: u64,
    pub audio_capture_queue_depth: u64,
    pub audio_capture_queue_capacity: u64,
    pub audio_capture_queue_drops: u64,
}

impl PayloadDigest {
    #[must_use]
    pub fn encode(self) -> [u8; RECORD_SIZE] {
        let mut bytes = [0; RECORD_SIZE];
        bytes[0] = self.kind as u8;
        for (offset, value) in [
            (1, self.generation),
            (9, self.sequence),
            (17, self.payload_size),
            (25, self.hashed_at_micros),
            (33, self.producer_hash_duration_micros),
            (41, self.boundary_hash_duration_micros),
            (49, self.producer_dropped_records),
        ] {
            bytes[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
        }
        bytes[57] = u8::from(self.boundary_matched) | (u8::from(self.quality_present) << 1);
        bytes[58..90].copy_from_slice(&self.sha256);
        for (offset, value) in [
            (90, self.encode_started_at_micros),
            (98, self.encoded_at_micros),
            (106, self.worker_queued_at_micros),
            (114, self.service_received_at_micros),
            (122, self.packetized_at_micros),
            (130, self.captured_at_micros),
            (138, self.mirror_decode_micros),
            (146, self.quality_presentation_timestamp as u64),
            (154, self.source_readback_micros),
            (162, self.decoded_readback_micros),
            (170, self.scoring_micros),
            (178, self.readback_bytes),
            (186, self.luma_psnr_millidecibels),
            (194, self.luma_mean_absolute_error_ppm),
            (202, self.packetization_completed_at_micros),
            (210, self.first_send_attempt_at_micros),
            (218, self.last_send_completed_at_micros),
            (226, self.capture_acquisition_micros),
            (234, self.cross_adapter_copy_micros),
            (242, self.color_conversion_micros),
            (250, self.encoder_call_micros),
            (258, self.audio_capture_buffer_frames),
            (266, self.audio_capture_discontinuities),
            (274, self.audio_invalid_capture_timestamps),
            (282, self.audio_device_reopens),
            (290, self.audio_encoder_resets),
            (298, self.audio_capture_queue_depth),
            (306, self.audio_capture_queue_capacity),
            (314, self.audio_capture_queue_drops),
        ] {
            bytes[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() != RECORD_SIZE {
            return Err("invalid diagnostic digest record size");
        }
        let word = |offset| u64::from_be_bytes(bytes[offset..offset + 8].try_into().unwrap());
        if bytes[57] & !3 != 0 {
            return Err("invalid diagnostic flags");
        }
        let boundary_matched = bytes[57] & 1 != 0;
        Ok(Self {
            kind: MediaKind::try_from(bytes[0])?,
            generation: word(1),
            sequence: word(9),
            payload_size: word(17),
            hashed_at_micros: word(25),
            producer_hash_duration_micros: word(33),
            boundary_hash_duration_micros: word(41),
            producer_dropped_records: word(49),
            boundary_matched,
            quality_present: bytes[57] & 2 != 0,
            sha256: bytes[58..90].try_into().unwrap(),
            encode_started_at_micros: word(90),
            encoded_at_micros: word(98),
            worker_queued_at_micros: word(106),
            service_received_at_micros: word(114),
            packetized_at_micros: word(122),
            captured_at_micros: word(130),
            mirror_decode_micros: word(138),
            quality_presentation_timestamp: word(146) as i64,
            source_readback_micros: word(154),
            decoded_readback_micros: word(162),
            scoring_micros: word(170),
            readback_bytes: word(178),
            luma_psnr_millidecibels: word(186),
            luma_mean_absolute_error_ppm: word(194),
            packetization_completed_at_micros: word(202),
            first_send_attempt_at_micros: word(210),
            last_send_completed_at_micros: word(218),
            capture_acquisition_micros: word(226),
            cross_adapter_copy_micros: word(234),
            color_conversion_micros: word(242),
            encoder_call_micros: word(250),
            audio_capture_buffer_frames: word(258),
            audio_capture_discontinuities: word(266),
            audio_invalid_capture_timestamps: word(274),
            audio_device_reopens: word(282),
            audio_encoder_resets: word(290),
            audio_capture_queue_depth: word(298),
            audio_capture_queue_capacity: word(306),
            audio_capture_queue_drops: word(314),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_digest_has_a_fixed_round_trip() {
        let record = PayloadDigest {
            kind: MediaKind::Video,
            generation: 3,
            sequence: 4,
            payload_size: 5,
            hashed_at_micros: 6,
            producer_hash_duration_micros: 7,
            boundary_hash_duration_micros: 8,
            producer_dropped_records: 9,
            boundary_matched: true,
            sha256: [8; 32],
            encode_started_at_micros: 10,
            encoded_at_micros: 11,
            worker_queued_at_micros: 12,
            service_received_at_micros: 13,
            packetized_at_micros: 14,
            captured_at_micros: 9,
            mirror_decode_micros: 15,
            quality_present: true,
            quality_presentation_timestamp: -4,
            source_readback_micros: 16,
            decoded_readback_micros: 17,
            scoring_micros: 18,
            readback_bytes: 19,
            luma_psnr_millidecibels: 20,
            luma_mean_absolute_error_ppm: 21,
            packetization_completed_at_micros: 22,
            first_send_attempt_at_micros: 23,
            last_send_completed_at_micros: 24,
            capture_acquisition_micros: 25,
            cross_adapter_copy_micros: 26,
            color_conversion_micros: 27,
            encoder_call_micros: 28,
            audio_capture_buffer_frames: 29,
            audio_capture_discontinuities: 30,
            audio_invalid_capture_timestamps: 31,
            audio_device_reopens: 32,
            audio_encoder_resets: 33,
            audio_capture_queue_depth: 34,
            audio_capture_queue_capacity: 35,
            audio_capture_queue_drops: 36,
        };
        let bytes = record.encode();
        assert_eq!(bytes.len(), RECORD_SIZE);
        assert_eq!(PayloadDigest::decode(&bytes), Ok(record));
        assert!(PayloadDigest::decode(&bytes[..RECORD_SIZE - 1]).is_err());
        let mut unknown = bytes;
        unknown[0] = 0;
        assert!(PayloadDigest::decode(&unknown).is_err());
    }
}
