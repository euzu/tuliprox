use shared::error::info_err;
use shared::error::{TuliproxError, TuliproxErrorKind};
use crate::model::{AppConfig, ProxyUserCredentials};
use crate::model::{ConfigTarget};
use shared::model::{ConfigTargetOptions, M3uPlaylistItem, PlaylistItemType, ProxyType, TargetType, XtreamCluster};
use crate::repository::indexed_document::IndexedDocumentIterator;
use crate::repository::m3u_repository::m3u_get_file_paths;
use crate::repository::storage::ensure_target_storage_path;
use crate::repository::storage_const;
use crate::repository::user_repository::user_get_bouquet_filter;
use crate::utils::FileReadGuard;
use std::collections::HashSet;

#[allow(clippy::struct_excessive_bools)]
pub struct M3uPlaylistIterator {
    reader: IndexedDocumentIterator<u32, M3uPlaylistItem>,
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
        let m3u_output = target.get_m3u_output().ok_or_else(|| info_err!(format!("Unexpected failure, missing m3u target output for target {}",  target.name)))?;
        let config = cfg.config.load();
        let target_path = ensure_target_storage_path(&config, target.name.as_str())?;
        let (m3u_path, idx_path) = m3u_get_file_paths(&target_path);

        let file_lock = cfg.file_locks.read_lock(&m3u_path).await;

        let reader =
            IndexedDocumentIterator::<u32, M3uPlaylistItem>::new(&m3u_path, &idx_path)
                .map_err(|err| info_err!(format!("Could not deserialize file {m3u_path:?} - {err}")))?;

        let filter = user_get_bouquet_filter(&config, &user.username, None, TargetType::M3u, XtreamCluster::Live).await;

        let server_info = cfg.get_user_server_info(user);
        Ok(Self {
            reader,
            base_url: server_info.get_base_url(),
            username: user.username.to_string(),
            password: user.password.to_string(),
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
        if typed {
            let stream_type = match m3u_pli.item_type {
                PlaylistItemType::Live
                | PlaylistItemType::Catchup
                | PlaylistItemType::LiveUnknown
                | PlaylistItemType::LiveHls
                | PlaylistItemType::LiveDash => "live",
                PlaylistItemType::Video => "movie",
                PlaylistItemType::Series
                | PlaylistItemType::SeriesInfo => "series",
            };
            format!("{}/{prefix_path}/{stream_type}/{}/{}/{}",
                    &self.base_url,
                    &self.username,
                    &self.password,
                    m3u_pli.virtual_id
            )
        } else {
            format!("{}/{prefix_path}/{}/{}/{}",
                    &self.base_url, &self.username, &self.password, m3u_pli.virtual_id
            )
        }
    }

    fn get_stream_url(&self, m3u_pli: &M3uPlaylistItem, typed: bool) -> String {
        self.get_rewritten_url(m3u_pli, typed, storage_const::M3U_STREAM_PATH)
    }
    fn get_resource_url(&self, m3u_pli: &M3uPlaylistItem) -> String {
        self.get_rewritten_url(m3u_pli, false, storage_const::M3U_RESOURCE_PATH)
    }

    fn get_next(&mut self) -> Option<(M3uPlaylistItem, bool)> {
        let entry = if let Some(set) = &self.filter {
            if let Some((current_item, _)) = self.lookup_item.take() {
                let next_valid = self.reader.find(|(pli, _)| set.contains(&pli.group.to_string()));
                self.lookup_item = next_valid;
                let has_next = self.lookup_item.is_some();
                Some((current_item, has_next))
            } else {
                let current_item = self.reader.find(|(item, _)| set.contains(&item.group.to_string()));
                if let Some((item, _)) = current_item {
                    self.lookup_item = self.reader.find(|(item, _)| set.contains(&item.group.to_string()));
                    let has_next = self.lookup_item.is_some();
                    Some((item, has_next))
                } else {
                    None
                }
            }
        } else {
            self.reader.next()
        };

        // TODO hls and unknown reverse proxy
        entry.map(|(mut m3u_pli, has_next)| {
            let is_redirect = self.proxy_type.is_redirect(m3u_pli.item_type) || self.target_options.as_ref().and_then(|o| o.force_redirect.as_ref()).is_some_and(|f| f.has_cluster(m3u_pli.item_type));
            let should_rewrite_urls = if is_redirect { self.mask_redirect_url} else { true };
            let rewrite_urls = if should_rewrite_urls {
                Some((self.get_stream_url(&m3u_pli, self.include_type_in_url), if self.rewrite_resource { Some(self.get_resource_url(&m3u_pli)) } else { None }))
            } else {
                None
            };
            let url = m3u_pli.url.to_string();
            let (stream_url, resource_url) = rewrite_urls
                .map_or_else(|| (url, None), |(su, ru)| (su, ru.as_ref().map(String::to_string)));

            m3u_pli.t_stream_url = stream_url.to_string();
            m3u_pli.t_resource_url = resource_url.map(|s| s.to_string());
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
