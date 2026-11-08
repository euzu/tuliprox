//! Per-plugin, per-destination managed live channel snapshots. Provider caches stay untouched.
use crate::{
    convert::item_type_from_wit,
    dispatch::{count, HookDispatcher},
    host_fns,
    registry::{sha256_hex, PluginState},
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
    fn lock_for(&self, path: &Path) -> Arc<AsyncMutex<()>> {
        Arc::clone(self.locks.lock().unwrap_or_else(PoisonError::into_inner).entry(path.to_path_buf()).or_default())
    }
}
fn normalize(
    owner: &str,
    scope: &str,
    inputs: &[String],
    channels: Vec<ManagedChannel>,
) -> Result<Vec<PlaylistItem>, String> {
    if channels.len() > MAX_CHANNELS {
        return Err("managed channel limit is 10000 per plugin/input".into());
    }
    let mut ids = HashSet::new();
    let mut items = Vec::with_capacity(channels.len());
    for managed in channels {
        if !inputs.contains(&managed.source_input) {
            return Err("managed source input is outside destination".into());
        }
        let input = managed.source_input.as_str();
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
        let identity = serde_json::to_vec(&(owner, scope, &managed.id)).expect("string tuple serializes");
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

async fn read_scoped_snapshot(path: &Path, owner: &str, input: &str) -> Result<Option<Vec<PlaylistItem>>, String> {
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
        || snapshot.items.iter().any(|item| !item.header.id.starts_with("plugin:"))
    {
        return Err("managed snapshot identity mismatch".into());
    }
    for item in &mut snapshot.items {
        item.header.gen_uuid();
    }
    Ok(Some(snapshot.items))
}

impl ManagedSnapshots {
    pub(crate) async fn reconcile(
        &self,
        dispatcher: &HookDispatcher<'_>,
        plugin: &str,
        request: &shared::model::ManagedChannelRequest<'_>,
        report: &mut PluginRunReport,
    ) -> Result<Vec<PlaylistItem>, PluginAbort> {
        use shared::model::{ManagedChannelStage, ManagedDestination};
        use tuliprox_plugin_api::bindings::exports::tuliprox::plugin::managed_channel_hooks as wit;
        const HOOK: PluginHook = PluginHook::ManagedChannels;
        let abort = |reason: String| PluginAbort { plugin: plugin.intern(), hook: HOOK, reason };
        let (kind, point, scope) = match (request.destination, request.stage) {
            (ManagedDestination::Input, None) => (wit::DestinationKind::Input, wit::InsertionPoint::Input, "input"),
            (ManagedDestination::Target, Some(ManagedChannelStage::BeforeFilter)) => {
                (wit::DestinationKind::Target, wit::InsertionPoint::BeforeFilter, "before_filter")
            }
            (ManagedDestination::Target, Some(ManagedChannelStage::AfterFilter)) => {
                (wit::DestinationKind::Target, wit::InsertionPoint::AfterFilter, "after_filter")
            }
            _ => return Err(abort("invalid managed insertion destination/stage".into())),
        };
        let Some(record) = dispatcher.record(plugin) else { return Ok(Vec::new()) };
        let Some(loaded) = record
            .loaded
            .as_ref()
            .filter(|loaded| loaded.hooks.contains(&HOOK) && loaded.env.capabilities.playlist_write)
        else {
            return Ok(Vec::new());
        };
        if !matches!(record.state(), PluginState::Enabled | PluginState::Failed) {
            return Ok(Vec::new());
        }
        let key = serde_json::to_string(&(
            match request.destination {
                ManagedDestination::Input => "input",
                ManagedDestination::Target => "target",
            },
            scope,
            request.name,
        ))
        .expect("strings serialize");
        let path = self.root.join(plugin).join("bindings").join(format!("{}.json", sha256_hex(key.as_bytes())));
        let lock = self.lock_for(&path);
        let _guard = lock.lock().await;
        let mut previous = read_scoped_snapshot(&path, plugin, &key).await.map_err(abort)?.unwrap_or_default();
        // Revoking an input from a target also revokes cached channels using its credentials.
        previous.retain(|item| request.inputs.iter().any(|name| name.as_str() == &*item.header.input_name));
        if record.state() == PluginState::Failed {
            report.ran(loaded.current_fingerprint().stats());
            return Ok(previous);
        }
        let started = Instant::now();
        let ctx = match request.destination {
            ManagedDestination::Input => HookCtx::new("").with_input(request.name),
            ManagedDestination::Target => HookCtx::new(request.name),
        };
        let outcome = async {
            let indices = loaded
                .managed_channels
                .as_ref()
                .ok_or_else(|| CallError::Instantiate("managed insertion export missing".into()))?;
            let mut live = dispatcher.open(loaded).await?;
            let instance = live.instance();
            let guest = indices.load(live.store_mut(), &instance).map_err(|e| CallError::Trap(format!("{e:#}")))?;
            let req = wit::Request { kind, name: request.name.into(), point, inputs: request.inputs.to_vec() };
            let channels = call_with_deadline(live.limits(), guest.call_reconcile(live.armed(), &req)).await;
            host_fns::end_call(live.store_mut().data_mut()).await;
            let channels = channels?.map_err(CallError::Trap)?;
            if channels.len() > MAX_CHANNELS {
                return Err(CallError::Trap("managed channel limit is 10000 per binding".into()));
            }
            let identity_scope = serde_json::to_string(&(
                match request.destination {
                    ManagedDestination::Input => "input",
                    ManagedDestination::Target => "target",
                },
                request.name,
            ))
            .expect("strings serialize");
            let items = normalize(plugin, &identity_scope, request.inputs, channels).map_err(CallError::Trap)?;
            let snapshot = Snapshot { version: 1, owner: plugin.into(), input: key.clone(), items };
            let bytes = serde_json::to_vec(&snapshot)
                .map_err(|_| CallError::Trap("cannot serialize managed snapshot".into()))?;
            if bytes.len() > MAX_SNAPSHOT_BYTES {
                return Err(CallError::Trap("managed snapshot exceeds 16 MiB".into()));
            }
            write_file_atomic(&path, &bytes)
                .await
                .map_err(|_| CallError::Trap("cannot save managed snapshot".into()))?;
            Ok(snapshot.items)
        }
        .await;
        let outcome = outcome.map_err(|err| match err {
            CallError::Trap(text) => CallError::Trap(redact(&text, &loaded.env.secrets)),
            CallError::Instantiate(text) => CallError::Instantiate(redact(&text, &loaded.env.secrets)),
            other => other,
        });
        let mut stats = PluginHookStats::new(plugin, HOOK);
        stats.kept = count(outcome.as_ref().map_or(0, Vec::len));
        let _ =
            HookDispatcher::settle(record, loaded, HOOK, &ctx, stats, started, outcome.as_ref().map(|_| ()), report);
        Ok(outcome.unwrap_or(previous))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::PlaylistEntry;
    use tuliprox_plugin_api::bindings::tuliprox::plugin::types::Channel;
    fn channel(id: &str, source: &str, url: &str) -> ManagedChannel {
        ManagedChannel {
            id: id.into(),
            source_input: source.into(),
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
    fn identity_is_scoped_and_source_cannot_be_spoofed() {
        let allowed = vec!["provider".to_string()];
        let first =
            normalize("owner", "target-a", &allowed, vec![channel("team", "provider", "https://host/1")]).unwrap();
        let changed =
            normalize("owner", "target-a", &allowed, vec![channel("team", "provider", "https://host/2")]).unwrap();
        assert_eq!(first[0].get_uuid(), changed[0].get_uuid());
        let elsewhere =
            normalize("owner", "target-b", &allowed, vec![channel("team", "provider", "https://host/1")]).unwrap();
        assert_ne!(first[0].get_uuid(), elsewhere[0].get_uuid());
        assert_eq!(&*first[0].header.input_name, "provider");
        assert!(normalize("owner", "target-a", &allowed, vec![channel("team", "outside", "https://host/1")]).is_err());
    }
    #[test]
    fn invalid_snapshots_are_rejected_in_full() {
        let allowed = vec!["provider".to_string()];
        for id in ["", " ", " team", "team\n"] {
            assert!(normalize("owner", "target", &allowed, vec![channel(id, "provider", "https://host/1")]).is_err());
        }
        for url in ["file:///etc/passwd", "relative", "provider://example"] {
            assert!(normalize("owner", "target", &allowed, vec![channel("team", "provider", url)]).is_err());
        }
        assert!(normalize(
            "owner",
            "target",
            &allowed,
            vec![channel("team", "provider", "https://host/1"), channel("team", "provider", "https://host/2")]
        )
        .is_err());
    }
    #[tokio::test]
    async fn missing_snapshot_differs_from_corrupt_or_wrong_owner_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.json");
        assert!(read_scoped_snapshot(&path, "owner", "scope").await.unwrap().is_none());
        tokio::fs::write(&path, b"{truncated").await.unwrap();
        assert!(read_scoped_snapshot(&path, "owner", "scope").await.is_err());
        let snapshot = Snapshot { version: 1, owner: "other".into(), input: "scope".into(), items: vec![] };
        tokio::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).await.unwrap();
        assert!(read_scoped_snapshot(&path, "owner", "scope").await.is_err());
    }
}
