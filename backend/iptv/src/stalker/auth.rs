use crate::stalker::{
    action::StalkerAction,
    client::StalkerApiClient,
    error::{safe_stalker_url, StalkerError, StalkerResult},
    presets::stalker_mag_preset_spec,
    profile::{StalkerHandshake, StalkerProviderProfile, StalkerRawProviderProfile},
    recipes::{detect_fingerprint, fallback_recipes_for, recipe_spec_for},
    session::StalkerSession,
    transport::StalkerTransport,
    url_factory::StalkerLoadUrl,
};
use log::{debug, info, warn};
use serde::Deserialize;
use serde_json::Value;
use shared::model::stalker::{StalkerAuthMode, StalkerBootstrapRecipe, StalkerPortalCapabilitiesDto};
use tuliprox_core::utils::Clock;

/// The fields we expect in a successful handshake response. The portal wraps the result
/// in `{"js": {...}}` — we accept both wrapped and unwrapped shapes for robustness.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct StalkerHandshakeResponse {
    pub js: Option<StalkerHandshakeJs>,
    pub text: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub status: Option<i32>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct StalkerHandshakeJs {
    pub token: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub mac: Option<String>,
    #[serde(default)]
    pub module: Option<String>,
    #[serde(default)]
    pub expires: Option<String>,
}

/// Drive the full handshake against the configured portal. The function iterates through
/// the recipe chain derived from the user-supplied auth mode, re-issuing the handshake
/// call against the next recipe on any 4xx/5xx. Once a recipe succeeds we follow up with
/// a `get_profile` call to extract account info and a `get_capabilities` call (best
/// effort) to populate the `StalkerPortalCapabilitiesDto`.
pub async fn handshake<Tr: StalkerTransport, C: Clock>(client: &StalkerApiClient<Tr, C>) -> StalkerResult<StalkerHandshake> {
    let config = client.config();
    let preset = config.mag_preset;
    // Start from the recipe that last worked. The rest of the chain still follows, so a
    // portal that has changed its mind is still reachable - this only spares us replaying
    // four rejected handshakes against a portal whose answer we already know, which is
    // the part that looks like credential stuffing from the provider's side.
    let chain = prefer_remembered_recipe(client, fallback_recipes_for(config.auth_mode, preset));
    let mut last_err: Option<StalkerError> = None;
    for recipe in chain {
        match attempt_recipe(client, recipe).await {
            Ok(handshake) => {
                info!("Stalker handshake succeeded with recipe {recipe:?}");
                client.record_successful_handshake(&format!("{recipe:?}"), &handshake.session.load_url);
                return Ok(handshake);
            }
            Err(err) => {
                warn!("Stalker handshake recipe {recipe:?} failed: {err}");
                // The loop body intentionally does not short-circuit on token rejection
                // or `HandshakeFailed` — those errors are still recorded as `last_err`
                // and the next recipe is tried, so the eventual caller sees the most
                // recent error from the full recipe chain.
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| StalkerError::RecipesExhausted { portal: safe_stalker_url(client.portal_url()) }))
}

/// Move the remembered recipe to the front of `chain`, keeping the rest in order.
fn prefer_remembered_recipe<Tr: StalkerTransport, C: Clock>(
    client: &StalkerApiClient<Tr, C>,
    chain: Vec<StalkerBootstrapRecipe>,
) -> Vec<StalkerBootstrapRecipe> {
    let Some(remembered) = client.remembered_recipe() else {
        return chain;
    };
    let Some(position) = chain.iter().position(|recipe| format!("{recipe:?}") == remembered) else {
        return chain;
    };
    let mut chain = chain;
    chain.swap(0, position);
    chain
}

async fn attempt_recipe<Tr: StalkerTransport, C: Clock>(client: &StalkerApiClient<Tr, C>, recipe: StalkerBootstrapRecipe) -> StalkerResult<StalkerHandshake> {
    let spec = recipe_spec_for(recipe);
    let candidates = client.ordered_load_urls();
    if candidates.is_empty() {
        return Err(StalkerError::NoEndpoint { portal: safe_stalker_url(client.portal_url()) });
    }
    let mut last_err: Option<StalkerError> = None;
    for load_url in candidates {
        match perform_handshake_against(client, &load_url, &spec).await {
            Ok((mut session, handshake_status)) => {
                if spec.emit_handshake_extra {
                    if let Err(err) = perform_handshake_extra(client, &mut session, &load_url, &spec).await {
                        warn!("Stalker handshake-extra failed: {err}");
                        last_err = Some(err);
                        continue;
                    }
                }
                if spec.require_portal_handshake {
                    if let Err(err) = perform_portal_handshake(client, &session, &load_url, &spec).await {
                        warn!("Stalker portal handshake failed: {err}");
                        last_err = Some(err);
                        continue;
                    }
                }
                if let Some((login, password)) = account_credentials(client) {
                    if let Err(err) = perform_do_auth(client, &session, &load_url, &spec, &login, &password).await {
                        warn!("Stalker do_auth failed: {err}");
                        last_err = Some(err);
                        continue;
                    }
                }
                // A profile failure on this endpoint should not abort the whole recipe:
                // fall through to the next load-url candidate instead.
                let raw_profile = match fetch_profile(client, &session, &load_url, &spec).await {
                    Ok(profile) => profile,
                    Err(err) => {
                        warn!("Stalker get_profile failed: {err}");
                        last_err = Some(err);
                        continue;
                    }
                };
                let fingerprint =
                    detect_fingerprint(handshake_status, &session.fingerprint_evidence, client.config().mag_preset);
                debug!("Stalker portal fingerprint for {}: {fingerprint:?}", safe_stalker_url(&load_url.load_url));
                let capabilities = fetch_capabilities(client, &session, &load_url, &spec).await.unwrap_or_default();
                let size_caps = client.config().size_caps.unwrap_or_default();
                let profile = StalkerProviderProfile::from_config(
                    client.config(),
                    raw_profile,
                    recipe,
                    fallback_recipes_for(client.config().auth_mode, client.config().mag_preset),
                    capabilities,
                    size_caps,
                    client.config().username.clone(),
                    client.config().password.clone(),
                );
                return Ok(StalkerHandshake { session, profile });
            }
            Err(err) => {
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| StalkerError::HandshakeFailed {
        message: "no endpoint accepted the handshake".to_string(),
        url: None,
    }))
}

async fn perform_handshake_against<Tr: StalkerTransport, C: Clock>(
    client: &StalkerApiClient<Tr, C>,
    load_url: &StalkerLoadUrl,
    spec: &crate::stalker::recipes::StalkerRecipeSpec,
) -> StalkerResult<(StalkerSession, u16)> {
    let config = client.config();
    let preset_spec = stalker_mag_preset_spec(config.mag_preset);
    let mut builder = client.get(&load_url.load_url).headers(client.common_headers(load_url)).query(&[
        ("type", "stb"),
        ("action", "handshake"),
        ("JsHttpRequest", "1-xml"),
        ("HttpRequest", "1-xml"),
    ]);
    builder = client.apply_mac_query(builder);
    builder = client.apply_bearer(builder, None, spec.token_in_query || preset_spec.emit_token_query);
    let response = client.send_with_cap(builder, StalkerAction::Handshake, client.cap_for_action(StalkerAction::Handshake)).await?;
    client.ingest_response_cookies(&response);
    let status = response.status();
    let body = client.read_body_with_cap(response, StalkerAction::Handshake, client.cap_for_action(StalkerAction::Handshake)).await?;
    if !status.is_success() {
        if matches!(status.as_u16(), 401 | 403 | 456) {
            return Err(StalkerError::TokenRejected {
                status: status.as_u16(),
                url: Some(load_url.load_url.as_str().into()),
            });
        }
        return Err(StalkerError::HandshakeFailed {
            message: format!("handshake status {}", status.as_u16()),
            url: Some(load_url.load_url.as_str().into()),
        });
    }
    let parsed: StalkerHandshakeResponse = client.decode_body_bytes(&body, StalkerAction::Handshake)?;
    let token = parsed.js.as_ref().and_then(|js| js.token.clone()).or(parsed.token.clone()).or_else(|| {
        // Some portals stash the token under the `text` field as a stringified object.
        parsed.text.as_ref().and_then(|t| {
            serde_json::from_str::<Value>(t)
                .ok()
                .and_then(|v| v.get("token").and_then(|t| t.as_str().map(String::from)))
        })
    });
    let Some(token) = token else {
        return Err(StalkerError::HandshakeFailed {
            message: "no token in handshake response".to_string(),
            url: Some(load_url.load_url.as_str().into()),
        });
    };
    let evidence = parsed
        .js
        .as_ref()
        .map(|js| {
            let mut keys = Vec::new();
            if js.id.is_some() {
                keys.push("js.id".to_string());
            }
            if js.token.is_some() {
                keys.push("js.token".to_string());
            }
            if js.mac.is_some() {
                keys.push("js.mac".to_string());
            }
            if js.module.is_some() {
                keys.push("js.module".to_string());
            }
            keys
        })
        .unwrap_or_default();
    let session = StalkerSession::new(token, load_url.referer.clone(), load_url.load_url.clone());
    Ok((session.with_evidence(evidence), status.as_u16()))
}

/// The account credentials to authenticate with, when the auth mode wants them. `MacOnly`
/// explicitly opts out; every other mode forwards configured non-blank credentials.
fn account_credentials<Tr: StalkerTransport, C: Clock>(client: &StalkerApiClient<Tr, C>) -> Option<(String, String)> {
    let config = client.config();
    if matches!(config.auth_mode, StalkerAuthMode::MacOnly) {
        return None;
    }
    let username = config.username.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
    let password = config.password.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
    Some((username.to_string(), password.to_string()))
}

/// Authenticate the account on the portal (`action=do_auth`). Stalker portals that pair
/// MAC identities with account credentials reject all catalog calls until this step has
/// been performed once per session.
async fn perform_do_auth<Tr: StalkerTransport, C: Clock>(
    client: &StalkerApiClient<Tr, C>,
    session: &StalkerSession,
    load_url: &StalkerLoadUrl,
    spec: &crate::stalker::recipes::StalkerRecipeSpec,
    login: &str,
    password: &str,
) -> StalkerResult<()> {
    let mut builder = client.get(&load_url.load_url).headers(client.common_headers(load_url)).query(&[
        ("type", "stb"),
        ("action", "do_auth"),
        ("login", login),
        ("password", password),
        ("JsHttpRequest", "1-xml"),
    ]);
    builder = client.apply_mac_query(builder);
    builder = client.apply_bearer(builder, Some(session), spec.token_in_query);
    let value: Value = client.send_json(builder, StalkerAction::DoAuth).await?;
    // Portals answer `{"js": true}` on success and `{"js": false}` on bad credentials;
    // anything else (object payloads, missing key) is treated as success.
    if matches!(value.get("js"), Some(Value::Bool(false))) {
        return Err(StalkerError::HandshakeFailed {
            message: "portal rejected do_auth credentials".to_string(),
            url: Some(load_url.load_url.as_str().into()),
        });
    }
    Ok(())
}

async fn perform_handshake_extra<Tr: StalkerTransport, C: Clock>(
    client: &StalkerApiClient<Tr, C>,
    session: &mut StalkerSession,
    load_url: &StalkerLoadUrl,
    spec: &crate::stalker::recipes::StalkerRecipeSpec,
) -> StalkerResult<()> {
    let mut builder = client
        .get(&load_url.load_url)
        .headers(client.common_headers(load_url))
        .query(&[("type", "stb"), ("action", "handshake-extra")]);
    builder = client.apply_mac_query(builder);
    builder = client.apply_bearer(builder, Some(session), spec.token_in_query);
    let response = client.send_with_cap(builder, StalkerAction::HandshakeExtra, client.cap_for_action(StalkerAction::HandshakeExtra)).await?;
    client.ingest_response_cookies(&response);
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(StalkerError::BadStatus {
            status: status.as_u16(),
            action: StalkerAction::HandshakeExtra,
            body_snippet: String::new(),
        })
    }
}

async fn perform_portal_handshake<Tr: StalkerTransport, C: Clock>(
    client: &StalkerApiClient<Tr, C>,
    session: &StalkerSession,
    load_url: &StalkerLoadUrl,
    spec: &crate::stalker::recipes::StalkerRecipeSpec,
) -> StalkerResult<()> {
    let target = load_url;
    let mut builder = client
        .get(&target.load_url)
        .headers(client.common_headers(target))
        .query(&[("type", "stb"), ("action", "handshake")]);
    builder = client.apply_mac_query(builder);
    builder = client.apply_bearer(builder, Some(session), spec.token_in_query);
    let response = client.send_with_cap(builder, StalkerAction::HandshakePortal, client.cap_for_action(StalkerAction::HandshakePortal)).await?;
    client.ingest_response_cookies(&response);
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(StalkerError::BadStatus {
            status: status.as_u16(),
            action: StalkerAction::HandshakePortal,
            body_snippet: String::new(),
        })
    }
}

async fn fetch_profile<Tr: StalkerTransport, C: Clock>(
    client: &StalkerApiClient<Tr, C>,
    session: &StalkerSession,
    load_url: &StalkerLoadUrl,
    spec: &crate::stalker::recipes::StalkerRecipeSpec,
) -> StalkerResult<StalkerRawProviderProfile> {
    let mut builder = client.get(&load_url.load_url).headers(client.common_headers(load_url)).query(&[
        ("type", "stb"),
        ("action", "get_profile"),
        ("HttpRequest", "1-xml"),
    ]);
    builder = client.apply_mac_query(builder);
    builder = client.apply_bearer(builder, Some(session), spec.token_in_query);
    let raw: serde_json::Value = client.send_json(builder, StalkerAction::GetProfile).await?;
    // The portal sometimes returns the account fields under `js.*` and sometimes at the
    // top level. Merge both into a single object before deserialising.
    let merged = match raw {
        Value::Object(map) => {
            let mut js_map = match map.get("js") {
                Some(Value::Object(j)) => j.clone(),
                _ => serde_json::Map::new(),
            };
            for (k, v) in map {
                if k != "js" {
                    js_map.entry(k).or_insert(v);
                }
            }
            Value::Object(js_map)
        }
        other => other,
    };
    serde_json::from_value::<StalkerRawProviderProfile>(merged)
        .map_err(|err| StalkerError::BodyDecode { message: format!("get_profile decode: {err}") })
}

async fn fetch_capabilities<Tr: StalkerTransport, C: Clock>(
    client: &StalkerApiClient<Tr, C>,
    session: &StalkerSession,
    load_url: &StalkerLoadUrl,
    spec: &crate::stalker::recipes::StalkerRecipeSpec,
) -> StalkerResult<StalkerPortalCapabilitiesDto> {
    let mut builder = client
        .get(&load_url.load_url)
        .headers(client.common_headers(load_url))
        .query(&[("type", "stb"), ("action", "get_capabilities")]);
    builder = client.apply_mac_query(builder);
    builder = client.apply_bearer(builder, Some(session), spec.token_in_query);
    let raw = client.send_json::<serde_json::Value>(builder, StalkerAction::GetCapabilities).await.ok();
    let Some(value) = raw else {
        return Ok(StalkerPortalCapabilitiesDto::default());
    };
    let capabilities_value = match value {
        Value::Object(ref map) => map.get("js").cloned().unwrap_or(value.clone()),
        _ => return Ok(StalkerPortalCapabilitiesDto::default()),
    };
    serde_json::from_value::<StalkerPortalCapabilitiesDto>(capabilities_value)
        .map_err(|err| StalkerError::BodyDecode { message: format!("get_capabilities decode: {err}") })
}

// Re-export to keep callers from having to know about the inner submodule path.
