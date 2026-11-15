# Connecting a Media Server as a Plugin

tuliprox has built-in inputs for Plex, Emby, Jellyfin and Silo. This page explains how to connect any other media
server, such as a home-grown library, a NAS index or a commercial service, as a WebAssembly plugin. The plugin
delivers the catalog. The host does everything else, exactly as for the built-in servers: storage, series grouping,
Xtream/M3U/HDHomeRun output, the playlist explorer, stream proxying with `Range` support and image proxying.

The runnable reference is the test fixture `plugins/plugin-api/fixtures/media-probe`. It exercises every call on this
page and is built with `make plugin-fixtures`.

## How it works

1. The operator adds an input with `type: plugin` and `plugin: <your id>` to `source.yml`.
2. On every playlist update the host calls `fetch-catalog` page by page and stores the catalog like a Jellyfin
   catalog. Movies become VOD items, series become series with seasons and episodes.
3. A client plays an item: the host calls `resolve-stream` and proxies the URL you return. The client never sees
   your upstream URL or headers.
4. A client loads artwork: the host calls `resolve-image` and proxies that URL the same way.

Your plugin never handles video bytes. It only answers "what is in the library" and "where is this item".

## Manifest

```yaml
id: my-media-server
name: My media server
version: 0.1.0
api_version: 1
hooks:
  - media.fetch
  - rpc.call
provides:
  input_type: true
capabilities:
  network: true
resources:
  max_exec_ms: 5000     # one catalog page
  resolve_ms: 3000      # one resolve-stream / resolve-image call (default 5 s, max 30 s)
ui:
  input_form:
    - id: main
      schema: input-schema.json
  rpc_exports:
    - name: libraries
      kind: read
      input_scoped: true
      input_schema: rpc/libraries.in.json
```

- `media.fetch` together with `provides.input_type: true` makes the plugin selectable as an input type.
- The plugin exports `media-provider-v1-1` and therefore cannot also export the classic `input-provider` (M3U channel
  list). The host refuses a plugin with both.
- `network` is required: without it every resolved target is refused.

## Input configuration

```yaml
inputs:
  - name: living-room
    type: plugin
    plugin: my-media-server
    url: https://media.example.org
    username: alice
    password: ${env:MEDIA_PASSWORD}
    plugin_config:
      api_token: ${env:MEDIA_TOKEN}
      library: movies
```

`url`, `username` and `password` reach the plugin as configured. `plugin_config` holds the fields of your input form
and reaches the plugin as `options`. `${env:VAR}` references are resolved before the call.

## Implementing the provider with the SDK

```rust
use tuliprox_plugin_sdk::prelude::*;

#[tuliprox_plugin]
mod plugin {
    use super::*;

    struct MyServer;

    impl MediaProvider for MyServer {
        fn fetch_catalog(req: &MediaRequest) -> Result<MediaCatalogPage, String> {
            // One page per call. `req.cursor` is `None` on the first page.
            let page = api::list_items(&req.url, token(req), req.cursor.as_deref())?;
            let mut catalog = MediaCatalog::default();
            catalog.libraries.push(MediaLibrary {
                id: "movies".into(),
                name: "Movies".into(),
                kind: LibraryKind::Movies,
            });
            for item in page.items {
                catalog.movies.push(MediaMovie {
                    library_id: "movies".into(),
                    item_id: item.id.clone(),
                    title: item.title,
                    year: item.year,
                    stream_key: Some(item.file_id),
                    image_key: Some(item.id),
                    ..MediaMovie::default()
                });
            }
            Ok(MediaCatalogPage { catalog, next_cursor: page.next })
        }

        fn resolve_stream(req: &StreamRequest) -> Result<StreamTarget, String> {
            let file_id = req.stream_key.as_deref().unwrap_or(&req.item_id);
            Ok(StreamTarget {
                url: format!("{}/files/{file_id}/raw", req.url),
                headers: vec![("Authorization".into(), format!("Bearer {}", token_of(&req.options)))],
                ttl_secs: Some(300),
            })
        }

        fn resolve_image(req: &ImageRequest) -> Result<StreamTarget, String> {
            Ok(StreamTarget {
                url: format!("{}/items/{}/poster", req.url, req.image_key),
                headers: vec![("Authorization".into(), format!("Bearer {}", token_of(&req.options)))],
                ttl_secs: Some(3600),
            })
        }
    }

    impl Rpc for MyServer {
        fn call(method: &str, payload: &str) -> Result<String, String> {
            rpc::Router::new()
                .route_scoped("libraries", |req: rpc::ScopedRequest| {
                    let libraries = api::list_libraries(req.input.url.as_deref().unwrap_or_default())?;
                    Ok::<_, String>(
                        libraries.into_iter().map(|lib| rpc::RpcOption::new(lib.id, lib.name)).collect::<Vec<_>>(),
                    )
                })
                .dispatch(method, payload)
        }
    }
}
```

`api`, `token` and `token_of` stand for your own HTTP client code (use `tuliprox_plugin_sdk::http`).

### The catalog model

- `libraries`, `movies`, `series`, `seasons` and `episodes` are flat lists. Seasons point to their series with
  `series_id`, episodes to their season with `season_id` (or to the series with `series_id` and `season`).
- All ids are yours and only need to be unique within your plugin. The host adds the input name and the plugin id.
- `stream_key` and `image_key` are opaque to the host. They come back unchanged in `resolve-stream` and
  `resolve-image`, so put whatever your server needs there (a file id, a media source id).
- `provider_ids` (`tmdb`, `imdb`, `tvdb`) enable metadata enrichment and matching like for the built-in servers.
- `descriptive` (plot, genres, cast) and `technical` (container, duration, codecs) are optional.

### Paging and refresh

- Return `next_cursor: None` on the last page. The host publishes the catalog only after the last page succeeded; a
  failed page keeps the previous catalog.
- `req.refresh` is `true` when the operator forced a refresh. Skip your own caches then.
- `req.last_success_at` is the Unix time of the previous successful update, or `None` on the first run. Use it for
  incremental fetching when your server supports it, but always return the complete catalog.
- The host reports progress per page in the Web UI.

## Secrets in the input form

Mark secret fields with `writeOnly: true` in the input form schema:

```json
{
  "type": "object",
  "properties": {
    "api_token": { "type": "string", "title": "API token", "writeOnly": true },
    "library": { "type": "string", "title": "Library", "x-options-rpc": "libraries" }
  }
}
```

- The Web UI and the config API show a secret as `********`. Saving the masked value keeps the stored one.
- An `${env:VAR}` reference stays visible, because it names a variable and not a secret. Saving a literal secret
  logs a warning that recommends `${env:VAR}`.
- For input-scoped RPC calls (`x-options-rpc` and MCP), the host scrubs credentials (`username`, `password`),
  secret-looking options, and `writeOnly` form secrets from the JSON answer and from guest log lines.
- For catalog updates (`fetch-catalog`) and media resolution (`resolve-stream`, `resolve-image`), the host redacts
  the same call secrets from guest log lines. Upstream stream and image targets (URLs and headers) stay hidden from
  clients.
- A `writeOnly` field must not have a `default`.

## Dynamic options (`x-options-rpc`)

`x-options-rpc` turns a string field into a select whose options come from your plugin, for example the libraries
the server offers. The named export must be `kind: read` and `input_scoped: true`. Implement `Rpc` on your plugin
struct inside the `#[tuliprox_plugin] mod plugin { ... }` module, as the `impl Rpc for MyServer` block in the SDK
example above shows. Its `libraries` route answers the `x-options-rpc: libraries` field.

- The answer is a list of `{"value", "label"}` objects (or `{"options": [...]}`), at most 1000 entries and 1024
  characters per value or label. Anything else shows an error in the form and a text field as fallback.
- While the operator edits an unsaved input, the form sends its current values as `input_draft`. Calling with a
  draft needs the permission to edit sources. Stored secrets are only filled in while the draft's URL is the stored
  URL, so a draft cannot send a saved password to another server.
- The same export can be offered as an MCP tool with `mcp: true`. MCP calls must name a saved input.

## Limits the host enforces

| What | Limit |
| --- | --- |
| Catalog pages | 10,000 per update, repeated cursors are detected |
| Catalog items | 1,000,000 per update |
| Titles | 512 bytes; summaries 16 KiB; cast and crew 100 entries |
| Duplicate ids, dangling references | dropped and logged |
| `resolve-stream`, `resolve-image` | deadline `resources.resolve_ms` (default 5 s, max 30 s) |
| Target URL | `http` or `https` with a host |
| Target headers | 32 headers, 256 bytes per name, 4 KiB per value, 16 KiB in total; no hop-by-hop headers |
| Target cache | per item and key, `ttl_secs` capped at 1 hour, 4,096 entries; errors are never cached |

A plugin that hangs, traps or breaks a limit fails only that update or that single request. The host, other inputs
and other streams keep running.

## Images with headers

Media catalog items reference artwork with `image_key` and always resolve artwork on demand through `resolve-image`.
The target returned by `resolve-image` must be an absolute `http` or `https` URL with a host; the host proxies it
and caches targets for up to 1 hour based on `ttl_secs`.

The host API `images::cache_with_headers(url, &headers)` (from `host-v1-1`) fetches an image once through the host
cache and returns a host-local relative URL (such as `/api/v1/playlist/resource/...`). This local URL is designed for
channel logos and icons in classic playlists (`input-provider`), not for media catalog items or `resolve-image`
targets (which require absolute URLs).

## Testing

- Unit-test your mapping natively: the SDK `mock` host backs `http`, `kv` and `images` outside wasm.
- Run your plugin against the host the way `backend/plugins/tests/media_provider.rs` runs `media-probe`: normal
  catalogs, paging, a hanging guest, a trapping guest, oversized data and broken references.
- Point a real input at the plugin and check the playlist explorer and a playback with a seek (a `Range` request).
