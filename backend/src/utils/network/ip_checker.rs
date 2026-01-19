use crate::model::IpCheckConfig;
use regex::Regex;
use shared::error::{TuliproxError, TuliproxErrorKind};
use shared::utils::sanitize_sensitive_info;
use std::sync::Arc;

async fn fetch_ip(
    client: &reqwest::Client,
    url: &str,
    regex: Option<&Arc<Regex>>,
) -> Result<String, TuliproxError> {
    let response = client.get(url).send().await.map_err(|e| {
        TuliproxError::new(
            TuliproxErrorKind::Info,
            format!("Failed to request {}: {e}", sanitize_sensitive_info(url)),
        )
    })?;

    let text = response.text().await.map_err(|e| {
        TuliproxError::new(
            TuliproxErrorKind::Info,
            format!("Failed to read response: {e}"),
        )
    })?;

    if let Some(re) = regex {
        return if let Some(caps) = re.captures(&text) {
            if let Some(m) = caps.get(1) {
                Ok(m.as_str().to_string())
            } else {
                Err(TuliproxError::new(
                    TuliproxErrorKind::Info,
                    "Regex matched but no group found".to_string(),
                ))
            }
        } else {
            Err(TuliproxError::new(
                TuliproxErrorKind::Info,
                "Regex did not match".to_string(),
            ))
        };
    }

    Ok(text.trim().to_string())
}

/// Fetch both IPs from a shared URL (if both regex patterns are available)
async fn fetch_combined_ips(
    client: &reqwest::Client,
    config: &IpCheckConfig,
    url: &str,
) -> (Option<String>, Option<String>) {
    let response = client.get(url).send().await.ok();
    let text = match response {
        Some(r) => r.text().await.ok(),
        None => None,
    };

    if let Some(body) = text {
        let ipv4 = config
            .pattern_ipv4
            .as_ref()
            .and_then(|re| re.captures(&body))
            .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()));

        let ipv6 = config
            .pattern_ipv6
            .as_ref()
            .and_then(|re| re.captures(&body))
            .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()));

        (ipv4, ipv6)
    } else {
        (None, None)
    }
}

/// Fetch both IPv4 and IPv6 addresses, using separate or combined URL(s)
pub async fn get_ips(
    client: &reqwest::Client,
    config: &IpCheckConfig,
) -> Result<(Option<String>, Option<String>), TuliproxError> {
    match (&config.url_ipv4, &config.url_ipv6, &config.url) {
        // Both dedicated URLs provided
        (Some(url_v4), Some(url_v6), _) => {
            let (ipv4, ipv6) = tokio::join!(
                fetch_ip(client, url_v4, config.pattern_ipv4.as_ref()),
                fetch_ip(client, url_v6, config.pattern_ipv6.as_ref())
            );
            Ok((ipv4.ok(), ipv6.ok()))
        }

        // Only one combined URL provided
        (_, _, Some(shared_url)) => {
            let result = fetch_combined_ips(client, config, shared_url).await;
            Ok(result)
        }

        // Only one dedicated URL
        (Some(url_v4), None, _) => {
            let ipv4 = fetch_ip(client, url_v4, config.pattern_ipv4.as_ref())
                .await
                .ok();
            Ok((ipv4, None))
        }
        (None, Some(url_v6), _) => {
            let ipv6 = fetch_ip(client, url_v6, config.pattern_ipv6.as_ref())
                .await
                .ok();
            Ok((None, ipv6))
        }

        // No URLs given
        _ => Err(TuliproxError::new(
            TuliproxErrorKind::Info,
            "No valid IP-check URLs provided".to_owned(),
        )),
    }
}
