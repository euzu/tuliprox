use super::{
    const_string, options_no_skip, options_remove_no_skip, options_remove_skip_from_title_no_skip,
    options_remove_skip_if_already_found, options_skip_from_title_no_skip, push_language, push_language_and_en,
};
use crate::ptt::parser::{handler_options, PttParser};
use fancy_regex::Regex as FancyRegex;

#[allow(clippy::too_many_lines)]
pub(super) fn register(parser: &mut PttParser) {
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
}
