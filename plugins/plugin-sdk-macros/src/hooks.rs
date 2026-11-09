//! The hook table: which manifest hook ids a trait serves and which WIT
//! interface it exports.
//!
//! It mirrors `backend/plugins/src/manifest/hook.rs` (`hook_ids!`). Two tests
//! keep it honest: one asserts that its interfaces are exactly
//! `tuliprox_plugin_api::EXPORT_INTERFACES`, the other parses `hook.rs` and
//! compares every `id -> interface` row.

/// One hook family.
pub(crate) struct Hook {
    /// The SDK trait the author implements (`tuliprox_plugin_sdk::EventHooks`).
    pub trait_name: &'static str,
    /// The WIT interface, e.g. `event-hooks`.
    pub interface: &'static str,
    /// The manifest `hooks:` ids that this interface serves.
    pub ids: &'static [&'static str],
}

impl Hook {
    /// The Rust module wit-bindgen generates for the interface.
    pub(crate) fn module(&self) -> String { self.interface.replace('-', "_") }

    /// The per-interface export macro wit-bindgen generates (`pub_export_macro`).
    pub(crate) fn export_macro(&self) -> String { format!("__export_tuliprox_plugin_{}_1_0_0_cabi", self.module()) }

    /// The fully qualified component export name the registry looks up.
    pub(crate) fn qualified(&self) -> String { qualified(self.interface) }
}

/// The type-only interfaces an export `use`s besides `types`. wit-bindgen
/// wants a `with` entry for exactly the ones the world reaches (an entry for an
/// unreached one is an error), and the SDK's own bindings hold them all.
pub(crate) fn type_interfaces(interface: &str) -> &'static [&'static str] {
    match interface {
        "epg-load-hooks"
        | "epg-transform-hooks"
        | "epg-enrich-hooks"
        | "epg-match-hooks"
        | "epg-conflict-hooks"
        | "input-provider" => &["epg-types"],
        "metrics-hooks" | "notification-hooks" => &["status"],
        _ => &[],
    }
}

pub(crate) fn qualified(interface: &str) -> String { format!("tuliprox:plugin/{interface}@1.0.0") }

/// `lifecycle` is exported by every plugin and has no manifest hook id.
pub(crate) const LIFECYCLE_TRAIT: &str = "Lifecycle";
pub(crate) const LIFECYCLE_INTERFACE: &str = "lifecycle";

macro_rules! hook_table {
    ($($trait_name:literal, $interface:literal, [$($id:literal),+ $(,)?];)+) => {
        pub(crate) const HOOKS: &[Hook] = &[
            $(Hook { trait_name: $trait_name, interface: $interface, ids: &[$($id),+] },)+
        ];
    };
}

hook_table! {
    "PlaylistHooks", "playlist-hooks", [
        "playlist.before-filter", "playlist.after-filter", "playlist.before-rename",
        "playlist.after-rename", "playlist.before-map", "playlist.after-map",
    ];
    "SourceHooks", "source-hooks", ["playlist.loaded"];
    "SortHooks", "sort-hooks", ["playlist.after-sort"];
    "MetadataHooks", "metadata-hooks", ["playlist.enrich-metadata"];
    "SortKeyHooks", "sort-key-hooks", ["playlist.sort-key"];
    "ChnoHooks", "chno-hooks", ["playlist.assign-chno"];
    "DedupeHooks", "dedupe-hooks", ["playlist.resolve-duplicates"];
    "GroupMergeHooks", "group-merge-hooks", ["playlist.merge-groups"];
    "FilterHooks", "filter-hooks", ["filter.eval-fn"];
    "EpgLoadHooks", "epg-load-hooks", ["epg.loaded"];
    "EpgTransformHooks", "epg-transform-hooks", ["epg.transform-programmes"];
    "EpgEnrichHooks", "epg-enrich-hooks", ["epg.enrich-programmes"];
    "EpgMatchHooks", "epg-match-hooks", ["epg.match-channel"];
    "EpgConflictHooks", "epg-conflict-hooks", ["epg.resolve-conflict"];
    "StreamHooks", "stream-hooks", ["stream.inject-headers"];
    "EventHooks", "event-hooks", ["events.on-event"];
    "MetricsHooks", "metrics-hooks", ["metrics.collect"];
    "NotificationHooks", "notification-hooks", ["notifications.send"];
    "InputProvider", "input-provider", ["input.fetch"];
    "OutputGenerator", "output-generator", ["output.render"];
    "Rpc", "rpc", ["rpc.call"];
    "Tasks", "tasks", ["tasks.run"];
    "SettingsMigration", "settings-migration", ["settings.migrate"];
    "HttpRoutes", "http-routes", ["http.handle"];
    "ManagedChannelHooks", "managed-channel-hooks", ["playlist.managed-channels"];
}

/// The family that serves a manifest hook id.
pub(crate) fn hook_for_id(id: &str) -> Option<&'static Hook> { HOOKS.iter().find(|hook| hook.ids.contains(&id)) }

/// The family an SDK trait belongs to (`None` for `Lifecycle` and unrelated traits).
pub(crate) fn hook_for_trait(name: &str) -> Option<&'static Hook> { HOOKS.iter().find(|hook| hook.trait_name == name) }

/// Every manifest id, in table order.
#[cfg(test)]
pub(crate) fn all_ids() -> Vec<&'static str> { HOOKS.iter().flat_map(|hook| hook.ids.iter().copied()).collect() }

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn the_table_covers_exactly_the_exported_interfaces() {
        let mut table: BTreeSet<String> = HOOKS.iter().map(Hook::qualified).collect();
        table.insert(qualified(LIFECYCLE_INTERFACE));
        let api: BTreeSet<String> =
            tuliprox_plugin_api::EXPORT_INTERFACES.iter().map(|name| (*name).to_string()).collect();
        assert_eq!(table, api);
        assert_eq!(table.len(), HOOKS.len() + 1, "an interface is listed twice");
    }

    #[test]
    fn ids_and_traits_are_unique() {
        let ids = all_ids();
        assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), ids.len());
        let traits: BTreeSet<_> = HOOKS.iter().map(|hook| hook.trait_name).collect();
        assert_eq!(traits.len(), HOOKS.len());
        assert!(!traits.contains(LIFECYCLE_TRAIT));
    }

    /// `backend/plugins/src/manifest/hook.rs` is the registry's table: every
    /// `Variant => "id", "interface", ...;` row there must be a row here.
    #[test]
    fn the_table_matches_the_registry_hook_table() {
        let registry = include_str!("../../../backend/plugins/src/manifest/hook.rs");
        let rows: Vec<(String, String)> = registry
            .lines()
            .filter_map(|line| {
                let (_, rest) = line.trim().split_once("=> \"")?;
                let (id, rest) = rest.split_once("\", \"")?;
                let (interface, _) = rest.split_once("\", Requirement::")?;
                Some((id.to_string(), interface.to_string()))
            })
            .collect();
        assert_eq!(rows.len(), all_ids().len(), "the registry table and the SDK table differ in size");
        for (id, interface) in rows {
            let hook = hook_for_id(&id).unwrap_or_else(|| panic!("registry hook `{id}` is missing from the SDK table"));
            assert_eq!(hook.interface, interface, "hook `{id}` maps to a different interface");
        }
    }
}
