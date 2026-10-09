use super::*;
use crate::{
    foundation::mapper::EvalResult::{Named, Undefined, Value},
    model::{
        PlaylistItem, PlaylistItemHeader, PlaylistItemType, SeriesStreamProperties, StreamProperties, TemplateValue,
        VideoStreamDetailProperties, VideoStreamProperties,
    },
    utils::Internable,
};
use regex::Regex;

#[test]
fn test_mapper_dsl_eval() {
    let dsl = r#"
            coast = @Caption ~ "(?i)\b(EAST|WEST)\b"
            quality = @Caption ~ "(?i)\b([FUSL]?HD|SD|4K|1080p|720p|3840p)\b"
            quality = uppercase(quality)
            quality = map quality {
                       "SHD" => "SD",
                       "LHD" => "HD",
                       "720p" => "HD",
                       "1080p" => "FHD",
                       "4K" => "UHD",
                       "3840p" => "UHD",
                        _ => quality,
            }
            coast_quality = match {
                (coast, quality) => concat(capitalize(coast), " ", uppercase(quality)),
                coast => concat(capitalize(coast), " HD"),
                quality => concat("East ", uppercase(quality)),
            }
            @Caption = concat("US: TNT", " ", coast_quality)
            @Group = "United States - Entertainment"
    "#;

    let mapper = MapperScript::parse(dsl, None).expect("Parsing failed");
    println!("Program: {mapper:?}");
    let mut channels: Vec<PlaylistItem> = vec![
        ("D", "HD"),
        ("A", "FHD"),
        ("Z", ""),
        ("K", "HD"),
        ("B", "HD"),
        ("A", "HD"),
        ("K", "SHD"),
        ("C", "LHD"),
        ("L", "FHD"),
        ("R", "UHD"),
        ("T", "SD"),
        ("A", "FHD"),
    ]
    .into_iter()
    .map(|(name, quality)| PlaylistItem {
        header: PlaylistItemHeader { title: format!("Chanel {name} [{quality}]").into(), ..Default::default() },
    })
    .collect::<Vec<PlaylistItem>>();

    for pli in &mut channels {
        let mut accessor = ValueAccessor { pli, virtual_items: vec![], match_as_ascii: false, changed_fields: vec![] };
        mapper.eval(&mut accessor, None);
        println!("Result: {pli:?}");
    }

    // ctx.fields.insert("Caption".to_string(), "US: TNT East LHD bubble".to_string());
    //
    // for stmt in &program.statements {
    //     //let res = stmt.eval(&mut ctx);
    //     println!("Statement Result: {:?}", res);
    // }
    //
    // println!("Result variable: {:?}", ctx.variables.get("result"));
    // assert_eq!(ctx.variables.get("result").unwrap(), "US: TNT East HD");
}

#[test]
fn test_complex() {
    let script = r#"
        print("LOCAL")
            coast = @Caption ~ "!COAST!"
            quality = uppercase(@Caption ~ "!QUALITY!")

            quality = map quality {
              "SHD" | "SD"           => "SD",
              "LHD" | "720P" | "HD"  => "HD",
              "FHD" | "1080P"        => "FHD",
              "UHD" | "4K" | "3840P" => "UHD",
              _ => quality,
            }

            coast_quality = match {
                (coast, quality) => concat(capitalize(coast), " ", uppercase(quality)),
                quality => uppercase(quality),
                _ => "HD",
            }

            network = uppercase(first(@Caption ~ "(?i)\b(CBS|NBC|FOX|ABC|PBS|CW|UNIVISION)\b"))
            station = map network {
              "CBS" => @Caption ~ "(?i)\b(WINK|WFOR)\b",
              "NBC" => @Caption ~ "(?i)\b(WBBH|WTVJ)\b",
              "FOX" => @Caption ~ "(?i)\b(WFTX|WSVM)\b",
              "ABC" => @Caption ~ "(?i)\b(WZVN|WPLG)\b",
              "PBS" => @Caption ~ "(?i)\b(WGCU|WPBT)\b",
              "CW" => @Caption ~ "(?i)\b(WINK|WSFL)\b",
              "UNIVISION" => @Caption ~ "(?i)\b(WUVF|WLTV)\b",
              _ => null,
            }

            match {
              station => {
                station = uppercase(station)
                @Caption = map station {
                  "WINK" => concat("!US_CBS_FM_PREFIX!", " ", coast_quality),
                  "WBBH" => concat("!US_NBC_FM_PREFIX!", " ", coast_quality),
                  "WFTX" => concat("!US_FOX_FM_PREFIX!", " ", coast_quality),
                  "WZVN" => concat("!US_ABC_FM_PREFIX!", " ", coast_quality),
                  "WGCU" => concat("!US_PBS_FM_PREFIX!", " ", coast_quality),
                  "WUVF" => concat("!US_UNIVISION_FM_PREFIX!", " ", coast_quality),

                  "WFOR" => concat("!US_CBS_MIA_PREFIX!", " ", coast_quality),
                  "WTVJ" => concat("!US_NBC_MIA_PREFIX!", " ", coast_quality),
                  "WSVM" => concat("!US_FOX_MIA_PREFIX!", " ", coast_quality),
                  "WPLG" => concat("!US_ABC_MIA_PREFIX!", " ", coast_quality),
                  "WPBT" => concat("!US_PBS_MIA_PREFIX!", " ", coast_quality),
                  "WSFL" => concat("!US_CW_MIA_PREFIX!", " ", coast_quality),
                  "WLTV" => concat("!US_UNIVISION_MIA_PREFIX!", " ", coast_quality),

                  _ => concat(network, " ", station, " ", coast_quality),
                }

                @Group = concat("🇺🇸 > USA - ", network, " Locals")
              }
            }
        "#;
    let mapper = MapperScript::parse(script, None).expect("Parsing failed");
    println!("Program: {mapper:?}");
}

#[test]
fn test_mapper_format() {
    let dsl = r#"
            @Name = pad(1000, 10, 0);
            @Title = format("Channel {} is {}", 12, "live");
        "#;

    let mapper = MapperScript::parse(dsl, None).expect("Parsing failed");
    let mut channels: Vec<PlaylistItem> = vec![("D", "HD")]
        .into_iter()
        .map(|(name, quality)| PlaylistItem {
            header: PlaylistItemHeader { title: format!("Chanel {name} [{quality}]").into(), ..Default::default() },
        })
        .collect::<Vec<PlaylistItem>>();

    for pli in &mut channels {
        let mut accessor = ValueAccessor { pli, virtual_items: vec![], match_as_ascii: false, changed_fields: vec![] };
        mapper.eval(&mut accessor, None);
        assert_eq!(accessor.pli.header.title.as_ref(), "Channel 12 is live");
    }
}

#[test]
fn regex_identifier_ascii_normalization_preserves_map_selection() {
    let dsl = r#"
            source = @Name
            capture = source ~ "(Cinema)"
            @Group = map capture {
                "Cinema" => "matched",
                _ => "missed",
            }
        "#;
    let mapper = MapperScript::parse(dsl, None).expect("mapper should parse");
    let mut channel = PlaylistItem { header: PlaylistItemHeader { name: "Cinéma".intern(), ..Default::default() } };
    let mut accessor =
        ValueAccessor { pli: &mut channel, virtual_items: vec![], match_as_ascii: true, changed_fields: vec![] };

    mapper.eval(&mut accessor, None);

    assert_eq!(accessor.pli.header.group.as_ref(), "matched");
}

#[test]
fn regex_evaluation_keeps_all_groups_from_only_the_first_match() -> Result<(), Box<dyn std::error::Error>> {
    let regex = Regex::new(r"(?P<one>[A-Z])([A-Z])(?P<three>[A-Z])([A-Z])(?P<five>[A-Z])([A-Z])")?;

    let Named(values) = eval_regex("ABCDEFUVWXYZ", &regex, false) else {
        return Err("regex captures should produce a named result".into());
    };

    assert_eq!(
        values,
        [
            ("1", "A"),
            ("2", "B"),
            ("3", "C"),
            ("4", "D"),
            ("5", "E"),
            ("6", "F"),
            ("one", "A"),
            ("three", "C"),
            ("five", "E"),
        ]
        .map(|(key, value)| (key.to_string(), value.to_string()))
    );
    Ok(())
}

#[test]
fn regex_capture_groups_are_available_by_index_and_name() -> Result<(), Box<dyn std::error::Error>> {
    let dsl = r#"
            captures = @Name ~ "(?P<one>[A-Z])([A-Z])(?P<three>[A-Z])([A-Z])(?P<five>[A-Z])([A-Z])"
            @Title = concat(captures.1, captures.2, captures.3, captures.4, captures.5, captures.6)
            @Group = concat(captures.one, captures.three, captures.five)
        "#;
    let mapper = MapperScript::parse(dsl, None)?;
    let mut channel = PlaylistItem { header: PlaylistItemHeader { name: "ABCDEF".intern(), ..Default::default() } };
    let mut accessor =
        ValueAccessor { pli: &mut channel, virtual_items: vec![], match_as_ascii: false, changed_fields: vec![] };

    mapper.eval(&mut accessor, None);

    assert_eq!(accessor.pli.header.title.as_ref(), "ABCDEF");
    assert_eq!(accessor.pli.header.group.as_ref(), "ACE");
    Ok(())
}

#[test]
fn single_regex_capture_is_available_as_scalar_and_by_index() -> Result<(), Box<dyn std::error::Error>> {
    let dsl = r#"
            capture = @Name ~ "([A-Z]+)"
            @Group = map capture.1 {
                "ABC" => concat(capture, ":", capture.1),
                _ => "missed",
            }
        "#;
    let mapper = MapperScript::parse(dsl, None)?;
    let mut channel = PlaylistItem { header: PlaylistItemHeader { name: "ABC".intern(), ..Default::default() } };
    let mut accessor =
        ValueAccessor { pli: &mut channel, virtual_items: vec![], match_as_ascii: false, changed_fields: vec![] };

    mapper.eval(&mut accessor, None);

    assert_eq!(accessor.pli.header.group.as_ref(), "ABC:ABC");
    Ok(())
}

#[test]
fn read_only_fields_work_in_direct_regex_and_map_expressions() -> Result<(), Box<dyn std::error::Error>> {
    let dsl = r#"
            input_match = @Input ~ "(source)"
            type_match = @Type ~ "(live)"
            @Title = concat(@Input, ":", @Type, ":", input_match, ":", type_match)
            @Group = map @Input {
                "source" => "input-map",
                _ => "missed",
            }
            @Name = map @Type {
                "live" => concat(@Group, ":type-map"),
                _ => "missed",
            }
        "#;
    let mapper = MapperScript::parse(dsl, None)?;
    let mut channel = PlaylistItem {
        header: PlaylistItemHeader {
            input_name: "source".intern(),
            item_type: PlaylistItemType::Live,
            ..Default::default()
        },
    };
    let mut accessor =
        ValueAccessor { pli: &mut channel, virtual_items: vec![], match_as_ascii: false, changed_fields: vec![] };

    mapper.eval(&mut accessor, None);

    assert_eq!(accessor.pli.header.title.as_ref(), "source:live:source:live");
    assert_eq!(accessor.pli.header.group.as_ref(), "input-map");
    assert_eq!(accessor.pli.header.name.as_ref(), "input-map:type-map");
    Ok(())
}

#[test]
fn read_only_field_assignments_are_rejected() {
    for script in [r#"@Input = "changed""#, r#"@Type = "movie""#] {
        assert!(MapperScript::parse(script, None).is_err());
    }
    assert!(MapperScript::parse(r#"@Group = "changed""#, None).is_ok());
}

#[test]
fn template_lookup_keeps_last_duplicate_value() {
    let dsl = r#"@Group = template("label")"#;
    let templates = vec![
        PatternTemplate {
            name: "label".to_string(),
            value: TemplateValue::Single("first".to_string()),
            placeholder: "!label!".to_string(),
        },
        PatternTemplate {
            name: "label".to_string(),
            value: TemplateValue::Single("second".to_string()),
            placeholder: "!label!".to_string(),
        },
    ];
    let mapper = MapperScript::parse(dsl, Some(&templates)).expect("mapper should parse");
    let mut channel = PlaylistItem { header: PlaylistItemHeader::default() };
    let mut accessor =
        ValueAccessor { pli: &mut channel, virtual_items: vec![], match_as_ascii: false, changed_fields: vec![] };

    mapper.eval(&mut accessor, Some(&templates));

    assert_eq!(accessor.pli.header.group.as_ref(), "second");
}

#[test]
fn test_mapper_add_favourite() {
    use crate::model::PlaylistItemType;
    let dsl = r#"
            add_favourite("My Favs");
        "#;

    let mapper = MapperScript::parse(dsl, None).expect("Parsing failed");

    // Test with Video (should work)
    let mut video = PlaylistItem {
        header: PlaylistItemHeader {
            name: "Movie 1".to_string().into(),
            item_type: PlaylistItemType::Video,
            ..Default::default()
        },
    };
    let mut accessor =
        ValueAccessor { pli: &mut video, virtual_items: vec![], match_as_ascii: false, changed_fields: vec![] };
    mapper.eval(&mut accessor, None);
    assert_eq!(accessor.virtual_items.len(), 1);
    assert_eq!(&*accessor.virtual_items[0].1.header.group, "My Favs");

    // Test with SeriesInfo (should work)
    let mut series_info = PlaylistItem {
        header: PlaylistItemHeader {
            name: "Series 1".to_string().into(),
            item_type: PlaylistItemType::SeriesInfo,
            ..Default::default()
        },
    };
    let mut accessor =
        ValueAccessor { pli: &mut series_info, virtual_items: vec![], match_as_ascii: false, changed_fields: vec![] };
    mapper.eval(&mut accessor, None);
    assert_eq!(accessor.virtual_items.len(), 1);

    // Test with Series episode (should NOT work)
    let mut episode = PlaylistItem {
        header: PlaylistItemHeader {
            name: "Episode 1".to_string().into(),
            item_type: PlaylistItemType::Series,
            ..Default::default()
        },
    };
    let mut accessor =
        ValueAccessor { pli: &mut episode, virtual_items: vec![], match_as_ascii: false, changed_fields: vec![] };
    mapper.eval(&mut accessor, None);
    assert_eq!(accessor.virtual_items.len(), 0);
}

#[test]
fn test_mapper_split_loop() {
    use crate::model::PlaylistItemType;
    let dsl = r#"
            genres = split(@Genre, ",")
            print(genres)
            genres.for_each((_, gen) => {
                    add_favourite(concat("Genre - ", gen))
                })
        "#;

    let mapper = MapperScript::parse(dsl, None).expect("Parsing failed");

    // Test with Video (should work)
    let mut video = PlaylistItem {
        header: PlaylistItemHeader {
            name: "Movie 1".to_string().into(),
            item_type: PlaylistItemType::Video,
            additional_properties: Some(StreamProperties::Video(Box::new(VideoStreamProperties {
                details: Some(VideoStreamDetailProperties {
                    genre: Some("A, B, C".intern()),
                    ..VideoStreamDetailProperties::default()
                }),
                ..VideoStreamProperties::default()
            }))),
            ..Default::default()
        },
    };
    let mut accessor =
        ValueAccessor { pli: &mut video, virtual_items: vec![], match_as_ascii: false, changed_fields: vec![] };
    mapper.eval(&mut accessor, None);
    assert_eq!(accessor.virtual_items.len(), 3);
    assert_eq!(&*accessor.virtual_items[0].1.header.group, "Genre - A");
    assert_eq!(&*accessor.virtual_items[1].1.header.group, "Genre - B");
    assert_eq!(&*accessor.virtual_items[2].1.header.group, "Genre - C");

    // Test with SeriesInfo (should work)
    let mut series_info = PlaylistItem {
        header: PlaylistItemHeader {
            name: "Series 1".to_string().into(),
            item_type: PlaylistItemType::SeriesInfo,
            additional_properties: Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                genre: Some("A, B, C".intern()),
                ..SeriesStreamProperties::default()
            }))),
            ..Default::default()
        },
    };
    let mut accessor =
        ValueAccessor { pli: &mut series_info, virtual_items: vec![], match_as_ascii: false, changed_fields: vec![] };
    mapper.eval(&mut accessor, None);
    assert_eq!(accessor.virtual_items.len(), 3);
}

#[test]
fn for_each_restores_existing_loop_variables() {
    let expressions = vec![
        Expression::Identifier("value".to_string()),
        Expression::ForEachBlock {
            key: ForEachKey::Identifier("items".to_string()),
            expr: ForEachExpr {
                key_var: Some("key".to_string()),
                value_var: Some("value".to_string()),
                expression: ExprId(0),
            },
        },
    ];
    let mut ctx = MapperContext::new(&expressions, None);
    ctx.set_var("items", Named(vec![("first".to_string(), "current".to_string())]));
    ctx.set_var("value", Value("outer".to_string()));
    let mut item = PlaylistItem { header: PlaylistItemHeader::default() };
    let mut accessor =
        ValueAccessor { pli: &mut item, virtual_items: vec![], match_as_ascii: false, changed_fields: vec![] };

    ExprId(1).eval(&mut ctx, &mut accessor);

    assert!(matches!(ctx.get_var("value"), Value(value) if value == "outer"));
    assert!(matches!(ctx.get_var("key"), Undefined));
}

#[test]
fn eval_reports_changes_and_statement_failures() {
    let mapper = MapperScript::parse(
        r#"
                @Group = "News"
                capture = @Name ~ "(?P<known>BBC)"
                @Title = capture.missing
            "#,
        None,
    )
    .expect("script should parse");
    let mut item = PlaylistItem { header: PlaylistItemHeader { name: "BBC One".intern(), ..Default::default() } };
    let mut accessor =
        ValueAccessor { pli: &mut item, virtual_items: vec![], match_as_ascii: false, changed_fields: vec![] };

    let outcome = mapper.eval(&mut accessor, None);

    assert_eq!(outcome.changed_fields, ["Group"]);
    assert_eq!(outcome.emitted_items, 0);
    assert_eq!(outcome.diagnostics.len(), 1);
    assert_eq!(outcome.diagnostics[0].statement, 2);
    assert!(outcome.diagnostics[0].message.contains("has no field missing"));
}
