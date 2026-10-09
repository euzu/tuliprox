use super::{
    append_p, const_string, ignore, mark_adult, options_default, options_keep, options_keep_no_skip, options_remove,
    options_remove_no_skip, options_remove_skip_if_already_found, options_skip_from_title_no_skip, push_language,
    set_adult, set_container, set_episode_code, set_group, set_quality_to_scr_if_trash,
    set_quality_to_tele_cine_if_trash, set_quality_to_tele_sync_if_trash, set_quality_to_vhs_if_trash,
    set_quality_to_vhsrip_if_trash, set_resolution, set_tmdb, set_trash, set_tvdb, set_year,
};
use crate::ptt::{
    parser::PttParser,
    transformers::{boolean, first_uinteger, lowercase, none, transform_resolution, uppercase},
};
use fancy_regex::Regex as FancyRegex;

#[allow(clippy::too_many_lines)]
pub(super) fn register(parser: &mut PttParser) {
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
}
