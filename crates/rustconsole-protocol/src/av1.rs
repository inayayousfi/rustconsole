//! Deterministic hardware-only AV1 capability negotiation.

use std::collections::BTreeSet;
use std::fmt;

pub const MAXIMUM_VIDEO_BITRATE_BITS_PER_SECOND: u64 = 100_000_000;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ChromaSubsampling {
    Yuv420,
    Yuv422,
    Yuv444,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum VideoBitDepth {
    Eight,
    Ten,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Av1Mode {
    pub chroma_subsampling: ChromaSubsampling,
    pub bit_depth: VideoBitDepth,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Av1HardwareCapability {
    pub mode: Av1Mode,
    pub maximum_width: u32,
    pub maximum_height: u32,
    pub maximum_frames_per_second: u16,
}

impl Av1HardwareCapability {
    fn supports(self, settings: &Av1ViewerSettings, mode: Av1Mode) -> bool {
        self.mode == mode
            && settings.width <= self.maximum_width
            && settings.height <= self.maximum_height
            && settings.frames_per_second <= self.maximum_frames_per_second
    }

    fn has_valid_limits(self) -> bool {
        self.maximum_width != 0 && self.maximum_height != 0 && self.maximum_frames_per_second != 0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Av1ViewerSettings {
    pub width: u32,
    pub height: u32,
    pub frames_per_second: u16,
    pub mode_preferences: Vec<Av1Mode>,
    pub maximum_bitrate_bits_per_second: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NegotiatedAv1Configuration {
    pub width: u32,
    pub height: u32,
    pub frames_per_second: u16,
    pub mode: Av1Mode,
    pub maximum_bitrate_bits_per_second: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Av1NegotiationError {
    EmptyEncoderCapabilities,
    EmptyDecoderCapabilities,
    EmptyModePreferences,
    ZeroWidth,
    ZeroHeight,
    ZeroFramesPerSecond,
    ZeroMaximumBitrate,
    MaximumBitrateTooHigh,
    InvalidEncoderCapability(Av1Mode),
    InvalidDecoderCapability(Av1Mode),
    DuplicateEncoderCapability(Av1Mode),
    DuplicateDecoderCapability(Av1Mode),
    DuplicateModePreference(Av1Mode),
    NoCompatibleHardwareMode,
}

impl fmt::Display for Av1NegotiationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyEncoderCapabilities => formatter.write_str("empty AV1 encoder capabilities"),
            Self::EmptyDecoderCapabilities => formatter.write_str("empty AV1 decoder capabilities"),
            Self::EmptyModePreferences => formatter.write_str("empty AV1 mode preferences"),
            Self::ZeroWidth => formatter.write_str("AV1 stream width is zero"),
            Self::ZeroHeight => formatter.write_str("AV1 stream height is zero"),
            Self::ZeroFramesPerSecond => formatter.write_str("AV1 stream frame rate is zero"),
            Self::ZeroMaximumBitrate => formatter.write_str("AV1 maximum bitrate is zero"),
            Self::MaximumBitrateTooHigh => {
                formatter.write_str("AV1 maximum bitrate exceeds 100 Mbit/s")
            }
            Self::InvalidEncoderCapability(mode) => {
                write!(
                    formatter,
                    "AV1 encoder capability has a zero limit: {mode:?}"
                )
            }
            Self::InvalidDecoderCapability(mode) => {
                write!(
                    formatter,
                    "AV1 decoder capability has a zero limit: {mode:?}"
                )
            }
            Self::DuplicateEncoderCapability(mode) => {
                write!(formatter, "duplicate AV1 encoder capability: {mode:?}")
            }
            Self::DuplicateDecoderCapability(mode) => {
                write!(formatter, "duplicate AV1 decoder capability: {mode:?}")
            }
            Self::DuplicateModePreference(mode) => {
                write!(formatter, "duplicate AV1 mode preference: {mode:?}")
            }
            Self::NoCompatibleHardwareMode => {
                formatter.write_str("no compatible hardware AV1 mode")
            }
        }
    }
}

impl std::error::Error for Av1NegotiationError {}

pub fn negotiate_av1_configuration(
    encoder_capabilities: &[Av1HardwareCapability],
    decoder_capabilities: &[Av1HardwareCapability],
    settings: &Av1ViewerSettings,
) -> Result<NegotiatedAv1Configuration, Av1NegotiationError> {
    validate_capabilities(
        encoder_capabilities,
        Av1NegotiationError::EmptyEncoderCapabilities,
        Av1NegotiationError::InvalidEncoderCapability,
        Av1NegotiationError::DuplicateEncoderCapability,
    )?;
    validate_capabilities(
        decoder_capabilities,
        Av1NegotiationError::EmptyDecoderCapabilities,
        Av1NegotiationError::InvalidDecoderCapability,
        Av1NegotiationError::DuplicateDecoderCapability,
    )?;
    validate_settings(settings)?;

    for &mode in &settings.mode_preferences {
        let encoder_supports = encoder_capabilities
            .iter()
            .any(|capability| capability.supports(settings, mode));
        let decoder_supports = decoder_capabilities
            .iter()
            .any(|capability| capability.supports(settings, mode));

        if encoder_supports && decoder_supports {
            return Ok(NegotiatedAv1Configuration {
                width: settings.width,
                height: settings.height,
                frames_per_second: settings.frames_per_second,
                mode,
                maximum_bitrate_bits_per_second: settings.maximum_bitrate_bits_per_second,
            });
        }
    }

    Err(Av1NegotiationError::NoCompatibleHardwareMode)
}

fn validate_capabilities(
    capabilities: &[Av1HardwareCapability],
    empty_error: Av1NegotiationError,
    invalid_error: fn(Av1Mode) -> Av1NegotiationError,
    duplicate_error: fn(Av1Mode) -> Av1NegotiationError,
) -> Result<(), Av1NegotiationError> {
    if capabilities.is_empty() {
        return Err(empty_error);
    }

    let mut modes = BTreeSet::new();
    for &capability in capabilities {
        if !capability.has_valid_limits() {
            return Err(invalid_error(capability.mode));
        }
        if !modes.insert(capability.mode) {
            return Err(duplicate_error(capability.mode));
        }
    }

    Ok(())
}

fn validate_settings(settings: &Av1ViewerSettings) -> Result<(), Av1NegotiationError> {
    if settings.width == 0 {
        return Err(Av1NegotiationError::ZeroWidth);
    }
    if settings.height == 0 {
        return Err(Av1NegotiationError::ZeroHeight);
    }
    if settings.frames_per_second == 0 {
        return Err(Av1NegotiationError::ZeroFramesPerSecond);
    }
    if settings.maximum_bitrate_bits_per_second == 0 {
        return Err(Av1NegotiationError::ZeroMaximumBitrate);
    }
    if settings.maximum_bitrate_bits_per_second > MAXIMUM_VIDEO_BITRATE_BITS_PER_SECOND {
        return Err(Av1NegotiationError::MaximumBitrateTooHigh);
    }
    if settings.mode_preferences.is_empty() {
        return Err(Av1NegotiationError::EmptyModePreferences);
    }

    let mut modes = BTreeSet::new();
    for &mode in &settings.mode_preferences {
        if !modes.insert(mode) {
            return Err(Av1NegotiationError::DuplicateModePreference(mode));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const YUV420_8: Av1Mode = Av1Mode {
        chroma_subsampling: ChromaSubsampling::Yuv420,
        bit_depth: VideoBitDepth::Eight,
    };
    const YUV420_10: Av1Mode = Av1Mode {
        chroma_subsampling: ChromaSubsampling::Yuv420,
        bit_depth: VideoBitDepth::Ten,
    };
    const YUV444_10: Av1Mode = Av1Mode {
        chroma_subsampling: ChromaSubsampling::Yuv444,
        bit_depth: VideoBitDepth::Ten,
    };

    fn capability(mode: Av1Mode) -> Av1HardwareCapability {
        Av1HardwareCapability {
            mode,
            maximum_width: 3_840,
            maximum_height: 2_160,
            maximum_frames_per_second: 120,
        }
    }

    fn settings() -> Av1ViewerSettings {
        Av1ViewerSettings {
            width: 2_560,
            height: 1_440,
            frames_per_second: 120,
            mode_preferences: vec![YUV444_10, YUV420_10, YUV420_8],
            maximum_bitrate_bits_per_second: 80_000_000,
        }
    }

    #[test]
    fn both_peers_compute_the_same_first_supported_preference() {
        let encoder = [
            capability(YUV420_8),
            capability(YUV420_10),
            capability(YUV444_10),
        ];
        let decoder = [capability(YUV420_8), capability(YUV420_10)];
        let settings = settings();

        let host_result = negotiate_av1_configuration(&encoder, &decoder, &settings).unwrap();
        let viewer_result = negotiate_av1_configuration(&encoder, &decoder, &settings).unwrap();

        assert_eq!(host_result, viewer_result);
        assert_eq!(host_result.mode, YUV420_10);
        assert_eq!(host_result.maximum_bitrate_bits_per_second, 80_000_000);
    }

    #[test]
    fn preference_order_is_authoritative() {
        let capabilities = [capability(YUV420_8), capability(YUV420_10)];
        let mut settings = settings();
        settings.mode_preferences = vec![YUV420_8, YUV420_10];

        let result = negotiate_av1_configuration(&capabilities, &capabilities, &settings).unwrap();

        assert_eq!(result.mode, YUV420_8);
    }

    #[test]
    fn changed_decoder_capabilities_rerun_selection() {
        let encoder = [capability(YUV420_8), capability(YUV420_10)];
        let initial_decoder = [capability(YUV420_8), capability(YUV420_10)];
        let changed_decoder = [capability(YUV420_8)];
        let settings = settings();

        assert_eq!(
            negotiate_av1_configuration(&encoder, &initial_decoder, &settings)
                .unwrap()
                .mode,
            YUV420_10
        );
        assert_eq!(
            negotiate_av1_configuration(&encoder, &changed_decoder, &settings)
                .unwrap()
                .mode,
            YUV420_8
        );
    }

    #[test]
    fn limits_must_support_the_complete_requested_configuration() {
        let encoder = [Av1HardwareCapability {
            maximum_frames_per_second: 60,
            ..capability(YUV420_10)
        }];
        let decoder = [capability(YUV420_10)];
        let mut settings = settings();
        settings.mode_preferences = vec![YUV420_10];

        assert_eq!(
            negotiate_av1_configuration(&encoder, &decoder, &settings),
            Err(Av1NegotiationError::NoCompatibleHardwareMode)
        );
    }

    #[test]
    fn no_hardware_intersection_is_rejected() {
        let encoder = [capability(YUV444_10)];
        let decoder = [capability(YUV420_10)];

        assert_eq!(
            negotiate_av1_configuration(&encoder, &decoder, &settings()),
            Err(Av1NegotiationError::NoCompatibleHardwareMode)
        );
    }

    #[test]
    fn empty_and_duplicate_lists_are_rejected() {
        let valid = [capability(YUV420_10)];

        assert_eq!(
            negotiate_av1_configuration(&[], &valid, &settings()),
            Err(Av1NegotiationError::EmptyEncoderCapabilities)
        );
        assert_eq!(
            negotiate_av1_configuration(&valid, &[], &settings()),
            Err(Av1NegotiationError::EmptyDecoderCapabilities)
        );
        assert_eq!(
            negotiate_av1_configuration(&[valid[0], valid[0]], &valid, &settings()),
            Err(Av1NegotiationError::DuplicateEncoderCapability(YUV420_10))
        );

        let mut duplicate_preferences = settings();
        duplicate_preferences.mode_preferences = vec![YUV420_10, YUV420_10];
        assert_eq!(
            negotiate_av1_configuration(&valid, &valid, &duplicate_preferences),
            Err(Av1NegotiationError::DuplicateModePreference(YUV420_10))
        );
    }

    #[test]
    fn zero_settings_and_capability_limits_are_rejected() {
        let valid = [capability(YUV420_10)];
        let invalid = [Av1HardwareCapability {
            maximum_width: 0,
            ..valid[0]
        }];
        let mut zero_bitrate = settings();
        zero_bitrate.maximum_bitrate_bits_per_second = 0;
        let mut excessive_bitrate = settings();
        excessive_bitrate.maximum_bitrate_bits_per_second =
            MAXIMUM_VIDEO_BITRATE_BITS_PER_SECOND + 1;

        assert_eq!(
            negotiate_av1_configuration(&invalid, &valid, &settings()),
            Err(Av1NegotiationError::InvalidEncoderCapability(YUV420_10))
        );
        assert_eq!(
            negotiate_av1_configuration(&valid, &valid, &zero_bitrate),
            Err(Av1NegotiationError::ZeroMaximumBitrate)
        );
        assert_eq!(
            negotiate_av1_configuration(&valid, &valid, &excessive_bitrate),
            Err(Av1NegotiationError::MaximumBitrateTooHigh)
        );
    }
}
