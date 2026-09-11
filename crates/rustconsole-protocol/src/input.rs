//! Input stream framing shared by the player and host.

/// Identifies the bounded-pack revision of the dedicated reliable input stream.
pub const STREAM_PREAMBLE: [u8; 6] = *b"RCIN\0\x02";

/// Keeps every input message well below the reliable control-frame limit.
pub const MAX_EVENTS_PER_PACK: usize = 256;
