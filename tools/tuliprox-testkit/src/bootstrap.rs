use crate::{
    config::{BootstrapConfig, Channel, FixtureInputType, FixtureStalker, PolicyContract},
    TestkitError,
};
use std::{
    collections::HashMap,
    fmt::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use tempfile::TempDir;
use tokio::process::{Child, Command};

const FIXTURE_PASSWORD: &str = "testkit-password";

/// The provider protocol the generated fixture input uses, plus the portal settings the
/// emulated Stalker portal needs.
pub struct FixtureInputPlan<'a> {
    pub input_type: FixtureInputType,
    pub stalker: &'a FixtureStalker,
}

impl FixtureInputPlan<'_> {
    const fn is_stalker(&self) -> bool { matches!(self.input_type, FixtureInputType::Stalker) }
}

/// Files owned by one isolated run. They are written below a unique temporary
/// directory and must never refer to an operator's configuration or storage.
pub struct FixturePaths {
    _directory: TempDir,
    pub root: PathBuf,
    pub config_file: PathBuf,
    pub source_file: PathBuf,
    pub api_proxy_file: PathBuf,
    pub web_root: PathBuf,
}

impl FixturePaths {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        run_id: &str,
        origin_address: SocketAddr,
        tuliprox_address: SocketAddr,
        policy: Option<&PolicyContract>,
        channels: &HashMap<String, Channel>,
        fixture_stream: &crate::config::FixtureStreamOptions,
        add_xtream_output: bool,
        input: &FixtureInputPlan<'_>,
    ) -> Result<Self, TestkitError> {
        let directory = tempfile::Builder::new().prefix("tuliprox-testkit-").tempdir()?;
        let root = directory.path().to_path_buf();
        let web_root = root.join("web");
        let storage_dir = root.join("storage");
        let backup_dir = root.join("backup");
        let history_dir = root.join("history");
        std::fs::create_dir_all(web_root.join("static/.well-known"))?;
        std::fs::create_dir_all(&storage_dir)?;
        std::fs::create_dir_all(&backup_dir)?;
        std::fs::create_dir_all(&history_dir)?;

        let config_file = root.join("config.yml");
        let source_file = root.join("source.yml");
        let api_proxy_file = root.join("api-proxy.yml");
        std::fs::write(
            &config_file,
            render_config(tuliprox_address, &web_root, &storage_dir, &backup_dir, &history_dir, policy)?,
        )?;
        std::fs::write(
            &source_file,
            render_sources(run_id, origin_address, channels, policy, fixture_stream, add_xtream_output, input),
        )?;
        std::fs::write(&api_proxy_file, render_api_proxy(tuliprox_address, policy))?;

        Ok(Self { _directory: directory, root, config_file, source_file, api_proxy_file, web_root })
    }
}

#[must_use]
pub const fn fixture_password() -> &'static str { FIXTURE_PASSWORD }

fn render_config(
    tuliprox_address: SocketAddr,
    web_root: &Path,
    storage_dir: &Path,
    backup_dir: &Path,
    history_dir: &Path,
    policy: Option<&PolicyContract>,
) -> Result<String, TestkitError> {
    let user_access_control = policy.is_some_and(|contract| contract.user_access_control);
    let grace = policy.and_then(|contract| contract.grace.as_ref());
    let grace_millis = grace.map_or(0, |contract| contract.timeout_millis);
    let grace_hold = grace.is_none_or(|contract| matches!(contract.mode, crate::oracle::GraceMode::HoldStream));
    let mut config = format!(
        "api:\n  host: 127.0.0.1\n  port: {}\n  web_root: {}\nworking_dir: {}\nbackup_dir: {}\nuser_access_control: {user_access_control}\nupdate_on_boot: true\nlog:\n  log_level: debug\nweb_ui:\n  enabled: true\n  user_ui_enabled: false\nreverse_proxy:\n  stream:\n    grace_period_millis: {grace_millis}\n    grace_period_hold_stream: {grace_hold}\n    shared_subscriber_idle_timeout_secs: 1\n",
        tuliprox_address.port(),
        yaml_scalar(web_root),
        yaml_scalar(storage_dir),
        yaml_scalar(backup_dir),
    );
    if let Some(policy) = policy {
        if let Some(strategies) = &policy.admission_strategies {
            if strategies.is_empty() {
                config.push_str("    admission_strategies: []\n");
            } else {
                config.push_str("    admission_strategies:\n");
                for strategy in strategies {
                    let encoded = serde_json::to_string(strategy).map_err(|error| {
                        TestkitError::Configuration(format!("cannot render admission strategy: {error}"))
                    })?;
                    let _ = writeln!(config, "      - {}", encoded.trim_matches('\"'));
                }
            }
        }
        if let Some(ttl_ms) = policy.recent_eviction_reentry_ttl_ms {
            let _ = writeln!(config, "    recent_eviction_reentry_ttl_ms: {ttl_ms}");
        }
    }
    let _ = writeln!(
        config,
        "  stream_history:\n    stream_history_enabled: true\n    stream_history_directory: {}\n  rewrite_secret: 0123456789abcdef0123456789abcdef",
        yaml_scalar(history_dir)
    );
    Ok(config)
}

fn render_sources(
    run_id: &str,
    origin_address: SocketAddr,
    channels: &HashMap<String, Channel>,
    policy: Option<&PolicyContract>,
    fixture_stream: &crate::config::FixtureStreamOptions,
    add_xtream_output: bool,
    input: &FixtureInputPlan<'_>,
) -> String {
    let mut markers = channels.values().map(|channel| channel.origin_marker).collect::<Vec<_>>();
    markers.sort_unstable();
    markers.dedup();
    if markers.is_empty() {
        markers.push(17);
    }
    let marker_comment = markers.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let provider_config = render_provider_input(run_id, origin_address, policy, input);
    let share_hls = fixture_stream.share_live_hls;
    let share_mpeg_ts = fixture_stream.share_live_mpeg_ts;
    let extra_output = if add_xtream_output { "          - type: xtream\n" } else { "" };
    format!(
        "inputs:\n{provider_config}sources:\n  - inputs:\n      - testkit-origin\n    targets:\n      - name: testkit\n        output:\n          - type: m3u\n{extra_output}        options:\n          share_live_streams:\n            hls: {share_hls}\n            mpeg_ts: {share_mpeg_ts}\n# Origin markers for this fixture: {marker_comment}\n"
    )
}

fn render_provider_input(
    run_id: &str,
    origin_address: SocketAddr,
    policy: Option<&PolicyContract>,
    input: &FixtureInputPlan<'_>,
) -> String {
    if input.is_stalker() {
        return render_stalker_input(origin_address, input.stalker);
    }
    render_provider_pool(run_id, origin_address, policy)
}

/// The fixture input for the emulated Stalker portal.
///
/// The portal URL keeps the `/stalker_portal/c/` web-UI path a real Ministra install uses;
/// the SUT derives its load endpoints from it. `endpoint_preference: server_load` pins the
/// endpoint the fixture answers on, so a scenario never depends on the fallback order.
fn render_stalker_input(origin_address: SocketAddr, stalker: &FixtureStalker) -> String {
    format!(
        "  - name: testkit-origin\n    type: stalker\n    url: 'http://{origin_address}/stalker_portal/c/'\n    stalker:\n      auth_mode: mac_only\n      mag_preset: {}\n      endpoint_preference: server_load\n      device:\n        mac_address: {}\n",
        yaml_string(&stalker.mag_preset),
        yaml_string(&stalker.mac_address)
    )
}

fn render_provider_pool(run_id: &str, origin_address: SocketAddr, policy: Option<&PolicyContract>) -> String {
    let accounts = policy.map(|contract| contract.provider_pool.as_slice()).unwrap_or_default();
    if let Some((root, aliases)) = accounts.split_first() {
        let mut rendered = format!(
            "  - name: testkit-origin\n    type: m3u\n    url: http://{origin_address}/catalog/input.m3u?run={run_id}&account={}\n    max_connections: {}\n",
            root.name, root.max_connections
        );
        if !aliases.is_empty() {
            rendered.push_str("    aliases:\n");
            for alias in aliases {
                let _ = writeln!(
                    rendered,
                    "      - name: {}\n        url: http://{origin_address}/catalog/input.m3u?run={run_id}&account={}\n        max_connections: {}",
                    yaml_string(&alias.name),
                    alias.name,
                    alias.max_connections
                );
            }
        }
        rendered
    } else {
        let provider_limit = policy
            .and_then(|contract| contract.provider_max_connections)
            .map_or_else(String::new, |limit| format!("    max_connections: {limit}\n"));
        format!(
            "  - name: testkit-origin\n    type: m3u\n    url: http://{origin_address}/catalog/input.m3u?run={run_id}\n{provider_limit}"
        )
    }
}

fn render_api_proxy(tuliprox_address: SocketAddr, policy: Option<&PolicyContract>) -> String {
    let mut output = format!(
        "server:\n  - name: testkit\n    protocol: http\n    host: 127.0.0.1\n    port: \"{}\"\n    timezone: UTC\n    message: Tuliprox testkit\nuser:\n  - target: testkit\n    credentials:\n",
        tuliprox_address.port()
    );
    if let Some(policy) = policy {
        for (username, user) in &policy.users {
            let _ = writeln!(
                output,
                "      - username: {}\n        password: {}\n        proxy: reverse\n        server: testkit\n        max_connections: {}\n        soft_connections: {}\n        status: Active",
                yaml_string(username),
                FIXTURE_PASSWORD,
                user.max_connections,
                user.soft_connections,
            );
        }
    }
    if !output.contains("      - username:") {
        output.push_str("      - username: testkit\n        password: testkit-password\n        proxy: reverse\n        server: testkit\n        status: Active\n");
    }
    output
}

fn yaml_scalar(path: &Path) -> String { yaml_string(&path.to_string_lossy()) }

fn yaml_string(value: &str) -> String { format!("'{}'", value.replace('\'', "''")) }

/// Owns only the SUT process launched for this run and waits for its explicit readiness endpoint.
pub struct IsolatedSut {
    child: Child,
}

/// Owns every process and every generated file used by one self-contained
/// Tuliprox scenario. The fixture never adopts an externally running process.
pub struct IsolatedFixture {
    pub paths: FixturePaths,
    pub base_url: String,
    pub origin_control_url: String,
    origin: Child,
    sut: IsolatedSut,
}

impl IsolatedFixture {
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        bootstrap: &BootstrapConfig,
        working_directory: &Path,
        testkit_binary: &Path,
        run_id: &str,
        origin_config: Option<&crate::config::OriginConfig>,
        policy: Option<&PolicyContract>,
        channels: &HashMap<String, Channel>,
        fixture_stream: &crate::config::FixtureStreamOptions,
        add_xtream_output: bool,
        input: &FixtureInputPlan<'_>,
    ) -> Result<Self, TestkitError> {
        const MAX_START_ATTEMPTS: usize = 5;
        let mut last_error = None;

        for attempt in 1..=MAX_START_ATTEMPTS {
            let attempt_run_id = if attempt == 1 { run_id.to_string() } else { format!("{run_id}-retry{attempt}") };
            match Self::start_attempt(
                bootstrap,
                working_directory,
                testkit_binary,
                &attempt_run_id,
                origin_config,
                policy,
                channels,
                fixture_stream,
                add_xtream_output,
                input,
            )
            .await
            {
                Ok(env) => {
                    if attempt > 1 {
                        eprintln!("Bootstrap succeeded on attempt {attempt}/{MAX_START_ATTEMPTS}");
                    }
                    return Ok(env);
                }
                Err(error) => {
                    eprintln!("Bootstrap start attempt {attempt}/{MAX_START_ATTEMPTS} failed: {error}; retrying with fresh ports...");
                    last_error = Some(error);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| TestkitError::Protocol("failed to bootstrap environment".to_owned())))
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_attempt(
        bootstrap: &BootstrapConfig,
        working_directory: &Path,
        testkit_binary: &Path,
        run_id: &str,
        origin_config: Option<&crate::config::OriginConfig>,
        policy: Option<&PolicyContract>,
        channels: &HashMap<String, Channel>,
        fixture_stream: &crate::config::FixtureStreamOptions,
        add_xtream_output: bool,
        input: &FixtureInputPlan<'_>,
    ) -> Result<Self, TestkitError> {
        let origin_address = reserve_loopback_address()?;
        let origin_control_address = reserve_loopback_address()?;
        let tuliprox_address = reserve_loopback_address()?;
        let paths = FixturePaths::create(
            run_id,
            origin_address,
            tuliprox_address,
            policy,
            channels,
            fixture_stream,
            add_xtream_output,
            input,
        )?;
        let markers = fixture_markers(channels);
        let mut origin_cmd = Command::new(testkit_binary);
        origin_cmd
            .arg("origin")
            .arg("--listen")
            .arg(origin_address.to_string())
            .arg("--control-listen")
            .arg(origin_control_address.to_string())
            .arg("--run-id")
            .arg(run_id)
            .arg("--markers")
            .arg(markers);
        if let Some(origin_cfg) = origin_config {
            if let Some(limit) = origin_cfg.account_limit {
                origin_cmd.arg("--account-limit").arg(limit.to_string());
            }
            if let Some(mode) = origin_cfg.limit_mode {
                let mode_str = match mode {
                    crate::origin_transport::OriginLimitMode::ObserveOnly => "observe_only",
                    crate::origin_transport::OriginLimitMode::RejectNew => "reject_new",
                    crate::origin_transport::OriginLimitMode::EvictOldest => "evict_oldest",
                };
                origin_cmd.arg("--limit-mode").arg(mode_str);
            }
        }
        if input.stalker.refuse_create_link_once {
            origin_cmd.arg("--stalker-refuse-create-link-once");
        }
        if input.stalker.separate_descriptor_command {
            origin_cmd.arg("--stalker-separate-descriptor-command");
        }
        if !input.stalker.refuse_create_link_markers.is_empty() {
            let markers =
                input.stalker.refuse_create_link_markers.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
            origin_cmd.arg("--stalker-refuse-create-link-markers").arg(markers);
        }
        let mut origin = origin_cmd.current_dir(working_directory).spawn()?;
        let origin_health = format!("http://{origin_control_address}/health");
        if let Err(error) = wait_for_http_ready(&mut origin, &origin_health, Duration::from_secs(10)).await {
            let _ = stop_child(&mut origin).await;
            return Err(error);
        }
        let base_url = format!("http://{tuliprox_address}");
        let rendered = render_bootstrap(bootstrap, &paths, &base_url)?;
        let sut = match IsolatedSut::start(&rendered, working_directory).await {
            Ok(sut) => sut,
            Err(error) => {
                let _ = stop_child(&mut origin).await;
                return Err(error);
            }
        };
        let origin_control_url = format!("http://{origin_control_address}");
        Ok(Self { paths, base_url, origin_control_url, origin, sut })
    }

    pub async fn stop(mut self) -> Result<(), TestkitError> {
        let sut_result = self.sut.stop().await;
        let origin_result = stop_child(&mut self.origin).await;
        match (sut_result, origin_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(sut_error), Err(origin_error)) => Err(TestkitError::Protocol(format!(
                "Tuliprox cleanup failed ({sut_error}); origin cleanup failed ({origin_error})"
            ))),
        }
    }
}

fn reserve_loopback_address() -> Result<SocketAddr, TestkitError> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    drop(listener);
    Ok(address)
}

fn fixture_markers(channels: &HashMap<String, Channel>) -> String {
    let mut markers = channels.values().map(|channel| channel.origin_marker).collect::<Vec<_>>();
    markers.sort_unstable();
    markers.dedup();
    if markers.is_empty() {
        markers.push(17);
    }
    markers.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
}

fn render_bootstrap(
    bootstrap: &BootstrapConfig,
    paths: &FixturePaths,
    base_url: &str,
) -> Result<BootstrapConfig, TestkitError> {
    let render = |value: &str| {
        value
            .replace("{config_file}", &paths.config_file.to_string_lossy())
            .replace("{source_file}", &paths.source_file.to_string_lossy())
            .replace("{api_proxy_file}", &paths.api_proxy_file.to_string_lossy())
            .replace("{run_directory}", &paths.root.to_string_lossy())
            .replace("{base_url}", base_url)
    };
    let command = resolve_environment_command(&render(&bootstrap.command))?;
    Ok(BootstrapConfig {
        command,
        arguments: bootstrap.arguments.iter().map(|argument| render(argument)).collect(),
        readiness_url: render(&bootstrap.readiness_url),
        readiness_timeout_millis: bootstrap.readiness_timeout_millis,
    })
}

fn resolve_environment_command(command: &str) -> Result<String, TestkitError> {
    let Some(variable) = command.strip_prefix("${env:").and_then(|value| value.strip_suffix('}')) else {
        return Ok(command.to_owned());
    };
    std::env::var(variable).map_err(|_| {
        TestkitError::Configuration(format!("bootstrap command environment variable {variable} is not set"))
    })
}

async fn wait_for_http_ready(child: &mut Child, readiness_url: &str, timeout: Duration) -> Result<(), TestkitError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let client = reqwest::Client::new();
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(TestkitError::Protocol(format!("fixture process exited before readiness: {status}")));
        }
        if client.get(readiness_url).send().await.is_ok_and(|response| response.status().is_success()) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(TestkitError::Protocol(format!("fixture process did not become ready at {readiness_url}")));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn stop_child(child: &mut Child) -> Result<(), TestkitError> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    child.start_kill()?;
    tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .map_err(|_| TestkitError::Protocol("fixture process did not exit during cleanup".to_owned()))??;
    Ok(())
}

impl IsolatedSut {
    pub async fn start(config: &BootstrapConfig, working_directory: &Path) -> Result<Self, TestkitError> {
        if config.command.trim().is_empty() || config.readiness_url.trim().is_empty() {
            return Err(TestkitError::Configuration("bootstrap command and readiness URL are required".to_owned()));
        }
        let child = Command::new(&config.command).args(&config.arguments).current_dir(working_directory).spawn()?;
        let mut sut = Self { child };
        if let Err(startup_error) =
            sut.wait_ready(&config.readiness_url, Duration::from_millis(config.readiness_timeout_millis.max(1))).await
        {
            if let Err(cleanup_error) = sut.stop().await {
                return Err(TestkitError::Protocol(format!(
                    "isolated Tuliprox startup failed ({startup_error}) and cleanup failed ({cleanup_error})"
                )));
            }
            return Err(startup_error);
        }
        Ok(sut)
    }

    async fn wait_ready(&mut self, readiness_url: &str, timeout: Duration) -> Result<(), TestkitError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let client = reqwest::Client::new();
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Err(TestkitError::Protocol(format!(
                    "isolated Tuliprox process exited before readiness: {status}"
                )));
            }
            if let Ok(response) = client.get(readiness_url).send().await {
                if response.status().is_success() {
                    return Ok(());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(TestkitError::Protocol("isolated Tuliprox process did not become ready".to_owned()));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn stop(mut self) -> Result<(), TestkitError> {
        if self.child.try_wait()?.is_none() {
            self.child.start_kill()?;
            self.child.wait().await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn early_process_exit_does_not_wait_for_the_readiness_deadline() {
        let config = BootstrapConfig {
            command: "/bin/false".to_owned(),
            arguments: Vec::new(),
            readiness_url: "http://127.0.0.1:1/ready".to_owned(),
            readiness_timeout_millis: 5_000,
        };
        let started_at = tokio::time::Instant::now();
        let result = IsolatedSut::start(&config, Path::new("/tmp")).await;
        assert!(matches!(result, Err(TestkitError::Protocol(message)) if message.contains("exited before readiness")));
        assert!(started_at.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn fixture_files_are_run_scoped_and_contain_no_operator_paths() -> Result<(), TestkitError> {
        let policy = PolicyContract {
            user_access_control: true,
            users: HashMap::from([(
                "fixture-user".to_owned(),
                crate::config::UserPolicy { max_connections: 1, soft_connections: 0 },
            )]),
            admission_strategies: Some(vec![crate::oracle::AdmissionStrategy::EvictUserSameIpLatest]),
            recent_eviction_reentry_ttl_ms: None,
            grace: None,
            provider_max_connections: None,
            provider_pool: Vec::new(),
            expected_provider_slots: None,
        };
        let channels = HashMap::from([(
            "fixture-channel".to_owned(),
            Channel { origin_marker: 17, protocol: "xtream_ts".to_owned() },
        )]);
        let fixture = FixturePaths::create(
            "fixture-run",
            "127.0.0.1:9910".parse::<SocketAddr>().map_err(|error| TestkitError::Configuration(error.to_string()))?,
            "127.0.0.1:8901".parse::<SocketAddr>().map_err(|error| TestkitError::Configuration(error.to_string()))?,
            Some(&policy),
            &channels,
            &crate::config::FixtureStreamOptions::default(),
            false,
            &FixtureInputPlan {
                input_type: crate::config::FixtureInputType::M3u,
                stalker: &crate::config::FixtureStalker::default(),
            },
        )?;
        let config = std::fs::read_to_string(&fixture.config_file)?;
        let source = std::fs::read_to_string(&fixture.source_file)?;
        let api_proxy = std::fs::read_to_string(&fixture.api_proxy_file)?;

        assert!(config.contains(fixture.root.to_string_lossy().as_ref()));
        assert!(source.contains("fixture-run"));
        assert!(api_proxy.contains("fixture-user"));
        let expected_paths = [
            fixture.root.join("web"),
            fixture.root.join("storage"),
            fixture.root.join("backup"),
            fixture.root.join("history"),
        ];
        for expected in &expected_paths {
            assert!(
                expected.starts_with(&fixture.root),
                "fixture path {} must stay under the fixture root {}",
                expected.display(),
                fixture.root.display()
            );
            assert!(
                config.contains(expected.to_string_lossy().as_ref()),
                "rendered config must reference fixture path {}",
                expected.display()
            );
        }
        Ok(())
    }

    fn m3u_input() -> FixtureInputPlan<'static> {
        FixtureInputPlan {
            input_type: crate::config::FixtureInputType::M3u,
            stalker: Box::leak(Box::new(crate::config::FixtureStalker::default())),
        }
    }

    #[test]
    fn stalker_input_renders_a_portal_url_and_device_identity() {
        let stalker = crate::config::FixtureStalker {
            mac_address: "00:1A:79:00:00:09".to_owned(),
            mag_preset: "ministra_modern".to_owned(),
            refuse_create_link_once: true,
            refuse_create_link_markers: vec![19],
            separate_descriptor_command: false,
        };
        let plan = FixtureInputPlan { input_type: crate::config::FixtureInputType::Stalker, stalker: &stalker };
        let source = render_sources(
            "run",
            "127.0.0.1:9910".parse().unwrap(),
            &HashMap::new(),
            None,
            &crate::config::FixtureStreamOptions::default(),
            false,
            &plan,
        );

        assert!(source.contains("type: stalker"), "{source}");
        assert!(source.contains("http://127.0.0.1:9910/stalker_portal/c/"), "{source}");
        assert!(source.contains("mac_address: '00:1A:79:00:00:09'"), "{source}");
        assert!(source.contains("mag_preset: 'ministra_modern'"), "{source}");
        assert!(!source.contains("catalog/input.m3u"), "{source}");
    }

    #[test]
    fn render_sources_with_mpeg_ts_sharing_disabled() {
        let opts = crate::config::FixtureStreamOptions { share_live_hls: true, share_live_mpeg_ts: false };
        let source =
            render_sources("run", "127.0.0.1:9910".parse().unwrap(), &HashMap::new(), None, &opts, false, &m3u_input());
        assert!(source.contains("mpeg_ts: false"));
        assert!(source.contains("hls: true"));
        assert!(!source.contains("type: xtream"));
    }

    #[test]
    fn render_sources_includes_xtream_output_when_requested() {
        let opts = crate::config::FixtureStreamOptions::default();
        let source =
            render_sources("run", "127.0.0.1:9910".parse().unwrap(), &HashMap::new(), None, &opts, true, &m3u_input());
        assert!(source.contains("type: xtream"));
    }

    #[test]
    fn render_sources_omits_xtream_output_by_default() {
        let opts = crate::config::FixtureStreamOptions::default();
        let source =
            render_sources("run", "127.0.0.1:9910".parse().unwrap(), &HashMap::new(), None, &opts, false, &m3u_input());
        assert!(!source.contains("type: xtream"));
        assert!(source.contains("mpeg_ts: true"));
    }
}
