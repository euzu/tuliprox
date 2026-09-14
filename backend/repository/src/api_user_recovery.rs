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

    #[cfg(test)]
    pub(crate) fn inject_fault(&mut self, point: Option<tuliprox_btree::RecoveryFaultPoint>) {
        self.journal.inject_fault(point);
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

/// The full container x value matrix.
///
/// Container version and value layout are independent, so reading a stored
/// user database means getting both right. These tests write each of the 21
/// combinations the way the version that produced it would have, then run the
/// real importer over it.
#[cfg(test)]
mod matrix {
    use super::{
        StoredApiUserV1, StoredApiUserV2, StoredApiUserV3, StoredApiUserV4, StoredApiUserV5, StoredApiUserV6,
        StoredApiUserV7, SUPPORTED_CONTAINER_VERSIONS, SUPPORTED_VALUE_VERSIONS,
    };
    use crate::{startup_migration, storage_const};
    use serde::Serialize;
    use shared::model::{ClusterFlags, ProxyType, ProxyUserStatus};
    use std::{
        fs::OpenOptions,
        io::{self, Seek, SeekFrom, Write},
        path::{Path, PathBuf},
    };
    use tempfile::tempdir;

    const FIXTURE_KEY: &str = "fixture-user";

    /// Obviously fake, and deliberately so: a fixture that looks like a real
    /// credential invites someone to paste it somewhere real.
    fn v1() -> StoredApiUserV1 {
        StoredApiUserV1 {
            target: "target-a".to_string(),
            username: FIXTURE_KEY.to_string(),
            password: "not-a-real-password".to_string(),
            token: Some("not-a-real-token".to_string()),
            proxy: ProxyType::Reverse(None),
            server: Some("default".to_string()),
            epg_timeshift: Some("+1".to_string()),
            created_at: Some(1_700_000_000),
            exp_date: Some(1_800_000_000),
            max_connections: Some(3),
            status: Some(ProxyUserStatus::Active),
            ui_enabled: true,
            comment: Some("fixture".to_string()),
        }
    }

    fn v2() -> StoredApiUserV2 {
        let v1 = v1();
        StoredApiUserV2 {
            target: v1.target,
            username: v1.username,
            password: v1.password,
            token: v1.token,
            proxy: v1.proxy,
            server: v1.server,
            epg_timeshift: v1.epg_timeshift,
            epg_request_timeshift: Some("+2".to_string()),
            created_at: v1.created_at,
            exp_date: v1.exp_date,
            max_connections: v1.max_connections,
            status: v1.status,
            ui_enabled: v1.ui_enabled,
            comment: v1.comment,
        }
    }

    /// Each layout carries a distinctive value in the field *it* introduced.
    ///
    /// Without this the fixtures would be indistinguishable: building every
    /// version from V2 leaves each newer field at `None`, so misreading a V6
    /// record as V5 would still satisfy every assertion.
    const FIXTURE_PRIORITY: i8 = 5;
    const FIXTURE_SOFT_CONNECTIONS: u16 = 7;
    const FIXTURE_SOFT_PRIORITY: i8 = -3;
    const FIXTURE_PLAN: &str = "fixture-plan";
    const FIXTURE_FILTER: &str = "fixture-filter";

    fn fixture_clusters() -> ClusterFlags { ClusterFlags::Live | ClusterFlags::Series }

    fn fixture_network_access() -> shared::model::NetworkAccessDto {
        shared::model::NetworkAccessDto { allowed_countries: Some(vec!["DE".to_string()]), allowed_networks: None }
    }

    fn v3() -> StoredApiUserV3 {
        StoredApiUserV3 { priority: Some(FIXTURE_PRIORITY), ..StoredApiUserV3::from_v2(&v2()) }
    }

    fn v4() -> StoredApiUserV4 {
        StoredApiUserV4 {
            soft_connections: Some(FIXTURE_SOFT_CONNECTIONS),
            soft_priority: Some(FIXTURE_SOFT_PRIORITY),
            ..StoredApiUserV4::from_v3(&v3())
        }
    }

    fn v5() -> StoredApiUserV5 {
        StoredApiUserV5 { output_clusters: fixture_clusters(), ..StoredApiUserV5::from_v4(&v4()) }
    }

    fn v6() -> StoredApiUserV6 {
        StoredApiUserV6 { network_access: Some(fixture_network_access()), ..StoredApiUserV6::from_v5(&v5()) }
    }

    fn v7() -> StoredApiUserV7 {
        StoredApiUserV7 {
            plan: Some(FIXTURE_PLAN.to_string()),
            filter: Some(FIXTURE_FILTER.to_string()),
            ..StoredApiUserV7::from_v6(&v6())
        }
    }

    /// Shared with the future-chain rehearsal, which needs a fully populated
    /// current record without a `Default` impl existing for one.
    pub(super) fn current_credentials() -> crate::user_repository::StoredProxyUserCredentials {
        crate::user_repository::StoredProxyUserCredentials::from(&super::ApiUserRecovery::from(&v7()))
    }

    /// Writes the database the way the code at `container_version` would have.
    ///
    /// Container 1 is produced by writing a v2 file and patching the version
    /// field, which is how the rest of this crate fabricates v1 fixtures: the
    /// v1 writer no longer exists.
    fn write_container<V>(path: &Path, container_version: u16, value: V) -> io::Result<()>
    where
        V: Serialize + for<'de> serde::Deserialize<'de> + Clone,
    {
        if container_version == 3 {
            let mut tree = crate::BPlusTree::<String, V>::new();
            tree.insert(FIXTURE_KEY.to_string(), value);
            let _ = tree.store(path)?;
            return Ok(());
        }
        let mut tree = crate::bplustree::v2::BPlusTree::<String, V>::new();
        tree.insert(FIXTURE_KEY.to_string(), value);
        let _ = tree.store(path)?;
        if container_version == 1 {
            let mut file = OpenOptions::new().write(true).open(path)?;
            file.seek(SeekFrom::Start(4))?;
            file.write_all(&u32::from(container_version).to_le_bytes())?;
            file.sync_all()?;
        }
        Ok(())
    }

    fn write_value_version(path: &Path, container_version: u16, value_version: u8) -> io::Result<()> {
        match value_version {
            1 => write_container(path, container_version, v1()),
            2 => write_container(path, container_version, v2()),
            3 => write_container(path, container_version, v3()),
            4 => write_container(path, container_version, v4()),
            5 => write_container(path, container_version, v5()),
            6 => write_container(path, container_version, v6()),
            7 => write_container(path, container_version, v7()),
            other => panic!("unsupported value version {other}"),
        }
    }

    struct Case {
        _dir: tempfile::TempDir,
        db_path: PathBuf,
        guard_path: PathBuf,
    }

    fn case(container_version: u16, value_version: u8) -> io::Result<Case> {
        let dir = tempdir()?;
        let db_path = dir.path().join(storage_const::API_USER_DB_FILE);
        let guard_path = startup_migration::user_db_merge_guard_path(dir.path());
        write_value_version(&db_path, container_version, value_version)?;
        Ok(Case { _dir: dir, db_path, guard_path })
    }

    /// Every one of the 21 combinations must end up readable as the current
    /// layout, with the fields that version actually carried preserved.
    #[test]
    fn api_user_recovery_matrix_every_container_and_value_version_imports() -> io::Result<()> {
        let mut failures = Vec::new();
        for container_version in SUPPORTED_CONTAINER_VERSIONS {
            for value_version in SUPPORTED_VALUE_VERSIONS {
                let case = case(container_version, value_version)?;
                let label = format!("container v{container_version} / value V{value_version}");

                if let Err(error) = startup_migration::migrate_user_db_schema(&case.db_path, &case.guard_path) {
                    failures.push(format!("{label}: import failed: {error}"));
                    continue;
                }
                let Ok(tree) = crate::BPlusTree::<String, StoredApiUserV7>::load(&case.db_path) else {
                    failures.push(format!("{label}: not readable as the current layout after import"));
                    continue;
                };
                let Some(user) = tree.query(&FIXTURE_KEY.to_string()) else {
                    failures.push(format!("{label}: the user disappeared during import"));
                    continue;
                };
                if user.username != FIXTURE_KEY {
                    failures.push(format!("{label}: username became {}", user.username));
                }
                if user.password != "not-a-real-password" {
                    failures.push(format!("{label}: password was not preserved"));
                }
                if user.max_connections != Some(3) {
                    failures.push(format!("{label}: max_connections became {:?}", user.max_connections));
                }
                // Each of these pins the layout that introduced the field. If
                // detection picks the wrong value version, the field it does
                // not know about reads back as its default and this catches it.
                let expected_timeshift = if value_version >= 2 { Some("+2") } else { None };
                if user.epg_request_timeshift.as_deref() != expected_timeshift {
                    failures.push(format!(
                        "{label}: epg_request_timeshift became {:?}, expected {expected_timeshift:?}",
                        user.epg_request_timeshift
                    ));
                }
                let expected_priority = (value_version >= 3).then_some(FIXTURE_PRIORITY);
                if user.priority != expected_priority {
                    failures
                        .push(format!("{label}: priority became {:?}, expected {expected_priority:?}", user.priority));
                }
                let expected_soft = (value_version >= 4).then_some(FIXTURE_SOFT_CONNECTIONS);
                if user.soft_connections != expected_soft {
                    failures.push(format!(
                        "{label}: soft_connections became {:?}, expected {expected_soft:?}",
                        user.soft_connections
                    ));
                }
                let expected_soft_priority = (value_version >= 4).then_some(FIXTURE_SOFT_PRIORITY);
                if user.soft_priority != expected_soft_priority {
                    failures.push(format!(
                        "{label}: soft_priority became {:?}, expected {expected_soft_priority:?}",
                        user.soft_priority
                    ));
                }
                // Layouts before V5 had no cluster field, and the documented
                // default for them is every cluster.
                let expected_clusters = if value_version >= 5 { fixture_clusters() } else { ClusterFlags::all() };
                if user.output_clusters != expected_clusters {
                    failures.push(format!(
                        "{label}: output_clusters became {:?}, expected {expected_clusters:?}",
                        user.output_clusters
                    ));
                }
                let expected_network = (value_version >= 6).then(fixture_network_access);
                if user.network_access != expected_network {
                    failures.push(format!(
                        "{label}: network_access became {:?}, expected {expected_network:?}",
                        user.network_access
                    ));
                }
                let expected_plan = (value_version >= 7).then(|| FIXTURE_PLAN.to_string());
                if user.plan != expected_plan {
                    failures.push(format!("{label}: plan became {:?}, expected {expected_plan:?}", user.plan));
                }
                let expected_filter = (value_version >= 7).then(|| FIXTURE_FILTER.to_string());
                if user.filter != expected_filter {
                    failures.push(format!("{label}: filter became {:?}, expected {expected_filter:?}", user.filter));
                }
            }
        }
        assert!(failures.is_empty(), "{} of 21 matrix cases failed:\n{}", failures.len(), failures.join("\n"));
        Ok(())
    }

    /// A file that is not a B+Tree at all must be refused, not silently
    /// replaced with an empty database.
    #[test]
    fn api_user_recovery_matrix_a_corrupt_database_is_refused() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join(storage_const::API_USER_DB_FILE);
        let guard_path = startup_migration::user_db_merge_guard_path(dir.path());
        std::fs::write(&db_path, b"this is not a b+tree")?;

        assert!(
            startup_migration::migrate_user_db_schema(&db_path, &guard_path).is_err(),
            "a corrupt user database must be refused"
        );
        assert!(!guard_path.exists(), "a refused import must not leave a merge guard behind");
        Ok(())
    }

    /// A container version from the future is refused rather than guessed at.
    #[test]
    fn api_user_recovery_matrix_a_future_container_version_is_refused() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join(storage_const::API_USER_DB_FILE);
        let guard_path = startup_migration::user_db_merge_guard_path(dir.path());
        write_value_version(&db_path, 3, 7)?;
        // Stamp a container version this build has never heard of.
        let mut file = OpenOptions::new().write(true).open(&db_path)?;
        file.seek(SeekFrom::Start(4))?;
        file.write_all(&99u32.to_le_bytes())?;
        file.sync_all()?;
        drop(file);

        assert!(
            startup_migration::migrate_user_db_schema(&db_path, &guard_path).is_err(),
            "a future container version must be refused"
        );
        assert!(!guard_path.exists(), "a refused import must not leave a merge guard behind");
        Ok(())
    }

    /// A refused import must leave the file exactly as it found it.
    #[test]
    fn api_user_recovery_matrix_a_refused_import_does_not_touch_the_file() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join(storage_const::API_USER_DB_FILE);
        let guard_path = startup_migration::user_db_merge_guard_path(dir.path());
        write_value_version(&db_path, 3, 7)?;
        let mut file = OpenOptions::new().write(true).open(&db_path)?;
        file.seek(SeekFrom::Start(4))?;
        file.write_all(&99u32.to_le_bytes())?;
        file.sync_all()?;
        drop(file);
        let before = std::fs::read(&db_path)?;

        let _ = startup_migration::migrate_user_db_schema(&db_path, &guard_path);

        assert_eq!(std::fs::read(&db_path)?, before, "a refused import rewrote the database");
        Ok(())
    }

    /// A missing database is not an error: it is a first run.
    #[test]
    fn api_user_recovery_matrix_a_missing_database_is_not_a_failure() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join(storage_const::API_USER_DB_FILE);
        let guard_path = startup_migration::user_db_merge_guard_path(dir.path());

        assert!(!startup_migration::migrate_user_db_schema(&db_path, &guard_path)?);
        assert!(!guard_path.exists());
        Ok(())
    }

    /// The fixtures must not carry anything that looks like a real secret.
    #[test]
    fn api_user_recovery_matrix_fixtures_use_obviously_fake_credentials() {
        let user = v1();
        assert!(user.password.starts_with("not-a-real-"), "fixture password must be obviously fake");
        assert!(
            user.token.as_deref().is_some_and(|token| token.starts_with("not-a-real-")),
            "fixture token must be obviously fake"
        );
    }

    /// Guards the import order the matrix depends on: `ClusterFlags::all()` is
    /// the documented default for layouts that predate output clusters.
    #[test]
    fn api_user_recovery_matrix_layouts_without_clusters_default_to_all() {
        assert_eq!(StoredApiUserV7::from_v2(&v2()).output_clusters, ClusterFlags::all());
    }

    /// Legacy data reaches normalized recovery before anything can mutate it.
    ///
    /// Not because the importer exports on its way past, but structurally:
    /// every mutation path opens the repository first, and opening a database
    /// that has no history adopts it. So the normalized generation exists
    /// before the first write, whatever that write turns out to be.
    #[test]
    fn api_user_recovery_matrix_legacy_data_is_normalized_before_any_mutation() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join(storage_const::API_USER_DB_FILE);
        let guard_path = startup_migration::user_db_merge_guard_path(dir.path());
        // A legacy container holding a layout four versions behind current.
        write_value_version(&db_path, 2, 3)?;
        assert!(startup_migration::migrate_user_db_schema(&db_path, &guard_path)?);

        let (mut repository, report) = super::ApiUserRepository::open(&db_path, dir.path())?;
        assert_eq!(
            report.action,
            tuliprox_btree::RecoveryOpenAction::Adopted,
            "the first open of a migrated database must take it under management"
        );
        assert!(dir.path().join("api_user_recovery").is_dir(), "the generation must be on disk after open");

        let users = repository.load()?;
        assert_eq!(users.len(), 1, "the migrated user must be present in recovery");
        let (key, user) = &users[0];
        assert_eq!(key, FIXTURE_KEY);
        assert_eq!(user.username, FIXTURE_KEY);
        assert_eq!(user.password, "not-a-real-password");
        assert_eq!(user.priority, Some(FIXTURE_PRIORITY));
        assert_eq!(user.epg_request_timeshift.as_deref(), Some("+2"));
        Ok(())
    }

    /// The normalized record survives a restart, which is what makes it a
    /// migration target rather than a cache.
    #[test]
    fn api_user_recovery_matrix_the_normalized_generation_survives_reopen() -> io::Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join(storage_const::API_USER_DB_FILE);
        let guard_path = startup_migration::user_db_merge_guard_path(dir.path());
        write_value_version(&db_path, 2, 3)?;
        assert!(startup_migration::migrate_user_db_schema(&db_path, &guard_path)?);

        let (_, first) = super::ApiUserRepository::open(&db_path, dir.path())?;
        assert_eq!(first.action, tuliprox_btree::RecoveryOpenAction::Adopted);

        let (mut repository, second) = super::ApiUserRepository::open(&db_path, dir.path())?;
        assert_eq!(
            second.action,
            tuliprox_btree::RecoveryOpenAction::Opened,
            "adoption happens once; later opens must find a tracked database"
        );
        assert_eq!(repository.load()?.len(), 1);
        Ok(())
    }

    /// Nothing an operator or a log can see may carry a credential.
    ///
    /// Counts, revisions and schema versions are fine; the secret itself is
    /// not. The fixture password is a marker string so a leak anywhere in a
    /// rendered report or error is unambiguous.
    #[test]
    fn api_user_recovery_matrix_reports_and_errors_carry_no_secrets() -> io::Result<()> {
        const MARKER_PASSWORD: &str = "LEAK-MARKER-PASSWORD";
        const MARKER_TOKEN: &str = "LEAK-MARKER-TOKEN";

        let dir = tempdir()?;
        let db_path = dir.path().join(storage_const::API_USER_DB_FILE);
        let mut value = v7();
        value.password = MARKER_PASSWORD.to_string();
        value.token = Some(MARKER_TOKEN.to_string());
        write_container(&db_path, 3, value)?;

        let (mut repository, report) = super::ApiUserRepository::open(&db_path, dir.path())?;
        let rendered_report = format!("{report:?}");

        // The record itself must still round-trip, or this test would pass by
        // simply having lost the data.
        let users = repository.load()?;
        assert_eq!(users[0].1.password, MARKER_PASSWORD, "the credential must survive the round trip");

        assert!(!rendered_report.contains(MARKER_PASSWORD), "a password reached a report: {rendered_report}");
        assert!(!rendered_report.contains(MARKER_TOKEN), "a token reached a report: {rendered_report}");

        // A refused open is the other surface an operator sees.
        let corrupt_dir = tempdir()?;
        let corrupt_db = corrupt_dir.path().join(storage_const::API_USER_DB_FILE);
        std::fs::write(&corrupt_db, format!("not a b+tree {MARKER_PASSWORD}"))?;
        if let Err(error) = super::ApiUserRepository::open(&corrupt_db, corrupt_dir.path()) {
            let rendered = format!("{error}");
            assert!(!rendered.contains(MARKER_PASSWORD), "a password reached an error: {rendered}");
        }
        Ok(())
    }
}

/// A rehearsal for the change this module exists to survive.
///
/// The normalized record is field-named so that a future change to the
/// credential shape is a migration rather than a reinterpretation of bytes.
/// Nothing proves that until a second version exists, so these simulate one:
/// schemas at version 2 and 3 that rename and add fields, run against
/// generations written at version 1 and 2.
///
/// This axis is unrelated to the legacy positional V1-V7 numbering.
#[cfg(test)]
mod future_chain {
    use super::{ApiUserRecoverySchema, ApiUserRepository};
    use crate::user_repository::StoredProxyUserCredentials;
    use serde_json::Value;
    use std::{io, path::Path};
    use tempfile::tempdir;
    use tuliprox_btree::{
        BPlusTreeRecoveryJournal, RecoveryBatch, RecoveryOperation, RecoveryPaths, RecoveryPolicy, RecoverySchema,
    };

    const USERNAME: &str = "fixture-user";
    const PASSWORD: &str = "not-a-real-password";

    fn invalid(message: impl Into<String>) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message.into()) }

    /// What the record is expected to look like once it reaches version 3.
    #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    struct FutureApiUser {
        username: String,
        password: String,
        /// Renamed from `plan` by the 2 -> 3 step.
        plan_id: Option<String>,
        /// Introduced by the 1 -> 2 step.
        locale: Option<String>,
    }

    /// What a build at version 2 would have had: `plan` not yet renamed.
    ///
    /// Using the version 3 type here instead would decode a version 2 record
    /// into a field that does not exist yet, silently dropping `plan` - which
    /// is exactly the failure this rehearsal is meant to catch.
    #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    struct FutureApiUserV2 {
        username: String,
        password: String,
        plan: Option<String>,
        locale: Option<String>,
    }

    /// The 1 -> 2 step: a new field appears with a default.
    fn step_one_to_two(mut value: Value) -> io::Result<Value> {
        let object = value.as_object_mut().ok_or_else(|| invalid("recovery record is not an object"))?;
        object.entry("locale").or_insert(Value::Null);
        Ok(value)
    }

    /// The 2 -> 3 step: a field is renamed, which is exactly what positional
    /// encoding could never survive.
    fn step_two_to_three(mut value: Value) -> io::Result<Value> {
        let object = value.as_object_mut().ok_or_else(|| invalid("recovery record is not an object"))?;
        let plan = object.remove("plan").unwrap_or(Value::Null);
        object.insert("plan_id".to_string(), plan);
        Ok(value)
    }

    /// Shares `NAME` with the real schema so it reads the same generations.
    struct SchemaV2;

    impl RecoverySchema<String, FutureApiUserV2> for SchemaV2 {
        const NAME: &'static str = "api_user";
        const CURRENT_VERSION: u32 = 2;

        fn encode_key(&self, key: &String) -> io::Result<Value> { Ok(Value::String(key.clone())) }

        fn migrate_key_one(&self, _from: u32, key: Value) -> io::Result<Value> { Ok(key) }

        fn decode_current_key(&self, key: Value) -> io::Result<String> {
            key.as_str().map(str::to_owned).ok_or_else(|| invalid("key is not a string"))
        }

        fn encode_current(&self, value: &FutureApiUserV2) -> io::Result<Value> {
            serde_json::to_value(value).map_err(|error| invalid(error.to_string()))
        }

        fn migrate_one(&self, from: u32, value: Value) -> io::Result<Value> {
            match from {
                1 => step_one_to_two(value),
                other => Err(invalid(format!("no api user migration from version {other}"))),
            }
        }

        fn decode_current(&self, value: Value) -> io::Result<FutureApiUserV2> {
            serde_json::from_value(value).map_err(|error| invalid(error.to_string()))
        }
    }

    struct SchemaV3;

    impl RecoverySchema<String, FutureApiUser> for SchemaV3 {
        const NAME: &'static str = "api_user";
        const CURRENT_VERSION: u32 = 3;

        fn encode_key(&self, key: &String) -> io::Result<Value> { Ok(Value::String(key.clone())) }

        fn migrate_key_one(&self, _from: u32, key: Value) -> io::Result<Value> { Ok(key) }

        fn decode_current_key(&self, key: Value) -> io::Result<String> {
            key.as_str().map(str::to_owned).ok_or_else(|| invalid("key is not a string"))
        }

        fn encode_current(&self, value: &FutureApiUser) -> io::Result<Value> {
            serde_json::to_value(value).map_err(|error| invalid(error.to_string()))
        }

        fn migrate_one(&self, from: u32, value: Value) -> io::Result<Value> {
            match from {
                1 => step_one_to_two(value),
                2 => step_two_to_three(value),
                other => Err(invalid(format!("no api user migration from version {other}"))),
            }
        }

        fn decode_current(&self, value: Value) -> io::Result<FutureApiUser> {
            serde_json::from_value(value).map_err(|error| invalid(error.to_string()))
        }
    }

    type JournalV2 = BPlusTreeRecoveryJournal<String, FutureApiUserV2, SchemaV2>;
    type JournalV3 = BPlusTreeRecoveryJournal<String, FutureApiUser, SchemaV3>;

    fn paths(root: &Path) -> RecoveryPaths {
        RecoveryPaths { database: root.join("api_user.db"), directory: root.join("api_user_recovery") }
    }

    fn a_user() -> StoredProxyUserCredentials {
        let mut user = super::matrix::current_credentials();
        user.username = USERNAME.to_string();
        user.password = PASSWORD.to_string();
        user.plan = Some("fixture-plan".to_string());
        user
    }

    /// Writes a version 1 generation using the real schema.
    fn seed_version_one(root: &Path) -> io::Result<()> {
        let (mut repository, _) = ApiUserRepository::open(&paths(root).database, root)?;
        repository.commit(vec![(USERNAME.to_string(), a_user())])
    }

    /// The whole chain: a generation written at 1, read by a build at 3.
    #[test]
    fn recovery_v1_migrates_through_v2_to_v3() -> io::Result<()> {
        let dir = tempdir()?;
        seed_version_one(dir.path())?;

        let (mut journal, _) = JournalV3::open(paths(dir.path()), SchemaV3, RecoveryPolicy::default())?;
        let entries = journal.entries()?;

        assert_eq!(entries.len(), 1);
        let (key, user) = &entries[0];
        assert_eq!(key, USERNAME);
        assert_eq!(
            user,
            &FutureApiUser {
                username: USERNAME.to_string(),
                password: PASSWORD.to_string(),
                // Carried across the rename rather than lost.
                plan_id: Some("fixture-plan".to_string()),
                locale: None,
            }
        );
        Ok(())
    }

    /// The shorter hop, which must not depend on having gone through 1.
    #[test]
    fn recovery_v2_migrates_to_v3() -> io::Result<()> {
        let dir = tempdir()?;
        seed_version_one(dir.path())?;

        // A build at version 2 reads the version 1 generation and writes it
        // forward, leaving a genuine version 2 generation behind.
        {
            let (mut journal, _) = JournalV2::open(paths(dir.path()), SchemaV2, RecoveryPolicy::default())?;
            let entries = journal.entries()?;
            assert_eq!(entries[0].1.plan, Some("fixture-plan".to_string()), "version 2 still calls it `plan`");
            let operations =
                entries.into_iter().map(|(key, value)| RecoveryOperation::Upsert(key, value)).collect::<Vec<_>>();
            let _ = journal.apply_batch(RecoveryBatch::new(operations))?;
        }

        let (mut journal, _) = JournalV3::open(paths(dir.path()), SchemaV3, RecoveryPolicy::default())?;
        let entries = journal.entries()?;

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1.plan_id, Some("fixture-plan".to_string()));
        assert_eq!(entries[0].1.password, PASSWORD);
        Ok(())
    }

    /// A generation from a version this build has never heard of is refused,
    /// not guessed at.
    #[test]
    fn a_generation_from_the_future_is_refused() -> io::Result<()> {
        let dir = tempdir()?;
        seed_version_one(dir.path())?;
        {
            let (mut journal, _) = JournalV3::open(paths(dir.path()), SchemaV3, RecoveryPolicy::default())?;
            let entries = journal.entries()?;
            let operations =
                entries.into_iter().map(|(key, value)| RecoveryOperation::Upsert(key, value)).collect::<Vec<_>>();
            let _ = journal.apply_batch(RecoveryBatch::new(operations))?;
        }

        // The original schema is at version 1 and must refuse a version 3
        // generation rather than reinterpret it.
        let result = BPlusTreeRecoveryJournal::<String, StoredProxyUserCredentials, ApiUserRecoverySchema>::open(
            paths(dir.path()),
            ApiUserRecoverySchema,
            RecoveryPolicy::default(),
        );
        assert!(result.is_err(), "a generation from a future schema must be refused");
        Ok(())
    }
}

/// What a half-finished adoption leaves behind.
///
/// Adopting the User API database is a staged operation, and every stage can
/// fail. The requirement is the same at each one: whatever was already stored
/// must still be there afterwards. A failure that loses the previous contents
/// is worse than no adoption at all.
#[cfg(test)]
mod staged_adoption {
    use super::ApiUserRepository;
    use crate::user_repository::StoredProxyUserCredentials;
    use std::{io, path::Path};
    use tempfile::tempdir;
    use tuliprox_btree::RecoveryFaultPoint;

    fn db_path(root: &Path) -> std::path::PathBuf { root.join("api_user.db") }

    fn user(username: &str) -> (String, StoredProxyUserCredentials) {
        let mut stored = super::matrix::current_credentials();
        stored.username = username.to_string();
        (username.to_string(), stored)
    }

    fn seeded(root: &Path) -> io::Result<()> {
        let (mut repository, _) = ApiUserRepository::open(&db_path(root), root)?;
        repository.commit(vec![user("first")])
    }

    /// Every fault point must leave a coherent stored set behind.
    ///
    /// Not "the write never happened": once a journal record reaches the file
    /// it may legitimately replay on reopen, so some of these fault points
    /// leave the new user in place. The invariant that matters is that the
    /// result is always exactly one of the two states, never empty and never
    /// a mixture.
    #[test]
    fn a_failed_commit_always_leaves_a_coherent_set() -> io::Result<()> {
        let faults = [
            RecoveryFaultPoint::BeforeJournalAppend,
            RecoveryFaultPoint::DuringJournalAppend,
            RecoveryFaultPoint::BeforeJournalSync,
            RecoveryFaultPoint::DuringDatabaseBatch,
            RecoveryFaultPoint::DuringDatabaseCommit,
        ];

        for fault in faults {
            let dir = tempdir()?;
            seeded(dir.path())?;

            {
                let (mut repository, _) = ApiUserRepository::open(&db_path(dir.path()), dir.path())?;
                repository.inject_fault(Some(fault));
                assert!(
                    repository.commit(vec![user("second")]).is_err(),
                    "{fault:?}: the commit must fail with a fault injected"
                );
            }

            // Reopening is the operator's next move after a crash.
            let (mut repository, _) = ApiUserRepository::open(&db_path(dir.path()), dir.path())?;
            let users = repository.load()?;
            assert_eq!(users.len(), 1, "{fault:?}: the stored set must hold exactly one user, got {users:?}");
            assert!(
                users[0].0 == "first" || users[0].0 == "second",
                "{fault:?}: the stored user is neither of the two coherent states: {}",
                users[0].0
            );
        }
        Ok(())
    }

    /// The two points before anything durable is written must not apply.
    #[test]
    fn a_fault_before_the_journal_is_written_does_not_apply() -> io::Result<()> {
        for fault in [RecoveryFaultPoint::BeforeJournalAppend, RecoveryFaultPoint::DuringJournalAppend] {
            let dir = tempdir()?;
            seeded(dir.path())?;

            {
                let (mut repository, _) = ApiUserRepository::open(&db_path(dir.path()), dir.path())?;
                repository.inject_fault(Some(fault));
                assert!(repository.commit(vec![user("second")]).is_err());
            }

            let (mut repository, _) = ApiUserRepository::open(&db_path(dir.path()), dir.path())?;
            let users = repository.load()?;
            assert_eq!(users[0].0, "first", "{fault:?}: a write that never reached the journal was applied");
        }
        Ok(())
    }

    /// A fault after the journal record is durable is a committed write, so
    /// reopening must show it rather than lose it.
    #[test]
    fn a_fault_after_the_journal_is_durable_still_commits() -> io::Result<()> {
        let dir = tempdir()?;
        seeded(dir.path())?;

        {
            let (mut repository, _) = ApiUserRepository::open(&db_path(dir.path()), dir.path())?;
            repository.inject_fault(Some(RecoveryFaultPoint::AfterJournalSync));
            let _ = repository.commit(vec![user("second")]);
        }

        let (mut repository, _) = ApiUserRepository::open(&db_path(dir.path()), dir.path())?;
        let users = repository.load()?;
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].0, "second", "a durable journal record must survive the reopen");
        Ok(())
    }

    /// A repository that failed mid-write refuses further writes until it is
    /// reopened, rather than continuing from an uncertain state.
    #[test]
    fn a_failed_repository_refuses_further_writes_until_reopened() -> io::Result<()> {
        let dir = tempdir()?;
        seeded(dir.path())?;

        let (mut repository, _) = ApiUserRepository::open(&db_path(dir.path()), dir.path())?;
        repository.inject_fault(Some(RecoveryFaultPoint::DuringJournalAppend));
        assert!(repository.commit(vec![user("second")]).is_err());

        repository.inject_fault(None);
        assert!(
            repository.commit(vec![user("third")]).is_err(),
            "a repository that failed mid-write must not accept another write"
        );
        Ok(())
    }
}
