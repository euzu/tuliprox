use super::{
    build_panel_api_probe_targets, build_user_api_account_info_input_source, resolve_batch_alias_path,
    source_doc_with_aliases, PanelApiProbeTarget,
};
use crate::{
    api::source_yml_patch::{apply_sources_yml_patches, resolve_provisioned_account_base_url, SourcesYmlPatch},
    model::{ConfigInput, ConfigProvider},
    repository::AliasExpDateSortOrder,
};
use shared::model::{ConfigInputAliasDto, ConfigProviderDto, ProviderUrlSelectionPolicy};
use std::sync::Arc;

pub(in crate::api::panel_api::tests) fn source_alias(name: &str, exp_date: Option<i64>) -> ConfigInputAliasDto {
    ConfigInputAliasDto {
        id: 0,
        name: Arc::from(name),
        url: "provider://tivione".to_string(),
        username: Some(format!("{name}-user")),
        password: Some(format!("{name}-pass")),
        priority: 0,
        max_connections: 1,
        exp_date,
        enabled: true,
        stalker: None,
    }
}

#[test]
fn batch_alias_storage_never_falls_back_to_source_yml() {
    assert!(resolve_batch_alias_path(None).expect("non-batch input").is_none());
    assert!(resolve_batch_alias_path(Some("provider://not-a-csv")).is_err());
}

#[test]
fn sources_yml_add_alias_appends_without_overwriting_existing_alias() {
    let mut doc = source_doc_with_aliases(vec![source_alias("cdn-dev-old", Some(10))]);
    doc.inputs[0].aliases.as_mut().expect("aliases")[0].id = 2;

    let changed = apply_sources_yml_patches(
        &mut doc,
        &[
            SourcesYmlPatch::AddAlias {
                input_name: Arc::from("cdn-dev"),
                alias_name: Arc::from("cdn-dev-new"),
                base_url: "provider://tivione".to_string(),
                username: "new-user".to_string(),
                password: "new-pass".to_string(),
                exp_date: Some(20),
            },
            SourcesYmlPatch::SortAliases {
                input_name: Arc::from("cdn-dev"),
                order: AliasExpDateSortOrder::NewestFirst,
            },
        ],
    )
    .expect("patches apply");

    assert!(changed);
    let aliases = doc.inputs[0].aliases.as_ref().expect("aliases");
    assert_eq!(aliases.len(), 2);
    assert_eq!(aliases[0].name.as_ref(), "cdn-dev-new");
    assert_eq!(aliases[0].id, 3);
    assert_eq!(aliases[1].name.as_ref(), "cdn-dev-old");
    assert_eq!(aliases[1].id, 2);
}

#[test]
fn sources_yml_sort_aliases_oldest_first_is_available_for_maintenance_paths() {
    let mut doc = source_doc_with_aliases(vec![
        source_alias("cdn-dev-newest", Some(30)),
        source_alias("cdn-dev-oldest", Some(10)),
        source_alias("cdn-dev-missing-exp", None),
    ]);

    let changed = apply_sources_yml_patches(
        &mut doc,
        &[SourcesYmlPatch::SortAliases { input_name: Arc::from("cdn-dev"), order: AliasExpDateSortOrder::OldestFirst }],
    )
    .expect("patches apply");

    assert!(changed);
    let aliases = doc.inputs[0].aliases.as_ref().expect("aliases");
    assert_eq!(aliases[0].name.as_ref(), "cdn-dev-oldest");
    assert_eq!(aliases[1].name.as_ref(), "cdn-dev-newest");
    assert_eq!(aliases[2].name.as_ref(), "cdn-dev-missing-exp");
}

#[test]
fn sources_yml_update_exp_date_keeps_legacy_root_refresh_semantics() {
    let mut doc = source_doc_with_aliases(Vec::new());
    doc.inputs[0].exp_date = Some(20);
    doc.inputs[0].enabled = false;
    doc.inputs[0].max_connections = 0;

    let changed = apply_sources_yml_patches(
        &mut doc,
        &[SourcesYmlPatch::UpdatePanelAccountExpiry {
            input_name: Arc::from("cdn-dev"),
            account_name: Arc::from("cdn-dev"),
            exp_date: 20,
        }],
    )
    .expect("patches apply");

    assert!(changed);
    assert_eq!(doc.inputs[0].exp_date, Some(20));
    assert!(doc.inputs[0].enabled);
    assert_eq!(doc.inputs[0].max_connections, 1);
}

#[test]
fn sources_yml_update_exp_date_keeps_legacy_alias_refresh_semantics() {
    let mut doc = source_doc_with_aliases(vec![source_alias("cdn-dev-old", Some(20))]);
    doc.inputs[0].aliases.as_mut().expect("aliases")[0].max_connections = 0;

    let changed = apply_sources_yml_patches(
        &mut doc,
        &[SourcesYmlPatch::UpdatePanelAccountExpiry {
            input_name: Arc::from("cdn-dev"),
            account_name: Arc::from("cdn-dev-old"),
            exp_date: 20,
        }],
    )
    .expect("patches apply");

    assert!(changed);
    let alias = &doc.inputs[0].aliases.as_ref().expect("aliases")[0];
    assert_eq!(alias.exp_date, Some(20));
    assert_eq!(alias.max_connections, 1);
}

#[test]
fn sources_yml_update_root_credentials_updates_root_directly() {
    let mut doc = source_doc_with_aliases(Vec::new());

    let changed = apply_sources_yml_patches(
        &mut doc,
        &[SourcesYmlPatch::UpdateRootCredentials {
            input_name: Arc::from("cdn-dev"),
            username: "new-root".to_string(),
            password: "new-pass".to_string(),
            exp_date: Some(42),
        }],
    )
    .expect("patches apply");

    assert!(changed);
    assert_eq!(doc.inputs[0].username.as_deref(), Some("new-root"));
    assert_eq!(doc.inputs[0].password.as_deref(), Some("new-pass"));
    assert_eq!(doc.inputs[0].exp_date, Some(42));
    assert!(doc.inputs[0].enabled);
    assert_eq!(doc.inputs[0].max_connections, 1);
}

#[test]
fn resolve_base_url_keeps_provider_scheme_for_provisioned_accounts() {
    let result = resolve_provisioned_account_base_url(
        "provider://demo-provider/live",
        Some("http://panel.example.com:8080/get.php?username=new&password=new"),
        "new",
        "secret",
    );

    assert_eq!(result, "provider://demo-provider/live");
}

#[test]
fn resolve_base_url_uses_panel_response_origin_for_http_inputs() {
    let result = resolve_provisioned_account_base_url(
        "http://input.example.org/path?x=1",
        Some("http://panel.example.com:8080/get.php?username=new&password=new"),
        "new",
        "secret",
    );

    assert_eq!(result, "http://panel.example.com:8080");
}

#[test]
fn resolve_base_url_avoids_null_origin_for_non_special_schemes() {
    let result = resolve_provisioned_account_base_url(
        "http://input.example.org/path?x=1",
        Some("custom-scheme://panel.example.com/path?username=new&password=new"),
        "new",
        "secret",
    );

    assert_eq!(result, "custom-scheme://panel.example.com/path?username=new&password=new");
}

#[test]
fn panel_api_probe_targets_preserve_provider_failover_context() {
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "tivione".into(),
        urls: vec!["http://line-a.example.test".into(), "http://line-b.example.test".into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
        dns: None,
    }));
    let input = ConfigInput {
        name: Arc::from("cdn-dev"),
        url: "provider://tivione".to_string(),
        provider_configs: Some(vec![Arc::clone(&provider)]),
        ..ConfigInput::default()
    };

    let targets = build_panel_api_probe_targets(&input, "probe-user", "probe-pass");

    assert_eq!(targets.len(), 4);
    let PanelApiProbeTarget::PlayerApi { action, input_source } = &targets[0];
    assert_eq!(*action, "client_info");
    assert_eq!(input_source.provider.as_ref().expect("provider context should be preserved").name.as_ref(), "tivione");
    assert!(input_source.url.starts_with("provider://tivione/player_api.php?"));
    assert!(input_source.url.contains("username=probe-user"));
    assert!(input_source.url.contains("password=probe-pass"));
    assert!(input_source.url.contains("action=client_info"));
}

#[test]
fn panel_api_probe_targets_keep_plain_http_without_provider_context() {
    let input = ConfigInput {
        name: Arc::from("plain"),
        url: "http://origin.example.test/some/path?ignored=1".to_string(),
        ..ConfigInput::default()
    };

    let targets = build_panel_api_probe_targets(&input, "probe-user", "probe-pass");

    assert_eq!(targets.len(), 4);
    let PanelApiProbeTarget::PlayerApi { action, input_source } = &targets[0];
    assert_eq!(*action, "client_info");
    assert!(input_source.provider.is_none());
    assert!(input_source.url.starts_with("http://origin.example.test/player_api.php?"));
    assert!(input_source.url.contains("username=probe-user"));
    assert!(input_source.url.contains("password=probe-pass"));
    assert!(input_source.url.contains("action=client_info"));
}

#[test]
fn user_api_account_info_preserves_provider_failover_context() {
    let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
        name: "tivione".into(),
        urls: vec!["http://line-a.example.test".into(), "http://line-b.example.test".into()],
        provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
        dns: None,
    }));
    let input = ConfigInput {
        name: Arc::from("cdn-dev"),
        url: "provider://tivione".to_string(),
        provider_configs: Some(vec![Arc::clone(&provider)]),
        ..ConfigInput::default()
    };

    let input_source = build_user_api_account_info_input_source(&input, "root-user", "root-pass")
        .expect("expected provider account_info input source");

    assert_eq!(input_source.provider.as_ref().expect("provider context should be preserved").name.as_ref(), "tivione");
    assert!(input_source.url.starts_with("provider://tivione/player_api.php?"));
    assert!(input_source.url.contains("username=root-user"));
    assert!(input_source.url.contains("password=root-pass"));
    assert!(input_source.url.contains("action=account_info"));
}

#[test]
fn user_api_account_info_keeps_plain_http_without_provider_context() {
    let input = ConfigInput {
        name: Arc::from("plain"),
        url: "http://origin.example.test/some/path?ignored=1".to_string(),
        ..ConfigInput::default()
    };

    let input_source = build_user_api_account_info_input_source(&input, "root-user", "root-pass")
        .expect("expected plain account_info input source");

    assert!(input_source.provider.is_none());
    assert!(input_source.url.starts_with("http://origin.example.test/player_api.php?"));
    assert!(input_source.url.contains("username=root-user"));
    assert!(input_source.url.contains("password=root-pass"));
    assert!(input_source.url.contains("action=account_info"));
}
