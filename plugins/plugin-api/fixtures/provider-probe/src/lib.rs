//! Fixture: an input provider and an output generator, both deterministic.
//!
//! * `fetch-playlist`: `count` channels (setting `count`, default 5), named
//!   `ch-000`, `ch-001`, ..., in `Group 0..2`, with stable URLs and EPG ids.
//! * `fetch-epg`: `ok([])`, the provider has no EPG.
//! * Output (`probe-txt`): a header line (`# <title>`, the `title` option or
//!   `provider-probe`), one `name|group|url` line per channel, and a trailer
//!   `# end <count>`. Nothing time-dependent, so the same items always give the
//!   same bytes.
//!
//! Attack switches for the isolation tests: setting `mode: loop` makes
//! `fetch-playlist` spin forever; the output option `render_bytes` makes
//! `render-batch` return that many bytes.

wit_bindgen::generate!({
    path: ["../../wit", "wit"],
    world: "tuliprox:provider-probe/provider-probe",
    generate_all,
});

use exports::tuliprox::plugin::{input_provider, lifecycle, output_generator};
use serde_json::Value;
use std::sync::Mutex;
use tuliprox::plugin::{
    epg_types::{EpgChannel, EpgProgramme},
    host,
    types::{Channel, Cluster, ItemType, LogLevel},
};

const DEFAULT_COUNT: usize = 5;

static COUNT: Mutex<Option<usize>> = Mutex::new(None);
static SPIN: Mutex<bool> = Mutex::new(false);
// Per instance, and an output file has an instance of its own.
static RENDERED: Mutex<usize> = Mutex::new(0);
static RENDER_BYTES: Mutex<usize> = Mutex::new(0);

struct ProviderProbe;

fn count() -> usize {
    COUNT.lock().ok().and_then(|guard| *guard).unwrap_or(DEFAULT_COUNT)
}

impl lifecycle::Guest for ProviderProbe {
    fn init(config: String) -> Result<(), String> {
        let parsed: Value = serde_json::from_str(&config).unwrap_or(Value::Null);
        let configured = parsed.get("count").and_then(Value::as_u64).and_then(|count| usize::try_from(count).ok());
        if let Ok(mut guard) = COUNT.lock() {
            *guard = configured;
        }
        if let Ok(mut guard) = SPIN.lock() {
            *guard = parsed.get("mode").and_then(Value::as_str) == Some("loop");
        }
        host::log(LogLevel::Debug, "provider-probe initialised");
        Ok(())
    }
}

fn channel(index: usize, input_name: &str) -> Channel {
    Channel {
        handle: 0,
        name: format!("ch-{index:03}"),
        title: format!("Channel {index:03}"),
        group: format!("Group {}", index % 3),
        url: format!("http://provider.example/live/{index}.ts"),
        epg_id: Some(format!("epg.ch-{index:03}")),
        logo: String::new(),
        chno: u32::try_from(index + 1).ok(),
        item_type: ItemType::Live,
        cluster: Cluster::Live,
        input_name: input_name.to_string(),
        input_stream_id: format!("{}", 1000 + index),
        extra: Vec::new(),
    }
}

impl input_provider::Guest for ProviderProbe {
    fn fetch_playlist(req: input_provider::InputRequest) -> Result<Vec<Channel>, String> {
        if SPIN.lock().is_ok_and(|spin| *spin) {
            let mut ticks: u64 = 0;
            loop {
                ticks = core::hint::black_box(ticks.wrapping_add(1));
            }
        }
        Ok((0..count()).map(|index| channel(index, &req.input_name)).collect())
    }

    fn fetch_epg(_req: input_provider::InputRequest) -> Result<Vec<(EpgChannel, Vec<EpgProgramme>)>, String> {
        Ok(Vec::new())
    }
}

impl output_generator::Guest for ProviderProbe {
    fn begin_output(req: output_generator::OutputRequest) -> Result<Vec<u8>, String> {
        if let Ok(mut rendered) = RENDERED.lock() {
            *rendered = 0;
        }
        let render_bytes = req
            .options
            .iter()
            .find(|(key, _)| key == "render_bytes")
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or_default();
        if let Ok(mut bytes) = RENDER_BYTES.lock() {
            *bytes = render_bytes;
        }
        let title = req
            .options
            .iter()
            .find(|(key, _)| key == "title")
            .map_or("provider-probe", |(_, value)| value.as_str());
        Ok(format!("# {title}\n").into_bytes())
    }

    fn render_batch(channels: Vec<Channel>) -> Result<Vec<u8>, String> {
        let flood = RENDER_BYTES.lock().map(|bytes| *bytes).unwrap_or_default();
        if flood > 0 {
            return Ok(vec![b'x'; flood]);
        }
        if let Ok(mut rendered) = RENDERED.lock() {
            *rendered += channels.len();
        }
        let mut text = String::new();
        for channel in channels {
            text.push_str(&format!("{}|{}|{}\n", channel.name, channel.group, channel.url));
        }
        Ok(text.into_bytes())
    }

    fn finish_output() -> Result<Vec<u8>, String> {
        let rendered = RENDERED.lock().map(|rendered| *rendered).unwrap_or_default();
        Ok(format!("# end {rendered}\n").into_bytes())
    }
}

export!(ProviderProbe);
