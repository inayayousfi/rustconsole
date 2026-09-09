use std::collections::BTreeMap;
use std::fmt;

const MAX_DESCRIPTOR_SIZE: usize = 4096;
const MAX_REPORT_SIZE: usize = 80;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReportLengths {
    pub input: BTreeMap<u8, usize>,
    pub output: BTreeMap<u8, usize>,
    pub feature: BTreeMap<u8, usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DescriptorLayout {
    pub reports: ReportLengths,
    pub uses_report_ids: bool,
}

#[derive(Clone, Copy, Default)]
struct Globals {
    report_size: u32,
    report_count: u32,
    report_id: u8,
}

pub fn parse_report_descriptor(bytes: &[u8]) -> Result<DescriptorLayout, DescriptorError> {
    if bytes.is_empty() || bytes.len() > MAX_DESCRIPTOR_SIZE {
        return Err(DescriptorError::InvalidDescriptorSize);
    }
    let mut globals = Globals::default();
    let mut stack = Vec::new();
    let mut depth = 0u32;
    let mut uses_report_ids = false;
    let mut bits = [BTreeMap::<u8, u64>::new(), BTreeMap::new(), BTreeMap::new()];
    let mut offset = 0;
    while offset < bytes.len() {
        let prefix = bytes[offset];
        offset += 1;
        if prefix == 0xfe {
            return Err(DescriptorError::LongItemUnsupported);
        }
        let size = match prefix & 3 {
            3 => 4,
            value => usize::from(value),
        };
        let data = bytes
            .get(offset..offset + size)
            .ok_or(DescriptorError::TruncatedItem)?;
        offset += size;
        let value = data.iter().enumerate().fold(0u32, |value, (index, byte)| {
            value | (u32::from(*byte) << (index * 8))
        });
        let item_type = (prefix >> 2) & 3;
        let tag = prefix >> 4;
        match (item_type, tag) {
            (1, 7) => globals.report_size = value,
            (1, 8) => {
                let id = u8::try_from(value).map_err(|_| DescriptorError::InvalidReportId)?;
                if id == 0 {
                    return Err(DescriptorError::InvalidReportId);
                }
                globals.report_id = id;
                uses_report_ids = true;
            }
            (1, 9) => globals.report_count = value,
            (1, 10) => stack.push(globals),
            (1, 11) => globals = stack.pop().ok_or(DescriptorError::GlobalStackUnderflow)?,
            (0, 10) => {
                depth = depth
                    .checked_add(1)
                    .ok_or(DescriptorError::ArithmeticOverflow)?
            }
            (0, 12) => {
                depth = depth
                    .checked_sub(1)
                    .ok_or(DescriptorError::CollectionUnderflow)?
            }
            (0, 8 | 9 | 11) => {
                let kind = match tag {
                    8 => 0,
                    9 => 1,
                    _ => 2,
                };
                let added = u64::from(globals.report_size)
                    .checked_mul(u64::from(globals.report_count))
                    .ok_or(DescriptorError::ArithmeticOverflow)?;
                let total = bits[kind].entry(globals.report_id).or_default();
                *total = total
                    .checked_add(added)
                    .ok_or(DescriptorError::ArithmeticOverflow)?;
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(DescriptorError::UnclosedCollection);
    }
    if !stack.is_empty() {
        return Err(DescriptorError::UnclosedGlobalStack);
    }
    if uses_report_ids && bits.iter().any(|map| map.contains_key(&0)) {
        return Err(DescriptorError::MixedReportIds);
    }
    let mut reports = ReportLengths::default();
    for (source, target) in bits.into_iter().zip([
        &mut reports.input,
        &mut reports.output,
        &mut reports.feature,
    ]) {
        for (id, bit_count) in source {
            let bytes = bit_count
                .checked_add(7)
                .ok_or(DescriptorError::ArithmeticOverflow)?
                / 8
                + u64::from(uses_report_ids);
            let bytes = usize::try_from(bytes).map_err(|_| DescriptorError::ArithmeticOverflow)?;
            if bytes == 0 || bytes > MAX_REPORT_SIZE {
                return Err(DescriptorError::InvalidReportSize);
            }
            target.insert(id, bytes);
        }
    }
    if reports.input.is_empty() {
        return Err(DescriptorError::MissingInputReport);
    }
    Ok(DescriptorLayout {
        reports,
        uses_report_ids,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DescriptorError {
    InvalidDescriptorSize,
    TruncatedItem,
    LongItemUnsupported,
    InvalidReportId,
    GlobalStackUnderflow,
    UnclosedGlobalStack,
    CollectionUnderflow,
    UnclosedCollection,
    MixedReportIds,
    ArithmeticOverflow,
    InvalidReportSize,
    MissingInputReport,
}

impl fmt::Display for DescriptorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid HID report descriptor: {self:?}")
    }
}

impl std::error::Error for DescriptorError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{KEYBOARD_REPORT_DESCRIPTOR, MOUSE_REPORT_DESCRIPTOR};

    #[test]
    fn project_descriptors_have_the_frozen_lengths() {
        let mouse = parse_report_descriptor(MOUSE_REPORT_DESCRIPTOR).unwrap();
        assert_eq!(mouse.reports.input, BTreeMap::from([(1, 8), (2, 8)]));
        assert!(mouse.reports.output.is_empty());
        let keyboard = parse_report_descriptor(KEYBOARD_REPORT_DESCRIPTOR).unwrap();
        assert_eq!(keyboard.reports.input, BTreeMap::from([(0, 29)]));
        assert_eq!(keyboard.reports.output, BTreeMap::from([(0, 1)]));
    }

    #[test]
    fn rejects_truncation_long_items_and_unbalanced_state() {
        assert_eq!(
            parse_report_descriptor(&[0x76, 1]),
            Err(DescriptorError::TruncatedItem)
        );
        assert_eq!(
            parse_report_descriptor(&[0xfe]),
            Err(DescriptorError::LongItemUnsupported)
        );
        assert_eq!(
            parse_report_descriptor(&[0xc0]),
            Err(DescriptorError::CollectionUnderflow)
        );
        assert_eq!(
            parse_report_descriptor(&[0xa4]),
            Err(DescriptorError::UnclosedGlobalStack)
        );
    }

    #[test]
    fn push_and_pop_restore_report_globals() {
        let descriptor = [
            0xa1, 1, 0x75, 8, 0x95, 1, 0xa4, 0x75, 1, 0x95, 8, 0x81, 2, 0xb4, 0x81, 2, 0xc0,
        ];
        assert_eq!(
            parse_report_descriptor(&descriptor).unwrap().reports.input,
            BTreeMap::from([(0, 2)])
        );
    }
}
