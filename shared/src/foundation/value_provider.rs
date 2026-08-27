use crate::{
    model::{FieldGet, FieldGetAccessor, ItemField, PlaylistItem, StreamProperties},
    utils::{deunicode_string, Internable},
};
use std::{borrow::Cow, sync::Arc};

#[macro_export]
macro_rules! set_genre {
    ($header:ident, $value:ident) => {
        if let Some(ref mut additional_properties) = $header.additional_properties {
            // The four-arm match lives on StreamProperties now.
            $crate::model::StreamProperties::set_genre(additional_properties, $value)
        } else {
            // No properties yet: build them from the header. This half stays here
            // because it needs the header, not just the properties.
            let empty_str = "".intern();
            match $header.item_type {
                $crate::model::PlaylistItemType::LocalVideo | $crate::model::PlaylistItemType::Video => {
                    $header.additional_properties =
                        Some($crate::model::StreamProperties::Video(Box::from($crate::model::VideoStreamProperties {
                            name: $header.title.clone(),
                            category_id: $header.category_id,
                            stream_id: $header.virtual_id.get(),
                            stream_icon: $header.logo.clone(),
                            direct_source: ::std::sync::Arc::clone(&empty_str),
                            custom_sid: None,
                            added: ::std::sync::Arc::clone(&empty_str),
                            container_extension: $header
                                .get_container_extension()
                                .unwrap_or_else(|| Arc::clone(&empty_str)),
                            rating: None,
                            rating_5based: None,
                            stream_type: None,
                            trailer: None,
                            tmdb: None,
                            is_adult: 0,
                            details: Some($crate::model::VideoStreamDetailProperties {
                                genre: Some($value.intern()),
                                ..$crate::model::VideoStreamDetailProperties::default()
                            }),
                        })));
                    true
                }
                $crate::model::PlaylistItemType::LocalSeriesInfo | $crate::model::PlaylistItemType::SeriesInfo => {
                    $header.additional_properties = Some($crate::model::StreamProperties::Series(Box::from(
                        $crate::model::SeriesStreamProperties {
                            name: $header.title.clone(),
                            category_id: $header.category_id,
                            series_id: $header.virtual_id.get(),
                            backdrop_path: None,
                            cast: ::std::sync::Arc::clone(&empty_str),
                            cover: ::std::sync::Arc::clone(&empty_str),
                            director: ::std::sync::Arc::clone(&empty_str),
                            episode_run_time: None,
                            genre: Some($value.intern()),
                            last_modified: None,
                            plot: None,
                            rating: 0.0,
                            rating_5based: 0.0,
                            release_date: None,
                            youtube_trailer: ::std::sync::Arc::clone(&empty_str),
                            tmdb: None,
                            details: None,
                        },
                    )));
                    true
                }
                _ => false,
            }
        }
    };
}

pub use set_genre;

/// Canonical list of `ItemField` variants that map 1:1 to a simple `Arc<str>` slot on
/// `PlaylistItemHeader`.
///
/// Both `get_field_value` and `set_field_value` are generated from this single list (via a
/// callback macro), so a directly-bound field cannot be added to the read half but forgotten in
/// the write half (or vice versa). The asymmetric fields (`Genre`, `Type`, `Caption`) stay
/// explicit because their read/write behavior differs.
macro_rules! for_each_direct_field {
    ($cb:ident) => {
        $cb! {
            Group => group,
            Name => name,
            Title => title,
            Url => url,
            Input => input_name,
        }
    };
}

pub fn get_field_value(pli: &PlaylistItem, field: ItemField) -> Arc<str> {
    let header = &pli.header;

    macro_rules! get_arms {
        ($($variant:ident => $prop:ident),+ $(,)?) => {
            match field {
                $(ItemField::$variant => Arc::clone(&header.$prop),)+
                ItemField::Genre => header
                    .additional_properties
                    .as_ref()
                    .and_then(StreamProperties::genre)
                    .map_or_else(|| "".intern(), Arc::clone),
                ItemField::Type => header.item_type.interned_label(),
                ItemField::EpgId => header.epg_channel_id.clone().unwrap_or_else(|| "".intern()),
                ItemField::Chno => header.chno.to_string().intern(),
                ItemField::Quality => header_quality_rank(header).to_string().intern(),
                ItemField::Caption => {
                    if header.title.is_empty() {
                        Arc::clone(&header.name)
                    } else {
                        Arc::clone(&header.title)
                    }
                }
            }
        };
    }

    for_each_direct_field!(get_arms)
}

pub fn set_field_value(pli: &mut PlaylistItem, field: ItemField, value: &str) -> bool {
    let header = &mut pli.header;

    macro_rules! set_arms {
        ($($variant:ident => $prop:ident),+ $(,)?) => {
            match field {
                $(ItemField::$variant => header.$prop = value.intern(),)+
                ItemField::Genre => {
                    return set_genre!(header, value);
                }
                ItemField::Caption => {
                    header.title = value.intern();
                    header.name = header.title.clone();
                }
                ItemField::EpgId => header.epg_channel_id = Some(value.intern()),
                ItemField::Chno => match value.parse::<u32>() {
                    Ok(chno) => header.chno = chno,
                    Err(_) => return false,
                },
                ItemField::Type | ItemField::Quality => {}
            }
        };
    }

    for_each_direct_field!(set_arms);
    true
}

fn header_quality_rank(header: &crate::model::PlaylistItemHeader) -> u8 {
    let caption = if header.title.is_empty() { &header.name } else { &header.title };
    crate::utils::quality_rank(caption)
}

pub struct ValueProvider<'a> {
    pub pli: &'a PlaylistItem,
    pub match_as_ascii: bool,
}

impl ValueProvider<'_> {
    pub fn quality_rank(&self) -> u8 { header_quality_rank(&self.pli.header) }

    pub(crate) fn get_filter_value(&self, field: ItemField) -> Option<Cow<'_, str>> {
        let header = &self.pli.header;
        if field == ItemField::Chno {
            return Some(Cow::Owned(header.chno.to_string()));
        }
        if field == ItemField::Quality {
            return Some(Cow::Owned(header_quality_rank(header).to_string()));
        }
        let value = match field {
            ItemField::Group => header.group.as_ref(),
            ItemField::Name => header.name.as_ref(),
            ItemField::Title => header.title.as_ref(),
            ItemField::Genre => header.additional_properties.as_ref().and_then(StreamProperties::genre)?.as_ref(),
            ItemField::Url => header.url.as_ref(),
            ItemField::Input => header.input_name.as_ref(),
            ItemField::Type => header.item_type.as_str(),
            ItemField::EpgId => header.epg_channel_id.as_deref()?,
            ItemField::Chno | ItemField::Quality => unreachable!("handled above"),
            ItemField::Caption => {
                if header.title.is_empty() {
                    header.name.as_ref()
                } else {
                    header.title.as_ref()
                }
            }
        };

        Some(if self.match_as_ascii { deunicode_string(value) } else { Cow::Borrowed(value) })
    }

    /// Typed sibling of [`Self::get`], for callers that already hold an
    /// `ItemField`.
    ///
    /// Skips the name parse and the string-comparison chain entirely: the
    /// header read becomes a match on a discriminant.
    pub fn get_typed(&self, field: ItemField) -> Option<Arc<str>> {
        // Virtual field: quality tier derived from the caption, not stored.
        if field == ItemField::Quality {
            return Some(header_quality_rank(&self.pli.header).to_string().intern());
        }
        let value = self.pli.header.get(field.header_field()?)?;
        if self.match_as_ascii {
            return Some(deunicode_string(value.as_cow().as_ref()).into_owned().into());
        }
        Some(value.to_arc())
    }

    pub fn get(&self, field: &str) -> Option<Arc<str>> {
        // Virtual field: quality tier derived from the caption, not stored on the header.
        if field.eq_ignore_ascii_case("quality") {
            return Some(header_quality_rank(&self.pli.header).to_string().intern());
        }
        let val = self.pli.header.get_field(field)?;
        if self.match_as_ascii {
            return Some(deunicode_string(&val).into_owned().into());
        }
        Some(val)
    }
}
