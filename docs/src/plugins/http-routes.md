# Incoming HTTP routes

A plugin can expose a small HTTP API under its own namespace. This is useful for external
integrations that expect REST endpoints, status codes and their own authentication scheme.
It does not register paths in the server root or replace built-in APIs.

The server must have the `plugins` feature and the Web UI REST API enabled. The URL prefix is
`<web_ui_path>/api/v1/plugins/<id>/http`. For example, a client that appends `/api/channels/`
to its configured server URL would use `http://localhost:8901/api/v1/plugins/my-bridge/http`
as its base URL. Plugin loading, approval and enablement are required. Hot reloads take effect
on the next request; in-flight calls retain their registry snapshot.

## Manifest

```yaml
capabilities:
  http_routes: true
hooks: [http.handle]
routes:
  - id: status
    method: GET
    path: /status
    # auth: host is the default
  - id: external-channel-update
    method: PATCH
    path: /api/channels/{id}/
    auth: plugin
```

Route IDs must be unique kebab-case names, 1–64 characters. Up to 256 routes are allowed.
Methods are explicit: `GET`, `HEAD`, `POST`, `PUT`, `PATCH`, `DELETE`, `OPTIONS`. HEAD does not
implicitly call a GET route. Templates use matchit-style `{parameter}` segments; static paths
win over parameters. Conflicting templates are rejected at install/load. Wildcards, full URLs,
query strings, percent escapes and dot segments are not allowed in manifest paths. Trailing
slashes are significant. Use `/` for the namespace root.

## Authentication

`auth: host` uses the existing Tuliprox bearer-token validation, including expiry, revocation
and current permissions. GET/HEAD/OPTIONS require `SystemRead`; POST/PUT/PATCH/DELETE also
require `SystemWrite`. When web authentication is explicitly disabled, host-auth routes follow
that setting. A handler must not use a read method for an operation that changes data.
The host supplies the verified username in `request.principal`. Host authentication credentials
(`Authorization`, `Cookie`, `X-API-Key`, `Proxy-Authorization`) are removed before the handler runs.

`auth: plugin` explicitly delegates authentication to `HttpRoutes::authenticate`. It receives
the request with `principal: None`, including credential headers, and must validate its own API
key, JWT, or other scheme. It returns `Ok(Some(identity))` to accept, `Ok(None)` to reject with
401, or `Err` to fail with 502. Empty identities or identities over 1024 bytes are rejected.
The host calls `handle` only after acceptance, with that identity in `principal`. Authentication
and handling share an instance and one CPU/wall-clock deadline. Authentication errors and traps
never reach the handler. Plugin authentication remains required when web authentication is off;
a Tuliprox admin token does not bypass it. A plugin identity grants no Tuliprox permissions.

There is no implicit public mode. For a login/token endpoint the plugin can explicitly accept
an anonymous identity for that route, then check credentials in its handler before issuing a
token. Store credential secrets in secret settings and never log credentials or request bodies.
Plugin-controlled responses can carry issued tokens; unlike management RPC, response bodies are
not redacted. Error details from the guest are not sent to clients or logged by the route dispatcher.
The host's generic 401 challenge is `Bearer`; a handler may return its own 401 and challenge.

## SDK

Implement `HttpRoutes` inside `#[tuliprox_plugin]`. The additive WIT interface is `http-routes`
and its standalone world is `http-plugin`; existing ABI interfaces and worlds are unchanged.

```rust,ignore
use tuliprox_plugin_sdk::prelude::*;

#[tuliprox_plugin]
mod plugin {
    use super::*;
    pub struct Api;
    impl HttpRoutes for Api {
        fn authenticate(_req: &HttpRouteRequest) -> Result<Option<String>, String> {
            // Not called for auth: host. Fail closed if the manifest changes.
            Ok(None)
        }
        fn handle(req: HttpRouteRequest) -> Result<HttpRouteResponse, String> {
            Ok(HttpRouteResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/plain".into())],
                body: format!("Hello {}", req.principal.as_deref().unwrap_or("operator")).into_bytes(),
            })
        }
    }
}
```

Requests include the host-selected route ID, method, plugin-relative encoded path, raw query
without `?`, encoded path parameters, repeated text headers, binary body and authenticated
principal. Client-supplied principal/route fields cannot override the host's values. Decode query
values and path parameters explicitly when the protocol requires it. Non-text header values are
rejected with 400. The SDK/wasm fixture in `plugins/plugin-api/fixtures/http-probe` demonstrates
custom API-key authentication and binary request handling for tests.

Responses contain a status (200–599), repeated headers and binary body. Invalid responses fail
with 502. HEAD bodies and bodies for 204, 205 and 304 are discarded. The host removes transport
headers, headers named by `Connection`, `Set-Cookie`, CORS and browser-security overrides.
Responses are served with `Cache-Control: no-store`, `nosniff`, frame denial and a sandbox CSP;
HTML returned by a plugin cannot execute scripts on the host origin. Use bearer/API-key
credentials rather than cookies. This is a bounded request/response API, not a streaming,
WebSocket or arbitrary socket server.

## Limits and failures

- Request body: 1 MiB. Response body: 16 MiB.
- URI path plus query: 8192 bytes.
- Headers each way: 64 entries and 16 KiB of name/value bytes.
- Rate: shared with plugin RPC, 10 calls/second per plugin, burst 20; 429 carries `Retry-After`.
- Existing plugin memory/instance limits and hook execution deadlines apply.
- Missing plugin/route: 404. Disabled/unloaded plugin: 409. Unsupported method for a known path: 405.
- Failed authentication: 401. Insufficient host permissions: 403. Oversized request: 413.
- Bad header/URI sizes: 431/414. Guest error/trap/timeout/invalid response: 502. No free plugin resources: 503.

HTTP calls use the RPC counters in the management stats and do not trip the pipeline breaker.
The dispatcher does not automatically schedule a playlist refresh or implement any compatibility
protocol: those belong to the integration plugin and its synchronization design.
