//! Namespaced incoming HTTP, resolved against the current registry snapshot.
//! Authentication always precedes the handler; failures never fall through.
use crate::{
    dispatch::HookDispatcher,
    host_fns,
    manifest::{HttpRoute, RouteAuth},
    registry::PluginState,
    rpc::RpcFailure,
    runtime::{call_with_deadline, CallError},
    system::PluginSystem,
};
use http::{HeaderName, HeaderValue, StatusCode};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
pub use tuliprox_plugin_api::bindings::exports::tuliprox::plugin::http_routes::{
    Request as HttpRouteRequest, Response as HttpRouteResponse,
};

pub(crate) const MAX_BODY: usize = 1024 * 1024;
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// Only the HTTP host constructs this from verified claims, never request JSON.
#[derive(Debug, Clone)]
pub enum HostAuthorization {
    Authenticated {
        username: String,
        may_write: bool,
    },
    /// Explicitly follows the server's disabled web authentication setting.
    Disabled,
    Unauthorized,
    Forbidden,
}

fn failure(status: StatusCode, error: &str) -> RpcFailure {
    RpcFailure { status, error: error.to_string(), fields: Vec::new(), retry_after: None }
}

fn route_trees(routes: &[HttpRoute]) -> Result<HashMap<&str, matchit::Router<usize>>, String> {
    if routes.len() > 256 {
        return Err("at most 256 HTTP routes may be declared".into());
    }
    let mut ids = HashSet::new();
    let mut trees = HashMap::new();
    for (index, route) in routes.iter().enumerate() {
        if !shared::model::is_kebab_id(&route.id, 1..=64) || !ids.insert(&route.id) {
            return Err(format!("invalid or duplicate HTTP route id `{}`", route.id));
        }
        if !matches!(route.method.as_str(), "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS") {
            return Err(format!("route `{}` has an unsupported HTTP method", route.id));
        }
        // No wildcards or encoded path syntax: paths are local templates, not URLs.
        if !route.path.starts_with('/')
            || route.path.len() > 2048
            || route.path.contains(['?', '#', '%', '*', '\\'])
            || route.path.bytes().any(|b| b <= b' ' || b >= 127)
            || route.path.split('/').any(|s| matches!(s, "." | ".."))
            || route.path.contains("//")
        {
            return Err(format!("route `{}` has an invalid path", route.id));
        }
        trees
            .entry(route.method.as_str())
            .or_insert_with(matchit::Router::new)
            .insert(&route.path, index)
            .map_err(|err| format!("invalid or conflicting HTTP route `{}`: {err}", route.id))?;
    }
    Ok(trees)
}

pub(crate) fn validate_routes(routes: &[HttpRoute]) -> Result<(), String> { route_trees(routes).map(|_| ()) }

fn select_route<'a>(routes: &'a [HttpRoute], request: &mut HttpRouteRequest) -> Result<&'a HttpRoute, RpcFailure> {
    let trees = route_trees(routes).map_err(|_| failure(StatusCode::BAD_GATEWAY, "invalid plugin routes"))?;
    let Some(tree) = trees.get(request.method.as_str()) else {
        return Err(if trees.values().any(|tree| tree.at(&request.path).is_ok()) {
            failure(StatusCode::METHOD_NOT_ALLOWED, "method not registered by this plugin")
        } else {
            failure(StatusCode::NOT_FOUND, "unknown plugin route")
        });
    };
    let matched = tree.at(&request.path).map_err(|_| {
        if trees.values().any(|tree| tree.at(&request.path).is_ok()) {
            failure(StatusCode::METHOD_NOT_ALLOWED, "method not registered for this path")
        } else {
            failure(StatusCode::NOT_FOUND, "unknown plugin route")
        }
    })?;
    request.route.clone_from(&routes[*matched.value].id);
    request.params = matched.params.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    Ok(&routes[*matched.value])
}

fn host_principal(auth: HostAuthorization, method: &str) -> Result<Option<String>, RpcFailure> {
    match auth {
        HostAuthorization::Disabled => Ok(None),
        HostAuthorization::Authenticated { username, may_write } => {
            if !matches!(method, "GET" | "HEAD" | "OPTIONS") && !may_write {
                Err(failure(StatusCode::FORBIDDEN, "this route needs system write permission"))
            } else {
                Ok(Some(username))
            }
        }
        HostAuthorization::Unauthorized => Err(failure(StatusCode::UNAUTHORIZED, "authentication required")),
        HostAuthorization::Forbidden => Err(failure(StatusCode::FORBIDDEN, "system read permission required")),
    }
}

fn check_request(req: &HttpRouteRequest) -> Result<(), RpcFailure> {
    if req.body.len() > MAX_BODY {
        return Err(failure(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds 1 MiB"));
    }
    if req.path.len() + req.query.len() > 8192 {
        return Err(failure(StatusCode::URI_TOO_LONG, "request URI too long"));
    }
    if req.headers.len() > MAX_HEADERS
        || req.headers.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() > MAX_HEADER_BYTES
    {
        return Err(failure(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE, "request headers too large"));
    }
    Ok(())
}

/// Transport and browser policy belong to the host, not guest-provided headers.
pub(crate) fn forbidden_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "keep-alive"
            | "transfer-encoding"
            | "content-length"
            | "te"
            | "trailer"
            | "upgrade"
            | "content-security-policy"
            | "content-security-policy-report-only"
            | "x-frame-options"
            | "x-content-type-options"
            | "set-cookie"
            | "clear-site-data"
            | "service-worker-allowed"
    ) || name.starts_with("proxy-")
        || name.starts_with("access-control-")
}

fn vet_response(mut response: HttpRouteResponse) -> Result<HttpRouteResponse, RpcFailure> {
    if !(200..=599).contains(&response.status)
        || response.body.len() > MAX_RESPONSE
        || response.headers.len() > MAX_HEADERS
        || response.headers.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() > MAX_HEADER_BYTES
    {
        return Err(failure(StatusCode::BAD_GATEWAY, "invalid or oversized plugin response"));
    }
    for (key, value) in &response.headers {
        HeaderName::from_bytes(key.as_bytes())
            .map_err(|_| failure(StatusCode::BAD_GATEWAY, "invalid response header"))?;
        HeaderValue::from_str(value).map_err(|_| failure(StatusCode::BAD_GATEWAY, "invalid response header"))?;
    }
    // A Connection header may name additional hop-by-hop headers.
    let connection: Vec<String> = response
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("connection"))
        .flat_map(|(_, v)| v.split(',').map(|s| s.trim().to_ascii_lowercase()))
        .collect();
    response.headers.retain(|(key, _)| {
        let key = key.to_ascii_lowercase();
        !forbidden_header(&key) && !connection.contains(&key)
    });
    if matches!(response.status, 204 | 205 | 304) {
        response.body.clear();
    }
    Ok(response)
}

impl PluginSystem {
    /// Dispatch a bounded request under `/api/v1/plugins/{id}/http/…`.
    /// Host and plugin identities are separate; plugin auth never grants host permissions.
    pub async fn api_http_call(
        &self,
        id: &str,
        mut request: HttpRouteRequest,
        auth: HostAuthorization,
    ) -> Result<HttpRouteResponse, RpcFailure> {
        let snapshot = self.registry().snapshot();
        let record = snapshot.get(id).ok_or_else(|| failure(StatusCode::NOT_FOUND, "unknown plugin"))?;
        if record.state() != PluginState::Enabled {
            return Err(failure(StatusCode::CONFLICT, "plugin is not enabled"));
        }
        let manifest =
            record.manifest.as_deref().ok_or_else(|| failure(StatusCode::CONFLICT, "manifest unavailable"))?;
        if !manifest.capabilities.http_routes {
            return Err(failure(StatusCode::NOT_FOUND, "plugin has no HTTP routes"));
        }
        check_request(&request)?;
        let route = select_route(&manifest.routes, &mut request)?;
        request.principal = None;
        if route.auth == RouteAuth::Host {
            request.principal = host_principal(auth, &request.method)?;
        }
        let gate = self.runtime().rpc();
        gate.take(id)?;
        let loaded = record.loaded.as_ref().ok_or_else(|| failure(StatusCode::CONFLICT, "plugin is not loaded"))?;
        let indices =
            loaded.http_routes.as_ref().ok_or_else(|| failure(StatusCode::BAD_GATEWAY, "HTTP export missing"))?;
        let result = async {
            let dispatcher = HookDispatcher::new(self.runtime(), Arc::clone(&snapshot));
            let mut live = dispatcher.open(loaded).await?;
            let instance = live.instance();
            let guest = indices.load(live.store_mut(), &instance).map_err(|err| CallError::Trap(format!("{err:#}")))?;
            let limits = live.limits();
            // Authentication and handling share one wall-clock and CPU deadline.
            live.armed();
            let outcome = call_with_deadline(limits, async {
                if route.auth == RouteAuth::Plugin {
                    let principal = guest.call_authenticate(live.store_mut(), &request).await?;
                    match principal {
                        Ok(Some(name)) if !name.is_empty() && name.len() <= 1024 => request.principal = Some(name),
                        Ok(None) => return Ok(Err(failure(StatusCode::UNAUTHORIZED, "authentication required"))),
                        _ => return Ok(Err(failure(StatusCode::BAD_GATEWAY, "plugin authentication failed"))),
                    }
                }
                if route.auth == RouteAuth::Host {
                    request.headers.retain(|(name, _)| {
                        !matches!(
                            name.to_ascii_lowercase().as_str(),
                            "authorization" | "cookie" | "x-api-key" | "proxy-authorization"
                        )
                    });
                }
                let result = guest.call_handle(live.store_mut(), &request).await?;
                Ok(result.map_err(|_| failure(StatusCode::BAD_GATEWAY, "plugin handler failed")))
            })
            .await;
            host_fns::end_call(live.store_mut().data_mut()).await;
            outcome
        }
        .await;
        let result = match result {
            Ok(Ok(response)) => vet_response(response),
            Ok(Err(err)) => Err(err),
            Err(CallError::Budget { .. }) => {
                gate.record_budget_skip(id);
                return Err(failure(StatusCode::SERVICE_UNAVAILABLE, "plugin resources unavailable"));
            }
            // Neither guest errors nor credentials/backtraces are returned or logged.
            Err(_) => Err(failure(StatusCode::BAD_GATEWAY, "plugin request failed or timed out")),
        };
        gate.record(id, result.as_ref().err().map(|err| err.error.as_str()));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;
    fn route(id: &str, method: &str, path: &str) -> HttpRoute {
        HttpRoute { id: id.into(), method: method.into(), path: path.into(), auth: RouteAuth::Host }
    }
    fn request(method: &str, path: &str) -> HttpRouteRequest {
        HttpRouteRequest {
            route: "spoof".into(),
            method: method.into(),
            path: path.into(),
            query: String::new(),
            headers: vec![],
            params: vec![],
            body: vec![],
            principal: Some("spoof".into()),
        }
    }
    #[test]
    fn manifest_requires_capability_hook_and_routes_and_defaults_to_host_auth() {
        let base = "id: demo\nname: Demo\nversion: 1.0.0\napi_version: 1\n";
        let routes = "routes:\n  - {id: list, method: GET, path: /api/channels/}\n";
        assert!(Manifest::parse(&format!("{base}{routes}"), "demo").is_err());
        assert!(Manifest::parse(&format!("{base}hooks: [http.handle]\n{routes}"), "demo").is_err());
        let good = format!("{base}capabilities: {{http_routes: true}}\nhooks: [http.handle]\n{routes}");
        let manifest = Manifest::parse(&good, "demo").unwrap();
        assert_eq!(manifest.routes[0].auth, RouteAuth::Host);
        assert!(Manifest::parse(&good.replace("GET", "TRACE"), "demo").is_err());
        assert!(Manifest::parse(&good.replace("path:", "authentication: none, path:"), "demo").is_err());
        assert!(Manifest::parse(&good.replace("method: GET", "auth: public, method: GET"), "demo").is_err());
    }
    #[test]
    fn conflicting_paths_invalid_ids_and_unsafe_templates_are_rejected() {
        assert!(validate_routes(&[route("one", "GET", "/a/{id}"), route("two", "GET", "/a/{other}")]).is_err());
        assert!(validate_routes(&[route("one", "GET", "/a"), route("one", "POST", "/b")]).is_err());
        for path in ["https://other/a", "/a/../b", "/a?x=1", "/a#x", "/a/%2f", "/a/{*rest}", "/a\\b", "/a//b"] {
            assert!(validate_routes(&[route("one", "GET", path)]).is_err(), "{path}");
        }
    }
    #[test]
    fn static_routes_take_precedence_and_parameters_stay_encoded() {
        let routes = [route("detail", "GET", "/api/{id}/"), route("search", "GET", "/api/search/")];
        let mut req = request("GET", "/api/search/");
        assert_eq!(select_route(&routes, &mut req).unwrap().id, "search");
        let mut req = request("GET", "/api/a%2Fb/");
        select_route(&routes, &mut req).unwrap();
        assert_eq!(req.route, "detail");
        assert_eq!(req.params, [("id".into(), "a%2Fb".into())]);
        assert_eq!(
            select_route(&routes, &mut request("PATCH", "/api/a/")).unwrap_err().status,
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(select_route(&routes, &mut request("PATCH", "/missing")).unwrap_err().status, StatusCode::NOT_FOUND);
    }
    #[test]
    fn host_auth_fails_closed_and_writes_require_permission() {
        assert_eq!(
            host_principal(HostAuthorization::Unauthorized, "GET").unwrap_err().status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(host_principal(HostAuthorization::Forbidden, "GET").unwrap_err().status, StatusCode::FORBIDDEN);
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            assert_eq!(
                host_principal(
                    HostAuthorization::Authenticated { username: "reader".into(), may_write: false },
                    method
                )
                .unwrap_err()
                .status,
                StatusCode::FORBIDDEN
            );
        }
        assert_eq!(
            host_principal(HostAuthorization::Authenticated { username: "reader".into(), may_write: false }, "GET")
                .unwrap(),
            Some("reader".into())
        );
        assert_eq!(host_principal(HostAuthorization::Disabled, "PATCH").unwrap(), None);
    }
    #[test]
    fn request_and_response_limits_and_transport_headers_are_enforced() {
        let mut req = request("GET", "/");
        req.body = vec![0; MAX_BODY + 1];
        assert_eq!(check_request(&req).unwrap_err().status, StatusCode::PAYLOAD_TOO_LARGE);
        req.body.clear();
        req.headers = vec![("x".into(), "y".into()); 65];
        assert_eq!(check_request(&req).unwrap_err().status, StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);
        let mut response = HttpRouteResponse {
            status: 204,
            body: vec![0, 255],
            headers: vec![
                ("Connection".into(), "x-hop".into()),
                ("X-Hop".into(), "bad".into()),
                ("content-length".into(), "9999".into()),
                ("set-cookie".into(), "x=y".into()),
                ("access-control-allow-origin".into(), "*".into()),
                ("content-type".into(), "application/json".into()),
            ],
        };
        let vetted = vet_response(response.clone()).unwrap();
        assert_eq!(vetted.body, Vec::<u8>::new());
        assert_eq!(vetted.headers, [("content-type".into(), "application/json".into())]);
        response.status = 101;
        assert!(vet_response(response.clone()).is_err());
        response.status = 200;
        response.headers = vec![("x-bad".into(), "a\r\nb".into())];
        assert!(vet_response(response).is_err());
    }
}
