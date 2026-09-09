//! Canonical field names for structured session traces.

pub const SESSION_ID: &str = "session_id";
pub const CONNECTION_ID: &str = "connection_id";
pub const SIDE: &str = "side";
pub const PHASE: &str = "phase";
pub const CHANNEL: &str = "channel";
pub const STAGE: &str = "stage";
pub const QUEUE: &str = "queue";
pub const FRAME_SEQUENCE: &str = "frame_sequence";
pub const PACKET_SEQUENCE: &str = "packet_sequence";
pub const DURATION_MICROS: &str = "duration_micros";
pub const QUEUE_RESIDENCE_MICROS: &str = "queue_residence_micros";
pub const SIZE_BYTES: &str = "size_bytes";
pub const REASON: &str = "reason";
pub const ERROR: &str = "error";

pub const ALL: [&str; 14] = [
    SESSION_ID,
    CONNECTION_ID,
    SIDE,
    PHASE,
    CHANNEL,
    STAGE,
    QUEUE,
    FRAME_SEQUENCE,
    PACKET_SEQUENCE,
    DURATION_MICROS,
    QUEUE_RESIDENCE_MICROS,
    SIZE_BYTES,
    REASON,
    ERROR,
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn trace_field_names_are_unique_and_machine_friendly() {
        let unique = ALL.into_iter().collect::<BTreeSet<_>>();

        assert_eq!(unique.len(), ALL.len());
        assert!(ALL.into_iter().all(|field| {
            !field.is_empty()
                && field
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        }));
    }
}
