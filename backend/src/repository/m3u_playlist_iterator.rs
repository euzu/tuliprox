use shared::error::info_err;
use shared::error::{TuliproxError};
use crate::model::{AppConfig, ProxyUserCredentials};
use crate::model::{ConfigTarget};
use shared::model::{ConfigTargetOptions, M3uPlaylistItem, PlaylistItemType, ProxyType, TargetType, XtreamCluster};
use crate::repository::bplustree::{BPlusTreeQuery, PlaylistIteratorReader};
use crate::repository::m3u_repository::m3u_get_file_path_for_db;
use crate::repository::storage::{ensure_target_storage_path, get_file_path_for_db_index};
use crate::repository::storage_const;
use crate::repository::user_repository::user_get_bouquet_filter;
use crate::utils::FileReadGuard;
use std::collections::HashSet;
use std::iter::Peekable;
use log::error;
// concat_string! macro from shared utils is used for efficient String building

#[allow(clippy::struct_excessive_bools)]
pub struct M3uPlaylistIterator {
    reader: Peekable<PlaylistIteratorReader<M3uPlaylistItem>>,
    base_url: String,
    username: String,
    password: String,
    target_options: Option<ConfigTargetOptions>,
    mask_redirect_url: bool,
    include_type_in_url: bool,
    rewrite_resource: bool,
    proxy_type: ProxyType,
    filter: Option<HashSet<String>>,
    lookup_item: Option<(M3uPlaylistItem, bool)>,
    _file_lock: FileReadGuard,
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
        let target_path = ensure_target_storage_path(&config, target.name.as_str())?;
        let m3u_path = m3u_get_file_path_for_db(&target_path);

        let file_lock = cfg.file_locks.read_lock(&m3u_path).await;

        let index_path = get_file_path_for_db_index(&m3u_path);
        let reader = if index_path.exists() {
             let query = BPlusTreeQuery::<u32, M3uPlaylistItem>::try_new(&m3u_path)
                 .map_err(|err| info_err!("Could not open BPlusTreeQuery {m3u_path:?} - {err}"))?;
             match query.disk_iter_sorted() {
                 Ok(reader) => PlaylistIteratorReader::Sorted(reader),
                 Err(err) => {
                     error!("Sorted index error for m3u, fallback: {err}");
                     let query = BPlusTreeQuery::<u32, M3uPlaylistItem>::try_new(&m3u_path)
                         .map_err(|err| info_err!("Could not open BPlusTreeQuery {m3u_path:?} - {err}"))?;
                     PlaylistIteratorReader::Unsorted(query.disk_iter())
                 }
             }
        } else {
             let query = BPlusTreeQuery::<u32, M3uPlaylistItem>::try_new(&m3u_path)
                 .map_err(|err| info_err!("Could not open BPlusTreeQuery {m3u_path:?} - {err}"))?;
             PlaylistIteratorReader::Unsorted(query.disk_iter())
        }.peekable();

        let filter = user_get_bouquet_filter(&config, &user.username, None, TargetType::M3u, XtreamCluster::Live).await;

        let server_info = cfg.get_user_server_info(user);
        Ok(Self {
            reader,
            base_url: server_info.get_base_url(),
            username: user.username.clone(),
            password: user.password.clone(),
            target_options: target.options.clone(),
            include_type_in_url: m3u_output.include_type_in_url,
            mask_redirect_url: m3u_output.mask_redirect_url,
            filter,
            proxy_type: user.proxy,
            _file_lock: file_lock, // Save lock inside struct
            rewrite_resource: cfg.is_reverse_proxy_resource_rewrite_enabled(),
            lookup_item: None,
        })
    }

    fn get_rewritten_url(&self, m3u_pli: &M3uPlaylistItem, typed: bool, prefix_path: &str) -> String {
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

        let mut cap = self.base_url.len()
            + prefix_path.len()
            + self.username.len()
            + self.password.len()
            + 32; // separators and id
        if typed { cap += stream_type.len() + 1; }

        if typed {
            shared::concat_string!(
                cap = cap;
                &self.base_url, "/", prefix_path, "/", stream_type, "/",
                &self.username, "/", &self.password, "/", &m3u_pli.virtual_id.to_string()
            )
        } else {
            shared::concat_string!(
                cap = cap;
                &self.base_url, "/", prefix_path, "/",
                &self.username, "/", &self.password, "/", &m3u_pli.virtual_id.to_string()
            )
        }
    }

    fn get_stream_url(&self, m3u_pli: &M3uPlaylistItem, typed: bool) -> String {
        self.get_rewritten_url(m3u_pli, typed, storage_const::M3U_STREAM_PATH)
    }
    fn get_resource_url(&self, m3u_pli: &M3uPlaylistItem) -> String {
        self.get_rewritten_url(m3u_pli, false, storage_const::M3U_RESOURCE_PATH)
    }

    fn find_next_matching(reader: &mut Peekable<PlaylistIteratorReader<M3uPlaylistItem>>, set: &HashSet<String>) -> Option<M3uPlaylistItem> {
        loop {
            match reader.next() {
                Some(Ok((_, item))) => {
                    if set.contains(&*item.group) {
                        return Some(item);
                    }
                }
                Some(Err(e)) => {
                    error!("Iterator error: {e}");
                    return None;
                }
                None => return None,
            }
        }
    }

    fn get_next(&mut self) -> Option<(M3uPlaylistItem, bool)> {
        let entry = if let Some(set) = &self.filter {
            if let Some((current_item, _)) = self.lookup_item.take() {
                // Avoid cloning strings while filtering
                let next_valid = Self::find_next_matching(&mut self.reader, set);
                self.lookup_item = next_valid.map(|v| (v, true)); // has_next handled by iterator usually, but here we just need the item
                let has_next = self.lookup_item.is_some();
                Some((current_item, has_next))
            } else {
                let current_item = Self::find_next_matching(&mut self.reader, set);
                if let Some(item) = current_item {
                    self.lookup_item = Self::find_next_matching(&mut self.reader, set).map(|item| (item, true));
                    let has_next = self.lookup_item.is_some();
                    Some((item, has_next))
                } else {
                    None
                }
            }
        } else {
            match self.reader.next() {
                Some(Ok((_, v))) => Some((v, self.reader.peek().is_some())),
                Some(Err(e)) => {
                    error!("Iterator error: {e}");
                    None
                }
                None => None,
            }
        };

        // TODO hls and unknown reverse proxy
        entry.map(|(mut m3u_pli, has_next)| {
            let is_redirect = self.proxy_type.is_redirect(m3u_pli.item_type)
                || self
                    .target_options
                    .as_ref()
                    .and_then(|o| o.force_redirect.as_ref())
                    .is_some_and(|f| f.has_cluster(m3u_pli.item_type));
            let should_rewrite_urls = if is_redirect { self.mask_redirect_url } else { true };

            if should_rewrite_urls {
                let stream_url = self.get_stream_url(&m3u_pli, self.include_type_in_url);
                let resource_url = if self.rewrite_resource {
                    Some(self.get_resource_url(&m3u_pli))
                } else {
                    None
                };
                m3u_pli.t_stream_url = stream_url;
                m3u_pli.t_resource_url = resource_url;
            } else {
                // Keep original URL (clone required because target field is distinct)
                m3u_pli.t_stream_url = m3u_pli.url.clone();
                m3u_pli.t_resource_url = None;
            }

            (m3u_pli, has_next)
        })
    }
}

impl Iterator for M3uPlaylistIterator {
    type Item = (M3uPlaylistItem, bool);

    fn next(&mut self) -> Option<Self::Item> {
        self.get_next()
    }
}

pub struct M3uPlaylistM3uTextIterator {
    inner: M3uPlaylistIterator,
    started: bool,
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
        })
    }
}

impl Iterator for M3uPlaylistM3uTextIterator {
    type Item = String;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.started {
            self.started = true;
            return Some("#EXTM3U".to_string());
        }

        // TODO hls and unknown reverse proxy
        self.inner.get_next().map(|(m3u_pli, _has_next)| {
            let target_options = self.inner.target_options.as_ref();
            m3u_pli.to_m3u(target_options, true)
        })
    }
}
