//! DVR catalog projection.
//!
//! Completed recordings are projected from persisted `RecordingMetadata`.
//! Orphan files are modeled separately as administrator-only entries with no
//! private owner.

use serde::{Deserialize, Serialize};

/// Opaque identifier for an orphan recording entry. This is a typed wrapper
/// around a string so callers cannot accidentally pass a path or a real
/// recording id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OrphanRecordingId(pub String);

/// Source classification for a catalog entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CatalogSource {
    /// The recording is in the persisted queue (normal case).
    Persisted,
    /// The recording is on disk only (orphan/legacy discovery).
    Orphan,
}

/// A single catalog entry. The `owner_id` is `None` for orphan entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingCatalogEntry {
    pub key: CatalogKey,
    pub source: CatalogSource,
    pub display_name: String,
    pub relative_path: String,
    /// Owner subject id. `None` for orphan/legacy entries.
    pub owner_id: Option<crate::model::identity_registry::UserId>,
    pub visibility: Option<crate::model::recording::RecordingVisibility>,
}

/// Deduplication key. We use the relative path as the stable identity.
/// Persisted and orphan entries with the same relative path are deduplicated
/// to the persisted entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CatalogKey(pub String);

impl CatalogKey {
    /// Build a dedup key from a relative path. Same relative path means same
    /// key.
    pub fn from_relative_path(path: &str) -> Self { Self(path.to_string()) }
}

impl RecordingCatalogEntry {
    /// Returns true when the entry is an orphan that only
    /// administrators may see. Non-admin callers must not see this
    /// entry.
    pub fn is_orphan_only(&self) -> bool { matches!(self.source, CatalogSource::Orphan) && self.owner_id.is_none() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{identity_registry::UserId, recording::RecordingVisibility};

    #[test]
    fn dedup_key_uses_relative_path() {
        let key = CatalogKey::from_relative_path("users/web:alice/pilot.ts");
        assert_eq!(key.0, "users/web:alice/pilot.ts");
    }

    #[test]
    fn orphan_only_requires_no_owner_and_orphan_source() {
        let orphan = RecordingCatalogEntry {
            key: CatalogKey::from_relative_path("orphan.ts"),
            source: CatalogSource::Orphan,
            display_name: "orphan".to_string(),
            relative_path: "orphan.ts".to_string(),
            owner_id: None,
            visibility: None,
        };
        assert!(orphan.is_orphan_only());

        let orphan_with_owner = RecordingCatalogEntry { owner_id: Some(UserId::from("web:alice")), ..orphan.clone() };
        assert!(!orphan_with_owner.is_orphan_only());

        let persisted = RecordingCatalogEntry { source: CatalogSource::Persisted, ..orphan };
        assert!(!persisted.is_orphan_only());
        let _ = RecordingVisibility::default();
    }
}
