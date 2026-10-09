use super::*;

#[test]
fn disabled_resource_rewrite_proxies_private_xtream_images() {
    let mut options = sample_options();
    options.web_ui_request = false;
    options.base_url = "https://proxy.example".to_string();
    options.flags = XtreamMappingFlagsSet::new();
    options.resource_host_policies.insert("192.168.1.20".to_string(), ResourceOutputPolicy::Proxy);
    options.resource_host_policies.insert("8.8.8.8".to_string(), ResourceOutputPolicy::Direct);
    options.resource_host_policies.insert("127.0.0.1".to_string(), ResourceOutputPolicy::Blocked);

    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Live,
            PlaylistItemType::Live,
            VirtualId::new(1),
            "http://192.168.1.20/logo.png",
            "logo",
        ),
        "https://proxy.example/resource/live/user/pass/1/logo"
    );
    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Live,
            PlaylistItemType::Live,
            VirtualId::new(1),
            "http://8.8.8.8/logo.png",
            "logo",
        ),
        "http://8.8.8.8/logo.png"
    );
    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Live,
            PlaylistItemType::Live,
            VirtualId::new(1),
            "http://127.0.0.1/logo.png",
            "logo",
        ),
        ""
    );
    assert_eq!(
        options.get_bd_path_resource_url(
            XtreamCluster::Live,
            PlaylistItemType::Live,
            VirtualId::new(1),
            "http://192.168.1.20/backdrop.png",
            "",
            0,
        ),
        "https://proxy.example/resource/live/user/pass/1/backdrop_path_0"
    );

    options.resource_proxy_item_types = PlaylistItemTypeSet::empty();
    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Live,
            PlaylistItemType::Live,
            VirtualId::new(1),
            "http://192.168.1.20/logo.png",
            "logo",
        ),
        "https://proxy.example/resource/live/user/pass/1/logo"
    );
}

#[test]
fn get_resource_url_keeps_internal_paths_for_web_ui_requests() {
    let options = sample_options();

    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Series,
            PlaylistItemType::Series,
            VirtualId::new(1),
            "/api/v1/library/thumbnail/abc",
            "logo",
        ),
        "/api/v1/library/thumbnail/abc",
    );
}

#[test]
fn get_bd_path_resource_url_keeps_internal_thumbnail_paths_for_web_ui_requests() {
    let options = sample_options();

    assert_eq!(
        options.get_bd_path_resource_url(
            XtreamCluster::Series,
            PlaylistItemType::Series,
            VirtualId::new(1),
            "/api/v1/library/thumbnail/backdrop",
            "backdrop_",
            0,
        ),
        "/api/v1/library/thumbnail/backdrop",
    );
}

#[test]
fn get_resource_url_absolutizes_internal_thumbnail_paths_for_xtream_clients() {
    let mut options = sample_options();
    options.web_ui_request = false;
    options.base_url = "http://proxy.example/base".to_string();
    options.reverse_item_types = PlaylistItemTypeSet::empty();
    options.resource_proxy_item_types = PlaylistItemTypeSet::empty();

    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Series,
            PlaylistItemType::LocalSeries,
            VirtualId::new(1),
            "/api/v1/library/thumbnail/abc",
            "logo",
        ),
        "http://proxy.example/base/api/v1/library/thumbnail/abc",
    );
}

#[test]
fn get_bd_path_resource_url_absolutizes_internal_thumbnail_paths_for_xtream_clients() {
    let mut options = sample_options();
    options.web_ui_request = false;
    options.base_url = "http://proxy.example/base".to_string();
    options.reverse_item_types = PlaylistItemTypeSet::empty();
    options.resource_proxy_item_types = PlaylistItemTypeSet::empty();

    assert_eq!(
        options.get_bd_path_resource_url(
            XtreamCluster::Series,
            PlaylistItemType::LocalSeries,
            VirtualId::new(1),
            "/api/v1/library/thumbnail/backdrop",
            "backdrop_",
            0,
        ),
        "http://proxy.example/base/api/v1/library/thumbnail/backdrop",
    );
}

#[test]
fn get_resource_url_obfuscates_protocol_relative_urls_for_web_ui_requests() {
    let options = sample_options();
    let resource_url = "//cdn.example.com/poster.jpg";

    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Series,
            PlaylistItemType::Series,
            VirtualId::new(1),
            resource_url,
            "logo",
        ),
        concat_path(&options.base_url, &seal_web_ui_resource_url(&options.encrypt_secret, resource_url)),
    );
}

#[test]
fn get_resource_url_rewrites_redirect_resources_for_xtream_clients() {
    let mut options = sample_options();
    options.web_ui_request = false;
    options.base_url = "http://proxy.example/iptv".to_string();
    options.reverse_item_types = PlaylistItemTypeSet::empty();
    options.resource_proxy_item_types = PlaylistItemTypeSet::from_item(PlaylistItemType::Live);

    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Live,
            PlaylistItemType::Live,
            VirtualId::new(2017),
            "https://provider.example/logo.png",
            "logo",
        ),
        "http://proxy.example/iptv/resource/live/user/pass/2017/logo",
    );
}

#[test]
fn get_resource_url_does_not_bypass_untrusted_root_relative_paths_for_web_ui_requests() {
    let options = sample_options();
    let resource_url = "/provider-controlled/poster.jpg";

    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Series,
            PlaylistItemType::Series,
            VirtualId::new(1),
            resource_url,
            "logo",
        ),
        concat_path(&options.base_url, &seal_web_ui_resource_url(&options.encrypt_secret, resource_url)),
    );
}

#[test]
fn get_resource_url_does_not_trust_absolute_urls_containing_internal_thumbnail_path() {
    let options = sample_options();
    let resource_url = "https://provider.example/api/v1/library/thumbnail/abc";

    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Series,
            PlaylistItemType::Series,
            VirtualId::new(1),
            resource_url,
            "logo",
        ),
        concat_path(&options.base_url, &seal_web_ui_resource_url(&options.encrypt_secret, resource_url)),
    );
}
