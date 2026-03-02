use crate::api::model::AppState;
use crate::model::ConfigProvider;
use crate::utils::read_sources_file_from_path;
use log::{debug, warn};
use shared::model::{DnsPrefer, OnResolveErrorPolicy, SourcesConfigDto};
use std::collections::HashSet;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::net::lookup_host;
use tokio_util::sync::CancellationToken;

fn filter_by_preference(ips: Vec<IpAddr>, prefer: DnsPrefer) -> Vec<IpAddr> {
    match prefer {
        DnsPrefer::System => ips,
        DnsPrefer::Ipv4 => ips.into_iter().filter(IpAddr::is_ipv4).collect(),
        DnsPrefer::Ipv6 => ips.into_iter().filter(IpAddr::is_ipv6).collect(),
    }
}

fn dedup_keep_order(ips: Vec<IpAddr>) -> Vec<IpAddr> {
    let mut seen = HashSet::new();
    ips.into_iter().filter(|ip| seen.insert(*ip)).collect()
}

async fn resolve_hostname(hostname: &str, prefer: DnsPrefer, max_addrs: Option<usize>) -> std::io::Result<Vec<IpAddr>> {
    let addrs = lookup_host((hostname, 0)).await?;
    let mut ips: Vec<IpAddr> = addrs.map(|addr| addr.ip()).collect();
    ips = dedup_keep_order(ips);
    ips = filter_by_preference(ips, prefer);
    if let Some(max) = max_addrs.filter(|max| *max > 0) {
        ips.truncate(max);
    }
    Ok(ips)
}

#[derive(Debug, Default)]
struct ProviderResolveStats {
    total: usize,
    overridden: usize,
    resolved: usize,
    empty: usize,
    failed: usize,
}

async fn resolve_provider(provider: &Arc<ConfigProvider>) -> ProviderResolveStats {
    let mut stats = ProviderResolveStats::default();
    let Some(dns_cfg) = provider.get_dns_config().cloned() else {
        return stats;
    };
    if !dns_cfg.enabled {
        return stats;
    }

    let hostnames = provider.hostnames_from_urls();
    stats.total = hostnames.len();
    if hostnames.is_empty() {
        debug!(
            "Provider dns task '{}' found no hostname URLs to resolve (urls={:?})",
            provider.name, provider.urls
        );
        return stats;
    }

    for host in hostnames {
        if let Some(overridden) = dns_cfg.overrides.get(&host) {
            provider.store_resolved(&host, overridden.clone());
            stats.overridden += 1;
            stats.resolved += 1;
            debug!("Provider dns '{}' host '{}' resolved from override: {:?}", provider.name, host, overridden);
            continue;
        }

        match resolve_hostname(&host, dns_cfg.prefer, dns_cfg.max_addrs).await {
            Ok(ips) if !ips.is_empty() => {
                debug!("Provider dns '{}' host '{}' resolved: {:?}", provider.name, host, ips);
                provider.store_resolved(&host, ips);
                stats.resolved += 1;
            }
            Ok(_) => {
                stats.empty += 1;
                provider.mark_resolve_error(&host, "DNS resolution returned no addresses");
                if dns_cfg.on_resolve_error == OnResolveErrorPolicy::FallbackToHostname {
                    provider.clear_resolved(&host);
                }
                warn!(
                    "Provider dns '{}' host '{}' returned empty address set (policy={:?})",
                    provider.name, host, dns_cfg.on_resolve_error
                );
            }
            Err(err) => {
                stats.failed += 1;
                provider.mark_resolve_error(&host, err.to_string());
                if dns_cfg.on_resolve_error == OnResolveErrorPolicy::FallbackToHostname {
                    provider.clear_resolved(&host);
                }
                warn!("provider dns resolve failed for '{}' host '{}': {err}", provider.name, host);
            }
        }
    }

    stats
}

fn serialize_sources_for_persist(sources: &SourcesConfigDto) -> io::Result<String> {
    let mut serialized = String::new();
    let options = serde_saphyr::SerializerOptions {
        prefer_block_scalars: false,
        ..Default::default()
    };
    serde_saphyr::to_fmt_writer_with_options(&mut serialized, sources, options)
        .map_err(|err| io::Error::other(format!("Could not serialize source.yml: {err}")))?;
    Ok(serialized)
}

async fn write_sources_file_force(path: &Path, sources: &SourcesConfigDto) -> io::Result<()> {
    let serialized = serialize_sources_for_persist(sources)?;
    let parent_dir = path.parent().ok_or_else(|| {
        io::Error::other(format!(
            "Could not write source.yml '{}': missing parent directory",
            path.display()
        ))
    })?;
    let dest_file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("source.yml");
    let mut tmp_path = parent_dir.to_path_buf();
    tmp_path.push(format!(
        ".{dest_file_name}.tmp-{}-{}",
        std::process::id(),
        chrono::Local::now().timestamp_nanos_opt().unwrap_or_default()
    ));

    fs::write(&tmp_path, serialized).await?;
    match fs::rename(&tmp_path, path).await {
        Ok(()) => Ok(()),
        Err(err) => {
            #[cfg(windows)]
            {
                // Try to rename again after removing destination (Windows often needs this)
                if let Ok(()) = fs::remove_file(path).await {
                    if fs::rename(&tmp_path, path).await.is_ok() {
                        return Ok(());
                    }
                    // Rename still failed after removing dest - fall through to clean up
                }
            }
            let _ = fs::remove_file(&tmp_path).await;
            Err(io::Error::other(format!(
                "Could not replace '{}' with '{}': {err}",
                path.display(),
                tmp_path.display()
            )))
        }
    }
}

async fn persist_provider_resolved_to_source_file(app_state: &Arc<AppState>, provider: &Arc<ConfigProvider>) {
    let source_file = {
        let paths = app_state.app_config.paths.load();
        paths.sources_file_path.clone()
    };
    let source_path = PathBuf::from(&source_file);
    let _lock = app_state.app_config.file_locks.write_lock(&source_path).await;

    let mut sources_dto = match read_sources_file_from_path(&source_path, false, false, None).await {
        Ok(dto) => dto,
        Err(err) => {
            warn!(
                "Provider dns '{}' failed to read source.yml '{}': {err}",
                provider.name,
                source_path.display()
            );
            return;
        }
    };

    let Some(provider_dtos) = sources_dto.provider.as_mut() else {
        debug!(
            "Provider dns '{}' source.yml '{}' has no provider section to persist resolved values",
            provider.name,
            source_path.display()
        );
        return;
    };

    let Some(provider_dto) = provider_dtos.iter_mut().find(|dto| dto.name.as_ref() == provider.name.as_ref()) else {
        warn!(
            "Provider dns '{}' not found in source.yml '{}', cannot persist resolved values",
            provider.name,
            source_path.display()
        );
        return;
    };

    let Some(dns_dto) = provider_dto.dns.as_mut() else {
        debug!(
            "Provider dns '{}' has no dns section in source.yml '{}', skipping resolved persist",
            provider.name,
            source_path.display()
        );
        return;
    };

    let resolved_hosts = {
        let snapshot = provider.snapshot_resolved();
        dns_dto.resolved = (!snapshot.is_empty()).then_some(snapshot);
        dns_dto.resolved.as_ref().map_or(0, std::collections::HashMap::len)
    };

    match write_sources_file_force(&source_path, &sources_dto).await {
        Ok(()) => {
            debug!(
                "Provider dns '{}' persisted dns.resolved to '{}' (hosts={resolved_hosts})",
                provider.name,
                source_path.display()
            );
        }
        Err(err) => {
            warn!(
                "Provider dns '{}' failed to persist dns.resolved to '{}': {err}",
                provider.name,
                source_path.display()
            );
        }
    }
}

fn spawn_provider_dns_task(app_state: Arc<AppState>, provider_name: Arc<str>, cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut refresh_secs = 300_u64;
        {
            let sources = app_state.app_config.sources.load();
            if let Some(provider) = sources.get_provider_by_name(provider_name.as_ref()) {
                refresh_secs = provider.get_dns_config().map_or(300, |dns| dns.refresh_secs.max(10));
            }
        }
        // Add initial jitter (0-10% of refresh interval) to prevent thundering herd
        let jitter_ms = (rand::random::<u64>() % (refresh_secs * 100)).min(5000);
        tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
        debug!("Starting provider dns task for '{provider_name}' (refresh={refresh_secs}s)");
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    debug!("Stopping provider dns task for '{provider_name}'");
                    break;
                }
                () = async {
                    let start = Instant::now();
                    debug!("Provider dns tick '{provider_name}' started");
                    let provider = {
                        let sources = app_state.app_config.sources.load();
                        sources.get_provider_by_name(provider_name.as_ref()).cloned()
                    };
                    let Some(provider) = provider else {
                        warn!("Provider dns '{provider_name}' not found in runtime sources, retrying");
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        return;
                    };
                    refresh_secs = provider.get_dns_config().map_or(300, |dns| dns.refresh_secs.max(10));
                    let stats = resolve_provider(&provider).await;
                    persist_provider_resolved_to_source_file(&app_state, &provider).await;
                    let cache_hosts = provider.snapshot_resolved().len();
                    debug!(
                        "Provider dns tick '{}' finished: total_hosts={} resolved={} overridden={} empty={} failed={} cache_hosts={} elapsed_ms={}",
                        provider.name,
                        stats.total,
                        stats.resolved,
                        stats.overridden,
                        stats.empty,
                        stats.failed,
                        cache_hosts,
                        start.elapsed().as_millis(),
                    );
                    debug!("Provider dns '{}' next tick in {}s", provider.name, refresh_secs);
                    tokio::time::sleep(Duration::from_secs(refresh_secs)).await;
                } => {}
            }
        }
    });
}

pub fn exec_provider_dns(app_state: &Arc<AppState>, cancel: &CancellationToken) {
    let sources = app_state.app_config.sources.load();
    let provider_names: Vec<_> = sources
        .provider
        .iter()
        .filter(|provider| provider.get_dns_config().is_some_and(|dns| dns.enabled))
        .map(|provider| provider.name.clone())
        .collect();
    drop(sources);

    if provider_names.is_empty() {
        debug!("Provider dns manager: no enabled providers found");
        return;
    }

    debug!("Provider dns manager: starting {} provider task(s)", provider_names.len());

    for provider_name in provider_names {
        spawn_provider_dns_task(Arc::clone(app_state), provider_name, cancel.clone());
    }
}
