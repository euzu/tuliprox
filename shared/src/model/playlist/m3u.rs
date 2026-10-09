use super::{FieldRef, PlaylistEntry, PlaylistItemType, XtreamMappingOptions};
use crate::{
    model::{
        CatchupAttribute, CatchupProperties, CommonPlaylistItem, ConfigTargetOptions, HeaderField, StreamProperties,
        UUIDType, VirtualId, XtreamInfoDocument,
    },
    utils::{arc_str_option_serde, arc_str_serde, generate_runtime_playlist_uuid, get_provider_id},
};
use serde::{Deserialize, Serialize};
use std::{fmt::Write, sync::Arc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct M3uPlaylistItem {
    pub virtual_id: VirtualId,
    #[serde(with = "arc_str_serde")]
    pub provider_id: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    pub chno: u32,
    #[serde(with = "arc_str_serde")]
    pub logo: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub logo_small: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub group: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub title: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub parent_code: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub audio_track: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub time_shift: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub rec: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub url: Arc<str>,
    #[serde(default, with = "arc_str_option_serde")]
    pub epg_channel_id: Option<Arc<str>>,
    #[serde(with = "arc_str_serde")]
    pub input_name: Arc<str>,
    pub item_type: PlaylistItemType,
    #[serde(skip)]
    pub t_stream_url: Arc<str>,
    #[serde(skip)]
    pub t_resource_url: Option<String>,
    #[serde(skip)]
    pub t_catchup_source: Option<Arc<str>>,
    #[serde(skip)]
    pub t_catchup_mode: Option<Arc<str>>,
    #[serde(default)]
    pub source_ordinal: u32,
    #[serde(default)]
    pub additional_properties: Option<StreamProperties>,
    /// Stable provider/origin ID captured before any target transformation.
    #[serde(default, with = "arc_str_serde")]
    pub input_stream_id: Arc<str>,
    #[serde(default, rename = "source_user_agent", alias = "upstream_user_agent", with = "arc_str_option_serde")]
    pub upstream_user_agent: Option<Arc<str>>,
}

fn write_m3u_attr(line: &mut String, name: &str, value: &str) { let _ = write!(line, " {name}=\"{value}\""); }

fn append_catchup_attribute(line: &mut String, name: &str, value: Option<&Arc<str>>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        write_m3u_attr(line, name, value);
    }
}

fn append_extra_catchup_attributes(line: &mut String, attributes: &[CatchupAttribute]) {
    for attribute in attributes {
        if !attribute.name.is_empty() && !attribute.value.is_empty() {
            write_m3u_attr(line, attribute.name.as_ref(), attribute.value.as_ref());
        }
    }
}

fn append_m3u_catchup_attributes(
    line: &mut String,
    catchup: &CatchupProperties,
    rewritten_mode: Option<&Arc<str>>,
    rewritten_source: Option<&Arc<str>>,
) {
    if let Some(mode) = rewritten_mode.filter(|mode| !mode.is_empty()) {
        write_m3u_attr(line, "catchup", mode.as_ref());
    } else {
        append_catchup_attribute(line, "catchup", catchup.mode.as_ref());
    }
    append_catchup_attribute(line, "catchup-days", catchup.days.as_ref());
    if let Some(source) = rewritten_source.filter(|source| !source.is_empty()) {
        write_m3u_attr(line, "catchup-source", source.as_ref());
    } else {
        append_catchup_attribute(line, "catchup-source", catchup.source.as_ref());
    }
    append_catchup_attribute(line, "catchup-time", catchup.time.as_ref());
    append_catchup_attribute(line, "catchup-correction", catchup.correction.as_ref());
    if !catchup.is_flussonic() || rewritten_mode.is_none_or(|mode| mode.is_empty()) {
        append_catchup_attribute(line, "catchup-type", catchup.catchup_type.as_ref());
    }
    append_extra_catchup_attributes(line, &catchup.extra_attributes);
}

fn append_unified_catchup_type_attributes(
    line: &mut String,
    catchup: &CatchupProperties,
    catchup_type: &str,
    rewritten_source: Option<&Arc<str>>,
    emit_source: bool,
) {
    // Unified export: always `catchup-type=...`, never `catchup=`.
    write_m3u_attr(line, "catchup-type", catchup_type);
    append_catchup_attribute(line, "catchup-days", catchup.days.as_ref());
    if emit_source {
        if let Some(source) = rewritten_source.filter(|source| !source.is_empty()) {
            write_m3u_attr(line, "catchup-source", source.as_ref());
        } else {
            append_catchup_attribute(line, "catchup-source", catchup.source.as_ref());
        }
    }
    append_catchup_attribute(line, "catchup-time", catchup.time.as_ref());
    append_catchup_attribute(line, "catchup-correction", catchup.correction.as_ref());
    append_extra_catchup_attributes(line, &catchup.extra_attributes);
}

impl M3uPlaylistItem {
    pub fn to_m3u(&self, target_options: Option<&ConfigTargetOptions>, rewrite_urls: bool) -> String {
        let options = target_options.as_ref();
        let ignore_logo = options.is_some_and(|o| o.ignore_logo);
        let mut line = String::with_capacity(256);
        let _ = write!(
            &mut line,
            "#EXTINF:-1 tvg-id=\"{}\" tvg-name=\"{}\" group-title=\"{}\"",
            self.epg_channel_id.as_ref().map_or("", |o| o.as_ref()),
            self.name,
            self.group
        );

        if !ignore_logo {
            if let (true, Some(resource_url)) = (rewrite_urls, self.t_resource_url.as_ref()) {
                to_m3u_resource_non_empty_fields!(self, resource_url, line, (logo, "tvg-logo"), (logo_small, "tvg-logo-small"););
            } else {
                to_m3u_non_empty_fields!(self, line, (logo, "tvg-logo"), (logo_small, "tvg-logo-small"););
            }
        }

        if self.chno != 0 {
            let _ = write!(line, " tvg-chno=\"{}\"", self.chno);
        }
        let flussonic_mode = self.additional_properties.as_ref().and_then(|props| match props {
            StreamProperties::Live(live) => {
                live.catchup.as_ref().and_then(CatchupProperties::native_flussonic_player_mode)
            }
            _ => None,
        });
        let append_type = self.additional_properties.as_ref().and_then(|props| match props {
            StreamProperties::Live(live) => live.catchup.as_ref().and_then(CatchupProperties::append_player_type),
            _ => None,
        });
        // Emit timeshift only when the source had it (non-empty). Never invent catchup attrs.
        to_m3u_non_empty_fields!(self, line,
            (parent_code, "parent-code"),
            (audio_track, "audio-track"),
            (time_shift, "timeshift"),
            (rec, "tvg-rec"););
        if let Some(StreamProperties::Live(live)) = self.additional_properties.as_ref() {
            if let Some(catchup) = live.catchup.as_ref() {
                let has_rewritten_catchup = self.t_catchup_mode.as_ref().is_some_and(|mode| !mode.is_empty())
                    || self.t_catchup_source.as_ref().is_some_and(|source| !source.is_empty());
                if has_rewritten_catchup {
                    append_m3u_catchup_attributes(
                        &mut line,
                        catchup,
                        self.t_catchup_mode.as_ref(),
                        self.t_catchup_source.as_ref(),
                    );
                } else if let Some(mode) = flussonic_mode {
                    // Unify catchup=/catchup-type=flussonic* to catchup-type only.
                    // No catchup=, no shift-style catchup-source, no invented days.
                    append_unified_catchup_type_attributes(&mut line, catchup, mode, None, false);
                } else if let Some(append_type) = append_type {
                    // Unify catchup=/catchup-type=append to catchup-type="append" only.
                    append_unified_catchup_type_attributes(
                        &mut line,
                        catchup,
                        append_type,
                        self.t_catchup_source.as_ref(),
                        true,
                    );
                } else {
                    append_m3u_catchup_attributes(
                        &mut line,
                        catchup,
                        self.t_catchup_mode.as_ref(),
                        self.t_catchup_source.as_ref(),
                    );
                }
            }
        }

        let _ = writeln!(&mut line, ",{}", self.title);
        if let Some(user_agent) =
            self.upstream_user_agent.as_deref().filter(|value| !value.is_empty() && !value.contains(['\r', '\n']))
        {
            let _ = writeln!(&mut line, "#EXTVLCOPT:http-user-agent={user_agent}");
        }
        let url = if self.t_stream_url.is_empty() { &self.url } else { &self.t_stream_url };
        line.push_str(url);
        line
    }

    pub fn to_common(&self) -> CommonPlaylistItem {
        CommonPlaylistItem {
            virtual_id: self.virtual_id,
            provider_id: Arc::clone(&self.provider_id),
            name: Arc::clone(&self.name),
            chno: self.chno,
            logo: Arc::clone(&self.logo),
            logo_small: Arc::clone(&self.logo_small),
            group: Arc::clone(&self.group),
            title: Arc::clone(&self.title),
            parent_code: Arc::clone(&self.parent_code),
            audio_track: Arc::clone(&self.audio_track),
            time_shift: Arc::clone(&self.time_shift),
            rec: Arc::clone(&self.rec),
            url: Arc::clone(&self.url),
            input_name: Arc::clone(&self.input_name),
            item_type: self.item_type,
            epg_channel_id: self.epg_channel_id.clone(),
            xtream_cluster: Some(self.item_type.cluster()),
            additional_properties: self.additional_properties.clone(),
            category_id: None,
        }
    }
}

impl PlaylistEntry for M3uPlaylistItem {
    #[inline]
    fn get_virtual_id(&self) -> VirtualId { self.virtual_id }

    fn get_input_stream_id(&self) -> Option<Arc<str>> {
        let input_stream_id = if self.input_stream_id.is_empty() { &self.provider_id } else { &self.input_stream_id };
        (!input_stream_id.is_empty()).then(|| Arc::clone(input_stream_id))
    }

    fn get_upstream_user_agent(&self) -> Option<&str> { self.upstream_user_agent.as_deref() }

    fn get_provider_id(&self) -> Option<u32> { get_provider_id(&self.provider_id, &self.url) }
    #[inline]
    fn get_category_id(&self) -> Option<u32> { None }
    #[inline]
    fn get_provider_url(&self) -> Arc<str> { Arc::clone(&self.url) }

    fn get_uuid(&self) -> UUIDType {
        generate_runtime_playlist_uuid(&self.input_name, &self.provider_id, self.item_type, &self.url)
    }

    #[inline]
    fn get_item_type(&self) -> PlaylistItemType { self.item_type }

    #[inline]
    fn get_group(&self) -> Arc<str> { Arc::clone(&self.group) }

    #[inline]
    fn get_name(&self) -> Arc<str> {
        if self.title.is_empty() {
            Arc::clone(&self.name)
        } else {
            Arc::clone(&self.title)
        }
    }

    #[inline]
    fn get_resolved_info_document(&self, _options: &XtreamMappingOptions) -> Option<XtreamInfoDocument> { None }
    #[inline]
    fn get_additional_properties(&self) -> Option<&StreamProperties> { self.additional_properties.as_ref() }
    #[inline]
    fn get_additional_properties_mut(&mut self) -> Option<&mut StreamProperties> { self.additional_properties.as_mut() }
}

impl crate::model::FieldGet for M3uPlaylistItem {
    fn get(&self, field: HeaderField) -> Option<FieldRef<'_>> {
        match field {
            HeaderField::ProviderId => Some(FieldRef::Shared(&self.provider_id)),
            HeaderField::Title => Some(FieldRef::Shared(&self.title)),
            HeaderField::Name => Some(FieldRef::Shared(&self.name)),
            HeaderField::Logo => Some(FieldRef::Shared(&self.logo)),
            HeaderField::LogoSmall => Some(FieldRef::Shared(&self.logo_small)),
            HeaderField::ParentCode => Some(FieldRef::Shared(&self.parent_code)),
            HeaderField::AudioTrack => Some(FieldRef::Shared(&self.audio_track)),
            HeaderField::TimeShift => Some(FieldRef::Shared(&self.time_shift)),
            HeaderField::Rec => Some(FieldRef::Shared(&self.rec)),
            HeaderField::Url => Some(FieldRef::Shared(&self.url)),
            HeaderField::Group => Some(FieldRef::Shared(&self.group)),
            HeaderField::Caption => {
                Some(FieldRef::Shared(if self.title.is_empty() { &self.name } else { &self.title }))
            }
            HeaderField::EpgChannelId => self.epg_channel_id.as_ref().map(FieldRef::Shared),
            HeaderField::Chno => Some(FieldRef::Num(self.chno)),
            // Deliberately not addressable by name on an M3U item, even though the
            // struct carries input_name, item_type and additional_properties. The
            // M3U resource endpoint resolves a URL path segment through
            // `get_field`, so making these resolvable would turn a 404 into a
            // response. Preserved from the string-keyed accessor this replaces.
            HeaderField::Id | HeaderField::Input | HeaderField::Type | HeaderField::Genre => None,
        }
    }
}

impl crate::model::FieldGetAccessor for M3uPlaylistItem {
    #[inline]
    fn get_field(&self, field: &str) -> Option<Arc<str>> {
        use crate::model::FieldGet;
        let field = HeaderField::parse(field)?;
        let value = self.get(field)?.to_arc();
        Some(if matches!(field, HeaderField::Logo | HeaderField::LogoSmall) {
            super::super::persisted_resource_arc(&value)
        } else {
            value
        })
    }
}

impl From<M3uPlaylistItem> for CommonPlaylistItem {
    fn from(item: M3uPlaylistItem) -> Self { item.to_common() }
}
