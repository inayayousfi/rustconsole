//! Platform-neutral media types and codec interfaces.

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MediaTimestampMicros(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    pub frames_per_second: u16,
}

#[derive(Debug)]
pub struct VideoFrame<Frame> {
    pub sequence: u64,
    pub captured_at: MediaTimestampMicros,
    pub format: VideoFormat,
    pub frame: Frame,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedVideoFrame {
    pub sequence: u64,
    pub captured_at: MediaTimestampMicros,
    pub keyframe: bool,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioSamples {
    pub captured_at: MediaTimestampMicros,
    pub format: AudioFormat,
    pub interleaved: Vec<f32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedAudioPacket {
    pub captured_at: MediaTimestampMicros,
    pub format: AudioFormat,
    /// Decoded samples per channel, before trimming encoder delay or end padding.
    pub decoded_samples: u16,
    pub skip_start_samples: u16,
    pub skip_end_samples: u16,
    pub payload: Vec<u8>,
}

pub trait VideoEncoder<InputFrame> {
    type Error;

    fn encode(
        &mut self,
        frame: VideoFrame<InputFrame>,
    ) -> Result<Option<EncodedVideoFrame>, Self::Error>;

    fn request_keyframe(&mut self) -> Result<(), Self::Error>;

    fn set_bitrate(&mut self, bits_per_second: u64) -> Result<(), Self::Error>;
}

pub trait VideoDecoder {
    type OutputFrame;
    type Error;

    fn decode(
        &mut self,
        frame: EncodedVideoFrame,
    ) -> Result<Option<VideoFrame<Self::OutputFrame>>, Self::Error>;

    fn reset(&mut self) -> Result<(), Self::Error>;
}

pub trait AudioEncoder {
    type Error;

    fn encode(&mut self, samples: AudioSamples) -> Result<Vec<EncodedAudioPacket>, Self::Error>;

    fn finish(&mut self) -> Result<Vec<EncodedAudioPacket>, Self::Error>;

    /// Start an independent stream; return the number of un-emitted samples per channel discarded.
    fn reset(&mut self) -> Result<u64, Self::Error>;
}

pub trait AudioDecoder {
    type Error;

    fn decode(&mut self, packet: EncodedAudioPacket) -> Result<AudioSamples, Self::Error>;

    fn reset(&mut self) -> Result<(), Self::Error>;
}
