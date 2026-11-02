//! Per-target record of which override revisions the last successful run applied.
//!
//! The store answers one question for the UI: is the override file of a scope newer than
//! what the last successful playlist run of a target consumed? Only successful runs record
//! anything, so a failed run leaves every revision pending.
use crate::override_repository::{OverrideRevision, OverrideScope};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// File name of the status document inside the storage directory.
pub const OVERRIDE_STATUS_FILE_NAME: &str = "override_status.json";

/// Why an override file of a run was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkippedOverrideFileReason {
    /// The file could not be parsed.
    Malformed,
    /// The file declares an unsupported schema version.
    Unsupported,
}

/// An override file a run did not apply, together with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedOverrideFile {
    /// Name of the override file relative to the storage directory.
    pub name: String,
    /// Why the file was skipped.
    pub reason: SkippedOverrideFileReason,
}

impl SkippedOverrideFile {
    #[must_use]
    pub fn malformed(name: impl Into<String>) -> Self {
        Self { name: name.into(), reason: SkippedOverrideFileReason::Malformed }
    }

    #[must_use]
    pub fn unsupported(name: impl Into<String>) -> Self {
        Self { name: name.into(), reason: SkippedOverrideFileReason::Unsupported }
    }
}

/// Diagnostics of one playlist run, persisted next to the applied revisions of its target.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OverrideRunDiagnostics {
    /// Override files the run applied.
    #[serde(default)]
    pub applied_files: Vec<String>,
    /// Override files the run skipped because it could not use them.
    #[serde(default)]
    pub skipped_files: Vec<SkippedOverrideFile>,
    /// Number of override keys that resolved to more than one item.
    #[serde(default)]
    pub ambiguous_keys: usize,
    /// Completion time of the run as unix seconds.
    #[serde(default)]
    pub finished_at: u64,
}

impl OverrideRunDiagnostics {
    /// Builds diagnostics for a run that just finished; `finished_at` is stamped with the current time.
    #[must_use]
    pub fn new(applied_files: Vec<String>, skipped_files: Vec<SkippedOverrideFile>, ambiguous_keys: usize) -> Self {
        Self { applied_files, skipped_files, ambiguous_keys, finished_at: unix_now() }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())
}

/// One `(scope, revision)` pair a run applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AppliedScopeRevision {
    scope: OverrideScope,
    revision: OverrideRevision,
}

/// Status of a single target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct TargetStatus {
    /// Revisions the last successful run of this target applied.
    #[serde(default)]
    applied: Vec<AppliedScopeRevision>,
    /// Diagnostics of that run.
    #[serde(default)]
    diagnostics: Option<OverrideRunDiagnostics>,
}

/// On-disk document: target name to the revisions its last successful run applied.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct OverrideStatusFile {
    #[serde(default)]
    targets: BTreeMap<String, TargetStatus>,
}

impl OverrideStatusFile {
    fn applied_revision(&self, target: &str, scope: &OverrideScope) -> Option<OverrideRevision> {
        self.targets.get(target)?.applied.iter().find(|entry| entry.scope == *scope).map(|entry| entry.revision.clone())
    }
}

/// Records which override revisions the last successful run of each target applied.
///
/// The document is loaded lazily on first access and cached in memory. Writes are
/// serialized by a mutex and land atomically (temp file in the same directory, then rename).
pub struct OverrideStatusStore {
    dir: PathBuf,
    state: tokio::sync::Mutex<Option<OverrideStatusFile>>,
}

impl OverrideStatusStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self { Self { dir: dir.into(), state: tokio::sync::Mutex::new(None) } }

    /// Directory holding the status document.
    #[must_use]
    pub fn dir(&self) -> &Path { &self.dir }

    /// Path of the status document.
    #[must_use]
    pub fn status_path(&self) -> PathBuf { self.dir.join(OVERRIDE_STATUS_FILE_NAME) }

    /// Revision recorded for `scope` by the last successful run of `target`, if any.
    pub async fn applied_revision(&self, target: &str, scope: &OverrideScope) -> Option<OverrideRevision> {
        let mut guard = self.state.lock().await;
        let state = self.load_locked(&mut guard).await;
        state.applied_revision(target, scope)
    }

    /// Whether `target` previously had a status record in the store.
    pub async fn has_target(&self, target: &str) -> bool {
        let mut guard = self.state.lock().await;
        let state = self.load_locked(&mut guard).await;
        state.targets.contains_key(target)
    }

    /// `true` if `current` differs from the revision recorded for `scope` by the last successful
    /// run of `target`, or if nothing was recorded yet.
    ///
    /// A missing (empty) override file with no recorded revision has nothing to apply, so it is
    /// never pending.
    pub async fn is_pending(&self, target: &str, scope: &OverrideScope, current: &OverrideRevision) -> bool {
        match self.applied_revision(target, scope).await {
            Some(applied) => applied != *current,
            None => *current != OverrideRevision::empty(),
        }
    }

    /// Records the revisions `target` applied in a successful run, replacing what the previous
    /// successful run recorded for that target.
    ///
    /// # Errors
    /// Returns `std::io::Error` if the status document cannot be serialized or persisted. The
    /// in-memory state is left unchanged in that case, so the affected revisions stay pending.
    pub async fn record_success(
        &self,
        target: &str,
        revisions: Vec<(OverrideScope, OverrideRevision)>,
        diagnostics: OverrideRunDiagnostics,
    ) -> std::io::Result<()> {
        let mut guard = self.state.lock().await;
        let mut next = self.load_locked(&mut guard).await.clone();

        next.targets.insert(
            target.to_string(),
            TargetStatus {
                applied: revisions
                    .into_iter()
                    .map(|(scope, revision)| AppliedScopeRevision { scope, revision })
                    .collect(),
                diagnostics: Some(diagnostics),
            },
        );

        self.write_document(&next).await?;

        let recorded = next.targets.get(target).cloned();
        *guard = Some(next);

        if let Some(TargetStatus { applied, diagnostics }) = recorded {
            let revision_count = applied.len();
            if let Some(diagnostics) = diagnostics {
                for skipped in &diagnostics.skipped_files {
                    log::warn!("Target '{target}' skipped override file {}: {:?}", skipped.name, skipped.reason);
                }
                log::info!(
                    "Override status for target '{target}': {revision_count} revision(s) applied, {} file(s) applied, {} skipped, {} ambiguous key(s), finished_at {}",
                    diagnostics.applied_files.len(),
                    diagnostics.skipped_files.len(),
                    diagnostics.ambiguous_keys,
                    diagnostics.finished_at
                );
            } else {
                log::info!("Override status for target '{target}': {revision_count} revision(s) applied");
            }
        }

        Ok(())
    }

    /// Returns the cached document, loading it from disk on first access.
    async fn load_locked<'a>(&self, guard: &'a mut Option<OverrideStatusFile>) -> &'a OverrideStatusFile {
        if guard.is_none() {
            *guard = Some(self.read_document().await);
        }
        match guard {
            Some(document) => document,
            None => unreachable!("status document was just loaded"),
        }
    }

    /// Reads the status document, treating a missing, unreadable, or corrupt file as empty.
    async fn read_document(&self) -> OverrideStatusFile {
        let path = self.status_path();
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return OverrideStatusFile::default(),
            Err(err) => {
                log::warn!("Cannot read override status file {}: {err}", path.display());
                return OverrideStatusFile::default();
            }
        };

        serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            log::warn!("Override status file {} is corrupt, treating it as empty: {err}", path.display());
            OverrideStatusFile::default()
        })
    }

    /// Persists the document atomically: temp file in the same directory, then rename.
    async fn write_document(&self, document: &OverrideStatusFile) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;

        tokio::fs::create_dir_all(&self.dir).await?;
        let payload = serde_json::to_vec_pretty(document)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;

        let target_path = self.status_path();
        let temp_path = self.dir.join(format!(".{OVERRIDE_STATUS_FILE_NAME}.{}.tmp", uuid::Uuid::new_v4()));

        let write_result: std::io::Result<()> = async {
            let mut temp_file = tokio::fs::File::create(&temp_path).await?;
            temp_file.write_all(&payload).await?;
            temp_file.write_all(b"\n").await?;
            temp_file.sync_all().await?;
            drop(temp_file);
            tokio::fs::rename(&temp_path, &target_path).await
        }
        .await;

        if write_result.is_err() {
            let _ = tokio::fs::remove_file(&temp_path).await;
        }

        write_result
    }
}

#[cfg(test)]
mod tests;
