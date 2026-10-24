//! Network-access checks and the `GeoIp` database update: `AppState` pairs the configuration with its `GeoIp` store,
//! so callers do not thread `app_config` and `geoip` through every call.

use super::AppState;
use crate::{
    auth::{
        check_network_access_only, check_permission_and_network_access_only, resolve_api_user_context,
        try_check_network_access_only, ApiUserAuthError, ApiUserContext, Fingerprint,
    },
    model::{ConfigTarget, ProxyUserCredentials},
    processing::geoip::{update_geoip_db, GeoIpUpdateError},
};
use std::sync::Arc;

impl AppState {
    /// Denies a user whose client address the network rules reject; an unavailable `GeoIp` database follows the
    /// configured policy.
    pub fn check_network_access(
        &self,
        user: &Arc<ProxyUserCredentials>,
        fingerprint: &Fingerprint,
    ) -> Result<(), ApiUserAuthError> {
        check_network_access_only(user, fingerprint, &self.app_config, &self.geoip)
    }

    /// Like [`AppState::check_network_access`], without logging an allowed request on an unavailable database.
    pub fn try_check_network_access(
        &self,
        user: &Arc<ProxyUserCredentials>,
        fingerprint: &Fingerprint,
    ) -> Result<(), ApiUserAuthError> {
        try_check_network_access_only(user, fingerprint, &self.app_config, &self.geoip)
    }

    /// The user permission check followed by [`AppState::check_network_access`].
    pub fn check_permission_and_network_access(
        &self,
        user: &Arc<ProxyUserCredentials>,
        fingerprint: &Fingerprint,
    ) -> Result<(), ApiUserAuthError> {
        check_permission_and_network_access_only(user, fingerprint, &self.app_config, &self.geoip)
    }

    /// Permission and network checks for a user and its target, yielding the request context.
    pub fn resolve_api_user_context(
        &self,
        user: Arc<ProxyUserCredentials>,
        target: Arc<ConfigTarget>,
        fingerprint: Fingerprint,
    ) -> Result<ApiUserContext, ApiUserAuthError> {
        resolve_api_user_context(user, target, fingerprint, &self.app_config, &self.geoip)
    }

    /// Downloads the `GeoIp` database and swaps it into the store the network checks read.
    pub async fn update_geoip_db(&self) -> Result<(), GeoIpUpdateError> {
        update_geoip_db(&self.app_config, &self.http_clients.default.load(), &self.geoip).await
    }
}
