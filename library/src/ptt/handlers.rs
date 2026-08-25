use crate::ptt::{
    models::PttMetadata,
    parser::{handler_options, HandlerOptions, MatchInfo, ParseContext, PttParser},
    transformers::{
        boolean, date, first_uinteger, lowercase, none, range_i32, range_u32, transform_resolution, uinteger,
        uppercase, value,
    },
};
use fancy_regex::Regex as FancyRegex;

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

#[allow(clippy::too_many_lines)]
pub fn add_defaults(parser: &mut PttParser) {
    parser.add_handler(
        "tmdb",
        FancyRegex::new(r"(?i)\btmdb\b[-=]\d+").unwrap(),
        first_uinteger,
        set_tmdb,
        options_remove_skip_if_already_found(),
    );
    parser.add_handler(
        "tvdb",
        FancyRegex::new(r"(?i)\btvdb\b[-=]\d+").unwrap(),
        first_uinteger,
        set_tvdb,
        options_remove_skip_if_already_found(),
    );

    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bPRE[- .]?HDRip\b").unwrap(),
        boolean,
        set_quality_to_scr_if_trash,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bE[- ]?Sub\b").unwrap(),
        const_string("en"),
        push_language,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bTS-Screener\b").unwrap(),
        boolean,
        set_quality_to_tele_sync_if_trash,
        options_remove_no_skip(),
    );

    parser.add_handler(
        "year",
        FancyRegex::new(r"\b19\d{2}\s?-\s?20\d{2}\b").unwrap(),
        first_uinteger,
        set_year,
        options_keep(),
    );

    parser.add_handler(
        "title_cleanup",
        FancyRegex::new(r"(?i)\b(?:19|20)\d{2}\s*[-]\s*(?:(?:19|20)\d{2}|\d{2})\b").unwrap(),
        none,
        ignore,
        options_remove(),
    );

    parser.add_handler(
        "title_cleanup",
        FancyRegex::new(r"(?i)\b100[ .-]*years?[ .-]*quest\b").unwrap(),
        none,
        ignore,
        options_remove(),
    );
    parser.add_handler(
        "title_cleanup",
        FancyRegex::new(r"(?i)\[?(\+.)?Extras\]?").unwrap(),
        none,
        ignore,
        options_remove(),
    );
    parser.add_handler(
        "title_cleanup",
        FancyRegex::new(r"(?i)(\+Movies)?\+Specials").unwrap(),
        none,
        ignore,
        options_remove(),
    );
    parser.add_handler(
        "group",
        FancyRegex::new(r"-?EDGE2020").unwrap(),
        const_string("EDGE2020"),
        set_group,
        options_remove(),
    );
    parser.add_handler("title_cleanup", FancyRegex::new(r"(?i)TV Money").unwrap(), none, ignore, options_remove());

    parser.add_handler(
        "container",
        FancyRegex::new(r"(?i)\.?[\[(]?\b(MKV|AVI|MP4|WMV|MPG|MPEG)\b[\])]?").unwrap(),
        lowercase,
        set_container,
        options_default(),
    );

    parser.add_handler("torrent", FancyRegex::new(r"\.torrent$").unwrap(), boolean, ignore, options_remove());

    parser.add_handler("adult", FancyRegex::new(r"\b(XXX|xxx|Xxx)\b").unwrap(), boolean, set_adult, options_remove());

    if let Ok(re) = FancyRegex::new(r"(?i)\b(18\+|adult|porn|xxx)\b") {
        parser.add_handler("adult", re, boolean, mark_adult, options_default());
    }

    parser.add_handler(
        "extras",
        FancyRegex::new(r"(?i)\bOVA\b").unwrap(),
        const_string("OVA"),
        ignore,
        options_remove(),
    );

    parser.add_handler(
        "extras",
        FancyRegex::new(r"(?i)\bOVA\b").unwrap(),
        const_string("OVA"),
        ignore,
        options_remove(),
    );

    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)\[?\]?3840x\d{4}[\])?]?").unwrap(),
        const_string("2160p"),
        set_resolution,
        options_remove(),
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)\[?\]?1920x\d{3,4}[\])?]?").unwrap(),
        const_string("1080p"),
        set_resolution,
        options_remove(),
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)\[?\]?1280x\d{3}[\])?]?").unwrap(),
        const_string("720p"),
        set_resolution,
        options_remove(),
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)\[?\]?(\d{3,4}x\d{3,4})[\])?]?p?").unwrap(),
        append_p,
        set_resolution,
        options_remove(),
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)(480|720|1080)0[pi]").unwrap(),
        append_p,
        set_resolution,
        options_remove(),
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)(?:QHD|QuadHD|WQHD|2560(\d+)?x(\d+)?1440p?)").unwrap(),
        const_string("1440p"),
        set_resolution,
        options_remove(),
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)(?:Full HD|FHD|1920(\d+)?x(\d+)?1080p?)").unwrap(),
        const_string("1080p"),
        set_resolution,
        options_remove(),
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)(?:BD|HD|M)(2160p?|4k)").unwrap(),
        const_string("2160p"),
        set_resolution,
        options_remove(),
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)(?:BD|HD|M)1080p?").unwrap(),
        const_string("1080p"),
        set_resolution,
        options_remove(),
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)(?:BD|HD|M)720p?").unwrap(),
        const_string("720p"),
        set_resolution,
        options_remove(),
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)(?:BD|HD|M)480p?").unwrap(),
        const_string("480p"),
        set_resolution,
        options_remove(),
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)\b(?:4k|2160p|1080p|720p|480p)(?!.*\b(?:4k|2160p|1080p|720p|480p)\b)").unwrap(),
        transform_resolution,
        set_resolution,
        options_remove(),
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)\b4k|21600?[pi]\b").unwrap(),
        const_string("2160p"),
        set_resolution,
        options_remove(),
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)(\d{3,4}[pi])").unwrap(),
        lowercase,
        set_resolution,
        options_remove(),
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)(240|360|480|576|720|1080|2160|3840)[pi]").unwrap(),
        lowercase,
        set_resolution,
        options_remove(),
    );

    parser.add_handler(
        "episode_code",
        FancyRegex::new(r"[\[\()]([A-Fa-f0-9]{8})[\]\)]").unwrap(),
        uppercase,
        set_episode_code,
        options_remove(),
    );
    parser.add_handler(
        "episode_code",
        FancyRegex::new(r"[\[\()]([0-9]{8})[\]\)]").unwrap(),
        uppercase,
        set_episode_code,
        options_remove_skip_if_already_found(),
    );

    parser.add_handler(
        "trash",
        FancyRegex::new(
            r"(?i)\b(?:H[DQ][ .-]*)?(?<!Body\s)CAM(?:H[DQ])?(?!.?(S|E|\()\d+)(?:H[DQ])?(?:[ .-]*Rip|Rp)?\b",
        )
        .unwrap(),
        boolean,
        set_trash,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\b(?:H[DQ][ .-]*)?TS(?:H[DQ])?(?:[ .-]*Rip|Rp)?\b").unwrap(),
        boolean,
        set_quality_to_tele_sync_if_trash,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\b(?:H[DQ][ .-]*)?TC(?:H[DQ])?(?:[ .-]*Rip|Rp)?\b").unwrap(),
        boolean,
        set_quality_to_tele_cine_if_trash,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\b(?:H[DQ][ .-]*)?P(?:re)?DVD[ .-]*Rip\b").unwrap(),
        boolean,
        set_quality_to_scr_if_trash,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\b(?:H[DQ][ .-]*)?(?:DVD|WEB|BR|HD)?Scr(?:eener)?\b").unwrap(),
        boolean,
        set_quality_to_scr_if_trash,
        options_keep_no_skip(),
    );
    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\bVHSRip\b").unwrap(),
        boolean,
        set_quality_to_vhsrip_if_trash,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\bVHS\b").unwrap(),
        boolean,
        set_quality_to_vhs_if_trash,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\b(?:H[DQ][ .-]*)?R5(?:[ .-]*Line)?\b").unwrap(),
        boolean,
        set_trash,
        options_keep(),
    );
    parser.add_handler("trash", FancyRegex::new(r"(?i)\bVHSRip\b").unwrap(), boolean, set_trash, options_keep());

    // parser.add_handler(
    //     "trash",
    //     FancyRegex::new(r"(?i)\bHDTV(?:Rip)?\b").unwrap(),
    //     boolean,
    //     |meta, val| { println!("TRASH MATCH HDTV: {}", val); meta.trash = val; },
    //     handler_options! { remove: false, ..Default::default() }
    // );

    parser.add_handler(
        "date",
        FancyRegex::new(r"(?:\W|^)([\[(]?(?:19[6-9]|20[012])[0-9]([. \-/\\])(?:0[1-9]|1[012])\2(?:0[1-9]|[12][0-9]|3[01])[\])]?)(?:\W|$)").unwrap(),
        |val| date(val, &["%Y-%m-%d", "%Y.%m.%d", "%Y %m %d"]).unwrap_or_default(),
        set_date,
        options_remove(),
    );
    parser.add_handler(
        "date",
        FancyRegex::new(r"(?:\W|^)(\[?\]?(?:0[1-9]|[12][0-9]|3[01])([. \-/\\])(?:0[1-9]|1[012])\2(?:19[6-9]|20[01])[0-9][\])]?)(?:\W|$)").unwrap(),
        |val| date(val, &["%d-%m-%Y", "%d.%m.%Y", "%d %m %Y"]).unwrap_or_default(),
        set_date,
        options_remove(),
    );

    parser.add_handler(
        "date",
        FancyRegex::new(r"(?:\W)(\[?\]?(?:0[1-9]|1[012])([. \-/\\])(?:0[1-9]|[12][0-9]|3[01])\2(?:[0][1-9]|[0126789][0-9])[\])]?)(?:\W|$)").unwrap(),
        |val| date(val, &["%m %d %y", "%m.%d.%y", "%m-%d-%y"]).unwrap_or_default(),
        set_date,
        options_remove(),
    );
    parser.add_handler(
        "date",
        FancyRegex::new(r"(?:\W)(\[?\]?(?:[0][1-9]|[12][0-9]|3[0-9])([. \-/\\])(?:0[1-9]|1[012])\2(?:0[1-9]|[12][0-9])[\])]?)(?:\W|$)").unwrap(),
        |val| date(val, &["%y %m %d", "%y.%m.%d", "%y-%m-%d"]).unwrap_or_default(),
        set_date,
        options_remove(),
    );
    parser.add_handler(
        "date",
        FancyRegex::new(r"(?:\W)(\[?\]?(?:0[1-9]|[12][0-9]|3[01])([. \-/\\])(?:0[1-9]|1[012])\2(?:[0][1-9]|[0126789][0-9])[\])]?)(?:\W|$)").unwrap(),
        |val| date(val, &["%d %m %y", "%d.%m.%y", "%d-%m-%y"]).unwrap_or_default(),
        set_date,
        options_remove(),
    );
    parser.add_handler(
        "date",
        FancyRegex::new(r"(?i)(?:\W|^)([(\[]?(?:0?[1-9]|[12][0-9]|3[01])[. ]?(?:st|nd|rd|th)?([. \-/\\])(?:feb(?:ruary)?|jan(?:uary)?|mar(?:ch)?|apr(?:il)?|may|june?|july?|aug(?:ust)?|sept?(?:ember)?|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?)\2(?:19[7-9]|20[012])[0-9][)\]]?)(?=\W|$)").unwrap(),
        |val| date(val, &["%d %b %Y", "%d %B %Y", "%d.%b.%Y", "%d.%B.%Y", "%d-%b-%Y", "%d-%B-%Y"]).unwrap_or_default(),
        set_date,
        options_remove(),
    );
    parser.add_handler(
        "date",
        FancyRegex::new(r"(?i)(?:\W|^)(\[?\]?(?:0?[1-9]|[12][0-9]|3[01])[. ]?(?:st|nd|rd|th)?([. \-\/\\])(?:feb(?:ruary)?|jan(?:uary)?|mar(?:ch)?|apr(?:il)?|may|june?|july?|aug(?:ust)?|sept?(?:ember)?|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?)\2(?:0[1-9]|[0126789][0-9])[\])]?)(?:\W|$)").unwrap(),
        |val| date(val, &["%d %b %y", "%d %B %y", "%d.%b.%y", "%d.%B.%y", "%d-%b-%y", "%d-%B-%y"]).unwrap_or_default(),
        set_date,
        options_remove(),
    );
    parser.add_handler(
        "date",
        FancyRegex::new(r"(?:\W|^)(\[?\]?20[012][0-9](?:0[1-9]|1[012])(?:0[1-9]|[12][0-9]|3[01])[\])]?)(?:\W|$)")
            .unwrap(),
        |val| date(val, &["%Y%m%d"]).unwrap_or_default(),
        set_date,
        options_remove(),
    );
    parser.add_handler(
        "date",
        FancyRegex::new(r"(?i)(?:\W|^)((?:0?[1-9]|[12][0-9]|3[01])(?:st|nd|rd|th)\s+(?:Jan(?:uary)?|Feb(?:ruary)?|Mar(?:ch)?|Apr(?:il)?|May|June?|July?|Aug(?:ust)?|Sept?(?:ember)?|Oct(?:ober)?|Nov(?:ember)?|Dec(?:ember)?)\s+(?:19[7-9]|20[012])[0-9])(?=\W|$)").unwrap(),
        |val| {
            let clean = val.replace("st ", " ").replace("nd ", " ").replace("rd ", " ").replace("th ", " ");
            date(&clean, &["%d %b %Y", "%d %B %Y"]).unwrap_or_default()
        },
        set_date,
        options_remove(),
    );

    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)\b((?:19\d|20[012])\d[ .]?-[ .]?(?:19\d|20[012])\d)\b").unwrap(),
        boolean,
        set_complete,
        options_remove(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)[(\[][ .]?((?:19\d|20[012])\d[ .]?-[ .]?\d{2})[ .]?[)\]]").unwrap(),
        boolean,
        set_complete,
        options_remove(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)\bcomplete\b").unwrap(),
        boolean,
        set_complete,
        options_remove(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)\b(?:INTEGRALE?|INTÉGRALE?)\b").unwrap(),
        boolean,
        set_complete,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)(Movie|Complete).Collection").unwrap(),
        boolean,
        set_complete,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)Complete(.\d{1,2})").unwrap(),
        boolean,
        set_complete,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)(?:\bthe\W)?(?:\bcomplete|collection|dvd)?\b[ .]?\bbox[ .-]?set\b").unwrap(),
        boolean,
        set_complete,
        options_remove(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)(?:\bthe\W)?(?:\bcomplete|collection|dvd)?\b[ .]?\bmini[ .-]?series\b").unwrap(),
        boolean,
        set_complete,
        options_default(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)(?:\bthe\W)?(?:\bcomplete\b|\bfull\b|\ball\b)\b.*\b(?:series|seasons|collection|episodes|set|pack|movies)\b").unwrap(),
        boolean,
        set_complete,
        options_default(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)(Top\W+)?\d+\W+(movies?|series|seasons?)\W+Collection").unwrap(),
        boolean,
        set_complete,
        options_remove(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)(?:\bthe\W)?\bultimate\b[ .]\bcollection\b").unwrap(),
        boolean,
        set_complete,
        options_no_skip(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)\bcollection\b.*\b(?:set|pack|movies)\b").unwrap(),
        boolean,
        set_complete,
        options_default(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)\bcollection(?:(\s\[|\s\())").unwrap(),
        boolean,
        set_complete,
        options_remove(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)duology|trilogy|quadr[oi]logy|tetralogy|pentalogy|hexalogy|heptalogy|anthology").unwrap(),
        boolean,
        set_complete,
        options_no_skip(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)\bcompleta\b").unwrap(),
        boolean,
        set_complete,
        options_remove(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)\bsaga\b").unwrap(),
        boolean,
        set_complete,
        options_skip_from_title(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)\b\[Complete\]\b").unwrap(),
        boolean,
        set_complete,
        options_remove(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)(?<!A.?|The.?)\bComplete\b").unwrap(),
        boolean,
        set_complete,
        options_remove(),
    );

    parser.add_handler(
        "bitrate",
        FancyRegex::new(r"(?i)\b\d+[kmg]bps\b").unwrap(),
        lowercase,
        set_bitrate,
        options_remove(),
    );

    parser.add_handler(
        "year",
        FancyRegex::new(r"(?:^|[^-])\b(20[0-9]{2}|2100)(?!(?:\s*[-]\s*\d{4}|\s*\d{4})\b)").unwrap(),
        uinteger,
        set_year,
        options_remove(),
    );

    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?:\b[ée]p?(?:isode)?|[Ээ]пизод|[Сс]ер(?:ии|ия|\.)?|cap(?:itulo)?|epis[oó]dio)[. ]?[-:#№]?[. ]?(\d{1,4})(?:[abc]|v0?[1-4]|\W|$)").unwrap(),
        uinteger,
        push_episode,
        options_keep(),
    );

    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\b\d+[0o]+[mg]b\b").unwrap(),
        boolean,
        set_trash,
        options_remove(),
    );

    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:(?:D[ .])?HD[ .-]*)?T(?:ELE)?S(?:YNC)?(?:Rip)?\b").unwrap(),
        const_string("TeleSync"),
        set_quality,
        options_remove(),
    );

    parser.add_handler(
        "season",
        FancyRegex::new(r"(?i)\b(\d{1,2})x\d{1,2}\b").unwrap(),
        uinteger,
        push_season,
        options_remove(),
    );
    parser.add_handler(
        "episode",
        FancyRegex::new(r"(?i)\b\d{1,2}x(\d{1,2})\b").unwrap(),
        uinteger,
        push_episode,
        options_remove(),
    );
    parser.add_handler(
        "year",
        FancyRegex::new(r"(?i)[^SE][\[(]?(?!^)(?<![\d-]|Cap[.]?|Ep[.]?)((?:19\d|20[012])\d)(?!(?:\s*[-]\s*\d{4}|\s*\d{4}|kbps)\b)[)\]]?").unwrap(),
        uinteger,
        set_year,
        options_remove(),
    );
    parser.add_handler(
        "year",
        FancyRegex::new(r"(?i)(?!^\w{4})^[(\[]?((?:19\d|20[012])\d)(?!(?:\s*[-]\s*\d{4}|\s*\d{4}|kbps)\b)[)\]]?")
            .unwrap(),
        uinteger,
        set_year_with_trace,
        options_remove(),
    );

    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\b\d{2,3}(th)?[\.\s\-\+_\/(),]Anniversary[\.\s\-\+_\/(),](Edition|Ed)?\b").unwrap(),
        const_string("Anniversary Edition"),
        set_edition,
        options_remove(),
    );
    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\bRemaster(?:ed)?\b").unwrap(),
        const_string("Remastered"),
        set_edition,
        options_remove_skip_if_already_found(),
    );

    parser.add_handler(
        "upscaled",
        FancyRegex::new(r"(?i)\b(?:AI.?)?(Upscal(ed?|ing)|Enhanced?)\b").unwrap(),
        boolean,
        set_upscaled,
        options_keep(),
    );

    parser.add_handler("convert", FancyRegex::new(r"\bCONVERT\b").unwrap(), boolean, set_convert, options_remove());

    parser.add_handler(
        "hardcoded",
        FancyRegex::new(r"\b(HC|HARDCODED)\b").unwrap(),
        boolean,
        set_hardcoded,
        options_remove(),
    );

    parser.add_handler(
        "proper",
        FancyRegex::new(r"(?i)\b(?:REAL.)?PROPER\b").unwrap(),
        boolean,
        set_proper,
        options_remove(),
    );

    parser.add_handler(
        "repack",
        FancyRegex::new(r"(?i)\bREPACK|RERIP\b").unwrap(),
        boolean,
        set_repack,
        options_remove(),
    );

    parser.add_handler("retail", FancyRegex::new(r"(?i)\bRetail\b").unwrap(), boolean, set_retail, options_remove());

    parser.add_handler(
        "remastered",
        FancyRegex::new(r"(?i)\bRemaster(?:ed)?\b").unwrap(),
        boolean,
        set_remastered,
        options_remove(),
    );

    parser.add_handler(
        "documentary",
        FancyRegex::new(r"(?i)\bDOCU(?:menta?ry)?\b").unwrap(),
        boolean,
        set_documentary,
        handler_options! {
            skip_from_title: true,
            ..Default::default()
        },
    );

    parser.add_handler("unrated", FancyRegex::new(r"(?i)\bunrated\b").unwrap(), boolean, set_unrated, options_remove());

    parser.add_handler(
        "uncensored",
        FancyRegex::new(r"(?i)\buncensored\b").unwrap(),
        boolean,
        set_uncensored,
        options_remove(),
    );

    parser.add_handler(
        "commentary",
        FancyRegex::new(r"(?i)\bcommentary\b").unwrap(),
        boolean,
        set_commentary,
        options_remove(),
    );

    parser.add_handler("region", FancyRegex::new(r"R\dJ?\b").unwrap(), uppercase, set_region, options_remove());
    parser.add_handler(
        "region",
        FancyRegex::new(r"(?i)\b(PAL|NTSC|SECAM)\b").unwrap(),
        uppercase,
        set_region,
        options_remove(),
    );

    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:HD[ .-]*)?T(?:ELE)?S(?:YNC)?(?:Rip)?\b").unwrap(),
        const_string("TeleSync"),
        set_quality,
        options_remove(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:BD|Blu-?Ray|UHD|4K)[ .-]*(?:Remux)\b").unwrap(),
        const_string("BluRay REMUX"),
        set_quality,
        options_remove(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:UHD|BD)Remux\b").unwrap(),
        const_string("BluRay REMUX"),
        set_quality,
        options_remove(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bBlu[ .-]*Ray[ .-]*Rip\b").unwrap(),
        const_string("BRRip"),
        set_quality,
        options_remove(),
    );

    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bremux\b").unwrap(),
        const_string("REMUX"),
        set_quality_remux,
        options_remove_no_skip(),
    );

    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bBlu[ .-]*Ray\b(?![ .-]*Rip)").unwrap(),
        const_string("BluRay"),
        set_quality_bluray,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:HD)?TC(?:Rip)?\b").unwrap(),
        const_string("TeleCine"),
        set_quality,
        options_remove(),
    );

    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bUHD[ .-]*Rip\b").unwrap(),
        const_string("UHDRip"),
        set_quality,
        options_remove(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bR5\b").unwrap(),
        const_string("R5"),
        set_quality,
        options_remove(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:BD|Blu-?Ray)(?:Rip)?\b").unwrap(),
        const_string("BDRip"),
        set_quality,
        options_remove(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bWEB[ .-]*(?:DLRip|DL-?Rip)\b").unwrap(),
        const_string("WEB-DLRip"),
        set_quality,
        options_remove(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:BD|Blu-?Ray|UHD|4K)[ .-]*(?:Remux)\b").unwrap(),
        const_string("BluRay REMUX"),
        set_quality,
        options_remove(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:UHD|BD)Remux\b").unwrap(),
        const_string("BluRay REMUX"),
        set_quality,
        options_remove(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bWEB[ .-]*(DL|.BDrip)\b").unwrap(),
        const_string("WEB-DL"),
        set_quality,
        options_remove(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?<!\w.)WEB\b|\bWEB(?!([ \.\-\(\],]+\d))\b").unwrap(),
        const_string("WEB"),
        set_quality,
        handler_options! {
            remove: true,
            skip_from_title: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:HD[ .-]*)?DVD[ .-]*Rip\b").unwrap(),
        const_string("DVDRip"),
        set_quality,
        options_remove(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bHD-?DVD-?Rip\b").unwrap(),
        const_string("DVDRip"),
        set_quality,
        options_remove(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:DVD?|BD|BR|HD)?[ .-]*Scr(?:eener)?\b").unwrap(),
        const_string("SCR"),
        set_quality,
        options_remove(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bDVD(?:R\d?|.*Mux)?\b").unwrap(),
        const_string("DVD"),
        set_quality,
        options_remove(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:H[DQ][ .-]*)?S[ \.\-]print\b").unwrap(),
        const_string("CAM"),
        set_quality,
        handler_options! {
            remove: true,
            skip_from_title: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b4K[ .-]*UHD[ .-]*remux\b").unwrap(),
        const_string("BluRay REMUX"),
        set_quality,
        options_remove(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:HD)?CAM(?:-?Rip)?\b").unwrap(),
        const_string("CAM"),
        set_quality,
        handler_options! {
            remove: true,
            skip_from_title: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "bit_depth",
        FancyRegex::new(r"(?i)\bhevc\s?10\b").unwrap(),
        const_string("10bit"),
        set_bit_depth,
        options_default(),
    );
    parser.add_handler(
        "bit_depth",
        FancyRegex::new(r"(?i)(?:8|10|12)[-\.]?(?=bit\b)").unwrap(),
        |val| format!("{val}bit"),
        set_bit_depth,
        options_remove(),
    );

    parser.add_handler(
        "hdr",
        FancyRegex::new(r"(?i)\bDV\b|dolby.?vision|\bDoVi\b").unwrap(),
        const_string("DV"),
        push_hdr,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "hdr",
        FancyRegex::new(r"(?i)HDR10(?:\+|[-\.\s]?plus)").unwrap(),
        const_string("HDR10+"),
        push_hdr,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "hdr",
        FancyRegex::new(r"(?i)\bHDR(?:10)?\b").unwrap(),
        const_string("HDR"),
        push_hdr,
        options_remove_no_skip(),
    );

    parser.add_handler(
        "codec",
        FancyRegex::new(r"(?i)\b[hx][\. \-]?264\b").unwrap(),
        const_string("avc"),
        set_codec,
        options_remove(),
    );
    parser.add_handler(
        "codec",
        FancyRegex::new(r"(?i)\[AVC\]").unwrap(),
        const_string("avc"),
        set_codec,
        options_remove(),
    );
    parser.add_handler(
        "codec",
        FancyRegex::new(r"(?i)\[HEVC\]").unwrap(),
        const_string("hevc"),
        set_codec,
        options_remove(),
    );
    parser.add_handler(
        "codec",
        FancyRegex::new(r"(?i)\bAVC[_\s]").unwrap(),
        const_string("avc"),
        set_codec,
        options_keep(),
    );
    parser.add_handler(
        "codec",
        FancyRegex::new(r"(?i)\bHEVC10(bit)?\b|\b[xh][\. \-]?265\b").unwrap(),
        const_string("hevc"),
        set_codec,
        options_remove(),
    );
    parser.add_handler(
        "codec",
        FancyRegex::new(r"(?i)\bhevc(?:\s?10)?\b").unwrap(),
        const_string("hevc"),
        set_codec,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "codec",
        FancyRegex::new(r"(?i)\bav1\b").unwrap(),
        const_string("av1"),
        set_codec,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "codec",
        FancyRegex::new(r"(?i)\b(?:mpe?g\d*)\b").unwrap(),
        const_string("mpeg"),
        set_codec,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "codec",
        FancyRegex::new(r"\b\W264\W\b").unwrap(),
        const_string("avc"),
        set_codec,
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            skip_from_title: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "codec",
        FancyRegex::new(r"\b\W265\W\b").unwrap(),
        const_string("hevc"),
        set_codec,
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            skip_from_title: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "codec",
        FancyRegex::new(r"(?i)\bdivx|xvid\b").unwrap(),
        const_string("xvid"),
        set_codec,
        options_remove_no_skip(),
    );

    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\bOrg(?:inal)?\W+Aud(?:io)?\b").unwrap(),
        const_string("Original Audio"),
        push_audio,
        options_remove(),
    );
    parser.add_handler(
        "channels",
        FancyRegex::new(r"(?i)5[\.\s]1(?:ch|-S\d+)?\b").unwrap(),
        const_string("5.1"),
        push_channels,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\b(custom.?)?Extended\b").unwrap(),
        const_string("Extended Edition"),
        set_edition,
        options_remove(),
    );
    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\buncut(?!.gems)\b").unwrap(),
        const_string("Uncut"),
        set_edition,
        options_remove(),
    );
    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\bRemaster(?:ed)?\b").unwrap(),
        const_string("Remastered"),
        set_edition,
        options_remove_skip_if_already_found(),
    );
    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\bDirector(')?s.?Cut\b").unwrap(),
        const_string("Directors Cut"),
        set_edition,
        options_remove(),
    );
    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\bCollector(')?s\b").unwrap(),
        const_string("Collectors Edition"),
        set_edition,
        options_remove(),
    );
    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\bTheatrical\b").unwrap(),
        const_string("Theatrical"),
        set_edition,
        options_remove(),
    );
    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\bIMAX\b").unwrap(),
        const_string("IMAX"),
        set_edition,
        options_remove(),
    );
    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\bUltimate[\.\s\-\+_\/(),]Edition\b").unwrap(),
        const_string("Ultimate Edition"),
        set_edition,
        options_remove(),
    );

    parser.add_handler(
        "ppv",
        FancyRegex::new(r"(?i)\bPPV\b").unwrap(),
        boolean,
        set_ppv,
        handler_options! {
            remove: true,
            skip_from_title: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "ppv",
        FancyRegex::new(r"(?i)\b\W?Fight.?Nights?\W?\b").unwrap(),
        boolean,
        set_ppv,
        handler_options! {
            skip_from_title: true,
            ..Default::default()
        },
    );

    parser.add_handler("proper", FancyRegex::new(r"(?i)\bPROPER\b").unwrap(), boolean, set_proper, options_remove());
    parser.add_handler("repack", FancyRegex::new(r"(?i)\bREPACK\b").unwrap(), boolean, set_repack, options_remove());
    parser.add_handler("retail", FancyRegex::new(r"(?i)\bRetail\b").unwrap(), boolean, set_retail, options_remove());
    parser.add_handler(
        "extended",
        FancyRegex::new(r"(?i)\bEXTENDED\b").unwrap(),
        boolean,
        set_extended,
        options_remove(),
    );
    parser.add_handler(
        "remastered",
        FancyRegex::new(r"(?i)\bRemastered\b").unwrap(),
        boolean,
        set_remastered,
        options_remove(),
    );
    parser.add_handler(
        "unrated",
        FancyRegex::new(r"(?i)\b(?:uncensored|unrated)\b").unwrap(),
        boolean,
        set_unrated,
        options_remove(),
    );
    parser.add_handler(
        "uncensored",
        FancyRegex::new(r"(?i)\buncensored\b").unwrap(),
        boolean,
        set_uncensored,
        options_remove(),
    );

    parser.add_handler(
        "site",
        FancyRegex::new(r"(?i)^(www?[., ][\w-]+[. ][\w-]+(?:[. ][\w-]+)?)\s+-\s*").unwrap(),
        |val| val.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '.').trim().to_string(),
        set_site,
        handler_options! {
            remove: true,
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "site",
        FancyRegex::new(r"(?i)^\[\s*([\w.-]+\.[a-z]{2,4})\s*\]").unwrap(),
        std::string::ToString::to_string,
        set_site,
        handler_options! {
            remove: true,
            skip_from_title: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "site",
        FancyRegex::new(r"(?i)\[\s*([\w.-]+\.[a-z]{2,4})\s*\]$").unwrap(),
        std::string::ToString::to_string,
        set_site,
        handler_options! {
            remove: true,
            skip_from_title: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "site",
        FancyRegex::new(r"(?i)\[([^\]]+\.[^\]]+)\](?=\.\w{2,4}$|\s)").unwrap(),
        std::string::ToString::to_string,
        set_site,
        handler_options! {
            remove: true,
            skip_from_title: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "site",
        FancyRegex::new(r"(?i)^((?:www?[\.,])?[\w-]+\.[\w-]+(?:\.[\w-]+)*?)\s+-\s*").unwrap(),
        |val| val.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '.').trim().to_string(),
        set_site,
        options_no_skip(),
    );

    parser.add_handler(
        "year",
        FancyRegex::new(r"(?i)\b(19\d{2}\s?-\s?20\d{2})\b").unwrap(),
        |val| Some(val.split(['-', ' ']).next().unwrap().parse::<u32>().unwrap()),
        set_year,
        options_keep(),
    );
    parser.add_handler(
        "channels",
        FancyRegex::new(r"(?i)\b(?:x[2-4]|5[\W]1(?:x[2-4])?)\b").unwrap(),
        const_string("5.1"),
        push_channels,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "channels",
        FancyRegex::new(r"(?i)\b7[\.\- ]1(.?ch(annel)?)?\b").unwrap(),
        const_string("7.1"),
        push_channels,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "channels",
        FancyRegex::new(r"(?i)\+?2[\.\s]0(?:x[2-4])?\b").unwrap(),
        const_string("2.0"),
        push_channels,
        options_remove_no_skip(),
    );

    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\b(?!.+HR)(DTS.?HD.?Ma(ster)?|DTS.?X)\b").unwrap(),
        const_string("DTS Lossless"),
        push_audio,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\bDTS(?!(.?HD.?Ma(ster)?|.X)).?(HD.?HR|HD)?\b").unwrap(),
        const_string("DTS Lossy"),
        push_audio,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\b(Dolby.?)?Atmos\b").unwrap(),
        const_string("Atmos"),
        push_audio,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\b(True[ .-]?HD|\.True\.)\b").unwrap(),
        const_string("TrueHD"),
        push_audio,
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            skip_from_title: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\bTRUE\b").unwrap(),
        const_string("TrueHD"),
        push_audio,
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            skip_from_title: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\bFLAC(?:\d+(?:\.\d+)?)?(?:x\d+)?").unwrap(),
        const_string("FLAC"),
        push_audio,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)DD2?[\+p]|DD Plus|Dolby Digital Plus|DDP(5[ \.\_]1)?|E-?AC-?3(?:-S\d+)?").unwrap(),
        const_string("Dolby Digital Plus"),
        push_audio,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\bddp(5.1)?").unwrap(),
        const_string("Dolby Digital Plus"),
        push_audio,
        options_remove_skip_if_already_found(),
    );
    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\bMP3\b").unwrap(),
        const_string("MP3"),
        push_audio,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\b(DD|Dolby.?Digital|DolbyD|AC-?3(x2)?(?:-S\d+)?)\b").unwrap(),
        const_string("Dolby Digital"),
        push_audio,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\bQ?Q?AAC(x?2)?\b").unwrap(),
        const_string("AAC"),
        push_audio,
        options_remove_no_skip(),
    );

    parser.add_handler(
        "group",
        FancyRegex::new(r"(?i)- ?(?!\d+$|S\d+|\d+x|ep?\d+|[^\[]+]$)([^\-. \[]+[^\-. \[)\]\d][^\-. \[)\]]*)(?:\[[\w.-]+])?(?=\.\w{2,4}$|$)").unwrap(),
        value,
        set_group,
        options_keep(),
    );
    parser.add_handler(
        "group",
        FancyRegex::new(r"\(([\w-]+)\)(?:$|\.\w{2,4}$)").unwrap(),
        value,
        set_group,
        options_default(),
    );
    parser.add_handler("group", FancyRegex::new(r"^\[([^\[\]]+)\]").unwrap(), value, set_group, options_default());

    parser.add_handler(
        "volumes",
        FancyRegex::new(r"(?i)\bvol(?:s|umes?)?[. -]*(?:\d{1,2}[., +/\\&-]+)+\d{1,2}\b").unwrap(),
        range_i32,
        set_volumes_if_present,
        options_remove(),
    );
    let volume_regex = FancyRegex::new(r"(?i)\bvol(?:ume)?[. -]*(\d{1,2})\b").unwrap();
    parser.add_handler_fn(
        "volumes",
        Box::new(move |context: &mut ParseContext| -> Option<MatchInfo> {
            let title = &context.title;
            let matched = &context.matched;

            let start_index = matched.get("year").map_or(0, |m| m.match_index);

            if start_index >= title.len() {
                return None;
            }

            let search_slice = &title[start_index..];

            if let Ok(Some(m)) = volume_regex.find(search_slice) {
                let raw_match = m.as_str().to_string();
                let relative_start = m.start();

                if let Ok(Some(cap)) = volume_regex.captures(search_slice) {
                    let volume_number = cap.get(1).map_or(0, |m| m.as_str().parse::<i32>().unwrap_or(0));

                    context.result.volumes = vec![volume_number];
                }

                let abs_start = start_index + relative_start;

                let info = MatchInfo { raw_match, match_index: abs_start, remove: true, skip_from_title: false };

                context.matched.insert("volumes".to_string(), info.clone());
                return Some(info);
            }
            None
        }),
    );

    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:complete\W|seasons?\W|\W|^)((?:s\d{1,2}[., +/\\&-]+)+s\d{1,2}\b)").unwrap(),
        parse_season_range,
        extend_seasons,
        options_remove(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:complete\W|seasons?\W|\W|^)[(\[]?(s\d{2,}-\d{2,}\b)[)\]]?").unwrap(),
        parse_season_range,
        extend_seasons,
        options_remove(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:complete\W|seasons?\W|\W|^)[(\[]?(s[1-9]-[2-9])[)\]]?").unwrap(),
        parse_season_range,
        extend_seasons,
        options_remove(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)\d+ª(?:.+)?(?:a.?)?\d+ª(?:(?:.+)?(?:temporadas?))").unwrap(),
        parse_season_range,
        extend_seasons,
        options_remove(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:(?:\bthe\W)?\bcomplete\W)?(?:seasons?|[Сс]езони?|temporadas?)[. ]?[-:]?[. ]?[( \[]?((?:\d{1,2}[., /\\&]+)+\d{1,2}\b)[)\]]?").unwrap(),
        parse_season_range,
        extend_seasons,
        options_remove(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:(?:\bthe\W)?\bcomplete\W)?(?:seasons?|[Сс]езони?|temporadas?)[. ]?[-:]?[. ]?[( \[]?((?:\d{1,2}[.-]+)+[1-9]\d?\b)(?!\W*\d{4})[)\]]?").unwrap(),
        parse_season_range,
        extend_seasons,
        options_remove(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(
            r"(?i)(?:(?:\bthe\W)?\bcomplete\W)?season[. ]?[( \[]?((?:\d{1,2}[. -]+)+[1-9]\d?\b)[)\]]?(?!.*\.\w{2,4}$)",
        )
        .unwrap(),
        parse_season_range,
        extend_seasons,
        options_remove(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(
            r"(?i)(?:(?:\bthe\W)?\bcomplete\W)?\bseasons?\b[. -]?(\d{1,2}[. -]?(?:to|thru|and|\+|:)[. -]?\d{1,2})\b",
        )
        .unwrap(),
        parse_season_range,
        extend_seasons,
        options_remove(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bDVB(?:\b|-)").unwrap(),
        const_string("HDTV"),
        set_quality,
        options_remove(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(
            r"(?i)(?:(?:\bthe\W)?\bcomplete\W)?(?:saison|seizoen|season|series|temp(?:orada)?):?[. ]?(\d{1,2})\b",
        )
        .unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        extend_seasons,
        options_remove(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(\d{1,2})(?:-?й)?[. _]?(?:[Сс]езон|sez(?:on)?)(?:\W?\D|$)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        extend_seasons,
        options_remove(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)[Сс]езон:?[. _]?№?(\d{1,2})(?!\d)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        extend_seasons,
        options_remove(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:\D|^)(\d{1,2})Â?[°ºªa]?[. ]*temporada").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        extend_seasons,
        options_remove(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)t(\d{1,3})(?:[ex]+|$)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        extend_seasons,
        options_remove(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:(?:\bthe\W)?\bcomplete)?(?<![a-z])\bs(\d{1,3})(?:[\Wex]|\d{2}\b|$)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        extend_seasons,
        handler_options! {
            remove: false,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:(?:\bthe\W)?\bcomplete\W)?(?:\W|^)(\d{1,2})[. ]?(?:st|nd|rd|th)[. ]*season").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        extend_seasons,
        options_keep(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?<=S)\d{2}(?=E\d+)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        extend_seasons,
        options_remove(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:\D|^)(\d{1,2})[xх]\d{1,3}(?:\D|$)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        extend_seasons,
        options_keep(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)\bSn([1-9])(?:\D|$)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        extend_seasons,
        options_keep(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)[(\[](\d{1,2})\.\d{1,3}[)\]]").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        extend_seasons,
        options_keep(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)-\s?(\d{1,2})\.\d{2,3}\s?-").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        extend_seasons,
        options_keep(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:^|\/)(\d{1,2})-\d{2}\b(?!-\d)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        extend_seasons,
        options_keep(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)[^\w-](\d{1,2})-\d{2}(?=\.\w{2,4}$|$)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        extend_seasons,
        options_keep(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)\b(\d{2})[ ._]\d{2}(?:.F)?\.\w{2,4}$").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        extend_seasons,
        options_keep(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)\bEp(?:isode)?\W+(\d{1,2})\.\d{1,3}\b").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        extend_seasons,
        options_keep(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)\bSeasons?\b.*\b(?!(?:19|20)\d{2})(\d{1,2}-\d{1,2})\b").unwrap(),
        parse_season_range,
        extend_seasons,
        options_remove(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:\W|^)(\d{1,2})(?:e|ep)\d{1,3}(?:\W|$)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        extend_seasons,
        options_keep(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)\bТВ-(\d{1,2})\b").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        extend_seasons,
        options_keep(),
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)\bs(\d{1,4})").unwrap(),
        |val| vec![val.parse::<u32>().unwrap_or(0)],
        extend_seasons,
        options_remove_skip_if_already_found(),
    );

    parser.add_handler(
        "episodes",
        FancyRegex::new(
            r"(?i)(?:[\W\d]|^)e[ .]?[\[(]?(\d{1,3}(?:[ .-]*(?:[&+]|e|.){1,2}(?:[ .]*e)?[ .]?\d{1,3})+)(?:\W|$)",
        )
        .unwrap(),
        range_u32,
        extend_episodes,
        options_no_skip(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?:[\W\d]|^)ep[ .]?[\[(]?(\d{1,3}(?:[ .-]*(?:[&+]|ep){1,2}[ .]?\d{1,3})+)(?:\W|$)")
            .unwrap(),
        range_u32,
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?:[\W\d]|^)\d+[xх][ .]?[\[(]?(\d{1,3}(?:[ .]?[xх][ .]?\d{1,3})+)(?:\W|$)").unwrap(),
        range_u32,
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)Серии:\s+(\d+)\s+(?:of|из|iz)\s+\d+\b").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(
            r"(?i)(?:[\W\d]|^)(?:episodes?|[Сс]ерии:?)[ .]?[\[(]?(\d{1,3}(?:[ .+]*[&+][ .]?\d{1,3})+)(?:\W|$)",
        )
        .unwrap(),
        range_u32,
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)[\[(]?(?:\D|^)(\d{1,3}[ .]?ao[ .]?\d{1,3})[)\]]?(?:\W|$)").unwrap(),
        range_u32,
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(
            r"(?i)(?:[\W\d]|^)(?:e|eps?|episodes?|[Сс]ерии:?|\d+[xх])[ .]*[\[(]?(\d{1,3}(?:-\d{1,3})+)(?:\W|$)",
        )
        .unwrap(),
        range_u32,
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?:\W|^)(\d{1,3}(?:[ .]*~[ .]*\d{1,3})+)(?:\W|$)").unwrap(),
        range_u32,
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)\bE\d{1,4}\s*à\s*E\d{1,4}\b").unwrap(),
        range_u32,
        extend_episodes,
        options_remove_skip_if_already_found(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)[st]\d{1,2}[. ]?[xх-]?[. ]?(?:e|x|х|ep|-|\.)[. ]?(\d{1,4})(?:[abc]|v0?[1-4]|\D|$)")
            .unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_remove(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)\b[st]\d{2}(\d{2})\b").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)-\s(\d{1,3}[ .]*-[ .]*\d{1,3})(?!-\d)(?:\W|$)").unwrap(),
        range_u32,
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)s\d{1,2}\s?\((\d{1,3}[ .]*-[ .]*\d{1,3})\)").unwrap(),
        range_u32,
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?:^|/)\d{1,2}-(\d{2})\b(?!-\d)").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?<!\d-)\b\d{1,2}-(\d{2})(?=\.\w{2,4}$)").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?<=^\[.+].+)[. ]+-[. ]+(\d{1,4})[. ]+(?=\W)").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_remove(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(
            r"(?i)(?<!(?:seasons?|[Сс]езони?)\W*)(?:[ .(\[-]|^)(\d{1,3}(?:[ .]?[,&+~][ .]?\d{1,3})+)(?:[ .)\]-]|$)",
        )
        .unwrap(),
        range_u32,
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?<!(?:seasons?|[Сс]езони?)\W*)(?:[ .(\[-]|^)(\d{1,3}(?:-\d{1,3})+)(?:[ .)\(\]]|-\D|$)")
            .unwrap(),
        range_u32,
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)\bEp(?:isode)?\W+\d{1,2}\.(\d{1,3})\b").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)Ep.\d+.-.\\d+").unwrap(),
        range_u32,
        extend_episodes,
        options_remove(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?:\b[ée]p?(?:isode)?|[Ээ]пизод|[Сс]ер(?:ии|ия|\.)?|cap(?:itulo)?|epis[oó]dio)[. ]?[-:#№]?[. ]?(\d{1,4})(?:[abc]|v0?[1-4]|\W|$)").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)\b(\d{1,3})(?:-?я)?[ ._-]*(?:ser(?:i?[iyja]|\b)|[Сс]ер(?:ии|ия|\.)?)").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?:\D|^)\d{1,2}[. ]?[xх][. ]?(\d{1,3})(?:[abc]|v0?[1-4]|\D|$)").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?<=S\d{2}E)(\d+)").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"[\[(]\d{1,2}\.(\d{1,3})[)\]]").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"\b[Ss]\d{1,2}[ .](\d{1,2})\b").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"-\s?\d{1,2}\.(\d{2,3})\s?-").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?:\[|\()(\d+)\s(?:of|из|iz)\s\d+(?:\]|\))").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?<=\D|^)(\d{1,3})[. ]?(?:of|из|iz)[. ]?\d{1,3}(?=\D|$)").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"\b\d{2}[ ._-](\d{2})(?:.F)?\.\\w{2,4}$").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(\d+)(?=.?\[([A-Z0-9]{8})\])").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?<!\bMovie\s-\s)(?<=\s-\s)(\d+)(?=\s[-(\s])").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_remove_skip_if_already_found(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?:\W|^)(?:\d+)?(?:e|ep)(\d{1,3})(?:\W|$)").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        options_remove(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)E(\d+)\b").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        extend_episodes,
        handler_options! {
            remove: false,
            skip_if_already_found: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)\b(\d{1,4})-(\d{1,4})\b").unwrap(),
        range_u32,
        extend_episodes,
        handler_options! {
            remove: false,
            skip_if_already_found: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "country",
        FancyRegex::new(r"\b(US|UK|AU|NZ|CA)\b").unwrap(),
        value,
        set_country,
        options_default(),
    );

    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bengl?(?:sub[A-Z]*)?\b").unwrap(),
        const_string("en"),
        push_language,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bEnglish[\. _-]*(?:subs?|sdh|hi)\b").unwrap(),
        const_string("en"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:ingl[eéê]s|inglese?)\b").unwrap(),
        const_string("en"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\[En\b").unwrap(),
        const_string("en"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bIT\s+EN\b").unwrap(),
        const_string("it"),
        push_language_and_en,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bEng(?:,|\s)").unwrap(),
        const_string("en"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bCze(?:ch)?\b").unwrap(),
        const_string("cs"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bGer(?:,|\s|\b)").unwrap(),
        const_string("de"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\[spanish\]").unwrap(),
        const_string("es"),
        push_language,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:español|espanhol)\b").unwrap(),
        const_string("es"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bFR(?:a|e|anc[eê]s|VF[FQIB2]?)\b").unwrap(),
        const_string("fr"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b\[?(VF[FQRIB2]?\]?\b|(VOST)?FR2?)\b").unwrap(),
        const_string("fr"),
        push_language,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:GERMAN|GER)\b|(?-i)\bDE\b").unwrap(),
        const_string("de"),
        push_language,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(TRUE|SUB).?FRENCH\b|\bFRENCH\b|\bFre?\b").unwrap(),
        const_string("fr"),
        push_language,
        handler_options! {
            remove: true,
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(VOST(?:FR?|A)?)\b").unwrap(),
        const_string("fr"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(VF[FQIB2]?|(TRUE|SUB).?FRENCH|(VOST)?FR2?)\b").unwrap(),
        const_string("fr"),
        push_language,
        options_remove_skip_if_already_found(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bspanish\W?latin|american\W*(?:spa|esp?)").unwrap(),
        const_string("la"),
        push_language,
        options_remove_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:\bla\b.+(?:cia\b))").unwrap(),
        const_string("es"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:audio.)?lat(?:in?|ino)?\b").unwrap(),
        const_string("la"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:audio.)?(?:ESP?|spa|(en[ .]+)?espa[nñ]ola?|castellano)\b").unwrap(),
        const_string("es"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bes(?=[ .,/-]+(?:[A-Z]{2}[ .,/-]+){2,})\b").unwrap(),
        const_string("es"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?<=[ .,/-]+(?:[A-Z]{2}[ .,/-]+){2,})es\b").unwrap(),
        const_string("es"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?<=[ .,/-]+[A-Z]{2}[ .,/-]+)es(?=[ .,/-]+[A-Z]{2}[ .,/-]+)\b").unwrap(),
        const_string("es"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bes(?=\.(?:ass|ssa|srt|sub|idx)$)").unwrap(),
        const_string("es"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(temporadas?|completa)\b").unwrap(),
        const_string("es"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:INT[EÉ]GRALE?)\b").unwrap(),
        const_string("fr"),
        push_language,
        handler_options! {
            remove: false,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:Saison)\b").unwrap(),
        const_string("fr"),
        push_language,
        handler_options! {
            remove: false,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:p[rt]|en|port)[. (\\/-]*BR\b").unwrap(),
        const_string("pt"),
        push_language,
        handler_options! {
            skip_if_already_found: false,
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bbr(?:a|azil|azilian)\W+(?:pt|por)\b").unwrap(),
        const_string("pt"),
        push_language,
        handler_options! {
            skip_if_already_found: false,
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:leg(?:endado|endas?)?|dub(?:lado)?|portugu[eèê]se?)[. -]*BR\b").unwrap(),
        const_string("pt"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bleg(?:endado|endas?)\b").unwrap(),
        const_string("pt"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bportugu[eèê]s[ea]?\b").unwrap(),
        const_string("pt"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bPT[. -]*(?:PT|ENG?|sub(?:s|titles?))\b").unwrap(),
        const_string("pt"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bpt(?=\.(?:ass|ssa|srt|sub|idx)$)").unwrap(),
        const_string("pt"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bPT\b").unwrap(),
        const_string("pt"),
        push_language,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bpor\b").unwrap(),
        const_string("pt"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b-?ITA\b").unwrap(),
        const_string("it"),
        push_language,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?<!w{3}\.\w+\.)IT(?=[ .,/-]+(?:[a-zA-Z]{2}[ .,/-]+){2,})\b").unwrap(),
        const_string("it"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bit(?=\.(?:ass|ssa|srt|sub|idx)$)").unwrap(),
        const_string("it"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bitaliano?\b").unwrap(),
        const_string("it"),
        push_language,
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bslo(?:vak|vakian|subs|[\]_)]?\.\w{2,4}$)\b").unwrap(),
        const_string("sk"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bHU\b").unwrap(),
        const_string("hu"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bHUN(?:garian)?\b").unwrap(),
        const_string("hu"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bROM(?:anian)?\b").unwrap(),
        const_string("ro"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bRO(?=[ .,/-]*(?:[A-Z]{2}[ .,/-]+)*sub)").unwrap(),
        const_string("ro"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bbul(?:garian)?\b").unwrap(),
        const_string("bg"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:srp|serbian)\b").unwrap(),
        const_string("sr"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:HRV|croatian)\b").unwrap(),
        const_string("hr"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bHR(?=[ .,/-]*(?:[A-Z]{2}[ .,/-]+)*sub)\b").unwrap(),
        const_string("hr"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bslovenian\b").unwrap(),
        const_string("sl"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)NL|dut|holand[eê]s)\b").unwrap(),
        const_string("nl"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bdutch\b").unwrap(),
        const_string("nl"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bflemish\b").unwrap(),
        const_string("nl"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:DK|danska|dansub|nordic)\b").unwrap(),
        const_string("da"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(danish|dinamarqu[eê]s)\b").unwrap(),
        const_string("da"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bdan\b(?=.*\.(?:srt|vtt|ssa|ass|sub|idx)$)").unwrap(),
        const_string("da"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.|Sci-)FI|finsk|finsub|nordic)\b").unwrap(),
        const_string("fi"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bfinnish\b").unwrap(),
        const_string("fi"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)SE|swe|swesubs?|sv(?:ensk)?|nordic)\b").unwrap(),
        const_string("sv"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(swedish|sueco)\b").unwrap(),
        const_string("sv"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:NOR|norsk|norsub|nordic)\b").unwrap(),
        const_string("no"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(norwegian|noruegu[eê]s|bokm[aå]l|nob)\b").unwrap(),
        const_string("no"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bnor\b(?=[\]_)]?\.\\w{2,4}$)").unwrap(),
        const_string("no"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:TURKISH|TUR|TIVIBU)\b").unwrap(),
        const_string("tr"),
        push_language,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:HEBREW|HEB)\b").unwrap(),
        const_string("he"),
        push_language,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:POLISH|POL)\b").unwrap(),
        const_string("pl"),
        push_language,
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            skip_if_first: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:BW|BENGALI)\b").unwrap(),
        const_string("bn"),
        push_language,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:JP|JAP|JPN)\b").unwrap(),
        const_string("ja"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(japanese|japon[eê]s)\b").unwrap(),
        const_string("ja"),
        push_language,
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:KOR|kor[ .-]?sub)\b").unwrap(),
        const_string("ko"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(korean|coreano)\b").unwrap(),
        const_string("ko"),
        push_language,
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:traditional\W*chinese|chinese\W*traditional)(?:\Wchi)?\b").unwrap(),
        const_string("zh"),
        push_language,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bzh-hant\b").unwrap(),
        const_string("zh"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:mand[ae]rin|ch[sn])\b").unwrap(),
        const_string("zh"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bgreek[ .-]*(?:audio|lang(?:uage)?|subs?(?:titles?)?)?\b").unwrap(),
        const_string("el"),
        push_language,
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:GER|DEU)\b").unwrap(),
        const_string("de"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bde(?=[ .,/-]+(?:[A-Z]{2}[ .,/-]+){2,})\b").unwrap(),
        const_string("de"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?<=[ .,/-]+(?:[A-Z]{2}[ .,/-]+){2,})de\b").unwrap(),
        const_string("de"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?<=[ .,/-]+[A-Z]{2}[ .,/-]+)de(?=[ .,/-]+[A-Z]{2}[ .,/-]+)\b").unwrap(),
        const_string("de"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bde(?=\.(?:ass|ssa|srt|sub|idx)$)").unwrap(),
        const_string("de"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(german|alem[aã]o)\b").unwrap(),
        const_string("de"),
        push_language,
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bRUS?\b").unwrap(),
        const_string("ru"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(russian|russo)\b").unwrap(),
        const_string("ru"),
        push_language,
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bUKR\b").unwrap(),
        const_string("uk"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bukrainian\b").unwrap(),
        const_string("uk"),
        push_language,
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bhin(?:di)?\b").unwrap(),
        const_string("hi"),
        push_language,
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    let anime_regex = FancyRegex::new(r"(?i)One.*?Piece|Bleach|Naruto").unwrap();
    let volume_regex = FancyRegex::new(r"(?i)\bvol(?:ume)?[. -]*(\d{1,2})\b").unwrap();
    parser.add_handler_fn(
        "volumes",
        Box::new(move |context: &mut ParseContext| -> Option<MatchInfo> {
            let title = &context.title;
            let matched = &context.matched;

            let start_index = matched.get("year").map_or(0, |m| m.match_index);

            if start_index >= title.len() {
                return None;
            }

            let search_slice = &title[start_index..];

            if let Ok(Some(m)) = volume_regex.find(search_slice) {
                let raw_match = m.as_str().to_string();
                let relative_start = m.start();

                if let Ok(Some(cap)) = volume_regex.captures(search_slice) {
                    let volume_number = cap.get(1).map_or(0, |m| m.as_str().parse::<i32>().unwrap_or(0));

                    context.result.volumes = vec![volume_number];
                }

                let abs_start = start_index + relative_start;

                let info = MatchInfo { raw_match, match_index: abs_start, remove: true, skip_from_title: false };

                context.matched.insert("volumes".to_string(), info.clone());
                return Some(info);
            }
            None
        }),
    );
    let ep_regex = FancyRegex::new(r"\b\d{1,4}\b").unwrap();

    parser.add_handler_fn(
        "episodes",
        Box::new(move |context: &mut ParseContext| -> Option<MatchInfo> {
            if context.matched.contains_key("episodes") {
                return None;
            }

            let title = &context.title;

            if anime_regex.is_match(title).unwrap_or(false) {
                if let Ok(Some(m)) = ep_regex.find(title) {
                    let raw_match = m.as_str().to_string();
                    let val = raw_match.parse::<u32>().unwrap_or(0);

                    context.result.episodes.push(val);

                    let info = MatchInfo { raw_match, match_index: m.start(), remove: true, skip_from_title: true };
                    context.matched.insert("episodes".to_string(), info.clone());
                    return Some(info);
                }
            }
            None
        }),
    );

    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(PLDUB|PLSUB|DUBPL|DubbingPL|LekPL|LektorPL)\b").unwrap(),
        const_string("pl"),
        push_language,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(PLDUB|PLSUB|DUBPL|DubbingPL|LekPL|LektorPL)\b").unwrap(),
        const_string("pl"),
        push_language,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(PLDUB|PLSUB|DUBPL|DubbingPL|LekPL|LektorPL)\b").unwrap(),
        const_string("pl"),
        push_language,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)tel(?!\W*aviv)|telugu)\b").unwrap(),
        const_string("te"),
        push_language,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bt[aâ]m(?:il)?\b").unwrap(),
        const_string("ta"),
        push_language,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)MAL(?:ay)?|malayalam)\b").unwrap(),
        const_string("ml"),
        push_language,
        handler_options! {
            remove: true,
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)KAN(?:nada)?|kannada)\b").unwrap(),
        const_string("kn"),
        push_language,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)MAR(?:a(?:thi)?)?|marathi)\b").unwrap(),
        const_string("mr"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)GUJ(?:arati)?|gujarati)\b").unwrap(),
        const_string("gu"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)PUN(?:jabi)?|punjabi)\b").unwrap(),
        const_string("pa"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)BEN(?!.\bThe|and|of\b)(?:gali)?|bengali)\b").unwrap(),
        const_string("bn"),
        push_language,
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)(?<!shang-?)\bCH(?:I|T)\b").unwrap(),
        const_string("zh"),
        push_language,
        options_skip_from_title_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(chinese|chin[eê]s)\b").unwrap(),
        const_string("zh"),
        push_language,
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bzh-hans\b").unwrap(),
        const_string("zh"),
        push_language,
        options_no_skip(),
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\benglish?\b").unwrap(),
        const_string("en"),
        push_language,
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );

    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bHDTV(?:Rip)?\b").unwrap(),
        |val| {
            if val.to_lowercase().contains("rip") {
                "HDTVRip".to_string()
            } else {
                "HDTV".to_string()
            }
        },
        set_quality,
        options_remove_skip_if_already_found(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bSAT(?:Rip)?\b").unwrap(),
        const_string("SATRip"),
        set_quality,
        options_remove_skip_if_already_found(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bWEB(?:Rip)?\b").unwrap(),
        |val| {
            if val.to_lowercase().contains("rip") {
                "WEBRip".to_string()
            } else {
                "WEB-DL".to_string()
            }
        },
        set_quality,
        options_remove_skip_if_already_found(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bPPVRip\b").unwrap(),
        const_string("PPVRip"),
        set_quality,
        options_remove_skip_if_already_found(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bWEBMux\b").unwrap(),
        const_string("WEBMux"),
        set_quality,
        options_remove_skip_if_already_found(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:HDRip|MicroHD)\b").unwrap(),
        const_string("HDRip"),
        set_quality,
        options_remove_skip_if_already_found(),
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bRemux\b").unwrap(),
        const_string("REMUX"),
        |meta, _val| {
            if let Some(ref q) = meta.quality {
                if !q.contains("REMUX") {
                    meta.quality = Some(format!("{q} REMUX"));
                }
            } else {
                meta.quality = Some("REMUX".to_string());
            }
        },
        options_remove_no_skip(),
    );

    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\bS-Print\b").unwrap(),
        boolean,
        set_quality_to_cam_if_trash,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\bTELECINE\b").unwrap(),
        boolean,
        set_quality_to_tele_cine_if_trash,
        options_remove_no_skip(),
    );

    parser.add_handler(
        "subbed",
        FancyRegex::new(r"(?i)\bmulti(?:ple)?[ .-]*(?:su?$|sub\w*|dub\w*)\b|msub").unwrap(),
        boolean,
        set_subbed,
        options_remove(),
    );
    parser.add_handler(
        "subbed",
        FancyRegex::new(r"(?i)\b(?:Official.*?|Dual-?)?sub(s|bed)?\b").unwrap(),
        boolean,
        set_subbed,
        options_remove(),
    );

    parser.add_handler(
        "dubbed",
        FancyRegex::new(r"(?i)[\[(\s]?\bmulti(?:ple)?[ .-]*(?:lang(?:uages?)?|audio|VF2)\b\][\[(\s]?").unwrap(),
        boolean,
        set_dubbed,
        options_remove(),
    );
    parser.add_handler(
        "dubbed",
        FancyRegex::new(r"(?i)\btri(?:ple)?[ .-]*(?:audio|dub\w*)\b").unwrap(),
        boolean,
        set_dubbed,
        options_no_skip(),
    );
    parser.add_handler(
        "dubbed",
        FancyRegex::new(r"(?i)\bdual[ .-]*(?:au?$|[aá]udio|line)\b").unwrap(),
        boolean,
        set_dubbed,
        options_no_skip(),
    );
    parser.add_handler(
        "dubbed",
        FancyRegex::new(r"(?i)\bdual\b(?![ .-]*sub)").unwrap(),
        boolean,
        set_dubbed,
        options_no_skip(),
    );
    parser.add_handler(
        "dubbed",
        FancyRegex::new(r"(?i)\b(fan\s?dub)\b").unwrap(),
        boolean,
        set_dubbed,
        handler_options! {
            remove: true,
            skip_from_title: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "dubbed",
        FancyRegex::new(r"(?i)\b(Fan.*)?(?:DUBBED|dublado|dubbing|DUBS?)\b").unwrap(),
        boolean,
        set_dubbed,
        options_remove(),
    );
    parser.add_handler(
        "dubbed",
        FancyRegex::new(r"(?i)\b(?!.*\bsub(s|bed)?\b)([ _\-\[(\.]*)?(dual|multi)([ _\-\[(\.]*)?(audio)\b").unwrap(),
        boolean,
        set_dubbed,
        options_remove(),
    );
    parser.add_handler("dubbed", FancyRegex::new(r"(?i)\bMULTi\b").unwrap(), boolean, set_dubbed, options_remove());

    parser.add_handler("3d", FancyRegex::new(r"(?i)\b3D\b").unwrap(), boolean, set_is_3d, options_keep_skip_if_first());

    parser.add_handler(
        "size",
        FancyRegex::new(r"(?i)\b(\d+(\.\d+)?\s?(MB|GB|TB))\b").unwrap(),
        |val| val.replace(' ', "").to_uppercase(),
        set_size,
        options_remove(),
    );
    parser.add_handler(
        "size",
        FancyRegex::new(r"(?i)[-\s](\d+(?:\.\d+)?(?:MB|GB|TB))[-\s]").unwrap(),
        |val| val.replace(' ', "").to_uppercase(),
        set_size,
        options_remove_skip_if_already_found(),
    );

    parser.add_handler(
        "site",
        FancyRegex::new(r"(?i)\b(?:www?.?)?(?:\w+\-)?\w+\.(?:com|org|net|ms|tv|mx|co|party|vip|nu|pics|re)\b").unwrap(),
        value,
        set_site,
        options_remove(),
    );
    parser.add_handler(
        "site",
        FancyRegex::new(r"(?i)\bwww?.?[\w.-]+\.(?:link|world|cam|xyz|info|club)\b").unwrap(),
        value,
        set_site,
        options_remove(),
    );
    parser.add_handler(
        "site",
        FancyRegex::new(r"(?i)\bwww\.?[\s.]?(\w+[\.\s]?\w+)\b").unwrap(),
        |_| String::new(),
        set_site,
        options_remove_no_skip(),
    );

    parser.add_handler(
        "network",
        FancyRegex::new(r"(?i)\bNF|Netflix\b").unwrap(),
        const_string("Netflix"),
        push_network,
        options_remove(),
    );
    parser.add_handler(
        "network",
        FancyRegex::new(r"(?i)\bAMZN\b").unwrap(),
        const_string("Amazon"),
        push_network,
        options_remove(),
    );
    parser.add_handler(
        "network",
        FancyRegex::new(r"(?i)\bHULU\b").unwrap(),
        const_string("Hulu"),
        push_network,
        options_remove_no_skip(),
    );
    parser.add_handler(
        "network",
        FancyRegex::new(r"(?i)\bANPL\b").unwrap(),
        const_string("Animal Planet"),
        push_network,
        options_remove(),
    );

    parser.add_handler("trash", FancyRegex::new(r"(?i)\bCUSTOM\b").unwrap(), boolean, set_trash, options_remove());

    parser.add_handler(
        "extension",
        FancyRegex::new(r"(?i)\.(3g2|3gp|avi|flv|mkv|mk3d|mov|mp2|mp4|m4v|mpe|mpeg|mpg|mpv|webm|wmv|ogm|divx|ts|m2ts|iso|vob|sub|idx|ttxt|txt|smi|srt|ssa|ass|vtt|nfo|html)$").unwrap(),
        |val| val.to_lowercase().trim_start_matches('.').to_string(),
        set_extension,
        options_remove(),
    );
}
