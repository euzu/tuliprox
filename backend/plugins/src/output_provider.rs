//! `type: plugin` target outputs (plan §7 *Output providers*):
//! `begin-output -> render-batch… -> finish-output`, written to a temp file
//! and renamed; a failure fails only that output.
//!
//! One fresh instance per output file, under the provider deadline. The
//! returned bytes are appended to `<path>.tmp` as they arrive (so a large
//! catalog is never held in memory), and the temp file replaces `path` only
//! once `finish-output` succeeded. Any failure removes the temp file and leaves
//! `path` as the last good run wrote it. The host writes no timestamp or
//! anything else of its own: the same plugin, settings, options and items give
//! a byte-identical file.

use crate::{
    convert::item_to_channel,
    dispatch::{count, HookDispatcher},
    host_fns,
    registry::{LoadedPlugin, PluginRecord},
    runtime::{call_with_deadline, CallError, LiveInstance},
};
use shared::model::{HookCtx, PlaylistItem, PluginHook, PluginHookStats, PluginOutputRequest, PluginRunReport};
use std::{
    future::Future,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::Instant,
};
use tuliprox_core::utils::tmp_path_for;
use tuliprox_plugin_api::bindings::{
    exports::tuliprox::plugin::output_generator::{self, OutputRequest},
    tuliprox::plugin::types::Channel,
};

/// Why an output was not produced: the plugin failed, or the host could not
/// write the file. Only the first counts against the plugin's circuit breaker.
#[derive(Debug)]
enum Failure {
    Plugin(CallError),
    Host(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plugin(err) => write!(f, "{err}"),
            Self::Host(message) => f.write_str(message),
        }
    }
}

/// The most bytes one `begin-output` / `render-batch` / `finish-output` call may
/// return; beyond that the call fails like a trap.
const MAX_OUTPUT_CHUNK_BYTES: usize = 16 * 1024 * 1024;

/// `chunk` of the guest call `what`, or a trap when it is over the cap.
fn capped(what: &str, chunk: Vec<u8>) -> Result<Vec<u8>, CallError> {
    if chunk.len() > MAX_OUTPUT_CHUNK_BYTES {
        return Err(CallError::Trap(format!("{what} returned {} bytes (cap {MAX_OUTPUT_CHUNK_BYTES})", chunk.len())));
    }
    Ok(chunk)
}

/// The three guest calls of one output file; the wasm instance behind it is
/// [`GuestOutput`], a test double stands in for it in the tests.
trait Generator {
    fn begin(&mut self) -> impl Future<Output = Result<Vec<u8>, CallError>> + Send;
    fn render(&mut self, channels: Vec<Channel>) -> impl Future<Output = Result<Vec<u8>, CallError>> + Send;
    fn finish(&mut self) -> impl Future<Output = Result<Vec<u8>, CallError>> + Send;
}

/// One output file being written: `<path>.tmp` until [`Self::commit`].
struct OutputFile {
    path: PathBuf,
    tmp: PathBuf,
    writer: Option<BufWriter<std::fs::File>>,
}

/// Run blocking file work off the async workers (Rule 0).
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> std::io::Result<T> + Send + 'static) -> Result<T, String> {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result.map_err(|err| err.to_string()),
        Err(err) => Err(format!("file task failed: {err}")),
    }
}

impl OutputFile {
    async fn create(path: &Path) -> Result<Self, String> {
        let path = path.to_path_buf();
        let tmp = tmp_path_for(&path);
        let create = tmp.clone();
        let file = blocking(move || {
            if let Some(parent) = create.parent().filter(|parent| !parent.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::File::create(&create)
        })
        .await
        .map_err(|err| format!("can't create {}: {err}", tmp.display()))?;
        Ok(Self { path, tmp, writer: Some(BufWriter::new(file)) })
    }

    async fn write(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        if bytes.is_empty() {
            return Ok(());
        }
        let Some(mut writer) = self.writer.take() else {
            return Err(format!("{} is already closed", self.tmp.display()));
        };
        let writer = blocking(move || writer.write_all(&bytes).map(|()| writer))
            .await
            .map_err(|err| format!("can't write {}: {err}", self.tmp.display()))?;
        self.writer = Some(writer);
        Ok(())
    }

    /// Flush and close the temp file, then rename it over the final path.
    async fn commit(mut self) -> Result<(), String> {
        let (path, tmp) = (self.path.clone(), self.tmp.clone());
        let writer = self.writer.take();
        let result = blocking(move || {
            if let Some(mut writer) = writer {
                writer.flush()?;
            }
            std::fs::rename(&tmp, &path)
        })
        .await
        .map_err(|err| format!("can't publish {}: {err}", self.path.display()));
        if result.is_err() {
            self.discard().await;
        }
        result
    }

    /// Remove the temp file; the final path is untouched.
    async fn discard(mut self) {
        drop(self.writer.take());
        let tmp = self.tmp.clone();
        if let Err(err) = blocking(move || match std::fs::remove_file(&tmp) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        })
        .await
        {
            log::warn!("can't remove {}: {err}", self.tmp.display());
        }
    }
}

/// `begin-output`, `render-batch` per `batch_size` items, `finish-output`, each
/// result appended to `file`.
async fn drive<G: Generator + Send>(
    generator: &mut G,
    items: &[&PlaylistItem],
    batch_size: usize,
    file: &mut OutputFile,
) -> Result<(), Failure> {
    let chunk = capped("begin-output", generator.begin().await.map_err(Failure::Plugin)?).map_err(Failure::Plugin)?;
    file.write(chunk).await.map_err(Failure::Host)?;
    for batch in items.chunks(batch_size.max(1)) {
        let channels = batch
            .iter()
            .enumerate()
            // A batch never exceeds MAX_PLUGINS_BATCH_SIZE, which fits in u32.
            .map(|(handle, item)| item_to_channel(u32::try_from(handle).unwrap_or(u32::MAX), item))
            .collect();
        let chunk = capped("render-batch", generator.render(channels).await.map_err(Failure::Plugin)?)
            .map_err(Failure::Plugin)?;
        file.write(chunk).await.map_err(Failure::Host)?;
    }
    let chunk = capped("finish-output", generator.finish().await.map_err(Failure::Plugin)?).map_err(Failure::Plugin)?;
    file.write(chunk).await.map_err(Failure::Host)?;
    Ok(())
}

/// The output generator of one fresh instance.
struct GuestOutput {
    live: LiveInstance,
    guest: output_generator::Guest,
    request: OutputRequest,
}

/// The guest's `err` string is a trap of this call, like any other hook.
fn guest_err(what: &str, err: &str) -> CallError { CallError::Trap(format!("{what} returned err: {err}")) }

impl Generator for GuestOutput {
    async fn begin(&mut self) -> Result<Vec<u8>, CallError> {
        let limits = self.live.limits();
        let result = call_with_deadline(limits, self.guest.call_begin_output(self.live.armed(), &self.request)).await;
        host_fns::end_call(self.live.store_mut().data_mut()).await;
        result?.map_err(|err| guest_err("begin-output", &err))
    }

    async fn render(&mut self, channels: Vec<Channel>) -> Result<Vec<u8>, CallError> {
        let limits = self.live.limits();
        let result = call_with_deadline(limits, self.guest.call_render_batch(self.live.armed(), &channels)).await;
        host_fns::end_call(self.live.store_mut().data_mut()).await;
        result?.map_err(|err| guest_err("render-batch", &err))
    }

    async fn finish(&mut self) -> Result<Vec<u8>, CallError> {
        let limits = self.live.limits();
        let result = call_with_deadline(limits, self.guest.call_finish_output(self.live.armed())).await;
        host_fns::end_call(self.live.store_mut().data_mut()).await;
        result?.map_err(|err| guest_err("finish-output", &err))
    }
}

/// The `provides.output_format` of `record`'s manifest.
fn output_format_of(record: &PluginRecord) -> Option<&str> {
    record.manifest.as_deref().and_then(|manifest| manifest.provides.output_format.as_deref())
}

/// The enabled plugin `plugin` that runs `output.render`.
fn generator<'d>(
    dispatcher: &'d HookDispatcher<'_>,
    plugin: &str,
) -> Result<(&'d std::sync::Arc<PluginRecord>, &'d LoadedPlugin), String> {
    dispatcher
        .named(PluginHook::RenderOutput, plugin)
        .ok_or_else(|| format!("plugin {plugin} is not enabled or does not provide an output format"))
}

/// The `provides.output_format` of the enabled plugin `plugin` that runs
/// `output.render`; `None` for any other plugin. What a configured `format:`
/// must equal (checked after plugin load).
pub(crate) fn output_format(dispatcher: &HookDispatcher<'_>, plugin: &str) -> Option<String> {
    let (record, _) = generator(dispatcher, plugin).ok()?;
    output_format_of(record).map(ToString::to_string)
}

/// [`shared::model::PluginHost::render_output`].
pub(crate) async fn render_output(
    dispatcher: &HookDispatcher<'_>,
    plugin: &str,
    ctx: &HookCtx<'_>,
    request: &PluginOutputRequest<'_>,
    items: &[&PlaylistItem],
) -> Result<(), String> {
    let (record, loaded) = generator(dispatcher, plugin)?;
    // The manifest is the source of truth for the format; a configured one that disagrees is a config
    // error, reported here as well as by the check after plugin load (a reload can change it).
    let format = output_format_of(record).unwrap_or_default();
    if !request.format.is_empty() && request.format != format {
        return Err(format!("plugin {plugin} renders format `{format}`, but the output asks for `{}`", request.format));
    }

    let started = Instant::now();
    let outcome = render(dispatcher, loaded, request, format, items).await;
    let mut stats = PluginHookStats::new(&record.id, PluginHook::RenderOutput);
    stats.kept = count(items.len());
    let breaker = match &outcome {
        Err(Failure::Plugin(err)) => Err(err),
        Ok(()) | Err(Failure::Host(_)) => Ok(()),
    };
    // `PluginHost` hands no run report to a provider call, so the fingerprint `settle` records goes to
    // a report nobody reads; `fail_mode: abort-target` has nothing to abort (the output fails on `Err`).
    let mut report = PluginRunReport::default();
    let _ = HookDispatcher::settle(record, loaded, PluginHook::RenderOutput, ctx, stats, started, breaker, &mut report);
    outcome.map_err(|failure| format!("plugin {plugin}: rendering {} failed: {failure}", request.path.display()))
}

async fn render(
    dispatcher: &HookDispatcher<'_>,
    loaded: &LoadedPlugin,
    request: &PluginOutputRequest<'_>,
    format: &str,
    items: &[&PlaylistItem],
) -> Result<(), Failure> {
    let indices = loaded
        .output_generator
        .as_ref()
        .ok_or_else(|| Failure::Plugin(CallError::Instantiate("does not export output-generator".into())))?;
    let exec = loaded.prepared.limits().provider_exec(dispatcher.runtime().config());
    let mut live = dispatcher.open_with(loaded, exec).await.map_err(Failure::Plugin)?;
    let instance = live.instance();
    let guest = indices
        .load(live.store_mut(), &instance)
        .map_err(|err| Failure::Plugin(CallError::Trap(format!("{err:#}"))))?;
    let mut output = GuestOutput {
        live,
        guest,
        request: OutputRequest {
            target: request.target.to_string(),
            format: format.to_string(),
            options: request.options.to_vec(),
        },
    };

    let mut file = OutputFile::create(request.path).await.map_err(Failure::Host)?;
    match drive(&mut output, items, dispatcher.batch_size(), &mut file).await {
        Ok(()) => file.commit().await.map_err(Failure::Host),
        Err(failure) => {
            file.discard().await;
            Err(failure)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::{model::PlaylistItemHeader, utils::Internable};

    fn item(name: &str) -> PlaylistItem {
        PlaylistItem {
            header: PlaylistItemHeader {
                name: name.intern(),
                title: name.intern(),
                group: "g".intern(),
                url: format!("http://host/{name}").intern(),
                ..PlaylistItemHeader::default()
            },
        }
    }

    /// Records the calls, answers with text, and fails at a chosen call.
    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
        fail_at: Option<&'static str>,
        /// Answers this call with a chunk one byte over the cap.
        oversize_at: Option<&'static str>,
    }

    impl Fake {
        fn answer(&mut self, call: &str, bytes: String) -> Result<Vec<u8>, CallError> {
            self.calls.push(call.to_string());
            if self.fail_at == Some(call) {
                return Err(CallError::Trap(format!("{call} returned err: boom")));
            }
            if self.oversize_at == Some(call) {
                return Ok(vec![0; MAX_OUTPUT_CHUNK_BYTES + 1]);
            }
            Ok(bytes.into_bytes())
        }
    }

    impl Generator for Fake {
        fn begin(&mut self) -> impl Future<Output = Result<Vec<u8>, CallError>> + Send {
            std::future::ready(self.answer("begin", "HEADER\n".to_string()))
        }

        fn render(&mut self, channels: Vec<Channel>) -> impl Future<Output = Result<Vec<u8>, CallError>> + Send {
            let names: Vec<_> = channels.iter().map(|c| format!("{}:{}", c.handle, c.name)).collect();
            std::future::ready(self.answer("render", format!("{}\n", names.join(","))))
        }

        fn finish(&mut self) -> impl Future<Output = Result<Vec<u8>, CallError>> + Send {
            std::future::ready(self.answer("finish", "TRAILER\n".to_string()))
        }
    }

    fn items() -> Vec<PlaylistItem> { ["a", "b", "c"].into_iter().map(item).collect() }

    #[tokio::test]
    async fn success_writes_begin_batches_finish_and_renames_the_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out").join("list.txt");
        let items = items();
        let refs: Vec<&PlaylistItem> = items.iter().collect();

        let mut fake = Fake::default();
        let mut file = OutputFile::create(&path).await.unwrap();
        assert!(tmp_path_for(&path).exists(), "bytes go to the temp file first");
        assert!(!path.exists());
        drive(&mut fake, &refs, 2, &mut file).await.unwrap();
        file.commit().await.unwrap();

        assert_eq!(fake.calls, ["begin", "render", "render", "finish"]);
        // Handles restart per batch (0..len), so the second batch holds "0:c".
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "HEADER\n0:a,1:b\n0:c\nTRAILER\n");
        assert!(!tmp_path_for(&path).exists());
    }

    #[tokio::test]
    async fn the_same_input_gives_a_byte_identical_file() {
        let dir = tempfile::tempdir().unwrap();
        let items = items();
        let refs: Vec<&PlaylistItem> = items.iter().collect();
        let mut contents = Vec::new();
        for name in ["one.txt", "two.txt"] {
            let path = dir.path().join(name);
            let mut file = OutputFile::create(&path).await.unwrap();
            drive(&mut Fake::default(), &refs, 2, &mut file).await.unwrap();
            file.commit().await.unwrap();
            contents.push(std::fs::read(&path).unwrap());
        }
        assert_eq!(contents[0], contents[1]);
    }

    #[tokio::test]
    async fn a_failure_removes_the_temp_file_and_keeps_the_previous_output() {
        for fail_at in ["begin", "render", "finish"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("list.txt");
            std::fs::write(&path, "previous run").unwrap();
            let items = items();
            let refs: Vec<&PlaylistItem> = items.iter().collect();

            let mut fake = Fake { fail_at: Some(fail_at), ..Fake::default() };
            let mut file = OutputFile::create(&path).await.unwrap();
            let failure = drive(&mut fake, &refs, 2, &mut file).await.unwrap_err();
            file.discard().await;

            assert!(matches!(failure, Failure::Plugin(_)), "{fail_at}");
            assert!(failure.to_string().contains("boom"), "{fail_at}");
            assert!(!tmp_path_for(&path).exists(), "{fail_at}: temp file left behind");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "previous run", "{fail_at}");
        }
    }

    #[tokio::test]
    async fn a_chunk_over_the_cap_fails_the_output_like_a_trap() {
        for oversize_at in ["begin", "render", "finish"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("list.txt");
            std::fs::write(&path, "previous run").unwrap();
            let items = items();
            let refs: Vec<&PlaylistItem> = items.iter().collect();

            let mut fake = Fake { oversize_at: Some(oversize_at), ..Fake::default() };
            let mut file = OutputFile::create(&path).await.unwrap();
            let failure = drive(&mut fake, &refs, 2, &mut file).await.unwrap_err();
            file.discard().await;

            assert!(matches!(failure, Failure::Plugin(CallError::Trap(_))), "{oversize_at}");
            assert!(failure.to_string().contains("cap"), "{oversize_at}: {failure}");
            assert!(!tmp_path_for(&path).exists(), "{oversize_at}: temp file left behind");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "previous run", "{oversize_at}");
        }
    }

    #[test]
    fn a_chunk_of_exactly_the_cap_is_accepted() {
        assert_eq!(capped("render-batch", vec![0; MAX_OUTPUT_CHUNK_BYTES]).unwrap().len(), MAX_OUTPUT_CHUNK_BYTES);
        assert!(capped("render-batch", vec![0; MAX_OUTPUT_CHUNK_BYTES + 1]).is_err());
    }

    #[tokio::test]
    async fn a_failed_render_stops_calling_the_guest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("list.txt");
        let items = items();
        let refs: Vec<&PlaylistItem> = items.iter().collect();
        let mut fake = Fake { fail_at: Some("render"), ..Fake::default() };
        let mut file = OutputFile::create(&path).await.unwrap();
        drive(&mut fake, &refs, 1, &mut file).await.unwrap_err();
        file.discard().await;
        // begin, the first render that failed; no second render and no finish.
        assert_eq!(fake.calls, ["begin", "render"]);
    }

    #[tokio::test]
    async fn publishing_over_a_directory_fails_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("taken");
        std::fs::create_dir(&path).unwrap();
        let mut file = OutputFile::create(&path).await.unwrap();
        file.write(b"x".to_vec()).await.unwrap();
        let err = file.commit().await.unwrap_err();
        assert!(err.contains("can't publish"), "{err}");
        assert!(!tmp_path_for(&path).exists());
        assert!(path.is_dir());
    }
}
