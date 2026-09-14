//! Legacy User API value schemas, and the normalized recovery record they
//! all migrate into.
//!
//! Two independent version axes meet here, and confusing them is the main
//! hazard:
//!
//! - The **container** version is the B+Tree storage format (1, 2 or 3).
//!   It decides how to open the file at all.
//! - The **value** version is the positional layout of the record inside
//!   (V1 through V7). It decides how to read a record once the file is open.
//!
//! Any container may hold any value version, so the supported matrix is all
//! 21 combinations. Neither axis can be inferred from the other.
//!
//! The `StoredApiUserV1`..`StoredApiUserV7` types are the historical layouts,
//! moved here from `startup_migration` rather than copied: two definitions of
//! the same legacy record would drift, and a drifted legacy decoder silently
//! loses user data. **Field order is load-bearing** — these are decoded
//! positionally from `MessagePack`, so reordering a field reinterprets every
//! stored record.
//!
//! [`ApiUserRecovery`] is the field-named normalized form. It exists so a
//! future change to the current credential type is a migration rather than a
//! loss: recovery records carry their names, so they can be migrated
//! forward, while the positional legacy layouts cannot.

use crate::user_repository::StoredProxyUserCredentials;
use serde_json::Value;
use shared::model::{ClusterFlags, NetworkAccessDto, ProxyType, ProxyUserStatus};
use std::{io, path::Path};
use tuliprox_btree::{
    BPlusTreeRecoveryJournal, RecoveryBatch, RecoveryOpenReport, RecoveryOperation, RecoveryPaths, RecoveryPolicy,
    RecoverySchema,
};

/// B+Tree storage container versions this importer can read.
#[cfg(test)]
pub(crate) const SUPPORTED_CONTAINER_VERSIONS: [u16; 3] = [1, 2, 3];

/// Positional value layouts this importer can read, newest first.
///
/// Detection tries newest first: an older layout is a prefix of a newer one
/// in several cases, so trying oldest first would match a truncated read of
/// a newer record and silently drop its trailing fields.
#[cfg(test)]
pub(crate) const SUPPORTED_VALUE_VERSIONS: [u8; 7] = [7, 6, 5, 4, 3, 2, 1];

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredApiUserV1 {
    pub target: String,
    pub username: String,
    pub password: String,
    pub token: Option<String>,
    pub proxy: ProxyType,
    pub server: Option<String>,
    pub epg_timeshift: Option<String>,
    pub created_at: Option<i64>,
    pub exp_date: Option<i64>,
    pub max_connections: Option<u32>,
    pub status: Option<ProxyUserStatus>,
    pub ui_enabled: bool,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredApiUserV2 {
    pub target: String,
    pub username: String,
    pub password: String,
    pub token: Option<String>,
    pub proxy: ProxyType,
    pub server: Option<String>,
    pub epg_timeshift: Option<String>,
    pub epg_request_timeshift: Option<String>,
    pub created_at: Option<i64>,
    pub exp_date: Option<i64>,
    pub max_connections: Option<u32>,
    pub status: Option<ProxyUserStatus>,
    pub ui_enabled: bool,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredApiUserV3 {
    pub target: String,
    pub username: String,
    pub password: String,
    pub token: Option<String>,
    pub proxy: ProxyType,
    pub server: Option<String>,
    pub epg_timeshift: Option<String>,
    pub epg_request_timeshift: Option<String>,
    pub created_at: Option<i64>,
    pub exp_date: Option<i64>,
    pub max_connections: Option<u32>,
    pub status: Option<ProxyUserStatus>,
    pub ui_enabled: bool,
    pub comment: Option<String>,
    pub priority: Option<i8>,
}

impl StoredApiUserV3 {
    pub(crate) fn from_v2(v2: &StoredApiUserV2) -> Self {
        Self {
            target: v2.target.clone(),
            username: v2.username.clone(),
            password: v2.password.clone(),
            token: v2.token.clone(),
            proxy: v2.proxy,
            server: v2.server.clone(),
            epg_timeshift: v2.epg_timeshift.clone(),
            epg_request_timeshift: v2.epg_request_timeshift.clone(),
            created_at: v2.created_at,
            exp_date: v2.exp_date,
            max_connections: v2.max_connections,
            status: v2.status,
            ui_enabled: v2.ui_enabled,
            comment: v2.comment.clone(),
            priority: None,
        }
    }

    pub(crate) fn from_v1(v1: &StoredApiUserV1) -> Self {
        Self {
            target: v1.target.clone(),
            username: v1.username.clone(),
            password: v1.password.clone(),
            token: v1.token.clone(),
            proxy: v1.proxy,
            server: v1.server.clone(),
            epg_timeshift: v1.epg_timeshift.clone(),
            epg_request_timeshift: None,
            created_at: v1.created_at,
            exp_date: v1.exp_date,
            max_connections: v1.max_connections,
            status: v1.status,
            ui_enabled: v1.ui_enabled,
            comment: v1.comment.clone(),
            priority: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredApiUserV4 {
    pub target: String,
    pub username: String,
    pub password: String,
    pub token: Option<String>,
    pub proxy: ProxyType,
    pub server: Option<String>,
    pub epg_timeshift: Option<String>,
    pub epg_request_timeshift: Option<String>,
    pub created_at: Option<i64>,
    pub exp_date: Option<i64>,
    pub max_connections: Option<u32>,
    pub status: Option<ProxyUserStatus>,
    pub ui_enabled: bool,
    pub comment: Option<String>,
    pub priority: Option<i8>,
    pub soft_connections: Option<u16>,
    pub soft_priority: Option<i8>,
}

impl StoredApiUserV4 {
    pub(crate) fn from_v3(v3: &StoredApiUserV3) -> Self {
        Self {
            target: v3.target.clone(),
            username: v3.username.clone(),
            password: v3.password.clone(),
            token: v3.token.clone(),
            proxy: v3.proxy,
            server: v3.server.clone(),
            epg_timeshift: v3.epg_timeshift.clone(),
            epg_request_timeshift: v3.epg_request_timeshift.clone(),
            created_at: v3.created_at,
            exp_date: v3.exp_date,
            max_connections: v3.max_connections,
            status: v3.status,
            ui_enabled: v3.ui_enabled,
            comment: v3.comment.clone(),
            priority: v3.priority,
            soft_connections: None,
            soft_priority: None,
        }
    }

    pub(crate) fn from_v2(v2: &StoredApiUserV2) -> Self { Self::from_v3(&StoredApiUserV3::from_v2(v2)) }

    pub(crate) fn from_v1(v1: &StoredApiUserV1) -> Self { Self::from_v3(&StoredApiUserV3::from_v1(v1)) }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredApiUserV5 {
    pub target: String,
    pub username: String,
    pub password: String,
    pub token: Option<String>,
    pub proxy: ProxyType,
    pub server: Option<String>,
    pub epg_timeshift: Option<String>,
    pub epg_request_timeshift: Option<String>,
    pub created_at: Option<i64>,
    pub exp_date: Option<i64>,
    pub max_connections: Option<u32>,
    pub status: Option<ProxyUserStatus>,
    pub output_clusters: ClusterFlags,
    pub ui_enabled: bool,
    pub comment: Option<String>,
    pub priority: Option<i8>,
    pub soft_connections: Option<u16>,
    pub soft_priority: Option<i8>,
}

impl StoredApiUserV5 {
    pub(crate) fn from_v4(v4: &StoredApiUserV4) -> Self {
        Self {
            target: v4.target.clone(),
            username: v4.username.clone(),
            password: v4.password.clone(),
            token: v4.token.clone(),
            proxy: v4.proxy,
            server: v4.server.clone(),
            epg_timeshift: v4.epg_timeshift.clone(),
            epg_request_timeshift: v4.epg_request_timeshift.clone(),
            created_at: v4.created_at,
            exp_date: v4.exp_date,
            max_connections: v4.max_connections,
            status: v4.status,
            output_clusters: ClusterFlags::all(),
            ui_enabled: v4.ui_enabled,
            comment: v4.comment.clone(),
            priority: v4.priority,
            soft_connections: v4.soft_connections,
            soft_priority: v4.soft_priority,
        }
    }

    pub(crate) fn from_v3(v3: &StoredApiUserV3) -> Self { Self::from_v4(&StoredApiUserV4::from_v3(v3)) }

    pub(crate) fn from_v2(v2: &StoredApiUserV2) -> Self { Self::from_v4(&StoredApiUserV4::from_v2(v2)) }

    pub(crate) fn from_v1(v1: &StoredApiUserV1) -> Self { Self::from_v4(&StoredApiUserV4::from_v1(v1)) }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredApiUserV6 {
    pub target: String,
    pub username: String,
    pub password: String,
    pub token: Option<String>,
    pub proxy: ProxyType,
    pub server: Option<String>,
    pub epg_timeshift: Option<String>,
    pub epg_request_timeshift: Option<String>,
    pub created_at: Option<i64>,
    pub exp_date: Option<i64>,
    pub max_connections: Option<u32>,
    pub status: Option<ProxyUserStatus>,
    pub output_clusters: ClusterFlags,
    pub ui_enabled: bool,
    pub comment: Option<String>,
    pub priority: Option<i8>,
    pub soft_connections: Option<u16>,
    pub soft_priority: Option<i8>,
    pub network_access: Option<NetworkAccessDto>,
}

impl StoredApiUserV6 {
    pub(crate) fn from_v5(v5: &StoredApiUserV5) -> Self {
        Self {
            target: v5.target.clone(),
            username: v5.username.clone(),
            password: v5.password.clone(),
            token: v5.token.clone(),
            proxy: v5.proxy,
            server: v5.server.clone(),
            epg_timeshift: v5.epg_timeshift.clone(),
            epg_request_timeshift: v5.epg_request_timeshift.clone(),
            created_at: v5.created_at,
            exp_date: v5.exp_date,
            max_connections: v5.max_connections,
            status: v5.status,
            output_clusters: v5.output_clusters,
            ui_enabled: v5.ui_enabled,
            comment: v5.comment.clone(),
            priority: v5.priority,
            soft_connections: v5.soft_connections,
            soft_priority: v5.soft_priority,
            network_access: None,
        }
    }

    pub(crate) fn from_v4(v4: &StoredApiUserV4) -> Self { Self::from_v5(&StoredApiUserV5::from_v4(v4)) }

    pub(crate) fn from_v3(v3: &StoredApiUserV3) -> Self { Self::from_v5(&StoredApiUserV5::from_v3(v3)) }

    pub(crate) fn from_v2(v2: &StoredApiUserV2) -> Self { Self::from_v5(&StoredApiUserV5::from_v2(v2)) }

    pub(crate) fn from_v1(v1: &StoredApiUserV1) -> Self { Self::from_v5(&StoredApiUserV5::from_v1(v1)) }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredApiUserV7 {
    pub target: String,
    pub username: String,
    pub password: String,
    pub token: Option<String>,
    pub proxy: ProxyType,
    pub server: Option<String>,
    pub epg_timeshift: Option<String>,
    pub epg_request_timeshift: Option<String>,
    pub created_at: Option<i64>,
    pub exp_date: Option<i64>,
    pub max_connections: Option<u32>,
    pub status: Option<ProxyUserStatus>,
    pub output_clusters: ClusterFlags,
    pub ui_enabled: bool,
    pub comment: Option<String>,
    pub priority: Option<i8>,
    pub soft_connections: Option<u16>,
    pub soft_priority: Option<i8>,
    pub network_access: Option<NetworkAccessDto>,
    pub plan: Option<String>,
    pub filter: Option<String>,
}

impl StoredApiUserV7 {
    pub(crate) fn from_v6(v6: &StoredApiUserV6) -> Self {
        Self {
            target: v6.target.clone(),
            username: v6.username.clone(),
            password: v6.password.clone(),
            token: v6.token.clone(),
            proxy: v6.proxy,
            server: v6.server.clone(),
            epg_timeshift: v6.epg_timeshift.clone(),
            epg_request_timeshift: v6.epg_request_timeshift.clone(),
            created_at: v6.created_at,
            exp_date: v6.exp_date,
            max_connections: v6.max_connections,
            status: v6.status,
            output_clusters: v6.output_clusters,
            ui_enabled: v6.ui_enabled,
            comment: v6.comment.clone(),
            priority: v6.priority,
            soft_connections: v6.soft_connections,
            soft_priority: v6.soft_priority,
            network_access: v6.network_access.clone(),
            plan: None,
            filter: None,
        }
    }

    pub(crate) fn from_v5(v5: &StoredApiUserV5) -> Self { Self::from_v6(&StoredApiUserV6::from_v5(v5)) }

    pub(crate) fn from_v4(v4: &StoredApiUserV4) -> Self { Self::from_v6(&StoredApiUserV6::from_v4(v4)) }

    pub(crate) fn from_v3(v3: &StoredApiUserV3) -> Self { Self::from_v6(&StoredApiUserV6::from_v3(v3)) }

    pub(crate) fn from_v2(v2: &StoredApiUserV2) -> Self { Self::from_v6(&StoredApiUserV6::from_v2(v2)) }

    pub(crate) fn from_v1(v1: &StoredApiUserV1) -> Self { Self::from_v6(&StoredApiUserV6::from_v1(v1)) }
}

/// The normalized, field-named recovery record.
///
/// Every legacy layout converges here before anything is written. Unlike the
/// positional `StoredApiUserV*` types, this is encoded by name, so a later
/// change to the current credential shape is a migration step rather than a
/// reinterpretation of stored bytes.
///
/// Secrets live here because the record has to be able to recreate a working
/// user. They must never reach a log, a report, a diff or a fixture; the
/// fixtures use obviously fake values for that reason.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApiUserRecovery {
    pub target: String,
    pub username: String,
    pub password: String,
    pub token: Option<String>,
    pub proxy: ProxyType,
    pub server: Option<String>,
    pub epg_timeshift: Option<String>,
    pub epg_request_timeshift: Option<String>,
    pub created_at: Option<i64>,
    pub exp_date: Option<i64>,
    pub max_connections: Option<u32>,
    pub status: Option<ProxyUserStatus>,
    pub output_clusters: ClusterFlags,
    pub ui_enabled: bool,
    pub comment: Option<String>,
    pub priority: Option<i8>,
    pub soft_connections: Option<u16>,
    pub soft_priority: Option<i8>,
    pub network_access: Option<NetworkAccessDto>,
    pub plan: Option<String>,
    pub filter: Option<String>,
}

#[cfg(test)]
impl From<&StoredApiUserV7> for ApiUserRecovery {
    fn from(v7: &StoredApiUserV7) -> Self {
        Self {
            target: v7.target.clone(),
            username: v7.username.clone(),
            password: v7.password.clone(),
            token: v7.token.clone(),
            proxy: v7.proxy,
            server: v7.server.clone(),
            epg_timeshift: v7.epg_timeshift.clone(),
            epg_request_timeshift: v7.epg_request_timeshift.clone(),
            created_at: v7.created_at,
            exp_date: v7.exp_date,
            max_connections: v7.max_connections,
            status: v7.status,
            output_clusters: v7.output_clusters,
            ui_enabled: v7.ui_enabled,
            comment: v7.comment.clone(),
            priority: v7.priority,
            soft_connections: v7.soft_connections,
            soft_priority: v7.soft_priority,
            network_access: v7.network_access.clone(),
            plan: v7.plan.clone(),
            filter: v7.filter.clone(),
        }
    }
}

impl From<&StoredProxyUserCredentials> for ApiUserRecovery {
    fn from(stored: &StoredProxyUserCredentials) -> Self {
        Self {
            target: stored.target.clone(),
            username: stored.username.clone(),
            password: stored.password.clone(),
            token: stored.token.clone(),
            proxy: stored.proxy,
            server: stored.server.clone(),
            epg_timeshift: stored.epg_timeshift.clone(),
            epg_request_timeshift: stored.epg_request_timeshift.clone(),
            created_at: stored.created_at,
            exp_date: stored.exp_date,
            max_connections: stored.max_connections,
            status: stored.status,
            output_clusters: stored.output_clusters,
            ui_enabled: stored.ui_enabled,
            comment: stored.comment.clone(),
            priority: stored.priority,
            soft_connections: stored.soft_connections,
            soft_priority: stored.soft_priority,
            network_access: stored.network_access.clone(),
            plan: stored.plan.clone(),
            filter: stored.filter.clone(),
        }
    }
}

impl From<&ApiUserRecovery> for StoredProxyUserCredentials {
    fn from(recovery: &ApiUserRecovery) -> Self {
        Self {
            target: recovery.target.clone(),
            username: recovery.username.clone(),
            password: recovery.password.clone(),
            token: recovery.token.clone(),
            proxy: recovery.proxy,
            server: recovery.server.clone(),
            epg_timeshift: recovery.epg_timeshift.clone(),
            epg_request_timeshift: recovery.epg_request_timeshift.clone(),
            created_at: recovery.created_at,
            exp_date: recovery.exp_date,
            max_connections: recovery.max_connections,
            status: recovery.status,
            output_clusters: recovery.output_clusters,
            ui_enabled: recovery.ui_enabled,
            comment: recovery.comment.clone(),
            priority: recovery.priority,
            soft_connections: recovery.soft_connections,
            soft_priority: recovery.soft_priority,
            network_access: recovery.network_access.clone(),
            plan: recovery.plan.clone(),
            filter: recovery.filter.clone(),
        }
    }
}

/// Version 1 of the normalized User API recovery schema.
///
/// The legacy `V1`..`V7` numbering is a different axis entirely: those are
/// positional layouts that predate recovery. This version counts changes to
/// the *named* record, and starts at 1 no matter which legacy layout the
/// data arrived as.
pub(crate) struct ApiUserRecoverySchema;

impl RecoverySchema<String, StoredProxyUserCredentials> for ApiUserRecoverySchema {
    const NAME: &'static str = "api_user";
    const CURRENT_VERSION: u32 = 1;

    fn encode_key(&self, key: &String) -> io::Result<Value> { Ok(Value::String(key.clone())) }

    fn migrate_key_one(&self, from: u32, _key: Value) -> io::Result<Value> {
        Err(invalid(format!("no api user key migration from version {from}")))
    }

    fn decode_current_key(&self, key: Value) -> io::Result<String> {
        key.as_str().map(str::to_owned).ok_or_else(|| invalid("api user key is not a string"))
    }

    fn encode_current(&self, value: &StoredProxyUserCredentials) -> io::Result<Value> {
        serde_json::to_value(ApiUserRecovery::from(value)).map_err(|error| invalid(error.to_string()))
    }

    fn migrate_one(&self, from: u32, _value: Value) -> io::Result<Value> {
        Err(invalid(format!("no api user value migration from version {from}")))
    }

    fn decode_current(&self, value: Value) -> io::Result<StoredProxyUserCredentials> {
        let recovery: ApiUserRecovery = serde_json::from_value(value).map_err(|error| invalid(error.to_string()))?;
        Ok(StoredProxyUserCredentials::from(&recovery))
    }
}

fn invalid(message: impl Into<String>) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message.into()) }

type Journal = BPlusTreeRecoveryJournal<String, StoredProxyUserCredentials, ApiUserRecoverySchema>;

/// The User API database, with a recovery history beside it.
///
/// Every mutation goes through here rather than through `BPlusTree::store`,
/// so a change to the credential shape becomes a migration on restore
/// instead of an unreadable file.
pub(crate) struct ApiUserRepository {
    journal: Journal,
}

impl ApiUserRepository {
    pub(crate) fn open(db_path: &Path, recovery_root: &Path) -> io::Result<(Self, RecoveryOpenReport)> {
        let paths =
            RecoveryPaths { database: db_path.to_path_buf(), directory: recovery_root.join("api_user_recovery") };
        let (journal, report) = Journal::open(paths, ApiUserRecoverySchema, RecoveryPolicy::default())?;
        Ok((Self { journal }, report))
    }

    pub(crate) fn load(&mut self) -> io::Result<Vec<(String, StoredProxyUserCredentials)>> { self.journal.entries() }

    /// Replace the stored set in one batch.
    ///
    /// The whole set is rewritten rather than diffed: the callers above
    /// rebuild the user list from configuration, so a diff would have to
    /// reconstruct an intent the caller never expressed.
    pub(crate) fn commit(&mut self, users: Vec<(String, StoredProxyUserCredentials)>) -> io::Result<()> {
        let existing = self.journal.entries()?;
        let incoming: std::collections::BTreeSet<String> = users.iter().map(|(name, _)| name.clone()).collect();
        let mut operations = Vec::new();
        for (name, _) in existing {
            if !incoming.contains(&name) {
                operations.push(RecoveryOperation::Delete(name));
            }
        }
        for (name, user) in users {
            operations.push(RecoveryOperation::Upsert(name, user));
        }
        let _ = self.journal.apply_batch(RecoveryBatch::new(operations))?;
        let _ = self.journal.checkpoint_if_needed()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ApiUserRecovery, StoredApiUserV1, StoredApiUserV7, SUPPORTED_CONTAINER_VERSIONS, SUPPORTED_VALUE_VERSIONS,
    };
    use shared::model::{ClusterFlags, ProxyType};

    /// Obviously fake. A fixture that looks like a real credential invites
    /// someone to paste it somewhere real.
    fn v1() -> StoredApiUserV1 {
        StoredApiUserV1 {
            target: "target-a".to_string(),
            username: "fixture-user".to_string(),
            password: "not-a-real-password".to_string(),
            token: Some("not-a-real-token".to_string()),
            proxy: ProxyType::Reverse(None),
            server: Some("default".to_string()),
            epg_timeshift: Some("+1".to_string()),
            created_at: Some(1_700_000_000),
            exp_date: Some(1_800_000_000),
            max_connections: Some(3),
            status: None,
            ui_enabled: true,
            comment: Some("fixture".to_string()),
        }
    }

    #[test]
    fn the_supported_matrix_is_twenty_one_cases() {
        // Container version and value version are independent axes: any
        // container can hold any value layout, so the importer has to cover
        // the product, not the diagonal.
        assert_eq!(SUPPORTED_CONTAINER_VERSIONS.len() * SUPPORTED_VALUE_VERSIONS.len(), 21);
    }

    #[test]
    fn value_versions_are_tried_newest_first() {
        // Several older layouts are prefixes of newer ones. Trying oldest
        // first would match a newer record as an older one and silently drop
        // its trailing fields.
        let mut descending = SUPPORTED_VALUE_VERSIONS;
        descending.sort_unstable_by(|left, right| right.cmp(left));
        assert_eq!(SUPPORTED_VALUE_VERSIONS, descending);
    }

    #[test]
    fn the_oldest_layout_climbs_to_the_newest_without_losing_a_field() {
        let v1 = v1();
        let v7 = StoredApiUserV7::from_v1(&v1);

        assert_eq!(v7.target, v1.target);
        assert_eq!(v7.username, v1.username);
        assert_eq!(v7.password, v1.password);
        assert_eq!(v7.token, v1.token);
        assert_eq!(v7.epg_timeshift, v1.epg_timeshift);
        assert_eq!(v7.created_at, v1.created_at);
        assert_eq!(v7.exp_date, v1.exp_date);
        assert_eq!(v7.max_connections, v1.max_connections);
        assert_eq!(v7.ui_enabled, v1.ui_enabled);
        assert_eq!(v7.comment, v1.comment);
        // Fields V1 never had take the documented defaults rather than
        // anything derived from the user.
        assert_eq!(v7.output_clusters, ClusterFlags::all());
        assert_eq!(v7.plan, None);
        assert_eq!(v7.filter, None);
    }

    #[test]
    fn the_normalized_record_is_encoded_by_name() {
        // This is the whole reason it exists beside the positional layouts:
        // a later change to the credential shape must be a migration, not a
        // reinterpretation of bytes.
        let recovery = ApiUserRecovery::from(&StoredApiUserV7::from_v1(&v1()));
        let encoded = serde_json::to_string(&recovery).expect("serialize");

        for field in ["\"target\"", "\"username\"", "\"proxy\"", "\"output_clusters\"", "\"ui_enabled\""] {
            assert!(encoded.contains(field), "{field} is missing from {encoded}");
        }
    }

    #[test]
    fn the_normalized_record_round_trips() {
        let recovery = ApiUserRecovery::from(&StoredApiUserV7::from_v1(&v1()));
        let encoded = serde_json::to_string(&recovery).expect("serialize");
        let decoded: ApiUserRecovery = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, recovery);
    }
}
