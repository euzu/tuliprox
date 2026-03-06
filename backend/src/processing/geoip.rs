use crate::{
    api::model::AppState,
    model::InputSource,
    repository::get_geoip_path,
    utils::{request::download_text_content, GeoIp},
};
use log::{error, info};
use shared::{
    model::{default_geoip_url, InputFetchMethod},
    utils::Internable,
};
use std::{collections::HashMap, io::Cursor, sync::Arc};

#[derive(Debug)]
pub(crate) enum GeoIpUpdateError {
    Disabled,
    DownloadFailed(String),
    ProcessFailed(String),
    UnknownProcessing,
}

impl std::fmt::Display for GeoIpUpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "GeoIp update is disabled"),
            Self::DownloadFailed(err) => write!(f, "Failed to download geoip db: {err}"),
            Self::ProcessFailed(err) => write!(f, "Failed to process geoip db: {err}"),
            Self::UnknownProcessing => write!(f, "Unknown GeoIp processing error"),
        }
    }
}

impl std::error::Error for GeoIpUpdateError {}

pub(crate) async fn update_geoip_db(app_state: &Arc<AppState>) -> Result<(), GeoIpUpdateError> {
    let config = app_state.app_config.config.load();
    if let Some(geoip) = config.reverse_proxy.as_ref().and_then(|r| r.geoip.as_ref()) {
        if geoip.enabled {
            let geoip_db_path = &*get_geoip_path(&config.storage_dir);
            let _file_lock = app_state.app_config.file_locks.write_lock(geoip_db_path).await;

            let url = if geoip.url.trim().is_empty() { default_geoip_url() } else { geoip.url.clone() };
            let input_source = InputSource {
                name: "GeoIP".intern(),
                url,
                provider: None,
                username: None,
                password: None,
                method: InputFetchMethod::GET,
                headers: HashMap::default(),
            };
            return match download_text_content(
                &app_state.app_config,
                &app_state.http_client.load(),
                &input_source,
                None,
                None,
                false,
            )
            .await
            {
                Ok((content, _)) => {
                    let reader = Cursor::new(content);
                    let mut geoip = GeoIp::new();
                    let result = {
                        match geoip.import_ipv4_from_csv(reader, geoip_db_path) {
                            Ok(size) => (Some(size), None),
                            Err(err) => (None, Some(err)),
                        }
                    };

                    return match result {
                        (Some(_), None) => {
                            info!("GeoIp db updated");
                            app_state.geoip.store(Some(Arc::new(geoip)));
                            Ok(())
                        }
                        (None, Some(err)) => {
                            let error = GeoIpUpdateError::ProcessFailed(err.to_string());
                            error!("{error}");
                            Err(error)
                        }
                        _ => Err(GeoIpUpdateError::UnknownProcessing),
                    };
                }
                Err(err) => {
                    let error = GeoIpUpdateError::DownloadFailed(err.to_string());
                    error!("{error}");
                    Err(error)
                }
            };
        }
    }
    Err(GeoIpUpdateError::Disabled)
}
