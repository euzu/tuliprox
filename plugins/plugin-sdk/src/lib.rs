//! Author SDK for tuliprox wasm plugins.
//!
//! ```ignore
//! use tuliprox_plugin_sdk::prelude::*;
//!
//! #[tuliprox_plugin_settings]
//! struct Settings { api_key: Secret<String>, #[serde(default)] refresh_hours: u32 }
//!
//! #[tuliprox_plugin]
//! mod plugin {
//!     use super::*;
//!     struct Enricher;
//!     impl PlaylistHooks for Enricher { /* ... */ }
//! }
//! ```
//!
//! What is in the box:
//!
//! - the guest bindings of the whole contract ([`bindings`], [`wit`]): ONE set
//!   of types and imports that the `#[tuliprox_plugin]` glue reuses, so your
//!   code and these wrappers agree on every type;
//! - the hook traits ([`hooks`]) and the two attribute macros;
//! - wrappers over the host: [`log`], [`kv`], [`images`], [`http`], [`clock`],
//!   [`config`], typed [`events`], [`metrics!`], [`tasks`] and [`rpc`]
//!   dispatch, and [`settings`] with [`settings::Secret`];
//! - on every target but `wasm32`, a [`mock`] host that backs all of those, so
//!   plugin logic is unit-testable natively.
//!
//! Everything that touches the wasm imports is behind `cfg(target_arch =
//! "wasm32")`; the crate builds and tests on the host.
#![recursion_limit = "256"]

/// The generated guest bindings of every capability world.
///
/// One world (`tuliprox:sdk/union`) is generated here to share its types; it is
/// never exported as such. `pub_export_macro` splits its component-type
/// section in two: the imports (kept, unioned with the plugin's own world) and
/// the exports (only inside the world-level `export!` macro, which nothing
/// invokes). The plugin's exports come from `#[tuliprox_plugin]`, per
/// interface, so a component exports exactly the hooks it implements.
#[allow(clippy::all, clippy::pedantic, missing_docs, unused_qualifications, dead_code, unused_imports, unreachable_pub)]
pub mod bindings {
    wit_bindgen::generate!({
        path: "../plugin-api/wit",
        inline: "
            package tuliprox:sdk;

            world union {
                include tuliprox:plugin/playlist-plugin@1.0.0;
                include tuliprox:plugin/source-plugin@1.0.0;
                include tuliprox:plugin/sort-plugin@1.0.0;
                include tuliprox:plugin/metadata-plugin@1.0.0;
                include tuliprox:plugin/sort-key-plugin@1.0.0;
                include tuliprox:plugin/chno-plugin@1.0.0;
                include tuliprox:plugin/dedupe-plugin@1.0.0;
                include tuliprox:plugin/group-merge-plugin@1.0.0;
                include tuliprox:plugin/filter-plugin@1.0.0;
                include tuliprox:plugin/epg-load-plugin@1.0.0;
                include tuliprox:plugin/epg-transform-plugin@1.0.0;
                include tuliprox:plugin/epg-enrich-plugin@1.0.0;
                include tuliprox:plugin/epg-match-plugin@1.0.0;
                include tuliprox:plugin/epg-conflict-plugin@1.0.0;
                include tuliprox:plugin/stream-plugin@1.0.0;
                include tuliprox:plugin/event-plugin@1.0.0;
                include tuliprox:plugin/metrics-plugin@1.0.0;
                include tuliprox:plugin/notifier-plugin@1.0.0;
                include tuliprox:plugin/input-plugin@1.0.0;
                include tuliprox:plugin/output-plugin@1.0.0;
                include tuliprox:plugin/rpc-plugin@1.0.0;
                include tuliprox:plugin/task-plugin@1.0.0;
                include tuliprox:plugin/settings-migration-plugin@1.0.0;
            }
        ",
        world: "tuliprox:sdk/union",
        pub_export_macro: true,
        export_macro_name: "__export_union",
        default_bindings_module: "$crate::bindings",
        generate_all,
    });
}

pub mod clock;
pub mod config;
pub mod events;
pub mod hooks;
pub mod http;
pub mod images;
pub mod kv;
pub mod log;
pub mod metrics;
#[cfg(not(target_arch = "wasm32"))]
pub mod mock;
pub mod rpc;
pub mod settings;
mod sys;
pub mod tasks;

pub use hooks::{
    ChnoHooks, DedupeHooks, EpgConflictHooks, EpgEnrichHooks, EpgLoadHooks, EpgMatchHooks, EpgTransformHooks,
    EventHooks, FilterHooks, GroupMergeHooks, InputProvider, Lifecycle, MetadataHooks, MetricsHooks, NotificationHooks,
    OutputGenerator, PlaylistHooks, Rpc, SettingsMigration, SortHooks, SortKeyHooks, SourceHooks, StreamHooks, Tasks,
};
pub use settings::{Migrations, PluginSettings, Secret};
/// The contract version the SDK is generated against.
pub use tuliprox_plugin_api::{ABI_MAJOR, ABI_VERSION};
pub use tuliprox_plugin_sdk_macros::{tuliprox_plugin, tuliprox_plugin_settings};

/// The wit types, grouped as the WIT names them.
pub mod wit {
    pub use crate::bindings::{
        exports::tuliprox::plugin::{
            chno_hooks, dedupe_hooks, epg_conflict_hooks, epg_enrich_hooks, epg_load_hooks, epg_match_hooks,
            epg_transform_hooks, event_hooks, filter_hooks, group_merge_hooks, input_provider, lifecycle,
            metadata_hooks, metrics_hooks, notification_hooks, output_generator, playlist_hooks, rpc,
            settings_migration, sort_hooks, sort_key_hooks, source_hooks, stream_hooks, tasks,
        },
        tuliprox::plugin::{epg_types, host, status, types},
    };
}

/// Everything a plugin crate usually needs.
pub mod prelude {
    pub use crate::{
        clock, config, debug, error,
        events::{EventExt, TypedEvent},
        hooks::*,
        http::{self, Client as HttpClient},
        images, info, kv, log, metrics, rpc,
        settings::{Migrations, PluginSettings, Secret},
        tasks, trace, warn,
        wit::{
            dedupe_hooks::DuplicateGroup,
            epg_conflict_hooks::{ConflictKind, EpgConflict},
            epg_match_hooks::MatchRequest,
            epg_types::{EpgCategory, EpgChannel, EpgProgramme},
            event_hooks::Event,
            input_provider::InputRequest,
            metrics_hooks::Metric,
            notification_hooks::{DeliveryError, Notification},
            output_generator::OutputRequest,
            playlist_hooks::Stage,
            status::Severity,
            stream_hooks::UpstreamRequest,
            tasks::TaskContext,
            types::{Channel, ChannelChange, Cluster, ItemType, LogLevel},
        },
    };
    pub use tuliprox_plugin_sdk_macros::{tuliprox_plugin, tuliprox_plugin_settings};
}

/// Used by the macros' expansions. Not part of the API.
#[doc(hidden)]
pub mod __private {
    pub use crate::hooks::Adapter;
    pub use serde;
    pub use wit_bindgen;
}
