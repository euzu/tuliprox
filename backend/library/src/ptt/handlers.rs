use crate::ptt::{
    models::PttMetadata,
    parser::{handler_options, HandlerOptions, PttParser},
};

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn push_if_some<T>(values: &mut Vec<T>, value: Option<T>) {
    if let Some(value) = value {
        values.push(value);
    }
}

fn set_some<T>(slot: &mut Option<T>, value: T) { *slot = Some(value); }

fn set_some_if_present<T>(slot: &mut Option<T>, value: Option<T>) {
    if let Some(value) = value {
        *slot = Some(value);
    }
}

fn set_some_if_not_empty(slot: &mut Option<String>, value: String) {
    if !value.is_empty() {
        *slot = Some(value);
    }
}

fn push_language_and_en(meta: &mut PttMetadata, value: String) {
    push_unique(&mut meta.languages, value);
    push_unique(&mut meta.languages, "en".to_string());
}

macro_rules! gen_helper {
    ($name:ident, $apply_fn:expr, $field:ident) => {
        fn $name(meta: &mut PttMetadata, value: String) { $apply_fn(&mut meta.$field, value); }
    };
}

macro_rules! gen_option_helper {
    ($name:ident, $apply_fn:expr, $field:ident, $ty:ty) => {
        fn $name(meta: &mut PttMetadata, value: $ty) { $apply_fn(&mut meta.$field, value); }
    };
}

macro_rules! gen_value_helper {
    ($name:ident, $field:ident, $ty:ty) => {
        fn $name(meta: &mut PttMetadata, value: $ty) { set_value(&mut meta.$field, value); }
    };
}

macro_rules! gen_extend_if_present_helper {
    ($name:ident, $field:ident, $item_ty:ty) => {
        fn $name(meta: &mut PttMetadata, value: Option<Vec<$item_ty>>) {
            if let Some(value) = value {
                meta.$field.extend(value);
            }
        }
    };
}

macro_rules! gen_set_if_present_helper {
    ($name:ident, $field:ident, $ty:ty) => {
        fn $name(meta: &mut PttMetadata, value: Option<$ty>) {
            if let Some(value) = value {
                meta.$field = value;
            }
        }
    };
}

macro_rules! gen_quality_if_trash_helper {
    ($name:ident, $quality:literal) => {
        fn $name(meta: &mut PttMetadata, value: bool) {
            meta.trash = value;
            if value {
                meta.quality = Some($quality.to_string());
            }
        }
    };
}

gen_helper!(push_language, push_unique, languages);
gen_helper!(push_network, push_unique, networks);
gen_helper!(push_hdr, push_unique, hdr);
gen_helper!(push_channels, push_unique, channels);
gen_helper!(push_audio, push_unique, audio);
gen_helper!(set_group, set_some, group);
gen_helper!(set_container, set_some, container);
gen_helper!(set_resolution, set_some, resolution);
gen_helper!(set_episode_code, set_some, episode_code);
gen_helper!(set_bitrate, set_some, bitrate);
gen_helper!(set_quality, set_some, quality);
gen_helper!(set_region, set_some, region);
gen_helper!(set_codec, set_some, codec);
gen_helper!(set_edition, set_some, edition);
gen_helper!(set_site, set_some, site);
gen_helper!(set_country, set_some, country);
gen_helper!(set_bit_depth, set_some, bit_depth);
gen_helper!(set_size, set_some, size);
gen_helper!(set_extension, set_some, extension);
gen_helper!(set_date, set_some_if_not_empty, date);

gen_option_helper!(set_tmdb, set_some_if_present, tmdb, Option<u32>);
gen_option_helper!(set_tvdb, set_some_if_present, tvdb, Option<u32>);
gen_option_helper!(set_year, set_some_if_present, year, Option<u32>);
gen_option_helper!(push_season, push_if_some, seasons, Option<u32>);
gen_option_helper!(push_episode, push_if_some, episodes, Option<u32>);

gen_value_helper!(set_trash, trash, bool);
gen_value_helper!(set_is_3d, is_3d, bool);
gen_value_helper!(set_adult, adult, bool);
gen_value_helper!(set_complete, complete, bool);
gen_value_helper!(set_ppv, ppv, bool);
gen_value_helper!(set_subbed, subbed, bool);
gen_value_helper!(set_dubbed, dubbed, bool);
gen_value_helper!(set_upscaled, upscaled, bool);
gen_value_helper!(set_convert, convert, bool);
gen_value_helper!(set_hardcoded, hardcoded, bool);
gen_value_helper!(set_proper, proper, bool);
gen_value_helper!(set_repack, repack, bool);
gen_value_helper!(set_retail, retail, bool);
gen_value_helper!(set_extended, extended, bool);
gen_value_helper!(set_remastered, remastered, bool);
gen_value_helper!(set_documentary, documentary, bool);
gen_value_helper!(set_commentary, commentary, bool);
gen_value_helper!(set_unrated, unrated, bool);
gen_value_helper!(set_uncensored, uncensored, bool);
fn extend_seasons(meta: &mut PttMetadata, value: Vec<u32>) { meta.seasons.extend(value); }
gen_extend_if_present_helper!(extend_episodes, episodes, u32);
gen_set_if_present_helper!(set_volumes_if_present, volumes, Vec<i32>);
gen_quality_if_trash_helper!(set_quality_to_scr_if_trash, "SCR");
gen_quality_if_trash_helper!(set_quality_to_tele_sync_if_trash, "TeleSync");
gen_quality_if_trash_helper!(set_quality_to_tele_cine_if_trash, "TeleCine");
gen_quality_if_trash_helper!(set_quality_to_vhsrip_if_trash, "VHSRip");
gen_quality_if_trash_helper!(set_quality_to_vhs_if_trash, "VHS");
gen_quality_if_trash_helper!(set_quality_to_cam_if_trash, "CAM");
fn set_quality_remux(meta: &mut PttMetadata, value: String) {
    if let Some(q) = &meta.quality {
        if q.contains("BluRay") || q.contains("BRRip") || q.contains("BDRip") {
            meta.quality = Some("BluRay REMUX".to_string());
            return;
        }
    }
    meta.quality = Some(value);
}
fn set_quality_bluray(meta: &mut PttMetadata, value: String) {
    if let Some(q) = &meta.quality {
        if q.contains("REMUX") {
            meta.quality = Some("BluRay REMUX".to_string());
            return;
        }
    }
    meta.quality = Some(value);
}
fn set_year_with_trace(meta: &mut PttMetadata, value: Option<u32>) {
    if let Some(value) = value {
        meta.year = Some(value);
    }
}

fn set_value<T>(slot: &mut T, value: T) { *slot = value; }

fn ignore<T, U>(_: &mut T, _: U) {}

fn const_string(value: &'static str) -> impl Fn(&str) -> String + Copy { move |_| value.to_string() }

fn append_p(value: &str) -> String { format!("{value}p") }

fn mark_adult(meta: &mut PttMetadata, _: bool) { meta.adult = true; }

fn options_default() -> HandlerOptions { HandlerOptions::default() }

fn options_keep() -> HandlerOptions {
    handler_options! {
        remove: false,
        ..Default::default()
    }
}

fn options_remove() -> HandlerOptions {
    handler_options! {
        remove: true,
        ..Default::default()
    }
}

fn options_remove_skip_if_already_found() -> HandlerOptions {
    handler_options! {
        remove: true,
        skip_if_already_found: true,
        ..Default::default()
    }
}

fn options_remove_no_skip() -> HandlerOptions {
    handler_options! {
        remove: true,
        skip_if_already_found: false,
        ..Default::default()
    }
}

fn options_no_skip() -> HandlerOptions {
    handler_options! {
        skip_if_already_found: false,
        ..Default::default()
    }
}

fn options_keep_no_skip() -> HandlerOptions {
    handler_options! {
        remove: false,
        skip_if_already_found: false,
        ..Default::default()
    }
}

fn options_skip_from_title() -> HandlerOptions {
    handler_options! {
        skip_from_title: true,
        ..Default::default()
    }
}

fn options_keep_skip_if_first() -> HandlerOptions {
    handler_options! {
        remove: false,
        skip_if_first: true,
        ..Default::default()
    }
}

fn options_skip_from_title_no_skip() -> HandlerOptions {
    handler_options! {
        skip_from_title: true,
        skip_if_already_found: false,
        ..Default::default()
    }
}

fn options_remove_skip_from_title_no_skip() -> HandlerOptions {
    handler_options! {
        remove: true,
        skip_from_title: true,
        skip_if_already_found: false,
        ..Default::default()
    }
}

fn parse_season_range(val: &str) -> Vec<u32> {
    let nums: Vec<u32> = val.split(|c: char| !c.is_numeric()).filter_map(|s| s.parse::<u32>().ok()).collect();

    if nums.len() == 2 {
        let start = nums[0];
        let end = nums[1];
        if start < end && (end - start) < 100 {
            let lower = val.to_lowercase();
            if val.contains('-') || lower.contains("to") || lower.contains("thru") || val.contains(':') {
                return (start..=end).collect();
            }
        }
    }
    nums
}

pub fn add_defaults(parser: &mut PttParser) {
    identity::register(parser);
    dates_release::register(parser);
    technical::register(parser);
    episodes::register(parser);
    languages::register(parser);
    anime::register(parser);
    regional_languages::register(parser);
    release_suffix::register(parser);
}

mod anime;
mod dates_release;
mod episodes;
mod identity;
mod languages;
mod regional_languages;
mod release_suffix;
mod technical;
