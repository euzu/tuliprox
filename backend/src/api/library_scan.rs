use crate::{
    api::model::{EventManager, EventMessage, UpdateGuardPermit},
    library::LibraryProcessor,
    model::{LibraryConfig, MetadataUpdateConfig},
};
use log::{error, info};
use shared::model::{LibraryScanSummary, LibraryScanSummaryStatus};
use std::sync::Arc;

pub(crate) struct LibraryScanTaskOptions {
    pub force_rescan: bool,
    pub message_prefix: &'static str,
    pub working_dir: String,
}

pub(crate) fn spawn_library_scan(
    event_manager: Arc<EventManager>,
    lib_config: LibraryConfig,
    metadata_update_config: Option<MetadataUpdateConfig>,
    client: reqwest::Client,
    options: LibraryScanTaskOptions,
    permit: UpdateGuardPermit,
) {
    let LibraryScanTaskOptions { force_rescan, message_prefix, working_dir } = options;
    let prefix = message_prefix.to_string();
    tokio::spawn(async move {
        let _permit = permit;
        let processor = LibraryProcessor::new(lib_config, metadata_update_config.as_ref(), client, &working_dir);
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
