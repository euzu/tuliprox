use shared::error::info_err;
use shared::error::TuliproxError;
use crate::model::{AppConfig, ProxyUserCredentials};
use crate::model::ConfigTarget;
use shared::create_bitset;
use shared::model::{ConfigTargetOptions, M3uPlaylistItem, PlaylistItemType, ProxyType, TargetType, XtreamCluster};
use crate::repository::{LockedReceiverStream, open_playlist_reader};
use crate::repository::m3u_get_file_path_for_db;
use crate::repository::{ensure_target_storage_path, get_file_path_for_db_index};
use crate::repository::storage_const;
use crate::repository::user_get_bouquet_filter;
use futures::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use log::error;
use shared::utils::{extract_extension_from_url, Internable};
use tokio::sync::mpsc;
use tokio::task;

create_bitset!(
    u8,
    M3uPlaylistIteratorFlags,
    MaskRedirectUrl,
    IncludeTypeInUrl,
    RewriteResource
);


pub struct M3uPlaylistIterator {
    inner: LockedReceiverStream<(M3uPlaylistItem, bool)>,
}

fn build_rewritten_url(
    base_url: &str,
    username: &str,
    password: &str,
    source_url: &str,
    m3u_pli: &M3uPlaylistItem,
    typed: bool,
    prefix_path: &str,
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
            PlaylistItemType::Series | PlaylistItemType::SeriesInfo | PlaylistItemType::LocalSeries | PlaylistItemType::LocalSeriesInfo => "series",
        }
    } else {
        ""
    };

    let mut cap = base_url.len()
        + prefix_path.len()
        + username.len()
        + password.len()
        + 32; // separators and id
    if typed { cap += stream_type.len() + 1; }

    let rewritten_url = if typed {
        shared::concat_string!(
                cap = cap;
                base_url, "/", prefix_path, "/", stream_type, "/",
                username, "/", password, "/", &m3u_pli.virtual_id.to_string()
            )
    } else {
        shared::concat_string!(
                cap = cap;
                base_url, "/", prefix_path, "/",
                username, "/", password, "/", &m3u_pli.virtual_id.to_string()
            )
    };

    extract_extension_from_url(source_url)
        .map(|ext| shared::concat_string!(&rewritten_url, &ext))
        .unwrap_or(rewritten_url)
}

fn apply_rewrite(
    mut m3u_pli: M3uPlaylistItem,
    base_url: &str,
    username: &str,
    password: &str,
    target_options: Option<&ConfigTargetOptions>,
    flags: M3uPlaylistIteratorFlagsSet,
    proxy_type: ProxyType,
) -> M3uPlaylistItem {
    let is_redirect = proxy_type.is_redirect(m3u_pli.item_type)
        || target_options
            .and_then(|o| o.force_redirect.as_ref())
            .is_some_and(|f| f.has_cluster(m3u_pli.item_type));
    let should_rewrite_urls = if is_redirect {
        flags.contains(M3uPlaylistIteratorFlags::MaskRedirectUrl)
    } else {
        true
    };

    if should_rewrite_urls {
        let stream_url = build_rewritten_url(
            base_url,
            username,
            password,
            m3u_pli.url.as_ref(),
            &m3u_pli,
            flags.contains(M3uPlaylistIteratorFlags::IncludeTypeInUrl),
            storage_const::M3U_STREAM_PATH,
        );
        let resource_url = if flags.contains(M3uPlaylistIteratorFlags::RewriteResource) {
            let source_url = if m3u_pli.logo.is_empty() {
                m3u_pli.logo_small.as_ref()
            } else {
                m3u_pli.logo.as_ref()
            };
            Some(build_rewritten_url(
                base_url,
                username,
                password,
                source_url,
                &m3u_pli,
                false,
                storage_const::M3U_RESOURCE_PATH,
            ))
        } else {
            None
        };
        m3u_pli.t_stream_url = stream_url.intern();
        m3u_pli.t_resource_url = resource_url;
    } else {
        // Keep original URL (clone required because target field is distinct)
        m3u_pli.t_stream_url = m3u_pli.url.clone();
        m3u_pli.t_resource_url = None;
    }

    m3u_pli
}


impl M3uPlaylistIterator {
    pub async fn new(
        cfg: &AppConfig,
        target: &ConfigTarget,
        user: &ProxyUserCredentials,
    ) -> Result<Self, TuliproxError> {

        // TODO use playlist memory cache, but be aware of sorting !

        let m3u_output = target.get_m3u_output().ok_or_else(|| info_err!("Unexpected failure, missing m3u target output for target {}",  target.name))?;
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

        let server_info = cfg.get_user_server_info(user);
        let base_url = server_info.get_base_url();
        let username = user.username.clone();
        let password = user.password.clone();
        let proxy_type = user.proxy;
        let target_options = target.options.clone();

        let m3u_path = m3u_path.clone();
        let index_path = get_file_path_for_db_index(&m3u_path);
        let (tx, rx) = mpsc::channel::<(M3uPlaylistItem, bool)>(256);

        task::spawn_blocking(move || {
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
                        break;
                    }
                };

                if let Some(set) = &filter {
                    if !set.contains(item.group.as_ref()) {
                        continue;
                    }
                }

                let item = apply_rewrite(
                    item,
                    &base_url,
                    &username,
                    &password,
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
