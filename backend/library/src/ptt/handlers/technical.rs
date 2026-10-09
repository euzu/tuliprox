use super::{
    const_string, options_default, options_keep, options_no_skip, options_remove, options_remove_no_skip,
    options_remove_skip_if_already_found, push_audio, push_channels, push_hdr, set_bit_depth, set_codec, set_edition,
    set_extended, set_ppv, set_proper, set_quality, set_quality_bluray, set_quality_remux, set_remastered, set_repack,
    set_retail, set_site, set_uncensored, set_unrated,
};
use crate::ptt::{
    parser::{handler_options, PttParser},
    transformers::boolean,
};
use fancy_regex::Regex as FancyRegex;

#[allow(clippy::too_many_lines)]
pub(super) fn register(parser: &mut PttParser) {
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
}
