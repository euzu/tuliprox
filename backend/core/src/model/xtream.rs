use crate::model::{AppConfig, ConfigTarget, ProxyUserCredentials, XtreamTargetFlags, XtreamTargetOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use shared::{
    error::TuliproxError,
    model::{
        PlaylistItem, PlaylistItemType, PlaylistItemTypeSet, XtreamMappingFlags, XtreamMappingFlagsSet,
        XtreamMappingOptions,
    },
    utils::{arc_str_serde, concat_path_leading_slash, deserialize_number_from_string_or_zero},
};
use std::sync::Arc;
use strum::IntoEnumIterator;

#[derive(Deserialize, Default)]
pub struct XtreamCategory {
    #[serde(
        deserialize_with = "deserialize_number_from_string_or_zero",
        serialize_with = "shared::utils::serialize_number_as_string"
    )]
    pub category_id: u32,
    #[serde(with = "arc_str_serde")]
    pub category_name: Arc<str>,
    //pub parent_id: i32,
    #[serde(default)]
    pub channels: Vec<PlaylistItem>,
}

impl XtreamCategory {
    pub fn add(&mut self, item: PlaylistItem) { self.channels.push(item); }
}

pub fn xtream_mapping_option_from_target_options(
    target: &ConfigTarget,
    target_output: &XtreamTargetOutput,
    app_config: &AppConfig,
    user: &ProxyUserCredentials,
    encrypt_secret: [u8; 16],
) -> Result<XtreamMappingOptions, TuliproxError> {
    let force_redirect = target.options.as_ref().and_then(|o| o.force_redirect);
    let mut reverse_item_types = PlaylistItemTypeSet::empty();
    let mut resource_proxy_item_types = PlaylistItemTypeSet::empty();

    for item_type in PlaylistItemType::iter() {
        if user.proxy.is_reverse(item_type) && !force_redirect.as_ref().is_some_and(|o| o.has_cluster(item_type)) {
            reverse_item_types.insert(item_type);
        }
        if user.proxy.is_redirect(item_type)
            || user.proxy.is_reverse(item_type)
            || force_redirect.as_ref().is_some_and(|o| o.has_cluster(item_type))
        {
            resource_proxy_item_types.insert(item_type);
        }
    }

    let mut flags = XtreamMappingFlagsSet::new();
    if target_output.flags.contains(XtreamTargetFlags::SkipLiveDirectSource) {
        flags.set(XtreamMappingFlags::SkipLiveDirectSource);
    }
    if target_output.flags.contains(XtreamTargetFlags::SkipVideoDirectSource) {
        flags.set(XtreamMappingFlags::SkipVideoDirectSource);
    }
    if target_output.flags.contains(XtreamTargetFlags::SkipSeriesDirectSource) {
        flags.set(XtreamMappingFlags::SkipSeriesDirectSource);
    }
    if app_config.is_reverse_proxy_resource_rewrite_enabled() {
        flags.set(XtreamMappingFlags::RewriteResourceUrl);
    }

    let base_url = if user.t_is_api_user {
        let config = app_config.config.load();
        let web_ui_path = config.web_ui.as_ref().and_then(|w| w.path.as_ref()).map_or("", String::as_str);
        concat_path_leading_slash(web_ui_path, "api/v1/playlist/resource")
    } else {
        match app_config.get_user_server_info(user) {
            Some(server_info) => server_info.get_base_url(),
            None => {
                return Err(TuliproxError::ApiXtream(format!("No server info configured for user '{}'", user.username)))
            }
        }
    };

    Ok(XtreamMappingOptions {
        flags,
        force_redirect,
        reverse_item_types,
        resource_proxy_item_types,
        username: user.username.clone(),
        password: user.password.clone(),
        base_url,
        web_ui_request: user.t_is_api_user,
        encrypt_secret,
    })
}

pub fn normalize_release_date(document: &mut serde_json::Map<String, Value>) {
    // Find the first non-empty release date key
    let date_value = document
        .get("release_date")
        .or_else(|| document.get("releaseDate"))
        .or_else(|| document.get("releasedate"))
        .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
        .cloned();

    // Remove unused keys (optional)
    document.remove("releaseDate");
    document.remove("releasedate");

    // Insert the normalized release date or null if not found
    if let Some(date) = date_value {
        document.insert("release_date".to_string(), date);
    } else {
        document.insert("release_date".to_string(), Value::Null);
    }
}

#[derive(Deserialize, Serialize, Clone)]
pub struct PlaylistXtreamCategory {
    #[serde(alias = "category_id")]
    pub id: u32,
    #[serde(alias = "category_name")]
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::xtream_mapping_option_from_target_options;
    use crate::{
        model::{
            AppConfig, Config, ConfigInput, ConfigTarget, MediaToolCapabilities, ProxyUserCredentials, SourcesConfig,
            TargetOutput, XtreamTargetFlagsSet, XtreamTargetOutput,
        },
        utils::FileLockManager,
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::{
        foundation::Filter,
        model::{ConfigPaths, InputFetchMethod, InputType, PlaylistItemType, ProcessingOrder},
        utils::Internable,
    };
    use std::{collections::HashMap, sync::Arc};

    fn create_test_app_config() -> AppConfig {
        let input = Arc::new(ConfigInput {
            id: 1,
            name: "provider_1".intern(),
            input_type: InputType::Xtream,
            headers: HashMap::default(),
            url: "http://provider-1.example".to_string(),
            enabled: true,
            priority: 0,
            max_connections: 1,
            method: InputFetchMethod::default(),
            aliases: None,
            ..ConfigInput::default()
        });
        let sources = SourcesConfig { inputs: vec![input], ..SourcesConfig::default() };

        AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(sources)),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        }
    }

    fn create_test_target() -> (ConfigTarget, XtreamTargetOutput) {
        let xtream_output = XtreamTargetOutput { flags: XtreamTargetFlagsSet::default(), trakt: None, filter: None };
        let target = ConfigTarget {
            id: 1,
            enabled: true,
            name: "xtream-target".to_string(),
            options: None,
            sort: None,
            filter: Filter::default(),
            output: vec![TargetOutput::Xtream(xtream_output.clone())],
            rename: None,
            mapping_ids: None,
            mapping: Arc::new(ArcSwapOption::default()),
            favourites: None,
            processing_order: ProcessingOrder::Frm,
            watch: None,
            use_memory_cache: false,
        };
        (target, xtream_output)
    }

    #[test]
    fn xtream_mapping_options_error_when_server_info_missing_for_non_api_user() {
        let app_config = create_test_app_config();
        let (target, xtream_output) = create_test_target();
        let mut user = ProxyUserCredentials::default();
        user.username = "missing-server".to_string();
        user.t_is_api_user = false;

        let result = xtream_mapping_option_from_target_options(&target, &xtream_output, &app_config, &user, [0; 16]);

        assert!(result.is_err(), "missing server info must not degrade to an empty base_url");
        let err = result.err().unwrap_or_else(|| unreachable!());
        assert_eq!(
            err.kind(),
            shared::error::ErrorKind::ApiXtream,
            "missing server info should surface as ApiXtream error"
        );
        assert!(
            err.to_string().contains("No server info configured"),
            "error should explicitly mention missing server info: {err}"
        );
    }

    #[test]
    fn xtream_mapping_options_rewrite_resources_for_redirect_users() {
        let app_config = create_test_app_config();
        let (target, xtream_output) = create_test_target();
        let mut user = ProxyUserCredentials::default();
        user.username = "viewer".to_string();
        user.password = "secret".to_string();
        user.t_is_api_user = true;

        let options = xtream_mapping_option_from_target_options(&target, &xtream_output, &app_config, &user, [0; 16])
            .expect("mapping options");

        assert!(!options.reverse_item_types.is_set(PlaylistItemType::Live));
        assert!(options.resource_proxy_item_types.is_set(PlaylistItemType::Live));
    }
}
