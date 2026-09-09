pub use rustconsole_codec_ffmpeg::opus::OpusEncoderConfiguration;
use rustconsole_codec_ffmpeg::opus::{OpusEncoder, OpusEncoderStatistics, OpusError, OpusPacket};
use rustconsole_media::{
    AudioEncoder, AudioFormat, AudioSamples, EncodedAudioPacket, MediaTimestampMicros,
};

pub struct OpusAudioEncoder {
    encoder: OpusEncoder,
}

impl OpusAudioEncoder {
    pub fn new(configuration: OpusEncoderConfiguration) -> Result<Self, OpusError> {
        Ok(Self {
            encoder: OpusEncoder::open(configuration)?,
        })
    }

    pub fn statistics(&self) -> OpusEncoderStatistics {
        self.encoder.statistics()
    }

    pub fn delay_samples(&self) -> u16 {
        self.encoder.delay_samples()
    }
}

impl AudioEncoder for OpusAudioEncoder {
    type Error = OpusError;

    fn encode(&mut self, samples: AudioSamples) -> Result<Vec<EncodedAudioPacket>, Self::Error> {
        if samples.format
            != (AudioFormat {
                sample_rate: 48_000,
                channels: 2,
            })
        {
            return Err(OpusError::Invalid("host Opus input must be 48 kHz stereo"));
        }
        self.encoder
            .encode(samples.captured_at.0, samples.interleaved)
            .map(media_packets)
    }

    fn finish(&mut self) -> Result<Vec<EncodedAudioPacket>, Self::Error> {
        self.encoder.finish().map(media_packets)
    }

    fn reset(&mut self) -> Result<u64, Self::Error> {
        self.encoder.reset()
    }
}

fn media_packets(packets: Vec<OpusPacket>) -> Vec<EncodedAudioPacket> {
    packets
        .into_iter()
        .map(|packet| EncodedAudioPacket {
            captured_at: MediaTimestampMicros(packet.captured_at_micros),
            format: AudioFormat {
                sample_rate: 48_000,
                channels: 2,
            },
            decoded_samples: packet.decoded_samples,
            skip_start_samples: packet.skip_start_samples,
            skip_end_samples: packet.skip_end_samples,
            payload: packet.payload,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_checks_format_and_moves_packets_without_another_payload_copy() {
        let mut encoder = OpusAudioEncoder::new(OpusEncoderConfiguration {
            bitrate_bits_per_second: 128_000,
            packet_duration_micros: 10_000,
        })
        .unwrap();
        assert!(
            encoder
                .encode(AudioSamples {
                    captured_at: MediaTimestampMicros(1_000_000),
                    format: AudioFormat {
                        sample_rate: 44_100,
                        channels: 2
                    },
                    interleaved: vec![0.0; 960],
                })
                .is_err()
        );
        assert_eq!(encoder.statistics(), OpusEncoderStatistics::default());
        let packets = encoder
            .encode(AudioSamples {
                captured_at: MediaTimestampMicros(1_000_000),
                format: AudioFormat {
                    sample_rate: 48_000,
                    channels: 2,
                },
                interleaved: vec![0.0; 960],
            })
            .unwrap();
        assert_eq!(packets[0].captured_at, MediaTimestampMicros(1_000_000));
        assert_eq!(encoder.statistics().input_shared_bytes, 3840);
        assert_eq!(encoder.statistics().input_copied_bytes, 0);
        encoder.finish().unwrap();
        let payload = vec![1, 2, 3];
        let pointer = payload.as_ptr();
        let packets = media_packets(vec![OpusPacket {
            captured_at_micros: 7,
            decoded_samples: 480,
            skip_start_samples: 312,
            skip_end_samples: 0,
            payload,
        }]);
        assert_eq!(packets[0].payload.as_ptr(), pointer);
        assert_eq!(packets[0].skip_start_samples, 312);
    }
}
