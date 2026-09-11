use std::fmt;
use std::mem::{offset_of, size_of};
use std::sync::atomic::{AtomicU64, Ordering};

pub const REPORT_RING_MAGIC: u32 = 0x4449_4856;
pub const REPORT_RING_VERSION: u32 = 1;
pub const REPORT_SLOT_COUNT: u32 = 256;
pub const REPORT_CAPACITY: u32 = 80;
pub const OUTPUT_REPORT_OPERATION: u8 = 1;
pub const FEATURE_WRITE_OPERATION: u8 = 2;

#[repr(C, align(8))]
pub struct ReportRingHeader {
    pub magic: u32,
    pub version: u32,
    pub slot_count: u32,
    pub report_capacity: u32,
    pub generation: AtomicU64,
    pub producer_sequence: AtomicU64,
    pub consumer_sequence: AtomicU64,
    pub overflow_count: AtomicU64,
}

#[repr(C, align(8))]
pub struct ReportSlot {
    pub committed_sequence: AtomicU64,
    pub length: u16,
    pub reserved: u16,
    pub data: [u8; REPORT_CAPACITY as usize],
}

#[repr(C, align(8))]
pub struct SharedReportRing {
    pub header: ReportRingHeader,
    pub slots: [ReportSlot; REPORT_SLOT_COUNT as usize],
}

const _: () = {
    assert!(size_of::<ReportRingHeader>() == 48);
    assert!(offset_of!(ReportRingHeader, generation) == 16);
    assert!(offset_of!(ReportRingHeader, producer_sequence) == 24);
    assert!(offset_of!(ReportRingHeader, consumer_sequence) == 32);
    assert!(offset_of!(ReportRingHeader, overflow_count) == 40);
    assert!(size_of::<ReportSlot>() == 96);
    assert!(offset_of!(ReportSlot, data) == 12);
    assert!(size_of::<SharedReportRing>() == 24_624);
    assert!(offset_of!(SharedReportRing, slots) == 48);
};

impl ReportRingHeader {
    pub fn validate(&self) -> Result<(), RingError> {
        if self.magic != REPORT_RING_MAGIC
            || self.version != REPORT_RING_VERSION
            || self.slot_count != REPORT_SLOT_COUNT
            || self.report_capacity != REPORT_CAPACITY
        {
            return Err(RingError::InvalidHeader);
        }
        Ok(())
    }
}

impl SharedReportRing {
    #[must_use]
    pub fn new(generation: u64) -> Self {
        Self {
            header: ReportRingHeader {
                magic: REPORT_RING_MAGIC,
                version: REPORT_RING_VERSION,
                slot_count: REPORT_SLOT_COUNT,
                report_capacity: REPORT_CAPACITY,
                generation: AtomicU64::new(generation),
                producer_sequence: AtomicU64::new(0),
                consumer_sequence: AtomicU64::new(0),
                overflow_count: AtomicU64::new(0),
            },
            slots: std::array::from_fn(|_| ReportSlot {
                committed_sequence: AtomicU64::new(0),
                length: 0,
                reserved: 0,
                data: [0; REPORT_CAPACITY as usize],
            }),
        }
    }

    pub fn reopen(&self, generation: u64) -> Result<(), RingError> {
        self.header.validate()?;
        if generation == 0 {
            return Err(RingError::InvalidHeader);
        }
        self.header.generation.store(generation, Ordering::Release);
        Ok(())
    }

    pub fn publish(&mut self, report: &[u8]) -> Result<u64, RingError> {
        self.header.validate()?;
        if report.is_empty() || report.len() > REPORT_CAPACITY as usize {
            return Err(RingError::InvalidReportLength);
        }
        let producer = self.header.producer_sequence.load(Ordering::Acquire);
        let consumer = self.header.consumer_sequence.load(Ordering::Acquire);
        if producer.saturating_sub(consumer) >= u64::from(REPORT_SLOT_COUNT) {
            self.header.overflow_count.fetch_add(1, Ordering::AcqRel);
            return Err(RingError::Full);
        }
        let sequence = producer
            .checked_add(1)
            .ok_or(RingError::SequenceExhausted)?;
        let slot = &mut self.slots[((sequence - 1) % u64::from(REPORT_SLOT_COUNT)) as usize];
        slot.length = report.len() as u16;
        slot.reserved = 0;
        slot.data.fill(0);
        slot.data[..report.len()].copy_from_slice(report);
        slot.committed_sequence.store(sequence, Ordering::Release);
        self.header
            .producer_sequence
            .store(sequence, Ordering::Release);
        Ok(sequence)
    }

    pub fn consume_output(&self) -> Result<Option<OutputRecord>, RingError> {
        self.header.validate()?;
        let consumer = self.header.consumer_sequence.load(Ordering::Acquire);
        let sequence = consumer
            .checked_add(1)
            .ok_or(RingError::SequenceExhausted)?;
        if sequence > self.header.producer_sequence.load(Ordering::Acquire) {
            return Ok(None);
        }
        let slot = &self.slots[((sequence - 1) % u64::from(REPORT_SLOT_COUNT)) as usize];
        if slot.committed_sequence.load(Ordering::Acquire) != sequence {
            return Ok(None);
        }
        let length = usize::from(slot.length);
        if !(3..=REPORT_CAPACITY as usize).contains(&length) || slot.reserved != 0 {
            return Err(RingError::InvalidOutputRecord);
        }
        let operation = slot.data[0];
        if !matches!(operation, OUTPUT_REPORT_OPERATION | FEATURE_WRITE_OPERATION) {
            return Err(RingError::InvalidOutputRecord);
        }
        let record = OutputRecord {
            sequence,
            operation,
            report_id: slot.data[1],
            payload: slot.data[2..length].to_vec(),
        };
        if slot.committed_sequence.load(Ordering::Acquire) != sequence {
            return Ok(None);
        }
        self.header
            .consumer_sequence
            .store(sequence, Ordering::Release);
        Ok(Some(record))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputRecord {
    pub sequence: u64,
    pub operation: u8,
    pub report_id: u8,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RingError {
    InvalidHeader,
    InvalidReportLength,
    Full,
    SequenceExhausted,
    InvalidOutputRecord,
}

impl fmt::Display for RingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "virtual HID report ring error: {self:?}")
    }
}

impl std::error::Error for RingError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct RingModel {
        producer: u64,
        consumer: u64,
        overflow: u64,
        queue: VecDeque<(u64, Vec<u8>)>,
    }

    impl RingModel {
        fn new() -> Self {
            Self {
                producer: 0,
                consumer: 0,
                overflow: 0,
                queue: VecDeque::with_capacity(REPORT_SLOT_COUNT as usize),
            }
        }

        fn publish(&mut self, report: &[u8]) -> Result<u64, RingError> {
            if report.is_empty() || report.len() > REPORT_CAPACITY as usize {
                return Err(RingError::InvalidReportLength);
            }
            if self.queue.len() == REPORT_SLOT_COUNT as usize {
                self.overflow += 1;
                return Err(RingError::Full);
            }
            let sequence = self
                .producer
                .checked_add(1)
                .ok_or(RingError::SequenceExhausted)?;
            self.queue.push_back((sequence, report.to_vec()));
            self.producer = sequence;
            Ok(sequence)
        }

        fn consume(&mut self) -> Option<(u64, Vec<u8>)> {
            let report = self.queue.pop_front()?;
            self.consumer = report.0;
            Some(report)
        }
    }

    #[test]
    fn layout_is_stable_and_header_validation_is_strict() {
        let mut header = ReportRingHeader {
            magic: REPORT_RING_MAGIC,
            version: REPORT_RING_VERSION,
            slot_count: REPORT_SLOT_COUNT,
            report_capacity: REPORT_CAPACITY,
            generation: AtomicU64::new(7),
            producer_sequence: AtomicU64::new(0),
            consumer_sequence: AtomicU64::new(0),
            overflow_count: AtomicU64::new(0),
        };
        assert_eq!(header.validate(), Ok(()));
        header.slot_count -= 1;
        assert_eq!(header.validate(), Err(RingError::InvalidHeader));
    }

    #[test]
    fn reopening_changes_only_the_generation() {
        let mut ring = SharedReportRing::new(7);
        ring.publish(&[1, 2, 3]).unwrap();

        ring.reopen(8).unwrap();

        assert_eq!(ring.header.generation.load(Ordering::Acquire), 8);
        assert_eq!(ring.header.producer_sequence.load(Ordering::Acquire), 1);
        assert_eq!(ring.slots[0].committed_sequence.load(Ordering::Acquire), 1);
        assert_eq!(ring.reopen(0), Err(RingError::InvalidHeader));
    }

    #[test]
    fn full_ring_rejects_without_changing_producer_state() {
        let mut ring = RingModel::new();
        for expected in 1..=REPORT_SLOT_COUNT as u64 {
            assert_eq!(ring.publish(&[expected as u8]), Ok(expected));
        }
        assert_eq!(ring.publish(&[9]), Err(RingError::Full));
        assert_eq!(ring.producer, REPORT_SLOT_COUNT as u64);
        assert_eq!(ring.overflow, 1);
        assert_eq!(ring.consume(), Some((1, vec![1])));
        assert_eq!(ring.publish(&[10]), Ok(REPORT_SLOT_COUNT as u64 + 1));
    }

    #[test]
    fn report_bounds_and_sequence_exhaustion_are_explicit() {
        let mut ring = RingModel::new();
        assert_eq!(ring.publish(&[]), Err(RingError::InvalidReportLength));
        assert_eq!(
            ring.publish(&[0; REPORT_CAPACITY as usize + 1]),
            Err(RingError::InvalidReportLength)
        );
        ring.producer = u64::MAX;
        assert_eq!(ring.publish(&[1]), Err(RingError::SequenceExhausted));
    }

    #[test]
    fn output_records_are_validated_and_consumed_in_order() {
        let mut ring = SharedReportRing::new(9);
        ring.publish(&[OUTPUT_REPORT_OPERATION, 0, 0x07]).unwrap();
        assert_eq!(
            ring.consume_output().unwrap(),
            Some(OutputRecord {
                sequence: 1,
                operation: OUTPUT_REPORT_OPERATION,
                report_id: 0,
                payload: vec![0x07],
            })
        );
        assert_eq!(ring.consume_output().unwrap(), None);
        ring.publish(&[9, 0, 0]).unwrap();
        assert_eq!(ring.consume_output(), Err(RingError::InvalidOutputRecord));
    }
}
