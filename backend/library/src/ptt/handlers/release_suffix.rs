use super::{
    const_string, options_keep_skip_if_first, options_no_skip, options_remove, options_remove_no_skip,
    options_remove_skip_if_already_found, push_network, set_dubbed, set_extension, set_is_3d, set_quality,
    set_quality_to_cam_if_trash, set_quality_to_tele_cine_if_trash, set_site, set_size, set_subbed, set_trash,
};
use crate::ptt::{
    parser::{handler_options, PttParser},
    transformers::{boolean, value},
};
use fancy_regex::Regex as FancyRegex;

#[allow(clippy::too_many_lines)]
pub(super) fn register(parser: &mut PttParser) {
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
