use super::{
    diagnostics::{debug_repair_analysis, debug_repair_event, debug_repair_stream_dropped, decide_repair},
    ffmpeg_identity_version, parse_ffmpeg_warnings,
    probe::{
        adopt_repair_output, cleanup_repair_output, detect_video_codec, parse_probe, repair_output_path,
        select_repair_remux_streams, validate_repair,
    },
    registry::repair_object_metadata_key,
    sha256_file,
    window::{RepairCandidateSelection, RepairStatus},
    CachedSegmentMetadata, HlsCacheObjectKey, HlsPostProcessingDeadline, HlsRepairObjectMetadata,
    HlsRepairWindowCandidateKey, HlsSegmentCache, HlsSegmentRepairManager, HlsSegmentRepairObjectContext,
    HlsSegmentRepairRuntime, RepairIdentity, RepairRemuxStreamSelection, SegmentProbe, StagedCacheObject,
    COMMAND_VERSION,
};
use log::{debug, warn};
use shared::model::{HlsSegmentRepairExecutionPlan, HlsSegmentRepairMode};
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use tokio::{fs, process::Command, time::timeout};

impl HlsSegmentRepairManager {
    pub(super) async fn process_staged_and_commit<K>(
        &self,
        segment_cache: &HlsSegmentCache,
        key: &K,
        raw: StagedCacheObject,
        context: HlsSegmentRepairObjectContext,
        runtime: Arc<HlsSegmentRepairRuntime>,
        deadline: HlsPostProcessingDeadline,
    ) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
    {
        let selected_repair = self.try_select_candidate_with_runtime(&context, &runtime).await;
        if selected_repair.is_none()
            && runtime.config.corrupt_segment_watchdog.mode.is_enabled()
            && context.is_repairable_ts()
        {
            let raw_hash = sha256_file(&raw.path).await?;
            return self
                .watchdog
                .process_staged_and_commit(
                    segment_cache,
                    key,
                    raw,
                    &context,
                    &runtime.config.corrupt_segment_watchdog,
                    &runtime.watchdog_semaphore,
                    raw_hash,
                    &deadline,
                )
                .await;
        }
        let Some(mode) = selected_repair else {
            return segment_cache.commit_staged(key, raw).await;
        };
        let raw_hash = sha256_file(&raw.path).await?;
        let object_key = repair_object_metadata_key(&context, mode);
        if self.object_metadata_matches(&object_key, &raw_hash).await {
            return segment_cache.commit_staged(key, raw).await;
        }
        let identity = RepairIdentity {
            raw_sha256: raw_hash.clone(),
            repair_mode: mode,
            command_version: COMMAND_VERSION,
            ffmpeg_version: ffmpeg_identity_version(),
        };
        if self.repair_metadata(&identity).await.is_some() {
            let committed = segment_cache.commit_staged(key, raw).await?;
            self.record_object_metadata_from_repair_identity(object_key, raw_hash.clone(), Some(raw_hash), &identity)
                .await;
            return Ok(committed);
        }
        let lock = self.lock_for_identity(identity.clone()).await;
        let result = {
            let _guard = lock.lock().await;
            if self.repair_metadata(&identity).await.is_some() {
                let committed = segment_cache.commit_staged(key, raw).await?;
                self.record_object_metadata_from_repair_identity(
                    object_key.clone(),
                    raw_hash.clone(),
                    Some(raw_hash.clone()),
                    &identity,
                )
                .await;
                Ok(committed)
            } else {
                let raw_size = raw.size;
                if let Some(fixed_path) =
                    self.repair_file(&raw.path, raw.size, &identity, &context, runtime.clone(), &deadline).await?
                {
                    let fixed_size = fs::metadata(&fixed_path).await?.len();
                    if let Err(error) = segment_cache.remove_staged(raw.clone()).await {
                        warn!("HLS repair raw staging cleanup deferred: error_kind={:?}", error.kind());
                    }
                    let fixed = adopt_repair_output(segment_cache, fixed_path, fixed_size).await?;
                    let committed = segment_cache.commit_staged(key, fixed).await?;
                    self.record_metadata(identity.clone(), RepairStatus::Fixed, raw.size, committed.size, None).await;
                    let committed_hash = sha256_file(&committed.path).await?;
                    self.record_object_metadata(
                        object_key.clone(),
                        HlsRepairObjectMetadata {
                            committed_sha256: committed_hash,
                            raw_sha256: Some(raw_hash.clone()),
                            status: RepairStatus::Fixed,
                            raw_size,
                            final_size: committed.size,
                            validation_reason: None,
                        },
                    )
                    .await;
                    Ok(committed)
                } else {
                    let committed = segment_cache.commit_staged(key, raw).await?;
                    self.record_object_metadata_from_repair_identity(
                        object_key.clone(),
                        raw_hash.clone(),
                        Some(raw_hash.clone()),
                        &identity,
                    )
                    .await;
                    Ok(committed)
                }
            }
        };
        self.remove_lock_if_unused(&identity, &lock).await;
        result
    }

    pub(super) async fn try_select_candidate(
        &self,
        context: &HlsSegmentRepairObjectContext,
    ) -> Option<(HlsSegmentRepairMode, Arc<HlsSegmentRepairRuntime>, HlsRepairWindowCandidateKey)> {
        let runtime = self.runtime.load_full();
        match self.select_candidate(context, &runtime).await {
            RepairCandidateSelection::Selected(mode, candidate) => Some((mode, runtime, candidate)),
            RepairCandidateSelection::Skipped(_) | RepairCandidateSelection::AlreadyChecked(_, _) => None,
        }
    }

    pub(super) async fn try_select_or_join_candidate(
        &self,
        context: &HlsSegmentRepairObjectContext,
    ) -> Option<(HlsSegmentRepairMode, Arc<HlsSegmentRepairRuntime>, HlsRepairWindowCandidateKey)> {
        let runtime = self.runtime.load_full();
        match self.select_candidate(context, &runtime).await {
            RepairCandidateSelection::Selected(mode, candidate)
            | RepairCandidateSelection::AlreadyChecked(mode, candidate) => Some((mode, runtime, candidate)),
            RepairCandidateSelection::Skipped(_) => None,
        }
    }

    pub(super) async fn try_select_candidate_with_runtime(
        &self,
        context: &HlsSegmentRepairObjectContext,
        runtime: &Arc<HlsSegmentRepairRuntime>,
    ) -> Option<HlsSegmentRepairMode> {
        match self.select_candidate(context, runtime).await {
            RepairCandidateSelection::Selected(mode, _) => Some(mode),
            RepairCandidateSelection::Skipped(_) | RepairCandidateSelection::AlreadyChecked(_, _) => None,
        }
    }

    pub(super) async fn select_candidate(
        &self,
        context: &HlsSegmentRepairObjectContext,
        runtime: &Arc<HlsSegmentRepairRuntime>,
    ) -> RepairCandidateSelection {
        if !runtime.repair_enabled() {
            return RepairCandidateSelection::Skipped("disabled");
        }
        if context.hls_access_lease_id.is_none() {
            return RepairCandidateSelection::Skipped("missing-lease");
        }
        let selection = self.windows.write().await.try_select_candidate(context);
        match &selection {
            RepairCandidateSelection::Selected(mode, _) => debug!(
                "HLS segment repair candidate selected: {} source={} resource={} mode={}",
                context.log_identity_fields(),
                context.source.as_log_value(),
                context.resource_id,
                mode.as_log_value()
            ),
            RepairCandidateSelection::Skipped(reason) => debug!(
                "HLS segment repair candidate skipped: {} source={} resource={} reason={}",
                context.log_identity_fields(),
                context.source.as_log_value(),
                context.resource_id,
                reason
            ),
            RepairCandidateSelection::AlreadyChecked(_, _) => {}
        }
        selection
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn repair_file(
        &self,
        raw_path: &Path,
        raw_size: u64,
        identity: &RepairIdentity,
        context: &HlsSegmentRepairObjectContext,
        runtime: Arc<HlsSegmentRepairRuntime>,
        deadline: &HlsPostProcessingDeadline,
    ) -> io::Result<Option<PathBuf>> {
        let _permit = match &runtime.semaphore {
            Some(semaphore) => {
                let Some(remaining) = deadline.remaining() else {
                    self.record_metadata(
                        identity.clone(),
                        RepairStatus::Timeout,
                        raw_size,
                        raw_size,
                        Some("timeout".to_string()),
                    )
                    .await;
                    return Ok(None);
                };
                Some(
                    timeout(remaining, semaphore.acquire())
                        .await
                        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "repair semaphore timed out"))?
                        .map_err(|_| io::Error::other("repair semaphore closed"))?,
                )
            }
            None => None,
        };
        let started = Instant::now();
        let raw_scan = match analyze_segment(raw_path, deadline).await {
            Ok(scan) => scan,
            Err(reason) => {
                debug_repair_event(context, identity.repair_mode, "analysis skipped", Some(&reason));
                self.record_metadata(identity.clone(), RepairStatus::Unsupported, raw_size, raw_size, Some(reason))
                    .await;
                return Ok(None);
            }
        };
        let codec = detect_video_codec(&raw_scan);
        let decision = decide_repair(codec, &raw_scan.warnings);
        let execution_plan = identity.repair_mode.execution_plan(decision.required_level);
        let executed_level = match execution_plan {
            HlsSegmentRepairExecutionPlan::Repair(level) => Some(level),
            HlsSegmentRepairExecutionPlan::SkipNoTrigger
            | HlsSegmentRepairExecutionPlan::SkipConfiguredMaxBelowRequired => None,
        };
        debug_repair_analysis(context, identity.repair_mode, decision, executed_level, &raw_scan.warnings);
        let executed_level = match execution_plan {
            HlsSegmentRepairExecutionPlan::Repair(level) => level,
            HlsSegmentRepairExecutionPlan::SkipNoTrigger => {
                self.record_metadata(identity.clone(), RepairStatus::Clean, raw_size, raw_size, None).await;
                return Ok(None);
            }
            HlsSegmentRepairExecutionPlan::SkipConfiguredMaxBelowRequired => {
                warn!(
                    "HLS segment repair required level exceeds configured max: {} source={} resource={} configured_max_level={} required_level={} trigger={} action=raw_commit",
                    context.log_identity_fields(),
                    context.source.as_log_value(),
                    context.resource_id,
                    identity.repair_mode.as_log_value(),
                    decision.required_level.as_log_value(),
                    decision.trigger_source.as_log_value()
                );
                self.record_metadata(
                    identity.clone(),
                    RepairStatus::PolicyLimited,
                    raw_size,
                    raw_size,
                    Some("configured_max_level_below_required_level".to_string()),
                )
                .await;
                return Ok(None);
            }
        };
        let stream_selection = match select_repair_remux_streams(&raw_scan) {
            Ok(selection) => selection,
            Err(reason) => {
                debug_repair_event(context, executed_level, "stream selection skipped", Some(&reason));
                self.record_metadata(identity.clone(), RepairStatus::Unsupported, raw_size, raw_size, Some(reason))
                    .await;
                return Ok(None);
            }
        };
        for dropped in &stream_selection.dropped_streams {
            debug_repair_stream_dropped(context, dropped);
        }
        let fixed_path = repair_output_path(raw_path);
        debug!(
            "HLS segment repair remux started: {} source={} resource={} configured_max_level={} executed_level={}",
            context.log_identity_fields(),
            context.source.as_log_value(),
            context.resource_id,
            identity.repair_mode.as_log_value(),
            executed_level.as_log_value()
        );
        let remux = run_remux(raw_path, &fixed_path, executed_level, &stream_selection, deadline).await;
        if let Err(reason) = remux {
            debug_repair_event(context, executed_level, "remux failed", Some(&reason));
            let status = if reason == "timeout" { RepairStatus::Timeout } else { RepairStatus::RemuxFailed };
            cleanup_repair_output(&fixed_path).await;
            self.record_metadata(identity.clone(), status, raw_size, raw_size, Some(reason)).await;
            return Ok(None);
        }
        let fixed_scan = match analyze_segment(&fixed_path, deadline).await {
            Ok(scan) => scan,
            Err(reason) => {
                debug_repair_event(context, executed_level, "validation probe failed", Some(&reason));
                cleanup_repair_output(&fixed_path).await;
                self.record_metadata(
                    identity.clone(),
                    RepairStatus::ValidationFailed,
                    raw_size,
                    raw_size,
                    Some(reason),
                )
                .await;
                return Ok(None);
            }
        };
        let validation =
            validate_repair(&runtime.config, codec, &raw_scan, &fixed_scan, executed_level, &stream_selection);
        if let Err(reason) = validation {
            debug_repair_event(context, executed_level, "validation failed", Some(&reason));
            cleanup_repair_output(&fixed_path).await;
            self.record_metadata(identity.clone(), RepairStatus::ValidationFailed, raw_size, raw_size, Some(reason))
                .await;
            return Ok(None);
        }
        debug!(
            "HLS segment repair remux completed: {} source={} resource={} configured_max_level={} executed_level={} duration_ms={}",
            context.log_identity_fields(),
            context.source.as_log_value(),
            context.resource_id,
            identity.repair_mode.as_log_value(),
            executed_level.as_log_value(),
            started.elapsed().as_millis()
        );
        Ok(Some(fixed_path))
    }
}

async fn analyze_segment(path: &Path, deadline: &HlsPostProcessingDeadline) -> Result<SegmentProbe, String> {
    let probe_output = run_command_with_deadline(
        "ffprobe",
        &[
            "-hide_banner",
            "-v",
            "warning",
            "-show_entries",
            "format=duration,size,bit_rate",
            "-show_streams",
            "-of",
            "json",
            path.to_str().ok_or_else(|| "invalid_path".to_string())?,
        ],
        deadline,
    )
    .await?;
    let warnings_output = run_command_with_deadline(
        "ffmpeg",
        &[
            "-hide_banner",
            "-nostdin",
            "-v",
            "warning",
            "-i",
            path.to_str().ok_or_else(|| "invalid_path".to_string())?,
            "-map",
            "0",
            "-c",
            "copy",
            "-f",
            "null",
            "-",
        ],
        deadline,
    )
    .await
    .unwrap_or_else(|stderr| stderr);
    parse_probe(&probe_output, parse_ffmpeg_warnings(&warnings_output))
}

async fn run_remux(
    input_path: &Path,
    output_path: &Path,
    mode: HlsSegmentRepairMode,
    stream_selection: &RepairRemuxStreamSelection,
    deadline: &HlsPostProcessingDeadline,
) -> Result<(), String> {
    let input = input_path.to_str().ok_or_else(|| "invalid_input_path".to_string())?;
    let output = output_path.to_str().ok_or_else(|| "invalid_output_path".to_string())?;
    let mut args = ["-hide_banner", "-nostdin", "-y", "-copyts", "-i", input]
        .into_iter()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    for stream_index in &stream_selection.mapped_streams {
        args.push("-map".to_string());
        args.push(format!("0:{stream_index}"));
    }
    args.extend(["-c", "copy"].into_iter().map(ToOwned::to_owned));
    if matches!(mode, HlsSegmentRepairMode::Medium | HlsSegmentRepairMode::High) {
        args.push("-bsf:v".to_string());
        args.push("dump_extra=freq=keyframe".to_string());
    }
    args.push("-mpegts_flags".to_string());
    args.push(
        if mode == HlsSegmentRepairMode::High { "+resend_headers+pat_pmt_at_frames" } else { "+resend_headers" }
            .to_string(),
    );
    args.extend(
        ["-mpegts_copyts", "1", "-muxpreload", "0", "-muxdelay", "0", "-f", "mpegts", output]
            .into_iter()
            .map(ToOwned::to_owned),
    );
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    run_command_with_deadline("ffmpeg", &args, deadline).await.map(|_| ())
}

pub(crate) async fn run_command_with_deadline(
    binary: &str,
    args: &[&str],
    deadline: &HlsPostProcessingDeadline,
) -> Result<String, String> {
    let Some(remaining) = deadline.remaining() else {
        return Err("timeout".to_string());
    };
    let output = timeout(remaining, {
        let mut command = Command::new(binary);
        command.args(args).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        command.output()
    })
    .await
    .map_err(|_| "timeout".to_string())?
    .map_err(
        |err| {
            if err.kind() == io::ErrorKind::NotFound {
                "unsupported".to_string()
            } else {
                err.to_string()
            }
        },
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if output.status.success() {
        Ok(if stdout.is_empty() { stderr } else { stdout })
    } else {
        Err(if stderr.is_empty() { "command_failed".to_string() } else { stderr })
    }
}
