use crate::ptt::parser::{handler_options, HandlerOptions, MatchInfo, ParseContext, PttParser};
use crate::ptt::transformers::{boolean, date, first_uinteger, lowercase, none, range_i32, range_u32, transform_resolution, uinteger, uppercase, value};
use fancy_regex::Regex as FancyRegex;

fn parse_season_range(val: &str) -> Vec<u32> {
    let nums: Vec<u32> = val
        .split(|c: char| !c.is_numeric())
        .filter_map(|s| s.parse::<u32>().ok())
        .collect();

    if nums.len() == 2 {
        let start = nums[0];
        let end = nums[1];
        if start < end && (end - start) < 100 {
            let lower = val.to_lowercase();
            if val.contains('-')
                || lower.contains("to")
                || lower.contains("thru")
                || val.contains(':')
            {
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
        |meta, val| {
            if let Some(v) = val {
                meta.tmdb = Some(v);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "tvdb",
        FancyRegex::new(r"(?i)\btvdb\b[-=]\d+").unwrap(),
        first_uinteger,
        |meta, val| {
            if let Some(v) = val {
                meta.tvdb = Some(v);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bPRE[- .]?HDRip\b").unwrap(),
        boolean,
        |meta, val| {
            meta.trash = val;
            if val {
                meta.quality = Some("SCR".to_string());
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bE[- ]?Sub\b").unwrap(),
        |_| "en".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bTS-Screener\b").unwrap(),
        boolean,
        |meta, val| {
            meta.trash = val;
            if val {
                meta.quality = Some("TeleSync".to_string());
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );

    parser.add_handler(
        "year",
        FancyRegex::new(r"\b19\d{2}\s?-\s?20\d{2}\b").unwrap(),
        first_uinteger,
        |meta, val| {
            if let Some(v) = val {
                meta.year = Some(v);
            }
        },
        handler_options! {
            remove: false,
            ..Default::default()
        },
    );

    parser.add_handler(
        "title_cleanup",
        FancyRegex::new(r"(?i)\b(?:19|20)\d{2}\s*[-]\s*(?:(?:19|20)\d{2}|\d{2})\b").unwrap(),
        none,
        |_, _| {},
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "title_cleanup",
        FancyRegex::new(r"(?i)\b100[ .-]*years?[ .-]*quest\b").unwrap(),
        none,
        |_, _| {},
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "title_cleanup",
        FancyRegex::new(r"(?i)\[?(\+.)?Extras\]?").unwrap(),
        none,
        |_, _| {},
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "title_cleanup",
        FancyRegex::new(r"(?i)(\+Movies)?\+Specials").unwrap(),
        none,
        |_, _| {},
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "group",
        FancyRegex::new(r"-?EDGE2020").unwrap(),
        |_| "EDGE2020".to_string(),
        |meta, val| meta.group = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "title_cleanup",
        FancyRegex::new(r"(?i)TV Money").unwrap(),
        none,
        |_, _| {},
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "container",
        FancyRegex::new(r"(?i)\.?[\[(]?\b(MKV|AVI|MP4|WMV|MPG|MPEG)\b[\])]?").unwrap(),
        lowercase,
        |meta, val| meta.container = Some(val),
        HandlerOptions::default(),
    );

    parser.add_handler(
        "torrent",
        FancyRegex::new(r"\.torrent$").unwrap(),
        boolean,
        |_, _| {},
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "adult",
        FancyRegex::new(r"\b(XXX|xxx|Xxx)\b").unwrap(),
        boolean,
        |meta, val| meta.adult = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    if let Ok(re) = FancyRegex::new(r"(?i)\b(18\+|adult|porn|xxx)\b") {
        parser.add_handler(
            "adult",
            re,
            boolean,
            |meta, _| meta.adult = true,
            HandlerOptions::default(),
        );
    }

    parser.add_handler(
        "extras",
        FancyRegex::new(r"(?i)\bOVA\b").unwrap(),
        |_| "OVA".to_string(),
        |_, _| {},
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "extras",
        FancyRegex::new(r"(?i)\bOVA\b").unwrap(),
        |_| "OVA".to_string(),
        |_, _| {},
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)\[?\]?3840x\d{4}[\])?]?").unwrap(),
        |_| "2160p".to_string(),
        |meta, val| meta.resolution = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)\[?\]?1920x\d{3,4}[\])?]?").unwrap(),
        |_| "1080p".to_string(),
        |meta, val| meta.resolution = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)\[?\]?1280x\d{3}[\])?]?").unwrap(),
        |_| "720p".to_string(),
        |meta, val| meta.resolution = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)\[?\]?(\d{3,4}x\d{3,4})[\])?]?p?").unwrap(),
        |val: &str| format!("{val}p"),
        |meta, val| meta.resolution = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)(480|720|1080)0[pi]").unwrap(),
        |val: &str| format!("{val}p"),
        |meta, val| meta.resolution = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)(?:QHD|QuadHD|WQHD|2560(\d+)?x(\d+)?1440p?)").unwrap(),
        |_| "1440p".to_string(),
        |meta, val| meta.resolution = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)(?:Full HD|FHD|1920(\d+)?x(\d+)?1080p?)").unwrap(),
        |_| "1080p".to_string(),
        |meta, val| meta.resolution = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)(?:BD|HD|M)(2160p?|4k)").unwrap(),
        |_| "2160p".to_string(),
        |meta, val| meta.resolution = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)(?:BD|HD|M)1080p?").unwrap(),
        |_| "1080p".to_string(),
        |meta, val| meta.resolution = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)(?:BD|HD|M)720p?").unwrap(),
        |_| "720p".to_string(),
        |meta, val| meta.resolution = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)(?:BD|HD|M)480p?").unwrap(),
        |_| "480p".to_string(),
        |meta, val| meta.resolution = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)\b(?:4k|2160p|1080p|720p|480p)(?!.*\b(?:4k|2160p|1080p|720p|480p)\b)")
            .unwrap(),
        transform_resolution,
        |meta, val| meta.resolution = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)\b4k|21600?[pi]\b").unwrap(),
        |_| "2160p".to_string(),
        |meta, val| meta.resolution = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)(\d{3,4}[pi])").unwrap(),
        lowercase,
        |meta, val| meta.resolution = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "resolution",
        FancyRegex::new(r"(?i)(240|360|480|576|720|1080|2160|3840)[pi]").unwrap(),
        lowercase,
        |meta, val| meta.resolution = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "episode_code",
        FancyRegex::new(r"[\[\()]([A-Fa-f0-9]{8})[\]\)]").unwrap(),
        uppercase,
        |meta, val| meta.episode_code = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "episode_code",
        FancyRegex::new(r"[\[\()]([0-9]{8})[\]\)]").unwrap(),
        uppercase,
        |meta, val| meta.episode_code = Some(val),
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\b(?:H[DQ][ .-]*)?(?<!Body\s)CAM(?:H[DQ])?(?!.?(S|E|\()\d+)(?:H[DQ])?(?:[ .-]*Rip|Rp)?\b").unwrap(),
        boolean,
        |meta, val| { meta.trash = val; },
        handler_options! { remove: false, skip_from_title: true, skip_if_already_found: false, ..Default::default() },
    );
    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\b(?:H[DQ][ .-]*)?TS(?:H[DQ])?(?:[ .-]*Rip|Rp)?\b").unwrap(),
        boolean,
        |meta, val| {
            meta.trash = val;
            if val {
                meta.quality = Some("TeleSync".to_string());
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\b(?:H[DQ][ .-]*)?TC(?:H[DQ])?(?:[ .-]*Rip|Rp)?\b").unwrap(),
        boolean,
        |meta, val| {
            meta.trash = val;
            if val {
                meta.quality = Some("TeleCine".to_string());
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\b(?:H[DQ][ .-]*)?P(?:re)?DVD[ .-]*Rip\b").unwrap(),
        boolean,
        |meta, val| {
            meta.trash = val;
            if val {
                meta.quality = Some("SCR".to_string());
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\b(?:H[DQ][ .-]*)?(?:DVD|WEB|BR|HD)?Scr(?:eener)?\b").unwrap(),
        boolean,
        |meta, val| {
            meta.trash = val;
            if val {
                meta.quality = Some("SCR".to_string());
            }
        },
        handler_options! {
            remove: false,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\bVHSRip\b").unwrap(),
        boolean,
        |meta, val| {
            meta.trash = val;
            if val {
                meta.quality = Some("VHSRip".to_string());
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\bVHS\b").unwrap(),
        boolean,
        |meta, val| {
            meta.trash = val;
            if val {
                meta.quality = Some("VHS".to_string());
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\b(?:H[DQ][ .-]*)?R5(?:[ .-]*Line)?\b").unwrap(),
        boolean,
        |meta, val| meta.trash = val,
        handler_options! {
            remove: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\bVHSRip\b").unwrap(),
        boolean,
        |meta, val| meta.trash = val,
        handler_options! {
            remove: false,
            ..Default::default()
        },
    );

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
        |meta, val| if !val.is_empty() { meta.date = Some(val) },
        handler_options! { remove: true, ..Default::default() },
    );
    parser.add_handler(
        "date",
        FancyRegex::new(r"(?:\W|^)(\[?\]?(?:0[1-9]|[12][0-9]|3[01])([. \-/\\])(?:0[1-9]|1[012])\2(?:19[6-9]|20[01])[0-9][\])]?)(?:\W|$)").unwrap(),
        |val| date(val, &["%d-%m-%Y", "%d.%m.%Y", "%d %m %Y"]).unwrap_or_default(),
        |meta, val| if !val.is_empty() { meta.date = Some(val) },
        handler_options! { remove: true, ..Default::default() },
    );

    parser.add_handler(
        "date",
        FancyRegex::new(r"(?:\W)(\[?\]?(?:0[1-9]|1[012])([. \-/\\])(?:0[1-9]|[12][0-9]|3[01])\2(?:[0][1-9]|[0126789][0-9])[\])]?)(?:\W|$)").unwrap(),
        |val| date(val, &["%m %d %y", "%m.%d.%y", "%m-%d-%y"]).unwrap_or_default(),
        |meta, val| if !val.is_empty() { meta.date = Some(val) },
        handler_options! { remove: true, ..Default::default() },
    );
    parser.add_handler(
        "date",
        FancyRegex::new(r"(?:\W)(\[?\]?(?:[0][1-9]|[12][0-9]|3[0-9])([. \-/\\])(?:0[1-9]|1[012])\2(?:0[1-9]|[12][0-9])[\])]?)(?:\W|$)").unwrap(),
        |val| date(val, &["%y %m %d", "%y.%m.%d", "%y-%m-%d"]).unwrap_or_default(),
        |meta, val| if !val.is_empty() { meta.date = Some(val) },
        handler_options! { remove: true, ..Default::default() },
    );
    parser.add_handler(
        "date",
        FancyRegex::new(r"(?:\W)(\[?\]?(?:0[1-9]|[12][0-9]|3[01])([. \-/\\])(?:0[1-9]|1[012])\2(?:[0][1-9]|[0126789][0-9])[\])]?)(?:\W|$)").unwrap(),
        |val| date(val, &["%d %m %y", "%d.%m.%y", "%d-%m-%y"]).unwrap_or_default(),
        |meta, val| if !val.is_empty() { meta.date = Some(val) },
        handler_options! { remove: true, ..Default::default() },
    );
    parser.add_handler(
        "date",
        FancyRegex::new(r"(?i)(?:\W|^)([(\[]?(?:0?[1-9]|[12][0-9]|3[01])[. ]?(?:st|nd|rd|th)?([. \-/\\])(?:feb(?:ruary)?|jan(?:uary)?|mar(?:ch)?|apr(?:il)?|may|june?|july?|aug(?:ust)?|sept?(?:ember)?|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?)\2(?:19[7-9]|20[012])[0-9][)\]]?)(?=\W|$)").unwrap(),
        |val| date(val, &["%d %b %Y", "%d %B %Y", "%d.%b.%Y", "%d.%B.%Y", "%d-%b-%Y", "%d-%B-%Y"]).unwrap_or_default(),
        |meta, val| if !val.is_empty() { meta.date = Some(val) },
        handler_options! { remove: true, ..Default::default() },
    );
    parser.add_handler(
        "date",
        FancyRegex::new(r"(?i)(?:\W|^)(\[?\]?(?:0?[1-9]|[12][0-9]|3[01])[. ]?(?:st|nd|rd|th)?([. \-\/\\])(?:feb(?:ruary)?|jan(?:uary)?|mar(?:ch)?|apr(?:il)?|may|june?|july?|aug(?:ust)?|sept?(?:ember)?|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?)\2(?:0[1-9]|[0126789][0-9])[\])]?)(?:\W|$)").unwrap(),
        |val| date(val, &["%d %b %y", "%d %B %y", "%d.%b.%y", "%d.%B.%y", "%d-%b-%y", "%d-%B-%y"]).unwrap_or_default(),
        |meta, val| if !val.is_empty() { meta.date = Some(val) },
        handler_options! { remove: true, ..Default::default() },
    );
    parser.add_handler(
        "date",
        FancyRegex::new(r"(?:\W|^)(\[?\]?20[012][0-9](?:0[1-9]|1[012])(?:0[1-9]|[12][0-9]|3[01])[\])]?)(?:\W|$)").unwrap(),
        |val| date(val, &["%Y%m%d"]).unwrap_or_default(),
        |meta, val| if !val.is_empty() { meta.date = Some(val) },
        handler_options! { remove: true, ..Default::default() },
    );
    parser.add_handler(
        "date",
        FancyRegex::new(r"(?i)(?:\W|^)((?:0?[1-9]|[12][0-9]|3[01])(?:st|nd|rd|th)\s+(?:Jan(?:uary)?|Feb(?:ruary)?|Mar(?:ch)?|Apr(?:il)?|May|June?|July?|Aug(?:ust)?|Sept?(?:ember)?|Oct(?:ober)?|Nov(?:ember)?|Dec(?:ember)?)\s+(?:19[7-9]|20[012])[0-9])(?=\W|$)").unwrap(),
        |val| {
            let clean = val.replace("st ", " ").replace("nd ", " ").replace("rd ", " ").replace("th ", " ");
            date(&clean, &["%d %b %Y", "%d %B %Y"]).unwrap_or_default()
        },
        |meta, val| if !val.is_empty() { meta.date = Some(val) },
        handler_options! { remove: true, ..Default::default() },
    );

    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)\b((?:19\d|20[012])\d[ .]?-[ .]?(?:19\d|20[012])\d)\b").unwrap(),
        boolean,
        |meta, val| meta.complete = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)[(\[][ .]?((?:19\d|20[012])\d[ .]?-[ .]?\d{2})[ .]?[)\]]").unwrap(),
        boolean,
        |meta, val| meta.complete = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)\bcomplete\b").unwrap(),
        boolean,
        |meta, val| meta.complete = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)\b(?:INTEGRALE?|INTÉGRALE?)\b").unwrap(),
        boolean,
        |meta, val| meta.complete = val,
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)(Movie|Complete).Collection").unwrap(),
        boolean,
        |meta, val| meta.complete = val,
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)Complete(.\d{1,2})").unwrap(),
        boolean,
        |meta, val| meta.complete = val,
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)(?:\bthe\W)?(?:\bcomplete|collection|dvd)?\b[ .]?\bbox[ .-]?set\b")
            .unwrap(),
        boolean,
        |meta, val| meta.complete = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)(?:\bthe\W)?(?:\bcomplete|collection|dvd)?\b[ .]?\bmini[ .-]?series\b")
            .unwrap(),
        boolean,
        |meta, val| meta.complete = val,
        HandlerOptions::default(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)(?:\bthe\W)?(?:\bcomplete\b|\bfull\b|\ball\b)\b.*\b(?:series|seasons|collection|episodes|set|pack|movies)\b").unwrap(),
        boolean,
        |meta, val| meta.complete = val,
        HandlerOptions::default(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)(Top\W+)?\d+\W+(movies?|series|seasons?)\W+Collection").unwrap(),
        boolean,
        |meta, val| meta.complete = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)(?:\bthe\W)?\bultimate\b[ .]\bcollection\b").unwrap(),
        boolean,
        |meta, val| meta.complete = val,
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)\bcollection\b.*\b(?:set|pack|movies)\b").unwrap(),
        boolean,
        |meta, val| meta.complete = val,
        HandlerOptions::default(),
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)\bcollection(?:(\s\[|\s\())").unwrap(),
        boolean,
        |meta, val| meta.complete = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(
            r"(?i)duology|trilogy|quadr[oi]logy|tetralogy|pentalogy|hexalogy|heptalogy|anthology",
        )
            .unwrap(),
        boolean,
        |meta, val| meta.complete = val,
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)\bcompleta\b").unwrap(),
        boolean,
        |meta, val| meta.complete = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)\bsaga\b").unwrap(),
        boolean,
        |meta, val| meta.complete = val,
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)\b\[Complete\]\b").unwrap(),
        boolean,
        |meta, val| meta.complete = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "complete",
        FancyRegex::new(r"(?i)(?<!A.?|The.?)\bComplete\b").unwrap(),
        boolean,
        |meta, val| meta.complete = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "bitrate",
        FancyRegex::new(r"(?i)\b\d+[kmg]bps\b").unwrap(),
        lowercase,
        |meta, val| meta.bitrate = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "year",
        FancyRegex::new(r"(?:^|[^-])\b(20[0-9]{2}|2100)(?!(?:\s*[-]\s*\d{4}|\s*\d{4})\b)").unwrap(),
        uinteger,
        |meta, val| {
            if let Some(v) = val {
                meta.year = Some(v);
            }
        },
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?:\b[ée]p?(?:isode)?|[Ээ]пизод|[Сс]ер(?:ии|ия|\.)?|cap(?:itulo)?|epis[oó]dio)[. ]?[-:#№]?[. ]?(\d{1,4})(?:[abc]|v0?[1-4]|\W|$)").unwrap(),
        uinteger,
        |meta, val| if let Some(v) = val { meta.episodes.push(v) },
        handler_options! { remove: false, ..Default::default() },
    );

    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\b\d+[0o]+[mg]b\b").unwrap(),
        boolean,
        |meta, val| meta.trash = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:(?:D[ .])?HD[ .-]*)?T(?:ELE)?S(?:YNC)?(?:Rip)?\b").unwrap(),
        |_val| "TeleSync".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "season",
        FancyRegex::new(r"(?i)\b(\d{1,2})x\d{1,2}\b").unwrap(),
        uinteger,
        |meta, val| {
            if let Some(v) = val {
                meta.seasons.push(v);
            }
        },
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "episode",
        FancyRegex::new(r"(?i)\b\d{1,2}x(\d{1,2})\b").unwrap(),
        uinteger,
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.push(v);
            }
        },
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "year",
        FancyRegex::new(r"(?i)[^SE][\[(]?(?!^)(?<![\d-]|Cap[.]?|Ep[.]?)((?:19\d|20[012])\d)(?!(?:\s*[-]\s*\d{4}|\s*\d{4}|kbps)\b)[)\]]?").unwrap(),
        uinteger,
        |meta, val| if let Some(v) = val { meta.year = Some(v) },
        handler_options! { remove: true, ..Default::default() },
    );
    parser.add_handler(
        "year",
        FancyRegex::new(r"(?i)(?!^\w{4})^[(\[]?((?:19\d|20[012])\d)(?!(?:\s*[-]\s*\d{4}|\s*\d{4}|kbps)\b)[)\]]?").unwrap(),
        uinteger,
        |meta, val| if let Some(v) = val {
            println!("YEAR MATCHED: {v}");
            meta.year = Some(v);
        },
        handler_options! { remove: true, ..Default::default() },
    );

    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\b\d{2,3}(th)?[\.\s\-\+_\/(),]Anniversary[\.\s\-\+_\/(),](Edition|Ed)?\b")
            .unwrap(),
        |_| "Anniversary Edition".to_string(),
        |meta, val| meta.edition = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\bRemaster(?:ed)?\b").unwrap(),
        |_| "Remastered".to_string(),
        |meta, val| meta.edition = Some(val),
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "upscaled",
        FancyRegex::new(r"(?i)\b(?:AI.?)?(Upscal(ed?|ing)|Enhanced?)\b").unwrap(),
        boolean,
        |meta, val| meta.upscaled = val,
        handler_options! {
            remove: false,
            ..Default::default()
        },
    );

    parser.add_handler(
        "convert",
        FancyRegex::new(r"\bCONVERT\b").unwrap(),
        boolean,
        |meta, val| meta.convert = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "hardcoded",
        FancyRegex::new(r"\b(HC|HARDCODED)\b").unwrap(),
        boolean,
        |meta, val| meta.hardcoded = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "proper",
        FancyRegex::new(r"(?i)\b(?:REAL.)?PROPER\b").unwrap(),
        boolean,
        |meta, val| meta.proper = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "repack",
        FancyRegex::new(r"(?i)\bREPACK|RERIP\b").unwrap(),
        boolean,
        |meta, val| meta.repack = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "retail",
        FancyRegex::new(r"(?i)\bRetail\b").unwrap(),
        boolean,
        |meta, val| meta.retail = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "remastered",
        FancyRegex::new(r"(?i)\bRemaster(?:ed)?\b").unwrap(),
        boolean,
        |meta, val| meta.remastered = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "documentary",
        FancyRegex::new(r"(?i)\bDOCU(?:menta?ry)?\b").unwrap(),
        boolean,
        |meta, val| meta.documentary = val,
        handler_options! {
            skip_from_title: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "unrated",
        FancyRegex::new(r"(?i)\bunrated\b").unwrap(),
        boolean,
        |meta, val| meta.unrated = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "uncensored",
        FancyRegex::new(r"(?i)\buncensored\b").unwrap(),
        boolean,
        |meta, val| meta.uncensored = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "commentary",
        FancyRegex::new(r"(?i)\bcommentary\b").unwrap(),
        boolean,
        |meta, val| meta.commentary = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "region",
        FancyRegex::new(r"R\dJ?\b").unwrap(),
        uppercase,
        |meta, val| meta.region = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "region",
        FancyRegex::new(r"(?i)\b(PAL|NTSC|SECAM)\b").unwrap(),
        uppercase,
        |meta, val| meta.region = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:HD[ .-]*)?T(?:ELE)?S(?:YNC)?(?:Rip)?\b").unwrap(),
        |_val| "TeleSync".to_string(),
        |meta, val| {
            meta.quality = Some(val);
        },
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:BD|Blu-?Ray|UHD|4K)[ .-]*(?:Remux)\b").unwrap(),
        |_| "BluRay REMUX".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:UHD|BD)Remux\b").unwrap(),
        |_| "BluRay REMUX".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bBlu[ .-]*Ray[ .-]*Rip\b").unwrap(),
        |_| "BRRip".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bremux\b").unwrap(),
        |_| "REMUX".to_string(),
        |meta, val| {
            if let Some(q) = &meta.quality {
                if q.contains("BluRay") || q.contains("BRRip") || q.contains("BDRip") {
                    meta.quality = Some("BluRay REMUX".to_string());
                    return;
                }
            }
            meta.quality = Some(val);
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );

    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bBlu[ .-]*Ray\b(?![ .-]*Rip)").unwrap(),
        |_| "BluRay".to_string(),
        |meta, val| {
            if let Some(q) = &meta.quality {
                if q.contains("REMUX") {
                    meta.quality = Some("BluRay REMUX".to_string());
                    return;
                }
            }
            meta.quality = Some(val);
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:HD)?TC(?:Rip)?\b").unwrap(),
        |_| "TeleCine".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bUHD[ .-]*Rip\b").unwrap(),
        |_| "UHDRip".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bR5\b").unwrap(),
        |_| "R5".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:BD|Blu-?Ray)(?:Rip)?\b").unwrap(),
        |_| "BDRip".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bWEB[ .-]*(?:DLRip|DL-?Rip)\b").unwrap(),
        |_| "WEB-DLRip".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:BD|Blu-?Ray|UHD|4K)[ .-]*(?:Remux)\b").unwrap(),
        |_| "BluRay REMUX".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:UHD|BD)Remux\b").unwrap(),
        |_| "BluRay REMUX".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bWEB[ .-]*(DL|.BDrip)\b").unwrap(),
        |_| "WEB-DL".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?<!\w.)WEB\b|\bWEB(?!([ \.\-\(\],]+\d))\b").unwrap(),
        |_| "WEB".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            skip_from_title: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:HD[ .-]*)?DVD[ .-]*Rip\b").unwrap(),
        |_| "DVDRip".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bHD-?DVD-?Rip\b").unwrap(),
        |_| "DVDRip".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:DVD?|BD|BR|HD)?[ .-]*Scr(?:eener)?\b").unwrap(),
        |_| "SCR".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bDVD(?:R\d?|.*Mux)?\b").unwrap(),
        |_| "DVD".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:H[DQ][ .-]*)?S[ \.\-]print\b").unwrap(),
        |_| "CAM".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            skip_from_title: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b4K[ .-]*UHD[ .-]*remux\b").unwrap(),
        |_| "BluRay REMUX".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:HD)?CAM(?:-?Rip)?\b").unwrap(),
        |_| "CAM".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            skip_from_title: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "bit_depth",
        FancyRegex::new(r"(?i)\bhevc\s?10\b").unwrap(),
        |_| "10bit".to_string(),
        |meta, val| meta.bit_depth = Some(val),
        HandlerOptions::default(),
    );
    parser.add_handler(
        "bit_depth",
        FancyRegex::new(r"(?i)(?:8|10|12)[-\.]?(?=bit\b)").unwrap(),
        |val| format!("{val}bit"),
        |meta, val| meta.bit_depth = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "hdr",
        FancyRegex::new(r"(?i)\bDV\b|dolby.?vision|\bDoVi\b").unwrap(),
        |_| "DV".to_string(),
        |meta, val| {
            if !meta.hdr.contains(&val) {
                meta.hdr.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "hdr",
        FancyRegex::new(r"(?i)HDR10(?:\+|[-\.\s]?plus)").unwrap(),
        |_| "HDR10+".to_string(),
        |meta, val| {
            if !meta.hdr.contains(&val) {
                meta.hdr.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "hdr",
        FancyRegex::new(r"(?i)\bHDR(?:10)?\b").unwrap(),
        |_| "HDR".to_string(),
        |meta, val| {
            if !meta.hdr.contains(&val) {
                meta.hdr.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );

    parser.add_handler(
        "codec",
        FancyRegex::new(r"(?i)\b[hx][\. \-]?264\b").unwrap(),
        |_| "avc".to_string(),
        |meta, val| meta.codec = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "codec",
        FancyRegex::new(r"(?i)\[AVC\]").unwrap(),
        |_| "avc".to_string(),
        |meta, val| meta.codec = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "codec",
        FancyRegex::new(r"(?i)\[HEVC\]").unwrap(),
        |_| "hevc".to_string(),
        |meta, val| meta.codec = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "codec",
        FancyRegex::new(r"(?i)\bAVC[_\s]").unwrap(),
        |_| "avc".to_string(),
        |meta, val| meta.codec = Some(val),
        handler_options! {
            remove: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "codec",
        FancyRegex::new(r"(?i)\bHEVC10(bit)?\b|\b[xh][\. \-]?265\b").unwrap(),
        |_| "hevc".to_string(),
        |meta, val| meta.codec = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "codec",
        FancyRegex::new(r"(?i)\bhevc(?:\s?10)?\b").unwrap(),
        |_| "hevc".to_string(),
        |meta, val| meta.codec = Some(val),
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "codec",
        FancyRegex::new(r"(?i)\bav1\b").unwrap(),
        |_| "av1".to_string(),
        |meta, val| meta.codec = Some(val),
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "codec",
        FancyRegex::new(r"(?i)\b(?:mpe?g\d*)\b").unwrap(),
        |_| "mpeg".to_string(),
        |meta, val| meta.codec = Some(val),
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "codec",
        FancyRegex::new(r"\b\W264\W\b").unwrap(),
        |_| "avc".to_string(),
        |meta, val| meta.codec = Some(val),
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
        |_| "hevc".to_string(),
        |meta, val| meta.codec = Some(val),
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
        |_| "xvid".to_string(),
        |meta, val| meta.codec = Some(val),
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );

    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\bOrg(?:inal)?\W+Aud(?:io)?\b").unwrap(),
        |_| "Original Audio".to_string(),
        |meta, val| {
            if !meta.audio.contains(&val) {
                meta.audio.push(val);
            }
        },
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "channels",
        FancyRegex::new(r"(?i)5[\.\s]1(?:ch|-S\d+)?\b").unwrap(),
        |_| "5.1".to_string(),
        |meta, val| {
            if !meta.channels.contains(&val) {
                meta.channels.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\b(custom.?)?Extended\b").unwrap(),
        |_| "Extended Edition".to_string(),
        |meta, val| meta.edition = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\buncut(?!.gems)\b").unwrap(),
        |_| "Uncut".to_string(),
        |meta, val| meta.edition = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\bRemaster(?:ed)?\b").unwrap(),
        |_| "Remastered".to_string(),
        |meta, val| meta.edition = Some(val),
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\bDirector(')?s.?Cut\b").unwrap(),
        |_| "Directors Cut".to_string(),
        |meta, val| meta.edition = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\bCollector(')?s\b").unwrap(),
        |_| "Collectors Edition".to_string(),
        |meta, val| meta.edition = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\bTheatrical\b").unwrap(),
        |_| "Theatrical".to_string(),
        |meta, val| meta.edition = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\bIMAX\b").unwrap(),
        |_| "IMAX".to_string(),
        |meta, val| meta.edition = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "edition",
        FancyRegex::new(r"(?i)\bUltimate[\.\s\-\+_\/(),]Edition\b").unwrap(),
        |_| "Ultimate Edition".to_string(),
        |meta, val| meta.edition = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "ppv",
        FancyRegex::new(r"(?i)\bPPV\b").unwrap(),
        boolean,
        |meta, val| meta.ppv = val,
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
        |meta, val| meta.ppv = val,
        handler_options! {
            skip_from_title: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "proper",
        FancyRegex::new(r"(?i)\bPROPER\b").unwrap(),
        boolean,
        |meta, val| meta.proper = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "repack",
        FancyRegex::new(r"(?i)\bREPACK\b").unwrap(),
        boolean,
        |meta, val| meta.repack = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "retail",
        FancyRegex::new(r"(?i)\bRetail\b").unwrap(),
        boolean,
        |meta, val| meta.retail = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "extended",
        FancyRegex::new(r"(?i)\bEXTENDED\b").unwrap(),
        boolean,
        |meta, val| meta.extended = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "remastered",
        FancyRegex::new(r"(?i)\bRemastered\b").unwrap(),
        boolean,
        |meta, val| meta.remastered = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "unrated",
        FancyRegex::new(r"(?i)\b(?:uncensored|unrated)\b").unwrap(),
        boolean,
        |meta, val| meta.unrated = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "uncensored",
        FancyRegex::new(r"(?i)\buncensored\b").unwrap(),
        boolean,
        |meta, val| meta.uncensored = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "site",
        FancyRegex::new(r"(?i)^(www?[., ][\w-]+[. ][\w-]+(?:[. ][\w-]+)?)\s+-\s*").unwrap(),
        |val| {
            val.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '.')
                .trim()
                .to_string()
        },
        |meta, val| meta.site = Some(val),
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
        |meta, val| meta.site = Some(val),
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
        |meta, val| meta.site = Some(val),
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
        |meta, val| meta.site = Some(val),
        handler_options! {
            remove: true,
            skip_from_title: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "site",
        FancyRegex::new(r"(?i)^((?:www?[\.,])?[\w-]+\.[\w-]+(?:\.[\w-]+)*?)\s+-\s*").unwrap(),
        |val| {
            val.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '.')
                .trim()
                .to_string()
        },
        |meta, val| meta.site = Some(val),
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );

    parser.add_handler(
        "year",
        FancyRegex::new(r"(?i)\b(19\d{2}\s?-\s?20\d{2})\b").unwrap(),
        |val| {
            val.split(['-', ' '])
                .next()
                .unwrap()
                .parse::<u32>()
                .unwrap()
        },
        |meta, val| meta.year = Some(val),
        handler_options! {
            remove: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "channels",
        FancyRegex::new(r"(?i)\b(?:x[2-4]|5[\W]1(?:x[2-4])?)\b").unwrap(),
        |_| "5.1".to_string(),
        |meta, val| {
            if !meta.channels.contains(&val) {
                meta.channels.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "channels",
        FancyRegex::new(r"(?i)\b7[\.\- ]1(.?ch(annel)?)?\b").unwrap(),
        |_| "7.1".to_string(),
        |meta, val| {
            if !meta.channels.contains(&val) {
                meta.channels.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "channels",
        FancyRegex::new(r"(?i)\+?2[\.\s]0(?:x[2-4])?\b").unwrap(),
        |_| "2.0".to_string(),
        |meta, val| {
            if !meta.channels.contains(&val) {
                meta.channels.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );

    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\b(?!.+HR)(DTS.?HD.?Ma(ster)?|DTS.?X)\b").unwrap(),
        |_| "DTS Lossless".to_string(),
        |meta, val| {
            if !meta.audio.contains(&val) {
                meta.audio.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\bDTS(?!(.?HD.?Ma(ster)?|.X)).?(HD.?HR|HD)?\b").unwrap(),
        |_| "DTS Lossy".to_string(),
        |meta, val| {
            if !meta.audio.contains(&val) {
                meta.audio.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\b(Dolby.?)?Atmos\b").unwrap(),
        |_| "Atmos".to_string(),
        |meta, val| {
            if !meta.audio.contains(&val) {
                meta.audio.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\b(True[ .-]?HD|\.True\.)\b").unwrap(),
        |_| "TrueHD".to_string(),
        |meta, val| {
            if !meta.audio.contains(&val) {
                meta.audio.push(val);
            }
        },
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
        |_| "TrueHD".to_string(),
        |meta, val| {
            if !meta.audio.contains(&val) {
                meta.audio.push(val);
            }
        },
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
        |_| "FLAC".to_string(),
        |meta, val| {
            if !meta.audio.contains(&val) {
                meta.audio.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)DD2?[\+p]|DD Plus|Dolby Digital Plus|DDP(5[ \.\_]1)?|E-?AC-?3(?:-S\d+)?")
            .unwrap(),
        |_| "Dolby Digital Plus".to_string(),
        |meta, val| {
            if !meta.audio.contains(&val) {
                meta.audio.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\bddp(5.1)?").unwrap(),
        |_| "Dolby Digital Plus".to_string(),
        |meta, val| {
            if !meta.audio.contains(&val) {
                meta.audio.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\bMP3\b").unwrap(),
        |_| "MP3".to_string(),
        |meta, val| {
            if !meta.audio.contains(&val) {
                meta.audio.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\b(DD|Dolby.?Digital|DolbyD|AC-?3(x2)?(?:-S\d+)?)\b").unwrap(),
        |_| "Dolby Digital".to_string(),
        |meta, val| {
            if !meta.audio.contains(&val) {
                meta.audio.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "audio",
        FancyRegex::new(r"(?i)\bQ?Q?AAC(x?2)?\b").unwrap(),
        |_| "AAC".to_string(),
        |meta, val| {
            if !meta.audio.contains(&val) {
                meta.audio.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );

    parser.add_handler(
        "group",
        FancyRegex::new(r"(?i)- ?(?!\d+$|S\d+|\d+x|ep?\d+|[^\[]+]$)([^\-. \[]+[^\-. \[)\]\d][^\-. \[)\]]*)(?:\[[\w.-]+])?(?=\.\w{2,4}$|$)").unwrap(),
        value,
        |meta, val| meta.group = Some(val),
        handler_options! { remove: false, ..Default::default() },
    );
    parser.add_handler(
        "group",
        FancyRegex::new(r"\(([\w-]+)\)(?:$|\.\w{2,4}$)").unwrap(),
        value,
        |meta, val| meta.group = Some(val),
        HandlerOptions::default(),
    );
    parser.add_handler(
        "group",
        FancyRegex::new(r"^\[([^\[\]]+)\]").unwrap(),
        value,
        |meta, val| meta.group = Some(val),
        HandlerOptions::default(),
    );

    parser.add_handler(
        "volumes",
        FancyRegex::new(r"(?i)\bvol(?:s|umes?)?[. -]*(?:\d{1,2}[., +/\\&-]+)+\d{1,2}\b").unwrap(),
        range_i32,
        |meta, val| {
            if let Some(v) = val {
                meta.volumes = v;
            }
        },
        handler_options! {
            remove: true,
            ..Default::default()
        },
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
                    let volume_number = cap
                        .get(1)
                        .map_or(0, |m| m.as_str().parse::<i32>().unwrap_or(0));

                    context.result.volumes = vec![volume_number];
                }

                let abs_start = start_index + relative_start;

                let info = MatchInfo {
                    raw_match,
                    match_index: abs_start,
                    remove: true,
                    skip_from_title: false,
                };

                context.matched.insert("volumes".to_string(), info.clone());
                return Some(info);
            }
            None
        }),
    );

    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:complete\W|seasons?\W|\W|^)((?:s\d{1,2}[., +/\\&-]+)+s\d{1,2}\b)")
            .unwrap(),
        parse_season_range,
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:complete\W|seasons?\W|\W|^)[(\[]?(s\d{2,}-\d{2,}\b)[)\]]?").unwrap(),
        parse_season_range,
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:complete\W|seasons?\W|\W|^)[(\[]?(s[1-9]-[2-9])[)\]]?").unwrap(),
        parse_season_range,
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)\d+ª(?:.+)?(?:a.?)?\d+ª(?:(?:.+)?(?:temporadas?))").unwrap(),
        parse_season_range,
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:(?:\bthe\W)?\bcomplete\W)?(?:seasons?|[Сс]езони?|temporadas?)[. ]?[-:]?[. ]?[( \[]?((?:\d{1,2}[., /\\&]+)+\d{1,2}\b)[)\]]?").unwrap(),
        parse_season_range,
        |meta, val| meta.seasons.extend(val),
        handler_options! { remove: true, ..Default::default() },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:(?:\bthe\W)?\bcomplete\W)?(?:seasons?|[Сс]езони?|temporadas?)[. ]?[-:]?[. ]?[( \[]?((?:\d{1,2}[.-]+)+[1-9]\d?\b)(?!\W*\d{4})[)\]]?").unwrap(),
        parse_season_range,
        |meta, val| meta.seasons.extend(val),
        handler_options! { remove: true, ..Default::default() },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:(?:\bthe\W)?\bcomplete\W)?season[. ]?[( \[]?((?:\d{1,2}[. -]+)+[1-9]\d?\b)[)\]]?(?!.*\.\w{2,4}$)").unwrap(),
        parse_season_range,
        |meta, val| meta.seasons.extend(val),
        handler_options! { remove: true, ..Default::default() },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:(?:\bthe\W)?\bcomplete\W)?\bseasons?\b[. -]?(\d{1,2}[. -]?(?:to|thru|and|\+|:)[. -]?\d{1,2})\b").unwrap(),
        parse_season_range,
        |meta, val| meta.seasons.extend(val),
        handler_options! { remove: true, ..Default::default() },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bDVB(?:\b|-)").unwrap(),
        |_| "HDTV".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:(?:\bthe\W)?\bcomplete\W)?(?:saison|seizoen|season|series|temp(?:orada)?):?[. ]?(\d{1,2})\b").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! { remove: true, ..Default::default() },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(\d{1,2})(?:-?й)?[. _]?(?:[Сс]езон|sez(?:on)?)(?:\W?\D|$)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)[Сс]езон:?[. _]?№?(\d{1,2})(?!\d)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:\D|^)(\d{1,2})Â?[°ºªa]?[. ]*temporada").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)t(\d{1,3})(?:[ex]+|$)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:(?:\bthe\W)?\bcomplete)?(?<![a-z])\bs(\d{1,3})(?:[\Wex]|\d{2}\b|$)")
            .unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: false,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(
            r"(?i)(?:(?:\bthe\W)?\bcomplete\W)?(?:\W|^)(\d{1,2})[. ]?(?:st|nd|rd|th)[. ]*season",
        )
            .unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?<=S)\d{2}(?=E\d+)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:\D|^)(\d{1,2})[xх]\d{1,3}(?:\D|$)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)\bSn([1-9])(?:\D|$)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)[(\[](\d{1,2})\.\d{1,3}[)\]]").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)-\s?(\d{1,2})\.\d{2,3}\s?-").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:^|\/)(\d{1,2})-\d{2}\b(?!-\d)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)[^\w-](\d{1,2})-\d{2}(?=\.\w{2,4}$|$)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)\b(\d{2})[ ._]\d{2}(?:.F)?\.\w{2,4}$").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)\bEp(?:isode)?\W+(\d{1,2})\.\d{1,3}\b").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)\bSeasons?\b.*\b(?!(?:19|20)\d{2})(\d{1,2}-\d{1,2})\b").unwrap(),
        parse_season_range,
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)(?:\W|^)(\d{1,2})(?:e|ep)\d{1,3}(?:\W|$)").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)\bТВ-(\d{1,2})\b").unwrap(),
        |val| vec![val.parse().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "seasons",
        FancyRegex::new(r"(?i)\bs(\d{1,4})").unwrap(),
        |val| vec![val.parse::<u32>().unwrap_or(0)],
        |meta, val| meta.seasons.extend(val),
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "episodes",
        FancyRegex::new(
            r"(?i)(?:[\W\d]|^)e[ .]?[\[(]?(\d{1,3}(?:[ .-]*(?:[&+]|e|.){1,2}(?:[ .]*e)?[ .]?\d{1,3})+)(?:\W|$)",
        ).unwrap(),
        range_u32,
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?:[\W\d]|^)ep[ .]?[\[(]?(\d{1,3}(?:[ .-]*(?:[&+]|ep){1,2}[ .]?\d{1,3})+)(?:\W|$)").unwrap(),
        range_u32,
        |meta, val| if let Some(v) = val { meta.episodes.extend(v) },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(
            r"(?i)(?:[\W\d]|^)\d+[xх][ .]?[\[(]?(\d{1,3}(?:[ .]?[xх][ .]?\d{1,3})+)(?:\W|$)",
        ).unwrap(),
        range_u32,
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)Серии:\s+(\d+)\s+(?:of|из|iz)\s+\d+\b").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?:[\W\d]|^)(?:episodes?|[Сс]ерии:?)[ .]?[\[(]?(\d{1,3}(?:[ .+]*[&+][ .]?\d{1,3})+)(?:\W|$)").unwrap(),
        range_u32,
        |meta, val| if let Some(v) = val { meta.episodes.extend(v) },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)[\[(]?(?:\D|^)(\d{1,3}[ .]?ao[ .]?\d{1,3})[)\]]?(?:\W|$)").unwrap(),
        range_u32,
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?:[\W\d]|^)(?:e|eps?|episodes?|[Сс]ерии:?|\d+[xх])[ .]*[\[(]?(\d{1,3}(?:-\d{1,3})+)(?:\W|$)").unwrap(),
        range_u32,
        |meta, val| if let Some(v) = val { meta.episodes.extend(v) },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?:\W|^)(\d{1,3}(?:[ .]*~[ .]*\d{1,3})+)(?:\W|$)").unwrap(),
        range_u32,
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)\bE\d{1,4}\s*à\s*E\d{1,4}\b").unwrap(),
        range_u32,
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)[st]\d{1,2}[. ]?[xх-]?[. ]?(?:e|x|х|ep|-|\.)[. ]?(\d{1,4})(?:[abc]|v0?[1-4]|\D|$)").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| if let Some(v) = val { meta.episodes.extend(v) },
        handler_options! { remove: true, ..Default::default() },
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)\b[st]\d{2}(\d{2})\b").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)-\s(\d{1,3}[ .]*-[ .]*\d{1,3})(?!-\d)(?:\W|$)").unwrap(),
        range_u32,
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)s\d{1,2}\s?\((\d{1,3}[ .]*-[ .]*\d{1,3})\)").unwrap(),
        range_u32,
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?:^|/)\d{1,2}-(\d{2})\b(?!-\d)").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?<!\d-)\b\d{1,2}-(\d{2})(?=\.\w{2,4}$)").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?<=^\[.+].+)[. ]+-[. ]+(\d{1,4})[. ]+(?=\W)").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?<!(?:seasons?|[Сс]езони?)\W*)(?:[ .(\[-]|^)(\d{1,3}(?:[ .]?[,&+~][ .]?\d{1,3})+)(?:[ .)\]-]|$)").unwrap(),
        range_u32,
        |meta, val| if let Some(v) = val { meta.episodes.extend(v) },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?<!(?:seasons?|[Сс]езони?)\W*)(?:[ .(\[-]|^)(\d{1,3}(?:-\d{1,3})+)(?:[ .)\(\]]|-\D|$)").unwrap(),
        range_u32,
        |meta, val| if let Some(v) = val { meta.episodes.extend(v) },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)\bEp(?:isode)?\W+\d{1,2}\.(\d{1,3})\b").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)Ep.\d+.-.\\d+").unwrap(),
        range_u32,
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?:\b[ée]p?(?:isode)?|[Ээ]пизод|[Сс]ер(?:ии|ия|\.)?|cap(?:itulo)?|epis[oó]dio)[. ]?[-:#№]?[. ]?(\d{1,4})(?:[abc]|v0?[1-4]|\W|$)").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| if let Some(v) = val { meta.episodes.extend(v) },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)\b(\d{1,3})(?:-?я)?[ ._-]*(?:ser(?:i?[iyja]|\b)|[Сс]ер(?:ии|ия|\.)?)")
            .unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?:\D|^)\d{1,2}[. ]?[xх][. ]?(\d{1,3})(?:[abc]|v0?[1-4]|\D|$)").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?<=S\d{2}E)(\d+)").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"[\[(]\d{1,2}\.(\d{1,3})[)\]]").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"\b[Ss]\d{1,2}[ .](\d{1,2})\b").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"-\s?\d{1,2}\.(\d{2,3})\s?-").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?:\[|\()(\d+)\s(?:of|из|iz)\s\d+(?:\]|\))").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?<=\D|^)(\d{1,3})[. ]?(?:of|из|iz)[. ]?\d{1,3}(?=\D|$)").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"\b\d{2}[ ._-](\d{2})(?:.F)?\.\\w{2,4}$").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(\d+)(?=.?\[([A-Z0-9]{8})\])").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        HandlerOptions::default(),
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?<!\bMovie\s-\s)(?<=\s-\s)(\d+)(?=\s[-(\s])").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)(?:\W|^)(?:\d+)?(?:e|ep)(\d{1,3})(?:\W|$)").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "episodes",
        FancyRegex::new(r"(?i)E(\d+)\b").unwrap(),
        |val| Some(vec![val.parse::<u32>().unwrap_or(0)]),
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
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
        |meta, val| {
            if let Some(v) = val {
                meta.episodes.extend(v);
            }
        },
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
        |meta, val| meta.country = Some(val),
        HandlerOptions::default(),
    );

    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bengl?(?:sub[A-Z]*)?\b").unwrap(),
        |_| "en".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bEnglish[\. _-]*(?:subs?|sdh|hi)\b").unwrap(),
        |_| "en".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:ingl[eéê]s|inglese?)\b").unwrap(),
        |_| "en".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\[En\b").unwrap(),
        |_| "en".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bIT\s+EN\b").unwrap(),
        |_| "it".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
                meta.languages.push("en".to_string());
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bEng(?:,|\s)").unwrap(),
        |_| "en".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bCze(?:ch)?\b").unwrap(),
        |_| "cs".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bGer(?:,|\s|\b)").unwrap(),
        |_| "de".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\[spanish\]").unwrap(),
        |_| "es".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:español|espanhol)\b").unwrap(),
        |_| "es".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bFR(?:a|e|anc[eê]s|VF[FQIB2]?)\b").unwrap(),
        |_| "fr".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b\[?(VF[FQRIB2]?\]?\b|(VOST)?FR2?)\b").unwrap(),
        |_| "fr".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:GERMAN|GER)\b|(?-i)\bDE\b").unwrap(),
        |_| "de".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(TRUE|SUB).?FRENCH\b|\bFRENCH\b|\bFre?\b").unwrap(),
        |_| "fr".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
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
        |_| "fr".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(VF[FQIB2]?|(TRUE|SUB).?FRENCH|(VOST)?FR2?)\b").unwrap(),
        |_| "fr".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bspanish\W?latin|american\W*(?:spa|esp?)").unwrap(),
        |_| "la".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:\bla\b.+(?:cia\b))").unwrap(),
        |_| "es".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:audio.)?lat(?:in?|ino)?\b").unwrap(),
        |_| "la".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:audio.)?(?:ESP?|spa|(en[ .]+)?espa[nñ]ola?|castellano)\b").unwrap(),
        |_| "es".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bes(?=[ .,/-]+(?:[A-Z]{2}[ .,/-]+){2,})\b").unwrap(),
        |_| "es".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?<=[ .,/-]+(?:[A-Z]{2}[ .,/-]+){2,})es\b").unwrap(),
        |_| "es".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?<=[ .,/-]+[A-Z]{2}[ .,/-]+)es(?=[ .,/-]+[A-Z]{2}[ .,/-]+)\b").unwrap(),
        |_| "es".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bes(?=\.(?:ass|ssa|srt|sub|idx)$)").unwrap(),
        |_| "es".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(temporadas?|completa)\b").unwrap(),
        |_| "es".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:INT[EÉ]GRALE?)\b").unwrap(),
        |_| "fr".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: false,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:Saison)\b").unwrap(),
        |_| "fr".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: false,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:p[rt]|en|port)[. (\\/-]*BR\b").unwrap(),
        |_| "pt".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bbr(?:a|azil|azilian)\W+(?:pt|por)\b").unwrap(),
        |_| "pt".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:leg(?:endado|endas?)?|dub(?:lado)?|portugu[eèê]se?)[. -]*BR\b")
            .unwrap(),
        |_| "pt".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bleg(?:endado|endas?)\b").unwrap(),
        |_| "pt".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bportugu[eèê]s[ea]?\b").unwrap(),
        |_| "pt".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bPT[. -]*(?:PT|ENG?|sub(?:s|titles?))\b").unwrap(),
        |_| "pt".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bpt(?=\.(?:ass|ssa|srt|sub|idx)$)").unwrap(),
        |_| "pt".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bPT\b").unwrap(),
        |_| "pt".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bpor\b").unwrap(),
        |_| "pt".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b-?ITA\b").unwrap(),
        |_| "it".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?<!w{3}\.\w+\.)IT(?=[ .,/-]+(?:[a-zA-Z]{2}[ .,/-]+){2,})\b").unwrap(),
        |_| "it".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bit(?=\.(?:ass|ssa|srt|sub|idx)$)").unwrap(),
        |_| "it".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bitaliano?\b").unwrap(),
        |_| "it".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bslo(?:vak|vakian|subs|[\]_)]?\.\w{2,4}$)\b").unwrap(),
        |_| "sk".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bHU\b").unwrap(),
        |_| "hu".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bHUN(?:garian)?\b").unwrap(),
        |_| "hu".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bROM(?:anian)?\b").unwrap(),
        |_| "ro".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bRO(?=[ .,/-]*(?:[A-Z]{2}[ .,/-]+)*sub)").unwrap(),
        |_| "ro".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bbul(?:garian)?\b").unwrap(),
        |_| "bg".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:srp|serbian)\b").unwrap(),
        |_| "sr".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:HRV|croatian)\b").unwrap(),
        |_| "hr".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bHR(?=[ .,/-]*(?:[A-Z]{2}[ .,/-]+)*sub)\b").unwrap(),
        |_| "hr".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bslovenian\b").unwrap(),
        |_| "sl".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)NL|dut|holand[eê]s)\b").unwrap(),
        |_| "nl".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bdutch\b").unwrap(),
        |_| "nl".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bflemish\b").unwrap(),
        |_| "nl".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:DK|danska|dansub|nordic)\b").unwrap(),
        |_| "da".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(danish|dinamarqu[eê]s)\b").unwrap(),
        |_| "da".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bdan\b(?=.*\.(?:srt|vtt|ssa|ass|sub|idx)$)").unwrap(),
        |_| "da".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.|Sci-)FI|finsk|finsub|nordic)\b").unwrap(),
        |_| "fi".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bfinnish\b").unwrap(),
        |_| "fi".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)SE|swe|swesubs?|sv(?:ensk)?|nordic)\b").unwrap(),
        |_| "sv".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(swedish|sueco)\b").unwrap(),
        |_| "sv".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:NOR|norsk|norsub|nordic)\b").unwrap(),
        |_| "no".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(norwegian|noruegu[eê]s|bokm[aå]l|nob)\b").unwrap(),
        |_| "no".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bnor\b(?=[\]_)]?\.\\w{2,4}$)").unwrap(),
        |_| "no".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:TURKISH|TUR|TIVIBU)\b").unwrap(),
        |_| "tr".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:HEBREW|HEB)\b").unwrap(),
        |_| "he".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:POLISH|POL)\b").unwrap(),
        |_| "pl".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
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
        |_| "bn".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:JP|JAP|JPN)\b").unwrap(),
        |_| "ja".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(japanese|japon[eê]s)\b").unwrap(),
        |_| "ja".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:KOR|kor[ .-]?sub)\b").unwrap(),
        |_| "ko".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(korean|coreano)\b").unwrap(),
        |_| "ko".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:traditional\W*chinese|chinese\W*traditional)(?:\Wchi)?\b").unwrap(),
        |_| "zh".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bzh-hant\b").unwrap(),
        |_| "zh".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:mand[ae]rin|ch[sn])\b").unwrap(),
        |_| "zh".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bgreek[ .-]*(?:audio|lang(?:uage)?|subs?(?:titles?)?)?\b").unwrap(),
        |_| "el".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:GER|DEU)\b").unwrap(),
        |_| "de".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bde(?=[ .,/-]+(?:[A-Z]{2}[ .,/-]+){2,})\b").unwrap(),
        |_| "de".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?<=[ .,/-]+(?:[A-Z]{2}[ .,/-]+){2,})de\b").unwrap(),
        |_| "de".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?<=[ .,/-]+[A-Z]{2}[ .,/-]+)de(?=[ .,/-]+[A-Z]{2}[ .,/-]+)\b").unwrap(),
        |_| "de".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bde(?=\.(?:ass|ssa|srt|sub|idx)$)").unwrap(),
        |_| "de".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(german|alem[aã]o)\b").unwrap(),
        |_| "de".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bRUS?\b").unwrap(),
        |_| "ru".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(russian|russo)\b").unwrap(),
        |_| "ru".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bUKR\b").unwrap(),
        |_| "uk".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bukrainian\b").unwrap(),
        |_| "uk".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bhin(?:di)?\b").unwrap(),
        |_| "hi".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
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
                    let volume_number = cap
                        .get(1)
                        .map_or(0, |m| m.as_str().parse::<i32>().unwrap_or(0));

                    context.result.volumes = vec![volume_number];
                }

                let abs_start = start_index + relative_start;

                let info = MatchInfo {
                    raw_match,
                    match_index: abs_start,
                    remove: true,
                    skip_from_title: false,
                };

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

                    let info = MatchInfo {
                        raw_match,
                        match_index: m.start(),
                        remove: true,
                        skip_from_title: true,
                    };
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
        |_| "pl".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(PLDUB|PLSUB|DUBPL|DubbingPL|LekPL|LektorPL)\b").unwrap(),
        |_| "pl".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(PLDUB|PLSUB|DUBPL|DubbingPL|LekPL|LektorPL)\b").unwrap(),
        |_| "pl".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)tel(?!\W*aviv)|telugu)\b").unwrap(),
        |_| "te".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bt[aâ]m(?:il)?\b").unwrap(),
        |_| "ta".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)MAL(?:ay)?|malayalam)\b").unwrap(),
        |_| "ml".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
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
        |_| "kn".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)MAR(?:a(?:thi)?)?|marathi)\b").unwrap(),
        |_| "mr".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)GUJ(?:arati)?|gujarati)\b").unwrap(),
        |_| "gu".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)PUN(?:jabi)?|punjabi)\b").unwrap(),
        |_| "pa".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(?:(?<!w{3}\.\w+\.)BEN(?!.\bThe|and|of\b)(?:gali)?|bengali)\b").unwrap(),
        |_| "bn".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)(?<!shang-?)\bCH(?:I|T)\b").unwrap(),
        |_| "zh".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_from_title: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\b(chinese|chin[eê]s)\b").unwrap(),
        |_| "zh".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_first: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\bzh-hans\b").unwrap(),
        |_| "zh".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "languages",
        FancyRegex::new(r"(?i)\benglish?\b").unwrap(),
        |_| "en".to_string(),
        |meta, val| {
            if !meta.languages.contains(&val) {
                meta.languages.push(val);
            }
        },
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
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bSAT(?:Rip)?\b").unwrap(),
        |_| "SATRip".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            ..Default::default()
        },
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
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bPPVRip\b").unwrap(),
        |_| "PPVRip".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bWEBMux\b").unwrap(),
        |_| "WEBMux".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\b(?:HDRip|MicroHD)\b").unwrap(),
        |_| "HDRip".to_string(),
        |meta, val| meta.quality = Some(val),
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "quality",
        FancyRegex::new(r"(?i)\bRemux\b").unwrap(),
        |_| "REMUX".to_string(),
        |meta, _val| {
            if let Some(ref q) = meta.quality {
                if !q.contains("REMUX") {
                    meta.quality = Some(format!("{q} REMUX"));
                }
            } else {
                meta.quality = Some("REMUX".to_string());
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );

    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\bS-Print\b").unwrap(),
        boolean,
        |meta, val| {
            meta.trash = val;
            if val {
                meta.quality = Some("CAM".to_string());
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\bTELECINE\b").unwrap(),
        boolean,
        |meta, val| {
            meta.trash = val;
            if val {
                meta.quality = Some("TeleCine".to_string());
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );

    parser.add_handler(
        "subbed",
        FancyRegex::new(r"(?i)\bmulti(?:ple)?[ .-]*(?:su?$|sub\w*|dub\w*)\b|msub").unwrap(),
        boolean,
        |meta, val| meta.subbed = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "subbed",
        FancyRegex::new(r"(?i)\b(?:Official.*?|Dual-?)?sub(s|bed)?\b").unwrap(),
        boolean,
        |meta, val| meta.subbed = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "dubbed",
        FancyRegex::new(r"(?i)[\[(\s]?\bmulti(?:ple)?[ .-]*(?:lang(?:uages?)?|audio|VF2)\b\][\[(\s]?")
            .unwrap(),
        boolean,
        |meta, val| meta.dubbed = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "dubbed",
        FancyRegex::new(r"(?i)\btri(?:ple)?[ .-]*(?:audio|dub\w*)\b").unwrap(),
        boolean,
        |meta, val| meta.dubbed = val,
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "dubbed",
        FancyRegex::new(r"(?i)\bdual[ .-]*(?:au?$|[aá]udio|line)\b").unwrap(),
        boolean,
        |meta, val| meta.dubbed = val,
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "dubbed",
        FancyRegex::new(r"(?i)\bdual\b(?![ .-]*sub)").unwrap(),
        boolean,
        |meta, val| meta.dubbed = val,
        handler_options! {
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "dubbed",
        FancyRegex::new(r"(?i)\b(fan\s?dub)\b").unwrap(),
        boolean,
        |meta, val| meta.dubbed = val,
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
        |meta, val| meta.dubbed = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "dubbed",
        FancyRegex::new(
            r"(?i)\b(?!.*\bsub(s|bed)?\b)([ _\-\[(\.]*)?(dual|multi)([ _\-\[(\.]*)?(audio)\b",
        )
            .unwrap(),
        boolean,
        |meta, val| meta.dubbed = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "dubbed",
        FancyRegex::new(r"(?i)\bMULTi\b").unwrap(),
        boolean,
        |meta, val| meta.dubbed = val,
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "3d",
        FancyRegex::new(r"(?i)\b3D\b").unwrap(),
        boolean,
        |meta, val| meta.is_3d = val,
        handler_options! {
            remove: false,
            skip_if_first: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "size",
        FancyRegex::new(r"(?i)\b(\d+(\.\d+)?\s?(MB|GB|TB))\b").unwrap(),
        |val| val.replace(' ', "").to_uppercase(),
        |meta, val| meta.size = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "size",
        FancyRegex::new(r"(?i)[-\s](\d+(?:\.\d+)?(?:MB|GB|TB))[-\s]").unwrap(),
        |val| val.replace(' ', "").to_uppercase(),
        |meta, val| meta.size = Some(val),
        handler_options! {
            remove: true,
            skip_if_already_found: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "site",
        FancyRegex::new(
            r"(?i)\b(?:www?.?)?(?:\w+\-)?\w+\.(?:com|org|net|ms|tv|mx|co|party|vip|nu|pics|re)\b",
        ).unwrap(),
        value,
        |meta, val| meta.site = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "site",
        FancyRegex::new(r"(?i)\bwww?.?[\w.-]+\.(?:link|world|cam|xyz|info|club)\b").unwrap(),
        value,
        |meta, val| meta.site = Some(val),
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "site",
        FancyRegex::new(r"(?i)\bwww\.?[\s.]?(\w+[\.\s]?\w+)\b").unwrap(),
        |_| String::new(),
        |meta, val| {
            meta.site = Some(val);
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );

    parser.add_handler(
        "network",
        FancyRegex::new(r"(?i)\bNF|Netflix\b").unwrap(),
        |_| "Netflix".to_string(),
        |meta, val| {
            if !meta.networks.contains(&val) {
                meta.networks.push(val);
            }
        },
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "network",
        FancyRegex::new(r"(?i)\bAMZN\b").unwrap(),
        |_| "Amazon".to_string(),
        |meta, val| {
            if !meta.networks.contains(&val) {
                meta.networks.push(val);
            }
        },
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );
    parser.add_handler(
        "network",
        FancyRegex::new(r"(?i)\bHULU\b").unwrap(),
        |_| "Hulu".to_string(),
        |meta, val| {
            if !meta.networks.contains(&val) {
                meta.networks.push(val);
            }
        },
        handler_options! {
            remove: true,
            skip_if_already_found: false,
            ..Default::default()
        },
    );
    parser.add_handler(
        "network",
        FancyRegex::new(r"(?i)\bANPL\b").unwrap(),
        |_| "Animal Planet".to_string(),
        |meta, val| {
            if !meta.networks.contains(&val) {
                meta.networks.push(val);
            }
        },
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "trash",
        FancyRegex::new(r"(?i)\bCUSTOM\b").unwrap(),
        boolean,
        |meta, val| {
            meta.trash = val;
        },
        handler_options! {
            remove: true,
            ..Default::default()
        },
    );

    parser.add_handler(
        "extension",
        FancyRegex::new(r"(?i)\.(3g2|3gp|avi|flv|mkv|mk3d|mov|mp2|mp4|m4v|mpe|mpeg|mpg|mpv|webm|wmv|ogm|divx|ts|m2ts|iso|vob|sub|idx|ttxt|txt|smi|srt|ssa|ass|vtt|nfo|html)$").unwrap(),
        |val| val.to_lowercase().trim_start_matches('.').to_string(),
        |meta, val| meta.extension = Some(val),
        handler_options! { remove: true, ..Default::default() },
    );
}
