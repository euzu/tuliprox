use super::{
    const_string, options_default, options_keep, options_no_skip, options_remove, options_remove_no_skip,
    options_remove_skip_if_already_found, options_skip_from_title, push_episode, push_season, set_bitrate,
    set_commentary, set_complete, set_convert, set_date, set_documentary, set_edition, set_hardcoded, set_proper,
    set_quality, set_region, set_remastered, set_repack, set_retail, set_trash, set_uncensored, set_unrated,
    set_upscaled, set_year, set_year_with_trace,
};
use crate::ptt::{
    parser::{handler_options, PttParser},
    transformers::{boolean, date, lowercase, uinteger, uppercase},
};
use fancy_regex::Regex as FancyRegex;

#[allow(clippy::too_many_lines)]
pub(super) fn register(parser: &mut PttParser) {
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
}
