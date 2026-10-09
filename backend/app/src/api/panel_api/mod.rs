//! Provisioning accounts against an upstream Xtream panel.
//!
//! This stays in `api`, and the reason is a cycle rather than `AppState`. It
//! reads only two fields off the root state - the configuration and the HTTP
//! client - so the state is not what pins it. What pins it is
//! `api::model::streams`:
//!
//! - this module calls `provider_stream::create_panel_api_provisioning_stream_with_stop`
//!   and `create_provider_connections_exhausted_stream` to answer an exhausted
//!   request with a provisioning clip;
//! - `provider_stream` and `active_client_stream` call back here for
//!   `can_provision_on_exhausted`, `run_panel_api_provisioning_probe` and
//!   `find_input_by_provider_name`.
//!
//! So provisioning and streaming are one mutually recursive cluster, roughly
//! 11.5k lines, and neither half moves without the other. Extracting them would
//! mean moving both together into a package that also depends on `hls`,
//! `session`, `iptv`, `repository` and `config-loader` - and `ConfigFile`, which
//! mutates the root state directly, would have to move too. That is a
//! deliberate not-yet, not an oversight.

use shared::create_bitset;

const PANEL_API_REQUEST_TIMEOUT_SECS: u64 = 30;

const PANEL_API_RETRY_ATTEMPTS: usize = 2;

const PANEL_API_DEFAULT_RETRY_AFTER_SECS: u64 = 1;

const PANEL_API_MAX_RETRY_AFTER_SECS: u64 = 5;

create_bitset!(u8, PanelApiOptionalFlags, AccountInfo, ClientNew, ClientRenew, AdultContent);

#[derive(Debug, Clone)]
pub(crate) enum PanelApiProvisionOutcome {
    Renewed,
    Created,
}

#[cfg(test)]
mod tests;

mod account;
mod alias_pool;
mod params;
mod probe;
mod provisioning;
mod sync;
mod time;
mod transport;

#[cfg(test)]
use self::account::build_user_api_account_info_input_source;
pub use self::provisioning::{create_panel_api_provisioning_stream_details, try_provision_account_on_exhausted};
#[cfg(test)]
use self::transport::panel_api_retry_after_from_header_value;
#[cfg(test)]
use self::transport::panel_api_retryable_status;
use self::{
    account::{
        fetch_root_user_api_info, panel_account_info, panel_client_adult_content, panel_client_info,
        panel_client_info_raw, panel_client_new, panel_client_renew,
    },
    alias_pool::{
        alias_pool_has_min, alias_pool_limit_values, alias_pool_remove_expired, collect_accounts,
        compare_alias_exp_date_config, count_enabled_proxy_users, count_valid_accounts_at, ensure_alias_pool_min,
        extract_account_creds_from_input, resolve_alias_pool_min, root_counts_towards_pool,
        root_counts_towards_pool_at, sort_account_aliases_keep_root_first,
    },
    params::{
        build_panel_url, extract_stringish, extract_username_password_from_json, first_json_object, parse_boolish,
        require_batch_alias_path, resolve_query_params, validate_account_info_params,
        validate_client_adult_content_params, validate_client_info_params, validate_client_new_params,
        validate_client_renew_params, validate_panel_api_config, AccountCredentials,
    },
    probe::{
        build_panel_api_probe_targets, build_player_api_action_url, format_probe_target_actions,
        provisioning_method_to_reqwest, PanelApiProbeTarget,
    },
    provisioning::{
        aliases_need_sort_config, is_date_only_yyyy_mm_dd, parse_panel_api_provisioning_offset_secs,
        resolve_panel_api_optional_flags,
    },
    sync::{
        refresh_internal_sources, resolve_batch_alias_path, should_reload_sources_after_internal_write,
        sync_panel_api_for_input_on_boot,
    },
    time::{
        apply_clock_skew, is_expiring_with_offset_at, load_panel_api_time_cache, normalize_panel_expire,
        panel_api_time_cache_path, parse_cached_tz, persist_panel_api_time_cache, resolve_panel_expire_mode,
        PanelApiTimeCacheEntry, PanelApiTimeContext,
    },
    transport::{panel_get_json, user_api_get_json},
};
pub(crate) use self::{
    alias_pool::{
        is_alias_pool_max_reached, sync_panel_api_alias_pool_for_target, sync_panel_api_exp_dates,
        sync_panel_api_exp_dates_on_boot, target_has_alias_pool_min,
    },
    probe::run_panel_api_provisioning_probe,
    provisioning::{can_provision_on_exhausted, find_input_by_provider_name, wait_for_panel_api_account_ready},
};
