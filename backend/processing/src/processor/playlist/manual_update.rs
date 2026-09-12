use super::{process_sources, MetadataUpdateSink, PlaylistProcessingContext};
use shared::{
    error::TuliproxError,
    model::{
        EventMessage, EventSink, LibraryScanResult, PlaylistUpdateProgressEvent, PlaylistUpdateState, SourceStats,
    },
};
use std::{future::Future, io};
use tuliprox_core::model::UpdateGuard;
use tuliprox_library::library::LibraryProcessor;

/// Bulk and scheduled runs read the existing catalog. Only an explicit card request scans.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LibraryUpdateMode {
    #[default]
    ExistingCatalog,
    Rescan {
        input_id: u16,
    },
}

impl LibraryUpdateMode {
    pub(crate) fn reloads_input(self, input_id: u16) -> bool { self == Self::Rescan { input_id } }
}

/// The rebuild is not even started until the existing scanner reports complete success.
async fn rebuild_after_scan<T>(
    scan: impl Future<Output = io::Result<LibraryScanResult>>,
    report: impl FnOnce(LibraryScanResult),
    rebuild: impl FnOnce() -> T,
) -> io::Result<T> {
    let result = scan.await?;
    let complete = result.errors == 0;
    report(result);
    if !complete {
        return Err(io::Error::other("Library rescan reported incomplete input data"));
    }
    Ok(rebuild())
}

pub(super) async fn process_manual_update<E: EventSink + Clone + 'static, M: MetadataUpdateSink>(
    ctx: &PlaylistProcessingContext<E, M>,
    update_guard: Option<&UpdateGuard>,
) -> (Vec<SourceStats>, Vec<TuliproxError>) {
    let LibraryUpdateMode::Rescan { input_id } = ctx.library_update_mode else {
        return process_sources(ctx).await;
    };
    let progress = |message: &str| {
        ctx.events.emit(EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent::for_run_input(
            ctx.run_id.clone(),
            ctx.execution_order,
            input_id,
            "Library rescan",
            message,
        )));
    };
    progress("Library rescan started; selected target rebuild is waiting");
    let result = async {
        // The worker already owns the playlist permit. Never wait for a second lock:
        // a concurrent standalone scan makes this request fail, without a lock cycle.
        let _library_permit = update_guard
            .and_then(UpdateGuard::try_library)
            .ok_or_else(|| io::Error::other("Library scan is already running or its guard is unavailable"))?;
        let (library, metadata, storage_dir) = {
            let config = ctx.config.config.load();
            let library = config
                .library
                .as_ref()
                .filter(|library| library.enabled)
                .ok_or_else(|| io::Error::other("Library is not enabled"))?
                .clone();
            (library, config.metadata_update.clone(), config.storage_dir.clone())
        };
        let processor = LibraryProcessor::new(library, metadata.as_ref(), ctx.client.clone(), &storage_dir);
        let rebuild = rebuild_after_scan(
            processor.scan_for_target_rebuild(),
            |result| {
                if result.errors > 0 {
                    log::error!("Library rescan failed for input {input_id}: incomplete scan; {result:?}");
                }
                ctx.events.emit(EventMessage::PlaylistUpdateProgress(
                    PlaylistUpdateProgressEvent::for_run_input(
                        ctx.run_id.clone(),
                        ctx.execution_order,
                        input_id,
                        "Library rescan",
                        "Library scan result",
                    )
                    .with_library_scan_result(result),
                ));
            },
            || {
                progress("Library rescan completed; loading input data and rebuilding selected targets");
                process_sources(ctx)
            },
        )
        .await?;
        // Retain the existing library permit until the catalog-dependent rebuild finishes.
        Ok::<_, io::Error>(rebuild.await)
    }
    .await;
    match result {
        Ok(result) => result,
        Err(error) => {
            log::error!("Library rescan failed for input {input_id}: {error}");
            if let Some(input) = ctx.config.get_input_by_id(input_id) {
                super::input_status::persist_input_completion(ctx, input.id, &input.name, PlaylistUpdateState::Failure)
                    .await;
            }
            ctx.events.emit(EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent::input_completed(
                ctx.run_id.clone(),
                ctx.execution_order,
                input_id,
                PlaylistUpdateState::Failure,
                "Library rescan",
                "Library rescan failed; selected targets were not rebuilt",
            )));
            (Vec::new(), vec![TuliproxError::RepositoryLibrary(format!("Library rescan failed: {error}"))])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn scan_result(errors: usize) -> LibraryScanResult {
        LibraryScanResult {
            files_scanned: 0,
            groups_scanned: 0,
            files_added: 0,
            files_updated: 0,
            files_removed: 0,
            errors,
        }
    }

    #[tokio::test]
    async fn manual_update_library_rebuild_waits_for_successful_scan() {
        let completed = Cell::new(false);
        let scan = async {
            tokio::task::yield_now().await;
            completed.set(true);
            Ok(scan_result(0))
        };
        let rebuilt = rebuild_after_scan(
            scan,
            |_| {},
            || {
                assert!(completed.get());
                "common target pipeline"
            },
        )
        .await;
        assert_eq!(rebuilt.unwrap(), "common target pipeline");
    }

    #[tokio::test]
    async fn manual_update_library_errors_and_partial_scan_never_start_target_rebuild() {
        for result in [Err(io::Error::other("scan failed")), Ok(LibraryScanResult { errors: 1, ..scan_result(0) })] {
            let rebuilt = Cell::new(false);
            let reported = std::cell::RefCell::new(None);
            let expected = result.as_ref().ok().cloned();
            assert!(rebuild_after_scan(
                async { result },
                |scan| *reported.borrow_mut() = Some(scan),
                || rebuilt.set(true)
            )
            .await
            .is_err());
            assert_eq!(*reported.borrow(), expected);
            assert!(!rebuilt.get());
        }
    }

    #[test]
    fn manual_update_library_bulk_reuses_catalog_without_scan_or_policy_override() {
        assert_eq!(LibraryUpdateMode::default(), LibraryUpdateMode::ExistingCatalog);
        assert!(!LibraryUpdateMode::ExistingCatalog.reloads_input(2));
        assert!(LibraryUpdateMode::Rescan { input_id: 2 }.reloads_input(2));
        assert!(!LibraryUpdateMode::Rescan { input_id: 2 }.reloads_input(3));
    }
}
