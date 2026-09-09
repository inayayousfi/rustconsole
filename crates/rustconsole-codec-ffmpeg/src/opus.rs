//! In-process stereo Opus encoding with shared, reference-counted input buffers.

use ffmpeg::{ChannelLayout, codec, ffi, format, frame};
use ffmpeg_next as ffmpeg;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::fmt;
use std::sync::Arc;

const SAMPLE_RATE: i64 = 48_000;
pub const MAX_INPUT_FRAMES: usize = 4_800;
// FFmpeg's libopus wrapper allocates this per stereo stream. This is not a QUIC limit.
const MAX_PACKET_BYTES: usize = 1_275 * 6 + 7;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpusEncoderConfiguration {
    pub bitrate_bits_per_second: u32,
    pub packet_duration_micros: u32,
}

impl OpusEncoderConfiguration {
    fn frame_samples(self) -> Result<usize, OpusError> {
        if !(500..=512_000).contains(&self.bitrate_bits_per_second) {
            return Err(OpusError::Invalid(
                "stereo bitrate must be between 500 and 512000 bits/s",
            ));
        }
        match self.packet_duration_micros {
            2_500 => Ok(120),
            5_000 => Ok(240),
            10_000 => Ok(480),
            20_000 => Ok(960),
            _ => Err(OpusError::Invalid(
                "packet duration must be 2500, 5000, 10000, or 20000 microseconds",
            )),
        }
    }
}

#[derive(Debug)]
pub enum OpusError {
    Invalid(&'static str),
    Allocation(&'static str),
    Ffmpeg(ffmpeg::Error),
}

impl fmt::Display for OpusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "Opus: {message}"),
            Self::Allocation(object) => write!(f, "Opus: could not allocate {object}"),
            Self::Ffmpeg(error) => write!(f, "FFmpeg Opus: {error}"),
        }
    }
}

impl std::error::Error for OpusError {}

impl From<ffmpeg::Error> for OpusError {
    fn from(error: ffmpeg::Error) -> Self {
        Self::Ffmpeg(error)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OpusEncoderStatistics {
    pub input_shared_bytes: u64,
    pub input_copied_bytes: u64,
    pub zero_padding_bytes: u64,
    pub packet_copied_bytes: u64,
    pub encoded_packets: u64,
    pub maximum_pending_frames: usize,
    pub resets: u64,
    pub discarded_samples: u64,
}

#[derive(Debug)]
pub struct OpusPacket {
    /// Time of the first retained sample, after skip_start_samples is applied.
    pub captured_at_micros: u64,
    pub decoded_samples: u16,
    pub skip_start_samples: u16,
    pub skip_end_samples: u16,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OpusDecoderStatistics {
    pub decoded_packets: u64,
    pub output_samples: u64,
    pub resets: u64,
}

pub struct OpusDecoder {
    decoder: ffmpeg::decoder::Audio,
    statistics: OpusDecoderStatistics,
}

impl OpusDecoder {
    pub fn open() -> Result<Self, OpusError> {
        ffmpeg::init()?;
        let codec =
            ffmpeg::decoder::find_by_name("libopus").ok_or(ffmpeg::Error::DecoderNotFound)?;
        let mut context = codec::Context::new_with_codec(codec);
        // SAFETY: this context is exclusively owned and has not been opened yet.
        unsafe {
            (*context.as_mut_ptr()).request_sample_fmt = ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT;
            (*context.as_mut_ptr()).ch_layout = ChannelLayout::STEREO.into();
        }
        let decoder = context.decoder().open_as(codec)?.audio()?;
        Ok(Self {
            decoder,
            statistics: OpusDecoderStatistics::default(),
        })
    }

    pub fn statistics(&self) -> OpusDecoderStatistics {
        self.statistics
    }

    pub fn decode(&mut self, packet: &OpusPacket) -> Result<Vec<f32>, OpusError> {
        let decoded_samples = usize::from(packet.decoded_samples);
        let skip_start = usize::from(packet.skip_start_samples);
        let skip_end = usize::from(packet.skip_end_samples);
        if packet.payload.is_empty()
            || packet.payload.len() > MAX_PACKET_BYTES
            || decoded_samples == 0
            || skip_start + skip_end > decoded_samples
        {
            return Err(OpusError::Invalid("invalid encoded packet"));
        }
        self.decoder
            .send_packet(&ffmpeg::Packet::copy(&packet.payload))?;
        let mut frame = frame::Audio::empty();
        self.decoder.receive_frame(&mut frame)?;
        if frame.samples() != decoded_samples
            || frame.channels() != 2
            || frame.rate() != SAMPLE_RATE as u32
            || frame.format() != format::Sample::F32(format::sample::Type::Packed)
        {
            return Err(OpusError::Invalid("decoder returned an unexpected format"));
        }
        let samples = frame.plane::<(f32, f32)>(0);
        let mut output = Vec::with_capacity((decoded_samples - skip_start - skip_end) * 2);
        for &(left, right) in &samples[skip_start..decoded_samples - skip_end] {
            output.push(left);
            output.push(right);
        }
        self.statistics.decoded_packets += 1;
        self.statistics.output_samples += output.len() as u64;
        Ok(output)
    }

    pub fn reset(&mut self) {
        self.decoder.flush();
        self.statistics.resets += 1;
    }
}

pub struct OpusEncoder {
    encoder: ffmpeg::encoder::Audio,
    configuration: OpusEncoderConfiguration,
    frame_samples: usize,
    delay_samples: u16,
    pending: Vec<f32>,
    timeline: VecDeque<(i64, u64)>,
    received_samples: i64,
    submitted_samples: i64,
    emitted_samples: u64,
    last_input_timestamp: Option<u64>,
    finished: bool,
    failed: bool,
    statistics: OpusEncoderStatistics,
}

impl OpusEncoder {
    pub fn open(configuration: OpusEncoderConfiguration) -> Result<Self, OpusError> {
        let frame_samples = configuration.frame_samples()?;
        ffmpeg::init()?;
        let codec =
            ffmpeg::encoder::find_by_name("libopus").ok_or(ffmpeg::Error::EncoderNotFound)?;
        // SAFETY: the codec descriptor is live; ownership transfers to Context only
        // after checking allocation. Its Drop owns avcodec_free_context.
        let context = unsafe {
            let raw = ffi::avcodec_alloc_context3(codec.as_ptr());
            if raw.is_null() {
                return Err(OpusError::Allocation("codec context"));
            }
            codec::Context::wrap(raw, None)
        };
        let mut encoder = context.encoder().audio()?;
        encoder.set_rate(SAMPLE_RATE as i32);
        encoder.set_channel_layout(ChannelLayout::STEREO);
        encoder.set_format(format::Sample::F32(format::sample::Type::Packed));
        encoder.set_time_base((1, SAMPLE_RATE as i32));
        encoder.set_bit_rate(configuration.bitrate_bits_per_second as usize);
        let mut options = ffmpeg::Dictionary::new();
        options.set(
            "frame_duration",
            &(f64::from(configuration.packet_duration_micros) / 1000.0).to_string(),
        );
        let encoder = encoder.open_as_with(codec, options)?;
        if encoder.frame_size() as usize != frame_samples {
            return Err(OpusError::Invalid(
                "libopus changed the requested frame size",
            ));
        }
        // SAFETY: opening the context initialized the encoder delay field.
        let delay = unsafe { (*encoder.as_ptr()).initial_padding };
        let delay_samples =
            u16::try_from(delay).map_err(|_| OpusError::Invalid("invalid encoder delay"))?;
        Ok(Self {
            encoder,
            configuration,
            frame_samples,
            delay_samples,
            pending: Vec::new(),
            timeline: VecDeque::new(),
            received_samples: 0,
            submitted_samples: 0,
            emitted_samples: 0,
            last_input_timestamp: None,
            finished: false,
            failed: false,
            statistics: OpusEncoderStatistics::default(),
        })
    }

    pub fn delay_samples(&self) -> u16 {
        self.delay_samples
    }

    pub fn statistics(&self) -> OpusEncoderStatistics {
        self.statistics
    }

    pub fn pending_samples(&self) -> usize {
        self.pending.len() / 2
    }

    /// Input is packed stereo float at 48 kHz. A discontinuity requires reset.
    pub fn encode(
        &mut self,
        captured_at_micros: u64,
        samples: Vec<f32>,
    ) -> Result<Vec<OpusPacket>, OpusError> {
        if self.finished || self.failed {
            return Err(OpusError::Invalid(
                "encoder needs reset before accepting more samples",
            ));
        }
        if samples.is_empty()
            || !samples.len().is_multiple_of(2)
            || samples.len() > MAX_INPUT_FRAMES * 2
        {
            return Err(OpusError::Invalid(
                "input must contain 1 to 4800 complete stereo frames",
            ));
        }
        if samples.iter().any(|value| !value.is_finite()) {
            return Err(OpusError::Invalid("input contains a non-finite sample"));
        }
        if self
            .last_input_timestamp
            .is_some_and(|previous| captured_at_micros <= previous)
        {
            return Err(OpusError::Invalid(
                "capture time must advance; reset after a discontinuity",
            ));
        }
        if self.timeline.len() >= MAX_INPUT_FRAMES {
            return Err(OpusError::Invalid(
                "capture timestamp queue reached its bound",
            ));
        }
        let next = self
            .received_samples
            .checked_add((samples.len() / 2) as i64)
            .filter(|value| *value <= i64::MAX - MAX_INPUT_FRAMES as i64)
            .ok_or(OpusError::Invalid("sample position overflow"))?;
        captured_at_micros
            .checked_add(100_000)
            .ok_or(OpusError::Invalid("capture timestamp overflow"))?;
        self.timeline
            .push_back((self.received_samples, captured_at_micros));
        self.received_samples = next;
        self.last_input_timestamp = Some(captured_at_micros);
        let previously_emitted = self.emitted_samples;
        let result = self.encode_batch(Arc::new(samples));
        if result.is_err() {
            self.failed = true;
            self.emitted_samples = previously_emitted;
        }
        result
    }

    fn encode_batch(&mut self, samples: Arc<Vec<f32>>) -> Result<Vec<OpusPacket>, OpusError> {
        let mut packets = Vec::new();
        let mut offset = 0;
        let frame_len = self.frame_samples * 2;
        if !self.pending.is_empty() {
            let count = (frame_len - self.pending.len()).min(samples.len());
            self.pending.extend_from_slice(&samples[..count]);
            self.statistics.input_copied_bytes += (count * size_of::<f32>()) as u64;
            self.statistics.maximum_pending_frames = self
                .statistics
                .maximum_pending_frames
                .max(self.pending_samples());
            offset += count;
            if self.pending.len() == frame_len {
                let assembled = Arc::new(std::mem::take(&mut self.pending));
                self.submit(assembled, 0, &mut packets)?;
            }
        }
        while samples.len() - offset >= frame_len {
            self.submit(Arc::clone(&samples), offset, &mut packets)?;
            self.statistics.input_shared_bytes += (frame_len * size_of::<f32>()) as u64;
            offset += frame_len;
        }
        if offset < samples.len() {
            if self.pending.capacity() < frame_len {
                self.pending.reserve_exact(frame_len - self.pending.len());
            }
            self.pending.extend_from_slice(&samples[offset..]);
            self.statistics.input_copied_bytes +=
                ((samples.len() - offset) * size_of::<f32>()) as u64;
            self.statistics.maximum_pending_frames = self
                .statistics
                .maximum_pending_frames
                .max(self.pending_samples());
        }
        Ok(packets)
    }

    fn submit(
        &mut self,
        samples: Arc<Vec<f32>>,
        offset: usize,
        packets: &mut Vec<OpusPacket>,
    ) -> Result<(), OpusError> {
        let frame = shared_frame(samples, offset, self.frame_samples, self.submitted_samples)?;
        self.encoder.send_frame(&frame)?;
        self.submitted_samples += self.frame_samples as i64;
        self.receive(packets, false)
    }

    fn receive(&mut self, packets: &mut Vec<OpusPacket>, draining: bool) -> Result<(), OpusError> {
        loop {
            let mut packet = ffmpeg::Packet::empty();
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {}
                Err(ffmpeg::Error::Other { errno })
                    if errno == ffmpeg::error::EAGAIN && !draining =>
                {
                    return Ok(());
                }
                Err(ffmpeg::Error::Eof) if draining => return Ok(()),
                Err(error) => return Err(error.into()),
            }
            let pts = packet
                .pts()
                .ok_or(OpusError::Invalid("encoded packet has no timestamp"))?;
            if pts < -i64::from(self.delay_samples) || pts > self.submitted_samples {
                return Err(OpusError::Invalid(
                    "encoded packet timestamp is outside submitted audio",
                ));
            }
            let start = (-pts).max(0).min(self.frame_samples as i64) as u16;
            let end = (pts + self.frame_samples as i64 - self.received_samples)
                .max(0)
                .min(self.frame_samples as i64 - i64::from(start)) as u16;
            let audible_position = pts.max(0).min(self.received_samples);
            while self
                .timeline
                .get(1)
                .is_some_and(|(position, _)| *position <= audible_position)
            {
                self.timeline.pop_front();
            }
            let &(position, timestamp) = self
                .timeline
                .front()
                .ok_or(OpusError::Invalid("packet has no capture time"))?;
            let elapsed = ((audible_position - position) as u64 * 1_000_000) / SAMPLE_RATE as u64;
            let captured_at_micros = timestamp
                .checked_add(elapsed)
                .ok_or(OpusError::Invalid("packet timestamp overflow"))?;
            let payload = packet
                .data()
                .ok_or(OpusError::Invalid("empty encoded packet"))?;
            if payload.is_empty() || payload.len() > MAX_PACKET_BYTES {
                return Err(OpusError::Invalid(
                    "encoded packet exceeds the FFmpeg libopus output bound",
                ));
            }
            self.statistics.packet_copied_bytes += payload.len() as u64;
            self.statistics.encoded_packets += 1;
            self.emitted_samples +=
                (self.frame_samples - usize::from(start) - usize::from(end)) as u64;
            packets.push(OpusPacket {
                captured_at_micros,
                decoded_samples: self.frame_samples as u16,
                skip_start_samples: start,
                skip_end_samples: end,
                payload: payload.to_vec(),
            });
            let next_position = (pts + self.frame_samples as i64).max(0);
            while self
                .timeline
                .get(1)
                .is_some_and(|(position, _)| *position <= next_position)
            {
                self.timeline.pop_front();
            }
        }
    }

    pub fn finish(&mut self) -> Result<Vec<OpusPacket>, OpusError> {
        if self.failed {
            return Err(OpusError::Invalid("failed encoder needs reset"));
        }
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        if self.received_samples == 0 {
            return Ok(Vec::new());
        }
        let previously_emitted = self.emitted_samples;
        let result = (|| {
            let mut packets = Vec::new();
            if !self.pending.is_empty() {
                let missing = self.frame_samples * 2 - self.pending.len();
                self.statistics.zero_padding_bytes += (missing * size_of::<f32>()) as u64;
                self.pending.resize(self.frame_samples * 2, 0.0);
                let assembled = Arc::new(std::mem::take(&mut self.pending));
                self.submit(assembled, 0, &mut packets)?;
            }
            self.encoder.send_eof()?;
            self.receive(&mut packets, true)?;
            Ok(packets)
        })();
        if result.is_err() {
            self.failed = true;
            self.emitted_samples = previously_emitted;
        }
        result
    }

    pub fn reset(&mut self) -> Result<u64, OpusError> {
        let discarded = (self.received_samples as u64).saturating_sub(self.emitted_samples);
        let mut replacement = Self::open(self.configuration)?;
        replacement.statistics = self.statistics;
        replacement.statistics.resets += 1;
        replacement.statistics.discarded_samples += discarded;
        *self = replacement;
        Ok(discarded)
    }
}

unsafe extern "C" fn release_samples(opaque: *mut c_void, _data: *mut u8) {
    // SAFETY: successful av_buffer_create owns this Box until its last reference
    // dies. Arc<Vec<f32>> has no user destructor and can be dropped on any thread.
    drop(unsafe { Box::from_raw(opaque.cast::<Arc<Vec<f32>>>()) });
}

fn shared_frame(
    samples: Arc<Vec<f32>>,
    offset: usize,
    count: usize,
    pts: i64,
) -> Result<frame::Audio, OpusError> {
    if count == 0
        || count > 960
        || !offset.is_multiple_of(2)
        || offset
            .checked_add(count * 2)
            .is_none_or(|end| end > samples.len())
    {
        return Err(OpusError::Invalid("shared sample range is invalid"));
    }
    let mut frame = frame::Audio::empty();
    // SAFETY: Frame owns its allocation, checked before any field access.
    unsafe {
        if frame.as_ptr().is_null() {
            return Err(OpusError::Allocation("audio frame"));
        }
    }
    frame.set_format(format::Sample::F32(format::sample::Type::Packed));
    frame.set_channel_layout(ChannelLayout::STEREO);
    frame.set_rate(SAMPLE_RATE as u32);
    frame.set_samples(count);
    frame.set_pts(Some(pts));
    let data = samples.as_ptr().cast_mut().cast::<u8>();
    // SAFETY: the boxed Arc retains read-only packed samples until FFmpeg releases
    // its last reference. The libopus wrapper does not modify or reorder stereo input.
    let buffer = sample_reference(samples, |data, bytes, owner| unsafe {
        ffi::av_buffer_create(
            data,
            bytes,
            Some(release_samples),
            owner,
            ffi::AV_BUFFER_FLAG_READONLY,
        )
    })?;
    // SAFETY: the frame takes ownership of the buffer reference. The validated
    // subrange lies inside that buffer and extended_data aliases the frame's own array.
    unsafe {
        let raw = frame.as_mut_ptr();
        (*raw).buf[0] = buffer;
        (*raw).data[0] = data.add(offset * size_of::<f32>());
        (*raw).extended_data = (*raw).data.as_mut_ptr();
        (*raw).linesize[0] = (count * 2 * size_of::<f32>()) as i32;
    }
    Ok(frame)
}

fn sample_reference(
    samples: Arc<Vec<f32>>,
    allocate: impl FnOnce(*mut u8, usize, *mut c_void) -> *mut ffi::AVBufferRef,
) -> Result<*mut ffi::AVBufferRef, OpusError> {
    let data = samples.as_ptr().cast_mut().cast();
    let bytes = samples.len() * size_of::<f32>();
    let owner = Box::into_raw(Box::new(samples));
    let buffer = allocate(data, bytes, owner.cast());
    if buffer.is_null() {
        // SAFETY: on allocation failure FFmpeg did not take ownership of the Box.
        drop(unsafe { Box::from_raw(owner) });
        Err(OpusError::Allocation("sample buffer reference"))
    } else {
        Ok(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_failure_requires_reset_and_counts_unreturned_samples_as_discarded() {
        let mut encoder = OpusEncoder::open(configuration(10_000)).unwrap();
        encoder.encoder.send_eof().unwrap();
        assert!(encoder.encode(0, signal(960)).is_err());
        assert!(encoder.encode(20_000, signal(480)).is_err());
        assert!(encoder.finish().is_err());
        assert_eq!(encoder.reset().unwrap(), 960);
        assert_eq!(encoder.statistics().discarded_samples, 960);
        let mut packets = encoder.encode(0, signal(480)).unwrap();
        packets.extend(encoder.finish().unwrap());
        assert_eq!(decode(&packets).len(), 960);
    }

    #[test]
    fn packet_time_uses_the_capture_batch_containing_its_first_retained_sample() {
        let mut encoder = OpusEncoder::open(configuration(10_000)).unwrap();
        let first = encoder.encode(1_000_000, signal(480)).unwrap();
        let second = encoder.encode(1_010_005, signal(480)).unwrap();
        let third = encoder.encode(1_020_010, signal(480)).unwrap();
        assert_eq!(first[0].captured_at_micros, 1_000_000);
        let remaining = 480 - u64::from(encoder.delay_samples());
        assert_eq!(
            second[0].captured_at_micros,
            1_000_000 + remaining * 1_000_000 / 48_000
        );
        assert_eq!(
            third[0].captured_at_micros,
            1_010_005 + remaining * 1_000_000 / 48_000
        );
        encoder.finish().unwrap();
    }

    #[test]
    fn failed_buffer_allocation_releases_owned_samples() {
        let samples = Arc::new(vec![0.0; 960]);
        let weak = Arc::downgrade(&samples);
        assert!(sample_reference(samples, |_, _, _| std::ptr::null_mut()).is_err());
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn bitrate_boundaries_produce_bounded_opus_packets() {
        for duration in [2_500, 5_000, 10_000, 20_000] {
            for bitrate in [500, 6_000, 64_000, 512_000] {
                let mut encoder = OpusEncoder::open(OpusEncoderConfiguration {
                    bitrate_bits_per_second: bitrate,
                    ..configuration(duration)
                })
                .unwrap();
                let mut packets = encoder.encode(0, signal(960)).unwrap();
                packets.extend(encoder.finish().unwrap());
                assert_eq!(decode(&packets).len(), 1920);
                assert!(
                    packets
                        .iter()
                        .all(|packet| packet.payload.len() <= MAX_PACKET_BYTES)
                );
            }
        }
    }

    fn configuration(duration: u32) -> OpusEncoderConfiguration {
        OpusEncoderConfiguration {
            bitrate_bits_per_second: 128_000,
            packet_duration_micros: duration,
        }
    }

    fn signal(frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                [440.0_f32, 660.0]
                    .map(|hz| 0.1 * (i as f32 * hz * std::f32::consts::TAU / 48_000.0).sin())
            })
            .collect()
    }

    fn decode(packets: &[OpusPacket]) -> Vec<f32> {
        let mut decoder = OpusDecoder::open().unwrap();
        let mut output = Vec::new();
        for packet in packets {
            output.extend(decoder.decode(packet).unwrap());
        }
        output
    }

    #[test]
    fn decoder_validates_trims_and_resets_packets() {
        let mut encoder = OpusEncoder::open(configuration(10_000)).unwrap();
        let packet = encoder.encode(1_000_000, signal(480)).unwrap().remove(0);
        let expected = (usize::from(packet.decoded_samples)
            - usize::from(packet.skip_start_samples)
            - usize::from(packet.skip_end_samples))
            * 2;
        let mut decoder = OpusDecoder::open().unwrap();
        assert_eq!(decoder.decode(&packet).unwrap().len(), expected);
        decoder.reset();
        assert_eq!(decoder.statistics().decoded_packets, 1);
        assert_eq!(decoder.statistics().output_samples, expected as u64);
        assert_eq!(decoder.statistics().resets, 1);

        let mut invalid = packet;
        invalid.skip_start_samples = invalid.decoded_samples;
        invalid.skip_end_samples = 1;
        assert!(decoder.decode(&invalid).is_err());
    }

    #[test]
    fn shared_frame_keeps_the_original_pointer_until_last_reference_drops() {
        let samples = Arc::new(signal(960));
        let weak = Arc::downgrade(&samples);
        let pointer = samples.as_ptr();
        let frame = shared_frame(samples, 960, 480, 0).unwrap();
        // SAFETY: frame is live, and av_frame_clone retains its referenced buffer.
        let mut clone = unsafe {
            assert_eq!(
                (*frame.as_ptr()).data[0].cast::<f32>(),
                pointer.add(960).cast_mut()
            );
            ffi::av_frame_clone(frame.as_ptr())
        };
        assert!(!clone.is_null());
        drop(frame);
        assert!(weak.upgrade().is_some());
        unsafe { ffi::av_frame_free(&mut clone) };
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn shared_input_is_released_on_range_error_and_codec_submission_error() {
        let samples = Arc::new(signal(480));
        let weak = Arc::downgrade(&samples);
        assert!(shared_frame(samples, 2, 480, 0).is_err());
        assert!(weak.upgrade().is_none());
        let samples = Arc::new(signal(480));
        let weak = Arc::downgrade(&samples);
        let frame = shared_frame(samples, 0, 480, 0).unwrap();
        let mut unopened = codec::Context::new().encoder().audio().unwrap();
        assert!(unopened.send_frame(&frame).is_err());
        drop(frame);
        drop(unopened);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn all_durations_round_trip_stereo_without_sample_copies() {
        for duration in [2_500, 5_000, 10_000, 20_000] {
            let mut encoder = OpusEncoder::open(configuration(duration)).unwrap();
            let original = signal(4_800);
            let mut packets = encoder.encode(1_000_000, original.clone()).unwrap();
            packets.extend(encoder.finish().unwrap());
            assert!(encoder.finish().unwrap().is_empty());
            assert!(encoder.encode(2_000_000, signal(480)).is_err());
            let decoded = decode(&packets);
            assert_eq!(decoded.len(), original.len());
            for channel in 0..2 {
                let error = original
                    .iter()
                    .skip(channel)
                    .step_by(2)
                    .zip(decoded.iter().skip(channel).step_by(2))
                    .map(|(a, b)| f64::from(a - b).powi(2))
                    .sum::<f64>()
                    / 4_800.0;
                assert!(
                    error.sqrt() < 0.025,
                    "{duration} us channel {channel}: RMS error {}",
                    error.sqrt()
                );
            }
            let stats = encoder.statistics();
            assert_eq!(stats.input_shared_bytes, 4_800 * 8);
            assert_eq!(stats.input_copied_bytes, 0);
            assert_eq!(stats.maximum_pending_frames, 0);
            assert_eq!(packets[0].captured_at_micros, 1_000_000);
            assert_eq!(
                packets
                    .iter()
                    .map(|packet| u64::from(packet.skip_start_samples))
                    .sum::<u64>(),
                u64::from(encoder.delay_samples())
            );
            assert_eq!(encoder.reset().unwrap(), 0);
        }
    }

    #[test]
    fn partial_frames_are_bounded_and_finish_trims_padding() {
        let mut encoder = OpusEncoder::open(configuration(10_000)).unwrap();
        let input = signal(1_337);
        let mut packets = Vec::new();
        let mut offset = 0;
        for frames in [1, 119, 361, 856] {
            packets.extend(
                encoder
                    .encode(
                        1_000_000 + offset as u64 * 1_000_000 / 48_000,
                        input[offset * 2..(offset + frames) * 2].to_vec(),
                    )
                    .unwrap(),
            );
            offset += frames;
            assert!(encoder.pending_samples() < 480);
        }
        packets.extend(encoder.finish().unwrap());
        assert_eq!(decode(&packets).len(), input.len());
        assert!(encoder.statistics().maximum_pending_frames <= 480);
        assert!(encoder.statistics().input_copied_bytes > 0);
        assert_eq!(
            encoder.statistics().input_copied_bytes + encoder.statistics().input_shared_bytes,
            1337 * 8
        );
        assert_eq!(encoder.statistics().zero_padding_bytes, (1440 - 1337) * 8);
    }

    #[test]
    fn reset_discards_partial_audio_and_accepts_a_new_clock_origin() {
        let mut encoder = OpusEncoder::open(configuration(20_000)).unwrap();
        assert!(encoder.encode(1_000_000, signal(480)).unwrap().is_empty());
        assert_eq!(encoder.reset().unwrap(), 480);
        let mut packets = encoder.encode(0, vec![0.0; 1920]).unwrap();
        packets.extend(encoder.finish().unwrap());
        assert_eq!(decode(&packets).len(), 1920);
        assert!(decode(&packets).iter().all(|value| value.abs() < 0.000_01));
        assert_eq!(packets[0].captured_at_micros, 0);
        assert_eq!(encoder.statistics().discarded_samples, 480);
    }

    #[test]
    fn invalid_settings_and_samples_do_not_change_encoder_state() {
        for duration in [0, 1, 3_000, 40_000] {
            assert!(OpusEncoder::open(configuration(duration)).is_err());
        }
        for bitrate in [0, 499, 512_001, u32::MAX] {
            assert!(
                OpusEncoder::open(OpusEncoderConfiguration {
                    bitrate_bits_per_second: bitrate,
                    ..configuration(10_000)
                })
                .is_err()
            );
        }
        let mut encoder = OpusEncoder::open(configuration(10_000)).unwrap();
        for samples in [
            vec![],
            vec![0.0],
            vec![0.0; 9602],
            vec![f32::NAN; 2],
            vec![f32::INFINITY; 2],
        ] {
            assert!(encoder.encode(1_000_000, samples).is_err());
        }
        assert!(encoder.encode(u64::MAX, signal(480)).is_err());
        assert_eq!(encoder.statistics(), OpusEncoderStatistics::default());
        assert!(!encoder.encode(1_000_000, signal(480)).unwrap().is_empty());
        assert!(encoder.encode(1_000_000, signal(480)).is_err());
        assert!(encoder.encode(999_999, signal(480)).is_err());
    }

    #[test]
    fn one_sample_batches_do_not_grow_the_timestamp_queue_without_bound() {
        let mut encoder = OpusEncoder::open(configuration(20_000)).unwrap();
        let mut packets = Vec::new();
        for i in 0..4_800_u64 {
            packets.extend(
                encoder
                    .encode(i * 1_000_000 / 48_000, vec![0.0; 2])
                    .unwrap(),
            );
            assert!(encoder.timeline.len() <= 960 + usize::from(encoder.delay_samples()) + 1);
        }
        packets.extend(encoder.finish().unwrap());
        assert_eq!(decode(&packets).len(), 9600);
    }
}
