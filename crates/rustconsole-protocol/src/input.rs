//! Input stream framing shared by the player and host.

/// Identifies the first revision of the dedicated reliable input stream.
pub const STREAM_PREAMBLE: [u8; 6] = *b"RCIN\0\x01";
