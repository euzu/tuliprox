use crate::{
    app::{
        components::{
            playlist::explorer::{
                detail_panel::item_meta_label,
                epg_picker::EpgPicker,
                identity::{item_override_id, Id},
                image_preview::ImagePreview,
            },
            recording::target_name_for_id,
            IconButton, RadioButtonGroup, TextButton,
        },
        context::{ConfigContext, PlaylistExplorerContext},
    },
    generate_form_reducer,
    hooks::use_service_context,
    html_if,
    i18n::{use_translation, YewI18n},
    use_default_form_reducer,
};
use shared::{
    model::{
        InputRefreshPolicy, InputUpdateAction, ItemFieldsDto, ItemOverridePatch, ItemPatchDto, OverrideFieldOp,
        OverrideItemKind, OverrideProvenanceDto, OverrideSaveRequest, OverrideScopeDto, Permission, PlaylistRequest,
        SeriesStreamProperties, UiOverrideState, UiPlaylistCategories, UiPlaylistGroup, UiPlaylistItem, XtreamCluster,
    },
    utils::parse_localized_float,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    rc::Rc,
    sync::Arc,
};
use yew::{platform::spawn_local, prelude::*};

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct PlaylistOverrideDto {
    pub name: String,
    pub title: String,
    pub group: String,
    pub logo: String,
    pub logo_small: String,
    pub epg_channel_id: String,
    pub chno: String,
    pub parent_code: String,
    pub audio_track: String,
    pub time_shift: String,
    pub rec: String,
    pub tmdb: String,
    pub plot: String,
    pub genre: String,
    pub cast: String,
    pub director: String,
    pub release_date: String,
    pub rating: String,
}

generate_form_reducer!(
    state: OverrideEditFormState { form: PlaylistOverrideDto },
    action_name: OverrideEditFormAction,
    fields {
        Name => name: String,
        Title => title: String,
        Group => group: String,
        Logo => logo: String,
        LogoSmall => logo_small: String,
        EpgChannelId => epg_channel_id: String,
        Chno => chno: String,
        Tmdb => tmdb: String,
        Plot => plot: String,
        Genre => genre: String,
        Cast => cast: String,
        Director => director: String,
        ReleaseDate => release_date: String,
        Rating => rating: String,
    }
);

/// Fields this panel renders; a backend field error for anything else is surfaced as a toast.
const RENDERED_FIELDS: &[&str] = &[
    "name",
    "title",
    "group",
    "logo",
    "logo_small",
    "epg_channel_id",
    "chno",
    "tmdb",
    "plot",
    "genre",
    "cast",
    "director",
    "release_date",
    "rating",
];

#[derive(Debug, Clone, PartialEq, Default)]
pub struct FieldDeltas {
    pub reset_fields: HashSet<String>,
    pub clear_fields: HashSet<String>,
    pub remove_unknown: Vec<String>,
}

#[must_use]
pub fn build_item_patch(
    initial: &PlaylistOverrideDto,
    current: &PlaylistOverrideDto,
    deltas: &FieldDeltas,
    kind: OverrideItemKind,
) -> ItemOverridePatch {
    let mut patch = ItemOverridePatch::default();

    macro_rules! handle_string_field {
        ($field:ident) => {
            if deltas.reset_fields.contains(stringify!($field)) {
                patch.$field = Some(OverrideFieldOp::Reset);
            } else if deltas.clear_fields.contains(stringify!($field)) {
                patch.$field = Some(OverrideFieldOp::Clear);
            } else if current.$field != initial.$field {
                let trimmed = current.$field.trim();
                patch.$field = Some(OverrideFieldOp::Set(trimmed.to_string()));
            }
        };
    }

    handle_string_field!(name);
    handle_string_field!(title);
    handle_string_field!(group);
    handle_string_field!(logo);

    match kind {
        OverrideItemKind::Live => {
            handle_string_field!(logo_small);
            handle_string_field!(epg_channel_id);
            handle_string_field!(parent_code);
            handle_string_field!(audio_track);
            handle_string_field!(time_shift);
            handle_string_field!(rec);

            if deltas.reset_fields.contains("chno") {
                patch.chno = Some(OverrideFieldOp::Reset);
            } else if deltas.clear_fields.contains("chno") {
                patch.chno = Some(OverrideFieldOp::Clear);
            } else if current.chno != initial.chno {
                let trimmed = current.chno.trim();
                if trimmed.is_empty() {
                    patch.chno = Some(OverrideFieldOp::Clear);
                } else if let Ok(n) = trimmed.parse::<u32>() {
                    patch.chno = Some(OverrideFieldOp::Set(n));
                }
            }
        }
        OverrideItemKind::Vod | OverrideItemKind::Series => {
            handle_string_field!(plot);
            handle_string_field!(genre);
            handle_string_field!(cast);
            handle_string_field!(director);
            handle_string_field!(release_date);

            if deltas.reset_fields.contains("tmdb") {
                patch.tmdb = Some(OverrideFieldOp::Reset);
            } else if deltas.clear_fields.contains("tmdb") {
                patch.tmdb = Some(OverrideFieldOp::Clear);
            } else if current.tmdb != initial.tmdb {
                let trimmed = current.tmdb.trim();
                if trimmed.is_empty() {
                    patch.tmdb = Some(OverrideFieldOp::Clear);
                } else if let Ok(n) = trimmed.parse::<u32>() {
                    patch.tmdb = Some(OverrideFieldOp::Set(n));
                }
            }

            if deltas.reset_fields.contains("rating") {
                patch.rating = Some(OverrideFieldOp::Reset);
            } else if deltas.clear_fields.contains("rating") {
                patch.rating = Some(OverrideFieldOp::Clear);
            } else if current.rating != initial.rating {
                let trimmed = current.rating.trim();
                if trimmed.is_empty() {
                    patch.rating = Some(OverrideFieldOp::Clear);
                } else if let Some(rating) = parse_localized_float(trimmed) {
                    patch.rating = Some(OverrideFieldOp::Set(rating));
                }
            }
        }
    }

    patch.remove_unknown = deltas.remove_unknown.clone();
    patch
}

#[must_use]
pub fn patch_playlist_item_from_patch(
    item: &UiPlaylistItem,
    patch: &ItemOverridePatch,
    provenance: Option<&OverrideProvenanceDto>,
    has_remaining_deltas: bool,
) -> UiPlaylistItem {
    let mut updated = item.clone();

    // 1. Name
    if let Some(ref op) = patch.name {
        match op {
            OverrideFieldOp::Set(s) => updated.name = Arc::from(s.as_str()),
            OverrideFieldOp::Clear => updated.name = Arc::from(""),
            OverrideFieldOp::Reset => {
                if let Some(p) = provenance.and_then(|pr| pr.provider.as_ref()) {
                    updated.name = Arc::from(p.name.as_str());
                }
            }
        }
    }

    // 2. Title
    if let Some(ref op) = patch.title {
        match op {
            OverrideFieldOp::Set(s) => updated.title = Arc::from(s.as_str()),
            OverrideFieldOp::Clear => updated.title = Arc::from(""),
            OverrideFieldOp::Reset => {
                if let Some(p) = provenance.and_then(|pr| pr.provider.as_ref()) {
                    updated.title = Arc::from(p.title.as_str());
                }
            }
        }
    }

    // 3. Group
    if let Some(ref op) = patch.group {
        match op {
            OverrideFieldOp::Set(s) => updated.group = Arc::from(s.as_str()),
            OverrideFieldOp::Clear => updated.group = Arc::from(""),
            OverrideFieldOp::Reset => {
                if let Some(p) = provenance.and_then(|pr| pr.provider.as_ref()) {
                    updated.group = Arc::from(p.group.as_str());
                }
            }
        }
    }

    // 4. Logo
    if let Some(ref op) = patch.logo {
        match op {
            OverrideFieldOp::Set(s) => updated.logo = Arc::from(s.as_str()),
            OverrideFieldOp::Clear => updated.logo = Arc::from(""),
            OverrideFieldOp::Reset => {
                if let Some(p) = provenance.and_then(|pr| pr.provider.as_ref()) {
                    updated.logo = Arc::from(p.logo.as_deref().unwrap_or(""));
                }
            }
        }
    }

    // 5. EPG channel ID
    if let Some(ref op) = patch.epg_channel_id {
        match op {
            OverrideFieldOp::Set(s) => {
                let trimmed = s.trim();
                updated.epg_channel_id = if trimmed.is_empty() { None } else { Some(Arc::from(trimmed)) };
            }
            OverrideFieldOp::Clear => updated.epg_channel_id = None,
            OverrideFieldOp::Reset => {
                if let Some(p) = provenance.and_then(|pr| pr.provider.as_ref()) {
                    updated.epg_channel_id = p.epg_channel_id.as_ref().map(|s| Arc::from(s.as_str()));
                }
            }
        }
    }

    // 6. Rating
    if let Some(ref op) = patch.rating {
        match op {
            OverrideFieldOp::Set(r) => updated.rating = *r,
            OverrideFieldOp::Clear => updated.rating = 0.0,
            OverrideFieldOp::Reset => updated.rating = 0.0,
        }
    }

    // 7. Explorer metadata
    let mut meta = updated.explorer_meta.take().unwrap_or_default();
    if let Some(ref op) = patch.chno {
        meta.chno = match op {
            OverrideFieldOp::Set(n) => Some(*n).filter(|n| *n > 0),
            OverrideFieldOp::Clear => None,
            OverrideFieldOp::Reset => provenance.and_then(|pr| pr.provider.as_ref()).map(|p| p.chno).filter(|n| *n > 0),
        };
    }
    meta.override_state =
        UiOverrideState { edited: has_remaining_deltas, pending: true, ambiguous: meta.override_state.ambiguous };
    updated.explorer_meta = Some(meta);

    updated
}

#[must_use]
pub fn patch_playlist_item(
    item: &UiPlaylistItem,
    form: &PlaylistOverrideDto,
    deltas: &FieldDeltas,
    provenance: Option<&OverrideProvenanceDto>,
    has_remaining_deltas: bool,
) -> UiPlaylistItem {
    let mut patch = ItemOverridePatch::default();

    macro_rules! handle_string_field {
        ($field:ident) => {
            if deltas.reset_fields.contains(stringify!($field)) {
                patch.$field = Some(OverrideFieldOp::Reset);
            } else if deltas.clear_fields.contains(stringify!($field)) {
                patch.$field = Some(OverrideFieldOp::Clear);
            } else if !form.$field.is_empty() {
                patch.$field = Some(OverrideFieldOp::Set(form.$field.clone()));
            }
        };
    }

    handle_string_field!(name);
    handle_string_field!(title);
    handle_string_field!(group);
    handle_string_field!(logo);

    if deltas.reset_fields.contains("epg_channel_id") {
        patch.epg_channel_id = Some(OverrideFieldOp::Reset);
    } else if deltas.clear_fields.contains("epg_channel_id") {
        patch.epg_channel_id = Some(OverrideFieldOp::Clear);
    } else if !form.epg_channel_id.trim().is_empty() {
        patch.epg_channel_id = Some(OverrideFieldOp::Set(form.epg_channel_id.trim().to_string()));
    }

    if deltas.reset_fields.contains("rating") {
        patch.rating = Some(OverrideFieldOp::Reset);
    } else if deltas.clear_fields.contains("rating") {
        patch.rating = Some(OverrideFieldOp::Clear);
    } else if let Some(rating) = parse_localized_float(form.rating.trim()) {
        patch.rating = Some(OverrideFieldOp::Set(rating));
    }

    patch_playlist_item_from_patch(item, &patch, provenance, has_remaining_deltas)
}

#[must_use]
pub fn replace_item_in_categories(
    categories: &UiPlaylistCategories,
    patched_item: UiPlaylistItem,
) -> UiPlaylistCategories {
    let target_vid = patched_item.virtual_id;
    let cluster = patched_item.xtream_cluster;

    let update_group_list = |groups: Option<&Vec<Rc<UiPlaylistGroup>>>| -> Option<Vec<Rc<UiPlaylistGroup>>> {
        let groups = groups?;
        let old_group_idx = groups.iter().position(|g| g.channels.iter().any(|ch| ch.virtual_id == target_vid));
        let Some(old_idx) = old_group_idx else {
            return Some(groups.clone());
        };

        let old_group = &groups[old_idx];
        let target_title = &patched_item.group;

        if old_group.title.as_ref() == target_title.as_ref() {
            // Group unchanged: replace channel in place in old_group
            let mut new_groups = Vec::with_capacity(groups.len());
            for (idx, g) in groups.iter().enumerate() {
                if idx == old_idx {
                    let mut new_channels = Vec::with_capacity(g.channels.len());
                    for ch in &g.channels {
                        if ch.virtual_id == target_vid {
                            new_channels.push(Rc::new(patched_item.clone()));
                        } else {
                            new_channels.push(ch.clone());
                        }
                    }
                    new_groups.push(Rc::new(UiPlaylistGroup {
                        id: g.id,
                        title: g.title.clone(),
                        channels: new_channels,
                        xtream_cluster: g.xtream_cluster,
                    }));
                } else {
                    new_groups.push(g.clone());
                }
            }
            Some(new_groups)
        } else {
            // Group changed: move item from old_group to dest_group
            let dest_group_idx = groups.iter().position(|g| g.title.as_ref() == target_title.as_ref());
            let mut new_groups = Vec::with_capacity(groups.len() + 1);

            for (idx, g) in groups.iter().enumerate() {
                if idx == old_idx {
                    let filtered_channels: Vec<Rc<UiPlaylistItem>> =
                        g.channels.iter().filter(|ch| ch.virtual_id != target_vid).cloned().collect();
                    if !filtered_channels.is_empty() {
                        new_groups.push(Rc::new(UiPlaylistGroup {
                            id: g.id,
                            title: g.title.clone(),
                            channels: filtered_channels,
                            xtream_cluster: g.xtream_cluster,
                        }));
                    }
                } else if Some(idx) == dest_group_idx {
                    let mut new_channels = Vec::with_capacity(g.channels.len() + 1);
                    new_channels.extend(g.channels.iter().cloned());
                    new_channels.push(Rc::new(patched_item.clone()));
                    new_groups.push(Rc::new(UiPlaylistGroup {
                        id: g.id,
                        title: g.title.clone(),
                        channels: new_channels,
                        xtream_cluster: g.xtream_cluster,
                    }));
                } else {
                    new_groups.push(g.clone());
                }
            }

            if dest_group_idx.is_none() {
                let new_id = if patched_item.category_id != 0 {
                    patched_item.category_id
                } else {
                    groups.iter().map(|g| g.id).max().unwrap_or(0).saturating_add(1)
                };
                new_groups.push(Rc::new(UiPlaylistGroup {
                    id: new_id,
                    title: patched_item.group.clone(),
                    channels: vec![Rc::new(patched_item)],
                    xtream_cluster: cluster,
                }));
            }

            Some(new_groups)
        }
    };

    match cluster {
        XtreamCluster::Live => UiPlaylistCategories {
            live: update_group_list(categories.live.as_ref()),
            vod: categories.vod.clone(),
            series: categories.series.clone(),
        },
        XtreamCluster::Video => UiPlaylistCategories {
            live: categories.live.clone(),
            vod: update_group_list(categories.vod.as_ref()),
            series: categories.series.clone(),
        },
        XtreamCluster::Series => UiPlaylistCategories {
            live: categories.live.clone(),
            vod: categories.vod.clone(),
            series: update_group_list(categories.series.as_ref()),
        },
    }
}

/// Bulk patches keyed by cluster and virtual id; virtual ids are only unique within a cluster.
pub type BulkPatchMap = HashMap<(XtreamCluster, u32), UiPlaylistItem>;

/// Builds the bulk patch map from the saved items.
#[must_use]
pub fn bulk_patch_map(items: Vec<UiPlaylistItem>) -> BulkPatchMap {
    items.into_iter().map(|it| ((it.xtream_cluster, it.virtual_id), it)).collect()
}

/// Applies bulk patches to all clusters and moves patched items into their target groups.
#[must_use]
pub fn apply_bulk_patches_to_categories(
    categories: &UiPlaylistCategories,
    patch_map: &BulkPatchMap,
) -> UiPlaylistCategories {
    if patch_map.is_empty() {
        return categories.clone();
    }

    let update_group_list =
        |groups: Option<&Vec<Rc<UiPlaylistGroup>>>, cluster: XtreamCluster| -> Option<Vec<Rc<UiPlaylistGroup>>> {
            let groups = groups?;
            let relevant_patches: Vec<&UiPlaylistItem> =
                patch_map.values().filter(|it| it.xtream_cluster == cluster).collect();
            // Virtual ids are only unique within a cluster, so patches of another cluster never apply here.
            if relevant_patches.is_empty() {
                return Some(groups.clone());
            }

            let mut new_groups = Vec::with_capacity(groups.len());
            let mut handled_dest_groups: HashSet<Arc<str>> = HashSet::new();

            for g in groups {
                let mut new_channels = Vec::with_capacity(g.channels.len());
                let mut modified = false;

                for ch in &g.channels {
                    if let Some(patched) = patch_map.get(&(cluster, ch.virtual_id)) {
                        modified = true;
                        if patched.group.as_ref() == g.title.as_ref() {
                            new_channels.push(Rc::new(patched.clone()));
                        }
                    } else {
                        new_channels.push(ch.clone());
                    }
                }

                for patched in &relevant_patches {
                    if patched.group.as_ref() == g.title.as_ref()
                        && !g.channels.iter().any(|ch| ch.virtual_id == patched.virtual_id)
                    {
                        modified = true;
                        new_channels.push(Rc::new((*patched).clone()));
                    }
                }

                if !new_channels.is_empty() {
                    if modified {
                        new_groups.push(Rc::new(UiPlaylistGroup {
                            id: g.id,
                            title: g.title.clone(),
                            channels: new_channels,
                            xtream_cluster: g.xtream_cluster,
                        }));
                    } else {
                        new_groups.push(g.clone());
                    }
                    handled_dest_groups.insert(g.title.clone());
                }
            }

            for patched in relevant_patches {
                if !handled_dest_groups.contains(&patched.group) {
                    let items_for_new_grp: Vec<Rc<UiPlaylistItem>> = patch_map
                        .values()
                        .filter(|it| it.xtream_cluster == cluster && it.group == patched.group)
                        .map(|it| Rc::new(it.clone()))
                        .collect();
                    let new_id = if patched.category_id != 0 {
                        patched.category_id
                    } else {
                        groups.iter().map(|g| g.id).max().unwrap_or(0).saturating_add(1)
                    };
                    new_groups.push(Rc::new(UiPlaylistGroup {
                        id: new_id,
                        title: patched.group.clone(),
                        channels: items_for_new_grp,
                        xtream_cluster: cluster,
                    }));
                    handled_dest_groups.insert(patched.group.clone());
                }
            }

            Some(new_groups)
        };

    UiPlaylistCategories {
        live: update_group_list(categories.live.as_ref(), XtreamCluster::Live),
        vod: update_group_list(categories.vod.as_ref(), XtreamCluster::Video),
        series: update_group_list(categories.series.as_ref(), XtreamCluster::Series),
    }
}

fn get_field_from_item_fields(fields: &ItemFieldsDto, field: &str) -> Option<String> {
    match field {
        "name" => Some(fields.name.clone()),
        "title" => Some(fields.title.clone()),
        "group" => Some(fields.group.clone()),
        "logo" => fields.logo.clone(),
        "logo_small" => fields.logo_small.clone(),
        "epg_channel_id" => fields.epg_channel_id.clone(),
        "chno" => (fields.chno > 0).then(|| fields.chno.to_string()),
        "parent_code" => Some(fields.parent_code.clone()),
        "audio_track" => Some(fields.audio_track.clone()),
        "time_shift" => Some(fields.time_shift.clone()),
        "rec" => Some(fields.rec.clone()),
        _ => None,
    }
}

/// Layer a `Reset` operation falls back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackSource {
    InputOverride,
    Provider,
    Processed,
    Base,
}

/// Result of a `Reset` for one field: the layer that provides the value and the value itself.
/// `None` means the layer clears the field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackInfo {
    pub source: FallbackSource,
    pub value: Option<String>,
}

#[must_use]
pub fn reset_fallback(scope: OverrideScopeDto, field: &str, provenance: &OverrideProvenanceDto) -> FallbackInfo {
    let from_map = |map: &BTreeMap<String, serde_json::Value>| -> Option<Option<String>> {
        map.get(field).map(|val| {
            if val.is_null() {
                None
            } else {
                Some(val.as_str().map_or_else(|| val.to_string(), ToString::to_string))
            }
        })
    };

    let from_fields = |fields: &ItemFieldsDto| get_field_from_item_fields(fields, field);

    match scope {
        OverrideScopeDto::Target => {
            if let Some(ref io) = provenance.input_override {
                if let Some(value) = from_map(io) {
                    return FallbackInfo { source: FallbackSource::InputOverride, value };
                }
            }
            if let Some(value) = provenance.provider.as_ref().and_then(from_fields) {
                return FallbackInfo { source: FallbackSource::Provider, value: Some(value) };
            }
            if let Some(value) = provenance.processed.as_ref().and_then(from_fields) {
                return FallbackInfo { source: FallbackSource::Processed, value: Some(value) };
            }
            FallbackInfo { source: FallbackSource::Base, value: None }
        }
        OverrideScopeDto::Input => {
            if let Some(value) = provenance.provider.as_ref().and_then(from_fields) {
                FallbackInfo { source: FallbackSource::Provider, value: Some(value) }
            } else {
                FallbackInfo { source: FallbackSource::Provider, value: None }
            }
        }
    }
}

/// Localized description of a reset target.
#[must_use]
pub fn fallback_label(translate: &YewI18n, info: &FallbackInfo) -> String {
    match (info.source, info.value.as_deref()) {
        (FallbackSource::InputOverride, Some(value)) => {
            translate.t("LABEL.EXPLORER_FALLBACK_INPUT_OVERRIDE").replace("{value}", value)
        }
        (FallbackSource::InputOverride, None) => translate.t("LABEL.EXPLORER_FALLBACK_INPUT_OVERRIDE_CLEARED"),
        (FallbackSource::Provider, Some(value)) => {
            translate.t("LABEL.EXPLORER_FALLBACK_PROVIDER").replace("{value}", value)
        }
        (FallbackSource::Provider, None) => translate.t("LABEL.EXPLORER_FALLBACK_PROVIDER_NONE"),
        (FallbackSource::Processed, Some(value)) => {
            translate.t("LABEL.EXPLORER_FALLBACK_PROCESSED").replace("{value}", value)
        }
        (FallbackSource::Processed, None) | (FallbackSource::Base, _) => translate.t("LABEL.EXPLORER_FALLBACK_BASE"),
    }
}

/// Layer the currently effective value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvenanceSource {
    TargetOverride,
    InputOverride,
    Provider,
    ProcessedFallback,
    None,
}

impl ProvenanceSource {
    #[must_use]
    pub const fn label_key(self) -> &'static str {
        match self {
            Self::TargetOverride => "LABEL.EXPLORER_PROVENANCE_TARGET_OVERRIDE",
            Self::InputOverride => "LABEL.EXPLORER_PROVENANCE_INPUT_OVERRIDE",
            Self::Provider => "LABEL.EXPLORER_PROVENANCE_PROVIDER",
            Self::ProcessedFallback => "LABEL.EXPLORER_PROVENANCE_PROCESSED",
            Self::None => "LABEL.EXPLORER_PROVENANCE_NONE",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldProvenanceInfo {
    /// `None` when the layer clears the value.
    pub effective_value: Option<String>,
    pub source: ProvenanceSource,
}

#[must_use]
pub fn field_provenance(field_name: &str, provenance: &OverrideProvenanceDto) -> FieldProvenanceInfo {
    let from_map = |map: &BTreeMap<String, serde_json::Value>, source: ProvenanceSource| {
        map.get(field_name).map(|val| FieldProvenanceInfo {
            effective_value: if val.is_null() {
                None
            } else {
                Some(val.as_str().map_or_else(|| val.to_string(), ToString::to_string))
            },
            source,
        })
    };
    let from_fields = |fields: &ItemFieldsDto, source: ProvenanceSource| {
        get_field_from_item_fields(fields, field_name)
            .map(|value| FieldProvenanceInfo { effective_value: Some(value), source })
    };

    provenance
        .target_override
        .as_ref()
        .and_then(|map| from_map(map, ProvenanceSource::TargetOverride))
        .or_else(|| provenance.input_override.as_ref().and_then(|map| from_map(map, ProvenanceSource::InputOverride)))
        .or_else(|| provenance.provider.as_ref().and_then(|fields| from_fields(fields, ProvenanceSource::Provider)))
        .or_else(|| {
            provenance.processed.as_ref().and_then(|fields| from_fields(fields, ProvenanceSource::ProcessedFallback))
        })
        .unwrap_or(FieldProvenanceInfo { effective_value: None, source: ProvenanceSource::None })
}

/// Localized `effective value` text; an empty layer renders as the translated "(cleared)".
#[must_use]
pub fn provenance_value_label(translate: &YewI18n, info: &FieldProvenanceInfo) -> String {
    match info.effective_value.as_deref() {
        Some(value) if info.source != ProvenanceSource::None => value.to_string(),
        _ if info.source == ProvenanceSource::None => "-".to_string(),
        _ => translate.t("LABEL.EXPLORER_CLEARED"),
    }
}

#[must_use]
pub fn find_duplicate_chno(
    chno_str: &str,
    current_virtual_id: u32,
    chno_map: &HashMap<u32, Vec<Rc<UiPlaylistItem>>>,
) -> Option<String> {
    let trimmed = chno_str.trim();
    if trimmed.is_empty() {
        return None;
    }
    let target_num = trimmed.parse::<u32>().ok()?;
    if target_num == 0 {
        return None;
    }

    if let Some(items) = chno_map.get(&target_num) {
        for item in items {
            if item.virtual_id == current_virtual_id {
                continue;
            }
            return Some(format!("{} ({})", item.name, item.title));
        }
    }
    None
}

#[must_use]
pub fn build_channel_no_map(categories: Option<&UiPlaylistCategories>) -> HashMap<u32, Vec<Rc<UiPlaylistItem>>> {
    let mut map: HashMap<u32, Vec<Rc<UiPlaylistItem>>> = HashMap::new();
    if let Some(cat) = categories {
        if let Some(ref live_groups) = cat.live {
            for group in live_groups {
                for item in &group.channels {
                    if let Some(chno) = crate::app::components::playlist::explorer::sort::item_chno(item) {
                        map.entry(chno).or_default().push(item.clone());
                    }
                }
            }
        }
    }
    map
}

fn get_unknown_fields(scope: OverrideScopeDto, provenance: Option<&OverrideProvenanceDto>) -> Vec<(String, String)> {
    let mut result = Vec::new();
    let Some(prov) = provenance else {
        return result;
    };
    let map = match scope {
        OverrideScopeDto::Target => prov.target_override.as_ref(),
        OverrideScopeDto::Input => prov.input_override.as_ref(),
    };
    let Some(fields) = map else {
        return result;
    };

    const KNOWN: &[&str] = &[
        "_hint",
        "name",
        "title",
        "group",
        "logo",
        "logo_small",
        "epg_channel_id",
        "chno",
        "parent_code",
        "audio_track",
        "time_shift",
        "rec",
        "tmdb",
        "plot",
        "genre",
        "cast",
        "director",
        "release_date",
        "rating",
    ];

    for (k, v) in fields {
        if !KNOWN.contains(&k.as_str()) {
            result.push((k.clone(), v.to_string()));
        }
    }
    result
}

fn initialize_form_data(
    item: &UiPlaylistItem,
    provenance: Option<&OverrideProvenanceDto>,
    series_info: Option<&SeriesStreamProperties>,
    scope: OverrideScopeDto,
) -> PlaylistOverrideDto {
    let mut form = PlaylistOverrideDto {
        name: item.name.to_string(),
        title: item.title.to_string(),
        group: item.group.to_string(),
        logo: item.logo.to_string(),
        epg_channel_id: item.epg_channel_id.as_deref().unwrap_or("").to_string(),
        ..Default::default()
    };

    if let Some(chno) = crate::app::components::playlist::explorer::sort::item_chno(item) {
        form.chno = chno.to_string();
    }

    // Numeric fields are prefilled with dot notation: they are parsed, not displayed, so a
    // localized comma would make validation and the patch fail.
    if item.rating > 0.0 {
        form.rating = format!("{:.1}", item.rating);
    }

    if let Some(series) = series_info {
        if let Some(ref p) = series.plot {
            if !p.is_empty() {
                form.plot = p.to_string();
            }
        }
        if let Some(ref genre) = series.genre {
            if !genre.is_empty() {
                form.genre = genre.to_string();
            }
        }
        if !series.cast.is_empty() {
            form.cast = series.cast.to_string();
        }
        if !series.director.is_empty() {
            form.director = series.director.to_string();
        }
        if let Some(ref rel) = series.release_date {
            if !rel.is_empty() {
                form.release_date = rel.to_string();
            }
        }
        if series.rating > 0.0 {
            form.rating = format!("{:.1}", series.rating);
        }
        if let Some(tmdb) = series.tmdb {
            form.tmdb = tmdb.to_string();
        }
    }

    if let Some(prov) = provenance {
        let apply_map = |map: &BTreeMap<String, serde_json::Value>, target: &mut PlaylistOverrideDto| {
            for (k, v) in map {
                match k.as_str() {
                    "name" => {
                        if let Some(s) = v.as_str() {
                            target.name = s.to_string();
                        }
                    }
                    "title" => {
                        if let Some(s) = v.as_str() {
                            target.title = s.to_string();
                        }
                    }
                    "group" => {
                        if let Some(s) = v.as_str() {
                            target.group = s.to_string();
                        }
                    }
                    "logo" => {
                        if let Some(s) = v.as_str() {
                            target.logo = s.to_string();
                        } else if v.is_null() {
                            target.logo.clear();
                        }
                    }
                    "logo_small" => {
                        if let Some(s) = v.as_str() {
                            target.logo_small = s.to_string();
                        } else if v.is_null() {
                            target.logo_small.clear();
                        }
                    }
                    "epg_channel_id" => {
                        if let Some(s) = v.as_str() {
                            target.epg_channel_id = s.to_string();
                        } else if v.is_null() {
                            target.epg_channel_id.clear();
                        }
                    }
                    "chno" => {
                        if let Some(n) = v.as_u64() {
                            target.chno = n.to_string();
                        } else if v.is_null() {
                            target.chno.clear();
                        }
                    }
                    "parent_code" => {
                        if let Some(s) = v.as_str() {
                            target.parent_code = s.to_string();
                        } else if v.is_null() {
                            target.parent_code.clear();
                        }
                    }
                    "audio_track" => {
                        if let Some(s) = v.as_str() {
                            target.audio_track = s.to_string();
                        } else if v.is_null() {
                            target.audio_track.clear();
                        }
                    }
                    "time_shift" => {
                        if let Some(s) = v.as_str() {
                            target.time_shift = s.to_string();
                        } else if v.is_null() {
                            target.time_shift.clear();
                        }
                    }
                    "rec" => {
                        if let Some(s) = v.as_str() {
                            target.rec = s.to_string();
                        } else if v.is_null() {
                            target.rec.clear();
                        }
                    }
                    "tmdb" => {
                        if let Some(n) = v.as_u64() {
                            target.tmdb = n.to_string();
                        } else if v.is_null() {
                            target.tmdb.clear();
                        }
                    }
                    "plot" => {
                        if let Some(s) = v.as_str() {
                            target.plot = s.to_string();
                        } else if v.is_null() {
                            target.plot.clear();
                        }
                    }
                    "genre" => {
                        if let Some(s) = v.as_str() {
                            target.genre = s.to_string();
                        } else if v.is_null() {
                            target.genre.clear();
                        }
                    }
                    "cast" => {
                        if let Some(s) = v.as_str() {
                            target.cast = s.to_string();
                        } else if v.is_null() {
                            target.cast.clear();
                        }
                    }
                    "director" => {
                        if let Some(s) = v.as_str() {
                            target.director = s.to_string();
                        } else if v.is_null() {
                            target.director.clear();
                        }
                    }
                    "release_date" => {
                        if let Some(s) = v.as_str() {
                            target.release_date = s.to_string();
                        } else if v.is_null() {
                            target.release_date.clear();
                        }
                    }
                    "rating" => {
                        if let Some(r) = v.as_f64() {
                            target.rating = r.to_string();
                        } else if v.is_null() {
                            target.rating.clear();
                        }
                    }
                    _ => {}
                }
            }
        };

        if let Some(ref io) = prov.input_override {
            apply_map(io, &mut form);
        }
        if scope == OverrideScopeDto::Target {
            if let Some(ref to) = prov.target_override {
                apply_map(to, &mut form);
            }
        }
    }

    form
}

fn is_valid_http_url(s: &str) -> bool { s.starts_with("http://") || s.starts_with("https://") }

fn is_valid_date(s: &str) -> bool { chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok() }

#[derive(Properties, PartialEq, Clone)]
pub struct ExplorerEditPanelProps {
    /// `Id` keeps the prop comparison at a pointer check.
    pub item: Option<Id<UiPlaylistItem>>,
    /// Bumped whenever the catalog behind the panel was replaced.
    #[prop_or_default]
    pub playlist_revision: u32,
    pub active_scope: UseStateHandle<OverrideScopeDto>,
    pub on_close: Callback<()>,
    #[prop_or_default]
    pub on_saved: Option<Callback<UiPlaylistItem>>,
}

#[component]
pub fn ExplorerEditPanel(props: &ExplorerEditPanelProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let explorer_ctx = use_context::<PlaylistExplorerContext>();
    let config_ctx = use_context::<ConfigContext>();

    let Some(ref item) = props.item else {
        return html! {};
    };

    let target_id = explorer_ctx.as_ref().and_then(|ctx| match ctx.playlist_request.as_ref() {
        Some(PlaylistRequest::Target(tid)) => Some(*tid),
        _ => None,
    });

    let is_target_view = target_id.is_some();
    let active_scope = props.active_scope.clone();
    let has_saved = use_state(|| false);
    let can_write = services.auth.has_permission(Permission::PlaylistWrite);

    let provenance = use_state(|| None::<OverrideProvenanceDto>);
    let base_revision = use_state(String::new);
    let initial_form = use_state(PlaylistOverrideDto::default);
    let is_loading = use_state(|| true);
    let load_error = use_state(|| None::<String>);
    let is_saving = use_state(|| false);
    let field_errors = use_state(HashMap::<String, String>::new);
    let deltas = use_state(FieldDeltas::default);
    let also_set_title = use_state(|| false);

    let form_state = use_default_form_reducer!(OverrideEditFormState { form: (*initial_form).clone() });

    let kind = OverrideItemKind::from(item.item_type);

    // Load provenance and file revision
    {
        let services = services.clone();
        let item = item.clone();
        let provenance = provenance.clone();
        let base_revision = base_revision.clone();
        let initial_form = initial_form.clone();
        let is_loading = is_loading.clone();
        let load_error = load_error.clone();
        let form_state = form_state.clone();
        let config_ctx = config_ctx.clone();
        let explorer_ctx = explorer_ctx.clone();
        let also_set_title = also_set_title.clone();
        let deltas = deltas.clone();
        let field_errors = field_errors.clone();
        let translate = translate.clone();

        use_effect_with((*active_scope, item.virtual_id, item.input_name.clone()), move |(scope, _, _)| {
            let services = services.clone();
            let item = item.clone();
            let provenance = provenance.clone();
            let base_revision = base_revision.clone();
            let initial_form = initial_form.clone();
            let is_loading = is_loading.clone();
            let load_error = load_error.clone();
            let form_state = form_state.clone();
            let config_ctx = config_ctx.clone();
            let explorer_ctx = explorer_ctx.clone();
            let deltas = deltas.clone();
            let field_errors = field_errors.clone();
            let translate = translate.clone();
            let scope = *scope;

            is_loading.set(true);
            load_error.set(None);
            // Deltas (reset/clear/remove) and their errors belong to the scope they were made in;
            // carrying them into the other scope would send operations the user never asked for.
            deltas.set(FieldDeltas::default());
            field_errors.set(HashMap::new());

            spawn_local(async move {
                let mut failed = false;

                // 1. Fetch series info if Series
                let mut series_props: Option<SeriesStreamProperties> = None;
                if kind == OverrideItemKind::Series {
                    if let Some(req) = explorer_ctx.as_ref().and_then(|c| c.playlist_request.as_ref()) {
                        series_props = services.playlist.get_series_info(&item, req).await;
                    }
                }

                // 2. Resolve scope name
                let scope_name = match scope {
                    OverrideScopeDto::Target => {
                        let mut name = String::new();
                        if let Some(tid) = target_id {
                            if let Some(cfg) = config_ctx.as_ref().and_then(|c| c.config.as_ref()) {
                                if let Some(n) = target_name_for_id(&cfg.sources, tid, Some(item.input_name.as_ref())) {
                                    name = n;
                                }
                            }
                        }
                        name
                    }
                    OverrideScopeDto::Input => item.input_name.to_string(),
                };

                // 3. Load the file revision. "No override file yet" is a valid state with an
                // empty revision; a failed read is not, and must disable saving.
                if !scope_name.is_empty() {
                    match services.playlist.get_overrides(scope, &scope_name).await {
                        Ok(Some(file)) => {
                            if let Some(ref parse_err) = file.parse_error {
                                failed = true;
                                let message =
                                    translate.t("MESSAGES.PLAYLIST_EXPLORER.PARSE_ERROR").replace("{error}", parse_err);
                                services.toastr.error(message.clone());
                                load_error.set(Some(message));
                            } else if let Some(schema_ver) = file.schema_error {
                                failed = true;
                                let message = translate
                                    .t("MESSAGES.PLAYLIST_EXPLORER.SCHEMA_ERROR")
                                    .replace("{version}", &schema_ver.to_string());
                                services.toastr.error(message.clone());
                                load_error.set(Some(message));
                            } else {
                                base_revision.set(file.revision);
                                let item_ov_id = item_override_id(&item);
                                let mut err_map = HashMap::new();
                                for fe in file.field_errors {
                                    if fe.entry.as_deref() == Some(item_ov_id) || fe.entry.is_none() {
                                        err_map.insert(fe.field, fe.message_key);
                                    }
                                }
                                if !err_map.is_empty() {
                                    field_errors.set(err_map);
                                }
                            }
                        }
                        Ok(None) => base_revision.set(String::new()),
                        Err(err) => {
                            failed = true;
                            let message = translate
                                .t("MESSAGES.PLAYLIST_EXPLORER.LOAD_FAILED")
                                .replace("{error}", &err.to_string());
                            services.toastr.error(message.clone());
                            load_error.set(Some(message));
                        }
                    }
                }

                // 4. Load item provenance
                match services
                    .playlist
                    .get_override_item(item.input_name.as_ref(), kind, item_override_id(&item), target_id)
                    .await
                {
                    Ok(prov_opt) => {
                        let loaded_form = initialize_form_data(&item, prov_opt.as_ref(), series_props.as_ref(), scope);
                        also_set_title.set(loaded_form.name == loaded_form.title);
                        initial_form.set(loaded_form.clone());
                        form_state.dispatch(OverrideEditFormAction::SetAll(loaded_form));
                        provenance.set(prov_opt);
                    }
                    Err(err) => {
                        failed = true;
                        let message =
                            translate.t("MESSAGES.PLAYLIST_EXPLORER.LOAD_FAILED").replace("{error}", &err.to_string());
                        services.toastr.error(message.clone());
                        load_error.set(Some(message));
                    }
                }

                if !failed {
                    load_error.set(None);
                }
                is_loading.set(false);
            });
            || {}
        });
    }

    let is_ambiguous = provenance.as_ref().is_some_and(|p| p.ambiguous);

    // Precompute channel-number -> items map once per catalog revision (use_memo)
    let catalog = explorer_ctx.as_ref().and_then(|ctx| (*ctx.playlist).clone());
    let live_chno_map = use_memo((catalog.map(Id::new), props.playlist_revision), |(catalog, _)| {
        Rc::new(build_channel_no_map(catalog.as_ref().map(|cat| &**cat)))
    });

    // Only recomputed when the channel number actually changed, not on every keystroke.
    let chno_changed = kind == OverrideItemKind::Live && form_state.form.chno != initial_form.chno;
    let duplicate_chno_warning = use_memo(
        (chno_changed, form_state.form.chno.clone(), Id::new(live_chno_map.clone())),
        |(changed, chno, live_chno_map)| {
            if *changed {
                find_duplicate_chno(chno, item.virtual_id, live_chno_map)
            } else {
                None
            }
        },
    );

    // Scope toggle handlers
    let on_scope_select = {
        let active_scope = active_scope.clone();
        Callback::from(move |selection: Rc<Vec<String>>| {
            let scope = if selection.first().map(String::as_str) == Some("input") {
                OverrideScopeDto::Input
            } else {
                OverrideScopeDto::Target
            };
            active_scope.set(scope);
        })
    };

    // Manual update handler
    let handle_update_now = {
        let services = services.clone();
        let translate = translate.clone();
        let config_ctx = config_ctx.clone();
        let item = item.clone();

        Callback::from(move |_: String| {
            let services = services.clone();
            let translate = translate.clone();
            let config_ctx = config_ctx.clone();
            let item = item.clone();

            spawn_local(async move {
                let mut triggered = false;
                if let Some(tid) = target_id {
                    if services.playlist.update_target_by_id(tid).await.is_ok() {
                        triggered = true;
                    }
                } else if let Some(cfg) = config_ctx.as_ref().and_then(|c| c.config.as_ref()) {
                    if let Some(input) = cfg.sources.inputs.iter().find(|i| i.name.as_ref() == item.input_name.as_ref())
                    {
                        if services
                            .playlist
                            .update_input(&[], input.id, InputUpdateAction::Provider(InputRefreshPolicy::NORMAL))
                            .await
                            .is_ok()
                        {
                            triggered = true;
                        }
                    }
                }

                if triggered {
                    services.toastr.success(translate.t("MESSAGES.PLAYLIST_EXPLORER.UPDATE_TRIGGERED"));
                } else {
                    services.toastr.error(translate.t("MESSAGES.PLAYLIST_EXPLORER.UPDATE_FAILED"));
                }
            });
        })
    };

    // Save handler
    let handle_save = {
        let services = services.clone();
        let translate = translate.clone();
        let form_state = form_state.clone();
        let initial_form = initial_form.clone();
        let deltas = deltas.clone();
        let active_scope = *active_scope;
        let base_revision = base_revision.clone();
        let field_errors = field_errors.clone();
        let item = item.clone();
        let config_ctx = config_ctx.clone();
        let provenance = provenance.clone();
        let has_saved = has_saved.clone();
        let is_saving = is_saving.clone();
        let load_error = load_error.clone();
        let on_saved = props.on_saved.clone();

        Callback::from(move |_: String| {
            // A second click while the first request is in flight would hit the revision the
            // first one just wrote.
            if *is_saving || !services.auth.has_permission(Permission::PlaylistWrite) {
                return;
            }

            let services = services.clone();
            let translate = translate.clone();
            let form = form_state.form.clone();
            let initial = (*initial_form).clone();
            let deltas_val = (*deltas).clone();
            let base_rev = (*base_revision).clone();
            let field_errors = field_errors.clone();
            let is_saving = is_saving.clone();
            let load_error = load_error.clone();
            let item = item.clone();
            let provenance = provenance.clone();
            let base_revision = base_revision.clone();
            let has_saved = has_saved.clone();
            let initial_form = initial_form.clone();
            let deltas = deltas.clone();
            let form_state = form_state.clone();
            let on_saved = on_saved.clone();

            // Only fields the patch actually touches are validated: a provider value the user
            // never edited (say a relative logo path) must not block an unrelated save.
            macro_rules! touched {
                ($field:ident) => {
                    form.$field != initial.$field
                        || deltas_val.reset_fields.contains(stringify!($field))
                        || deltas_val.clear_fields.contains(stringify!($field))
                };
            }

            let mut errors = HashMap::new();
            if touched!(name) && form.name.trim().is_empty() {
                errors.insert("name".to_string(), "error.invalid_name".to_string());
            }
            if touched!(title) && form.title.trim().is_empty() {
                errors.insert("title".to_string(), "error.invalid_title".to_string());
            }
            if touched!(group) && form.group.trim().is_empty() {
                errors.insert("group".to_string(), "error.invalid_group".to_string());
            }
            if touched!(logo) && !form.logo.trim().is_empty() && !is_valid_http_url(form.logo.trim()) {
                errors.insert("logo".to_string(), "error.invalid_url".to_string());
            }

            match kind {
                OverrideItemKind::Live => {
                    if touched!(logo_small)
                        && !form.logo_small.trim().is_empty()
                        && !is_valid_http_url(form.logo_small.trim())
                    {
                        errors.insert("logo_small".to_string(), "error.invalid_url".to_string());
                    }
                    if touched!(chno) && !form.chno.trim().is_empty() {
                        if let Ok(num) = form.chno.trim().parse::<u32>() {
                            if !(1..=999_999).contains(&num) {
                                errors.insert("chno".to_string(), "error.invalid_channel_no".to_string());
                            }
                        } else {
                            errors.insert("chno".to_string(), "error.invalid_channel_no".to_string());
                        }
                    }
                }
                OverrideItemKind::Vod | OverrideItemKind::Series => {
                    if touched!(tmdb) && !form.tmdb.trim().is_empty() {
                        if let Ok(num) = form.tmdb.trim().parse::<u32>() {
                            if num == 0 {
                                errors.insert("tmdb".to_string(), "error.invalid_tmdb".to_string());
                            }
                        } else {
                            errors.insert("tmdb".to_string(), "error.invalid_tmdb".to_string());
                        }
                    }
                    if touched!(release_date)
                        && !form.release_date.trim().is_empty()
                        && !is_valid_date(form.release_date.trim())
                    {
                        errors.insert("release_date".to_string(), "error.invalid_date".to_string());
                    }
                    if touched!(rating) && !form.rating.trim().is_empty() {
                        match parse_localized_float(form.rating.trim()) {
                            Some(rating) if (0.0..=10.0).contains(&rating) => {}
                            _ => {
                                errors.insert("rating".to_string(), "error.invalid_rating".to_string());
                            }
                        }
                    }
                }
            }

            if !errors.is_empty() {
                field_errors.set(errors);
                return;
            }
            field_errors.set(HashMap::new());

            let patch = build_item_patch(&initial, &form, &deltas_val, kind);

            let scope_name = match active_scope {
                OverrideScopeDto::Target => {
                    let mut name = String::new();
                    if let Some(tid) = target_id {
                        if let Some(cfg) = config_ctx.as_ref().and_then(|c| c.config.as_ref()) {
                            if let Some(n) = target_name_for_id(&cfg.sources, tid, Some(item.input_name.as_ref())) {
                                name = n;
                            }
                        }
                    }
                    name
                }
                OverrideScopeDto::Input => item.input_name.to_string(),
            };

            let hint = if let Some(ref prov) = *provenance {
                if let Some(ref p) = prov.provider {
                    Some(format!("{} / {}", p.group, p.title))
                } else {
                    Some(format!("{} / {}", item.group, item.title))
                }
            } else {
                Some(format!("{} / {}", item.group, item.title))
            };

            let patch_dto = ItemPatchDto {
                input: if active_scope == OverrideScopeDto::Target { Some(item.input_name.to_string()) } else { None },
                kind,
                id: item_override_id(&item).to_string(),
                patch,
                hint,
            };

            let save_req = OverrideSaveRequest {
                scope: active_scope,
                name: scope_name,
                base_revision: base_rev,
                patches: vec![patch_dto],
            };

            is_saving.set(true);

            spawn_local(async move {
                let result = services.playlist.save_overrides(&save_req).await;
                is_saving.set(false);

                match result {
                    crate::services::OverrideSaveResult::Ok(rev) => {
                        services.toastr.success(translate.t("MESSAGES.PLAYLIST_EXPLORER.SAVED"));
                        has_saved.set(true);
                        base_revision.set(rev.revision);
                        // The form is now the server state: it becomes the new baseline so the
                        // next save does not resend reset/clear operations.
                        initial_form.set(form.clone());
                        let saved_deltas = (*deltas).clone();
                        deltas.set(FieldDeltas::default());
                        match services
                            .playlist
                            .get_override_item(item.input_name.as_ref(), kind, item_override_id(&item), target_id)
                            .await
                        {
                            Ok(prov_opt) => {
                                let has_remaining_deltas = prov_opt.as_ref().is_some_and(|p| {
                                    p.input_override.as_ref().is_some_and(|m| !m.is_empty())
                                        || p.target_override.as_ref().is_some_and(|m| !m.is_empty())
                                });
                                let patched_item = patch_playlist_item(
                                    &item,
                                    &form,
                                    &saved_deltas,
                                    prov_opt.as_ref(),
                                    has_remaining_deltas,
                                );
                                provenance.set(prov_opt);
                                if let Some(ref cb) = on_saved {
                                    cb.emit(patched_item);
                                }
                            }
                            Err(err) => {
                                services.toastr.error(
                                    translate
                                        .t("MESSAGES.PLAYLIST_EXPLORER.LOAD_FAILED")
                                        .replace("{error}", &err.to_string()),
                                );
                            }
                        }
                    }
                    crate::services::OverrideSaveResult::Conflict(conflict) => {
                        if let Some(err_text) = conflict.error {
                            let msg = translate
                                .t("MESSAGES.PLAYLIST_EXPLORER.SAVE_FAILED_ERROR")
                                .replace("{error}", &err_text);
                            services.toastr.error(msg.clone());
                            load_error.set(Some(msg));
                        } else {
                            services.toastr.error(translate.t("MESSAGES.PLAYLIST_EXPLORER.SAVE_CONFLICT"));
                            // The revision in the conflict is the one the server holds; adopting it and
                            // reloading the form is the only way out of a 409 loop.
                            base_revision.set(conflict.current_revision.clone());
                            deltas.set(FieldDeltas::default());
                            field_errors.set(HashMap::new());
                            match services
                                .playlist
                                .get_override_item(item.input_name.as_ref(), kind, item_override_id(&item), target_id)
                                .await
                            {
                                Ok(reloaded_prov) => {
                                    let reloaded_form =
                                        initialize_form_data(&item, reloaded_prov.as_ref(), None, active_scope);
                                    initial_form.set(reloaded_form.clone());
                                    form_state.dispatch(OverrideEditFormAction::SetAll(reloaded_form));
                                    provenance.set(reloaded_prov);
                                }
                                Err(err) => {
                                    services.toastr.error(
                                        translate
                                            .t("MESSAGES.PLAYLIST_EXPLORER.LOAD_FAILED")
                                            .replace("{error}", &err.to_string()),
                                    );
                                }
                            }
                        }
                    }
                    crate::services::OverrideSaveResult::FieldErrors(errs) => {
                        let mut map = HashMap::new();
                        let mut unmapped = Vec::new();
                        for err in errs {
                            if RENDERED_FIELDS.contains(&err.field.as_str()) {
                                map.insert(err.field, err.message_key);
                            } else {
                                unmapped.push(format!("{}: {}", err.field, translate.t(&err.message_key)));
                            }
                        }
                        field_errors.set(map);
                        if !unmapped.is_empty() {
                            // Fields this panel does not render must still be visible somewhere.
                            services.toastr.error(
                                translate
                                    .t("MESSAGES.PLAYLIST_EXPLORER.UNMAPPED_FIELD_ERRORS")
                                    .replace("{errors}", &unmapped.join(", ")),
                            );
                        }
                    }
                    crate::services::OverrideSaveResult::Err(err_msg) => {
                        services.toastr.error(err_msg);
                    }
                }
            });
        })
    };

    let on_close = {
        let close_cb = props.on_close.clone();
        Callback::from(move |_: String| close_cb.emit(()))
    };

    let unknown_fields = get_unknown_fields(*active_scope, provenance.as_ref());

    let also_set_title_cb = {
        let also_set_title = also_set_title.clone();
        Callback::from(move |value: bool| also_set_title.set(value))
    };
    // Same markup as `edit_field_bool!`: ToggleSwitch followed by the standard field label.
    let name_also_set_title = html! {
        <div class="tp__form-field tp__form-field__bool">
            <crate::app::components::ToggleSwitch
                value={*also_set_title}
                readonly={false}
                on_change={also_set_title_cb}
            />
            <crate::app::components::FieldLabel
                label={translate.t("LABEL.EXPLORER_ALSO_SET_TITLE")}
                field_id={"PLAYLIST_OVERRIDE.ALSO_SET_TITLE".to_string()}
                hint_key={Some("PLAYLIST_OVERRIDE.ALSO_SET_TITLE".to_string())}
            />
        </div>
    };
    let also_set_title_val = *also_set_title;

    let is_prov_logo = provenance.as_ref().and_then(|p| p.provider.as_ref()).and_then(|prov| prov.logo.as_deref())
        == Some(&form_state.form.logo);
    let logo_opts = FieldRowOptions {
        custom_input: None,
        extra_content: Some(html! {
            <ImagePreview
                url={form_state.form.logo.clone()}
                is_provider_value={is_prov_logo}
                class={"tp__explorer-logo-preview"}
            />
        }),
    };

    let on_epg_change = {
        let form_state = form_state.clone();
        let deltas = deltas.clone();
        Callback::from(move |val: String| {
            let mut d = (*deltas).clone();
            d.reset_fields.remove("epg_channel_id");
            d.clear_fields.remove("epg_channel_id");
            deltas.set(d);
            form_state.dispatch(OverrideEditFormAction::EpgChannelId(val));
        })
    };
    let epg_opts = FieldRowOptions {
        custom_input: Some(html! {
            <EpgPicker
                value={form_state.form.epg_channel_id.clone()}
                on_select={on_epg_change.clone()}
                target_id={target_id}
                input={Some(item.input_name.to_string())}
                field_id={Some("PLAYLIST_OVERRIDE.EPG_CHANNEL_ID".to_string())}
                label={Some(translate.t("LABEL.EPG_CHANNEL_ID"))}
            />
        }),
        extra_content: None,
    };

    let is_prov_logo_small =
        provenance.as_ref().and_then(|p| p.provider.as_ref()).and_then(|prov| prov.logo_small.as_deref())
            == Some(&form_state.form.logo_small);
    let logo_small_opts = FieldRowOptions {
        custom_input: None,
        extra_content: Some(html! {
            <ImagePreview
                url={form_state.form.logo_small.clone()}
                is_provider_value={is_prov_logo_small}
                class={"tp__explorer-logo-preview"}
            />
        }),
    };

    html! {
            <div class="det tp__explorer-edit-panel">
                <div class="hero">
                    <IconButton
                        name="close_edit"
                        icon="Back"
                        class="tp__explorer-panel-back"
                        aria_label={Some(translate.t("LABEL.BACK"))}
                        onclick={let on_close = props.on_close.clone(); Callback::from(move |(_name, _): (String, MouseEvent)| on_close.emit(()))}
                    />
                    <b>{ item.title.clone() }</b>
                </div>
                <div class="in">
                    <h2>{ translate.t("LABEL.EXPLORER_EDIT_TITLE") }</h2>
                    <div class="meta">
                        { item_meta_label(&translate, item) }
                    </div>

                    <div class="scope">
                        {
                            if is_target_view {
                                html! {
    <RadioButtonGroup
                                        options={Rc::new(vec!["input".to_string(), "target".to_string()])}
                                        labels={Some(Rc::new(vec![translate.t("LABEL.EXPLORER_SCOPE_INPUT"), translate.t("LABEL.EXPLORER_SCOPE_TARGET")]))}
                                        selected={Rc::new(vec![if *active_scope == OverrideScopeDto::Input { "input" } else { "target" }.to_string()])}
                                        on_select={on_scope_select}
                                    />
                                }
                            } else {
                                html! {
                                    <span class="on">
                                        { translate.t("LABEL.EXPLORER_SCOPE_INPUT") }
                                    </span>
                                }
                            }
                        }
                    </div>

                    {
                        if is_ambiguous {
                            html! {
                                <div class="tp__explorer-notice warning">
                                    { translate.t("LABEL.EXPLORER_AMBIGUOUS_NOTICE") }
                                </div>
                            }
                        } else {
                            html! {}
                        }
                    }

                    {
                        if let Some(ref err) = *load_error {
                            html! {
                                <div class="tp__explorer-notice warning">
                                    { err.clone() }
                                </div>
                            }
                        } else {
                            html! {}
                        }
                    }

                    <div class="tp__explorer-notice info">
                        {
                            if *has_saved {
                                translate.t("LABEL.EXPLORER_MAPPER_NOTICE_SAVED")
                            } else {
                                translate.t("LABEL.EXPLORER_MAPPER_NOTICE")
                            }
                        }
                    </div>

                    {
                        if let Some(ref prov) = *provenance {
                            if prov.pending_per_target.is_empty() {
                                html! {}
                            } else {
                                html! {
                                    <div class="tp__explorer-pending-list">
                                        { for prov.pending_per_target.iter().map(|(target, is_pending)| html! {
                                            <span class={format!("chip {}", if *is_pending { "on" } else { "" })}>
                                                { format!("{}: {}", target, if *is_pending { translate.t("LABEL.EXPLORER_PENDING") } else { translate.t("LABEL.EXPLORER_UP_TO_DATE") }) }
                                            </span>
                                        })}
                                    </div>
                                }
                            }
                        } else {
                            html! {}
                        }
                    }

                    <div class="tp__explorer-fields-scroll">
                        {
                            render_field_row(
                                "name",
                                "LABEL.NAME",
                                &form_state.form.name,
                                &initial_form.name,
                                *active_scope,
                                provenance.as_ref(),
                                &deltas,
                                &field_errors,
                                false,
                                {
                                    let form_state = form_state.clone();
                                    let deltas = deltas.clone();
                                    Callback::from(move |val: String| {
                                        let mut d = (*deltas).clone();
                                        d.reset_fields.remove("name");
                                        d.clear_fields.remove("name");
                                        if also_set_title_val {
                                            d.reset_fields.remove("title");
                                            d.clear_fields.remove("title");
                                            form_state.dispatch(OverrideEditFormAction::Title(val.clone()));
                                        }
                                        deltas.set(d);
                                        form_state.dispatch(OverrideEditFormAction::Name(val));
                                    })
                                },
                                {
                                    let form_state = form_state.clone();
                                    let deltas = deltas.clone();
                                    let fallback = provenance.as_ref().map_or_else(String::new, |p| reset_fallback(*active_scope, "name", p).value.unwrap_or_default());
                                    Callback::from(move |_| {
                                        let mut d = (*deltas).clone();
                                        d.reset_fields.insert("name".to_string());
                                        d.clear_fields.remove("name");
                                        deltas.set(d);
                                        form_state.dispatch(OverrideEditFormAction::Name(fallback.clone()));
                                    })
                                },
                                None,
                                &translate,
                                Some(FieldRowOptions {
                                    custom_input: None,
                                    extra_content: Some(name_also_set_title),
                                }),
                            )
                        }

                        { render_field_row(
                            "title",
                            "LABEL.TITLE",
                            &form_state.form.title,
                            &initial_form.title,
                            *active_scope,
                            provenance.as_ref(),
                            &deltas,
                            &field_errors,
                            false,
                            {
                                let form_state = form_state.clone();
                                let deltas = deltas.clone();
                                Callback::from(move |val: String| {
                                    let mut d = (*deltas).clone();
                                    d.reset_fields.remove("title");
                                    d.clear_fields.remove("title");
                                    deltas.set(d);
                                    form_state.dispatch(OverrideEditFormAction::Title(val));
                                })
                            },
                            {
                                let form_state = form_state.clone();
                                let deltas = deltas.clone();
                                let fallback = provenance.as_ref().map_or_else(String::new, |p| reset_fallback(*active_scope, "title", p).value.unwrap_or_default());
                                Callback::from(move |_| {
                                    let mut d = (*deltas).clone();
                                    d.reset_fields.insert("title".to_string());
                                    d.clear_fields.remove("title");
                                    deltas.set(d);
                                    form_state.dispatch(OverrideEditFormAction::Title(fallback.clone()));
                                })
                            },
                            None,
                            &translate,
                            None,
                        ) }

                        {
                            if kind == OverrideItemKind::Live {
                                html! {
                                    <div class="tp__explorer-hint">
                                        { translate.t("LABEL.EXPLORER_NAME_VS_TITLE_NOTE") }
                                    </div>
                                }
                            } else {
                                html! {}
                            }
                        }

                        { render_field_row(
                            "group",
                            "LABEL.GROUP",
                            &form_state.form.group,
                            &initial_form.group,
                            *active_scope,
                            provenance.as_ref(),
                            &deltas,
                            &field_errors,
                            false,
                            {
                                let form_state = form_state.clone();
                                let deltas = deltas.clone();
                                Callback::from(move |val: String| {
                                    let mut d = (*deltas).clone();
                                    d.reset_fields.remove("group");
                                    d.clear_fields.remove("group");
                                    deltas.set(d);
                                    form_state.dispatch(OverrideEditFormAction::Group(val));
                                })
                            },
                            {
                                let form_state = form_state.clone();
                                let deltas = deltas.clone();
                                let fallback = provenance.as_ref().map_or_else(String::new, |p| reset_fallback(*active_scope, "group", p).value.unwrap_or_default());
                                Callback::from(move |_| {
                                    let mut d = (*deltas).clone();
                                    d.reset_fields.insert("group".to_string());
                                    d.clear_fields.remove("group");
                                    deltas.set(d);
                                    form_state.dispatch(OverrideEditFormAction::Group(fallback.clone()));
                                })
                            },
                            None,
                            &translate,
                            None,
                        ) }

                        {
                            render_field_row(
                                "logo",
                                "LABEL.LOGO",
                                &form_state.form.logo,
                                &initial_form.logo,
                                *active_scope,
                                provenance.as_ref(),
                                &deltas,
                                &field_errors,
                                true,
                                {
                                    let form_state = form_state.clone();
                                    let deltas = deltas.clone();
                                    Callback::from(move |val: String| {
                                        let mut d = (*deltas).clone();
                                        d.reset_fields.remove("logo");
                                        d.clear_fields.remove("logo");
                                        deltas.set(d);
                                        form_state.dispatch(OverrideEditFormAction::Logo(val));
                                    })
                                },
                                {
                                    let form_state = form_state.clone();
                                    let deltas = deltas.clone();
                                    let fallback = provenance.as_ref().map_or_else(String::new, |p| reset_fallback(*active_scope, "logo", p).value.unwrap_or_default());
                                    Callback::from(move |_| {
                                        let mut d = (*deltas).clone();
                                        d.reset_fields.insert("logo".to_string());
                                        d.clear_fields.remove("logo");
                                        deltas.set(d);
                                        form_state.dispatch(OverrideEditFormAction::Logo(fallback.clone()));
                                    })
                                },
                                Some({
                                    let form_state = form_state.clone();
                                    let deltas = deltas.clone();
                                    Callback::from(move |_| {
                                        let mut d = (*deltas).clone();
                                        d.clear_fields.insert("logo".to_string());
                                        d.reset_fields.remove("logo");
                                        deltas.set(d);
                                        form_state.dispatch(OverrideEditFormAction::Logo(String::new()));
                                    })
                                }),
                                &translate,
                                Some(logo_opts),
                            )
                        }

                        // Live specific fields
                        {
                            if kind == OverrideItemKind::Live {
                                html! {
                                    <>
                                        { render_field_row(
                                            "chno",
                                            "LABEL.CHNO",
                                            &form_state.form.chno,
                                            &initial_form.chno,
                                            *active_scope,
                                            provenance.as_ref(),
                                            &deltas,
                                            &field_errors,
                                            true,
                                            {
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                Callback::from(move |val: String| {
                                                    let mut d = (*deltas).clone();
                                                    d.reset_fields.remove("chno");
                                                    d.clear_fields.remove("chno");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Chno(val));
                                                })
                                            },
                                            {
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                let fallback = provenance.as_ref().map_or_else(String::new, |p| reset_fallback(*active_scope, "chno", p).value.unwrap_or_default());
                                                Callback::from(move |_| {
                                                    let mut d = (*deltas).clone();
                                                    d.reset_fields.insert("chno".to_string());
                                                    d.clear_fields.remove("chno");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Chno(fallback.clone()));
                                                })
                                            },
                                            Some({
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                Callback::from(move |_| {
                                                    let mut d = (*deltas).clone();
                                                    d.clear_fields.insert("chno".to_string());
                                                    d.reset_fields.remove("chno");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Chno(String::new()));
                                                })
                                            }),
                                            &translate,
                                            None,
                                        ) }

                                        {
                                            if let Some(conflict_name) = (*duplicate_chno_warning).clone() {
                                                html! {
                                                    <div class="tp__explorer-error-msg">
                                                        { format!("{}: {}", translate.t("LABEL.EXPLORER_DUPLICATE_CHNO_WARN"), conflict_name) }
                                                    </div>
                                                }
                                            } else {
                                                html! {}
                                            }
                                        }

                                        {
                                            render_field_row(
                                                "epg_channel_id",
                                                "LABEL.EPG_CHANNEL_ID",
                                                &form_state.form.epg_channel_id,
                                                &initial_form.epg_channel_id,
                                                *active_scope,
                                                provenance.as_ref(),
                                                &deltas,
                                                &field_errors,
                                                true,
                                                on_epg_change,
                                                {
                                                    let form_state = form_state.clone();
                                                    let deltas = deltas.clone();
                                                    let fallback = provenance.as_ref().map_or_else(String::new, |p| reset_fallback(*active_scope, "epg_channel_id", p).value.unwrap_or_default());
                                                    Callback::from(move |_| {
                                                        let mut d = (*deltas).clone();
                                                        d.reset_fields.insert("epg_channel_id".to_string());
                                                        d.clear_fields.remove("epg_channel_id");
                                                        deltas.set(d);
                                                        form_state.dispatch(OverrideEditFormAction::EpgChannelId(fallback.clone()));
                                                    })
                                                },
                                                Some({
                                                    let form_state = form_state.clone();
                                                    let deltas = deltas.clone();
                                                    Callback::from(move |_| {
                                                        let mut d = (*deltas).clone();
                                                        d.clear_fields.insert("epg_channel_id".to_string());
                                                        d.reset_fields.remove("epg_channel_id");
                                                        deltas.set(d);
                                                        form_state.dispatch(OverrideEditFormAction::EpgChannelId(String::new()));
                                                    })
                                                }),
                                                &translate,
                                                Some(epg_opts),
                                            )
                                        }

                                        {
                                            render_field_row(
                                                "logo_small",
                                                "LABEL.LOGO_SMALL",
                                                &form_state.form.logo_small,
                                                &initial_form.logo_small,
                                                *active_scope,
                                                provenance.as_ref(),
                                                &deltas,
                                                &field_errors,
                                                true,
                                                {
                                                    let form_state = form_state.clone();
                                                    let deltas = deltas.clone();
                                                    Callback::from(move |val: String| {
                                                        let mut d = (*deltas).clone();
                                                        d.reset_fields.remove("logo_small");
                                                        d.clear_fields.remove("logo_small");
                                                        deltas.set(d);
                                                        form_state.dispatch(OverrideEditFormAction::LogoSmall(val));
                                                    })
                                                },
                                                {
                                                    let form_state = form_state.clone();
                                                    let deltas = deltas.clone();
                                                    let fallback = provenance.as_ref().map_or_else(String::new, |p| reset_fallback(*active_scope, "logo_small", p).value.unwrap_or_default());
                                                    Callback::from(move |_| {
                                                        let mut d = (*deltas).clone();
                                                        d.reset_fields.insert("logo_small".to_string());
                                                        d.clear_fields.remove("logo_small");
                                                        deltas.set(d);
                                                        form_state.dispatch(OverrideEditFormAction::LogoSmall(fallback.clone()));
                                                    })
                                                },
                                                Some({
                                                    let form_state = form_state.clone();
                                                    let deltas = deltas.clone();
                                                    Callback::from(move |_| {
                                                        let mut d = (*deltas).clone();
                                                        d.clear_fields.insert("logo_small".to_string());
                                                        d.reset_fields.remove("logo_small");
                                                        deltas.set(d);
                                                        form_state.dispatch(OverrideEditFormAction::LogoSmall(String::new()));
                                                    })
                                                }),
                                                &translate,
                                                Some(logo_small_opts),
                                            )
                                        }
                                    </>
                                }
                            } else {
                                html! {}
                            }
                        }

                        // VOD / Series specific fields
                        {
                            if kind == OverrideItemKind::Vod || kind == OverrideItemKind::Series {
                                html! {
                                    <>
                                        { render_field_row(
                                            "tmdb",
                                            "LABEL.TMDB",
                                            &form_state.form.tmdb,
                                            &initial_form.tmdb,
                                            *active_scope,
                                            provenance.as_ref(),
                                            &deltas,
                                            &field_errors,
                                            true,
                                            {
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                Callback::from(move |val: String| {
                                                    let mut d = (*deltas).clone();
                                                    d.reset_fields.remove("tmdb");
                                                    d.clear_fields.remove("tmdb");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Tmdb(val));
                                                })
                                            },
                                            {
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                let fallback = provenance.as_ref().map_or_else(String::new, |p| reset_fallback(*active_scope, "tmdb", p).value.unwrap_or_default());
                                                Callback::from(move |_| {
                                                    let mut d = (*deltas).clone();
                                                    d.reset_fields.insert("tmdb".to_string());
                                                    d.clear_fields.remove("tmdb");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Tmdb(fallback.clone()));
                                                })
                                            },
                                            Some({
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                Callback::from(move |_| {
                                                    let mut d = (*deltas).clone();
                                                    d.clear_fields.insert("tmdb".to_string());
                                                    d.reset_fields.remove("tmdb");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Tmdb(String::new()));
                                                })
                                            }),
                                            &translate,
                                            None,
                                        ) }

                                        { render_field_row(
                                            "rating",
                                            "LABEL.RATING",
                                            &form_state.form.rating,
                                            &initial_form.rating,
                                            *active_scope,
                                            provenance.as_ref(),
                                            &deltas,
                                            &field_errors,
                                            true,
                                            {
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                Callback::from(move |val: String| {
                                                    let mut d = (*deltas).clone();
                                                    d.reset_fields.remove("rating");
                                                    d.clear_fields.remove("rating");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Rating(val));
                                                })
                                            },
                                            {
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                let fallback = provenance.as_ref().map_or_else(String::new, |p| reset_fallback(*active_scope, "rating", p).value.unwrap_or_default());
                                                Callback::from(move |_| {
                                                    let mut d = (*deltas).clone();
                                                    d.reset_fields.insert("rating".to_string());
                                                    d.clear_fields.remove("rating");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Rating(fallback.clone()));
                                                })
                                            },
                                            Some({
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                Callback::from(move |_| {
                                                    let mut d = (*deltas).clone();
                                                    d.clear_fields.insert("rating".to_string());
                                                    d.reset_fields.remove("rating");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Rating(String::new()));
                                                })
                                            }),
                                            &translate,
                                            None,
                                        ) }

                                        { render_field_row(
                                            "release_date",
                                            "LABEL.RELEASE_DATE",
                                            &form_state.form.release_date,
                                            &initial_form.release_date,
                                            *active_scope,
                                            provenance.as_ref(),
                                            &deltas,
                                            &field_errors,
                                            true,
                                            {
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                Callback::from(move |val: String| {
                                                    let mut d = (*deltas).clone();
                                                    d.reset_fields.remove("release_date");
                                                    d.clear_fields.remove("release_date");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::ReleaseDate(val));
                                                })
                                            },
                                            {
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                let fallback = provenance.as_ref().map_or_else(String::new, |p| reset_fallback(*active_scope, "release_date", p).value.unwrap_or_default());
                                                Callback::from(move |_| {
                                                    let mut d = (*deltas).clone();
                                                    d.reset_fields.insert("release_date".to_string());
                                                    d.clear_fields.remove("release_date");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::ReleaseDate(fallback.clone()));
                                                })
                                            },
                                            Some({
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                Callback::from(move |_| {
                                                    let mut d = (*deltas).clone();
                                                    d.clear_fields.insert("release_date".to_string());
                                                    d.reset_fields.remove("release_date");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::ReleaseDate(String::new()));
                                                })
                                            }),
                                            &translate,
                                            None,
                                        ) }

                                        { render_field_row(
                                            "plot",
                                            "LABEL.PLOT",
                                            &form_state.form.plot,
                                            &initial_form.plot,
                                            *active_scope,
                                            provenance.as_ref(),
                                            &deltas,
                                            &field_errors,
                                            true,
                                            {
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                Callback::from(move |val: String| {
                                                    let mut d = (*deltas).clone();
                                                    d.reset_fields.remove("plot");
                                                    d.clear_fields.remove("plot");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Plot(val));
                                                })
                                            },
                                            {
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                let fallback = provenance.as_ref().map_or_else(String::new, |p| reset_fallback(*active_scope, "plot", p).value.unwrap_or_default());
                                                Callback::from(move |_| {
                                                    let mut d = (*deltas).clone();
                                                    d.reset_fields.insert("plot".to_string());
                                                    d.clear_fields.remove("plot");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Plot(fallback.clone()));
                                                })
                                            },
                                            Some({
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                Callback::from(move |_| {
                                                    let mut d = (*deltas).clone();
                                                    d.clear_fields.insert("plot".to_string());
                                                    d.reset_fields.remove("plot");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Plot(String::new()));
                                                })
                                            }),
                                            &translate,
                                            None,
                                        ) }

                                        { render_field_row(
                                            "genre",
                                            "LABEL.GENRE",
                                            &form_state.form.genre,
                                            &initial_form.genre,
                                            *active_scope,
                                            provenance.as_ref(),
                                            &deltas,
                                            &field_errors,
                                            true,
                                            {
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                Callback::from(move |val: String| {
                                                    let mut d = (*deltas).clone();
                                                    d.reset_fields.remove("genre");
                                                    d.clear_fields.remove("genre");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Genre(val));
                                                })
                                            },
                                            {
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                let fallback = provenance.as_ref().map_or_else(String::new, |p| reset_fallback(*active_scope, "genre", p).value.unwrap_or_default());
                                                Callback::from(move |_| {
                                                    let mut d = (*deltas).clone();
                                                    d.reset_fields.insert("genre".to_string());
                                                    d.clear_fields.remove("genre");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Genre(fallback.clone()));
                                                })
                                            },
                                            Some({
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                Callback::from(move |_| {
                                                    let mut d = (*deltas).clone();
                                                    d.clear_fields.insert("genre".to_string());
                                                    d.reset_fields.remove("genre");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Genre(String::new()));
                                                })
                                            }),
                                            &translate,
                                            None,
                                        ) }

                                        { render_field_row(
                                            "cast",
                                            "LABEL.CAST",
                                            &form_state.form.cast,
                                            &initial_form.cast,
                                            *active_scope,
                                            provenance.as_ref(),
                                            &deltas,
                                            &field_errors,
                                            true,
                                            {
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                Callback::from(move |val: String| {
                                                    let mut d = (*deltas).clone();
                                                    d.reset_fields.remove("cast");
                                                    d.clear_fields.remove("cast");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Cast(val));
                                                })
                                            },
                                            {
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                let fallback = provenance.as_ref().map_or_else(String::new, |p| reset_fallback(*active_scope, "cast", p).value.unwrap_or_default());
                                                Callback::from(move |_| {
                                                    let mut d = (*deltas).clone();
                                                    d.reset_fields.insert("cast".to_string());
                                                    d.clear_fields.remove("cast");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Cast(fallback.clone()));
                                                })
                                            },
                                            Some({
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                Callback::from(move |_| {
                                                    let mut d = (*deltas).clone();
                                                    d.clear_fields.insert("cast".to_string());
                                                    d.reset_fields.remove("cast");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Cast(String::new()));
                                                })
                                            }),
                                            &translate,
                                            None,
                                        ) }

                                        { render_field_row(
                                            "director",
                                            "LABEL.DIRECTOR",
                                            &form_state.form.director,
                                            &initial_form.director,
                                            *active_scope,
                                            provenance.as_ref(),
                                            &deltas,
                                            &field_errors,
                                            true,
                                            {
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                Callback::from(move |val: String| {
                                                    let mut d = (*deltas).clone();
                                                    d.reset_fields.remove("director");
                                                    d.clear_fields.remove("director");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Director(val));
                                                })
                                            },
                                            {
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                let fallback = provenance.as_ref().map_or_else(String::new, |p| reset_fallback(*active_scope, "director", p).value.unwrap_or_default());
                                                Callback::from(move |_| {
                                                    let mut d = (*deltas).clone();
                                                    d.reset_fields.insert("director".to_string());
                                                    d.clear_fields.remove("director");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Director(fallback.clone()));
                                                })
                                            },
                                            Some({
                                                let form_state = form_state.clone();
                                                let deltas = deltas.clone();
                                                Callback::from(move |_| {
                                                    let mut d = (*deltas).clone();
                                                    d.clear_fields.insert("director".to_string());
                                                    d.reset_fields.remove("director");
                                                    deltas.set(d);
                                                    form_state.dispatch(OverrideEditFormAction::Director(String::new()));
                                                })
                                            }),
                                            &translate,
                                            None,
                                        ) }
                                    </>
                                }
                            } else {
                                html! {}
                            }
                        }

                        // Unknown fields section
                        {
                            if unknown_fields.is_empty() {
                                html! {}
                            } else {
                                html! {
                                    <div class="tp__explorer-unknown-section">
                                        <h4>{ translate.t("LABEL.EXPLORER_UNKNOWN_FIELDS") }</h4>
                                        { for unknown_fields.into_iter().map(|(k, v)| {
                                            let remove_k = k.clone();
                                            let deltas = deltas.clone();
                                            let on_remove = Callback::from(move |_: String| {
                                                let mut d = (*deltas).clone();
                                                if !d.remove_unknown.contains(&remove_k) {
                                                    d.remove_unknown.push(remove_k.clone());
                                                    deltas.set(d);
                                                }
                                            });
                                            html! {
                                                <div class="tp__explorer-unknown-row">
                                                    <span>{ format!("{}: {}", k, v) }</span>
                                                    <TextButton
                                                        name="remove_unknown"
                                                        class="secondary"
                                                        icon="Delete"
                                                        title={translate.t("LABEL.EXPLORER_REMOVE_UNKNOWN")}
                                                        onclick={on_remove}
                                                    />
                                                </div>
                                            }
                                        })}
                                    </div>
                                }
                            }
                        }
                    </div>

                    // Toolbar
                    <div class="tp__dialog__toolbar">
                        <div class="tp__dialog__toolbar-left">
                            <TextButton
                                class="secondary"
                                name="cancel"
                                icon="Cancel"
                                title={translate.t("LABEL.CANCEL")}
                                onclick={on_close}
                            />
                        </div>
                        <div class="tp__dialog__toolbar-right">
                            <TextButton
                                name="update_now"
                                icon="Refresh"
                                title={translate.t("LABEL.EXPLORER_UPDATE_NOW")}
                                onclick={handle_update_now}
                            />
                            <TextButton
                                class="primary"
                                name="save"
                                icon="Save"
                                title={translate.t("LABEL.SAVE")}
                                disabled={!can_write || is_ambiguous || *is_loading || load_error.is_some() || *is_saving}
                                onclick={handle_save}
                            />
                        </div>
                    </div>
                </div>
            </div>
        }
}

#[derive(Default)]
struct FieldRowOptions {
    custom_input: Option<Html>,
    extra_content: Option<Html>,
}

#[allow(clippy::too_many_arguments)]
fn render_field_row(
    field_id: &'static str,
    label_key: &'static str,
    current_value: &str,
    initial_value: &str,
    scope: OverrideScopeDto,
    provenance: Option<&OverrideProvenanceDto>,
    deltas: &UseStateHandle<FieldDeltas>,
    field_errors: &UseStateHandle<HashMap<String, String>>,
    clearable: bool,
    on_change: Callback<String>,
    on_reset: Callback<String>,
    on_clear: Option<Callback<String>>,
    translate: &crate::i18n::YewI18n,
    opts: Option<FieldRowOptions>,
) -> Html {
    let (custom_input, extra_content) = match opts {
        Some(o) => (o.custom_input, o.extra_content),
        None => (None, None),
    };

    let is_modified = current_value != initial_value
        || deltas.reset_fields.contains(field_id)
        || deltas.clear_fields.contains(field_id);

    let info = provenance.map(|p| field_provenance(field_id, p)).unwrap_or(FieldProvenanceInfo {
        effective_value: Some(current_value.to_string()),
        source: ProvenanceSource::None,
    });

    let fallback_text =
        provenance.map(|p| fallback_label(translate, &reset_fallback(scope, field_id, p))).unwrap_or_default();

    let error_msg = field_errors.get(field_id);

    let full_field_id = format!("PLAYLIST_OVERRIDE.{}", field_id.to_uppercase());

    html! {
        <div class={format!("tp__explorer-field-row {}", if is_modified { "chg" } else { "" })}>
            <div class="tp__form-field tp__form-field__text">
                {
                    if let Some(custom) = custom_input {
                        custom
                    } else {
                        html! {
                            <crate::app::components::input::Input
                                label={Some(translate.t(label_key))}
                                name={field_id}
                                field_id={Some(full_field_id)}
                                autocomplete={true}
                                value={current_value.to_string()}
                                on_change={on_change}
                            />
                        }
                    }
                }
                { extra_content.unwrap_or_default() }
            </div>

            <div class="tp__explorer-field-meta">
                { html_if!(is_modified, {
                    <span class="o">{ translate.t("LABEL.EXPLORER_EDITED_BADGE") }</span>
                })}
                <span class="tp__explorer-actions">
                    <TextButton
                        name="reset"
                        class="tp__explorer-field-action"
                        icon="Refresh"
                        title={translate.t("LABEL.RESET")}
                        hint={Some(fallback_text.clone())}
                        aria_label={Some(format!("{} {}", translate.t("LABEL.RESET"), fallback_text))}
                        onclick={on_reset}
                    />
                    {
                        match on_clear {
                            Some(on_clear) if clearable => html! {
                                <TextButton
                                    name="clear"
                                    class="tp__explorer-field-action"
                                    icon="Clear"
                                    title={translate.t("LABEL.EXPLORER_CLEAR_FIELD")}
                                    onclick={on_clear}
                                />
                            },
                            _ => html! {},
                        }
                    }
                </span>
            </div>
            <div class="tp__explorer-provenance-info">
                { format!("{}: {}, {}: {}",
                    translate.t("LABEL.EXPLORER_EFFECTIVE_VALUE"),
                    provenance_value_label(translate, &info),
                    translate.t("LABEL.EXPLORER_PROVENANCE_SOURCE"),
                    translate.t(info.source.label_key())) }
            </div>

            {
                if scope == OverrideScopeDto::Target {
                    html! {
                        <div class="tp__explorer-fallback-info">
                            { format!("{}: {}", translate.t("LABEL.EXPLORER_FALLBACK_ON_RESET"), fallback_text) }
                        </div>
                    }
                } else {
                    html! {}
                }
            }

            {
                if let Some(err) = error_msg {
                    html! {
                        <div class="tp__explorer-error-msg">
                            { translate.t(err) }
                        </div>
                    }
                } else {
                    html! {}
                }
            }
        </div>
    }
}

#[cfg(test)]
#[path = "edit_panel.test.rs"]
mod tests;
