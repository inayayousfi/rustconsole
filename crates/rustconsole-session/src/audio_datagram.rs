use rustconsole_protocol::audio::{
    AudioPacket, HEADER_SIZE, Header, MAX_FRAGMENTS, MAX_PACKET_SIZE,
};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

pub const AUDIO_WAIT: Duration = Duration::from_millis(40);
pub const AUDIO_QUEUE_PACKETS: usize = 4;

pub fn packetize(
    packet: &AudioPacket,
    maximum_datagram_size: usize,
) -> Result<Vec<Vec<u8>>, &'static str> {
    let capacity = maximum_datagram_size
        .checked_sub(HEADER_SIZE)
        .filter(|n| *n > 0)
        .ok_or("audio datagram limit too small")?;
    if packet.payload.is_empty() || packet.payload.len() > MAX_PACKET_SIZE {
        return Err("invalid audio packet size");
    }
    let count = packet.payload.len().div_ceil(capacity);
    if count > MAX_FRAGMENTS {
        return Err("too many audio fragments");
    }
    packet
        .payload
        .chunks(capacity)
        .enumerate()
        .map(|(index, payload)| {
            let header = Header {
                generation: packet.generation,
                sequence: packet.sequence,
                captured_at_micros: packet.captured_at_micros,
                decoded_samples: packet.decoded_samples,
                skip_start_samples: packet.skip_start_samples,
                skip_end_samples: packet.skip_end_samples,
                packet_size: packet.payload.len() as u16,
                fragment_index: index as u16,
                fragment_count: count as u16,
                offset: (index * capacity) as u16,
                payload_size: payload.len() as u16,
            };
            let mut bytes = Vec::with_capacity(HEADER_SIZE + payload.len());
            bytes.extend_from_slice(&header.encode()?);
            bytes.extend_from_slice(payload);
            Ok(bytes)
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioReceiveStatistics {
    pub generation: u64,
    pub received_fragments: u64,
    pub completed_packets: u64,
    pub payload_bytes: u64,
    pub expired_packets: u64,
    pub overflow_packets: u64,
    pub late_fragments: u64,
    pub duplicate_fragments: u64,
    pub malformed_fragments: u64,
    pub missing_packets: u64,
}

struct Partial {
    header: Header,
    started: Instant,
    pieces: Vec<Option<(usize, usize)>>,
    data: Vec<u8>,
}

#[derive(Default)]
pub struct AudioAssembler {
    partial: BTreeMap<u64, Partial>,
    // Retired sequence -> whether it completed rather than being discarded.
    retired: BTreeMap<u64, bool>,
    missing: BTreeSet<u64>,
    newest: Option<u64>,
    stats: AudioReceiveStatistics,
}

impl AudioAssembler {
    pub fn statistics(&self) -> AudioReceiveStatistics {
        self.stats
    }

    pub fn generation(&mut self, generation: u64) {
        if generation > self.stats.generation {
            self.stats.expired_packets += self.partial.len() as u64;
            self.stats.missing_packets = self
                .stats
                .missing_packets
                .saturating_add(self.missing.len() as u64);
            self.partial.clear();
            self.retired.clear();
            self.missing.clear();
            self.newest = None;
            self.stats.generation = generation;
        }
    }

    pub fn expire(&mut self, now: Instant) {
        let expired = self
            .partial
            .iter()
            .filter(|(_, p)| now.duration_since(p.started) >= AUDIO_WAIT)
            .map(|(s, _)| *s)
            .collect::<Vec<_>>();
        for sequence in expired {
            self.partial.remove(&sequence);
            self.retired.insert(sequence, false);
            self.stats.expired_packets += 1;
        }
        self.trim_history();
    }

    fn trim_history(&mut self) {
        if let Some(newest) = self.newest {
            self.retired.retain(|sequence, _| {
                newest.saturating_sub(*sequence) < AUDIO_QUEUE_PACKETS as u64
            });
            let before = self.missing.len();
            self.missing
                .retain(|sequence| newest.saturating_sub(*sequence) < AUDIO_QUEUE_PACKETS as u64);
            self.stats.missing_packets = self
                .stats
                .missing_packets
                .saturating_add((before - self.missing.len()) as u64);
        }
    }

    pub fn push(&mut self, bytes: &[u8], now: Instant) -> Option<AudioPacket> {
        self.expire(now);
        let (header, payload) = match Header::decode(bytes) {
            Ok(value) => value,
            Err(_) => {
                self.stats.malformed_fragments += 1;
                return None;
            }
        };
        self.stats.received_fragments += 1;
        if header.generation < self.stats.generation {
            self.stats.late_fragments += 1;
            return None;
        }
        self.generation(header.generation);
        if let Some(completed) = self.retired.get(&header.sequence) {
            if *completed {
                self.stats.duplicate_fragments += 1;
            } else {
                self.stats.late_fragments += 1;
            }
            return None;
        }
        if self.newest.is_some_and(|newest| {
            newest.saturating_sub(header.sequence) >= AUDIO_QUEUE_PACKETS as u64
        }) {
            self.stats.late_fragments += 1;
            return None;
        }
        let expected = self.newest.map_or(0, |newest| newest.saturating_add(1));
        if header.sequence > expected {
            let window_start = header
                .sequence
                .saturating_sub(AUDIO_QUEUE_PACKETS as u64 - 1)
                .max(expected);
            self.stats.missing_packets = self
                .stats
                .missing_packets
                .saturating_add(window_start - expected);
            self.missing.extend(window_start..header.sequence);
        }
        self.missing.remove(&header.sequence);
        self.newest = Some(self.newest.unwrap_or(0).max(header.sequence));
        self.trim_history();
        if !self.partial.contains_key(&header.sequence) {
            if self.partial.len() == AUDIO_QUEUE_PACKETS {
                self.partial.pop_first();
                self.stats.overflow_packets += 1;
            }
            self.partial.insert(
                header.sequence,
                Partial {
                    header,
                    started: now,
                    pieces: vec![None; usize::from(header.fragment_count)],
                    data: vec![0; usize::from(header.packet_size)],
                },
            );
        }
        let p = self.partial.get_mut(&header.sequence).unwrap();
        let original = p.header;
        if original.captured_at_micros != header.captured_at_micros
            || original.packet_size != header.packet_size
            || original.fragment_count != header.fragment_count
            || original.decoded_samples != header.decoded_samples
            || original.skip_start_samples != header.skip_start_samples
            || original.skip_end_samples != header.skip_end_samples
        {
            self.partial.remove(&header.sequence);
            self.retired.insert(header.sequence, false);
            self.stats.malformed_fragments += 1;
            return None;
        }
        let offset = usize::from(header.offset);
        let end = offset + payload.len();
        if let Some((previous_offset, length)) = p.pieces[usize::from(header.fragment_index)] {
            if previous_offset == offset
                && length == payload.len()
                && p.data[offset..end] == *payload
            {
                self.stats.duplicate_fragments += 1;
            } else {
                self.stats.malformed_fragments += 1;
                self.partial.remove(&header.sequence);
                self.retired.insert(header.sequence, false);
            }
            return None;
        }
        if p.pieces
            .iter()
            .flatten()
            .any(|(start, length)| offset < start + length && *start < end)
        {
            self.partial.remove(&header.sequence);
            self.retired.insert(header.sequence, false);
            self.stats.malformed_fragments += 1;
            return None;
        }
        p.data[offset..end].copy_from_slice(payload);
        p.pieces[usize::from(header.fragment_index)] = Some((offset, payload.len()));
        if p.pieces.iter().any(Option::is_none) {
            return None;
        }
        let partial = self.partial.remove(&header.sequence).unwrap();
        self.retired.insert(header.sequence, false);
        let mut length = 0;
        for piece in partial.pieces {
            let (offset, bytes) = piece.unwrap();
            if offset != length {
                self.stats.malformed_fragments += 1;
                return None;
            }
            length += bytes;
        }
        if length != usize::from(header.packet_size) {
            self.stats.malformed_fragments += 1;
            return None;
        }
        let data = partial.data;
        self.retired.insert(header.sequence, true);
        self.stats.completed_packets += 1;
        self.stats.payload_bytes += data.len() as u64;
        Some(AudioPacket {
            generation: header.generation,
            sequence: header.sequence,
            captured_at_micros: header.captured_at_micros,
            decoded_samples: header.decoded_samples,
            skip_start_samples: header.skip_start_samples,
            skip_end_samples: header.skip_end_samples,
            payload: data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extreme_sequences_do_not_overflow_or_reopen_completed_packets() {
        let now = Instant::now();
        let mut assembler = AudioAssembler::default();
        for generation in 1..4 {
            let mut source = packet(u64::MAX);
            source.generation = generation;
            let chunks = packetize(&source, 1200).unwrap();
            for chunk in &chunks {
                assembler.push(chunk, now);
            }
            assert!(assembler.push(&chunks[0], now).is_none());
        }
        assert_eq!(assembler.statistics().completed_packets, 3);
        assert_eq!(assembler.statistics().missing_packets, u64::MAX);
    }
    fn packet(sequence: u64) -> AudioPacket {
        AudioPacket {
            generation: 1,
            sequence,
            captured_at_micros: 1_000,
            decoded_samples: 480,
            skip_start_samples: 312,
            skip_end_samples: 0,
            payload: vec![42; 2500],
        }
    }

    #[test]
    fn reordered_packets_are_not_lost_and_overlapping_fragments_are_rejected() {
        let now = Instant::now();
        let mut assembler = AudioAssembler::default();
        for seq in [0, 2, 1, 3, 4, 5] {
            for bytes in packetize(&packet(seq), 1200).unwrap() {
                assembler.push(&bytes, now);
            }
        }
        assert_eq!(assembler.statistics().missing_packets, 0);
        let chunks = packetize(&packet(6), 1200).unwrap();
        assembler.push(&chunks[0], now);
        let mut overlapping = chunks[1].clone();
        overlapping[44..46].copy_from_slice(&0_u16.to_be_bytes());
        assert!(assembler.push(&overlapping, now).is_none());
        assert!(!assembler.partial.contains_key(&6));
        assert_eq!(assembler.statistics().malformed_fragments, 1);
    }

    #[test]
    fn fragmented_packets_round_trip_in_reverse_order_and_duplicates_are_ignored() {
        let now = Instant::now();
        let source = packet(0);
        let chunks = packetize(&source, 1200).unwrap();
        let mut assembler = AudioAssembler::default();
        let mut output = None;
        for chunk in chunks.iter().rev() {
            output = assembler.push(chunk, now).or(output);
        }
        assert_eq!(output, Some(source));
        assert!(assembler.push(&chunks[0], now).is_none());
        assert_eq!(assembler.statistics().completed_packets, 1);
        assert_eq!(assembler.statistics().duplicate_fragments, 1);
    }

    #[test]
    fn expiry_overflow_generation_and_malformed_data_are_bounded() {
        let now = Instant::now();
        let mut assembler = AudioAssembler::default();
        for seq in 0..5 {
            assembler.push(&packetize(&packet(seq), 1200).unwrap()[0], now);
        }
        assert_eq!(assembler.partial.len(), 4);
        assert_eq!(assembler.statistics().overflow_packets, 1);
        assembler.expire(now + AUDIO_WAIT);
        assert!(assembler.partial.is_empty());
        assert_eq!(assembler.statistics().expired_packets, 4);
        assert!(
            assembler
                .push(&packetize(&packet(4), 1200).unwrap()[1], now + AUDIO_WAIT)
                .is_none()
        );
        assembler.generation(2);
        assert!(
            assembler
                .push(&packetize(&packet(6), 1200).unwrap()[0], now + AUDIO_WAIT)
                .is_none()
        );
        assembler.push(b"bad", now + AUDIO_WAIT);
        assert_eq!(assembler.statistics().malformed_fragments, 1);
        assert!(packetize(&packet(0), HEADER_SIZE).is_err());
        assert!(packetize(&packet(0), HEADER_SIZE + 1).is_err());
    }
}
