use super::{FieldRef, PlaylistItemType, XtreamCluster};
use crate::{
    model::{HeaderField, StreamProperties, UUIDType, VirtualId},
    utils::{arc_str_option_serde, arc_str_serde, generate_runtime_playlist_uuid, get_provider_id, Internable},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistItemHeader {
    #[serde(skip)]
    pub uuid: UUIDType, // calculated
    #[serde(with = "arc_str_serde")]
    pub id: Arc<str>, // provider id
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
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
    #[serde(default)]
    pub additional_properties: Option<StreamProperties>,

    // 4-byte aligned
    pub virtual_id: VirtualId, // virtual id
    pub chno: u32,
    #[serde(default)]
    pub category_id: u32,
    #[serde(default)]
    pub source_ordinal: u32,

    // 1-byte aligned
    pub xtream_cluster: XtreamCluster,
    #[serde(default)]
    pub item_type: PlaylistItemType,
    /// Stable provider/origin ID captured before any target transformation.
    #[serde(default, with = "arc_str_serde")]
    pub input_stream_id: Arc<str>,
    #[serde(default, rename = "source_user_agent", alias = "upstream_user_agent", with = "arc_str_option_serde")]
    pub upstream_user_agent: Option<Arc<str>>,
}

impl Default for PlaylistItemHeader {
    fn default() -> Self {
        Self {
            uuid: UUIDType::default(),
            id: "".intern(),
            virtual_id: VirtualId::default(),
            name: "".intern(),
            chno: 0,
            logo: "".intern(),
            logo_small: "".intern(),
            group: "".intern(),
            title: "".intern(),
            parent_code: "".intern(),
            audio_track: "".intern(),
            time_shift: "".intern(),
            rec: "".intern(),
            url: "".intern(),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::default(),
            additional_properties: None,
            item_type: PlaylistItemType::default(),
            category_id: 0,
            input_name: "".intern(),
            source_ordinal: 0,
            input_stream_id: "".intern(),
            upstream_user_agent: None,
        }
    }
}

impl PlaylistItemHeader {
    /// Captures the input playlist item ID at the input-processing boundary.
    ///
    /// This must run before target transformations and must not be used to recover an identity
    /// from a target-mutated `id`.
    pub fn freeze_input_stream_id(&mut self) {
        if self.input_stream_id.is_empty() && !self.id.is_empty() {
            self.input_stream_id = Arc::clone(&self.id);
        }
    }

    /// Returns the input stream ID captured at the input-processing boundary.
    ///
    /// Legacy fallback must be resolved before a target transformation starts. Once an item is
    /// represented by this header, the mutable target-facing `id` is never a valid fallback.
    pub fn get_input_stream_id(&self) -> Option<Arc<str>> {
        (!self.input_stream_id.is_empty()).then(|| Arc::clone(&self.input_stream_id))
    }

    #[inline]
    pub fn gen_uuid(&mut self) {
        self.uuid = generate_runtime_playlist_uuid(&self.input_name, &self.id, self.item_type, &self.url);
    }

    #[inline]
    pub const fn get_uuid(&self) -> &UUIDType { &self.uuid }

    pub fn get_provider_id(&mut self) -> Option<u32> {
        match get_provider_id(&self.id, &self.url) {
            None => None,
            Some(newid) => {
                self.id = newid.to_string().intern();
                Some(newid)
            }
        }
    }

    #[inline]
    pub fn get_name(&self) -> Arc<str> {
        if self.title.is_empty() {
            Arc::clone(&self.name)
        } else {
            Arc::clone(&self.title)
        }
    }

    #[inline]
    pub fn get_container_extension(&self) -> Option<Arc<str>> {
        self.additional_properties
            .as_ref()
            .and_then(super::super::stream_properties::StreamProperties::get_container_extension)
    }
}

impl crate::model::FieldGet for crate::model::PlaylistItemHeader {
    fn get(&self, field: HeaderField) -> Option<FieldRef<'_>> {
        match field {
            HeaderField::Id => Some(FieldRef::Shared(&self.id)),
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
            HeaderField::Input => Some(FieldRef::Shared(&self.input_name)),
            HeaderField::Type => Some(FieldRef::Str(self.item_type.as_str())),
            HeaderField::EpgChannelId => self.epg_channel_id.as_ref().map(FieldRef::Shared),
            HeaderField::Chno => Some(FieldRef::Num(self.chno)),
            HeaderField::Genre => {
                self.additional_properties.as_ref().and_then(StreamProperties::genre).map(FieldRef::Shared)
            }
            // Not carried by the header.
            HeaderField::ProviderId => None,
        }
    }
}

impl crate::model::FieldSet for crate::model::PlaylistItemHeader {
    fn set(&mut self, field: HeaderField, value: &str) -> bool {
        match field {
            HeaderField::Id => self.id = value.intern(),
            HeaderField::Title => self.title = value.intern(),
            HeaderField::Name => self.name = value.intern(),
            HeaderField::Logo => self.logo = value.intern(),
            HeaderField::LogoSmall => self.logo_small = value.intern(),
            HeaderField::ParentCode => self.parent_code = value.intern(),
            HeaderField::AudioTrack => self.audio_track = value.intern(),
            HeaderField::TimeShift => self.time_shift = value.intern(),
            HeaderField::Rec => self.rec = value.intern(),
            HeaderField::Url => self.url = value.intern(),
            HeaderField::Group => self.group = value.intern(),
            HeaderField::Caption => {
                let interned = value.intern();
                self.title = Arc::clone(&interned);
                self.name = interned;
            }
            HeaderField::EpgChannelId => self.epg_channel_id = Some(value.intern()),
            HeaderField::Chno => match value.parse::<u32>() {
                Ok(parsed) => self.chno = parsed,
                Err(_) => return false,
            },
            HeaderField::Genre => return crate::set_genre!(self, value),
            // Read-only, or not carried by the header.
            HeaderField::Input | HeaderField::Type | HeaderField::ProviderId => return false,
        }
        true
    }
}

impl crate::model::FieldGetAccessor for crate::model::PlaylistItemHeader {
    #[inline]
    fn get_field(&self, field: &str) -> Option<Arc<str>> {
        use crate::model::FieldGet;
        self.get(HeaderField::parse(field)?).map(|value| value.to_arc())
    }
}

impl crate::model::FieldSetAccessor for crate::model::PlaylistItemHeader {
    #[inline]
    fn set_field(&mut self, field: &str, value: &str) -> bool {
        use crate::model::FieldSet;
        HeaderField::parse(field).is_some_and(|field| self.set(field, value))
    }
}
