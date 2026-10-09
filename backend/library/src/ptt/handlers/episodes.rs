use super::{
    const_string, extend_episodes, extend_seasons, options_default, options_keep, options_no_skip, options_remove,
    options_remove_no_skip, options_remove_skip_if_already_found, parse_season_range, push_audio, push_channels,
    set_country, set_group, set_quality, set_volumes_if_present, set_year,
};
use crate::ptt::{
    parser::{handler_options, MatchInfo, ParseContext, PttParser},
    transformers::{range_i32, range_u32, value},
};
use fancy_regex::Regex as FancyRegex;

#[allow(clippy::too_many_lines)]
pub(super) fn register(parser: &mut PttParser) {
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

            let start_index = matched.get("year").map_or(0, |m| {
                let mut idx = m.match_index.min(title.len());
                while idx > 0 && !title.is_char_boundary(idx) {
                    idx -= 1;
                }
                idx
            });

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
}
