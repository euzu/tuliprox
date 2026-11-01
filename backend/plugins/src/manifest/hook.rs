//! The `hooks:` ids of a manifest (plan §4, *Hook ids (frozen W2)*).

use super::Capability;
use shared::model::PluginHook;

/// What else a manifest must declare for a hook id to be valid: the "Also
/// requires" column of the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Requirement {
    None,
    Capability(Capability),
    /// A non-empty `events:` list.
    Events,
    /// `provides.input_type: true`.
    InputType,
    /// `provides.output_format: <id>`.
    OutputFormat,
    /// A non-empty `ui.rpc_exports`.
    RpcExports,
    /// A non-empty `tasks:` list.
    Tasks,
}

macro_rules! hook_ids {
    ($($variant:ident => $id:literal, $wit:literal, $requires:expr;)+) => {
        /// A manifest `hooks:` id. Each maps to exactly one WIT interface and
        /// the manifest lists an id iff the component exports that interface.
        /// Ids are case sensitive.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub(crate) enum HookId {
            $($variant,)+
        }

        impl HookId {
            /// Every id, in table order.
            pub(crate) const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The dotted manifest spelling.
            pub(crate) const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $id,)+ }
            }

            /// The `tuliprox:plugin/<interface>@1.0.0` interface the component exports.
            pub(crate) const fn wit_interface(self) -> &'static str {
                match self { $(Self::$variant => $wit,)+ }
            }

            /// What else the manifest must declare.
            pub(crate) const fn requirement(self) -> Requirement {
                match self { $(Self::$variant => $requires,)+ }
            }
        }
    };
}

hook_ids! {
    PlaylistBeforeFilter => "playlist.before-filter", "playlist-hooks", Requirement::None;
    PlaylistAfterFilter => "playlist.after-filter", "playlist-hooks", Requirement::None;
    PlaylistBeforeRename => "playlist.before-rename", "playlist-hooks", Requirement::None;
    PlaylistAfterRename => "playlist.after-rename", "playlist-hooks", Requirement::None;
    PlaylistBeforeMap => "playlist.before-map", "playlist-hooks", Requirement::None;
    PlaylistAfterMap => "playlist.after-map", "playlist-hooks", Requirement::None;
    PlaylistLoaded => "playlist.loaded", "source-hooks", Requirement::None;
    PlaylistAfterSort => "playlist.after-sort", "sort-hooks", Requirement::None;
    PlaylistEnrichMetadata => "playlist.enrich-metadata", "metadata-hooks", Requirement::None;
    PlaylistSortKey => "playlist.sort-key", "sort-key-hooks", Requirement::None;
    PlaylistAssignChno => "playlist.assign-chno", "chno-hooks", Requirement::None;
    PlaylistResolveDuplicates => "playlist.resolve-duplicates", "dedupe-hooks", Requirement::None;
    PlaylistMergeGroups => "playlist.merge-groups", "group-merge-hooks", Requirement::None;
    FilterEvalFn => "filter.eval-fn", "filter-hooks", Requirement::None;
    StreamInjectHeaders => "stream.inject-headers", "stream-hooks", Requirement::Capability(Capability::StreamHeaders);
    EpgLoaded => "epg.loaded", "epg-load-hooks", Requirement::None;
    EpgTransformProgrammes => "epg.transform-programmes", "epg-transform-hooks", Requirement::Capability(Capability::EpgWrite);
    EpgEnrichProgrammes => "epg.enrich-programmes", "epg-enrich-hooks", Requirement::Capability(Capability::EpgWrite);
    EpgMatchChannel => "epg.match-channel", "epg-match-hooks", Requirement::None;
    EpgResolveConflict => "epg.resolve-conflict", "epg-conflict-hooks", Requirement::None;
    EventsOnEvent => "events.on-event", "event-hooks", Requirement::Events;
    MetricsCollect => "metrics.collect", "metrics-hooks", Requirement::None;
    NotificationsSend => "notifications.send", "notification-hooks", Requirement::Capability(Capability::Notifications);
    InputFetch => "input.fetch", "input-provider", Requirement::InputType;
    OutputRender => "output.render", "output-generator", Requirement::OutputFormat;
    RpcCall => "rpc.call", "rpc", Requirement::RpcExports;
    TasksRun => "tasks.run", "tasks", Requirement::Tasks;
    SettingsMigrate => "settings.migrate", "settings-migration", Requirement::None;
}

/// Phase S spelled the six stage ids without the `playlist.` prefix.
const RETIRED_BARE_STAGES: [&str; 6] =
    ["before-filter", "after-filter", "before-rename", "after-rename", "before-map", "after-map"];

impl HookId {
    /// Parse one `hooks:` entry. The message names what to write instead for
    /// the retired bare stage spelling and for a wrong-case id.
    pub(crate) fn parse(name: &str) -> Result<Self, String> {
        if let Some(hook) = Self::ALL.iter().copied().find(|hook| hook.as_str() == name) {
            return Ok(hook);
        }
        if RETIRED_BARE_STAGES.contains(&name) {
            return Err(format!("hook `{name}` is a bare stage name; write the dotted form `playlist.{name}`"));
        }
        if let Some(hook) = Self::ALL.iter().copied().find(|hook| hook.as_str().eq_ignore_ascii_case(name)) {
            return Err(format!("unknown hook `{name}`; ids are case-sensitive, did you mean `{}`?", hook.as_str()));
        }
        Err(format!("unknown hook `{name}`"))
    }

    /// The pipeline hook the host can run, if this id has one.
    ///
    /// Every id maps to a host hook except three: `epg.resolve-conflict` (v1.x,
    /// needs the blocking bridge; validated and kept, never run), `rpc.call`
    /// (run by the rpc route and MCP tools) and `settings.migrate` (W8, run by
    /// the settings migration step).
    pub(crate) const fn pipeline_hook(self) -> Option<PluginHook> {
        match self {
            Self::PlaylistLoaded => Some(PluginHook::PlaylistLoaded),
            Self::PlaylistBeforeFilter => Some(PluginHook::BeforeFilter),
            Self::PlaylistAfterFilter => Some(PluginHook::AfterFilter),
            Self::PlaylistBeforeRename => Some(PluginHook::BeforeRename),
            Self::PlaylistAfterRename => Some(PluginHook::AfterRename),
            Self::PlaylistBeforeMap => Some(PluginHook::BeforeMap),
            Self::PlaylistAfterMap => Some(PluginHook::AfterMap),
            Self::PlaylistAfterSort => Some(PluginHook::AfterSort),
            Self::PlaylistEnrichMetadata => Some(PluginHook::EnrichMetadata),
            Self::PlaylistSortKey => Some(PluginHook::SortKey),
            Self::PlaylistAssignChno => Some(PluginHook::AssignChno),
            Self::PlaylistResolveDuplicates => Some(PluginHook::ResolveDuplicates),
            Self::PlaylistMergeGroups => Some(PluginHook::MergeGroups),
            Self::FilterEvalFn => Some(PluginHook::FilterEval),
            Self::EpgLoaded => Some(PluginHook::EpgLoaded),
            Self::EpgTransformProgrammes => Some(PluginHook::TransformProgrammes),
            Self::EpgEnrichProgrammes => Some(PluginHook::EnrichProgrammes),
            Self::EpgMatchChannel => Some(PluginHook::MatchChannel),
            Self::EventsOnEvent => Some(PluginHook::OnEvent),
            Self::MetricsCollect => Some(PluginHook::CollectMetrics),
            Self::NotificationsSend => Some(PluginHook::SendNotification),
            Self::InputFetch => Some(PluginHook::FetchInput),
            Self::OutputRender => Some(PluginHook::RenderOutput),
            Self::StreamInjectHeaders => Some(PluginHook::InjectHeaders),
            Self::TasksRun => Some(PluginHook::RunTask),
            // Called by the settings migration step, never by the pipeline.
            Self::EpgResolveConflict | Self::RpcCall | Self::SettingsMigrate => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn the_frozen_table_has_28_unique_ids_and_round_trips() {
        assert_eq!(HookId::ALL.len(), 28);
        let spellings: HashSet<_> = HookId::ALL.iter().map(|hook| hook.as_str()).collect();
        assert_eq!(spellings.len(), 28);
        for hook in HookId::ALL {
            assert_eq!(HookId::parse(hook.as_str()), Ok(*hook));
            let (group, name) = hook.as_str().split_once('.').expect("ids are dotted");
            assert!(!group.is_empty() && !name.is_empty() && !name.contains('.'), "{}", hook.as_str());
        }
    }

    #[test]
    fn the_six_stage_ids_share_playlist_hooks_and_the_rest_have_their_own_interface() {
        let stages = HookId::ALL.iter().filter(|hook| hook.wit_interface() == "playlist-hooks").count();
        assert_eq!(stages, 6);
        let others: HashSet<_> =
            HookId::ALL.iter().map(|hook| hook.wit_interface()).filter(|wit| *wit != "playlist-hooks").collect();
        assert_eq!(others.len(), 22, "22 other ids, 22 distinct interfaces");
        assert_eq!(HookId::PlaylistLoaded.wit_interface(), "source-hooks");
        assert_eq!(HookId::InputFetch.wit_interface(), "input-provider");
        assert_eq!(HookId::RpcCall.wit_interface(), "rpc");
    }

    #[test]
    fn the_also_requires_column() {
        let with_requirement: Vec<_> = HookId::ALL
            .iter()
            .filter(|hook| hook.requirement() != Requirement::None)
            .map(|hook| hook.as_str())
            .collect();
        assert_eq!(
            with_requirement,
            [
                "stream.inject-headers",
                "epg.transform-programmes",
                "epg.enrich-programmes",
                "events.on-event",
                "notifications.send",
                "input.fetch",
                "output.render",
                "rpc.call",
                "tasks.run"
            ]
        );
        assert_eq!(HookId::StreamInjectHeaders.requirement(), Requirement::Capability(Capability::StreamHeaders));
        assert_eq!(HookId::EpgEnrichProgrammes.requirement(), Requirement::Capability(Capability::EpgWrite));
        assert_eq!(HookId::NotificationsSend.requirement(), Requirement::Capability(Capability::Notifications));
        assert_eq!(HookId::EventsOnEvent.requirement(), Requirement::Events);
        assert_eq!(HookId::TasksRun.requirement(), Requirement::Tasks);
    }

    #[test]
    fn bare_stage_names_are_rejected_with_the_dotted_form() {
        for stage in RETIRED_BARE_STAGES {
            let message = HookId::parse(stage).unwrap_err();
            assert!(message.contains(&format!("`playlist.{stage}`")), "{message}");
            assert!(message.contains("bare stage name"), "{message}");
        }
        // only the six stage names were ever bare
        assert_eq!(HookId::parse("after-sort").unwrap_err(), "unknown hook `after-sort`");
    }

    #[test]
    fn ids_are_case_sensitive() {
        let message = HookId::parse("Playlist.After-Filter").unwrap_err();
        assert!(message.contains("did you mean `playlist.after-filter`"), "{message}");
        assert_eq!(HookId::parse("").unwrap_err(), "unknown hook ``");
    }

    #[test]
    fn every_hook_but_resolve_conflict_rpc_and_migrate_maps_to_a_host_hook() {
        let pipeline: Vec<_> = HookId::ALL.iter().filter_map(|hook| hook.pipeline_hook()).collect();
        assert_eq!(pipeline.len(), 25);
        assert_eq!(pipeline.iter().copied().collect::<HashSet<_>>().len(), 25);
        assert_eq!(HookId::PlaylistLoaded.pipeline_hook(), Some(PluginHook::PlaylistLoaded));
        assert_eq!(HookId::PlaylistAfterFilter.pipeline_hook(), Some(PluginHook::AfterFilter));
        assert_eq!(HookId::EpgResolveConflict.pipeline_hook(), None);
        assert_eq!(HookId::RpcCall.pipeline_hook(), None);
        assert_eq!(HookId::SettingsMigrate.pipeline_hook(), None);
        assert_eq!(HookId::StreamInjectHeaders.pipeline_hook(), Some(PluginHook::InjectHeaders));
        assert_eq!(HookId::TasksRun.pipeline_hook(), Some(PluginHook::RunTask));
    }
}
