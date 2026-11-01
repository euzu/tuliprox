//! The hook traits an author implements, one per WIT interface.
//!
//! Hooks are associated functions (no `self`): a component instance is
//! single-threaded and the host builds a fresh one per instantiation, so state
//! lives in `static`s / `thread_local!`s initialised by [`Lifecycle::init`].
//! Every hook returns `Result<_, String>`; an `Err` is treated exactly like a
//! trap (fail-open, logged, counted) and `std::result::Result<_, kv::Error>` /
//! `http::Error` convert into it with `?`.
//!
//! Implement the traits inside a `#[tuliprox_plugin]` module; the macro exports
//! exactly the interfaces implemented and checks them against `manifest.yml`.

use crate::{
    bindings::exports::tuliprox::plugin as exports,
    wit::{
        dedupe_hooks::DuplicateGroup,
        epg_conflict_hooks::EpgConflict,
        epg_match_hooks::MatchRequest,
        epg_types::{EpgChannel, EpgProgramme},
        event_hooks::Event,
        input_provider::InputRequest,
        metrics_hooks::Metric,
        notification_hooks::{DeliveryError, Notification},
        output_generator::OutputRequest,
        playlist_hooks::Stage,
        stream_hooks::UpstreamRequest,
        tasks::TaskContext,
        types::{Channel, ChannelChange},
    },
};
use std::marker::PhantomData;

type Res<T> = Result<T, String>;

/// `lifecycle`: always exported. Without an impl in the plugin module the
/// macro supplies one that parses the `#[tuliprox_plugin_settings]` struct (if
/// any) from `settings` and otherwise does nothing.
pub trait Lifecycle {
    /// Once per instantiation with the settings JSON. An `Err` disables the
    /// instance, with the message in the plugin log.
    fn init(settings: &str) -> Res<()>;
}

/// `playlist-hooks`: manifest `playlist.{before,after}-{filter,rename,map}`.
/// Only the stages the manifest lists arrive; the host may batch (several calls
/// per stage per target, disjoint subsets). Return a sparse change list.
pub trait PlaylistHooks {
    fn on_stage(stage: Stage, target: &str, channels: Vec<Channel>) -> Res<Vec<ChannelChange>>;
}

/// `source-hooks`: manifest `playlist.loaded`.
pub trait SourceHooks {
    fn on_playlist_loaded(input_name: &str, channels: Vec<Channel>) -> Res<Vec<ChannelChange>>;
}

/// `sort-hooks`: manifest `playlist.after-sort`. Handles in the new order.
pub trait SortHooks {
    fn after_sort(target: &str, channels: Vec<Channel>) -> Res<Vec<u32>>;
}

/// `metadata-hooks`: manifest `playlist.enrich-metadata`. Return amended
/// TMDB-style JSON, or `None` to leave it.
pub trait MetadataHooks {
    fn enrich_metadata(item: Channel, metadata_json: &str) -> Res<Option<String>>;
}

/// `sort-key-hooks`: manifest `playlist.sort-key`. Index-aligned keys.
pub trait SortKeyHooks {
    fn sort_key(target: &str, channels: Vec<Channel>) -> Res<Vec<String>>;
}

/// `chno-hooks`: manifest `playlist.assign-chno`. `(handle, chno)` pairs.
pub trait ChnoHooks {
    fn assign_channel_numbers(target: &str, channels: Vec<Channel>) -> Res<Vec<(u32, u32)>>;
}

/// `dedupe-hooks`: manifest `playlist.resolve-duplicates`. Handles to keep, in
/// preference order; empty means "host default".
pub trait DedupeHooks {
    fn resolve_duplicates(target: &str, group: DuplicateGroup) -> Res<Vec<u32>>;
}

/// `group-merge-hooks`: manifest `playlist.merge-groups`. Canonical group
/// name to the groups merged under it.
pub trait GroupMergeHooks {
    fn merge_groups(target: &str, groups: Vec<String>) -> Res<Vec<(String, Vec<String>)>>;
}

/// `filter-hooks`: manifest `filter.eval-fn`. Result positions match `channels`.
pub trait FilterHooks {
    fn eval_filter_batch(name: &str, channels: Vec<Channel>, args: Vec<String>) -> Res<Vec<bool>>;
}

/// `epg-load-hooks`: manifest `epg.loaded`. Sparse: returned ids replace names/icon.
pub trait EpgLoadHooks {
    fn on_epg_loaded(source: &str, channels: Vec<EpgChannel>) -> Res<Vec<EpgChannel>>;
}

/// `epg-transform-hooks`: manifest `epg.transform-programmes` (capability
/// `epg_write`). Returns the batch to keep, in order.
pub trait EpgTransformHooks {
    fn transform_programmes(source: &str, programmes: Vec<EpgProgramme>) -> Res<Vec<EpgProgramme>>;
}

/// `epg-enrich-hooks`: manifest `epg.enrich-programmes` (capability `epg_write`).
pub trait EpgEnrichHooks {
    fn enrich_programmes(channel_id: &str, programmes: Vec<EpgProgramme>) -> Res<Vec<EpgProgramme>>;
}

/// `epg-match-hooks`: manifest `epg.match-channel`. Index-aligned: the winning
/// EPG channel id per request, or `None`. Batched (plural).
pub trait EpgMatchHooks {
    fn match_channels(requests: Vec<MatchRequest>) -> Res<Vec<Option<String>>>;
}

/// `epg-conflict-hooks`: manifest `epg.resolve-conflict`. The candidate index
/// to keep, or `None` for the host default.
pub trait EpgConflictHooks {
    fn resolve_conflict(conflict: EpgConflict) -> Res<Option<u32>>;
}

/// `stream-hooks`: manifest `stream.inject-headers` (capability
/// `stream_headers`). Strictly bounded: 50 ms, cached, fail-open.
pub trait StreamHooks {
    fn inject_upstream_headers(req: UpstreamRequest) -> Res<Vec<(String, String)>>;
}

/// `event-hooks`: manifest `events.on-event` plus the `events:` list.
/// Fire-and-forget; decode the payload with [`EventExt::typed`](crate::events::EventExt::typed).
pub trait EventHooks {
    fn on_event(ev: Event) -> Res<()>;
}

/// `metrics-hooks`: manifest `metrics.collect`. Polled (throttled) by the host;
/// see the [`metrics!`](crate::metrics!) macro.
pub trait MetricsHooks {
    fn collect_metrics() -> Res<Vec<Metric>>;
}

/// `notification-hooks`: manifest `notifications.send` (capability
/// `notifications`). The host's outbox owns retrying; pick the right
/// [`DeliveryError`].
pub trait NotificationHooks {
    fn send_notification(msg: Notification) -> Result<(), DeliveryError>;
}

/// `input-provider`: manifest `input.fetch` (`provides.input_type: true`).
pub trait InputProvider {
    fn fetch_playlist(req: &InputRequest) -> Res<Vec<Channel>>;
    /// `Ok(vec![])` when the provider has no EPG.
    fn fetch_epg(req: &InputRequest) -> Res<Vec<(EpgChannel, Vec<EpgProgramme>)>>;
}

/// `output-generator`: manifest `output.render` (`provides.output_format`).
/// One instance per output file, so the three calls may share state.
pub trait OutputGenerator {
    /// The header bytes.
    fn begin_output(req: &OutputRequest) -> Res<Vec<u8>>;
    fn render_batch(channels: Vec<Channel>) -> Res<Vec<u8>>;
    /// The trailer bytes.
    fn finish_output() -> Res<Vec<u8>>;
}

/// `rpc`: manifest `rpc.call` (non-empty `ui.rpc_exports`). Payload and result
/// are JSON; see [`rpc::Router`](crate::rpc::Router).
pub trait Rpc {
    fn call(method: &str, payload: &str) -> Res<String>;
}

/// `tasks`: manifest `tasks.run` (non-empty `tasks:`). See
/// [`tasks::Router`](crate::tasks::Router) to dispatch on `ctx.task_id`.
pub trait Tasks {
    /// `Ok(summary)` is shown in the UI and stats; `Err` publishes `scheduled_task.failed`.
    fn run(ctx: TaskContext) -> Res<String>;
}

/// `settings-migration`: manifest `settings.migrate` (no capability). The host
/// calls it once when the stored settings carry an older settings-schema
/// `version` than the installed package: `from_version` is the stored version
/// and `settings` the stored JSON (secret `${env:VAR}` references resolved).
/// Return the JSON for the current schema version; the host validates it before
/// it saves it. An `Err` keeps the stored settings and marks the plugin "needs
/// settings review". No `init` ran on the old settings, so do not use
/// [`PluginSettings::current`](crate::settings::PluginSettings::current) here;
/// [`Migrations`](crate::settings::Migrations) chains one step per old version.
pub trait SettingsMigration {
    fn migrate(from_version: u32, settings: &str) -> Res<String>;
}

/// Implements each generated `Guest` trait for any type implementing the SDK
/// trait of the same interface. The `#[tuliprox_plugin]` glue exports
/// `Adapter<UserType>`, so no signature is written twice by the macro.
#[doc(hidden)]
pub struct Adapter<T>(PhantomData<fn() -> T>);

impl<T: Lifecycle> exports::lifecycle::Guest for Adapter<T> {
    fn init(settings: String) -> Res<()> { T::init(&settings) }
}

impl<T: PlaylistHooks> exports::playlist_hooks::Guest for Adapter<T> {
    fn on_stage(stage: Stage, target: String, channels: Vec<Channel>) -> Res<Vec<ChannelChange>> {
        T::on_stage(stage, &target, channels)
    }
}

impl<T: SourceHooks> exports::source_hooks::Guest for Adapter<T> {
    fn on_playlist_loaded(input_name: String, channels: Vec<Channel>) -> Res<Vec<ChannelChange>> {
        T::on_playlist_loaded(&input_name, channels)
    }
}

impl<T: SortHooks> exports::sort_hooks::Guest for Adapter<T> {
    fn after_sort(target: String, channels: Vec<Channel>) -> Res<Vec<u32>> { T::after_sort(&target, channels) }
}

impl<T: MetadataHooks> exports::metadata_hooks::Guest for Adapter<T> {
    fn enrich_metadata(item: Channel, metadata_json: String) -> Res<Option<String>> {
        T::enrich_metadata(item, &metadata_json)
    }
}

impl<T: SortKeyHooks> exports::sort_key_hooks::Guest for Adapter<T> {
    fn sort_key(target: String, channels: Vec<Channel>) -> Res<Vec<String>> { T::sort_key(&target, channels) }
}

impl<T: ChnoHooks> exports::chno_hooks::Guest for Adapter<T> {
    fn assign_channel_numbers(target: String, channels: Vec<Channel>) -> Res<Vec<(u32, u32)>> {
        T::assign_channel_numbers(&target, channels)
    }
}

impl<T: DedupeHooks> exports::dedupe_hooks::Guest for Adapter<T> {
    fn resolve_duplicates(target: String, group: DuplicateGroup) -> Res<Vec<u32>> {
        T::resolve_duplicates(&target, group)
    }
}

impl<T: GroupMergeHooks> exports::group_merge_hooks::Guest for Adapter<T> {
    fn merge_groups(target: String, groups: Vec<String>) -> Res<Vec<(String, Vec<String>)>> {
        T::merge_groups(&target, groups)
    }
}

impl<T: FilterHooks> exports::filter_hooks::Guest for Adapter<T> {
    fn eval_filter_batch(name: String, channels: Vec<Channel>, args: Vec<String>) -> Res<Vec<bool>> {
        T::eval_filter_batch(&name, channels, args)
    }
}

impl<T: EpgLoadHooks> exports::epg_load_hooks::Guest for Adapter<T> {
    fn on_epg_loaded(source: String, channels: Vec<EpgChannel>) -> Res<Vec<EpgChannel>> {
        T::on_epg_loaded(&source, channels)
    }
}

impl<T: EpgTransformHooks> exports::epg_transform_hooks::Guest for Adapter<T> {
    fn transform_programmes(source: String, programmes: Vec<EpgProgramme>) -> Res<Vec<EpgProgramme>> {
        T::transform_programmes(&source, programmes)
    }
}

impl<T: EpgEnrichHooks> exports::epg_enrich_hooks::Guest for Adapter<T> {
    fn enrich_programmes(channel_id: String, programmes: Vec<EpgProgramme>) -> Res<Vec<EpgProgramme>> {
        T::enrich_programmes(&channel_id, programmes)
    }
}

impl<T: EpgMatchHooks> exports::epg_match_hooks::Guest for Adapter<T> {
    fn match_channels(requests: Vec<MatchRequest>) -> Res<Vec<Option<String>>> { T::match_channels(requests) }
}

impl<T: EpgConflictHooks> exports::epg_conflict_hooks::Guest for Adapter<T> {
    fn resolve_conflict(conflict: EpgConflict) -> Res<Option<u32>> { T::resolve_conflict(conflict) }
}

impl<T: StreamHooks> exports::stream_hooks::Guest for Adapter<T> {
    fn inject_upstream_headers(req: UpstreamRequest) -> Res<Vec<(String, String)>> { T::inject_upstream_headers(req) }
}

impl<T: EventHooks> exports::event_hooks::Guest for Adapter<T> {
    fn on_event(ev: Event) -> Res<()> { T::on_event(ev) }
}

impl<T: MetricsHooks> exports::metrics_hooks::Guest for Adapter<T> {
    fn collect_metrics() -> Res<Vec<Metric>> { T::collect_metrics() }
}

impl<T: NotificationHooks> exports::notification_hooks::Guest for Adapter<T> {
    fn send_notification(msg: Notification) -> Result<(), DeliveryError> { T::send_notification(msg) }
}

impl<T: InputProvider> exports::input_provider::Guest for Adapter<T> {
    fn fetch_playlist(req: InputRequest) -> Res<Vec<Channel>> { T::fetch_playlist(&req) }
    fn fetch_epg(req: InputRequest) -> Res<Vec<(EpgChannel, Vec<EpgProgramme>)>> { T::fetch_epg(&req) }
}

impl<T: OutputGenerator> exports::output_generator::Guest for Adapter<T> {
    fn begin_output(req: OutputRequest) -> Res<Vec<u8>> { T::begin_output(&req) }
    fn render_batch(channels: Vec<Channel>) -> Res<Vec<u8>> { T::render_batch(channels) }
    fn finish_output() -> Res<Vec<u8>> { T::finish_output() }
}

impl<T: Rpc> exports::rpc::Guest for Adapter<T> {
    fn call(method: String, payload: String) -> Res<String> { T::call(&method, &payload) }
}

impl<T: Tasks> exports::tasks::Guest for Adapter<T> {
    fn run(ctx: TaskContext) -> Res<String> { T::run(ctx) }
}

impl<T: SettingsMigration> exports::settings_migration::Guest for Adapter<T> {
    fn migrate(from_version: u32, settings: String) -> Res<String> { T::migrate(from_version, &settings) }
}
