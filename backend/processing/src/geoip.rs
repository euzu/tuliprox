use log::{error, info};
use shared::{
    model::{default_geoip_url, InputFetchMethod},
    utils::Internable,
};
use std::{collections::HashMap, io::Cursor, sync::Arc};
use tuliprox_core::{
    model::{AppConfig, InputSource},
    utils::request::download_text_content,
};
use tuliprox_repository::{get_geoip_path, GeoIp};

#[derive(Debug, thiserror::Error)]
pub enum GeoIpUpdateError {
    #[error("GeoIp update is disabled")]
    Disabled,
    #[error("Failed to download geoip db: {0}")]
    DownloadFailed(String),
    #[error("Failed to process geoip db: {0}")]
    ProcessFailed(String),
    #[error("Unknown GeoIp processing error")]
    UnknownProcessing,
}

pub async fn update_geoip_db(
    app_config: &Arc<AppConfig>,
    http_client: &reqwest::Client,
    geoip_store: &arc_swap::ArcSwapOption<GeoIp>,
) -> Result<(), GeoIpUpdateError> {
    let config = app_config.config.load();
    if let Some(geoip) = config.reverse_proxy.as_ref().and_then(|r| r.geoip.as_ref()) {
        if geoip.enabled {
            let geoip_db_path = &*get_geoip_path(&config.storage_dir);
            let _file_lock = app_config.file_locks.write_lock(geoip_db_path).await;

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
            return match download_text_content(app_config, http_client, &input_source, None, None, false).await {
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
                            geoip_store.store(Some(Arc::new(geoip)));
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
