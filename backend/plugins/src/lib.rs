//! The wasm plugin host.
//!
//! [`PluginSystem`] scans `<plugins dir>/<id>/` (manifest + `plugin.wasm`),
//! keeps every plugin's lifecycle state in a hot-reloaded registry, runs guests
//! under an epoch deadline, a memory limit and a global memory budget, gives
//! them the `host` functions and outbound HTTP through the dedicated plugin
//! client, and implements [`shared::model::PluginHost`], the seam the playlist
//! pipeline is generic over.
//!
//! The contract a plugin is written against lives in `tuliprox-plugin-api`.
#![recursion_limit = "256"]

mod api;
mod convert;
mod convert_epg;
mod dispatch;
mod epg_hooks;
mod events;
mod filter_batch;
mod header_inject;
mod host_fns;
mod http;
mod image_cache;
mod input_provider;
mod install;
mod kv;
mod logs;
mod manifest;
mod mcp_tools;
mod metrics;
mod migration;
mod notify;
mod output_provider;
mod registry;
mod rpc;
mod runtime;
mod settings;
mod state;
mod strategy;
mod system;
mod tasks;
mod ui_assets;

pub use mcp_tools::{PluginMcpTool, PluginMcpToolKind};
pub use migration::MigratedSettings;
pub use state::{HostClock, PluginServices, SealResourceUrl};
pub use system::PluginSystem;
pub use tasks::{TaskOutcome, TaskTrigger};
