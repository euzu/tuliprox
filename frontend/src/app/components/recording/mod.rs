mod recording_edit_view;
mod recording_form;
mod recording_library_view;
mod recording_rule_form;
mod recording_rules_view;
mod recording_task_edit_form;
use crate::{hooks::Services, i18n::YewI18n, services::RecordingService};
#[allow(unused_imports)]
pub use recording_edit_view::*;
pub use recording_form::*;
pub use recording_library_view::*;
pub use recording_rules_view::*;
use std::rc::Rc;

/// Shared DVR preflight for entry points that open a recording form.
/// Reports the actionable error (DVR disabled or unreachable) as a toast
/// and returns whether the caller may proceed. The backend gates the
/// same way at submission; this surfaces the message before the user
/// fills in a form that could never submit.
pub async fn ensure_recording_available(services: &Rc<Services>, translate: &YewI18n) -> bool {
    match RecordingService::new().ensure_available().await {
        Ok(()) => true,
        Err(error) => {
            services.toastr.error(translate.t(error.i18n_key()));
            false
        }
    }
}
