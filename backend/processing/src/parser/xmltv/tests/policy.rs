use super::*;

#[test]
/// Tests normalization of a channel name using the default smart match configuration.
///
/// # Examples
///
/// ```
/// parse_normalize().unwrap();
/// ```
fn parse_normalize() {
    let epg_normalize_dto = EpgSmartMatchConfigDto { ..Default::default() };
    let epg_normalize = EpgSmartMatchConfig::from(epg_normalize_dto);
    let normalized = normalize_channel_name("Love Nature", &epg_normalize);
    assert_eq!(normalized, "lovenature".to_string());
}

#[test]
fn normalization_keeps_country_suffixes_consistent() {
    let mut dto = EpgSmartMatchConfigDto {
        enabled: true,
        name_prefix: EpgNamePrefix::Suffix(".".to_string()),
        ..Default::default()
    };
    dto.prepare().expect("valid smart-match config");
    let config = EpgSmartMatchConfig::from(dto);

    assert_eq!(normalize_channel_name("FR: TF1 FHD", &config), "tf1.fr");
    assert_eq!(normalize_channel_name("TF1.fr", &config), "tf1.fr");
}

#[test]
fn normalization_strips_quality_markers_without_corrupting_names() {
    let mut dto = EpgSmartMatchConfigDto { enabled: true, ..Default::default() };
    dto.prepare().expect("valid smart-match config");
    let config = EpgSmartMatchConfig::from(dto);

    assert_eq!(normalize_channel_name("RMC Story H265 50FPS", &config), "rmcstory");
    assert_eq!(normalize_channel_name("Ashdod TV HD", &config), "ashdodtv");
}

#[test]
fn marker_stripping_skips_non_utf8_boundaries() {
    assert_eq!(super::super::strip_markers("éclair", &["x".to_string()]), "éclair");
}

#[ignore = "requires a local XMLTV fixture under /tmp"]
#[test]
fn parse_test() {
    let run_test = async move || {
        //let file_path = PathBuf::from("/tmp/epg.xml.gz");
        let file_path = PathBuf::from("/tmp/invalid_epg.xml");

        if file_path.exists() {
            let tv_guide = TVGuide::new(vec![xmltv_source(file_path, 0, false)]);

            let mut id_cache = EpgIdCache::new(None);
            id_cache.insert_channel_epg_id("342");
            //id_cache.collect_epg_id(fp);

            let channel_ids = HashSet::from([342u32.intern()]);
            match tv_guide.filter(&mut id_cache).await {
                None => panic!("No epg filtered"),
                Some(epgs) => {
                    for epg in epgs {
                        assert_eq!(epg.children.len(), channel_ids.len() * 2, "Epg size does not match");
                    }
                }
            }
        }
    };
    tokio::runtime::Runtime::new().unwrap().block_on(run_test());
}

#[test]
/// Tests normalization of channel names with various prefixes, suffixes, and special characters using a configured `EpgSmartMatchConfig`.
///
/// # Examples
///
/// ```
/// normalize();
/// // This will assert that various channel names are normalized as expected.
/// ```
fn normalize() {
    let mut epg_smart_cfg_dto = EpgSmartMatchConfigDto {
        enabled: true,
        name_prefix: EpgNamePrefix::Suffix(".".to_string()),
        ..Default::default()
    };
    let _ = epg_smart_cfg_dto.prepare();
    let epg_smart_cfg = EpgSmartMatchConfig::from(epg_smart_cfg_dto);
    println!("{epg_smart_cfg:?}");
    assert_eq!("supersport6.ru", normalize_channel_name("RU: SUPERSPORT 6 ᴿᴬᵂ", &epg_smart_cfg));
    assert_eq!("odisea.sat", normalize_channel_name("SAT: ODISEA ᴿᴬᵂ", &epg_smart_cfg));
    assert_eq!("odisea.4k", normalize_channel_name("4K: ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_smart_cfg));
    assert_eq!("odisea", normalize_channel_name("ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_smart_cfg));
    assert_eq!("odisea.bu", normalize_channel_name("BU | ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_smart_cfg));
    assert_eq!("odisea.bg", normalize_channel_name("BG | ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_smart_cfg));
}

#[test]
/// Demonstrates phonetic encoding (Metaphone) of normalized channel names with various prefixes and suffixes.
///
/// This test prints the Metaphone-encoded representations of several normalized channel names using a configured `EpgSmartMatchConfig`.
///
/// # Examples
///
/// ```
/// test_metaphone();
/// // Output will show the Metaphone encodings for different channel name variants.
/// ```
fn test_metaphone() {
    let metaphone = Metaphone::default();
    let mut epg_smart_cfg_dto = EpgSmartMatchConfigDto {
        enabled: true,
        name_prefix: EpgNamePrefix::Suffix(".".to_string()),
        ..Default::default()
    };
    let _ = epg_smart_cfg_dto.prepare();
    let epg_smart_cfg = EpgSmartMatchConfig::from(epg_smart_cfg_dto);
    println!("{epg_smart_cfg:?}");
    // assert_eq!("supersport6.ru", metaphone.encode(&normalize_channel_name("RU: SUPERSPORT 6 ᴿᴬᵂ", &epg_normalize_cfg)));
    // assert_eq!("odisea.sat", metaphone.encode(&normalize_channel_name("SAT: ODISEA ᴿᴬᵂ", &epg_normalize_cfg)));
    // assert_eq!("odisea", metaphone.encode(&normalize_channel_name("4K: ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_normalize_cfg)));
    // assert_eq!("odisea", metaphone.encode(&normalize_channel_name("ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_normalize_cfg)));
    // assert_eq!("odisea.bu", metaphone.encode(&normalize_channel_name("BU | ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_normalize_cfg)));
    // assert_eq!("odisea.bg", metaphone.encode(&normalize_channel_name("BG | ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_normalize_cfg)));

    println!("{}", metaphone.encode(&normalize_channel_name("RU: SUPERSPORT 6 ᴿᴬᵂ", &epg_smart_cfg)));
    println!("{}", metaphone.encode(&normalize_channel_name("SAT: ODISEA ᴿᴬᵂ", &epg_smart_cfg)));
    println!("{}", metaphone.encode(&normalize_channel_name("4K: ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_smart_cfg)));
    println!("{}", metaphone.encode(&normalize_channel_name("ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_smart_cfg)));
    println!("{}", metaphone.encode(&normalize_channel_name("BU | ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_smart_cfg)));
    println!("{}", metaphone.encode(&normalize_channel_name("BG | ODISEA ᵁᴴᴰ ³⁸⁴⁰ᴾ", &epg_smart_cfg)));
}
