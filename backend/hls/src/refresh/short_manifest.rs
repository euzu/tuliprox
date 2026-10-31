//! Startup qualification shares recovery selection, singleflight and retry receipts.

use super::{commit_manifest_recovery_candidate, CommittedOriginManifest, OriginRefreshRequest};
use crate::{
    hls_origin_log_value,
    manifest_acceptance::HlsManifestAcceptanceTrigger,
    manifest_fetch::{
        retry_hls_origin_manifest_recovery_chain_with_diagnostics, FetchedOriginManifest,
        HlsManifestRecoveryDiagnostics, HlsOriginManifestFetchContext, OriginManifestFetchError,
    },
    manifest_origin_binding::HlsManifestOriginBinding,
    safe_proxy_session_id, safe_session_key,
    short_manifest::{
        HlsOriginQualification, HlsOriginQualificationBasis, HlsQualifiedOriginWindows, HlsTargetDurationQualification,
        HlsTargetDurationQualificationResult,
    },
    HlsFreshManifestRequiredReason, HlsSession,
};
use tuliprox_parser::hls::origin_manifest::{
    parse_manifest_semantics, parse_origin_media_manifest, OriginManifestParseOutcome,
};
use url::Url;

pub(super) fn prepare_qualification_refresh(request: &mut OriginRefreshRequest, session: &mut HlsSession, now_ms: u64) {
    let basis = HlsOriginQualificationBasis::capture(session);
    if !session.origin_control.qualification.pending_is_current(&basis) {
        session.origin_control.qualification = HlsOriginQualification::Unchecked;
    }
    if !session.origin_is_qualified() {
        return;
    }
    let Some(reason) = request.manifest_commit_requirement.fresh_reason() else {
        return;
    };
    let pending_requirement = session.fresh_manifest_commit_requirement_generation(reason).is_some();
    let already_qualified = !pending_requirement
        && match reason {
            HlsFreshManifestRequiredReason::ColdStart
            | HlsFreshManifestRequiredReason::PreviousHardManifestFailure
            | HlsFreshManifestRequiredReason::StaleEntryWindow => true,
            HlsFreshManifestRequiredReason::ProvisioningHandoff => {
                session.pending_handoff_discontinuity_sequence.is_none()
            }
            HlsFreshManifestRequiredReason::OriginReserveExhausted => false,
            HlsFreshManifestRequiredReason::ExpiredRevalidation => {
                // Each qualification may still be waiting for its media,
                // including revalidation of an already published session.
                session.origin_qualification_awaits_publication()
                    || crate::manifest_commit::has_servable_committed_manifest(session, now_ms)
            }
        };
    if already_qualified {
        // Another lease completed the foundation check. Freshness remains a
        // normal refresh requirement, without spending another configured burst.
        request.manifest_commit_requirement = super::HlsManifestCommitRequirement::FreshCommitRequired {
            reason: HlsFreshManifestRequiredReason::StaleEntryWindow,
        };
    } else if matches!(
        reason,
        HlsFreshManifestRequiredReason::OriginReserveExhausted
            | HlsFreshManifestRequiredReason::ExpiredRevalidation
            | HlsFreshManifestRequiredReason::PreviousHardManifestFailure
            | HlsFreshManifestRequiredReason::ProvisioningHandoff
    ) {
        session.origin_control.qualification = HlsOriginQualification::Unchecked;
    }
}

pub(super) fn prepare_target_duration_qualification(request: &OriginRefreshRequest, session: &mut HlsSession) {
    if session.key.archive_reference.is_some()
        || matches!(session.mode, crate::HlsSessionMode::TransientPassthrough { .. })
    {
        session.origin_control.target_duration_qualification = None;
        return;
    }
    let basis = HlsOriginQualificationBasis::capture(session);
    let reset = session.origin_control.target_duration_qualification.as_ref().is_none_or(|check| {
        check.basis != basis
            || (check.result.is_some()
                && matches!(session.origin_control.qualification, HlsOriginQualification::Unchecked))
    });
    if reset {
        session.origin_control.target_duration_qualification =
            Some(HlsTargetDurationQualification::new(session, request.hls_proxy.min_origin_target_duration_secs()));
    }
}

pub(crate) fn observe_target_duration(session: &mut HlsSession, target_duration: Option<u32>, host: Option<&str>) {
    let Some(check) = session.origin_control.target_duration_qualification.as_mut() else { return };
    let Some(duration) = target_duration else { return };
    if check.result.is_none() && !check.warned && u64::from(duration) < check.minimum.get() {
        check.warned = true;
        log::warn!(
            "HLS origin target duration below minimum: session={} proxy_session={} host={} target_duration_secs={} minimum_target_duration_secs={} action=recovery",
            safe_session_key(&session.key), safe_proxy_session_id(&session.proxy_session_id),
            host.map_or_else(|| "none".to_string(), hls_origin_log_value), duration, check.minimum.get(),
        );
    }
}

pub(super) fn record_target_duration_qualification(session: &mut HlsSession, duration: Option<u32>) {
    let basis = HlsOriginQualificationBasis::capture(session);
    let Some(check) = session.origin_control.target_duration_qualification.as_mut() else { return };
    if check.result.is_some() {
        return;
    }
    let result = match duration.filter(|duration| *duration > 0) {
        Some(duration) if u64::from(duration) >= check.minimum.get() => {
            HlsTargetDurationQualificationResult::SuitableOriginSelected
        }
        Some(_) => HlsTargetDurationQualificationResult::NoSuitableOrigin,
        None => HlsTargetDurationQualificationResult::NotApplicable,
    };
    check.basis = basis;
    check.result = Some(result);
    if check.warned {
        let result_label = match result {
            HlsTargetDurationQualificationResult::SuitableOriginSelected => "suitable_origin_selected",
            HlsTargetDurationQualificationResult::NoSuitableOrigin
            | HlsTargetDurationQualificationResult::NotApplicable => "no_suitable_origin",
        };
        log::info!(
            "HLS origin target duration qualification completed: session={} proxy_session={} result={} host={} target_duration_secs={} minimum_target_duration_secs={}",
            safe_session_key(&session.key), safe_proxy_session_id(&session.proxy_session_id), result_label,
            session.origin_control.pinned_host.as_deref().map_or_else(|| "none".to_string(), hls_origin_log_value),
            duration.map_or_else(|| "none".to_string(), |value| value.to_string()), check.minimum.get(),
        );
    }
}

pub(super) async fn qualify_short_manifest_if_needed(
    request: &OriginRefreshRequest,
    context: &HlsOriginManifestFetchContext,
    fetched: &FetchedOriginManifest,
) -> Option<Result<CommittedOriginManifest, OriginManifestFetchError>> {
    if parse_manifest_semantics(&fetched.body).window_policy().preserves_full_manifest() {
        return None;
    }
    let OriginManifestParseOutcome::Normal(manifest) =
        parse_origin_media_manifest(&fetched.body, &fetched.final_manifest_url)
    else {
        return None;
    };
    let duration = manifest.target_duration.filter(|duration| *duration > 0);
    let short_window = (1..3).contains(&manifest.segments.len());
    let short_duration = {
        let session = request.session.read().await;
        session.origin_control.target_duration_qualification.as_ref().is_some_and(|check| {
            check.result.is_none() && duration.is_some_and(|value| u64::from(value) < check.minimum.get())
        })
    };
    if !short_window && !short_duration {
        return None;
    }
    let binding = Url::parse(&fetched.resolved_request_url)
        .ok()
        .and_then(|url| HlsManifestOriginBinding::new(url, fetched.provider_url_index).ok())?;
    let qualified = {
        let session = request.session.read().await;
        session.origin_is_qualified()
            && session.origin_control.manifest_origin_binding.as_ref() == Some(&binding)
            && crate::manifest_fetch::fetched_effective_manifest_host(fetched).as_deref()
                == session.origin_control.pinned_host.as_deref()
    };
    if qualified {
        return None;
    }
    {
        let mut session = request.session.write().await;
        if !super::commit::refresh_origin_work_generation_is_current(&session, request) {
            return Some(Err(OriginManifestFetchError::RecoveryUnavailable {
                reason: crate::manifest_fetch::HlsManifestRecoveryUnavailableReason::BindingSuperseded,
            }));
        }
        observe_target_duration(
            &mut session,
            duration,
            crate::manifest_fetch::fetched_effective_manifest_host(fetched).as_deref(),
        );
    }
    let mut qualification_context = context.clone();
    if !fetched.provider_session_headers.is_empty() {
        let mut session = request.session.write().await;
        if !super::commit::refresh_origin_work_generation_is_current(&session, request) {
            return Some(Err(OriginManifestFetchError::RecoveryUnavailable {
                reason: crate::manifest_fetch::HlsManifestRecoveryUnavailableReason::BindingSuperseded,
            }));
        }
        // HTTP session cookies belong to the transport. Keeping this successful
        // response's cookie does not commit its media or qualify its short window.
        session.origin_provider_session_headers = crate::headers::merge_hls_provider_session_headers(
            &session.origin_provider_session_headers,
            &fetched.provider_session_headers,
        );
        qualification_context.headers =
            crate::hls_origin_headers_with_provider_session(&context.headers, &session.origin_provider_session_headers);
    }
    let mut diagnostics = HlsManifestRecoveryDiagnostics::default();
    let recovery_result = run_qualification(request, &qualification_context, binding, &mut diagnostics).await;
    // Duration is a preference, never a new failure reason. The original regular
    // window still has to pass the ordinary commit and ownership checks.
    let recovery_result = if recovery_result.is_err()
        && short_duration
        && !short_window
        && !request.attempt.as_ref().is_some_and(|attempt| attempt.is_closed())
    {
        match commit_manifest_recovery_candidate(
            request,
            fetched.clone(),
            request.manifest_commit_requirement.acceptance_mode(),
        )
        .await
        {
            Ok(committed) => {
                let mut session = request.session.write().await;
                if let Some(episode) = session.origin_control.acceptance_episode.as_mut() {
                    episode.complete();
                }
                Ok(committed)
            }
            Err(_) => recovery_result,
        }
    } else {
        recovery_result
    };
    let result = recovery_result.map(|mut committed| {
        diagnostics.include_initial_fetch(fetched, &mut committed.fetched);
        committed
    });
    Some(result)
}

pub(super) async fn resume_qualification(
    request: &OriginRefreshRequest,
    context: &HlsOriginManifestFetchContext,
) -> Option<Result<CommittedOriginManifest, OriginManifestFetchError>> {
    let target = {
        let mut session = request.session.write().await;
        let target = session.origin_qualification_retry_target().cloned()?;
        if let Some(reason) = request.manifest_commit_requirement.fresh_reason() {
            let trigger = request.manifest_commit_requirement.strict_handoff_trigger();
            if trigger.starts_episode()
                && session.fresh_manifest_commit_requirement_generation(reason).is_some()
                && session
                    .origin_control
                    .acceptance_episode
                    .as_ref()
                    .is_some_and(|episode| episode.trigger() != trigger)
            {
                // A new lifecycle finding gets one new full check of the known
                // concrete target. Further leases inherit that check's receipt.
                session.origin_control.qualification = HlsOriginQualification::Unchecked;
                session.origin_control.target_duration_qualification = Some(HlsTargetDurationQualification::new(
                    &session,
                    request.hls_proxy.min_origin_target_duration_secs(),
                ));
            }
        }
        target
    };
    Some(run_qualification(request, context, target, &mut HlsManifestRecoveryDiagnostics::default()).await)
}

async fn run_qualification(
    request: &OriginRefreshRequest,
    context: &HlsOriginManifestFetchContext,
    binding: HlsManifestOriginBinding,
    diagnostics: &mut HlsManifestRecoveryDiagnostics,
) -> Result<CommittedOriginManifest, OriginManifestFetchError> {
    let trigger = {
        let session = request.session.read().await;
        session
            .origin_qualification_retry_target()
            .and_then(|_| {
                session
                    .origin_control
                    .acceptance_episode
                    .as_ref()
                    .map(crate::manifest_acceptance::HlsManifestAcceptanceEpisode::trigger)
            })
            .unwrap_or_else(|| {
                let trigger = super::manifest_recovery_trigger(request);
                if trigger.starts_episode() {
                    trigger
                } else {
                    HlsManifestAcceptanceTrigger::Observe
                }
            })
    };
    Box::pin(retry_hls_origin_manifest_recovery_chain_with_diagnostics(
        context,
        binding,
        None,
        None,
        trigger,
        request.manifest_commit_requirement.acceptance_mode(),
        |fetched, mode| Box::pin(commit_manifest_recovery_candidate(request, fetched, mode)),
        diagnostics,
    ))
    .await
}

pub(super) fn record_origin_qualification(session: &mut HlsSession, segment_count: usize) {
    let basis = HlsOriginQualificationBasis::capture(session);
    if session.origin_control.qualification.qualified_windows(&basis).is_some() {
        return;
    }
    let generation = session.origin_control.acceptance_generation;
    let full_burst = session.origin_control.qualification.generation() == Some(generation)
        && matches!(session.origin_control.qualification, HlsOriginQualification::Checking { .. })
        && session
            .origin_control
            .acceptance_episode
            .as_ref()
            .is_some_and(|episode| episode.generation == generation && episode.full_burst_completed);
    let windows = if (1..3).contains(&segment_count) && full_burst {
        crate::short_manifest::log_short_window_warning(session, segment_count, "origin window qualified");
        HlsQualifiedOriginWindows::ShortAllowed
    } else if segment_count >= 3 {
        HlsQualifiedOriginWindows::Regular
    } else {
        return;
    };
    session.origin_control.qualification = HlsOriginQualification::Qualified {
        basis,
        generation,
        windows,
        publication_boundary: session.last_normal_manifest_commit_identity(),
    };
}
