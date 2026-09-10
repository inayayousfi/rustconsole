//! Bounded AV1 datagram packetization and two-frame assembly.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, Instant};

pub const VIDEO_DATAGRAM_HEADER_SIZE: usize = 68;
pub const MAX_ENCODED_FRAME_SIZE: usize = 16 * 1024 * 1024;
const MAGIC: [u8; 2] = *b"RC";
const VERSION: u8 = 4;
const KEYFRAME_FLAG: u8 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoFramePayload {
    pub sequence: u64,
    pub captured_at_micros: u64,
    pub encoded_at_micros: u64,
    pub packetized_at_micros: u64,
    pub input_sequence: u64,
    pub keyframe: bool,
    pub target_bitrate_bits_per_second: u64,
    pub estimated_capacity_bits_per_second: u64,
    pub payload: Vec<u8>,
}

pub fn packetize_video_frame(
    frame: &VideoFramePayload,
    maximum_datagram_size: usize,
) -> Result<Vec<Vec<u8>>, VideoDatagramError> {
    if frame.payload.is_empty() || frame.payload.len() > MAX_ENCODED_FRAME_SIZE {
        return Err(VideoDatagramError::InvalidFrameSize(frame.payload.len()));
    }
    let chunk_capacity = maximum_datagram_size
        .checked_sub(VIDEO_DATAGRAM_HEADER_SIZE)
        .filter(|capacity| *capacity > 0)
        .ok_or(VideoDatagramError::DatagramTooSmall(maximum_datagram_size))?;
    let chunk_count = frame.payload.len().div_ceil(chunk_capacity);
    let chunk_count = u16::try_from(chunk_count).map_err(|_| VideoDatagramError::TooManyChunks)?;
    let frame_size = u32::try_from(frame.payload.len())
        .map_err(|_| VideoDatagramError::InvalidFrameSize(frame.payload.len()))?;

    Ok(frame
        .payload
        .chunks(chunk_capacity)
        .enumerate()
        .map(|(index, chunk)| {
            let mut datagram = Vec::with_capacity(VIDEO_DATAGRAM_HEADER_SIZE + chunk.len());
            datagram.extend_from_slice(&MAGIC);
            datagram.push(VERSION);
            datagram.push(if frame.keyframe { KEYFRAME_FLAG } else { 0 });
            datagram.extend_from_slice(&frame.sequence.to_be_bytes());
            datagram.extend_from_slice(&frame.captured_at_micros.to_be_bytes());
            datagram.extend_from_slice(&frame.encoded_at_micros.to_be_bytes());
            datagram.extend_from_slice(&frame.packetized_at_micros.to_be_bytes());
            datagram.extend_from_slice(&frame.input_sequence.to_be_bytes());
            datagram.extend_from_slice(&frame.target_bitrate_bits_per_second.to_be_bytes());
            datagram.extend_from_slice(&frame.estimated_capacity_bits_per_second.to_be_bytes());
            datagram.extend_from_slice(&frame_size.to_be_bytes());
            datagram.extend_from_slice(&(index as u16).to_be_bytes());
            datagram.extend_from_slice(&chunk_count.to_be_bytes());
            datagram.extend_from_slice(chunk);
            datagram
        })
        .collect())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Header {
    sequence: u64,
    captured_at: u64,
    encoded_at: u64,
    packetized_at: u64,
    input_sequence: u64,
    target_bitrate_bits_per_second: u64,
    estimated_capacity_bits_per_second: u64,
    frame_size: usize,
    chunk_index: usize,
    chunk_count: usize,
    keyframe: bool,
}

fn parse_header(datagram: &[u8]) -> Result<(Header, &[u8]), VideoDatagramError> {
    if datagram.len() <= VIDEO_DATAGRAM_HEADER_SIZE {
        return Err(VideoDatagramError::MalformedHeader);
    }
    if datagram[..2] != MAGIC || datagram[2] != VERSION || datagram[3] & !KEYFRAME_FLAG != 0 {
        return Err(VideoDatagramError::MalformedHeader);
    }
    let frame_size = u32::from_be_bytes(datagram[60..64].try_into().unwrap()) as usize;
    let chunk_index = u16::from_be_bytes(datagram[64..66].try_into().unwrap()) as usize;
    let chunk_count = u16::from_be_bytes(datagram[66..68].try_into().unwrap()) as usize;
    let payload = &datagram[VIDEO_DATAGRAM_HEADER_SIZE..];
    if frame_size == 0
        || frame_size > MAX_ENCODED_FRAME_SIZE
        || chunk_count == 0
        || chunk_count > frame_size
        || chunk_index >= chunk_count
        || payload.is_empty()
    {
        return Err(VideoDatagramError::MalformedHeader);
    }
    Ok((
        Header {
            sequence: u64::from_be_bytes(datagram[4..12].try_into().unwrap()),
            captured_at: u64::from_be_bytes(datagram[12..20].try_into().unwrap()),
            encoded_at: u64::from_be_bytes(datagram[20..28].try_into().unwrap()),
            packetized_at: u64::from_be_bytes(datagram[28..36].try_into().unwrap()),
            input_sequence: u64::from_be_bytes(datagram[36..44].try_into().unwrap()),
            target_bitrate_bits_per_second: u64::from_be_bytes(
                datagram[44..52].try_into().unwrap(),
            ),
            estimated_capacity_bits_per_second: u64::from_be_bytes(
                datagram[52..60].try_into().unwrap(),
            ),
            frame_size,
            chunk_index,
            chunk_count,
            keyframe: datagram[3] & KEYFRAME_FLAG != 0,
        },
        payload,
    ))
}

#[derive(Debug)]
struct PartialFrame {
    header: Header,
    started: Instant,
    budget: Duration,
    deadline: Instant,
    received_bytes: usize,
    chunks: Vec<Option<Vec<u8>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameAssemblyProgress {
    pub sequence: u64,
    pub frame_size: usize,
    pub received_chunks: usize,
    pub expected_chunks: usize,
    pub elapsed: Duration,
    pub budget: Duration,
    pub target_bitrate_bits_per_second: u64,
    pub estimated_capacity_bits_per_second: u64,
}

#[derive(Debug)]
pub struct VideoFrameAssembler {
    frame_period: Duration,
    partial: BTreeMap<u64, PartialFrame>,
    newest_sequence: Option<u64>,
    newest_completed_sequence: Option<u64>,
    stats: VideoAssemblyStats,
}

impl VideoFrameAssembler {
    #[must_use]
    pub fn new(frames_per_second: u16) -> Self {
        let frame_period = if frames_per_second == 0 {
            Duration::from_millis(100)
        } else {
            Duration::from_nanos(1_000_000_000 / u64::from(frames_per_second))
        };
        Self {
            frame_period,
            partial: BTreeMap::new(),
            newest_sequence: None,
            newest_completed_sequence: None,
            stats: VideoAssemblyStats::default(),
        }
    }

    #[must_use]
    pub const fn stats(&self) -> VideoAssemblyStats {
        self.stats
    }

    pub fn push(
        &mut self,
        datagram: &[u8],
        now: Instant,
        round_trip_time: Duration,
    ) -> Result<AssemblyResult, VideoDatagramError> {
        let mut dependency_lost = self.expire(now);
        let (header, payload) = parse_header(datagram)?;
        self.stats.received_chunks = self.stats.received_chunks.saturating_add(1);
        if self
            .newest_completed_sequence
            .is_some_and(|sequence| header.sequence <= sequence)
        {
            self.stats.late_chunks = self.stats.late_chunks.saturating_add(1);
            return Ok(AssemblyResult {
                frame: None,
                dependency_lost,
                progress: None,
            });
        }
        if self
            .newest_sequence
            .is_some_and(|sequence| header.sequence.saturating_add(2) < sequence)
        {
            self.stats.late_chunks = self.stats.late_chunks.saturating_add(1);
            return Ok(AssemblyResult {
                frame: None,
                dependency_lost: true,
                progress: None,
            });
        }
        self.newest_sequence = Some(
            self.newest_sequence
                .map_or(header.sequence, |sequence| sequence.max(header.sequence)),
        );

        if !self.partial.contains_key(&header.sequence) {
            while self.partial.len() >= 2 {
                if let Some(oldest) = self.partial.keys().next().copied() {
                    if let Some(discarded) = self.partial.remove(&oldest) {
                        self.stats.lost_chunks = self
                            .stats
                            .lost_chunks
                            .saturating_add(missing_chunks(&discarded));
                        self.stats.assembly_overflows =
                            self.stats.assembly_overflows.saturating_add(1);
                        self.stats.incomplete_frames =
                            self.stats.incomplete_frames.saturating_add(1);
                    }
                    dependency_lost = true;
                }
            }
            let budget = assembly_deadline(self.frame_period, round_trip_time);
            self.partial.insert(
                header.sequence,
                PartialFrame {
                    header,
                    started: now,
                    budget,
                    deadline: now + budget,
                    received_bytes: 0,
                    chunks: vec![None; header.chunk_count],
                },
            );
        }

        let existing = self.partial[&header.sequence].header;
        if existing.captured_at != header.captured_at
            || existing.encoded_at != header.encoded_at
            || existing.packetized_at != header.packetized_at
            || existing.input_sequence != header.input_sequence
            || existing.frame_size != header.frame_size
            || existing.chunk_count != header.chunk_count
            || existing.keyframe != header.keyframe
            || existing.target_bitrate_bits_per_second != header.target_bitrate_bits_per_second
            || existing.estimated_capacity_bits_per_second
                != header.estimated_capacity_bits_per_second
        {
            self.partial.remove(&header.sequence);
            return Err(VideoDatagramError::InconsistentFrameHeader);
        }
        let invalid_size = {
            let partial = self.partial.get_mut(&header.sequence).unwrap();
            if partial.chunks[header.chunk_index].is_none() {
                partial.received_bytes = partial
                    .received_bytes
                    .checked_add(payload.len())
                    .ok_or(VideoDatagramError::InvalidFrameSize(usize::MAX))?;
                if partial.received_bytes <= header.frame_size {
                    partial.chunks[header.chunk_index] = Some(payload.to_vec());
                }
            }
            (partial.received_bytes > header.frame_size).then_some(partial.received_bytes)
        };
        if let Some(size) = invalid_size {
            self.partial.remove(&header.sequence);
            return Err(VideoDatagramError::InvalidFrameSize(size));
        }
        let partial = self.partial.get(&header.sequence).unwrap();
        let progress = FrameAssemblyProgress {
            sequence: header.sequence,
            frame_size: header.frame_size,
            received_chunks: partial
                .chunks
                .iter()
                .filter(|chunk| chunk.is_some())
                .count(),
            expected_chunks: header.chunk_count,
            elapsed: now.saturating_duration_since(partial.started),
            budget: partial.budget,
            target_bitrate_bits_per_second: header.target_bitrate_bits_per_second,
            estimated_capacity_bits_per_second: header.estimated_capacity_bits_per_second,
        };
        if partial.chunks.iter().any(Option::is_none) {
            return Ok(AssemblyResult {
                frame: None,
                dependency_lost,
                progress: Some(progress),
            });
        }

        let partial = self.partial.remove(&header.sequence).unwrap();
        if partial.received_bytes != partial.header.frame_size {
            return Err(VideoDatagramError::InvalidFrameSize(partial.received_bytes));
        }
        dependency_lost |= self
            .newest_completed_sequence
            .is_some_and(|sequence| partial.header.sequence > sequence.saturating_add(1));
        let mut payload = Vec::with_capacity(partial.header.frame_size);
        for chunk in partial.chunks {
            payload.extend_from_slice(&chunk.unwrap());
        }
        self.newest_completed_sequence = Some(partial.header.sequence);
        self.stats.completed_frames = self.stats.completed_frames.saturating_add(1);
        self.stats.completed_payload_bytes = self
            .stats
            .completed_payload_bytes
            .saturating_add(partial.header.frame_size as u64);
        self.stats.last_completed_assembly_micros =
            u64::try_from(progress.elapsed.as_micros()).unwrap_or(u64::MAX);
        self.stats.last_assembly_budget_micros =
            u64::try_from(progress.budget.as_micros()).unwrap_or(u64::MAX);
        Ok(AssemblyResult {
            frame: Some(VideoFramePayload {
                sequence: partial.header.sequence,
                captured_at_micros: partial.header.captured_at,
                encoded_at_micros: partial.header.encoded_at,
                packetized_at_micros: partial.header.packetized_at,
                input_sequence: partial.header.input_sequence,
                keyframe: partial.header.keyframe,
                target_bitrate_bits_per_second: partial.header.target_bitrate_bits_per_second,
                estimated_capacity_bits_per_second: partial
                    .header
                    .estimated_capacity_bits_per_second,
                payload,
            }),
            dependency_lost,
            progress: Some(progress),
        })
    }

    fn expire(&mut self, now: Instant) -> bool {
        let expired = self
            .partial
            .iter()
            .filter_map(|(sequence, frame)| (frame.deadline <= now).then_some(*sequence))
            .collect::<Vec<_>>();
        for sequence in &expired {
            if let Some(frame) = self.partial.remove(sequence) {
                self.stats.lost_chunks = self
                    .stats
                    .lost_chunks
                    .saturating_add(missing_chunks(&frame));
                self.stats.incomplete_frames = self.stats.incomplete_frames.saturating_add(1);
            }
        }
        !expired.is_empty()
    }
}

fn missing_chunks(frame: &PartialFrame) -> u64 {
    frame.chunks.iter().filter(|chunk| chunk.is_none()).count() as u64
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VideoAssemblyStats {
    pub received_chunks: u64,
    pub lost_chunks: u64,
    pub late_chunks: u64,
    pub assembly_overflows: u64,
    pub completed_frames: u64,
    pub completed_payload_bytes: u64,
    pub incomplete_frames: u64,
    pub last_completed_assembly_micros: u64,
    pub last_assembly_budget_micros: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub struct AssemblyResult {
    pub frame: Option<VideoFramePayload>,
    pub dependency_lost: bool,
    pub progress: Option<FrameAssemblyProgress>,
}

#[must_use]
pub fn assembly_deadline(frame_period: Duration, round_trip_time: Duration) -> Duration {
    frame_period
        .saturating_mul(3)
        .max(round_trip_time.saturating_mul(2))
        .min(Duration::from_millis(100))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoDatagramError {
    DatagramTooSmall(usize),
    InvalidFrameSize(usize),
    TooManyChunks,
    MalformedHeader,
    InconsistentFrameHeader,
}

impl fmt::Display for VideoDatagramError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatagramTooSmall(size) => {
                write!(formatter, "datagram size {size} has no payload")
            }
            Self::InvalidFrameSize(size) => write!(formatter, "invalid encoded frame size {size}"),
            Self::TooManyChunks => formatter.write_str("encoded frame requires too many chunks"),
            Self::MalformedHeader => formatter.write_str("malformed video datagram header"),
            Self::InconsistentFrameHeader => {
                formatter.write_str("video chunks have inconsistent frame headers")
            }
        }
    }
}

impl std::error::Error for VideoDatagramError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(sequence: u64, size: usize) -> VideoFramePayload {
        VideoFramePayload {
            sequence,
            captured_at_micros: 42,
            encoded_at_micros: 52,
            packetized_at_micros: 62,
            input_sequence: 7,
            keyframe: sequence == 1,
            target_bitrate_bits_per_second: 20_000_000,
            estimated_capacity_bits_per_second: 24_000_000,
            payload: (0..size).map(|value| value as u8).collect(),
        }
    }

    #[test]
    fn packetization_round_trips_out_of_order() {
        let frame = frame(1, 2_000);
        let mut datagrams = packetize_video_frame(&frame, 1_200).unwrap();
        datagrams.reverse();
        let now = Instant::now();
        let mut assembler = VideoFrameAssembler::new(120);
        let mut assembled = None;
        for datagram in datagrams {
            assembled = assembler
                .push(&datagram, now, Duration::from_millis(5))
                .unwrap()
                .frame
                .or(assembled);
        }
        assert_eq!(assembled, Some(frame));
    }

    #[test]
    fn third_sequence_discards_the_oldest_incomplete_frame() {
        let now = Instant::now();
        let mut assembler = VideoFrameAssembler::new(120);
        for sequence in 1..=3 {
            let datagram = packetize_video_frame(&frame(sequence, 2_000), 1_200)
                .unwrap()
                .remove(0);
            let result = assembler
                .push(&datagram, now, Duration::from_millis(5))
                .unwrap();
            assert_eq!(result.dependency_lost, sequence == 3);
        }
        assert_eq!(
            assembler.partial.keys().copied().collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(assembler.stats().incomplete_frames, 1);
    }

    #[test]
    fn slow_ninety_one_chunk_frame_reports_progress_before_eviction() {
        let now = Instant::now();
        let datagram_size = 1_200;
        let chunk_capacity = datagram_size - VIDEO_DATAGRAM_HEADER_SIZE;
        let mut assembler = VideoFrameAssembler::new(120);
        let first = packetize_video_frame(&frame(1, chunk_capacity * 90 + 1), datagram_size)
            .unwrap()
            .remove(0);
        let progress = assembler
            .push(&first, now, Duration::from_millis(5))
            .unwrap()
            .progress
            .unwrap();
        assert_eq!(progress.received_chunks, 1);
        assert_eq!(progress.expected_chunks, 91);
        assert_eq!(progress.budget, Duration::from_nanos(24_999_999));

        for sequence in 2..=3 {
            let next = packetize_video_frame(&frame(sequence, 2_000), datagram_size)
                .unwrap()
                .remove(0);
            assembler
                .push(
                    &next,
                    now + Duration::from_millis(sequence * 8),
                    Duration::from_millis(5),
                )
                .unwrap();
        }
        assert_eq!(assembler.stats().incomplete_frames, 1);
        assert_eq!(assembler.stats().assembly_overflows, 1);
        assert_eq!(assembler.stats().lost_chunks, 90);
    }

    #[test]
    fn skipped_complete_sequence_loses_decoder_dependency() {
        let now = Instant::now();
        let mut assembler = VideoFrameAssembler::new(120);
        let first = packetize_video_frame(&frame(1, 16), 1_200)
            .unwrap()
            .remove(0);
        let third = packetize_video_frame(&frame(3, 16), 1_200)
            .unwrap()
            .remove(0);

        assert!(
            !assembler
                .push(&first, now, Duration::from_millis(5))
                .unwrap()
                .dependency_lost
        );
        let result = assembler
            .push(&third, now, Duration::from_millis(5))
            .unwrap();
        assert_eq!(result.frame, Some(frame(3, 16)));
        assert!(result.dependency_lost);
    }

    #[test]
    fn deadline_uses_three_frames_or_two_rtts_with_a_hard_cap() {
        assert_eq!(
            assembly_deadline(Duration::from_millis(8), Duration::from_millis(5)),
            Duration::from_millis(24)
        );
        assert_eq!(
            assembly_deadline(Duration::from_millis(8), Duration::from_millis(40)),
            Duration::from_millis(80)
        );
        assert_eq!(
            assembly_deadline(Duration::from_millis(8), Duration::from_millis(80)),
            Duration::from_millis(100)
        );
    }

    #[test]
    fn statistics_count_expiry_overflow_and_late_chunks() {
        let now = Instant::now();
        let mut assembler = VideoFrameAssembler::new(120);
        for sequence in 1..=3 {
            let datagram = packetize_video_frame(&frame(sequence, 2_000), 1_200)
                .unwrap()
                .remove(0);
            assembler
                .push(&datagram, now, Duration::from_millis(5))
                .unwrap();
        }
        assert_eq!(assembler.stats().assembly_overflows, 1);
        assert_eq!(assembler.stats().lost_chunks, 1);

        let completed = packetize_video_frame(&frame(4, 16), 1_200)
            .unwrap()
            .remove(0);
        assembler
            .push(&completed, now, Duration::from_millis(5))
            .unwrap();
        assert_eq!(assembler.stats().completed_frames, 1);
        assert_eq!(assembler.stats().completed_payload_bytes, 16);
        assert_eq!(assembler.stats().last_completed_assembly_micros, 0);
        assert_eq!(assembler.stats().last_assembly_budget_micros, 24_999);
        assembler
            .push(&completed, now, Duration::from_millis(5))
            .unwrap();
        assert_eq!(assembler.stats().late_chunks, 1);

        let later = now + Duration::from_millis(101);
        let next = packetize_video_frame(&frame(5, 16), 1_200)
            .unwrap()
            .remove(0);
        assembler
            .push(&next, later, Duration::from_millis(5))
            .unwrap();
        assert_eq!(assembler.stats().lost_chunks, 3);
        assert_eq!(assembler.stats().incomplete_frames, 3);
    }
}
