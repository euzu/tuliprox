mod playlist;
mod xtream;
// mod affix;
mod xtream_vod;
mod xtream_series;
mod epg;
mod sort;
mod trakt;
mod library;
mod stream_probe;
pub use self::playlist::*;
pub use self::epg::*;
pub use self::xtream::*;
pub use self::xtream_vod::*;
pub use self::xtream_series::*;
pub use self::stream_probe::*;
use shared::create_bitset;

create_bitset!(u8, ResolveOptionsFlags, Resolve, TmdbMissing, Probe, Background);

pub struct ResolveOptions {
    flags: ResolveOptionsFlagsSet,
    pub resolve_delay: u16,
}

impl ResolveOptions {
    #[inline]
    pub fn has_flag(&self, flag: ResolveOptionsFlags) -> bool {
        self.flags.contains(flag)
    }
    #[inline]
    pub fn unset_flag(&mut self, flag: ResolveOptionsFlags) {
        self.flags.unset(flag);
    }
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            flags: ResolveOptionsFlagsSet::from_variants(&[ResolveOptionsFlags::Background]),
            resolve_delay: default_resolve_delay_secs(),
        }
    }
}

pub(crate) const FOREGROUND_BATCH_SIZE: usize = 200;
pub(crate) const FOREGROUND_RETRY_BATCH_MAX_SIZE: usize = FOREGROUND_BATCH_SIZE * 4;
pub(crate) const FOREGROUND_MIN_RETRY_DELAY_SECS: u64 = 1;


//
// fn get_resolve_<cluster>_options(target: &ConfigTarget, fpl: &FetchedPlaylist) -> bool
//
#[macro_export]
macro_rules! create_resolve_options_function_for_xtream_target {
    ($cluster:ident) => {
        paste::paste! {
            fn [<get_resolve_ $cluster _options>](target: &ConfigTarget, fpl: &FetchedPlaylist) -> $crate::processing::processor::ResolveOptions {
                match target.get_xtream_output() {
                    Some(_) => {
                        let input_options = fpl.input.options.as_ref();
                        let input_is_xtream = fpl.input.input_type.is_xtream();

                        let (
                            resolve_tmdb_missing,
                            input_resolve_enabled,
                            input_probe_enabled,
                            input_resolve_delay,
                            resolve_background
                        ) = if let Some(options) = input_options {
                            (
                                options.has_flag($crate::model::ConfigInputFlags::ResolveTmdb),
                                options.has_flag($crate::model::ConfigInputFlags::[<Resolve $cluster:camel>]),
                                options.has_flag($crate::model::ConfigInputFlags::[<Probe $cluster:camel>]),
                                options.resolve_delay,
                                options.has_flag($crate::model::ConfigInputFlags::ResolveBackground)
                            )
                        } else {
                            (
                                false,
                                false,
                                false,
                                shared::utils::default_resolve_delay_secs(),
                                shared::utils::default_resolve_background(),
                            )
                        };

                        let resolve_enabled = input_resolve_enabled;
                        let probe_enabled = input_probe_enabled;
                        let resolve_delay = input_resolve_delay;

                        let mut flags = $crate::processing::processor::ResolveOptionsFlagsSet::new();
                        if resolve_enabled && input_is_xtream {
                            flags.set($crate::processing::processor::ResolveOptionsFlags::Resolve);
                        }
                        if resolve_tmdb_missing {
                            flags.set($crate::processing::processor::ResolveOptionsFlags::TmdbMissing);
                        }
                        if input_is_xtream && probe_enabled {
                            flags.set($crate::processing::processor::ResolveOptionsFlags::Probe);
                        }
                        if resolve_background {
                            flags.set($crate::processing::processor::ResolveOptionsFlags::Background);
                        }
                        $crate::processing::processor::ResolveOptions {
                            flags,
                            resolve_delay,
                        }
                    },
                    None => $crate::processing::processor::ResolveOptions::default(),
                }
            }
        }
    };
}
use create_resolve_options_function_for_xtream_target;

/// Foreground retry helper that retries each queued item at most once.
///
/// `retry_delay_secs` is applied sequentially per item, so the total wall-clock
/// delay is roughly `retry_delay_secs * retry_item_count` plus network/DB time.
macro_rules! process_foreground_retry_once {
    (
        ctx: $ctx:expr,
        fpl: $fpl:expr,
        filter: $filter:expr,
        retry_once_ids: $retry_once_ids:ident,
        retry_delay_secs: $retry_delay_secs:expr,
        xtream_path: $xtream_path:ident,
        db_query_holder: $db_query_holder:ident,
        db_lock_holder: $db_lock_holder:ident,
        batch: $batch:ident,
        batch_size: $batch_size:expr,
        retry_batch_max_len: $retry_batch_max_len:expr,
        processed_count: $processed_count:ident,
        query_error_context: $query_error_context:expr,
        reasons: |$pli_reasons:ident| $reasons_expr:expr,
        update: |$active_provider:ident, $pli_update:ident, $provider_id:ident, $reasons:ident, $db_query_ref:ident| $update_expr:expr,
        apply_properties: |$pli_apply:ident, $updated_props:ident| $apply_expr:expr,
        persist: |$updates:ident| $persist_expr:expr,
        on_persist_error: |$persist_err:ident| $on_persist_error_expr:expr,
        on_retry_error: |$pli_error:ident, $retry_err:ident| $on_retry_error_expr:expr,
        on_after_attempt: |$pli_after:ident, $retry_succeeded:ident| $on_after_attempt_expr:expr $(,)?
    ) => {
        for __pli in $fpl.items_mut() {
            if !($filter)(__pli) {
                continue;
            }

            let __provider_id = if let Ok(__uid) = __pli.header.id.parse::<u32>() {
                crate::api::model::ProviderIdType::Id(__uid)
            } else {
                crate::api::model::ProviderIdType::from(&*__pli.header.id)
            };

            if !$retry_once_ids.remove(&__provider_id) {
                continue;
            }

            let mut __retry_succeeded = false;
            let __reasons = {
                let $pli_reasons = &mut *__pli;
                $reasons_expr
            };

            if !__reasons.is_empty() {
                if let Some(__active_provider) = $ctx.provider_manager.as_ref() {
                    // Do not hold a read lock over the retry delay window.
                    if $db_query_holder.is_some() {
                        $db_query_holder = None;
                        $db_lock_holder = None;
                    }

                    tokio::time::sleep(std::time::Duration::from_secs($retry_delay_secs)).await;

                    if $db_query_holder.is_none() && $xtream_path.exists() {
                        let __file_lock = $ctx.config.file_locks.read_lock(&$xtream_path).await;
                        let __xtream_path = $xtream_path.clone();
                        let __query = match tokio::task::spawn_blocking(move || {
                            crate::repository::BPlusTreeQuery::<u32, shared::model::XtreamPlaylistItem>::try_new(
                                &__xtream_path,
                            )
                        })
                        .await
                        {
                            Ok(Ok(__query)) => Some((__query, __file_lock)),
                            Ok(Err(__err)) => {
                                log::error!("Failed to open BPlusTreeQuery for {}: {__err}", $query_error_context);
                                None
                            }
                            Err(__err) => {
                                log::error!("Failed to open BPlusTreeQuery for {}: {__err}", $query_error_context);
                                None
                            }
                        };

                        if let Some((__query, __guard)) = __query {
                            $db_query_holder = Some(std::sync::Arc::new(parking_lot::Mutex::new(__query)));
                            $db_lock_holder = Some(__guard);
                        }
                    }

                    let __db_query_ref = $db_query_holder.as_ref().map(std::sync::Arc::clone);
                    let __update_future = {
                        let $active_provider = __active_provider;
                        let $pli_update = &mut *__pli;
                        let $provider_id = __provider_id.clone();
                        let $reasons = &__reasons;
                        let $db_query_ref = __db_query_ref;
                        $update_expr
                    };

                    match __update_future.await {
                        Ok(Some(__updated_props)) => {
                            {
                                let $pli_apply = &mut *__pli;
                                let $updated_props = &__updated_props;
                                $apply_expr
                            }

                            $batch.push((__provider_id.clone(), __updated_props));

                            if $batch.len() >= $batch_size {
                                $db_query_holder = None;
                                $db_lock_holder = None;

                                let __updates: Vec<(u32, _)> = $batch
                                    .iter()
                                    .filter_map(|(__id, __props)| {
                                        // Foreground retry batches can include text provider IDs (e.g. M3U).
                                        // Persist batch functions for these paths are keyed by numeric Xtream IDs,
                                        // so text IDs are intentionally skipped here.
                                        if let crate::api::model::ProviderIdType::Id(__vid) = __id {
                                            Some((*__vid, __props.clone()))
                                        } else {
                                            None
                                        }
                                    })
                                    .collect();

                                if __updates.is_empty() {
                                    $batch.clear();
                                } else {
                                    let __persist_future = {
                                        let $updates = __updates;
                                        $persist_expr
                                    };
                                    match __persist_future.await {
                                        Ok(()) => $batch.clear(),
                                        Err($persist_err) => {
                                            $on_persist_error_expr;
                                            if $batch.len() > $retry_batch_max_len {
                                                let __drop_count = $batch.len().saturating_sub($retry_batch_max_len);
                                                if __drop_count > 0 {
                                                    $batch.drain(0..__drop_count);
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            $processed_count += 1;
                            __retry_succeeded = true;
                        }
                        Ok(None) => {}
                        Err($retry_err) => {
                            let $pli_error = &*__pli;
                            $on_retry_error_expr;
                        }
                    }
                }
            }

            {
                let $pli_after = &mut *__pli;
                let $retry_succeeded = __retry_succeeded;
                $on_after_attempt_expr;
            }
        }
    };
}
pub(crate) use process_foreground_retry_once;
use shared::utils::default_resolve_delay_secs;
