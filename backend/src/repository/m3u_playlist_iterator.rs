use crate::model::ConfigTarget;
use crate::model::{AppConfig, ProxyUserCredentials};
use crate::iptv::m3u::{build_m3u_catchup_rewrite, flussonic_proxy_live_file};
use crate::repository::m3u_get_file_path_for_db;
use crate::repository::storage_const;
use crate::repository::user_get_bouquet_filter;
use crate::repository::{ensure_target_storage_path, get_file_path_for_db_index};
use crate::repository::{open_playlist_reader, LockedReceiverStream};
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
    inner: LockedReceiverStream<Result<(M3uPlaylistItem, bool), TuliproxError>>,
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
            .map(|ext| shared::concat_string!(&rewritten_url, ext))
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
        let flussonic_live_file = m3u_pli.additional_properties.as_ref().and_then(|properties| match properties {
            StreamProperties::Live(live) => live
                .catchup
                .as_ref()
                .and_then(shared::model::CatchupProperties::native_flussonic_player_mode)
                .map(flussonic_proxy_live_file),
            _ => None,
        });
        let stream_url = flussonic_live_file.map_or_else(
            || {
                build_rewritten_url(
                    ctx,
                    effective_source_url.as_ref(),
                    &m3u_pli,
                    flags.contains(M3uPlaylistIteratorFlags::IncludeTypeInUrl),
                    storage_const::M3U_STREAM_PATH,
                    true,
                )
            },
            |live_file| {
                let base = build_rewritten_url(
                    ctx,
                    effective_source_url.as_ref(),
                    &m3u_pli,
                    flags.contains(M3uPlaylistIteratorFlags::IncludeTypeInUrl),
                    storage_const::M3U_STREAM_PATH,
                    false,
                );
                shared::concat_string!(&base, "/", live_file)
            },
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
    if should_rewrite_urls {
        m3u_pli.upstream_user_agent = None;
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
        let (tx, rx) = mpsc::channel::<Result<(M3uPlaylistItem, bool), TuliproxError>>(256);

        let m3u_path_for_log = m3u_path.clone();
        let index_path_for_log = index_path.clone();
        let join_error_tx = tx.clone();
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
                    let _ = tx.blocking_send(Err(err));
                    return;
                }
            };

            let mut pending: Option<M3uPlaylistItem> = None;
            for entry in reader {
                let item = match entry {
                    Ok((_, item)) => item,
                    Err(err) => {
                        error!("Skipping unreadable M3U playlist entry: {err}");
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
                    if tx.blocking_send(Ok((prev, true))).is_err() {
                        return;
                    }
                }
            }

            if let Some(last) = pending {
                let _ = tx.blocking_send(Ok((last, false)));
            }
        });
        tokio::spawn(async move {
            if let Err(err) = handle.await {
                error!(
                    "M3U playlist iterator task failed for {} (index {}): {err}",
                    m3u_path_for_log.display(),
                    index_path_for_log.display()
                );
                let _ = join_error_tx
                    .send(Err(TuliproxError::RepositoryM3u(format!(
                        "M3U playlist iterator task failed for {}: {err}",
                        m3u_path_for_log.display()
                    ))))
                    .await;
            }
        });

        Ok(Self {
            inner: LockedReceiverStream::new(rx, iter_lock), // Save lock inside struct
        })
    }
}

impl Stream for M3uPlaylistIterator {
    type Item = Result<(M3uPlaylistItem, bool), TuliproxError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

fn build_proxy_xmltv_url_tvg(base_url: &str, username: &str, password: &str) -> Option<String> {
    let base = base_url.trim_end_matches('/');
    if base.is_empty() || username.is_empty() || password.is_empty() {
        return None;
    }
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("username", username)
        .append_pair("password", password)
        .finish();
    Some(format!("{base}/xmltv.php?{query}"))
}

pub struct M3uPlaylistM3uTextIterator {
    inner: M3uPlaylistIterator,
    started: bool,
    target_options: Option<ConfigTargetOptions>,
    url_tvg: Option<String>,
}

impl M3uPlaylistM3uTextIterator {
    pub async fn new(
        cfg: &AppConfig,
        target: &ConfigTarget,
        user: &ProxyUserCredentials,
    ) -> Result<Self, TuliproxError> {
        let base_url = cfg.get_user_server_info(user).map(|server| server.get_base_url()).unwrap_or_default();
        Ok(Self {
            inner: M3uPlaylistIterator::new(cfg, target, user).await?,
            started: false,
            target_options: target.options.clone(),
            url_tvg: build_proxy_xmltv_url_tvg(&base_url, &user.username, &user.password),
        })
    }
}

impl Stream for M3uPlaylistM3uTextIterator {
    type Item = Result<String, String>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if !self.started {
            self.started = true;
            let header = self.url_tvg.as_ref().map_or_else(
                || "#EXTM3U".to_string(),
                |url| format!("#EXTM3U url-tvg=\"{url}\" x-tvg-url=\"{url}\""),
            );
            return Poll::Ready(Some(Ok(header)));
        }

        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok((m3u_pli, _has_next)))) => {
                let target_options = self.target_options.as_ref();
                Poll::Ready(Some(Ok(m3u_pli.to_m3u(target_options, true))))
            }
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(error.to_string()))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_rewrite, build_proxy_xmltv_url_tvg, M3uPlaylistIterator, M3uPlaylistIteratorFlags,
        M3uPlaylistIteratorFlagsSet, M3uPlaylistM3uTextIterator, UrlRewriteContext,
    };
    use crate::model::{ConfigInput, ConfigProvider, ProviderDnsCache};
    use crate::repository::LockedReceiverStream;
    use futures::StreamExt;
    use shared::{
        model::{
            CatchupProperties, LiveStreamProperties, M3uPlaylistItem, PlaylistItemType, ProviderUrlSelectionPolicy,
            ProxyType, StreamProperties,
        },
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
            input_stream_id: "813294".intern(),
            upstream_user_agent: None,
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
    fn upstream_user_agent_is_only_exposed_for_direct_provider_urls() {
        let input_by_name = HashMap::new();
        let ctx = UrlRewriteContext {
            base_url: "https://example.com",
            username: "user",
            password: "pass",
        };
        let mut direct_item = m3u_item("http://example.com/live/user/pass/813294.ts");
        direct_item.upstream_user_agent = Some("Provider-UA".intern());

        let direct = apply_rewrite(
            direct_item.clone(),
            &ctx,
            1,
            &[7u8; 16],
            &input_by_name,
            None,
            M3uPlaylistIteratorFlagsSet::new(),
            ProxyType::Redirect,
        );
        let reverse = apply_rewrite(
            direct_item,
            &ctx,
            1,
            &[7u8; 16],
            &input_by_name,
            None,
            M3uPlaylistIteratorFlagsSet::new(),
            ProxyType::Reverse(None),
        );
        let mut masked_flags = M3uPlaylistIteratorFlagsSet::new();
        masked_flags.set(M3uPlaylistIteratorFlags::MaskRedirectUrl);
        let masked_redirect = apply_rewrite(
            direct.clone(),
            &ctx,
            1,
            &[7u8; 16],
            &input_by_name,
            None,
            masked_flags,
            ProxyType::Redirect,
        );

        assert!(direct.to_m3u(None, false).contains("#EXTVLCOPT:http-user-agent=Provider-UA"));
        assert!(!reverse.to_m3u(None, true).contains("#EXTVLCOPT:http-user-agent"));
        assert!(!masked_redirect.to_m3u(None, true).contains("#EXTVLCOPT:http-user-agent"));
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

    #[test]
    fn explicit_flussonic_modes_use_native_hls_and_ts_live_paths() {
        let ctx = UrlRewriteContext {
            base_url: "https://example.com",
            username: "user",
            password: "pass",
        };
        for (mode, source_url, expected_suffix) in [
            ("flussonic", "http://provider/ch/index.m3u8", "/813294/index.m3u8"),
            ("flussonic-ts", "http://provider/ch/channel.ts", "/813294/mpegts"),
        ] {
            let mut item = m3u_item(source_url);
            item.additional_properties = Some(StreamProperties::Live(Box::new(LiveStreamProperties {
                catchup: Some(CatchupProperties {
                    catchup_type: Some(mode.intern()),
                    ..CatchupProperties::default()
                }),
                ..LiveStreamProperties::default()
            })));
            let rewritten = apply_rewrite(
                item,
                &ctx,
                1,
                &[7u8; 16],
                &HashMap::new(),
                None,
                M3uPlaylistIteratorFlagsSet::new(),
                ProxyType::Reverse(None),
            );
            assert!(rewritten.t_stream_url.ends_with(expected_suffix));
            assert!(rewritten.t_catchup_source.is_none());
        }
    }

    #[test]
    fn append_mode_keeps_existing_flat_live_path() {
        let mut item = m3u_item("http://provider/ch/index.m3u8");
        item.additional_properties = Some(StreamProperties::Live(Box::new(LiveStreamProperties {
            catchup: Some(CatchupProperties {
                mode: Some("append".intern()),
                source: Some("?utcstart=${timestamp}&offset=-${offset}".intern()),
                ..CatchupProperties::default()
            }),
            ..LiveStreamProperties::default()
        })));
        let rewritten = apply_rewrite(
            item,
            &UrlRewriteContext {
                base_url: "https://example.com",
                username: "user",
                password: "pass",
            },
            1,
            &[7u8; 16],
            &HashMap::new(),
            None,
            M3uPlaylistIteratorFlagsSet::new(),
            ProxyType::Reverse(None),
        );

        assert_eq!(rewritten.t_stream_url.as_ref(), "https://example.com/m3u-stream/user/pass/813294.m3u8");
        assert!(rewritten.t_catchup_source.is_some());
    }

    #[test]
    fn proxy_xmltv_url_uses_user_server_path_and_encoded_credentials() {
        assert_eq!(
            build_proxy_xmltv_url_tvg("https://proxy.example/iptv/", "u ser", "p&ss").as_deref(),
            Some("https://proxy.example/iptv/xmltv.php?username=u+ser&password=p%26ss")
        );
        assert_eq!(build_proxy_xmltv_url_tvg("", "user", "pass"), None);
    }

    // Regression lock: never forward the provider's source `url-tvg`. Without
    // configured Tuliprox server information, the header stays bare.
    #[tokio::test]
    async fn m3u_text_iterator_emits_bare_extm3u_header_without_proxying_source_url_tvg() {
        let (tx, rx) = mpsc::channel::<Result<(M3uPlaylistItem, bool), shared::error::TuliproxError>>(1);
        drop(tx);
        let inner_iter = M3uPlaylistIterator { inner: LockedReceiverStream::new_empty(rx) };
        let mut text_iter = M3uPlaylistM3uTextIterator {
            inner: inner_iter,
            started: false,
            target_options: None,
            url_tvg: None,
        };

        let first = text_iter.next().await;
        assert_eq!(first.as_ref().and_then(|line| line.as_deref().ok()), Some("#EXTM3U"), "EXTM3U header must be bare, never contain url-tvg");

        let second = text_iter.next().await;
        assert_eq!(second, None, "inner channel is closed, so no item lines should follow");
    }

    #[tokio::test]
    async fn m3u_text_iterator_emits_proxy_xmltv_header_when_configured() {
        let (tx, rx) = mpsc::channel::<Result<(M3uPlaylistItem, bool), shared::error::TuliproxError>>(1);
        drop(tx);
        let inner_iter = M3uPlaylistIterator { inner: LockedReceiverStream::new_empty(rx) };
        let mut text_iter = M3uPlaylistM3uTextIterator {
            inner: inner_iter,
            started: false,
            target_options: None,
            url_tvg: Some("https://proxy.example/xmltv.php?username=user&password=pass".to_string()),
        };

        let first = text_iter.next().await;
        assert_eq!(
            first.as_ref().and_then(|line| line.as_deref().ok()),
            Some(
                "#EXTM3U url-tvg=\"https://proxy.example/xmltv.php?username=user&password=pass\" x-tvg-url=\"https://proxy.example/xmltv.php?username=user&password=pass\""
            )
        );
    }

    #[tokio::test]
    async fn iterator_forwards_one_storage_error_then_ends() {
        let (tx, rx) = mpsc::channel(2);
        assert!(tx.send(Ok((m3u_item("http://example.test/live.ts"), true))).await.is_ok());
        assert!(tx.send(Err(shared::error::TuliproxError::RepositoryM3u("corrupt page".into()))).await.is_ok());
        drop(tx);

        let mut iterator = M3uPlaylistIterator { inner: LockedReceiverStream::new_empty(rx) };
        assert!(iterator.next().await.is_some_and(|entry| entry.is_ok()));
        assert!(iterator.next().await.is_some_and(|entry| entry.is_err()));
        assert!(iterator.next().await.is_none());
    }
}
