//! Bounded AV1 datagram packetization and two-frame assembly.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, Instant};

pub const VIDEO_DATAGRAM_HEADER_SIZE: usize = 76;
pub use rustconsole_protocol::{LEGACY_VIDEO_DATAGRAM_VERSION, VIDEO_DATAGRAM_VERSION};
pub const MAX_ENCODED_FRAME_SIZE: usize = 16 * 1024 * 1024;
const MAGIC: [u8; 2] = *b"RC";
const LEGACY_HEADER_SIZE: usize = 68;
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
    pub soft_ceiling_bits_per_second: Option<u64>,
    pub payload: Vec<u8>,
}

pub fn packetize_video_frame(
    frame: &VideoFramePayload,
    maximum_datagram_size: usize,
) -> Result<Vec<Vec<u8>>, VideoDatagramError> {
    packetize_video_frame_for_version(frame, maximum_datagram_size, VIDEO_DATAGRAM_VERSION)
}

pub fn packetize_video_frame_for_version(
    frame: &VideoFramePayload,
    maximum_datagram_size: usize,
    version: u32,
) -> Result<Vec<Vec<u8>>, VideoDatagramError> {
    if frame.payload.is_empty() || frame.payload.len() > MAX_ENCODED_FRAME_SIZE {
        return Err(VideoDatagramError::InvalidFrameSize(frame.payload.len()));
    }
    let header_size = match version {
        LEGACY_VIDEO_DATAGRAM_VERSION => LEGACY_HEADER_SIZE,
        VIDEO_DATAGRAM_VERSION => VIDEO_DATAGRAM_HEADER_SIZE,
        _ => return Err(VideoDatagramError::UnsupportedVersion(version)),
    };
    let chunk_capacity = maximum_datagram_size
        .checked_sub(header_size)
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
            let mut datagram = Vec::with_capacity(header_size + chunk.len());
            datagram.extend_from_slice(&MAGIC);
            datagram.push(version as u8);
            datagram.push(if frame.keyframe { KEYFRAME_FLAG } else { 0 });
            datagram.extend_from_slice(&frame.sequence.to_be_bytes());
            datagram.extend_from_slice(&frame.captured_at_micros.to_be_bytes());
            datagram.extend_from_slice(&frame.encoded_at_micros.to_be_bytes());
            datagram.extend_from_slice(&frame.packetized_at_micros.to_be_bytes());
            datagram.extend_from_slice(&frame.input_sequence.to_be_bytes());
            datagram.extend_from_slice(&frame.target_bitrate_bits_per_second.to_be_bytes());
            datagram.extend_from_slice(&frame.estimated_capacity_bits_per_second.to_be_bytes());
            if version == VIDEO_DATAGRAM_VERSION {
                datagram.extend_from_slice(
                    &frame
                        .soft_ceiling_bits_per_second
                        .unwrap_or(0)
                        .to_be_bytes(),
                );
            }
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
    soft_ceiling_bits_per_second: Option<u64>,
    frame_size: usize,
    chunk_index: usize,
    chunk_count: usize,
    keyframe: bool,
}

fn parse_header(datagram: &[u8]) -> Result<(Header, &[u8]), VideoDatagramError> {
    if datagram.len() <= LEGACY_HEADER_SIZE {
        return Err(VideoDatagramError::MalformedHeader);
    }
    let version = u32::from(datagram[2]);
    let header_size = match version {
        LEGACY_VIDEO_DATAGRAM_VERSION => LEGACY_HEADER_SIZE,
        VIDEO_DATAGRAM_VERSION => VIDEO_DATAGRAM_HEADER_SIZE,
        _ => return Err(VideoDatagramError::MalformedHeader),
    };
    if datagram.len() <= header_size || datagram[..2] != MAGIC || datagram[3] & !KEYFRAME_FLAG != 0
    {
        return Err(VideoDatagramError::MalformedHeader);
    }
    let metadata_end = if version == VIDEO_DATAGRAM_VERSION {
        68
    } else {
        60
    };
    let frame_size =
        u32::from_be_bytes(datagram[metadata_end..metadata_end + 4].try_into().unwrap()) as usize;
    let chunk_index = u16::from_be_bytes(
        datagram[metadata_end + 4..metadata_end + 6]
            .try_into()
            .unwrap(),
    ) as usize;
    let chunk_count = u16::from_be_bytes(
        datagram[metadata_end + 6..metadata_end + 8]
            .try_into()
            .unwrap(),
    ) as usize;
    let payload = &datagram[header_size..];
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
            soft_ceiling_bits_per_second: (version == VIDEO_DATAGRAM_VERSION)
                .then(|| u64::from_be_bytes(datagram[60..68].try_into().unwrap()))
                .filter(|ceiling| *ceiling != 0),
            frame_size,
            chunk_index,
            chunk_count,
            keyframe: datagram[3] & KEYFRAME_FLAG != 0,
        },
        payload,
    ))
}

/// Frames normally arrive one period apart, so a longer gap means the link went quiet.
const SILENCE_FRAME_PERIODS: u32 = 2;
const PARTIAL_FRAMES: usize = 2;
/// Frames held while a resend is outstanding. It covers the repair time at 120 frames per second.
const RESEND_PARTIAL_FRAMES: usize = 8;
/// A repair needs one gap, one round trip, and a little scheduling time.
const RESEND_MINIMUM_BUDGET: Duration = Duration::from_millis(50);
const RESEND_RETRY_MARGIN: Duration = Duration::from_millis(5);
/// The host sends a frame's chunks back to back, so this much quiet means the rest were lost.
const FRAME_TAIL_QUIET: Duration = Duration::from_millis(4);

const CHUNK_REQUEST_MAGIC: [u8; 2] = *b"RQ";
const CHUNK_REQUEST_HEADER_SIZE: usize = 12;
/// A request must fit one datagram. The player asks again for whatever a request leaves out.
const MAX_REQUESTED_CHUNKS: usize = 256;

/// Chunks of one frame that the host should send again. No indexes means the whole frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissingVideoChunks {
    pub sequence: u64,
    pub chunk_indexes: Vec<u32>,
}

impl MissingVideoChunks {
    /// Requests travel as datagrams because a lost one is repeated soon anyway, while a reliable
    /// stream would hold every later request behind its retransmission timer.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let indexes = &self.chunk_indexes[..self.chunk_indexes.len().min(MAX_REQUESTED_CHUNKS)];
        let mut datagram = Vec::with_capacity(CHUNK_REQUEST_HEADER_SIZE + indexes.len() * 2);
        datagram.extend_from_slice(&CHUNK_REQUEST_MAGIC);
        datagram.extend_from_slice(&self.sequence.to_be_bytes());
        datagram.extend_from_slice(&(indexes.len() as u16).to_be_bytes());
        for index in indexes {
            datagram.extend_from_slice(&(*index as u16).to_be_bytes());
        }
        datagram
    }

    #[must_use]
    pub fn parse(datagram: &[u8]) -> Option<Self> {
        let header = datagram.get(..CHUNK_REQUEST_HEADER_SIZE)?;
        if header[..2] != CHUNK_REQUEST_MAGIC {
            return None;
        }
        let count = usize::from(u16::from_be_bytes(header[10..12].try_into().ok()?));
        let indexes = &datagram[CHUNK_REQUEST_HEADER_SIZE..];
        if count > MAX_REQUESTED_CHUNKS || indexes.len() != count * 2 {
            return None;
        }
        Some(Self {
            sequence: u64::from_be_bytes(header[2..10].try_into().ok()?),
            chunk_indexes: indexes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|index| u32::from(u16::from_be_bytes(*index)))
                .collect(),
        })
    }
}

#[derive(Debug)]
struct PartialFrame {
    header: Header,
    started: Instant,
    budget: Duration,
    deadline: Instant,
    received_bytes: usize,
    chunks: Vec<Option<Vec<u8>>>,
    highest_chunk: usize,
    last_chunk_at: Instant,
    resend_requested_at: Option<Instant>,
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
    pub soft_ceiling_bits_per_second: Option<u64>,
}

#[derive(Debug)]
pub struct VideoFrameAssembler {
    frame_period: Duration,
    partial: BTreeMap<u64, PartialFrame>,
    newest_sequence: Option<u64>,
    newest_completed_sequence: Option<u64>,
    last_datagram_at: Option<Instant>,
    stats: VideoAssemblyStats,
    resend: Option<ResendState>,
}

/// In resend mode a completed frame waits here until every older frame is complete or expired,
/// because a decoder cannot use a frame whose reference is still on its way.
#[derive(Debug, Default)]
struct ResendState {
    ready: BTreeMap<u64, VideoFramePayload>,
    released_sequence: Option<u64>,
    /// Frames the link swallowed whole, with their deadline and last request time.
    unseen: BTreeMap<u64, (Instant, Option<Instant>)>,
    dependency_lost: bool,
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
            last_datagram_at: None,
            stats: VideoAssemblyStats::default(),
            resend: None,
        }
    }

    /// Holds frames with missing chunks long enough for the host to send those chunks again.
    #[must_use]
    pub fn with_resend(frames_per_second: u16) -> Self {
        Self {
            resend: Some(ResendState::default()),
            ..Self::new(frames_per_second)
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
        let mut result = self.push_chunk(datagram, now, round_trip_time)?;
        if self.resend.is_some() {
            result.frame = self.next_ready();
        }
        if let Some(resend) = &mut self.resend {
            result.dependency_lost |= std::mem::take(&mut resend.dependency_lost);
        }
        Ok(result)
    }

    /// Returns the next frame that a repair released, oldest first.
    pub fn next_ready(&mut self) -> Option<VideoFramePayload> {
        let oldest_pending = self.oldest_pending_sequence();
        let resend = self.resend.as_mut()?;
        let (&sequence, _) = resend.ready.first_key_value()?;
        if oldest_pending.is_some_and(|pending| pending < sequence) {
            return None;
        }
        let frame = resend.ready.remove(&sequence)?;
        let skipped = resend.released_sequence.map_or(0, |released| {
            sequence.saturating_sub(released.saturating_add(1))
        });
        resend.dependency_lost |= skipped != 0;
        resend.released_sequence = Some(sequence);
        self.stats.skipped_frames = self.stats.skipped_frames.saturating_add(skipped);
        self.newest_completed_sequence = Some(sequence);
        Some(frame)
    }

    fn oldest_pending_sequence(&self) -> Option<u64> {
        let partial = self.partial.keys().next().copied();
        let unseen = self
            .resend
            .as_ref()
            .and_then(|resend| resend.unseen.keys().next().copied());
        partial.into_iter().chain(unseen).min()
    }

    /// Lists chunks known to be missing that have not been requested within one round trip.
    pub fn take_missing(
        &mut self,
        now: Instant,
        round_trip_time: Duration,
    ) -> Vec<MissingVideoChunks> {
        let Some(resend) = &mut self.resend else {
            return Vec::new();
        };
        let retry = round_trip_time
            .saturating_mul(2)
            .saturating_add(RESEND_RETRY_MARGIN);
        let due = |requested: Option<Instant>| {
            requested.is_none_or(|at| now.saturating_duration_since(at) >= retry)
        };
        let newest = self.newest_sequence;
        let mut missing = Vec::new();
        for (&sequence, frame) in &mut self.partial {
            let tail_lost = now.saturating_duration_since(frame.last_chunk_at) >= FRAME_TAIL_QUIET;
            let end = if tail_lost || newest.is_some_and(|newest| sequence < newest) {
                frame.chunks.len()
            } else {
                frame.highest_chunk
            };
            let chunk_indexes = frame.chunks[..end]
                .iter()
                .enumerate()
                .filter(|(_, chunk)| chunk.is_none())
                .filter_map(|(index, _)| u32::try_from(index).ok())
                .collect::<Vec<_>>();
            if !chunk_indexes.is_empty() && due(frame.resend_requested_at) {
                frame.resend_requested_at = Some(now);
                missing.push(MissingVideoChunks {
                    sequence,
                    chunk_indexes,
                });
            }
        }
        for (&sequence, (_, requested)) in &mut resend.unseen {
            if due(*requested) {
                *requested = Some(now);
                missing.push(MissingVideoChunks {
                    sequence,
                    chunk_indexes: Vec::new(),
                });
            }
        }
        missing
    }

    fn push_chunk(
        &mut self,
        datagram: &[u8],
        now: Instant,
        round_trip_time: Duration,
    ) -> Result<AssemblyResult, VideoDatagramError> {
        self.extend_deadlines_across_silence(now);
        let mut dependency_lost = self.expire(now);
        let (header, payload) = parse_header(datagram)?;
        self.stats.received_chunks = self.stats.received_chunks.saturating_add(1);
        if self
            .newest_completed_sequence
            .is_some_and(|sequence| header.sequence <= sequence)
        {
            // A resend can arrive twice. Frames that really failed are counted when they expire.
            if self.resend.is_none() {
                self.stats.late_chunks = self.stats.late_chunks.saturating_add(1);
            }
            return Ok(AssemblyResult {
                frame: None,
                dependency_lost,
                progress: None,
            });
        }
        if self
            .resend
            .as_ref()
            .is_some_and(|resend| resend.ready.contains_key(&header.sequence))
        {
            return Ok(AssemblyResult {
                frame: None,
                dependency_lost,
                progress: None,
            });
        }
        let partial_frames = if self.resend.is_some() {
            RESEND_PARTIAL_FRAMES
        } else {
            PARTIAL_FRAMES
        };
        if self.newest_sequence.is_some_and(|sequence| {
            header.sequence.saturating_add(partial_frames as u64) < sequence
        }) {
            self.stats.late_chunks = self.stats.late_chunks.saturating_add(1);
            return Ok(AssemblyResult {
                frame: None,
                dependency_lost: true,
                progress: None,
            });
        }
        let budget = self.frame_budget(round_trip_time);
        if let Some(resend) = &mut self.resend {
            resend.unseen.remove(&header.sequence);
            if let Some(newest) = self.newest_sequence {
                let first_unseen = newest
                    .saturating_add(1)
                    .max(header.sequence.saturating_sub(partial_frames as u64));
                for sequence in first_unseen..header.sequence {
                    resend.unseen.insert(sequence, (now + budget, None));
                }
            }
        }
        self.newest_sequence = Some(
            self.newest_sequence
                .map_or(header.sequence, |sequence| sequence.max(header.sequence)),
        );

        if !self.partial.contains_key(&header.sequence) {
            while self.partial.len() >= partial_frames {
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
            self.partial.insert(
                header.sequence,
                PartialFrame {
                    header,
                    started: now,
                    budget,
                    deadline: now + budget,
                    received_bytes: 0,
                    chunks: vec![None; header.chunk_count],
                    highest_chunk: 0,
                    last_chunk_at: now,
                    resend_requested_at: None,
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
            || existing.soft_ceiling_bits_per_second != header.soft_ceiling_bits_per_second
        {
            self.partial.remove(&header.sequence);
            return Err(VideoDatagramError::InconsistentFrameHeader);
        }
        let invalid_size = {
            let partial = self.partial.get_mut(&header.sequence).unwrap();
            partial.highest_chunk = partial.highest_chunk.max(header.chunk_index);
            partial.last_chunk_at = now;
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
            soft_ceiling_bits_per_second: header.soft_ceiling_bits_per_second,
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
        let skipped_frames = if self.resend.is_some() {
            0
        } else {
            self.newest_completed_sequence.map_or(0, |sequence| {
                partial
                    .header
                    .sequence
                    .saturating_sub(sequence.saturating_add(1))
            })
        };
        dependency_lost |= skipped_frames != 0;
        self.stats.skipped_frames = self.stats.skipped_frames.saturating_add(skipped_frames);
        let mut payload = Vec::with_capacity(partial.header.frame_size);
        for chunk in partial.chunks {
            payload.extend_from_slice(&chunk.unwrap());
        }
        if self.resend.is_none() {
            self.newest_completed_sequence = Some(partial.header.sequence);
        }
        self.stats.completed_frames = self.stats.completed_frames.saturating_add(1);
        self.stats.completed_payload_bytes = self
            .stats
            .completed_payload_bytes
            .saturating_add(partial.header.frame_size as u64);
        self.stats.last_completed_assembly_micros =
            u64::try_from(progress.elapsed.as_micros()).unwrap_or(u64::MAX);
        self.stats.last_assembly_budget_micros =
            u64::try_from(progress.budget.as_micros()).unwrap_or(u64::MAX);
        let frame = VideoFramePayload {
            sequence: partial.header.sequence,
            captured_at_micros: partial.header.captured_at,
            encoded_at_micros: partial.header.encoded_at,
            packetized_at_micros: partial.header.packetized_at,
            input_sequence: partial.header.input_sequence,
            keyframe: partial.header.keyframe,
            target_bitrate_bits_per_second: partial.header.target_bitrate_bits_per_second,
            estimated_capacity_bits_per_second: partial.header.estimated_capacity_bits_per_second,
            soft_ceiling_bits_per_second: partial.header.soft_ceiling_bits_per_second,
            payload,
        };
        let frame = match &mut self.resend {
            Some(resend) => {
                resend.ready.insert(frame.sequence, frame);
                None
            }
            None => Some(frame),
        };
        Ok(AssemblyResult {
            frame,
            dependency_lost,
            progress: Some(progress),
        })
    }

    fn frame_budget(&self, round_trip_time: Duration) -> Duration {
        let budget = assembly_deadline(self.frame_period, round_trip_time);
        if self.resend.is_some() {
            budget.max(RESEND_MINIMUM_BUDGET)
        } else {
            budget
        }
    }

    /// A link that delivers nothing is delaying chunks, not losing them, so silence must not
    /// run down a frame whose remaining chunks may be the very next datagrams.
    fn extend_deadlines_across_silence(&mut self, now: Instant) {
        let silence = self
            .last_datagram_at
            .replace(now)
            .map_or(Duration::ZERO, |last| now.saturating_duration_since(last));
        if silence >= self.frame_period.saturating_mul(SILENCE_FRAME_PERIODS) {
            for frame in self.partial.values_mut() {
                frame.deadline += silence;
                if self.resend.is_some() {
                    // Slow frames look like silence too, and held frames delay every newer frame.
                    frame.deadline = frame
                        .deadline
                        .min(frame.started + frame.budget.saturating_mul(2));
                }
            }
        }
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
        let mut unseen_expired = false;
        if let Some(resend) = &mut self.resend {
            let before = resend.unseen.len();
            resend.unseen.retain(|_, (deadline, _)| *deadline > now);
            let gone = before - resend.unseen.len();
            unseen_expired = gone != 0;
            self.stats.incomplete_frames = self.stats.incomplete_frames.saturating_add(gone as u64);
        }
        !expired.is_empty() || unseen_expired
    }
}

fn missing_chunks(frame: &PartialFrame) -> u64 {
    frame.chunks.iter().filter(|chunk| chunk.is_none()).count() as u64
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VideoAssemblyStats {
    /// Sequence gaps between completed frames, including frames never seen on the wire.
    pub skipped_frames: u64,
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
        .saturating_mul(4)
        .max(round_trip_time.saturating_mul(2))
        .min(Duration::from_millis(100))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoDatagramError {
    DatagramTooSmall(usize),
    InvalidFrameSize(usize),
    TooManyChunks,
    UnsupportedVersion(u32),
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
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported video datagram version {version}")
            }
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

    const RTT: Duration = Duration::from_millis(9);

    fn chunks(sequence: u64) -> Vec<Vec<u8>> {
        packetize_video_frame(&frame(sequence, 300), VIDEO_DATAGRAM_HEADER_SIZE + 100).unwrap()
    }

    #[test]
    fn a_repaired_frame_comes_out_in_order_and_is_not_counted_as_lost() {
        let now = Instant::now();
        let mut assembler = VideoFrameAssembler::with_resend(120);
        let (first, second) = (chunks(1), chunks(2));
        assembler.push(&first[0], now, RTT).unwrap();
        assembler.push(&first[2], now, RTT).unwrap();
        for chunk in &second {
            let result = assembler.push(chunk, now, RTT).unwrap();
            assert_eq!(result.frame, None);
        }

        assert_eq!(
            assembler.take_missing(now, RTT),
            vec![MissingVideoChunks {
                sequence: 1,
                chunk_indexes: vec![1]
            }]
        );
        let repaired = assembler
            .push(&first[1], now + Duration::from_millis(25), RTT)
            .unwrap();

        assert_eq!(repaired.frame.map(|frame| frame.sequence), Some(1));
        assert!(!repaired.dependency_lost);
        assert_eq!(assembler.next_ready().map(|frame| frame.sequence), Some(2));
        assert_eq!(assembler.stats().lost_chunks, 0);
        assert_eq!(assembler.stats().incomplete_frames, 0);
    }

    #[test]
    fn a_frame_the_link_swallowed_is_requested_whole_and_repaired() {
        let now = Instant::now();
        let mut assembler = VideoFrameAssembler::with_resend(120);
        let (first, second, third) = (chunks(1), chunks(2), chunks(3));
        for chunk in &first {
            assembler.push(chunk, now, RTT).unwrap();
        }
        for chunk in &third {
            assembler.push(chunk, now, RTT).unwrap();
        }

        assert_eq!(
            assembler.take_missing(now, RTT),
            vec![MissingVideoChunks {
                sequence: 2,
                chunk_indexes: Vec::new()
            }]
        );
        let mut released = Vec::new();
        for chunk in &second {
            let result = assembler.push(chunk, now, RTT).unwrap();
            assert!(!result.dependency_lost);
            released.extend(result.frame.map(|frame| frame.sequence));
        }
        released.extend(assembler.next_ready().map(|frame| frame.sequence));

        assert_eq!(released, [2, 3]);
    }

    #[test]
    fn a_frame_that_is_never_repaired_expires_and_asks_for_recovery() {
        let now = Instant::now();
        let mut assembler = VideoFrameAssembler::with_resend(120);
        let (first, second, third) = (chunks(1), chunks(2), chunks(3));
        assembler.push(&first[0], now, RTT).unwrap();
        for chunk in &second {
            assembler.push(chunk, now, RTT).unwrap();
        }

        // The link keeps delivering, so this is loss and not silence.
        let mut later = now;
        let mut result = None;
        for chunk in third.iter().cycle().take(12) {
            later += Duration::from_millis(5);
            result = Some(assembler.push(chunk, later, RTT).unwrap());
            if result.as_ref().is_some_and(|result| result.dependency_lost) {
                break;
            }
        }

        let result = result.unwrap();
        assert!(result.dependency_lost);
        assert_eq!(result.frame.map(|frame| frame.sequence), Some(2));
        assert_eq!(assembler.stats().incomplete_frames, 1);
    }

    #[test]
    fn a_chunk_request_survives_encoding_and_stays_within_one_datagram() {
        let request = MissingVideoChunks {
            sequence: 9,
            chunk_indexes: vec![1, 500],
        };
        assert_eq!(MissingVideoChunks::parse(&request.encode()), Some(request));

        let whole_frame = MissingVideoChunks {
            sequence: 9,
            chunk_indexes: Vec::new(),
        };
        assert_eq!(
            MissingVideoChunks::parse(&whole_frame.encode()),
            Some(whole_frame)
        );

        let oversized = MissingVideoChunks {
            sequence: 9,
            chunk_indexes: (0..1000).collect(),
        };
        let parsed = MissingVideoChunks::parse(&oversized.encode()).unwrap();
        assert_eq!(parsed.chunk_indexes.len(), MAX_REQUESTED_CHUNKS);
    }

    #[test]
    fn a_malformed_chunk_request_is_ignored() {
        let valid = MissingVideoChunks {
            sequence: 9,
            chunk_indexes: vec![1, 2],
        }
        .encode();

        assert_eq!(MissingVideoChunks::parse(&valid[..valid.len() - 1]), None);
        assert_eq!(MissingVideoChunks::parse(b"RQ"), None);
        assert_eq!(MissingVideoChunks::parse(&[0; 16]), None);
    }

    #[test]
    fn a_lost_frame_end_is_requested_without_waiting_for_the_next_frame() {
        let now = Instant::now();
        let mut assembler = VideoFrameAssembler::with_resend(120);
        assembler.push(&chunks(1)[0], now, RTT).unwrap();

        assert!(assembler.take_missing(now, RTT).is_empty());
        assert_eq!(
            assembler.take_missing(now + FRAME_TAIL_QUIET, RTT),
            vec![MissingVideoChunks {
                sequence: 1,
                chunk_indexes: vec![1, 2]
            }]
        );
    }

    #[test]
    fn a_broken_frame_cannot_hold_back_slow_frames_for_long() {
        let now = Instant::now();
        let mut assembler = VideoFrameAssembler::with_resend(120);
        assembler.push(&chunks(1)[0], now, RTT).unwrap();

        // Seven frames per second: every frame arrives after what looks like silence.
        let mut released_at = None;
        for step in 2..8 {
            let at = now + Duration::from_millis(140) * (step as u32 - 1);
            for chunk in &chunks(step) {
                let result = assembler.push(chunk, at, RTT).unwrap();
                if result.frame.is_some() && released_at.is_none() {
                    released_at = Some(at);
                }
            }
        }

        let waited = released_at.unwrap().saturating_duration_since(now);
        assert!(waited <= Duration::from_millis(300), "waited {waited:?}");
    }

    #[test]
    fn a_request_is_repeated_only_after_two_round_trips() {
        let now = Instant::now();
        let mut assembler = VideoFrameAssembler::with_resend(120);
        let (first, second) = (chunks(1), chunks(2));
        assembler.push(&first[0], now, RTT).unwrap();
        assembler.push(&second[0], now, RTT).unwrap();

        assert_eq!(assembler.take_missing(now, RTT).len(), 1);
        assert!(
            assembler
                .take_missing(now + Duration::from_millis(1), RTT)
                .is_empty()
        );
        let repeated = assembler.take_missing(now + RTT * 2 + RESEND_RETRY_MARGIN, RTT);
        assert_eq!(repeated[0].sequence, 1);
    }

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
            soft_ceiling_bits_per_second: Some(22_000_000),
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
    fn legacy_packetization_omits_the_soft_ceiling() {
        let mut expected = frame(1, 2_000);
        let datagrams =
            packetize_video_frame_for_version(&expected, 1_200, LEGACY_VIDEO_DATAGRAM_VERSION)
                .unwrap();
        expected.soft_ceiling_bits_per_second = None;
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
        assert_eq!(assembled, Some(expected));
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
        assert_eq!(progress.budget, Duration::from_nanos(33_333_332));

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
        assert_eq!(assembler.stats().skipped_frames, 1);
        assert_eq!(assembler.stats().lost_chunks, 0);
        assembler
            .push(&third, now, Duration::from_millis(5))
            .unwrap();
        assert_eq!(assembler.stats().skipped_frames, 1);
    }

    #[test]
    fn deadline_uses_four_frames_or_two_rtts_with_a_hard_cap() {
        assert_eq!(
            assembly_deadline(Duration::from_millis(8), Duration::from_millis(5)),
            Duration::from_millis(32)
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
        assert_eq!(assembler.stats().last_assembly_budget_micros, 33_333);
        assembler
            .push(&completed, now, Duration::from_millis(5))
            .unwrap();
        assert_eq!(assembler.stats().late_chunks, 1);

        for (sequence, milliseconds) in [(5, 12), (6, 24), (7, 36)] {
            let next = packetize_video_frame(&frame(sequence, 16), 1_200)
                .unwrap()
                .remove(0);
            assembler
                .push(
                    &next,
                    now + Duration::from_millis(milliseconds),
                    Duration::from_millis(5),
                )
                .unwrap();
        }
        assert_eq!(assembler.stats().lost_chunks, 3);
        assert_eq!(assembler.stats().incomplete_frames, 3);
    }

    #[test]
    fn a_frame_delayed_by_a_silent_link_still_completes() {
        let now = Instant::now();
        let round_trip_time = Duration::from_millis(5);
        let mut assembler = VideoFrameAssembler::new(120);
        let mut chunks = packetize_video_frame(&frame(1, 2_000), 1_200).unwrap();
        let last = chunks.pop().unwrap();
        for chunk in &chunks {
            assembler.push(chunk, now, round_trip_time).unwrap();
        }

        let result = assembler
            .push(&last, now + Duration::from_millis(80), round_trip_time)
            .unwrap();

        assert_eq!(result.frame, Some(frame(1, 2_000)));
        assert!(!result.dependency_lost);
        assert_eq!(assembler.stats().incomplete_frames, 0);
        assert_eq!(assembler.stats().lost_chunks, 0);
    }
}
