//! Display identities describe capture sources, independently of a codec or GPU vendor.

use std::fmt;

pub const MAX_DISPLAYS: usize = 32;
pub const MAX_ADAPTERS: usize = 32;
pub const MAX_DISPLAY_ID_BYTES: usize = 512;

/// An opaque host-scoped identity, resolved against a fresh inventory rather than an ordinal.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct DisplayId(String);

impl DisplayId {
    pub fn new(value: String) -> Result<Self, DisplayError> {
        if value.is_empty()
            || value.len() > MAX_DISPLAY_ID_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(DisplayError::InvalidIdentity);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An adapter identity in the current host topology, not a persistent GPU ordinal.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct AdapterId(pub u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphicsAdapter {
    pub id: AdapterId,
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub software: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Display {
    pub id: DisplayId,
    pub name: String,
    pub adapter: AdapterId,
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
    pub primary: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum DisplaySelection {
    #[default]
    Primary,
    Id(DisplayId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisplayInventory {
    displays: Vec<Display>,
    adapters: Vec<GraphicsAdapter>,
}

impl DisplayInventory {
    pub fn new(
        displays: Vec<Display>,
        adapters: Vec<GraphicsAdapter>,
    ) -> Result<Self, DisplayError> {
        use std::collections::BTreeSet;
        if displays.len() > MAX_DISPLAYS || adapters.len() > MAX_ADAPTERS {
            return Err(DisplayError::TooManyDevices);
        }
        let mut adapter_ids = BTreeSet::new();
        for adapter in &adapters {
            if adapter.name.len() > MAX_DISPLAY_ID_BYTES
                || adapter.name.chars().any(char::is_control)
            {
                return Err(DisplayError::InvalidIdentity);
            }
            if !adapter_ids.insert(adapter.id) {
                return Err(DisplayError::DuplicateIdentity);
            }
        }
        let mut display_ids = BTreeSet::new();
        for display in &displays {
            if display.name.len() > MAX_DISPLAY_ID_BYTES
                || display.name.chars().any(char::is_control)
            {
                return Err(DisplayError::InvalidIdentity);
            }
            if !display_ids.insert(&display.id) {
                return Err(DisplayError::DuplicateIdentity);
            }
            if !adapter_ids.contains(&display.adapter) {
                return Err(DisplayError::MissingAdapter);
            }
            if display.width == 0 || display.height == 0 {
                return Err(DisplayError::InvalidDimensions);
            }
        }
        Ok(Self { displays, adapters })
    }

    pub fn displays(&self) -> &[Display] {
        &self.displays
    }
    pub fn adapters(&self) -> &[GraphicsAdapter] {
        &self.adapters
    }

    pub fn resolve(&self, selection: &DisplaySelection) -> Result<&Display, DisplayError> {
        let mut matches = self.displays.iter().filter(|display| match selection {
            DisplaySelection::Primary => display.primary,
            DisplaySelection::Id(id) => &display.id == id,
        });
        let display = matches.next().ok_or(DisplayError::Unavailable)?;
        if matches.next().is_some() {
            return Err(DisplayError::Ambiguous);
        }
        Ok(display)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayError {
    InvalidIdentity,
    TooManyDevices,
    DuplicateIdentity,
    MissingAdapter,
    InvalidDimensions,
    Unavailable,
    Ambiguous,
}

impl fmt::Display for DisplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidIdentity => "invalid display identity",
            Self::TooManyDevices => "display inventory exceeds its device bound",
            Self::DuplicateIdentity => "duplicate device identity",
            Self::MissingAdapter => "display references an unavailable adapter",
            Self::InvalidDimensions => "display dimensions are zero",
            Self::Unavailable => "selected display is unavailable",
            Self::Ambiguous => "display selection is ambiguous",
        })
    }
}

impl std::error::Error for DisplayError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_survives_reordering_and_resolves_the_current_adapter() {
        let adapter = |id| GraphicsAdapter {
            id: AdapterId(id),
            name: "GPU".into(),
            vendor_id: 0,
            device_id: 0,
            software: false,
        };
        let display = |id: &str, gpu, primary| Display {
            id: DisplayId::new(id.into()).unwrap(),
            name: id.into(),
            adapter: AdapterId(gpu),
            width: 1920,
            height: 1080,
            refresh_rate: 60,
            primary,
        };
        let selection = DisplaySelection::Id(DisplayId::new("panel".into()).unwrap());
        let before = DisplayInventory::new(
            vec![display("panel", 1, true), display("virtual", 2, false)],
            vec![adapter(1), adapter(2)],
        )
        .unwrap();
        let after = DisplayInventory::new(
            vec![display("virtual", 2, true), display("panel", 2, false)],
            vec![adapter(2)],
        )
        .unwrap();
        assert_eq!(before.resolve(&selection).unwrap().adapter, AdapterId(1));
        assert_eq!(after.resolve(&selection).unwrap().adapter, AdapterId(2));
        let missing = DisplaySelection::Id(DisplayId::new("removed".into()).unwrap());
        assert_eq!(after.resolve(&missing), Err(DisplayError::Unavailable));
        assert_eq!(
            after
                .resolve(&DisplaySelection::Primary)
                .unwrap()
                .id
                .as_str(),
            "virtual"
        );
        let catalog = crate::wire::DisplayCatalog::from(&after);
        assert_eq!(DisplayInventory::try_from(catalog.clone()).unwrap(), after);
        let mut missing_adapter = catalog;
        missing_adapter.adapters.clear();
        assert_eq!(
            DisplayInventory::try_from(missing_adapter),
            Err(DisplayError::MissingAdapter)
        );
    }

    #[test]
    fn rejects_invalid_and_unbounded_identifiers() {
        for id in [
            String::new(),
            "bad\0id".into(),
            "x".repeat(MAX_DISPLAY_ID_BYTES + 1),
        ] {
            assert_eq!(DisplayId::new(id), Err(DisplayError::InvalidIdentity));
        }
    }
}
