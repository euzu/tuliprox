//! The wasm plugin host.
//!
//! [`PluginSystem`] scans `<plugins dir>/<id>/` (manifest + `plugin.wasm`),
//! keeps every plugin's lifecycle state in a hot-reloaded registry, runs guests
//! under an epoch deadline, a memory limit and a global memory budget, gives
//! them the `host` functions and outbound HTTP through the dedicated plugin
//! client, and implements [`shared::model::PluginHost`], the interface the playlist
//! pipeline is generic over.
//!
//! The contract a plugin is written against lives in `tuliprox-plugin-api`.
#![recursion_limit = "256"]

mod activation;
mod api;
mod boot_update;
mod convert;
mod convert_epg;
mod dispatch;
mod epg_hooks;
mod events;
mod filter_batch;
mod header_inject;
mod host_fns;
mod http;
mod http_routes;
mod image_cache;
mod input_provider;
mod install;
mod kv;
mod logs;
mod managed_channels;
mod manifest;
mod marketplace;
mod mcp_tools;
mod metrics;
mod migration;
mod notify;
mod output_provider;
mod process;
mod registry;
mod rpc;
mod runtime;
mod settings;
mod state;
mod strategy;
mod system;
mod tasks;
mod ui_assets;

pub use activation::{
    crash_knob, inject_test_crash, recover_activations, ActivationKind, ActivationStage, RecoveredActivation,
    RecoveryOutcome, ENV_PLUGIN_TEST_CRASH_AT,
};
pub use boot_update::{boot_report_path, read_boot_report, record_recovered, run_boot_update};
pub use http_routes::{HostAuthorization, HttpRouteRequest, HttpRouteResponse};
pub use install::{ActivatedUpdate, InstallGuard, InstallRejection, MarketplaceExpect};
pub use marketplace::{
    browse, fetch_package, has_catalog, probe_source_name, remove_cache, resolve_release, source_info, source_warnings,
    sync_source, Blocker, Eligibility, FetchPolicy, MarketplaceError, ReleaseLookup, ResolvedRelease, UpdateCandidate,
    CATALOG_MAX_BYTES, PACKAGE_MAX_BYTES,
};
pub use mcp_tools::{PluginMcpTool, PluginMcpToolKind};
pub use migration::MigratedSettings;
pub use state::{HostClock, PluginServices, SealResourceUrl};
pub use system::PluginSystem;
pub use tasks::{TaskOutcome, TaskTrigger};
