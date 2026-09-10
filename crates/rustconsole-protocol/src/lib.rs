//! Versioned, platform-neutral Rust Console protocol types.

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU16;

pub mod audio;
pub mod av1;
pub mod diagnostics;
pub mod input;
pub mod wire;

pub use av1::{
    Av1HardwareCapability, Av1Mode, Av1NegotiationError, Av1ViewerSettings, ChromaSubsampling,
    NegotiatedAv1Configuration, VideoBitDepth, negotiate_av1_configuration,
};

/// One platform-neutral input event before channel-specific wire encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputEvent {
    Key { hid_usage: u16, pressed: bool },
    ReleaseAll,
    PointerButton { button: u8, pressed: bool },
    PointerMotion { delta_x: i32, delta_y: i32 },
    PointerPosition { x: u16, y: u16 },
    Wheel { horizontal: i16, vertical: i16 },
}

/// The protocol version implemented by this build.
pub const CURRENT_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(1, 4);

/// A protocol version whose major number marks breaking changes.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    #[must_use]
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    /// Selects the newest minor version understood by both peers.
    pub fn negotiate(self, peer: Self) -> Result<Self, ProtocolVersionMismatch> {
        if self.major != peer.major {
            return Err(ProtocolVersionMismatch { local: self, peer });
        }

        Ok(Self::new(self.major, self.minor.min(peer.minor)))
    }
}

/// Two peers cannot communicate because their protocol majors differ.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolVersionMismatch {
    pub local: ProtocolVersion,
    pub peer: ProtocolVersion,
}

impl fmt::Display for ProtocolVersionMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "protocol major mismatch: local {}.{}, peer {}.{}",
            self.local.major, self.local.minor, self.peer.major, self.peer.minor
        )
    }
}

impl std::error::Error for ProtocolVersionMismatch {}

/// A stable numeric feature identifier. Zero is reserved for invalid data.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FeatureId(NonZeroU16);

impl FeatureId {
    pub const fn new(value: u16) -> Option<Self> {
        match NonZeroU16::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

/// An inclusive range of versions supported for one feature.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeatureVersionRange {
    pub oldest: u16,
    pub newest: u16,
}

impl FeatureVersionRange {
    pub const fn new(oldest: u16, newest: u16) -> Result<Self, InvalidFeatureVersionRange> {
        if oldest > newest {
            return Err(InvalidFeatureVersionRange { oldest, newest });
        }

        Ok(Self { oldest, newest })
    }

    const fn highest_common(self, peer: Self) -> Option<u16> {
        let oldest = if self.oldest > peer.oldest {
            self.oldest
        } else {
            peer.oldest
        };
        let newest = if self.newest < peer.newest {
            self.newest
        } else {
            peer.newest
        };

        if oldest <= newest { Some(newest) } else { None }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidFeatureVersionRange {
    pub oldest: u16,
    pub newest: u16,
}

impl fmt::Display for InvalidFeatureVersionRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid feature version range: {} is newer than {}",
            self.oldest, self.newest
        )
    }
}

impl std::error::Error for InvalidFeatureVersionRange {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeatureRequirement {
    Optional,
    Required,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeatureOffer {
    pub id: FeatureId,
    pub versions: FeatureVersionRange,
    pub requirement: FeatureRequirement,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NegotiatedFeature {
    pub id: FeatureId,
    pub version: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeatureNegotiationError {
    DuplicateLocalFeature(FeatureId),
    DuplicatePeerFeature(FeatureId),
    MissingRequiredLocalFeature(FeatureId),
    MissingRequiredPeerFeature(FeatureId),
    IncompatibleRequiredFeature(FeatureId),
}

impl fmt::Display for FeatureNegotiationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (message, id) = match self {
            Self::DuplicateLocalFeature(id) => ("duplicate local feature", id),
            Self::DuplicatePeerFeature(id) => ("duplicate peer feature", id),
            Self::MissingRequiredLocalFeature(id) => ("peer is missing required local feature", id),
            Self::MissingRequiredPeerFeature(id) => ("local is missing required peer feature", id),
            Self::IncompatibleRequiredFeature(id) => ("required feature has no common version", id),
        };

        write!(formatter, "{message}: {}", id.get())
    }
}

impl std::error::Error for FeatureNegotiationError {}

/// Negotiates independent features in stable identifier order.
pub fn negotiate_features(
    local: &[FeatureOffer],
    peer: &[FeatureOffer],
) -> Result<Vec<NegotiatedFeature>, FeatureNegotiationError> {
    let local = collect_offers(local, FeatureNegotiationError::DuplicateLocalFeature)?;
    let peer = collect_offers(peer, FeatureNegotiationError::DuplicatePeerFeature)?;
    let mut negotiated = Vec::new();

    for (id, local_offer) in &local {
        let Some(peer_offer) = peer.get(id) else {
            if local_offer.requirement == FeatureRequirement::Required {
                return Err(FeatureNegotiationError::MissingRequiredLocalFeature(*id));
            }
            continue;
        };

        match local_offer.versions.highest_common(peer_offer.versions) {
            Some(version) => negotiated.push(NegotiatedFeature { id: *id, version }),
            None if local_offer.requirement == FeatureRequirement::Required
                || peer_offer.requirement == FeatureRequirement::Required =>
            {
                return Err(FeatureNegotiationError::IncompatibleRequiredFeature(*id));
            }
            None => {}
        }
    }

    for (id, peer_offer) in &peer {
        if peer_offer.requirement == FeatureRequirement::Required && !local.contains_key(id) {
            return Err(FeatureNegotiationError::MissingRequiredPeerFeature(*id));
        }
    }

    Ok(negotiated)
}

fn collect_offers(
    offers: &[FeatureOffer],
    duplicate_error: fn(FeatureId) -> FeatureNegotiationError,
) -> Result<BTreeMap<FeatureId, FeatureOffer>, FeatureNegotiationError> {
    let mut collected = BTreeMap::new();
    for offer in offers {
        if collected.insert(offer.id, *offer).is_some() {
            return Err(duplicate_error(offer.id));
        }
    }
    Ok(collected)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEATURE_1: FeatureId = FeatureId::new(1).unwrap();
    const FEATURE_2: FeatureId = FeatureId::new(2).unwrap();

    fn offer(
        id: FeatureId,
        oldest: u16,
        newest: u16,
        requirement: FeatureRequirement,
    ) -> FeatureOffer {
        FeatureOffer {
            id,
            versions: FeatureVersionRange::new(oldest, newest).unwrap(),
            requirement,
        }
    }

    #[test]
    fn matching_protocol_major_selects_lower_minor() {
        assert_eq!(
            ProtocolVersion::new(1, 4).negotiate(ProtocolVersion::new(1, 2)),
            Ok(ProtocolVersion::new(1, 2))
        );
    }

    #[test]
    fn different_protocol_major_is_rejected() {
        assert_eq!(
            ProtocolVersion::new(1, 4).negotiate(ProtocolVersion::new(2, 0)),
            Err(ProtocolVersionMismatch {
                local: ProtocolVersion::new(1, 4),
                peer: ProtocolVersion::new(2, 0),
            })
        );
    }

    #[test]
    fn invalid_feature_range_is_rejected() {
        assert_eq!(
            FeatureVersionRange::new(3, 2),
            Err(InvalidFeatureVersionRange {
                oldest: 3,
                newest: 2,
            })
        );
    }

    #[test]
    fn feature_negotiation_selects_highest_common_version_in_id_order() {
        let local = [
            offer(FEATURE_2, 1, 4, FeatureRequirement::Optional),
            offer(FEATURE_1, 2, 5, FeatureRequirement::Required),
        ];
        let peer = [
            offer(FEATURE_1, 3, 4, FeatureRequirement::Optional),
            offer(FEATURE_2, 2, 3, FeatureRequirement::Required),
        ];

        assert_eq!(
            negotiate_features(&local, &peer),
            Ok(vec![
                NegotiatedFeature {
                    id: FEATURE_1,
                    version: 4,
                },
                NegotiatedFeature {
                    id: FEATURE_2,
                    version: 3,
                },
            ])
        );
    }

    #[test]
    fn unsupported_optional_features_are_ignored() {
        let local = [offer(FEATURE_1, 1, 1, FeatureRequirement::Optional)];
        let peer = [offer(FEATURE_2, 1, 1, FeatureRequirement::Optional)];

        assert_eq!(negotiate_features(&local, &peer), Ok(Vec::new()));
    }

    #[test]
    fn missing_required_feature_on_either_side_is_rejected() {
        let required = [offer(FEATURE_1, 1, 1, FeatureRequirement::Required)];

        assert_eq!(
            negotiate_features(&required, &[]),
            Err(FeatureNegotiationError::MissingRequiredLocalFeature(
                FEATURE_1
            ))
        );
        assert_eq!(
            negotiate_features(&[], &required),
            Err(FeatureNegotiationError::MissingRequiredPeerFeature(
                FEATURE_1
            ))
        );
    }

    #[test]
    fn incompatible_optional_feature_is_ignored_but_required_is_rejected() {
        let local = [offer(FEATURE_1, 1, 2, FeatureRequirement::Optional)];
        let optional_peer = [offer(FEATURE_1, 3, 4, FeatureRequirement::Optional)];
        let required_peer = [offer(FEATURE_1, 3, 4, FeatureRequirement::Required)];

        assert_eq!(negotiate_features(&local, &optional_peer), Ok(Vec::new()));
        assert_eq!(
            negotiate_features(&local, &required_peer),
            Err(FeatureNegotiationError::IncompatibleRequiredFeature(
                FEATURE_1
            ))
        );
    }

    #[test]
    fn duplicate_feature_is_rejected_on_either_side() {
        let duplicate = [
            offer(FEATURE_1, 1, 1, FeatureRequirement::Optional),
            offer(FEATURE_1, 1, 2, FeatureRequirement::Optional),
        ];

        assert_eq!(
            negotiate_features(&duplicate, &[]),
            Err(FeatureNegotiationError::DuplicateLocalFeature(FEATURE_1))
        );
        assert_eq!(
            negotiate_features(&[], &duplicate),
            Err(FeatureNegotiationError::DuplicatePeerFeature(FEATURE_1))
        );
    }
}
