use crate::utils::FailoverSource;
use shared::model::{OverrideValidationError, TargetFailoverOverridesDto};
use std::{collections::HashMap, path::PathBuf, sync::Arc};

/// One target section of the failover files, canonicalized, with the file that owns it.
#[derive(Debug, Clone)]
pub struct TargetOverridesEntry {
    pub overrides: TargetFailoverOverridesDto,
    pub file: PathBuf,
    pub etag: Arc<str>,
    /// Non-fatal validation findings computed when the section was loaded.
    pub findings: Vec<OverrideValidationError>,
}

/// Immutable snapshot of all failover override files; swapped atomically on load, reload and write-back.
#[derive(Debug, Clone, Default)]
pub struct FailoverOverrides {
    pub source: Option<FailoverSource>,
    pub sections: HashMap<Arc<str>, Arc<TargetOverridesEntry>>,
    pub files_used: Vec<PathBuf>,
    /// Fatal load error. While set, the snapshot is empty and write-back is refused.
    pub error: Option<String>,
}
