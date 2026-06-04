use crate::model::ConfigTarget;
use crate::model::{AppConfig, ProxyUserCredentials};
use crate::repository::m3u_get_file_path_for_db;
use crate::repository::storage_const;
use crate::repository::user_get_bouquet_filter;
use crate::repository::{ensure_target_storage_path, get_file_path_for_db_index};
use crate::repository::{open_playlist_reader, LockedReceiverStream};
use crate::utils::build_m3u_catchup_rewrite;
use futures::Stream;
use log::error;
use shared::create_bitset;
use shared::error::TuliproxError;
use shared::model::{ConfigTargetOptions, M3uPlaylistItem, PlaylistItemType, ProxyType, StreamProperties, TargetType, XtreamCluster};
use shared::utils::{extract_extension_from_url, sanitize_sensitive_info, Internable, PROVIDER_SCHEME_PREFIX};
use std::borrow::Cow;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio::task;

create_bitset!(u8, M3uPlaylistIteratorFlags, MaskRedirectUrl, IncludeTypeInUrl, RewriteResource);

pub struct M3uPlaylistIterator {
    inner: LockedReceiverStream<(M3uPlaylistItem, bool)>,
}

struct UrlRewriteContext<'a> {
    base_url: &'a str,
    username: &'a str,
    password: &'a str,
}

fn build_rewritten_url(
    ctx: &UrlRewriteContext<'_>,
    source_url: &str,
    m3u_pli: &M3uPlaylistItem,
    typed: bool,
    prefix_path: &str,
    append_extension: bool,
) -> String {
    // Build URL efficiently with a single allocation using concat_string! macro
    let stream_type: &str = if typed {
        match m3u_pli.item_type {
            PlaylistItemType::Live
            | PlaylistItemType::Catchup
            | PlaylistItemType::LiveUnknown
            | PlaylistItemType::LiveHls
            | PlaylistItemType::LiveDash => "live",
            PlaylistItemType::Video | PlaylistItemType::LocalVideo => "movie",
            PlaylistItemType::Series
            | PlaylistItemType::SeriesInfo
            | PlaylistItemType::LocalSeries
            | PlaylistItemType::LocalSeriesInfo => "series",
        }
    } else {
        ""
    };

    let mut cap = ctx.base_url.len() + prefix_path.len() + ctx.username.len() + ctx.password.len() + 32; // separators and id
    if typed {
        cap += stream_type.len() + 1;
    }

    let rewritten_url = if typed {
        shared::concat_string!(
            cap = cap;
            ctx.base_url, "/", prefix_path, "/", stream_type, "/",
            ctx.username, "/", ctx.password, "/", &m3u_pli.virtual_id.to_string()
        )
    } else {
        shared::concat_string!(
            cap = cap;
            ctx.base_url, "/", prefix_path, "/",
            ctx.username, "/", ctx.password, "/", &m3u_pli.virtual_id.to_string()
        )
    };

    if append_extension {
        extract_extension_from_url(source_url)
            .map(|ext| shared::concat_string!(&rewritten_url, &ext))
            .unwrap_or(rewritten_url)
    } else {
        rewritten_url
    }
}

fn resolve_effective_source_url<'a>(
    m3u_pli: &'a M3uPlaylistItem,
    input_by_name: &HashMap<Arc<str>, Arc<crate::model::ConfigInput>>,
) -> Cow<'a, str> {
    if !m3u_pli.url.starts_with(PROVIDER_SCHEME_PREFIX) {
        return Cow::Borrowed(m3u_pli.url.as_ref());
    }

    input_by_name.get(&m3u_pli.input_name).map_or_else(
        || {
            error!(
                "Input '{}' not found while resolving provider URL '{}'",
                m3u_pli.input_name,
                sanitize_sensitive_info(&m3u_pli.url)
            );
            Cow::Borrowed(m3u_pli.url.as_ref())
        },
        |input| match input.resolve_url(&m3u_pli.url) {
            Ok(resolved) => match resolved {
                Cow::Borrowed(url) => Cow::Borrowed(url),
                Cow::Owned(url) => Cow::Owned(url),
            },
            Err(err) => {
                error!(
                    "Failed to resolve provider URL '{}' for input '{}': {}",
                    sanitize_sensitive_info(&m3u_pli.url),
                    m3u_pli.input_name,
                    sanitize_sensitive_info(&err.to_string())
                );
                Cow::Borrowed(m3u_pli.url.as_ref())
            }
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn apply_rewrite(
    mut m3u_pli: M3uPlaylistItem,
    ctx: &UrlRewriteContext<'_>,
    target_id: u16,
    encrypt_secret: &[u8; 16],
    input_by_name: &HashMap<Arc<str>, Arc<crate::model::ConfigInput>>,
    target_options: Option<&ConfigTargetOptions>,
    flags: M3uPlaylistIteratorFlagsSet,
    proxy_type: ProxyType,
) -> M3uPlaylistItem {
    let is_redirect = proxy_type.is_redirect(m3u_pli.item_type)
        || target_options.and_then(|o| o.force_redirect.as_ref()).is_some_and(|f| f.has_cluster(m3u_pli.item_type));
    let should_rewrite_urls =
        if is_redirect { flags.contains(M3uPlaylistIteratorFlags::MaskRedirectUrl) } else { true };

    let effective_source_url = resolve_effective_source_url(&m3u_pli, input_by_name);
    let catchup_rewrite = if should_rewrite_urls {
        if let Some(StreamProperties::Live(live)) = m3u_pli.additional_properties.as_ref() {
            if let Some(catchup) = live.catchup.as_ref() {
                build_m3u_catchup_rewrite(
                    encrypt_secret,
                    ctx.base_url,
                    ctx.username,
                    target_id,
                    m3u_pli.virtual_id,
                    effective_source_url.as_ref(),
                    catchup,
                )
                .ok()
                .flatten()
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    if should_rewrite_urls {
        let stream_url = build_rewritten_url(
            ctx,
            effective_source_url.as_ref(),
            &m3u_pli,
            flags.contains(M3uPlaylistIteratorFlags::IncludeTypeInUrl),
            storage_const::M3U_STREAM_PATH,
            true,
        );
        let resource_url = if flags.contains(M3uPlaylistIteratorFlags::RewriteResource) {
            let source_url = if m3u_pli.logo.is_empty() { m3u_pli.logo_small.as_ref() } else { m3u_pli.logo.as_ref() };
            Some(build_rewritten_url(
                ctx,
                source_url,
                &m3u_pli,
                false,
                storage_const::M3U_RESOURCE_PATH,
                false,
            ))
        } else {
            None
        };
        m3u_pli.t_stream_url = stream_url.intern();
        m3u_pli.t_resource_url = resource_url;
    } else {
        m3u_pli.t_stream_url = match effective_source_url {
            Cow::Borrowed(_) => Arc::clone(&m3u_pli.url),
            Cow::Owned(url) => url.intern(),
        };
        m3u_pli.t_resource_url = None;
    }

    if let Some(rewrite) = catchup_rewrite {
        m3u_pli.t_catchup_mode = Some(rewrite.mode);
        m3u_pli.t_catchup_source = Some(rewrite.source);
    }

    m3u_pli
}


#[allow(clippy::too_many_lines)]
impl M3uPlaylistIterator {
    pub async fn new(
        cfg: &AppConfig,
        target: &ConfigTarget,
        user: &ProxyUserCredentials,
    ) -> Result<Self, TuliproxError> {
        // TODO use playlist memory cache, but be aware of sorting !

        let m3u_output = target.get_m3u_output().ok_or_else(|| {
            TuliproxError::Config(format!("Unexpected failure, missing m3u target output for target {}", target.name))
        })?;
        let config = cfg.config.load();
        let target_path = ensure_target_storage_path(&config, target.name.as_str()).await?;
        let m3u_path = m3u_get_file_path_for_db(&target_path);

        let iter_lock = cfg.file_locks.read_lock(&m3u_path).await;
        let bg_lock = cfg.file_locks.read_lock(&m3u_path).await;

        let filter = user_get_bouquet_filter(&config, &user.username, None, TargetType::M3u, XtreamCluster::Live).await;
        let mut flags = M3uPlaylistIteratorFlagsSet::new();
        if m3u_output.include_type_in_url {
            flags.set(M3uPlaylistIteratorFlags::IncludeTypeInUrl);
        }
        if m3u_output.mask_redirect_url {
            flags.set(M3uPlaylistIteratorFlags::MaskRedirectUrl);
        }
        if cfg.is_reverse_proxy_resource_rewrite_enabled() {
            flags.set(M3uPlaylistIteratorFlags::RewriteResource);
        }

        let base_url = cfg.get_user_server_info(user).map(|si| si.get_base_url()).unwrap_or_default();
        let encrypt_secret = cfg.get_reverse_proxy_rewrite_secret().unwrap_or(cfg.encrypt_secret);
        let username = user.username.clone();
        let password = user.password.clone();
        let target_id = target.id;
        let proxy_type = user.proxy;
        let output_clusters = user.output_clusters;
        let target_options = target.options.clone();
        let input_by_name: HashMap<Arc<str>, Arc<crate::model::ConfigInput>> = cfg
            .sources
            .load()
            .inputs
            .iter()
            .map(|input| (Arc::clone(&input.name), Arc::clone(input)))
            .collect();

        let m3u_path = m3u_path.clone();
        let index_path = get_file_path_for_db_index(&m3u_path);
        let (tx, rx) = mpsc::channel::<(M3uPlaylistItem, bool)>(256);

        let m3u_path_for_log = m3u_path.clone();
        let index_path_for_log = index_path.clone();
        let handle = task::spawn_blocking(move || {
            let _guard = bg_lock;
            let reader = match open_playlist_reader::<u32, M3uPlaylistItem, u32>(
                &m3u_path,
                &index_path,
                Some("Sorted index error for m3u, fallback"),
            ) {
                Ok(reader) => reader,
                Err(err) => {
                    error!("Failed to open M3U playlist DB {}: {err}", m3u_path.display());
                    return;
                }
            };

            let mut pending: Option<M3uPlaylistItem> = None;
            for entry in reader {
                let item = match entry {
                    Ok((_, item)) => item,
                    Err(err) => {
                        error!("Iterator error: {err}");
                        continue;
                    }
                };

                if !output_clusters.has_cluster(item.item_type) {
                    continue;
                }

                if let Some(set) = &filter {
                    if !set.contains(item.group.as_ref()) {
                        continue;
                    }
                }

                let rewrite_ctx = UrlRewriteContext {
                    base_url: &base_url,
                    username: &username,
                    password: &password,
                };

                let item = apply_rewrite(
                    item,
                    &rewrite_ctx,
                    target_id,
                    &encrypt_secret,
                    &input_by_name,
                    target_options.as_ref(),
                    flags,
                    proxy_type,
                );

                if let Some(prev) = pending.replace(item) {
                    if tx.blocking_send((prev, true)).is_err() {
                        return;
                    }
                }
            }

            if let Some(last) = pending {
                let _ = tx.blocking_send((last, false));
            }
        });
        tokio::spawn(async move {
            if let Err(err) = handle.await {
                error!(
                    "M3U playlist iterator task failed for {} (index {}): {err}",
                    m3u_path_for_log.display(),
                    index_path_for_log.display()
                );
            }
        });

        Ok(Self {
            inner: LockedReceiverStream::new(rx, iter_lock), // Save lock inside struct
        })
    }
}

impl Stream for M3uPlaylistIterator {
    type Item = (M3uPlaylistItem, bool);

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

pub struct M3uPlaylistM3uTextIterator {
    inner: M3uPlaylistIterator,
    started: bool,
    target_options: Option<ConfigTargetOptions>,
}

impl M3uPlaylistM3uTextIterator {
    pub async fn new(
        cfg: &AppConfig,
        target: &ConfigTarget,
        user: &ProxyUserCredentials,
    ) -> Result<Self, TuliproxError> {
        Ok(Self {
            inner: M3uPlaylistIterator::new(cfg, target, user).await?,
            started: false,
            target_options: target.options.clone(),
        })
    }
}

impl Stream for M3uPlaylistM3uTextIterator {
    type Item = String;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if !self.started {
            self.started = true;
            return Poll::Ready(Some("#EXTM3U".to_string()));
        }

        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some((m3u_pli, _has_next))) => {
                let target_options = self.target_options.as_ref();
                Poll::Ready(Some(m3u_pli.to_m3u(target_options, true)))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_rewrite, M3uPlaylistIterator, M3uPlaylistIteratorFlags, M3uPlaylistIteratorFlagsSet,
        M3uPlaylistM3uTextIterator, UrlRewriteContext,
    };
    use crate::model::{ConfigInput, ConfigProvider, ProviderDnsCache};
    use crate::repository::LockedReceiverStream;
    use futures::StreamExt;
    use shared::{
        model::{M3uPlaylistItem, PlaylistItemType, ProviderUrlSelectionPolicy, ProxyType},
        utils::Internable,
    };
    use std::{
        collections::HashMap,
        sync::{atomic::AtomicUsize, Arc},
    };
    use tokio::sync::mpsc;

    fn provider_input() -> Arc<ConfigInput> {
        Arc::new(ConfigInput {
            name: "example1".intern(),
            provider_configs: Some(vec![Arc::new(ConfigProvider {
                name: "example".intern(),
                urls: vec!["http://example.com".intern()],
                provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
                current_url_index: AtomicUsize::new(0),
                dns: None,
                dns_cache: Arc::new(ProviderDnsCache::default()),
            })]),
            ..ConfigInput::default()
        })
    }

    fn m3u_item(url: &str) -> M3uPlaylistItem {
        M3uPlaylistItem {
            virtual_id: 813_294,
            provider_id: "813294".intern(),
            name: "France 4K".intern(),
            chno: 0,
            logo: "".intern(),
            logo_small: "".intern(),
            group: "FR| FRANCE 4K".intern(),
            title: "France 4K".intern(),
            parent_code: "".intern(),
            audio_track: "".intern(),
            time_shift: "".intern(),
            rec: "".intern(),
            url: url.intern(),
            epg_channel_id: None,
            input_name: "example1".intern(),
            item_type: PlaylistItemType::Live,
            t_stream_url: "".intern(),
            t_resource_url: None,
            t_catchup_source: None,
            t_catchup_mode: None,
            source_ordinal: 0,
            additional_properties: None,
        }
    }

    #[test]
    fn redirect_without_mask_resolves_provider_scheme_url() {
        let input = provider_input();
        let input_by_name = HashMap::from([(Arc::clone(&input.name), input)]);
        let ctx = UrlRewriteContext {
            base_url: "https://example.com",
            username: "user",
            password: "pass",
        };
        let rewritten = apply_rewrite(
            m3u_item("provider://example/live/user/pass/813294.ts"),
            &ctx,
            1,
            &[7u8; 16],
            &input_by_name,
            None,
            M3uPlaylistIteratorFlagsSet::new(),
            ProxyType::Redirect,
        );

        assert_eq!(rewritten.t_stream_url.as_ref(), "http://example.com/live/user/pass/813294.ts");
    }

    #[test]
    fn redirect_without_mask_keeps_regular_url() {
        let input_by_name = HashMap::new();
        let ctx = UrlRewriteContext {
            base_url: "https://example.com",
            username: "user",
            password: "pass",
        };
        let rewritten = apply_rewrite(
            m3u_item("http://example.com/live/user/pass/813294.ts"),
            &ctx,
            1,
            &[7u8; 16],
            &input_by_name,
            None,
            M3uPlaylistIteratorFlagsSet::new(),
            ProxyType::Redirect,
        );

        assert_eq!(rewritten.t_stream_url.as_ref(), "http://example.com/live/user/pass/813294.ts");
    }

    #[test]
    fn redirect_without_mask_preserves_provider_scheme_url_when_input_is_missing() {
        let input_by_name = HashMap::new();
        let ctx = UrlRewriteContext {
            base_url: "https://example.com",
            username: "user",
            password: "pass",
        };
        let rewritten = apply_rewrite(
            m3u_item("provider://example/live/user/pass/813294.ts"),
            &ctx,
            1,
            &[7u8; 16],
            &input_by_name,
            None,
            M3uPlaylistIteratorFlagsSet::new(),
            ProxyType::Redirect,
        );

        assert_eq!(rewritten.t_stream_url.as_ref(), "provider://example/live/user/pass/813294.ts");
    }

    #[test]
    fn masked_redirect_uses_resolved_provider_source_for_rewritten_url() {
        let input = provider_input();
        let input_by_name = HashMap::from([(Arc::clone(&input.name), input)]);
        let mut flags = M3uPlaylistIteratorFlagsSet::new();
        flags.set(M3uPlaylistIteratorFlags::MaskRedirectUrl);
        let ctx = UrlRewriteContext {
            base_url: "https://example.com",
            username: "user",
            password: "pass",
        };
        let rewritten = apply_rewrite(
            m3u_item("provider://example/live/user/pass/813294.ts"),
            &ctx,
            1,
            &[7u8; 16],
            &input_by_name,
            None,
            flags,
            ProxyType::Redirect,
        );

        assert_eq!(rewritten.t_stream_url.as_ref(), "https://example.com/m3u-stream/user/pass/813294.ts");
    }

    // Regression lock: the streaming M3U response used by `m3u_api` must emit a
    // bare `#EXTM3U` header line, never a proxy-rewritten `url-tvg` from the
    // source playlist. The parser does not capture the source's `url-tvg`
    // attribute (`M3uPlaylistItem` has no field for it), and the writer must
    // therefore emit the bare marker regardless of what the upstream feed
    // declared. The inner channel is closed immediately so the iterator yields
    // no item lines, which lets the assertion focus on the header line only.
    #[tokio::test]
    async fn m3u_text_iterator_emits_bare_extm3u_header_without_proxying_source_url_tvg() {
        let (tx, rx) = mpsc::channel::<(M3uPlaylistItem, bool)>(1);
        drop(tx);
        let inner_iter = M3uPlaylistIterator { inner: LockedReceiverStream::new_empty(rx) };
        let mut text_iter = M3uPlaylistM3uTextIterator { inner: inner_iter, started: false, target_options: None };

        let first = text_iter.next().await;
        assert_eq!(first.as_deref(), Some("#EXTM3U"), "EXTM3U header must be bare, never contain url-tvg");

        let second = text_iter.next().await;
        assert_eq!(second, None, "inner channel is closed, so no item lines should follow");
    }
}
