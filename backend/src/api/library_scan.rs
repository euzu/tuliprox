use crate::{
    api::model::{EventManager, EventMessage, UpdateGuardPermit},
    library::LibraryProcessor,
    model::{LibraryConfig, MetadataUpdateConfig},
};
use log::{error, info};
use shared::model::{LibraryScanSummary, LibraryScanSummaryStatus};
use std::sync::Arc;

pub(crate) fn spawn_library_scan(
    event_manager: Arc<EventManager>,
    lib_config: LibraryConfig,
    metadata_update_config: Option<MetadataUpdateConfig>,
    client: reqwest::Client,
    force_rescan: bool,
    message_prefix: &'static str,
    permit: UpdateGuardPermit,
) {
    let prefix = message_prefix.to_string();
    tokio::spawn(async move {
        let _permit = permit;
        let processor = LibraryProcessor::new(lib_config, metadata_update_config.as_ref(), client);
        match processor.scan(force_rescan).await {
            Ok(result) => {
                info!("{prefix}Library scan completed successfully");
                let response = LibraryScanSummary {
                    status: LibraryScanSummaryStatus::Success,
                    message: format!(
                        "{prefix}Scan completed: {} files scanned, {} added, {} updated, {} removed",
                        result.files_scanned, result.files_added, result.files_updated, result.files_removed
                    ),
                    result: Some(result),
                };
                let _ = event_manager.send_event(EventMessage::LibraryScanProgress(response));
            }
            Err(err) => {
                error!("{prefix}Library scan failed: {err}");
                let response = LibraryScanSummary {
                    status: LibraryScanSummaryStatus::Error,
                    message: format!("{prefix}Scan failed: {err}"),
                    result: None,
                };
                let _ = event_manager.send_event(EventMessage::LibraryScanProgress(response));
            }
        }
    });
}
