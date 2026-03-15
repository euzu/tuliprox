use std::sync::Arc;
use crate::model::{AppConfig, ProxyUserCredentials};
use crate::model::{ConfigTarget, XtreamTargetOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use shared::model::{
    PlaylistItem, PlaylistItemType, PlaylistItemTypeSet, XtreamMappingFlags, XtreamMappingFlagsSet,
    XtreamMappingOptions,
};
use shared::utils::{arc_str_serde, concat_path_leading_slash, deserialize_number_from_string_or_zero};
use enum_iterator::all;
use crate::model::XtreamTargetFlags;

#[derive(Deserialize, Default)]
pub struct XtreamCategory {
    #[serde(deserialize_with = "deserialize_number_from_string_or_zero", serialize_with = "shared::utils::serialize_number_as_string")]
    pub category_id: u32,
    #[serde(with = "arc_str_serde")]
    pub category_name: Arc<str>,
    //pub parent_id: i32,
    #[serde(default)]
    pub channels: Vec<PlaylistItem>,
}

impl XtreamCategory {
    pub fn add(&mut self, item: PlaylistItem) {
        self.channels.push(item);
    }
}


pub fn xtream_mapping_option_from_target_options(target: &ConfigTarget, target_output: &XtreamTargetOutput,
                                                 app_config: &AppConfig, user: &ProxyUserCredentials,
                                                 encrypt_secret: [u8; 16]) -> XtreamMappingOptions {

    let force_redirect = target.options.as_ref().and_then(|o| o.force_redirect);
    let mut reverse_item_types = PlaylistItemTypeSet::empty();

    for item_type in all::<PlaylistItemType>() {
        if user.proxy.is_reverse(item_type) && !force_redirect.as_ref().is_some_and(|o| o.has_cluster(item_type)) {
            reverse_item_types.insert(item_type);
        }
    }

    let mut flags = XtreamMappingFlagsSet::new();
    if target_output
        .flags
        .contains(XtreamTargetFlags::SkipLiveDirectSource)
    {
        flags.set(XtreamMappingFlags::SkipLiveDirectSource);
    }
    if target_output
        .flags
        .contains(XtreamTargetFlags::SkipVideoDirectSource)
    {
        flags.set(XtreamMappingFlags::SkipVideoDirectSource);
    }
    if target_output
        .flags
        .contains(XtreamTargetFlags::SkipSeriesDirectSource)
    {
        flags.set(XtreamMappingFlags::SkipSeriesDirectSource);
    }
    if app_config.is_reverse_proxy_resource_rewrite_enabled() {
        flags.set(XtreamMappingFlags::RewriteResourceUrl);
    }


    let base_url = if user.t_is_api_user {
        let config = app_config.config.load();
        let web_ui_path = config
            .web_ui
            .as_ref()
            .and_then(|w| w.path.as_ref())
            .map_or("", String::as_str);
        concat_path_leading_slash(web_ui_path, "api/v1/playlist/resource")
    } else {
        app_config.get_user_server_info(user).get_base_url()
    };


    XtreamMappingOptions {
        flags,
        force_redirect,
        reverse_item_types,
        username: user.username.clone(),
        password: user.password.clone(),
        base_url,
        web_ui_request: user.t_is_api_user,
        encrypt_secret
    }
}

pub fn normalize_release_date(document: &mut serde_json::Map<String, Value>) {
    // Find the first non-empty release date key
    let date_value = document.get("release_date")
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
