use std::sync::Arc;
use log::{error, trace};
use crate::model::{FieldGetAccessor, FieldSetAccessor, ItemField, PlaylistItem};
use crate::utils::{deunicode_string, Internable};

#[macro_export]
macro_rules! set_genre {
    ($header:ident, $value:ident) => {
        if let Some(ref mut additional_properties) = $header.additional_properties {
                match additional_properties {
                    $crate::model::StreamProperties::Video(v) => {
                        if let Some(details) = &mut v.details {
                            details.genre = Some($value.intern());
                            true
                        } else {
                            v.details = Some($crate::model::VideoStreamDetailProperties {
                                genre: Some($value.intern()),
                                ..$crate::model::VideoStreamDetailProperties::default()
                            });
                            true
                        }
                    }
                    $crate::model::StreamProperties::Series(s) => {
                        s.genre = Some($value.intern());
                        true
                    }
                    $crate::model::StreamProperties::Live(_)
                    | $crate::model::StreamProperties::Episode(_) => false,
                }
            } else {
                let empty_str = "".intern();
                match $header.item_type {
                    $crate::model::PlaylistItemType::LocalVideo
                    | $crate::model::PlaylistItemType::Video => {
                        $header.additional_properties = Some($crate::model::StreamProperties::Video(Box::from($crate::model::VideoStreamProperties {
                            name: $header.title.clone(),
                            category_id: $header.category_id,
                            stream_id: $header.virtual_id,
                            stream_icon: $header.logo.clone(),
                            direct_source: ::std::sync::Arc::clone(&empty_str),
                            custom_sid: None,
                            added: ::std::sync::Arc::clone(&empty_str),
                            container_extension: $header.get_container_extension().unwrap_or_else(|| Arc::clone(&empty_str)),
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
                    $crate::model::PlaylistItemType::LocalSeriesInfo
                    | $crate::model::PlaylistItemType::SeriesInfo => {
                        $header.additional_properties = Some($crate::model::StreamProperties::Series(Box::from($crate::model::SeriesStreamProperties {
                            name: $header.title.clone(),
                            category_id: $header.category_id,
                            series_id: $header.virtual_id,
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
                        })));
                        true
                    }
                    _ => false,
                }
            }
    };
}

#[macro_export]
macro_rules! get_genre {
    ($header:ident) => {
        $header.additional_properties.as_ref().and_then(|props| {
            match props {
                $crate::model::StreamProperties::Video(v) => {
                    v.details.as_ref().and_then(|details| details.genre.as_ref().map(::std::sync::Arc::clone))
                }
                $crate::model::StreamProperties::Series(s) => { s.genre.as_ref().map(::std::sync::Arc::clone) }
                $crate::model::StreamProperties::Live(_)
                | $crate::model::StreamProperties::Episode(_) => None
            }
        })
    };
}

pub use set_genre;
pub use get_genre;

pub fn get_field_value(pli: &PlaylistItem, field: ItemField) -> Arc<str> {
    let header = &pli.header;
    match field {
        ItemField::Group => Arc::clone(&header.group),
        ItemField::Name => Arc::clone(&header.name),
        ItemField::Title => Arc::clone(&header.title),
        ItemField::Genre => get_genre!(header).unwrap_or_else(|| "".intern()),
        ItemField::Url => Arc::clone(&header.url),
        ItemField::Input => Arc::clone(&header.input_name),
        ItemField::Type => header.item_type.intern(),
        ItemField::Caption => if header.title.is_empty() { Arc::clone(&header.name) } else { Arc::clone(&header.title) },
    }
}

pub fn set_field_value(pli: &mut PlaylistItem, field: ItemField, value: String) -> bool {
    let header = &mut pli.header;
    match field {
        ItemField::Group => header.group = value.intern(),
        ItemField::Name => header.name = value.intern(),
        ItemField::Title => header.title = value.intern(),
        ItemField::Genre => {
            return set_genre!(header, value);
        }
        ItemField::Url => header.url = value.intern(),
        ItemField::Input => header.input_name = value.intern(),
        ItemField::Caption => {
            header.title = value.intern();
            header.name = header.title.clone();
        }
        ItemField::Type => {}
    }
    true
}

pub struct ValueProvider<'a> {
    pub pli: &'a PlaylistItem,
    pub match_as_ascii: bool,
}

impl ValueProvider<'_> {
    pub fn get(&self, field: &str) -> Option<Arc<str>> {
        let val = self.pli.header.get_field(field)?;
        if self.match_as_ascii {
            return Some(deunicode_string(&val).into_owned().into());
        }
        Some(val)
    }
}

pub struct ValueAccessor<'a> {
    pub pli: &'a mut PlaylistItem,
    pub virtual_items: Vec<(String, PlaylistItem)>,
    pub match_as_ascii: bool,
}

impl ValueAccessor<'_> {
    pub fn get(&self, field: &str) -> Option<Arc<str>> {
        let val = self.pli.header.get_field(field)?;
        if self.match_as_ascii {
            return Some(deunicode_string(&val).into_owned().into());
        }
        Some(val)
    }

    pub fn set(&mut self, field: &str, value: &str) {
        if self.pli.header.set_field(field, value) {
            trace!("Property {field} set to {value}");
        } else {
            error!("Can't set unknown field {field} set to {value}");
        }
    }
}