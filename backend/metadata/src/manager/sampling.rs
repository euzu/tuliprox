use super::MetadataUpdateManager;
use log::{debug, info, warn};
use std::sync::Arc;
use tuliprox_core::model::ConfigInput;
use tuliprox_processing::container_extension_sampling::sample_container_extensions;

impl MetadataUpdateManager {
    /// Queue the opt-in container-extension sampling of one input after its playlist update.
    ///
    /// The run happens off the request path and waits for the update pause window, so sampling never
    /// overlaps a playlist update, the two would otherwise double the portal requests of the input.
    /// A second request for an input whose run is still queued or running is dropped: the budget is
    /// per update, and two concurrent runs would spend it twice.
    pub fn spawn_container_extension_sampling(self: &Arc<Self>, input_name: Arc<str>) {
        if self.is_shutdown() {
            return;
        }
        if self.sampling_inputs.insert(Arc::clone(&input_name), ()).is_some() {
            debug!("Container extension sampling for input '{input_name}' is already queued");
            return;
        }
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            manager.run_container_extension_sampling(input_name).await;
        });
    }

    async fn run_container_extension_sampling(&self, input_name: Arc<str>) {
        // The marker is released on every path, including a shutdown in the middle of the wait, so a
        // later update can sample again.
        let _marker = SamplingMarker { manager: self, input_name: Arc::clone(&input_name) };
        let ctx = self.ctx.lock().await.clone();
        let Some(ctx) = ctx else {
            debug!("Container extension sampling for input '{input_name}' skipped: no server context bound");
            return;
        };
        let cancel = self.cancel_token.load_full();
        if cancel.is_cancelled() {
            return;
        }
        let Some(input) = find_sampling_input(&ctx.app_config, &input_name) else {
            debug!("Container extension sampling for input '{input_name}' skipped: the input no longer exists");
            return;
        };
        let http_client = ctx.http_clients.default.load().as_ref().clone();
        match sample_container_extensions(
            &ctx.app_config,
            &http_client,
            &input,
            ctx.maintenance.as_ref(),
            &cancel,
            Some(&self.update_pause_gate),
        )
        .await
        {
            Ok(Some(report)) => info!(
                "Container extension sampling finished for input '{input_name}': {} resolved in {} calls",
                report.resolved, report.calls
            ),
            Ok(None) => debug!("Container extension sampling for input '{input_name}' had nothing to do"),
            Err(err) => warn!("Container extension sampling for input '{input_name}' failed: {err}"),
        }
    }
}

/// The input as the live configuration has it, so a reload between the update and the run applies.
fn find_sampling_input(
    app_config: &Arc<tuliprox_core::model::AppConfig>,
    input_name: &str,
) -> Option<Arc<ConfigInput>> {
    let sources = app_config.sources.load();
    sources.inputs.iter().find(|input| input.name.as_ref() == input_name).cloned()
}

struct SamplingMarker<'a> {
    manager: &'a MetadataUpdateManager,
    input_name: Arc<str>,
}

impl Drop for SamplingMarker<'_> {
    fn drop(&mut self) { self.manager.sampling_inputs.remove(&self.input_name); }
}
