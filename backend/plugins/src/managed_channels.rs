//! Per-owner, per-input managed live channel snapshots. Provider caches are never written here.
use crate::{
    convert::item_type_from_wit,
    dispatch::{count, HookDispatcher},
    host_fns,
    registry::{sha256_hex, LoadedPlugin, PluginState},
    runtime::{call_with_deadline, CallError},
    settings::redact,
};
use serde::{Deserialize, Serialize};
use shared::{
    model::{
        HookCtx, PlaylistItem, PlaylistItemHeader, PluginAbort, PluginHook, PluginHookStats, PluginRunReport,
        XtreamCluster,
    },
    utils::Internable,
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Instant,
};
use tokio::{io::AsyncReadExt, sync::Mutex as AsyncMutex};
use tuliprox_core::utils::write_file_atomic;
use tuliprox_plugin_api::bindings::{
    exports::tuliprox::plugin::managed_channel_hooks::ManagedChannel,
    tuliprox::plugin::types::{Cluster, ItemType},
};

const MAX_CHANNELS: usize = 10_000;
const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
const HOOK: PluginHook = PluginHook::ManagedChannels;

#[derive(Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    owner: String,
    input: String,
    items: Vec<PlaylistItem>,
}

type SnapshotLocks = HashMap<PathBuf, Arc<AsyncMutex<()>>>;

pub(crate) struct ManagedSnapshots {
    root: PathBuf,
    locks: Mutex<SnapshotLocks>,
}

impl ManagedSnapshots {
    pub(crate) fn new(root: PathBuf) -> Self { Self { root, locks: Mutex::new(HashMap::new()) } }

    fn path(&self, owner: &str, input: &str) -> PathBuf {
        self.root.join(owner).join(format!("{}.json", sha256_hex(input.as_bytes())))
    }

    fn lock_for(&self, path: &Path) -> Arc<AsyncMutex<()>> {
        Arc::clone(self.locks.lock().unwrap_or_else(PoisonError::into_inner).entry(path.to_path_buf()).or_default())
    }

    pub(crate) async fn reconcile(
        &self,
        dispatcher: &HookDispatcher<'_>,
        ctx: &HookCtx<'_>,
        report: &mut PluginRunReport,
    ) -> Result<Vec<PlaylistItem>, PluginAbort> {
        let Some(input) = ctx.input else { return Ok(Vec::new()) };
        let mut result = Vec::new();
        // Include failed breakers for fallback only. Disabled, unapproved and removed owners
        // contribute nothing; their on-disk snapshots stay available for a later re-enable.
        for record in dispatcher.records() {
            let Some(loaded) = record.loaded.as_ref().filter(|loaded| loaded.hooks.contains(&HOOK)) else { continue };
            if !loaded.env.capabilities.playlist_write
                || !matches!(record.state(), PluginState::Enabled | PluginState::Failed)
            {
                continue;
            }
            let path = self.path(&record.id, input);
            let lock = self.lock_for(&path);
            let _guard = lock.lock().await;
            let previous = read_snapshot(&path, &record.id, input).await.map_err(|reason| PluginAbort {
                plugin: Arc::clone(&record.id),
                hook: HOOK,
                reason,
            })?;
            if record.state() == PluginState::Failed {
                result.extend(previous.unwrap_or_default());
                report.ran(loaded.current_fingerprint().stats());
                continue;
            }
            let started = Instant::now();
            let outcome = async {
                let channels = call_guest(dispatcher, loaded, input).await?;
                let items = normalize(&record.id, input, channels).map_err(CallError::Trap)?;
                let snapshot = Snapshot { version: 1, owner: record.id.to_string(), input: input.to_string(), items };
                let bytes = serde_json::to_vec(&snapshot)
                    .map_err(|_| CallError::Trap("cannot serialize managed channels".into()))?;
                if bytes.len() > MAX_SNAPSHOT_BYTES {
                    return Err(CallError::Trap("managed snapshot exceeds 16 MiB".into()));
                }
                // Commit before publishing. Even an intentional empty snapshot is persisted.
                write_file_atomic(&path, &bytes)
                    .await
                    .map_err(|_| CallError::Trap("cannot save managed channels".into()))?;
                Ok(snapshot.items)
            }
            .await;
            let mut stats = PluginHookStats::new(&record.id, HOOK);
            stats.kept = count(outcome.as_ref().map_or(0, Vec::len));
            // Snapshot errors and guest errors never delete the last-good contribution.
            // This hook deliberately ignores abort-target on a guest failure.
            let outcome = outcome.map_err(|err| match err {
                CallError::Trap(text) => CallError::Trap(redact(&text, &loaded.env.secrets)),
                CallError::Instantiate(text) => CallError::Instantiate(redact(&text, &loaded.env.secrets)),
                other => other,
            });
            let _ =
                HookDispatcher::settle(record, loaded, HOOK, ctx, stats, started, outcome.as_ref().map(|_| ()), report);
            match outcome {
                Ok(items) => result.extend(items),
                Err(_) => result.extend(previous.unwrap_or_default()),
            }
        }
        Ok(result)
    }
}

async fn call_guest(
    dispatcher: &HookDispatcher<'_>,
    loaded: &LoadedPlugin,
    input: &str,
) -> Result<Vec<ManagedChannel>, CallError> {
    let indices = loaded
        .managed_channels
        .as_ref()
        .ok_or_else(|| CallError::Instantiate("managed-channel export missing".into()))?;
    let mut live = dispatcher.open(loaded).await?;
    let instance = live.instance();
    let guest = indices.load(live.store_mut(), &instance).map_err(|err| CallError::Trap(format!("{err:#}")))?;
    let outcome = call_with_deadline(live.limits(), guest.call_reconcile_input(live.armed(), input)).await;
    host_fns::end_call(live.store_mut().data_mut()).await;
    outcome?.map_err(|err| CallError::Trap(format!("managed-channel hook returned err: {err}")))
}

fn normalize(owner: &str, input: &str, channels: Vec<ManagedChannel>) -> Result<Vec<PlaylistItem>, String> {
    if channels.len() > MAX_CHANNELS {
        return Err("managed channel limit is 10000 per plugin/input".into());
    }
    let mut ids = HashSet::new();
    let mut items = Vec::with_capacity(channels.len());
    for managed in channels {
        if managed.id.is_empty()
            || managed.id.trim() != managed.id
            || managed.id.len() > 256
            || managed.id.chars().any(char::is_control)
            || !ids.insert(managed.id.clone())
        {
            return Err("managed IDs must be unique, nonblank and at most 256 bytes".into());
        }
        let channel = managed.value;
        if channel.cluster != Cluster::Live
            || !matches!(
                channel.item_type,
                ItemType::Live | ItemType::LiveUnknown | ItemType::LiveHls | ItemType::LiveDash
            )
        {
            return Err("managed channels currently support live streams only".into());
        }
        let url = reqwest::Url::parse(&channel.url).map_err(|_| "managed channel URL must be HTTP(S)".to_string())?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err("managed channel URL must be HTTP(S)".into());
        }
        let identity = serde_json::to_vec(&(owner, input, &managed.id)).expect("string tuple serializes");
        let id = format!("plugin:{}", sha256_hex(&identity));
        let name = if channel.name.trim().is_empty() { channel.title.clone() } else { channel.name };
        if name.trim().is_empty() {
            return Err("managed channel name is required".into());
        }
        let title = if channel.title.is_empty() { name.clone() } else { channel.title };
        let mut header = PlaylistItemHeader {
            id: id.as_str().intern(),
            input_stream_id: id.intern(),
            input_name: input.intern(),
            name: name.intern(),
            title: title.intern(),
            group: if channel.group.trim().is_empty() { "Uncategorized".intern() } else { channel.group.intern() },
            url: channel.url.intern(),
            logo: channel.logo.intern(),
            epg_channel_id: channel.epg_id.filter(|id| !id.trim().is_empty()).map(Internable::intern),
            chno: channel.chno.unwrap_or(0),
            item_type: item_type_from_wit(channel.item_type),
            xtream_cluster: XtreamCluster::Live,
            ..PlaylistItemHeader::default()
        };
        header.gen_uuid();
        items.push(PlaylistItem { header });
    }
    Ok(items)
}

async fn read_snapshot(path: &Path, owner: &str, input: &str) -> Result<Option<Vec<PlaylistItem>>, String> {
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("cannot read managed channel snapshot".into()),
    };
    let mut bytes = Vec::new();
    file.take((MAX_SNAPSHOT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "cannot read managed channel snapshot")?;
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err("managed snapshot exceeds 16 MiB".into());
    }
    let mut snapshot: Snapshot = serde_json::from_slice(&bytes).map_err(|_| "invalid managed channel snapshot")?;
    if snapshot.version != 1
        || snapshot.owner != owner
        || snapshot.input != input
        || snapshot.items.len() > MAX_CHANNELS
        || snapshot.items.iter().any(|item| &*item.header.input_name != input || !item.header.id.starts_with("plugin:"))
    {
        return Err("managed snapshot identity mismatch".into());
    }
    for item in &mut snapshot.items {
        item.header.gen_uuid();
    }
    Ok(Some(snapshot.items))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::PlaylistEntry;
    use tuliprox_plugin_api::bindings::tuliprox::plugin::types::Channel;
    fn channel(id: &str, url: &str) -> ManagedChannel {
        ManagedChannel {
            id: id.into(),
            value: Channel {
                handle: 1,
                name: "Team".into(),
                title: String::new(),
                group: "Sports".into(),
                url: url.into(),
                epg_id: None,
                logo: String::new(),
                chno: None,
                item_type: ItemType::Live,
                cluster: Cluster::Live,
                input_name: "untrusted".into(),
                input_stream_id: "untrusted".into(),
                extra: vec![],
            },
        }
    }
    #[test]
    fn identity_is_owned_and_stable_across_url_changes() {
        let first = normalize("owner", "input", vec![channel("team", "https://host/1")]).unwrap();
        let changed = normalize("owner", "input", vec![channel("team", "https://host/2")]).unwrap();
        assert_eq!(first[0].get_uuid(), changed[0].get_uuid());
        let elsewhere = normalize("owner", "other-input", vec![channel("team", "https://host/1")]).unwrap();
        assert_ne!(first[0].get_uuid(), elsewhere[0].get_uuid());
        assert_eq!(&*first[0].header.input_name, "input");
        assert!(first[0].header.input_stream_id.starts_with("plugin:"));
    }
    #[test]
    fn invalid_snapshots_are_rejected_in_full() {
        for id in ["", " ", " team", "team\n"] {
            assert!(normalize("owner", "input", vec![channel(id, "https://host/1")]).is_err());
        }
        for url in ["file:///etc/passwd", "relative", "provider://example"] {
            assert!(normalize("owner", "input", vec![channel("team", url)]).is_err());
        }
        let mut invalid = channel("two", "https://host/2");
        invalid.value.cluster = Cluster::Video;
        invalid.value.item_type = ItemType::Video;
        assert!(normalize("owner", "input", vec![channel("one", "https://host/1"), invalid]).is_err());
    }
    #[tokio::test]
    async fn missing_snapshot_differs_from_corrupt_or_wrong_owner_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.json");
        assert!(read_snapshot(&path, "owner", "input").await.unwrap().is_none());
        tokio::fs::write(&path, b"{truncated").await.unwrap();
        assert!(read_snapshot(&path, "owner", "input").await.is_err());
        let snapshot = Snapshot { version: 1, owner: "other".into(), input: "input".into(), items: vec![] };
        tokio::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).await.unwrap();
        assert!(read_snapshot(&path, "owner", "input").await.is_err());
    }
}
