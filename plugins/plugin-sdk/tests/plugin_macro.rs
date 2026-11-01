//! `#[tuliprox_plugin]` expanded on the host target: the glue type-checks, the
//! manifest is read, and the adapters reach the user's impls through the mock
//! host. (The wasm exports themselves are `cfg(target_arch = "wasm32")`.)

use tuliprox_plugin_sdk::{bindings::exports::tuliprox::plugin as exports, prelude::*};

#[tuliprox_plugin(manifest = "tests/fixtures/manifest.yml")]
mod plugin {
    use super::*;

    #[tuliprox_plugin_settings]
    struct Settings {
        api_key: Secret<String>,
        #[serde(default)]
        label: String,
    }

    pub struct Fixture;

    impl PlaylistHooks for Fixture {
        fn on_stage(stage: Stage, target: &str, channels: Vec<Channel>) -> Result<Vec<ChannelChange>, String> {
            let settings = Settings::current()?;
            info!("{stage:?} on {target}: {} channels, label {}", channels.len(), settings.label);
            let response = http::Request::get("https://api.example/x").bearer(settings.api_key.expose()).send()?;
            kv::set_json_ttl("last-status", &response.status(), 30)?;
            Ok(channels.into_iter().map(|channel| ChannelChange::Drop(channel.handle)).collect())
        }
    }

    impl EventHooks for Fixture {
        fn on_event(ev: Event) -> Result<(), String> {
            match ev.typed()? {
                TypedEvent::PlaylistUpdate(update) => info!("update {:?}", update.state),
                other => warn!("ignored {other:?}"),
            }
            Ok(())
        }
    }

    impl SettingsMigration for Fixture {
        fn migrate(from_version: u32, settings: &str) -> Result<String, String> {
            Migrations::new()
                .step(1, |mut value| {
                    if let Some(object) = value.as_object_mut() {
                        if let Some(label) = object.remove("name") {
                            object.insert("label".to_string(), label);
                        }
                    }
                    Ok(value)
                })
                .run(from_version, settings)
        }
    }

    impl Tasks for Fixture {
        fn run(ctx: TaskContext) -> Result<String, String> {
            tasks::Router::new().task("refresh", |ctx| Ok(format!("ran at {}", ctx.scheduled_at))).dispatch(&ctx)
        }
    }
}

fn channel(handle: u32) -> Channel {
    Channel {
        handle,
        name: format!("ch{handle}"),
        title: String::new(),
        group: String::new(),
        url: String::new(),
        epg_id: None,
        logo: String::new(),
        chno: None,
        item_type: ItemType::Live,
        cluster: Cluster::Live,
        input_name: "in".into(),
        input_stream_id: handle.to_string(),
        extra: Vec::new(),
    }
}

#[test]
fn the_module_exposes_the_manifest_hooks_and_exactly_the_implemented_exports() {
    assert_eq!(
        plugin::HOOK_IDS,
        ["playlist.after-filter", "playlist.after-map", "events.on-event", "tasks.run", "settings.migrate"]
    );
    assert_eq!(
        plugin::EXPORTS,
        [
            "tuliprox:plugin/lifecycle@1.0.0",
            "tuliprox:plugin/event-hooks@1.0.0",
            "tuliprox:plugin/playlist-hooks@1.0.0",
            "tuliprox:plugin/settings-migration@1.0.0",
            "tuliprox:plugin/tasks@1.0.0",
        ]
    );
}

#[test]
fn the_default_lifecycle_parses_the_settings_and_a_hook_reaches_the_mock_host() {
    use exports::{lifecycle::Guest as _, playlist_hooks::Guest as _};

    tuliprox_plugin_sdk::mock::reset();
    tuliprox_plugin_sdk::mock::set_now(1_000);
    tuliprox_plugin_sdk::mock::http_respond(http::Method::Get, "https://api.example/x", 204, "");

    assert!(<tuliprox_plugin_sdk::__private::Adapter<plugin::__TuliproxDefaultLifecycle>>::init("{}".into()).is_err());
    <tuliprox_plugin_sdk::__private::Adapter<plugin::__TuliproxDefaultLifecycle>>::init(
        r#"{"api_key":"s3cret","label":"demo"}"#.into(),
    )
    .unwrap();

    let changes = <tuliprox_plugin_sdk::__private::Adapter<plugin::Fixture>>::on_stage(
        Stage::AfterFilter,
        "main".into(),
        vec![channel(1), channel(2)],
    )
    .unwrap();
    assert_eq!(changes.len(), 2);

    let sent = tuliprox_plugin_sdk::mock::http_requests();
    assert!(sent[0].headers().contains(&("authorization".to_string(), "Bearer s3cret".to_string())));
    assert_eq!(kv::get_json_fresh::<u16>("last-status").unwrap(), Some(204));
    let logs = tuliprox_plugin_sdk::mock::logs();
    assert!(logs[0].message.contains("2 channels, label demo"));
    assert!(logs.iter().all(|line| !line.message.contains("s3cret")));
}

#[test]
fn events_and_tasks_dispatch_through_their_adapters() {
    use exports::{event_hooks::Guest as _, tasks::Guest as _};

    tuliprox_plugin_sdk::mock::reset();
    <tuliprox_plugin_sdk::__private::Adapter<plugin::Fixture>>::on_event(Event {
        kind: "playlist.update".into(),
        payload: r#"{"state":"Success"}"#.into(),
        timestamp: 5,
    })
    .unwrap();
    assert_eq!(tuliprox_plugin_sdk::mock::logs()[0].message, "update Success");

    let ran = <tuliprox_plugin_sdk::__private::Adapter<plugin::Fixture>>::run(TaskContext {
        task_id: "refresh".into(),
        scheduled_at: 77,
        last_success_at: None,
    });
    assert_eq!(ran, Ok("ran at 77".to_string()));
}

#[test]
fn settings_migration_dispatches_through_its_adapter() {
    use exports::settings_migration::Guest as _;

    let migrated = <tuliprox_plugin_sdk::__private::Adapter<plugin::Fixture>>::migrate(1, r#"{"name":"x"}"#.into());
    assert_eq!(migrated, Ok(r#"{"label":"x"}"#.to_string()));
    let newer = <tuliprox_plugin_sdk::__private::Adapter<plugin::Fixture>>::migrate(5, "{}".into());
    assert!(newer.unwrap_err().contains("newer"));
}
