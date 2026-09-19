//! Revoking tokens that have already been issued.
//!
//! The tokens this server mints are stateless JWTs: once issued, nothing could
//! take one back. A leaked token stayed valid until it expired, and there was
//! no way to end a session, sign a principal out of every device, or respond
//! to a compromise short of rotating the signing secret - which invalidates
//! every session for every principal at once.
//!
//! This is a revocation *watermark* rather than a deny-list of individual
//! tokens: per subject, "everything issued at or before this instant is dead",
//! plus one global watermark for the same statement across all subjects. Two
//! properties fall out of that shape:
//!
//! * it is bounded. A deny-list grows with every revoked token and needs an
//!   expiry sweep to stay finite; a watermark is one timestamp per principal
//!   and one for the server.
//! * it revokes sessions a deny-list cannot name. "Sign out everywhere" and
//!   "revoke everything issued before the breach" do not have a list of token
//!   ids behind them.
//!
//! It costs precision: a watermark cannot revoke one session and spare
//! another issued in the same second. That is the right trade for the thing it
//! is for.
//!
//! Revocations are persisted, because the tokens they revoke survive a
//! restart. An in-memory revocation would be a security control that quietly
//! stops applying the next time the process starts.

use serde::{Deserialize, Serialize};
use shared::model::{Claims, UserId};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
};
use tokio::sync::RwLock;

/// The on-disk schema version. Bump when the shape changes.
pub const CURRENT_REVOCATIONS_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedRevocations {
    #[serde(default)]
    pub version: u32,
    /// Per subject: tokens issued at or before this unix second are revoked.
    #[serde(default)]
    pub by_subject: HashMap<String, i64>,
    /// Across every subject. `0` means never.
    #[serde(default)]
    pub global: i64,
}

/// Persisted revocation watermarks.
pub struct TokenRevocations {
    state: RwLock<PersistedRevocations>,
    path: PathBuf,
}

impl TokenRevocations {
    /// Load from disk, or start empty.
    ///
    /// A file that will not parse is *not* treated as "nothing is revoked" -
    /// that would silently reinstate every revoked session. It is reported, and
    /// the caller decides.
    pub async fn load(path: PathBuf) -> Result<Self, io::Error> {
        match tokio::fs::read(&path).await {
            Ok(bytes) if bytes.is_empty() => {
                Err(io::Error::new(io::ErrorKind::InvalidData, "token revocation file is empty"))
            }
            Ok(bytes) => {
                let mut state: PersistedRevocations = serde_json::from_slice(&bytes)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
                state.version = CURRENT_REVOCATIONS_VERSION;
                Ok(Self { state: RwLock::new(state), path })
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::empty(path)),
            Err(err) => Err(err),
        }
    }

    /// An empty store that never touches disk until something is revoked.
    pub fn empty(path: PathBuf) -> Self {
        Self {
            state: RwLock::new(PersistedRevocations {
                version: CURRENT_REVOCATIONS_VERSION,
                ..PersistedRevocations::default()
            }),
            path,
        }
    }

    /// Revoke every token issued for `subject` at or before `at`.
    ///
    /// `at` is normally "now". Watermarks only move forward: a second call
    /// with an earlier instant cannot resurrect sessions the first one killed.
    pub async fn revoke_subject(&self, subject: &UserId, at: i64) -> Result<(), io::Error> {
        let snapshot = {
            let mut state = self.state.write().await;
            let entry = state.by_subject.entry(subject.0.clone()).or_insert(at);
            *entry = (*entry).max(at);
            state.clone()
        };
        Self::write(&self.path, &snapshot).await
    }

    /// Revoke every token for every subject issued at or before `at`.
    pub async fn revoke_all(&self, at: i64) -> Result<(), io::Error> {
        let snapshot = {
            let mut state = self.state.write().await;
            state.global = state.global.max(at);
            state.clone()
        };
        Self::write(&self.path, &snapshot).await
    }

    /// `true` when this token has been revoked.
    ///
    /// The comparison is `<=`, not `<`: `iat` has one-second resolution, so a
    /// token minted in the same second as the revocation would otherwise
    /// survive it. Over-revoking by up to a second is the safe direction.
    pub async fn is_revoked(&self, claims: &Claims) -> bool {
        let state = self.state.read().await;
        if state.global > 0 && claims.iat <= state.global {
            return true;
        }
        claims
            .subject_id
            .as_ref()
            .and_then(|subject| state.by_subject.get(&subject.0))
            .is_some_and(|watermark| claims.iat <= *watermark)
    }

    async fn write(path: &Path, state: &PersistedRevocations) -> Result<(), io::Error> {
        if path.as_os_str().is_empty() {
            // The empty path is the test/no-persistence case.
            return Ok(());
        }
        let bytes = serde_json::to_vec_pretty(state)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{PermissionSet, RoleSet, CURRENT_PERMISSION_SCHEMA_VERSION};

    fn claims_for(subject: &str, iat: i64) -> Claims {
        Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat,
            exp: iat + 3600,
            roles: RoleSet::new(),
            permissions: PermissionSet::new(),
            pwd_version: 1,
            subject_id: Some(UserId::from(subject)),
            permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
        }
    }

    #[tokio::test]
    async fn nothing_is_revoked_by_default() {
        let store = TokenRevocations::empty(PathBuf::new());
        assert!(!store.is_revoked(&claims_for("web:alice", 1_000)).await);
    }

    #[tokio::test]
    async fn revoking_a_subject_kills_older_tokens_and_spares_newer_ones() {
        let store = TokenRevocations::empty(PathBuf::new());
        store.revoke_subject(&UserId::from("web:alice"), 1_000).await.expect("revoke");

        assert!(store.is_revoked(&claims_for("web:alice", 999)).await);
        // Same second as the revocation: killed, because `iat` cannot
        // distinguish "just before" from "just after".
        assert!(store.is_revoked(&claims_for("web:alice", 1_000)).await);
        assert!(!store.is_revoked(&claims_for("web:alice", 1_001)).await);
        // Another subject is untouched.
        assert!(!store.is_revoked(&claims_for("web:bob", 999)).await);
    }

    #[tokio::test]
    async fn the_global_watermark_covers_every_subject() {
        let store = TokenRevocations::empty(PathBuf::new());
        store.revoke_all(2_000).await.expect("revoke all");

        assert!(store.is_revoked(&claims_for("web:alice", 1_999)).await);
        assert!(store.is_revoked(&claims_for("api:bob", 2_000)).await);
        assert!(!store.is_revoked(&claims_for("web:alice", 2_001)).await);
    }

    #[tokio::test]
    async fn watermarks_only_move_forward() {
        let store = TokenRevocations::empty(PathBuf::new());
        store.revoke_subject(&UserId::from("web:alice"), 2_000).await.expect("revoke");
        // An earlier revocation must not resurrect what the later one killed.
        store.revoke_subject(&UserId::from("web:alice"), 1_000).await.expect("revoke");
        assert!(store.is_revoked(&claims_for("web:alice", 1_500)).await);

        store.revoke_all(2_000).await.expect("revoke all");
        store.revoke_all(1_000).await.expect("revoke all");
        assert!(store.is_revoked(&claims_for("web:bob", 1_500)).await);
    }

    #[tokio::test]
    async fn a_token_with_no_subject_is_only_caught_by_the_global_watermark() {
        let store = TokenRevocations::empty(PathBuf::new());
        let mut claims = claims_for("web:alice", 1_000);
        claims.subject_id = None;

        store.revoke_subject(&UserId::from("web:alice"), 5_000).await.expect("revoke");
        assert!(!store.is_revoked(&claims).await);

        store.revoke_all(5_000).await.expect("revoke all");
        assert!(store.is_revoked(&claims).await);
    }

    #[tokio::test]
    async fn revocations_survive_a_reload() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("revocations.json");

        let store = TokenRevocations::load(path.clone()).await.expect("load");
        store.revoke_subject(&UserId::from("web:alice"), 1_000).await.expect("revoke");

        // The tokens a revocation kills outlive the process that killed them,
        // so the revocation has to as well.
        let reloaded = TokenRevocations::load(path).await.expect("reload");
        assert!(reloaded.is_revoked(&claims_for("web:alice", 999)).await);
    }

    #[tokio::test]
    async fn a_corrupt_file_is_an_error_not_an_empty_store() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("revocations.json");
        tokio::fs::write(&path, b"{ not json").await.expect("write");

        // Reading it as "nothing is revoked" would silently reinstate every
        // revoked session.
        assert!(TokenRevocations::load(path).await.is_err());
    }
}
