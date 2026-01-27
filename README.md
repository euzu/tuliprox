# <img src="https://github.com/user-attachments/assets/8ef9ea79-62ff-4298-978f-22326c5c3d02" height="24" /> **tuliprox** - A Powerful IPTV Proxy & Playlist Processor

**tuliprox** is a lightweight, high-performance IPTV proxy and playlist processor written in Rust 🦀
It supports M3U and M3U8 formats, Xtream Codes API, HDHomeRun and STRM, making it easy to filter, merge, and serve IPTV streams for Plex, Jellyfin, Emby, and other media servers.

<p align="center">
<img src="https://github.com/user-attachments/assets/8ef9ea79-62ff-4298-978f-22326c5c3d02" height="128" />
</p>

## 🔧 Core Features:

- **Advanced Playlist Processing**: Filter, rename, map, and sort entries with ease.
- **Flexible Proxy Support**: Acts as a reverse/redirect proxy for EXTM3U, Xtream Codes, HDHomeRun, and STRM formats (Kodi, Plex, Emby, Jellyfin) with:
  - app-specific naming conventions
  - flat directory structure option (for compatibility reasons of some media scanners)
- **Local media library Management**: Scan and serve local video files with automatic metadata resolution:
  - Recursive directory scanning for movies and TV series
  - Multi-source metadata (NFO files, TMDB API, filename parsing)
  - Automatic classification (Movies vs Series)
  - Integration with Xtream API and M3U playlists
- **Multi-Source Handling**: Supports multiple input and output sources. Merge various playlists and generate custom outputs.
- **Scheduled Updates**: Keep playlists fresh with automatic updates in server mode.
- **Web Delivery**: Run as a CLI tool to create m3u playlist to serve with web servers like Nginx or Apache.
- **Template Reuse (DRY)**: Create and reuse templates using regular expressions and declarative logic.

## 🔍 Smart Filtering:
Define complex filters using expressive logic, e.g.:
`(Group ~ "^FR.*") AND NOT (Group ~ ".*XXX.*" OR Group ~ ".*SERIES.*" OR Group ~ ".*MOVIES.*")`

## 📢 Monitoring & Alerts:
- Send notifications via **Telegram**, **Pushover**, or custom **REST** endpoints when problems occur.
- Track group changes and get real-time alerts.

## 📺 Stream Management:
- Share live TV connections.
- Show a fallback video stream if a channel becomes unavailable.
- Integrate **HDHomeRun** devices with **Plex**, **Emby**, or **Jellyfin**.
- Use provider aliases to manage multiple lines from the same source.

## 🐋 Docker Container Templates: 
- traefik template
- crowdsec template
- gluetun/socks5 template
- tuliprox (incl. traefik) template

`> ./docker/container-templates`

## Want to join the community: [on <img src="https://cdn.simpleicons.org/discord/5865F2" height="24" /> Discord ](https://discord.gg/gkzCmWw9Tf)

## Command line Arguments
```
Usage: tuliprox [OPTIONS]

Options:
  -p, --config-path <CONFIG_PATH>  The config directory
  -c, --config <CONFIG_FILE>       The config file
  -i, --source <SOURCE_FILE>       The source config file
  -m, --mapping <MAPPING_FILE>     The mapping file
  -t, --target <TARGET>            The target to process
  -a, --api-proxy <API_PROXY>      The user file
  -s, --server                     Run in server mode
  -l, --log-level <LOG_LEVEL>      log level
  -h, --help                       Print help
  -V, --version                    Print version
  --genpwd                         Generate UI Password
  --healthcheck                    Healtcheck for docker
  --scan-library                   Scan library directories
  --force-library-rescan           Force full library rescan
  --dbx                            Database file type: xtream
  --dbm                            Database file type: m3u
  --dbe                            Database file type: epg
```

## 1. `config.yml`

For running in cli mode, you need to define a `config.yml` file which can be inside config directory next to the executable or provided with the
`-c` cli argument.

For running specific targets use the `-t` argument like `tuliprox -t <target_name> -t <other_target_name>`.
Target names should be provided in the config. The -t option overrides `enabled` attributes of `input` and `target` elements.
This means, even disabled inputs and targets are processed when the given target name as cli argument matches a target.

Top level entries in the config files are:
* `api`
* `working_dir`
* `default_user_agent` _optional_, used as fallback for upstream requests when no `User-Agent` is provided by input headers or the client request (client request overrides it).
* `process_parallel` _optional_
* `messaging`  _optional_
* `video` _optional_
* `schedules` _optional_
* `backup_dir` _optional_
* `update_on_boot` _optional_
* `web_ui` _optional_
* `reverse_proxy` _optional_
* `log` _optional
* `user_access_control` _optional_
* `connect_timeout_secs`: _optional_ and used for provider requests connection timeout, for only the connect phase.
* `custom_stream_response_path` _optional_
* `hdhomerun` _optional_
* `proxy` _optional_
* `ipcheck` _optional_
* `config_hot_reload` _optional_, default false.
* `sleep_timer_mins` _optional_, used for closing stream after the given minutes.
* `accept_unsecure_ssl_certificates` _optional_, default false.
* `disk_based_processing` _optional_, default false. When set to true, input playlists are processed from disk to save RAM.
* `library` _optional_, for local media 

### 1.1. `process_parallel`
If you are running on a cpu which has multiple cores, you can set for example `process_parallel: true` to run multiple threads.
If you process the same provider multiple times each thread uses a connection. Keep in mind that you hit the provider max-connection.

### 1.2. `api`
`api` contains the `server-mode` settings. To run `tuliprox` in `server-mode` you need to start it with the `-s`cli argument.
-`api: {host: localhost, port: 8901, web_root: ./web}`

### 1.3. `working_dir`
`working_dir` is the directory where files are written which are given with relative paths.
-`working_dir: ./data`

With this configuration, you should create a `data` directory where you execute the binary.

Be aware that different configurations (e.g. user bouquets) along the playlists are stored in this directory.

### 1.4 `messaging`
`messaging` is an optional configuration for receiving messages.
Currently `telegram`, `discord`, `rest` and `pushover.net` is supported.

Messaging is Opt-In, you need to set the `notify_on` message types which are
- `info`
- `stats`
- `error`

`telegram`, `rest` and `pushover.net` configurations are optional.

`telegram` supports markdown generation for structured json messages.
`telegram` supports `message_thread_id` for group chats. Simply put thread_id behind chat_id separated by `:`. `'<telegram chat id>:<message thread id>'`

```yaml
messaging:
  notify_on:
    - info
    - stats
    - error
  telegram:
    markdown: true
    bot_token: '<telegram bot token>'
    chat_ids:
      - '<telegram chat id>'
      - '<telegram chat id>:<message thread id>'
    templates: # templates per message kind
      stats: 'file:///path/to/stats_telegram.templ'
  discord:
    url: '<discord webhook url>'
    templates:
      info: '{"content": "{{message}}"}'
  rest:
    url: '<api url>'
    method: 'POST' # optional, default POST
    headers:
      - 'Content-Type: application/json'
    templates:
      error: '{"text": "Error: {{message}}"}'

  pushover:
    token: <api_token>
    user: <api_username>
    url: `optional`, default is `https://api.pushover.net/1/messages.json`
```

### 1.4.1 Messaging Templating
For `discord`, `telegram` and `rest` messaging, you can use [Handlebars](https://handlebarsjs.com/) templates to format the message body.

**Loading Templates:**
Templates can be provided in two ways:
1.  **Raw String**: The template content is written directly in the configuration.
2.  **URI**: A link to a file (`file://...`) or an external resource (`http(s)://...`).
> **Note**: When saving through the Web UI, raw template strings are automatically moved to individual files in `/config/messaging_templates/` and referenced via `file://` to keep the configuration file clean.

**Context Variables:**
- `message`: The text content for `info` and `error` notifications.
- `kind`: The type of notification (`info`, `stats`, `error`, `watch`).
- `timestamp`: Current UTC timestamp in RFC3339 format.
- `stats`: A list of processed source statistics (available for `stats` kind).
  - Each item contains `inputs` (list of `InputStats`) and `targets` (list of `TargetStats`).
- `watch`: Change details for groups (available for `watch` kind).
- `processing`: Detailed internal processing state.
  - `errors`: Combined error messages from a processing run.

**Example Multi-Source Telegram Template**:
```handlebars
*🔄 Playlist Update Report*

{{#each stats}}
*📥 Source Stats*
{{#each inputs}}
• *{{name}}* (`{{type}}`)
  ⏱️ Took: `{{took}}` | ❌ Errors: `{{errors}}`
  📊 `{{raw.groups}}`/`{{raw.channels}}` ➔ *`{{processed.groups}}`*/*`{{processed.channels}}`*
{{/each}}
{{/each}}
```

**Example Discord Template (Complex Embed)**:
```handlebars
{
  "content": "Tuliprox Notification",
  "embeds": [{
    "title": "Event: {{kind}}",
    "description": "{{message}}",
    "color": 3447003,
    "fields": [
      {{#each stats}}
      { "name": "Source {{@index}}", "value": "Processed {{#each inputs}}{{name}} {{/each}}", "inline": false }
      {{/each}}
    ],
    "footer": { "text": "Reported at {{timestamp}}" }
  }]
}
```

For more information: [Telegram bots](https://core.telegram.org/bots/tutorial)

### 1.5 `video`
`video` is optional.

It has 2 entries `extensions` and `download`.

- `extensions` are a list of video file extensions like `mp4`, `avi`, `mkv`.  
  When you have input `m3u` and output `xtream` the url's with the matching endings will be categorized as `video`.

- `download` is _optional_ and is only necessary if you want to download the video files from the ui
  to a specific directory. if defined, the download button from the `ui` is available.
  - `headers` _optional_, download headers
  - `organize_into_directories` _optional_, orgainize downloads into directories
  - `episode_pattern` _optional_ if you download episodes, the suffix like `S01.E01` should be removed to place all
    files into one folder. The named capture group `episode` is mandatory.  
    Example: `.*(?P<episode>[Ss]\\d{1,2}(.*?)[Ee]\\d{1,2}).*`
- `web_search` is _optional_, example: `https://www.imdb.com/search/title/?title={}`,
  define `download.episode_pattern` to remove episode suffix from titles.

```yaml
video:
  web_search: 'https://www.imdb.com/search/title/?title={}'
  extensions:
    - mkv
    - mp4
    - avi
  download:
    headers:
      User-Agent: "AppleTV/tvOS/9.1.1."
      Accept: "video/*"
    directory: /tmp/
    organize_into_directories: true
    episode_pattern: '.*(?P<episode>[Ss]\\d{1,2}(.*?)[Ee]\\d{1,2}).*'
```

### 1.5 `schedules`
For `version < 2.0.11`:
Schedule is optional.
Format is
```yaml
#   sec  min   hour   day of month   month   day of week   year
schedule: "0  0  8,20  *  *  *  *"
```

For `version >= 2.0.11`
Format is
```yaml
#   sec  min   hour   day of month   month   day of week   year
schedules:
- schedule: "0  0  8  *  *  *  *"
  targets:
  - m3u
- schedule: "0  0  10  *  *  *  *"
  targets:
  - xtream
- schedule: "0  0  20  *  *  *  *"
```

At the given times the update is started. Do not start it every second or minute.
You could be banned from your server. Twice a day should be enough.

### 1.6 `reverse_proxy`

This configuration is only used for reverse proxy mode. The Reverse Proxy mode can be activated for each user individually.

#### 1.6.1 `stream`
Attributes:
- `retry`
- `buffer`
- `throttle` Allowed units are `KB/s`,`MB/s`,`KiB/s`,`MiB/s`,`kbps`,`mbps`,`Mibps`. Default unit is `kbps`
- `grace_period_millis`  default set to 300 milliseconds.
- `grace_period_timeout_secs` default set to 2 seconds.
- `grace_period_hold_stream` if set to `true`, the stream will only start after the grace period check has completed. Default is `false`.
- `shared_burst_buffer_mb` optional (default `12`). Minimum burst buffer size (in MB) used for shared streams.

##### 1.6.1.1 `retry`
If set to `true` on connection loss to provider, the stream will be reconnected.

##### 1.6.1.2 `buffer`
Has 2 attributes
- `enabled`
- `size`

If `enabled` = true The stream is buffered. This is only possible if the provider stream is faster than the consumer.

The stream is buffered with the configured `size`.
`size` is the amount of `8192 byte` chunks. In this case the value `1024` means approx `8MB` for `2Mbit/s` stream.

If you enable `share_live_streams`, each shared channel consumes at least 12 MB of memory,
regardless of the number of clients. Increasing the buffer `size` above `1024` will increase memory usage.
For example, with a buffer size of `2024`, memory usage is at least `24 MB` for **each** shared channel.

This works differently for a BufferedStream. In this case, the buffer size refers to the number of data chunks received 
from the provider. Each chunk can be up to `8 KB` in size. For `1024` chunks, the maximum memory usage would 
be `1024 × 8 KB`, which is approximately `8 MB`  as stated above.


- *a.* if `retry` is `false` and `buffer.enabled` is `false`  the provider stream is piped as is to the client.
- *b.* if `retry` is `true` or  `buffer.enabled` is `true` the provider stream is processed and send to the client.

- The key difference: the `b.` approach is based on complex stream handling and more memory footprint.

##### 1.6.1.3 `throttle` 
Bandwidth throttle (speed limit).
Allowed units are `KB/s`,`MB/s`,`KiB/s`,`MiB/s`,`kbps`,`mbps`,`Mibps`.
Default unit is `kbps`.

| Resolution      |Framerate| Bitrate (kbps) | Quality     |
|-----------------|---------|----------------|-------------|
|480p (854x480)   |  30 fps | 819–2.457      | Low-Quality |
|720p (1280x720)  |  30 fps | 2.457–5.737    | HD-Streams  |
|1080p (1920x1080)|  30 fps | 5.737–12.288   | Full-HD     |
|4K (3840x2160)   |  30 fps | 20.480–49.152  | Ultra-HD    |

##### 1.6.1.3 `grace_period_millis`
If you have a provider or a user where the max_connection attribute is greater than 0,
a grace period can be given during the switchover.
If this period is set too short, it may result in access being denied in some cases.
The default is 0 milliseconds.
If the connection is not throttled, the player will play its buffered content longer than expected.

##### 1.6.1.4 `grace_period_timeout_secs`
How long the grace grant will last, until another grace grant can made.

##### 1.6.1.5 `grace_period_hold_stream`
If set to `true`, tuliprox will wait until the grace period check (defined by `grace_period_millis`) is finished before sending any data to the client. This is useful for players that might time out or error if they receive data and then a "connections exhausted" stream switch occurs. Default is `false`.

#### 1.6.2 `cache`
LRU-Cache is for resources. If it is `enabled`, the resources/images are persisted in the given `dir`. If the cache size exceeds `size`,
In an LRU cache, the least recently used items are evicted to make room for new items if the cache `size`is exceeded.

#### 1.6.3 `resource_rewrite_disabled`
If you have tuliprox behind a reverse proxy and dont want rewritten resource urls inside responses, you can disable the resource_url rewrite.
Default value is false.
If you set it `true` `cache` is disabled! Because the cache cant work without rewritten urls.

```yaml
reverse_proxy:
  resource_rewrite_disabled: false
  stream:
    throttle_kbps: 12500
    retry: true
    buffer:
      enabled: true
      size: 1024
  cache:
    enabled: true
    size: 1GB
    dir: ./cache
```

#### 1.6.4 `rate_limit`
Rate limiting per IP. The burst_size defines the initial number of available connections,
while period_millis specifies the interval at which one connection is replenished.
If behind a proxy `x-forwarded-for`, `x-real-ip` or `forwarded` should be set as header.
The configuration below allows up to 10 connections initially and then replenishes 1 connection every 500 milliseconds.
```yaml
reverse_proxy:
  rate_limit:
    enabled: true
    period_millis: 500
    burst_size: 10
```

#### 1.6.5 `disabled_header`
Controls which headers are removed before tuliprox forwards a request to the upstream provider when acting as a reverse proxy. Use `referer_header` to drop the Referer header, enable `x_header` to strip every header beginning with `X-`, and list any additional headers to remove under `custom_header`.

has the following attributes:
- referer_header
- x_header
- cloudfare_header
- custom_header is a list of header names

```yaml
reverse_proxy:
  disabled_header:
    referer_header: false
    x_header: false
    cloudfare_header: false
    custom_header:
      - my-custom-header
```

#### 1.6.6 `resource_retry`
Controls how aggressively tuliprox retries upstream resource (logo, EPG, stream) downloads whenever it proxies requests for clients.  
It has three attributes:

- `max_attempts`: How many times a failing request should be retried. Defaults to `3`, minimum `1`.
- `backoff_millis`: The wait time between attempts (unless the upstream responds with a `Retry-After` header). Defaults to `250`.
- `backoff_multiplier`: Multiplies the base backoff after each failed attempt. Values `<= 1.0` result in constant (linear) delay; values `> 1.0` produce exponential backoff.

```yaml
reverse_proxy:
  resource_retry:
    max_attempts: 5
    backoff_millis: 500
    backoff_multiplier: 1.5
```

### 1.6.7 `geoip`
`geoip` is for resolving IP addresses to country names.

Disabled by default.
Is used to resolve ip addresses to location.
It has 2 attributes:

```yaml
  geoip:
     enabled: true
     url: <the url> 
```  

The `url` is optional; default value: `https://raw.githubusercontent.com/sapics/ip-location-db/refs/heads/main/asn-country/asn-country-ipv4.csv`
The format is CSV with 3 columns: `range_start,range_end,country_code`.

Example:
```csv
1.0.0.0,1.0.0.255,AU
1.0.1.0,1.0.3.255,CN
1.0.4.0,1.0.7.255,AU
```

#### 1.6.8 `rewrite_secret`
The `rewrite_secret` field is used to keep generated resource URLs stable across application restarts.
Some parts of the system generate URLs that include a hashed or signed component based on an internal secret value.
Normally, this secret would change after every restart, which would invalidate previously generated URLs.

By explicitly setting a `rewrite_secret`, you ensure that the same value is reused on every startup.
This guarantees that resource URLs remain valid, even if the application restarts or updates.

In short:
`rewrite_secret` provides a persistent secret used for generating and verifying rewrite URLs, preventing them from breaking after a restart.

It must be a 32-character hexadecimal string (16 bytes), for example:
```yaml
reverse_proxy:
  rewrite_secret: A1B2C3D4E5F60718293A4B5C6D7E8F90 # Example only — generate your own
```
You can generate a random secret using:
```bash
openssl rand -hex 16
# or
node -e "console.log(require('crypto').randomBytes(16).toString('hex').toUpperCase())"
```

### 1.7 `backup_dir`
is the directory where the backup configuration files written, when saved from the ui.

### 1.8 `update_on_boot`
if set to true, an update is started when the application starts.

### 1.9 `log`
`log` has three attributes
- `sanitize_sensitive_info` default true
- `log_active_user` default false, if set to true reverse proxy client count is printed as info log.
- `log_level` can be set to `trace`, `debug`, `info`, `warn` and `error`.
  You can also set module based level like `hyper_util::client::legacy::connect=error,tuliprox=debug`


`log_level` priority  CLI-Argument, Env-Var, Config, Default(`info`).

```yaml
                    
log:
  sanitize_sensitive_info: false
  log_active_user: true
  log_level: debug
```

### 1.10 `web_ui`
- `enabled`: default is true, if set to false the web_ui is disabled
- `user_ui_enabled`: true or false, for user group editor
- `content_security_policy`: configure Content-Security-Policy headers. When `enabled` is true, the default directives `default-src 'self'`, `script-src 'self' 'wasm-unsafe-eval' 'nonce-{nonce_b64}'`, and `frame-ancestors 'none'` are applied. Additional directives can be added via `custom-attributes`. Enabling CSP may block external images/logos unless allowed via directives like `img-src`.
- `path` is for web_ui path like `/ui` for reverse proxy integration if necessary.
- `player_server` optional, if set the server setting is used for the web-ui-player.
- `kick_secs` default 90 seconds, if a user is kicked from the `web_ui`, they can't connect for this duration. This setting is also used for sleep-timed streams.
- `auth` for authentication settings 
  - `enabled` can be deactivated if `enabled` is set to `false`. If not set default is `true`.
  - `issuer`
  - `secret` is used for jwt token generation.
  - `token_ttl_mins`  default 30 minutes, setting it to 0 uses a 100-year expiration (effectively no expiration)—not recommended for production. !!CAUTION SECURITY RISK!!!
  - `userfile` is the file where the ui users are stored. If the filename is not absolute, `tuliprox` will look into the `config_dir`. If `userfile` is not given, the default value is `user.txt`.

```yaml
web_ui:
  enabled: true
  user_ui_enabled: true
  content_security_policy:
    enabled: true
    custom-attributes:
      - "default-src 'self'"                                        # default value
      - "script-src 'self' 'wasm-unsafe-eval' 'nonce-{nonce_b64}'"  # default value
      - "frame-ancestors 'none'"                                    # default value
      - "style-src 'self'"
      - "img-src 'self' data:"
      - "font-src 'self' data:"
      - "connect-src 'self' wss:"
      - "object-src 'none'"
      - "base-uri 'self'"
      - "form-action 'self'"
  path:
  auth:
    enabled: true
    issuer: tuliprox
    secret: ef9ab256a8c0abe5de92c2e05ca92baa810472ab702ff1674e9248308ceeec92
    userfile: user.txt
```

You can generate a secret for jwt token for example with `node -e "console.log(require('crypto').randomBytes(32).toString('hex'))"`

The userfile has the format  `username: password` per line.
Example:
```
test: $argon2id$v=19$m=19456,t=2,p=1$QUpBWW5uellicTFRUU1tR0RVYVVEUTN5UEJDaWNWQnI3Rm1aNU1xZ3VUSWc3djZJNjk5cGlkOWlZTGFHajllSw$3HHEnLmHW07pjE97Inh85RTi6VN6wbV27sT2hHzGgXk
nobody: $argon2id$v=$argon2id$v=19$m=19456,t=2,p=1$Y2FROE83ZDQ1c2VaYmJ4VU9YdHpuZ2c2ZUwzVkhlRWFpQk80YVhNMEJCSlhmYk8wRE16UEtWemV2dk81cmNaNw$BB81wmEm/faku/dXenC9wE7z0/pt40l4YGh8jl9G2ko
```

The password can be generated with
```shell
./tuliprox --genpwd`
```

or with docker
```shell
docker container exec -it tuliprox ./tuliprox --genpwd
```

The encrypted pasword needs to be added manually into the users file.

## Example config file
```yaml
threads: 4
working_dir: ./data
api:
  host: localhost
  port: 8901
  web_root: ./web
```

### 1.12 `user_access_control`
The default is `false`.
If you set it to `true`,  the attributes (if available)

- expiration date,
- status and
- max_connections

are checked to permit or deny access.

### 1.13 `connect_timeout_secs`
Defines the connection timeout for requests. If the connection takes longer than the specified number of seconds, it is terminated.
If set to 0, the connection attempt continues until the provider closes it or a network timeout occurs.

### 1.14 `custom_stream_response`
If you want to send a picture instead of black screen when a channel is not available or connections exhausted.

Following attributes are available:

- `channel_unavailable`: _optional_
- `user_connections_exhausted`: _optional_
- `provider_connections_exhausted`: _optional_
- `panel_api_provisioning`: _optional_

Video files with name `channel_unavailable.ts`, `user_connections_exhausted`, `provider_connections_exhausted`, `panel_api_provisioning` 
are already available in the docker image.

You can convert an image with `ffmpeg`.

`ffmpeg -loop 1 -i blank_screen.jpg -t 10 -r 1 -an -c:v libx264 -preset veryfast -crf 23 -pix_fmt yuv420p blank_screen.ts`

and add it to the `config.yml`.

`custom_stream_response_path`. The filename identifies the file inside the path
- `user_account_expired.ts`
- `provider_connections_exhausted.ts`
- `user_connections_exhausted.ts`
- `channel_unavailable.ts`
- `panel_api_provisioning.ts`

```yaml
custom_stream_response_path: /home/tuliprox/resources 
```

### 1.15 `user_config_dir`
It is the storage path for user configurations (f.e. bouquets).

### 1.16 `hdhomerun`

This feature allows `tuliprox` to emulate one or more HDHomeRun network tuners, enabling auto-discovery by clients like 
TVHeadend, Plex, Emby, and Jellyfin. It uses both the standard **SSDP/UPnP protocol (Port 1900)** 
for broad compatibility and the **proprietary SiliconDust protocol (Port 65001)** for compatibility with official HDHomeRun tools.

```yaml
hdhomerun:
  enabled: true
  auth: false # Set to true to require basic auth on lineup.json
  devices:
    - name: hdhr1
      tuner_count: 4
      port: 5004 # Each device needs a unique port for its HTTP API
      device_id: "107ABCDF" # Optional: 8-char hex ID. If invalid, will be corrected. If empty, will be generated.
    - name: hdhr2
      port: 5005
      tuner_count: 2
```

The `name` of each device must correspond to a `device` name in a `hdhomerun` output target in your `source.yml`.

```yaml
# In source.yml
targets:
  - name: my-tv-lineup
    output:
      - type: hdhomerun
        device: hdhr1      # Must match a device name from config.yml
        username: local    # The user whose playlist will be served
```

**Configuration Fields:**

- `enabled`: `true` or `false`. Enables or disables the entire HDHomeRun emulation feature. Default: `false`.
- `auth`: `true` or `false`. If `true`, the `/lineup.json` endpoint for each device will require HTTP Basic Authentication using the credentials of the user assigned to the device in `source.yml`. Default: `false`.
- `devices`: A list of virtual HDHomeRun devices to emulate.

**Device Fields:**

- `name`: (Mandatory) A unique internal name for the device (e.g., `hdhr1`). This name is used in your `source.yml` to link a playlist target to a specific virtual tuner.
- `tuner_count`: (Optional) The number of virtual tuners this device will report. Default: `1`.
- `port`: (Optional) The TCP port for this device's HTTP API (`/device.xml`, `/lineup.json`, etc.). Each device must have a unique port. If omitted, `tuliprox` will automatically assign a port, starting from the main API port + 1.
- `friendly_name`: (Optional) The human-readable name that appears in client applications (e.g., "Tuliprox Living Room"). If not specified, a default name is generated.
- `device_id`: (Optional) An 8-character hexadecimal string for the proprietary SiliconDust discovery protocol (Port 65001).
  - If left empty, a valid, random ID will be generated.
  - If you provide a value that is not a valid HDHomeRun ID, `tuliprox` will automatically correct it by calculating the proper checksum.
- `device_udn`: (Optional) The Unique Device Name (UDN) for the standard UPnP/SSDP discovery protocol (Port 1900). It must be a UUID.
  - It is recommended to leave this at its default value. `tuliprox` will automatically append a suffix to ensure it's unique for each device you define.
- `manufacturer`, `model_name`, `model_number`, `firmware_name`, `firmware_version`: (Optional) These fields allow you to customize the device information that is reported to clients. It is safe to leave them at their default values, which mimic a real HDTC-2US model.

**Important Distinction:**
- **`device_id`** is used by the proprietary discovery protocol (UDP port 65001).
- **`device_udn`** is used by the standard SSDP/UPnP discovery protocol (UDP port 1900).

Both are necessary for `tuliprox` to behave like a real HDHomeRun device and ensure maximum compatibility across different client applications.

### 1.17 `proxy`

Proxy configuration for all outgoing requests in `config.yml`. supported http, https, socks5 proxies.

```yaml
proxy:
  url: socks5://192.168.1.6:8123
  username: uname  # <- optional basic auth
  password: secret # <- optional basic auth
```

### 1.18 `ipcheck`
- `url` # URL that may return both IPv4 and IPv6 in one response
- `url_ipv4` # Dedicated URL to fetch only IPv4
- `url_ipv6` # Dedicated URL to fetch only IPv6
- `pattern_ipv4` # Optional regex pattern to extract IPv4
- `pattern_ipv6` # Optional regex pattern to extract IPv6

```yaml
ipcheck:
  url_ipv4: https://ipinfo.io/ip
```

### 1.19 `config_hot_reload`
if set to true, `mapping` files and `api_proxy.yml` are hot reloaded.

⚠️ Important Note for Bind-Mounted Directories
If you are using a bind mount, the file watcher may report the original source path instead of the mount point.

For example, if you have a bind mount like this:

```fstab
/config /home/tuliprox/config none bind 0 0
```

```text
/config                ← your configured mount point
/home/tuliprox/config  ← original source directory
```

and you use `/config` in your configuration files, the file watcher will still report events using `/home/tuliprox/config`.

This means that any file paths returned by the watcher might not match the paths in your configuration.
You need to account for this difference when handling file events, e.g., by mapping the original path to your configured path.

### 1.20 `library`

The local media file library module enables Tuliprox to scan, classify, and serve local video files with automatic metadata resolution.

**Key Features**:
- Recursive directory scanning for video files
- Automatic classification (Movies vs TV Series)
- Multi-source metadata resolution (NFO → TMDB → filename parsing)
- JSON-based metadata storage with UUID tracking
- TMDB API integration with rate limiting
- NFO file reading and writing (Kodi/Jellyfin/Emby/Plex compatible)
- Incremental scanning (only processes changed files)
- Virtual ID management for stable playlist integration

**Configuration Example**:
```yaml
library:
  enabled: true
  scan_directories:
    - enabled: true
      path: "/projects/media"
      content_type: auto
      recursive: true

  supported_extensions:
    - "mp4"
    - "mkv"
    - "avi"
    - "mov"
    - "ts"
    - "m4v"
    - "webm"

  metadata:
    path: "${env:TULIPROX_HOME}/library_metadata"
    read_existing:
      kodi: true
      jellyfin: false
      plex: false

    tmdb:
      enabled: true
      # api_key: "4219e299c89411838049ab0dab19ebd5" # Get your API key from https://www.themoviedb.org/settings/api
      rate_limit_ms: 250  # Milliseconds between API calls (default: 250ms)

    fallback_to_filename: true

    formats:
      - "nfo"  # Optionally write Kodi-compatible NFO files

  playlist:
    movie_category: "Local Movies"
    series_category: "Local Series"

```

**CLI Usage**:
```bash
# Scan VOD directories
./tuliprox --scan-library

# Force full rescan (ignores modification timestamps)
./tuliprox --force-library-rescan

# Show db content
./tuliprox --dbx /opt/tuliprox/data/all_channels/xtream/video.db
./tuliprox --dbm /opt/tuliprox/data/all_channels/m3u.db
./tuliprox --dbe /opt/tuliprox/data/all_channels/xtream/epg.db
```

**API Endpoints**:
- `POST /api/v1/library/scan` - Trigger library scan
  ```json
  {"force_rescan": false}
  ```
- `GET /api/v1/library/status` - Get library status

**Integration with source.yml**:
```yaml
inputs:
- name: local-movies
  type: library  # New input type
  enabled: true

sources:
- inputs:
  - local-movies
```

## 2. `source.yml`

Has the following top level entries:
* `templates` _optional_
* `inputs`
* `sources`

### 2.1 `templates`
If you have a lot of repeats in you regexps, you can use `templates` to make your regexps cleaner.
You can reference other templates in templates with `!name!`.
```yaml
templates:
  - {name: delimiter, value: '[\s_-]*' }
  - {name: quality, value: '(?i)(?P<quality>HD|LQ|4K|UHD)?'}
```
With this definition you can use `delimiter` and `quality` in your regexp's surrounded with `!` like.

`^.*TF1!delimiter!Series?!delimiter!Films?(!delimiter!!quality!)\s*$`

This will replace all occurrences of `!delimiter!` and `!quality!` in the regexp string.

List templates for for sequences can only be used for sequences.
For example if you define this template:
```yaml
templates:
 - name: CHAN_SEQ
   value:
   - '(?i)\bUHD\b'
   - '(?i)\bFHD\b'
```

It can be used inside a sequence
The template can now be used for sequence
```yaml
  sort:
    match_as_ascii: true
    rules:
      - target: group
        field: group
        filter: Input ~ "provider_1"
        order: asc
      - target: channel  
        field: caption
        filter: Group ~ "!US_TNT_ENTERTAIN!"
        order: asc
        sequence:
          - "!CHAN_SEQ!"
          - '(?i)\bHD\b'
          - '(?i)\bSD\b'
```

### 2.2 `inputs`
`inputs` is a list of sources.

Each input has the following attributes:

- `name` is mandatory, it must be unique.
- `type` is optional, default is `m3u`. Valid values are `m3u` and `xtream`
- `enabled` is optional, default is true, if you disable the processing is skipped
- `persist` is optional, you can skip or leave it blank to avoid persisting the input file. The `{}` in the filename is filled with the current timestamp.
- `url` for type `m3u` is the download url or a local filename (can be gzip) of the input-source. For type `xtream`it is `http://<hostname>:<port>`
- `epg` _optional_ xmltv epg configuration
- `headers` is optional
- `method` can be `GET` or `POST`
- `username` only mandatory for type `xtream`
- `password` only mandatory for type `xtream`
- `panel_api` _optional_ for provider panel api operations
- `cache_duration` (_optional_): Playlist cache duration.  
  Supported units are `s`, `m`, `h`, and `d` (seconds, minutes, hours, days).  
  Examples: `12h`, `1d`, `30m`.
  If `cache_duration` is set, the cached provider playlist stored on disk is reused
  for subsequent updates instead of downloading it again.
- `exp_date` optional, is a date as "YYYY-MM-DD HH:MM:SS" format like `2028-11-30 12:34:12` or Unix timestamp (seconds since epoch)
- `options` is optional,
  + `xtream_skip_live` true or false, live section can be skipped.
  + `xtream_skip_vod` true or false, vod section can be skipped.
  + `xtream_skip_series` true or false, series section can be skipped.
  + `xtream_live_stream_without_extension` default false, if set to true `.ts` extension is not added to the stream link.
  + `xtream_live_stream_use_prefix` default true, if set to true `/live/` prefix is added to the stream link.
- `aliases`  for alias definitions for the same provider with different credentials
- `staged` for side loading processed playlists.
  If you already have a provider configured but want to load the playlist from a different source — for example, 
from another playlist editor — you can specify a staged DTO.

Instead of fully configuring everything yourself, you can “stage” a source, meaning you provide a 
ready-made playlist from somewhere else. During the update process, the playlist will be read from this staged source.

However, regular playlist queries (such as streaming or fetching details) will still go through the 
main provider. The staged source is only used temporarily — just for updating the playlist.

In plain words: if you already have a playlist in another online tool or don’t want to deal with Tuliprox mapping,
you can plug that playlist in as a staged input. It won’t replace your main provider — it’s only there to update 
the list. All streaming, proxying, and metadata still come from the provider’s configuration.

`staged` has the following properties:
- `type` is optional, default is `m3u`. Valid values are `m3u` and `xtream`
- `url` for type `m3u` is the download url or a local filename (can be gzip) of the input-source. For type `xtream`it is `http://<hostname>:<port>`
- `headers` is optional
- `method` can be `GET` or `POST`
- `username` only mandatory for type `xtream`
- `pasword`only mandatory for type `xtream`


`persist` should be different for `m3u` and `xtream` types. For `m3u` use full filename like `./playlist_{}.m3u`.
For `xtream` use a prefix like `./playlist_`

Example `epg` config 

Url `auto` is replaced by generated provider epg url.
`priority` is `optional`. 
The `priority` value determines the importance or order of processing. Lower numbers mean higher priority. That is:
A `priority` of `0` is higher than `1`. **Negative numbers** are allowed and represent even higher priority

If `logo_override` is ste to true, the channel logos are replaced by the provider epg logo.

```yaml
epg:
  sources:
    - url: "auto"
      priority: -2
      logo_override: true
    - url: "http://localhost:3001/xmltv.php?epg_id=1"
      priority: -1
    - url: "http://localhost:3001/xmltv.php?epg_id=2"
      priority: 3
    - url: "http://localhost:3001/xmltv.php?epg_id=3"
      priority: 0
  smart_match:
    enabled: true
    fuzzy_matching: true
    match_threshold: 80
    best_match_threshold: 99
    name_prefix: { suffix: "." }
    name_prefix_separator: [':', '|', '-']
    strip :  ["3840p", "uhd", "fhd", "hd", "sd", "4k", "plus", "raw"]
    normalize_regex: '[^a-zA-Z0-9\-]'
```
`match_threshold`is optional and if not set 80.
`best_match_threshold` is optional and if not set 99.
`name_prefix` can be `ignore`, `suffix`, `prefix`. For `suffix` and `prefix` you need to define a concat string.
`strip :  ["3840p", "uhd", "fhd", "hd", "sd", "4k", "plus", "raw"]`  this is the defualt
`normalize_regex: [^a-zA-Z0-9\-]`   is the default

The fuzzy matching tries to guess the EPG ID for a given channel. Some keys are generated based on the channel name for similarity search.
When looking at playlists, it's common for a country prefix to be included in the name, such as `US:` or `FR|`.
The `name_prefix_separator` defines the possible separator characters used to identify this part.
For EPG IDs, the country code is typically added as a suffix, like cnn.us. This is controlled by the name_prefix attribute. 
The `{suffix: '.'}` setting means: if a prefix is found, append it to the name using the given separator character (in this case, a dot).

Example input config for `m3u`
```yaml
inputs:
  - url: 'http://provder.net/get_php?...'
    name: test_m3u
    epg: 'test-epg.xml'
    enabled: false
    persist: 'playlist_1_{}.m3u'
    options: {xtream_skip_series: true}
  - url: 'https://raw.githubusercontent.com/iptv-org/iptv/master/streams/ad.m3u'
  - url: 'https://raw.githubusercontent.com/iptv-org/iptv/master/streams/au.m3u'
  - url: 'https://raw.githubusercontent.com/iptv-org/iptv/master/streams/za.m3u'
sources:
  - inputs:
    - test_m3u
    targets:
    - name: test
      output:
      - type: m3u
        filename: test.m3u
```

Example input config for `xtream`
```yaml
inputs:
  - type: xtream
    persist: 'playlist_1_1{}.m3u'
    headers:
      User-Agent: "Mozilla/5.0 (AppleTV; U; CPU OS 14_2 like Mac OS X; en-us) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/14.0.1 Safari/605.1.15"
    url: 'http://localhost:8080'
    username: test
    password: test
```

Input alias definition for same provider with same content but different credentials.
`max_connections` default is unlimited
```yaml
inputs:
  - type: xtream
    name: my_provider # Mandatory: used for playlist UUID generation
    url: 'http://provider.net'
    username: xyz
    password: secret1
    aliases:
      - name: my_provider_2
        url: 'http://provider.net'
        username: abcd
        password: secret2
        max_connections: 2
sources:
  - inputs:
    - my_provider
    targets:
    - name: test
```

Input aliases can be defined as batches in csv files with `;` separator.
There are 2 batch input types  `xtream_batch` and `m3u_batch`.

##### `XtreamBatch`

```yaml

inputs:
  - type: xtream_batch
    name: my_provider # Mandatory: used for playlist UUID generation
    url: 'file:///home/tuliprox/config/my_provider_batch.csv'
sources:
  - inputs:
    - my_provider
    targets:
    - name: test
```

```csv
#name;username;password;url;max_connections;priority;exp_date
my_provider_1;user1;password1;http://my_provider_1.com:80;1;0;2028-11-23 12:34:23
my_provider_2;user2;password2;http://my_provider_2.com:8080;1;0;2028-11-23 12:34:23
```
Important: the first alias is renamed with the `name` from input definition. `my_provider_1` gets `my_provider`.
This is necessary because of playlist uuid generation and assigning same channel numbers on each update.

##### `M3uBatch`
```yaml
inputs:
  - type: m3u_batch
    url: 'file:///home/tuliprox/config/my_provider_batch.csv'
sources:
  - inputs:
      - m3u_batch
    targets:
    - name: test
```

```csv
#url;max_connections;priority
http://my_provider_1.com:80/get_php?username=user1&password=password1;1;0
http://my_provider_2.com:8080/get_php?username=user2&password=password2;1;0
```

The Fields `max_connections` and `priority`are optional.
`max_connections`  will be set default to `1`. This is different from yaml config where the default is `0=unlimited`

The `priority` value determines the importance or order of processing. Lower numbers mean higher priority. That is:
A `priority` of `0` is higher than `1`
**Negative numbers** are allowed and represent even higher priority
Higher numbers mean **lower priority**
This means tasks or items with smaller (even negative) values will be handled before those with larger values.

The `exp_date` field is a date as:
- "YYYY-MM-DD HH:MM:SS" format like `2028-11-30 12:34:12`
- or Unix timestamp (seconds since epoch)

#### `panel_api`

Tuliprox can optionally call a provider panel API to:
- fetch your current credit balance
- sync `exp_date` with your provider
- renew expired accounts first (based on `exp_date`)
- create a new account and persist it

**Important!** Panel api accounts are not considering unlimited provider access!

Optional alias pool controls:
- `alias_pool.size.min`: `number` or `auto`.
  - `number`: keep at least this many valid (not expired) accounts beyond the defined offset on boot/update. Must be greater `0` and <= `max` (when `max` is a number). Default `1`.
  - `auto`: uses the number of enabled tuliprox users (Active/Trial and not expired) for targets in the same source. If below, tuliprox tries to renew expired accounts first and then creates new accounts until the amount of enabled users is met during boot/update + offset. User add/update triggers only when `max` is also `auto`.
- `alias_pool.size.max`: `number` or `auto`. 
  - `number`: upper bound for valid accounts when provisioning is triggered by provider exhaustion. When the maximum is reached, provisioning (renew/create) is skipped. Must be greater than `0` and >= `min`. Default `1`.
  - `auto`: no upper bound; if `min` is also `auto`, alias-pool min checks are triggered when tuliprox users are added/updated.
- `alias_pool.remove_expired`: `boolean`
  - `true`: remove expired accounts from `source.yml` or batch CSVs during boot/update. This cleanup runs last in the panel_api routines and only removes aliases/rows (the root input is not removed).

Provisioning settings:
- `panel_api.provisioning.timeout_sec`: `number`
  - Maximum wait time (seconds) to probe a newly created/renewed account before forcing a client reconnect or continuing boot/update process.
  - Default `65`
- `panel_api.provisioning.method`: `HEAD` | `GET` | `POST`
  - HTTP method used for probes. 
  - Default `HEAD`
- `panel_api.provisioning.probe_interval_sec`: `number`
  - Probe interval in seconds.
  - Default `10`
- `panel_api.provisioning.cooldown_sec`: `number`
  - Extra wait time (seconds) after a successful probe before continuing boot/update provisioning. If you continue to see a 5XX message during the boot/update process despite a successful probe, gradually increase the cooldown time to give the provider enough time to provision the new root account.
  - Default `0`
- `panel_api.provisioning.offset`: e.g.: `15m` | `5h` | `1d`
  - Optional pre-expiry window for boot/update renewal of input accounts with `exp_date`; if `now + offset > exp_date`, tuliprox tries `client_renew`, and falls back to `client_new` if renew fails. Supports suffixes `s` (seconds), `m` (minutes), `h` (hours), `d` (days), e.g. `30m`, `12h`, `2d` 
  - Default `None`


The API is configured generically via predefined query parameters.
Optional fields can be deactivated by leaving them blank.
Use the literal value `auto` to fill sensitive values at runtime:
- in `account_info`: (_optional_)
  - `api_key: auto` is replaced by `panel_api.api_key`
- in `client_info`: (_mandatory_)
  - `api_key: auto` is replaced by `panel_api.api_key`
  - `username: auto`: are replaced by the account being queried
  - `password: auto` are replaced by the account being queried
- in `client_renew`: (_optional_)
  - `api_key: auto` is replaced by `panel_api.api_key`
  - `username: auto` are replaced by the account being renewed
  - `password: auto` are replaced by the account being renewed
  - `type: m3u` is the only supported type
- in `client_new`: (_optional_)
  - `api_key: auto` is replaced by `panel_api.api_key`
  - `username: auto` are replaced by the account being renewed
  - `password: auto` are replaced by the account being renewed
  - `type: m3u` is the only supported type
- in `client_adult_content`: (_optional_)
  - `api_key: auto` is replaced by `panel_api.api_key`
  - `username: auto` are replaced by the account being queried
  - `password: auto` are replaced by the account being queried

`account_info`
 
Is executed on boot/update to fetch account credits via the `credits` field. If credentials are required, (username/password=auto), Tuliprox uses the first available ones: the root input if present, otherwise the first alias in config order. If no credentials are required, none are used or if not auto the configured one is used.”

`client_info` 

Is used to fetch the exact `exp_date` (via the `expire` field) and is also executed on boot/update to sync `exp_date` for existing inputs/aliases.

`client_adult_content` 

Optionally executed after `client_new` or `client_renew` to unlock adult content.


Response evaluation logic
Tuliprox evaluates Panel API responses as JSON with the following logic, depending on the operation:

`Common rule (all operations)`

  - Require `status: true`.
  - If status is missing or not true, the operation is treated as failed.

`account_info (credits)`

  - Require `status: true`.
  - Extract `credits` and persist it to `panel_api.credits`.

`client_info (sync expiration)`

  -	Require `status: true`.
  -	Extract the expiration timestamp/date from the JSON field and normalize it to UTC:
    -	`expire` → used to populate/update exp_date for the corresponding input/alias.

`client_new (create new account)`
	
  -	Require `status: true`.
  -	Attempt to extract credentials directly from the JSON response:
    - username
    - password
  -	If one or both fields are missing, tuliprox attempts a fallback extraction from a URL contained in the JSON:
    -	If the JSON contains a url field, tuliprox parses it and tries to extract username/password from it (e.g., query string or embedded credentials depending on the provider’s URL format).
  -	If credentials cannot be derived from either the direct fields or the url fallback, the operation is treated as failed and no alias is persisted.

`client_renew (renew existing account)`
 
  -	Require `status: true`.
  -	No credentials are extracted or updated as part of renew.

`client_adult_content (toggle adult content)`

  - Require `status: true`.
```yaml
- sources:
- inputs:
  - type: xtream
    name: my_provider
    url: 'http://provider.net'
    username: xyz
    password: secret1
    panel_api:
      url: 'https://panel.example.tld/api.php'
      api_key: '1234567890'
      provisioning:
        timeout_sec: 65
        method: GET
        probe_interval_sec: 10
        cooldown_sec: 120
        offset: 12h
      alias_pool:
        size:
          min: auto
          max: auto
        remove_expired: true
      query_parameter:
        account_info:
          - { key: action, value: account_info }
          - { key: api_key, value: auto }
        client_info:
          - { key: action, value: client_info }
          - { key: username, value: auto }
          - { key: password, value: auto }
          - { key: api_key, value: auto }
        client_new:
          - { key: action, value: new }
          - { key: type, value: m3u }
          - { key: sub, value: '1' }
          - { key: api_key, value: auto }
        client_renew:
          - { key: action, value: renew }
          - { key: type, value: m3u }
          - { key: username, value: auto }
          - { key: password, value: auto }
          - { key: sub, value: '1' }
          - { key: api_key, value: auto }
        client_adult_content:
          - { key: action, value: adult_content }
          - { key: username, value: auto }
          - { key: password, value: auto }
          - { key: api_key, value: auto }
      credits: "0.0"
```
For `client_new`, the Panel API call would look like this in the example shown:

```text
https://panel.example.tld/api.php?action=new&type=m3u&sub=1&api_key=1234567890
```

### 2.3. `sources`
`sources` is a sequence of source definitions, which have two top level entries:
-`inputs`
-`targets`

### 2.3.1 `inputs`
Is a list of input names, from the inputs defined in the inputs section of `source.yml`.

### 2.3.2 `targets`
Has the following top level entries:
- `enabled` _optional_ default is `true`, if you disable the processing is skipped
- `name` _optional_ default is `default`, if not default it has to be unique, for running selective targets
- `sort`  _optional_
- `output` _mandatory_ list of output formats
- `processing_order` _optional_ default is `frm`
- `options` _optional_
- `filter` _mandatory_,
- `rename` _optional_
- `mapping` _optional_
- `watch` _optional_
- `use_memory_cache`, default is false. If set to `true` playlist is cached into memory to reduce disc access.
Placing playlist into memory causes more RAM usage but reduces disk access.

### 2.2.2.1 `sort`
Has three top level attributes
- `match_as_ascii` _optional_ default is `false`
- `rules`

#### `rules`

This is a list of sort configurations. Each configuration has the following top-level entries:
- `target` - can be `group` or `channel`. 
- `field`:
  - for target `channel`: `title`, `name`, `caption` or `url`.
  - for target `group`: `group`.
- `filter` - a filter expression.
- `order` - can be `asc`, `desc`, or `none` (which skips sorting for that group_pattern and keeps the playlist order coming from the sources).
- `sequence` _optional_  - a list of regexp matching field values (based on `field`). These are used to sort based on index. The `order` is ignored for this entries.

The pattern should be selected considering the processing sequence.

```yaml
sort:
  rules:
    - target: group
      order: asc
      filter: Group ~ ".*" 
      field: group
      sequence:
        - '^Freetv'
        - '^Shopping'
        - '^Entertainment'
        - '^Sunrise'
    - target: channel
      order: asc
      filter: Group ~ ".*"
      field: title
      sequence:
        - '(?P<c1>.*?)\bUHD\b'
        - '(?P<c1>.*?)\bFHD\b'
        - '(?P<c1>.*?)\bHD\b'
        - '(?P<c1>.*?)\bSD\b'
```
In the example above, groups are sorted based on the specified sequence.
Channels within the `Freetv` group are first sorted by `quality` (as matched by the regexp sequence), and then by the `captured prefix`.

To sort by specific parts of the content, use named capture groups such as `c1`, `c2`, `c3`, etc.
The numeric suffix indicates the priority: `c1` is evaluated first, followed by `c2`, and so on.

### 2.2.2.2 `output`

Is a list of output format:
Each format has different properties.
`Attention:` Output filters are applied after all transformations have been performed, therefore, all filter contents must refer to the final state of the playlist.

#### 'Target types':
`xtream`
- type: xtream
- skip_live_direct_source: true|false,
- skip_video_direct_source: true|false,
- skip_series_direct_source: true|false,
- resolve_series: true|false,
- resolve_series_delay: seconds,
- resolve_vod: true|false,
- resolve_vod_delay: true|false,
- trakt: Trakt Configuration
- filter: optional filter

`m3u`
- type: m3u
- filename: _optional_
- include_type_in_url: _optional_, true|false, default false
- mask_redirect_url: _optional_,  true|false, default false
- filter: optional filter

`strm`
- directory: _mandatory_,
- username: _optional_,
- underscore_whitespace: _optional_, true|false, default false
- cleanup: _optional_, true|false, default false
- style: _mandatory_, kodi|plex|emby|jellyfin
- flat: _optional_, true|false, default false
- strm_props: _optional_, list of strings
- add_quality_to_filename: _optional_, true|false
- filter: optional filter

`hdhomerun`
- device: _mandatory_,
- username: _mandatory_,
- use_output: _optional_, m3u|xtream

`options`
- ignore_logo:  _optional_,  true|false, default false
- share_live_streams:  _optional_,  true|false, default false
- remove_duplicates:  _optional_,  true|false, default false
- `force_redirect` _optional_


```yaml
targets:
  - name: xc_m3u
    output:
      - type: xtream
        skip_live_direct_source: true
        skip_video_direct_source: true
      - type: m3u
      - type: strm
        directory: /tmp/kodi
      - type: hdhomerun
        username: hdhruser
        device: hdhr1
        use_output: xtream
    options: {ignore_logo: false, share_live_streams: true, remove_duplicates: false}
```

### 2.2.2.3 `processing_order`
The processing order (Filter, Rename and Map) can be configured for each target with:
`processing_order: frm` (valid values are: frm, fmr, rfm, rmf, mfr, mrf. default is frm)

### 2.2.2.4 `options`
Target options are:

- `ignore_logo` logo attributes are ignored to avoid caching logo files on devices.
- `share_live_streams` to share live stream connections  in reverse proxy mode.
- `remove_duplicates` tries to remove duplicates by `url`.

If you enable share_live_streams, each shared channel consumes at least 12 MB of memory,
regardless of the number of clients. Increasing the buffer size above 1024 will increase memory usage. 
For example, with a buffer size of 2024, memory usage is at least 24 MB for **each** shared channel.     

`strm` output has additional options:
- `underscore_whitespace`: Replaces all whitespaces with `_` in the path and filename.
- `cleanup`: If `true`, the directory given at `filename` will be deleted. Don't point at existing media folder or everything will be deleted!
- `style`: Naming style convention for your media player / server (kodi, plex, emby, jellyfin)
- `flat`: If `true`, creates flat directory structure with category tags in folder names
- `strm_props`: List of stream properties placed within .strm file to configure how Kodi's internal player handles the media stream.
- `add_quality_to_filename`: If `true`, adds media quality tags to the filename (e.g., `Movie Title - [1080p|x265|HDR].strm`).

Supported styles:
- Kodi: `Movie Name (Year) {tmdb=ID}/Movie Name (Year).strm`
- Plex: `Movie Name (Year) {tmdb-ID}/Movie Name (Year).strm`
- Emby: `Movie Name (Year) [tmdbid=ID]/Movie Name (Year).strm`
- Jellyfin: `Movie Name (Year) [tmdbid-ID]/Movie Name (Year).strm`

If style is set to 'kodi', the property `#KODIPROP:seekable=true|false` is added. And if `strm_props` is not given `#KODIPROP:inputstream=inputstream.ffmpeg`, `"#KODIPROP:http-reconnect=true` are set too for style `kodi`.

`m3u` output has additional options
- `include_type_in_url`, default false, if true adds the stream type `live`, `movie`, `series` to the url of the stream.
- `mask_redirect_url`, default false, if true uses urls from `api_proxy.yml` for user in proxy mode `redirect`.
  Needs to be set `true`  if you have multiple provider and want to cycle in redirect mode.

`xtream` output has additional options
- `skip_live_direct_source`  if true the direct_source property from provider for live is ignored
- `skip_video_direct_source`  if true the direct_source property from provider for movies is ignored
- `skip_series_direct_source`  if true the direct_source property from provider for series is ignored

Iptv player can act differently and use the direct-source attribute or can compose the url based on the server info.
The options `skip_live_direct_source`, `skip_video_direct_source` and`skip_series_direct_source`
are default `true` to avoid this problem.
You can set them fo `false`to keep the direct-source attribute.

Because xtream api delivers only the metadata to series, we need to fetch the series and resolve them. But be aware,
each series info entry needs to be fetched one by one and the provider can ban you if you are doing request too frequently.
- `resolve_series` if is set to `true` and you have xtream input and m3u output, the series are fetched and resolved.
  This can cause a lot of requests to the provider. Be cautious when using this option.
- `resolve_series_delay` to avoid a provider ban you can set the seconds between series_info_request's. Default is 2 seconds.
  But be aware that the more series entries there are, the longer the process takes.

For `resolve_(vod|series)` the files are only fetched one for each input and cached. Only new and modified ones are updated.

The `kodi` format for movies can contain the `tmdb-id` (_optional_). Because xtream api delivers the data only on request,
we need to fetch this info for each movie entry. But be aware the provider can ban you if you are doing request too frequently.
- `xtream` `resolve_vod` if is set to `true` and you have xtream input, the movies info are fetched and stored.
  This can cause a lot of requests to the provider. Be cautious when using this option.
- `xtream` `resolve_vod_delay` to avoid a provider ban you can set the seconds between vod_info_request's. Default is 2 seconds.
  But be aware that the more series entries there are, the longer the process takes.
  Unlike `series info` `movie info` is only fetched once for each movie. If the data is stored locally there will be no update.

There is a difference for `resolve_vod` and `resolve_series`.
`resolve_series` works only when input: `xtream` and output: `m3u`.
`resolve_vod` works only when input: `xtream`.


- `xtream` `trakt`:
Trakt.tv is an online platform that helps you track, manage, and discover TV shows and movies. Think of it like Goodreads for TV and film.
You can add trakt list matches into your playlist.
You can define a `Trakt` config like
```yaml
      - name: iptv-trakt-example
        output:
          - type: xtream
            skip_live_direct_source: true
            skip_video_direct_source: true
            skip_series_direct_source: true
            resolve_series: false
            resolve_vod: false
            trakt:
              api:
                api_key: "your api key"
                version: "2"
                url: "https://api.trakt.tv"
                user_agent: "Mozilla/5.0"
              lists:
                - user: "linaspurinis"
                  list_slug: "top-watched-movies-of-the-week"
                  category_name: "📈 Top Weekly Movies"
                  content_type: "vod"
                  fuzzy_match_threshold: 80
                - user: "garycrawfordgc"
                  list_slug: "latest-tv-shows"
                  category_name: "📺 Latest TV Shows"
                  content_type: "series"
                  fuzzy_match_threshold: 80
```
This will create 2 new categories with matched entries. 

### 2.2.2.5 `filter`
The filter is a string with a filter statement.
The filter can have UnaryExpression `NOT`, BinaryExpression `AND OR`, Regexp Comparison `(Group|Title|Name|Url) ~ "regexp"`
and Type Comparsison `Type = vod` or `Type = live` or `Type = series`.
Filter fields are `Group`, `Title`, `Name`, `Caption`, `Url`, `Genre`, `Input` and `Type`.
Example filter:  `((Group ~ "^DE.*") AND (NOT Title ~ ".*Shopping.*")) OR (Group ~ "^AU.*")`

If you use characters like `+ | [ ] ( )` in filters don't forget to escape them!!

The regular expression syntax is similar to Perl-style regular expressions,
but lacks a few features like look around and backreferences.  
To test the regular expression i use [regex101.com](https://regex101.com/).
Don't forget to select `Rust` option which is under the `FLAVOR` section on the left.

### 2.2.2.6 `rename`
Is a List of rename configurations. Each configuration has 3 top level entries.
- `field` can be  `group`, `title`, `name`, `caption`  or `url`.
- `pattern` is a regular expression like `'^TR.:\s?(.*)'`
- `new_name` can contain capture groups variables addressed with `$1`,`$2`,...

`rename` supports capture groups. Each group can be addressed with `$1`, `$2` .. in the `new_name` attribute.

This could be used for players which do not observe the order and sort themselves.
```yaml
rename:
  - { field: group,  pattern: ^DE(.*),  new_name: 1. DE$1 }
```
In the above example each entry starting with `DE` will be prefixed with `1.`.

(_Please be aware of the processing order. If you first map, you should match the mapped entries!_)

### 2.2.2.7 `mapping`
`mapping: <list of mapping id's>`

Mapping can be defined in a file, or multiple mapping files can be stored in the mapping path.
If you use a mapping path, you need to set `mapping_path` in `config.yml`
The files are loaded in **alphanumeric** order.
**Note:** This is a lexicographic sort — so `m_10.yml` comes before `m_2.yml` unless you name files carefully (e.g., `m_01.yml`, `m_02.yml`, ..., `m_10.yml`).

The filename or path can be given as `-m` argument. (See Mappings section)

Default mapping file is `maping.yml`

## Example source.yml file
```yaml
templates:
- name: PROV1_TR
  value: >-
    Group ~ "(?i)^.TR.*Ulusal.*" OR
    Group ~ "(?i)^.TR.*Dini.*" OR
    Group ~ "(?i)^.TR.*Haber.*" OR
    Group ~ "(?i)^.TR.*Belgesel.*"
- name: PROV1_DE
  value: >-
    Group ~ "^(?i)^.DE.*Nachrichten.*" OR
    Group ~ "^(?i)^.DE.*Freetv.*" OR
    Group ~ "^(?i)^.DE.*Dokumentation.*"
- name: PROV1_FR
  value: >-
    Group ~ "((?i)FR[:|])?(?i)TF1.*" OR
    Group ~ "((?i)FR[:|])?(?i)France.*"
- name: PROV1_ALL
  value:  "!PROV1_TR! OR !PROV1_DE! OR !PROV1_FR!"
inputs:
  - enabled: true
    name: my_provider_1
    url: http://myserver.net/playlist.m3u
    persist: ./playlist_{}.m3u
sources:
  - inputs:
      - my_provider_1
    targets:
      - name: pl1
        output:
          - type: m3u
            filename: playlist_1.m3u
        processing_order: frm
        options:
          ignore_logo: true
        sort:
          order: asc
        filter: "!PROV1_ALL!" 
        rename:
          - field: group
            pattern: ^DE(.*)
            new_name: 1. DE$1
      - name: pl1strm
        enabled: false
        output:
          - type: strm
            filename: playlist_strm
        options:
          ignore_logo: true
          underscore_whitespace: false
          style: kodi
          cleanup: true
          flat: true
        sort:
          order: asc
        filter: "!PROV1_ALL!"
        mapping:
           - France
        rename:
          - field: group
            pattern: ^DE(.*)
            new_name: 1. DE$1
```

### 2.2.2.8 `favourites`
Allows you to explicitly add items to a favorite group based on a filter. This is processed after mapping and resolution.

- `cluster`: can be Series, Movie or Live.
- `group`: The name of the group to add the favorite items to.
- `filter`: A filter statement to select the original items.
- `match_as_ascii`: _optional_ (default `false`). If `true`, the filter matching will be case-insensitive and normalized (e.g., "Cinema" matches "Cinéma").

Example:
```yaml
favourites:
  - cluster: series
    group: "My Favourites"
    filter: 'Name ~ "Cinema"'
    match_as_ascii: true
```

### 2.2.2.9 `watch`
For each target with a *unique name*, you can define watched groups.
It is a list of regular expression matching final group names from this target playlist.
Final means in this case: the name in the resulting playlist after applying all steps
of transformation.

For example given the following configuration:
```yaml
watch:
  - 'FR - Movies \(202[34]\)'
  - 'FR - Series'
```

Changes from this groups will be printed as info on console and send to
the configured messaging (f.e. telegram channel).

To get the watch notifications over messaging notify_on `watch` should be enabled.  
In `config.yml`
```yaml
messaging:
  notify_on:
    - watch
```

## 3. `mapping.yml`
Has the root item `mappings` which has the following top level entries:
- `templates` _optional_
- `mapping` _mandatory_

Instead of using a single `mapping.yml` file, you can use multiple mapping files
when you set `mapping_path` in `config.yml` to a directory.

The files are loaded in **alphanumeric** order.
**Note:** This is a lexicographic sort — so `m_10.yml` comes before `m_2.yml` unless you name files carefully (e.g., `m_01.yml`, `m_02.yml`, ..., `m_10.yml`).

The filename or path can be given as `-m` argument. (See Mappings section)

Default mapping file is `maping.yml`

### 3.1 `templates`
If you have a lot of repeats in you regexps, you can use `templates` to make your regexps cleaner.
You can reference other templates in templates with `!name!`;
```yaml
templates:
  - {name: delimiter, value: '[\s_-]*' }
  - {name: quality, value: '(?i)(?P<quality>HD|LQ|4K|UHD)?'}
```
With this definition you can use `delimiter` and `quality` in your regexp's surrounded with `!` like.

`^.*TF1!delimiter!Series?!delimiter!Films?(!delimiter!!quality!)\s*$`

This will replace all occurrences of `!delimiter!` and `!quality!` in the regexp string.

### 2.3 `mapping`
Has the following top level entries:
- `id` _mandatory_
- `match_as_ascii` _optional_ default is `false`
- `create_alias` _optional_ default is `false`
- `mapper` _mandatory_
- `counter` _optional_

### 2.3.1 `id`
Is referenced in the `config.yml`, should be a unique identifier

### 2.3.2 `match_as_ascii`
If you have non-ASCII characters in your playlist (e.g., `é`, `ö`, `ß`) and want to write filters without considering these accents (e.g., using `e` to match `é`), set this option to `true`.
The system will automatically deunicode the field values on-the-fly during filtering and mapping operations.


Example:
```yaml
mapping:
  - id: favourites_news
    match_as_ascii: true
    mapper:
      - filter: 'Group ~ "(?i)news"'
        script: |
          add_favourite("Favourites")
```

### 2.3.4 `mapper`
Has the following top level entries:
- `filter`
- `script`

#### 2.3.4.1 `filter`
The filter  is a string with a statement (@see filter statements).
It is optional and allows you to filter the content.

#### 2.3.4.2 `script`
Script has a custom DSL syntax. 

This Domain-Specific Language (DSL) supports simple scripting operations including variable assignment, 
string operations, pattern matching, conditional mapping, and structured data access. 
It is whitespace-tolerant and uses familiar programming concepts with a custom syntax.

**Basic elements:**
- Identifiers: `Variable Names` composed of ASCII alphanumeric characters and underscores.
- FieldNames: `Playlist Field Names` starting with `@` following compose of ASCII alphanumeric characters and underscores.
- Strings / Text: Enclosed in double quotes. "example string" 
- Null value `null`
- Regexp Matching:   `@FieldName ~ "Regexp"` like in filter statements. You can match a `FieldName` or a existing `variable`.
- Access a field in a regexp match result:  with `result.capture`. For example, if you have multiple captures you can access them by their name, or their index beginning at `1` like `result.1`, `result.2`.
- Builtin functions: 
  - concat(a, b, ...)
  - uppercase(a)
  - lowercase(a)
  - capitalize(a)
  - trim(a)
  - print(a, b, c)
  - number(a)
  - first(a)
  - template(a)
  - replace(text, match, replacement)
  - pad(text | number, number, char, optional position: "<" | ">" | "^")
  - format(fmt_text, ...args)
  - add_favourite(group_name)
Field names are:  `name`, `title"`, `caption"`, `group"`, `id"`, `chno"`, `logo"`, `logo_small"`, `parent_code"`, `audio_track"`, `time_shift" |  "url"`, `epg_channel_id"`, `epg_id`.
Format is very simple and only supports in text replacement like  `format("Hello {}! Hello {}!", "Bob", "World")`  
When you use Regular expressions it could be that your match contains multiple results.
The builtin function `first` returns the first match.
Example `print(uppercase("hello"))`. output is only visible in `trace` log level you can enable it like `log_level: debug,tuliprox::foundation::mapper=trace` in config
- Assignment assigns an expression result. variable or field.
```dsl
  @Title = uppercase("hello")
  hello = concat(capitalize("hello"), " ", capitalize("world")) 
```
-  Match block evaluates expressions based on multiple matching cases.
Note: **The order of the cases are important.**

```dsl
result = match {
    (var1, var2) => result1,  <- only executed when both variables set
     var2 => result2,  <- only executed when var2 variable is set
     var3 => result3,  <- only executed when var3 variable is set
     _ => default <-  matches anything.
   }
```
- Map block assigns expression results to a variable or field

Mapping over text
It is possible to define multiple keys with `|` seperated for one case.
```dsl
result = map variable_name {
    "key1" => result1,
    "key2" => result2,
    _ => default
}

result = map variable_name {
    "key1" | "key2" => result1,
    _ => null
}
```

Mapping over number ranges
```dls
  year_text = @Caption ~ "(\d{4})\)?$"
  year = number(year_text)

  year_group = map year {
   ..2019 => "< 2020",
   2020..2023 => "2020 - 2023",
   2024..2025 => "2024 - 2025",
   2025.. => "> 2025",
   _ =>  year_text,
  }
```            

Example `if then else` block

```dsl
  # Maybe there is no station
  station = @Caption ~ "ABC"
  match {
     station => {
        # if block
        # station exists
     }
     # optional any match as else block
     _ => {
         # else block
         # station does not exists
     } 
  }
```

Example `for each` block

Iterates over a `Named` result (a list of key-value tuples).
The syntax is `variable.for_each( (key, value) => { ... })`.
The parameters `key` and `value` are variable names you define to access the tuple elements inside the loop.

You can use `_` for parameters you want to ignore (e.g., `(_, value)` or `(key, _)`). However, at least one parameter must be named (you cannot use `(_, _)`).

`Named` variables are created by:
1. **`split()` function**: keys are indices ("0", "1", ...), values are the split parts.
2. **Regex with capture groups**: keys are group names (or indices), values are the captured matches.

```dsl
  # 1. Using split()
  # Split the genre string into a Named result (index as key, genre as value)
  genres = split(@Genre, "[,/&]")
  
  # Iterate over each genre, ignoring the index
  genres.for_each((_, genre) => {
     # 'genre' will contain the split string value
     add_favourite(concat("Genre - ", genre))
  })

  # 2. Using Regex with named capture groups
  # Extract info using regex, creating a Named result like [("Movie", "Inception"), ("Year", "2010")]
  info = @Title ~ "(?P<Movie>.*?)\s-\s(?P<Year>\d{4})"
  
  info.for_each((k, v) => {
      # k will be "Movie" then "Year"
      # v will be "Inception" then "2010"
      print(concat("Found ", k, ": ", v))
  })
```

Example of removing prefix
`@Caption = replace(@Caption, "UK:",  "EN:"`

Example `mapping.yml`

```yaml
mappings:
  templates:
    # Template to match and capture different qualities in the caption (FHD, HD, SD, UHD)
    - name: QUALITY
      value: '(?i)\b([FUSL]?HD|SD|4K|1080p|720p|3840p)\b'

    - name: COAST
      value: '(?i)\b(EAST|WEST)\b'

    - name: USA_TNT_FILTER
      value: 'Caption ~ "(?i)^(US|USA|United States).*?TNT"'

    - name: US_TNT_PREFIX
      value: "US: TNT"

    - name: US_TNT_ENTERTAIN_GROUP
      value: "United States - Entertainment"

    # Template to capture the group name for US TNT channels
    - name: US_TNT_ENTERTAIN
      value: 'Group ~ "^United States - Entertainment"'

  mapping:
    # Mapping rules for all channels
    - id: all_channels
      match_as_ascii: true
      mapper:
        - filter: "!USA_TNT_FILTER!"
          script: |
            coast = Caption ~ "!COAST!"
            quality = uppercase(Caption ~ "!QUALITY!")
            quality = map quality {
                       "SHD" => "SD",
                       "LHD" => "HD",
                       "720p" => "HD",
                       "1080p" => "FHD",
                       "4K" => "UHD",
                       "3840p" => "UHD",
                        _ => quality,
            }
            coast_quality = match {
                (coast, quality) => concat(capitalize(coast), " ", uppercase(quality)),
                coast => concat(capitalize(coast), " HD"),
                quality => concat("East ", uppercase(quality)),
                _ => "East HD",
            }
            @Caption = concat("!US_TNT_PREFIX!", " ", coast_quality)
            @Group = "!US_TNT_ENTERTAIN_GROUP!"
```
### 2.3.5 counter

Each mapping can have a list of counter.

A counter has the following fields:
- `filter`: filter expression
- `value`: an initial start value
- `field`: `title`, `name`, `chno`
- `modifier`: `assign`, `suffix`, `prefix`
- `concat`: is _optional_ and only used if `suffix` or `prefix` modifier given.
- `padding`: is _optional_ 

```yaml
mapping:
  - id: simple
    match_as_ascii: true
    counter:
      - filter: 'Group ~ ".*FR.*"'
        value: 9000
        field: title
        padding: 2
        modifier: suffix
        concat: " - "
    mapper:
      - <Mapper definition>
```

### 2.5 Example mapping.yml file.
```yaml
mappings:
    templates:
      - name: delimiter
        value: '[\s_-]*'
      - name: quality
        value: '(?i)(?P<quality>HD|LQ|4K|UHD)?'
      - name: source
        value: 'Url ~ "https?:\/\/(.*?)\/(?P<query>.*)$"'
    mapping:
      - id: France
        match_as_ascii: true
        mapper:
          - filter: 'Name ~ "^TF.*"'
            script: |
              query_match = @Url ~ "https?:\/\/(.*?)\/(?P<query>.*)$" 
              @Url = concat("http://my.iptv.proxy.com/", query_match.query)
```

## 3. Api-Proxy Config

If you use tuliprox to deliver playlists, we require a configuration to provide the necessary server information, rewrite URLs in reverse proxy mode, and define users who can access the API.

For this purpose, we use the `api-proxy.yml` configuration.

You can specify the path to the file using the `-a` CLI argument.

You can define multiple servers with unique names; typically, two are defined—one for the local network and one for external access.
One server should be named `default`.

```yaml
server:
  - name: default
    protocol: http
    host: 192.169.1.9
    port: '8901'
    timezone: Europe/Paris
    message: Welcome to tuliprox
  - name: external
    protocol: https
    host: tuliprox.mydomain.tv
    port: '443'
    timezone: Europe/Paris
    message: Welcome to tuliprox
    path: tuliprox
```

User definitions are made for the targets. Each target can have multiple users. Usernames and tokens must be unique.

```yaml
user:
- target: xc_m3u
  credentials:
  - username: test1
    password: secret1
    token: 'token1'
    proxy: reverse
    server: default
    exp_date: 1672705545
    max_connections: 1
    status: Active
```

`username` and `password`are mandatory for credentials. `username` is unique.
The `token` is _optional_. If defined it should be unique. The `token`can be used
instead of username+password
`proxy` is _optional_. If defined it can be `reverse` or `redirect`. Default is `redirect`.
Reverse Proxy mode for user can be a subset
  - `reverse`           -> all reverse
  - `reverse[live]`     -> only live reverse, vod and series redirect
  - `reverse[live,vod]` -> series redirect, others reverse

`server` is _optional_. It should match one server definition, if not given the server with the name `default` is used or the first one.  
`epg_timeshift` is _optional_. It is only applied when source has `epg_url` configured. `epg_timeshift: [-+]hh:mm or TimeZone`, example  
`-2:30`(-2h30m), `1:45` (1h45m), `+0:15` (15m), `2` (2h), `:30` (30m), `:3` (3m), `2:` (2h), `Europe/Paris`, `America/New_York` 
- `max_connections` is _optional_
- `status` is _optional_
- `exp_date` is _optional_
- `max_connections`, `status` and `exp_date` are only used when `user_access_control` ist ste to true.
- `user_ui_enabled` is _optional_. If defined it can be `true` or `false`. Default is `true`. Disable/enable web_ui for user
- `user_access_control` is _optional_. If defined it can be `true` or `false`. Default is `false`. 

If you have a lot of users and dont want to keep them in `api-proxy.yml`, you can set the option
- `use_user_db` to true to store the user information inside a db-file.

If the `use_user_db` option is switched to `false` or `true`, the users will automatically
be migrated to the corresponding file (`false` → `api_proxy.yml`, `true` → `api_user.db`).

If you set  `use_user_db` to `true` you need to use the `Web-UI` to `edit`/`add`/`remove` users.

To access the api for:
- `xtream` use url like `http://192.169.1.2/player_api.php?username={}&password={}`
- `m3u` use url `http://192.169.1.2/get.php?username={}&password={}`
  or with token
- `xtream` use url like `http://192.169.1.2/player_api.php?token={}`
- `m3u` use url `http://192.169.1.2/get.php?token={}`

To access the xmltv-api use url like `http://192.169.1.2/xmltv.php?username={}&password={}`

_Do not forget to replace `{}` with credentials._

If you use the endpoints through rest calls, you can use, for the sake of simplicity:
- `m3u` inplace of `get.php`
- `xtream` inplace of `player_api.php`
- `epg` inplace of `xmltv.php`
- `token` inplace of `username` and `password` combination

When you define credentials for a `target`, ensure that this target has
`output` format  `xtream`or `m3u`.

The `proxy` property can be `reverse`or `redirect`. `reverse` means the streams are going through tuliprox, `redirect` means the streams are comming from your provider.

If you use `https` you need a ssl terminator. `tuliprox` does not support https traffic.

If you use a ssl-terminator or proxy in front of tuliprox you can set a `path` to make the configuration of your proxy simpler.
For example you use `nginx` as your reverse proxy.

`api-proxy.yml`
```yaml
server:
- name: default
  protocol: http
  host: 192.169.1.9
  port: '8901'
  timezone: Europe/Paris
  message: Welcome to tuliprox
- name: external
  protocol: https
  host: tuliprox.mydomain.tv
  port: '443'
  timezone: Europe/Paris
  message: Welcome to tuliprox
  path: tuliprox
user:
  - target: xc_m3u
    credentials:
      - username: test1
        password: secret1
        token: 'token1'
        proxy: reverse
        server: default
        exp_date: 1672705545
        max_connections: 1
        status: Active
```

If you use a reverse proxy in front of Tuliprox, don’t forget to forward:
- `X-Real-IP`
- `X-Forwarded-For`


Now you can do `nginx`  configuration like
```config
   location /tuliprox {
      rewrite ^/tuliprox/(.*)$ /$1 break;
      proxy_set_header X-Real-IP $remote_addr;
      proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
      proxy_set_header X-NginX-Proxy true;
      proxy_pass http://192.169.1.9:8901/;
      proxy_set_header Host $http_host;
      proxy_redirect off;
      proxy_buffering off;
      proxy_request_buffering off;
      proxy_cache off;
      tcp_nopush on;
      tcp_nodelay on;
   }
```
When you use nginx be sure to have 
```nginx
      proxy_redirect off;
      proxy_buffering off;
      proxy_request_buffering off;
      proxy_cache off;
      tcp_nopush on;
      tcp_nodelay on;
```
because without this config you could get very high cpu peaks.

You can also use traefik as reverse proxy server in front of your tuliprox instance. However if you wan't to use paths, you must note that the path for web-ui and api-proxy must be different. In this short example used paths are:
* web-ui: tuliprox
* api-proxy: tv
```yaml
labels:
  # ----- Service -----
  - "traefik.enable=true"

  # ----- HTTP (Port 80) -----
  - "traefik.http.routers.tuliprox.entrypoints=web"
  - "traefik.http.routers.tuliprox.rule=Host(`tv.my-domain.io`) && (PathPrefix(`/tv`) || PathPrefix(`/tuliprox`))" # 1. path: api-proxy endpoint || 2. path: web-ui endpoint

  # ----- HTTPS (Port 443) -----
  - "traefik.http.routers.tuliprox-secure.entrypoints=websecure"
  - "traefik.http.routers.tuliprox-secure.rule=Host(`tv.my-domain.io`) && (PathPrefix(`/tv`) || PathPrefix(`/tuliprox`))" # 1. path: api-proxy endpoint || 2. path: web-ui endpoint
  - "traefik.http.routers.tuliprox-secure.service=tuliprox"
  
  # ----- Serviceport -----
  - "traefik.http.services.tuliprox.loadbalancer.server.port=8901"

  # ----- Middlewares -----
  - "traefik.http.middlewares.tuliprox-strip.stripprefix.prefixes=/tv"  # <-- important for api-proxy endpoint
  - "traefik.http.routers.tuliprox.middlewares=forward-real-ip@file,tuliprox-strip@docker"
  - "traefik.http.routers.tuliprox-secure.middlewares=forward-real-ip@file,tuliprox-strip@docker"
```
Example:
```yaml
server:
  - name: default 
    protocol: http
    host: 192.168.0.3
    port: 80
    timezone: Europe/Paris
    message: Welcome to tuliprox
  - name: external
    protocol: https
    host: my_external_domain.com
    port: 443
    timezone: Europe/Paris
    message: Welcome to tuliprox
    path: /tuliprox
  - target: pl1
    credentials:
      - {username: x3452, password: ztrhgrGZ, token: 4342sd, proxy: reverse, server: external, epg_timeshift: -2:30}
      - {username: x3451, password: secret, token: abcde, proxy: redirect}
```

## 5. Logging
Following log levels are supported:
- `debug`
- `info` _default_
- `warn`
- `error`

Use the `-l` or `--log-level` cli-argument to specify the log-level.

The log level can be set through environment variable `TULIPROX_LOG`,
or config.

Precedence is cli-argument, env-var, config, default(`info`).

Log Level has module support like `tuliprox::util=error,tuliprox::filter=debug,tuliprox=debug`

## 6. Web-UI

The WebUI is for configuration the tuliprox config.

If you enable authentication, users can log in with their accounts (you can disable login per user),
and configure their playlist.

## 6. Compilation

### Docker build
Change into the root directory and run:

```shell
docker build --rm -f docker/Dockerfile -t tuliprox .  
```

This will build the complete project and create a docker image.

To start the container, you can use the `docker-compose.yml`
But you need to change `image: ghcr.io/euzu/tuliprox:latest` to `image: tuliprox`


### Manual build static binary for docker

#### `cross`compile

Ease way to compile is a docker toolchain `cross`

```shell
rust install cross
env  RUSTFLAGS="--remap-path-prefix $HOME=~" cross build -p tuliprox --release --target x86_64-unknown-linux-musl
```

#### Manual compile - install prerequisites
```shell
rustup update
sudo apt-get install pkg-config musl-tools libssl-dev
rustup target add x86_64-unknown-linux-musl
```
#### Build statically linked binary
```shell
cargo build -p tuliprox --target x86_64-unknown-linux-musl --release
```
#### Dockerize
There is a Dockerfile in `docker` directory. 

##### Build Image

Targets are 
- `scratch-final`
- `alpine-final`

```shell
# Build for a specific architecture
docker build --rm -f docker/Dockerfile -t tuliprox --target scratch-final --build-arg RUST_TARGET=x86_64-unknown-linux-musl .

docker build --rm -f docker/Dockerfile -t tuliprox --target scratch-final --build-arg RUST_TARGET=aarch64-unknown-linux-musl .

docker build --rm -f docker/Dockerfile -t tuliprox --target scratch-final --build-arg RUST_TARGET=armv7-unknown-linux-musleabihf .

docker build --rm -f docker/Dockerfile -t tuliprox --target scratch-final --build-arg RUST_TARGET=x86_64-apple-darwin .
```
##### docker-compose.yml
```docker
version: '3'
services:
  tuliprox:
    container_name: tuliprox
    image: tuliprox
    user: "133:144"
    working_dir: /app
    volumes:
      - /opt/tuliprox/config:/app/config
      - /opt/tuliprox/data:/app/data
      - /opt/tuliprox/backup:/app/backup
      - /opt/tuliprox/downloads:/app/downloads
    environment:
      - TZ=Europe/Paris
    ports:
      - "8901:8901"
    restart: unless-stopped
```
This example is for the local image, the official can be found under `ghcr.io/euzu/tuliprox:latest`

If you want to use tuliprox with docker-compose, there is a `--healthcheck` argument for healthchecks

```docker
    healthcheck:
      test: ["CMD", "/app/tuliprox", "-p", "/app/config" "--healthcheck"]  
      interval: 30s  
      timeout: 10s   
      retries: 3     
      start_period: 10s
``` 

#### Installing in LXC Container (Alpine)
To get it started in a Alpine 3.19 LXC

```shell
apk update
apk add nano git yarn bash cargo perl-local-lib perl-module-build make 
cd /opt
git clone https://github.com/euzu/tuliprox.git
cd /opt/tuliprox/bin
./build_lin.sh
ln -s /opt/tuliprox/target/release/tuliprox /bin/tuliprox 
cd /opt/tuliprox/frontend
yarn
yarn build
ln -s /opt/tuliprox/frontend/build /web
ln -s /opt/tuliprox/config /config
mkdir /data
mkdir /backup
```

**Creating a service, create /etc/init.d/tuliprox**
```shell
#!/sbin/openrc-run
name=tuliprox
command="/bin/tuliprox"
command_args="-p /config -s"
command_user="root"
command_background="yes"
output_log="/var/log/tuliprox/tuliprox.log"
error_log="/var/log/tuliprox/tuliprox.log"
supervisor="supervise-daemon"

depend() {
    need net
}

start_pre() {
    checkpath --directory --owner $command_user:$command_user --mode 0775 \
           /run/tuliprox /var/log/tuliprox
}
```

**then add it to boot**
```shell
rc-update add tuliprox default
```


### Cross compile for windows on linux
If you want to compile this project on linux for windows, you need to do the following steps.

#### Install mingw packages for your distribution
For ubuntu type:
```shell
sudo apt-get install gcc-mingw-w64
```
#### Install mingw support for rust
```shell
rustup target add x86_64-pc-windows-gnu
rustup toolchain install stable-x86_64-pc-windows-gnu
```

Compile it with:
```shell
cargo build -p tuliprox --release --target x86_64-pc-windows-gnu
```

### Cross compile for raspberry pi 2/3/4

Ease way to compile is a docker toolchain `cross`

```shell
rust install cross
env  RUSTFLAGS="--remap-path-prefix $HOME=~" cross build -p tuliprox --release --target armv7-unknown-linux-musleabihf
```

# Different Scenarios
## Using `tuliprox` with a m3u provider.
todo.

## Using `tuliprox` with a xtream provider.

You have a provider who supports the xtream api.

The provider gives you:
- the url: `http://fantastic.provider.xyz:8080`
- username: `tvjunkie`
- password: `junkie.secret`
- epg_url: `http://fantastic.provider.xyz:8080/xmltv.php?username=tvjunkie&password=junkie.secret`


To use `tuliprox` you need to create the configuration.
The configuration consist of 4 files.
- config.yml
- source.yml
- mapping.yml
- api-proxy.yml

The file `mapping.yml`is optional and only needed if you want to do something linke renaming titles or changing attributes.

Lets start with `config.yml`. An example basic configuration is:

```yaml
api: {host: 0.0.0.0, port: 8901, web_root: ./web}
working_dir: ./data
update_on_boot: true
```

This configuration starts `tuliprox`and listens on the 8901 port. The downloaded playlists are stored inside the `data`-folder in the current working directory.
The property `update_on_boot` is optional and can be helpful in the beginning until you have found a working configuration. I prefer to set it to false.

Now we have to define the sources we want to import. We do this inside `source.yml`

```yaml
templates:
- name: ALL_CHAN
  value: 'Group ~ ".*"'

inputs:
- type: xtream
  name: my_provider
  url: 'http://fantastic.provider.xyz:8080'
  epg_url: 'http://fantastic.provider.xyz:8080/xmltv.php?username=tvjunkie&password=junkie.secret'
  username: tvjunkie
  password: junkie.secret
  options: {xtream_info_cache: true}
sources:
- inputs:
  - my_provider
  targets:
  - name: all_channels
    output:
      - type: xtream
    filter: "!ALL_CHAN!"
    options: {ignore_logo: false, skip_live_direct_source: true, skip_video_direct_source: true}
```

What did we do? First, we defined the input source based on the information we received from our provider.
Then we defined a target that we will create from our source.
This configuration creates a 1:1 copy (this is probably not what we want, but we discuss the filtering later).

Now we need to define the user access to the created target. We need to define `api-proxy.yml`.

```yaml
server:
- name: default
  protocol: http
  host: 192.168.1.41
  port: '8901'
  timezone: Europe/Berlin
  message: Welcome to tuliprox
- name: external
  protocol: https
  host: tvjunkie.dyndns.org
  port: '443'
  timezone: Europe/Berlin
  message: Welcome to tuliprox
user:
- target: all_channels
  credentials:
  - username: xt
    password: xt.secret
    proxy: redirect
    server: default
  - username: xtext
    password: xtext.secret
    proxy: redirect
    server: external
```
We have defined 2 server configurations. The `default` configuration is intended for use in the local network, the IP address is that of the computer on which `tuliprox` is running. The `external` configuration is optional and is only required for access from outside your local network. External access requires port forwarding on your router and an SSL terminator proxy such as nginx and a dyndns provider configured from your router if you do not have a static IP address (this is outside the scope of this manual).

The next section of the `api-proxy.yml` contains the user definition. We can define users for each `target` from the `source.yml`.
This means that each `user` can only access one `target` from `source.yml`.  We have named our target `all_channels` in `source.yml` and used this name for the user definition.  We have defined 2 users, one for local access and one for external access.
We have set the proxy type to `redirect`, which means that the client will be redirected to the original provider URL when opening a stream. If you set the proxy type to `reverse`, the stream will be streamed from the provider through `tuliprox`. Based on the hardware you are running `tuliprox` on, you can opt for the proxy type `reverse`. But you should start with `redirect` first until everything works well.

If no server is specified for a user, the default one is taken.


To access a xtream api from our IPTV-application we need at least 3 information  the `url`, `username` and `password`.
All this information are now defined in `api-proxy.yml`.
- url: `http://192.168.1.41:8901`
- username: `xt`
- password: `xt.secret`

Start `tuliprox`,  fire up your IPTV-Application, enter credentials and watch.

# It works well, but I don't need all the channels, how can I filter?

You need to understand regular expressions to define filters. A good site for learning and testing regular expressions is [regex101.com](https://regex101.com). Don't forget to set FLAVOR on the left side to Rust.

To adjust the filter, you must change the `source.yml` file.
What we have currently is: (for a better overview I have removed some parts and marked them with ...)

```yaml
templates:
- name: ALL_CHAN
  value: 'Group ~ ".*"'
inputs:
- type: xtream
  name: my_provider
  ...
sources:
- inputs:
  - my_provider
  targets:
  - name: all_channels
    output:
      - type: xtream
    filter: "!ALL_CHAN!"
      
```

We use templates to make the filters easier to maintain and read.

Ok now let's start.

First: We have a lot of channel groups we dont need.

`tuliprox` excludes or includes groups or channels based on filter. Usable fields for filter are `Group`, `Name` and `Title`.
The simplest filter is:

`<Field> ~ <Regular Expression>`.  For example  `Group ~ ".*"`. This means include all categories.

Ok, if you only want the Shopping categories, here it is: `Group ~ ".*Shopping.*"`. This includes all categories whose name contains shopping.

Wait, we are missing categories that contain 'shopping'. Regular expressions are case-sensitive. You must explicitly define a case-insensitive regexp. `Group ~ "(?i).*Shopping.*"` will match everything containing Shopping, sHopping, ShOppInG,....

But what if i want to reverse the filter? I dont want a shoppping category. How can I achieve this? Quite simply with `NOT`.
`NOT(Group ~ "(?i).*Shopping.*")`. Thats it.


You can combine Filter with `AND` and `OR` to create more complex filter.

For example:
`(Group ~ "^FR.*" AND NOT(Group ~ "^FR.*SERIES.*" OR Group ~ "^DE.*EINKAUFEN.*" OR Group ~ "^EN.*RADIO.*" OR Group ~ "^EN.*ANIME.*"))`

As you can see, this can become very complex and unmaintainable. This is where the templates come into play.

We can disassemble the filter into smaller parts and combine them into a more powerfull filter.

```yaml
templates:
- name: NO_SHOPPING
  value: 'NOT(Group ~ "(?i).*Shopping.*" OR Group ~ "(?i).*Einkaufen.*") OR Group ~ "(?i).*téléachat.*"'
- name: GERMAN_CHANNELS
  value: 'Group ~ "^DE: .*"'
- name: FRENCH_CHANNELS
  value: 'Group ~ "^FR: .*"'
- name: MY_CHANNELS
  value: '!NO_SHOOPING! AND (!GERMAN_CHANNELS! OR !FRENCH_CHANNELS!)'

inputs:
- type: xtream
  name: my_provider
  ...
sources:
- inputs:
  - my_provider
  targets:
  - name: all_channels
    output:
      - type: xtream
    filter: "!MY_CHANNELS!"
    ...
 ```

The resulting playlist contains all French and German channels except Shopping.

Wait, we've only filtered categories, but what if I want to exclude a specific channel?
No Problem. You can write a filter for your channel using the `Name` or `Title` property.
`NOT(Title ~ "FR: TV5Monde")`. If you have this channel in different categories, you can alter your filter like:
`NOT(Group ~ "FR: TF1" AND Title ~ "FR: TV5Monde")`.

```yaml
templates:
  - name: NO_SHOPPING
    value: 'NOT(Group ~ "(?i).*Shopping.*" OR Group ~ "(?i).*Einkaufen.*") OR Group ~ "(?i).*téléachat.*"'
  - name: GERMAN_CHANNELS
    value: 'Group ~ "^DE: .*"'
  - name: FRENCH_CHANNELS
    value: 'Group ~ "^FR: .*"'
  - name: NO_TV5MONDE_IN_TF1
    value: 'NOT(Group ~ "FR: TF1" AND Title ~ "FR: TV5Monde")'
  - name: EXCLUDED_CHANNELS
    value: '!NO_TV5MONDE_IN_TF1! AND !NO_SHOOPING!'
  - name: MY_CHANNELS
    value: '!EXCLUDED_CHANNELS! AND (!GERMAN_CHANNELS! OR !FRENCH_CHANNELS!)'
```

# VLC seek problem when  *user_access_control* is enabled.
The issue with **max_connection** is that setting a hard limit can cause problems during channel switching. Seeking, for instance,
is essentially a rapid channel change — because each seek action triggers a new request to the provider.

Players like VLC calculate the seek position and determine the appropriate byte range based on the content size.
Then, a **partial request** is made using that byte range — that’s what we call a seek operation.

The more frequently a user seeks, the more they bombard the provider with new requests.

Now here's the tricky part: requests can come in so quickly that the termination of the previous connection is delayed.
This leads to the **max_connection** problem — the system might think the user is still connected multiple times.

To handle this, we introduce a **grace period_millis** and **grace_period_timeout_secs**.
```yaml
 grace_period_millis: 2000
 grace_period_timeout_secs: 5
```
The grace period means: if a user reaches the connection limit, we still allow one more connection for a short time.
After a delay, we check whether old connections have been properly closed. If not, we then enforce the limit and terminate the excess connection(s).

# Mapper example
## Grouping
We asume we have some groups with the text EU, SATELLITE, NATIONAL, NEWS, MUSIC, SPORT, RELIGION, FILM, KIDS, DOCU
in the group name.
We wwant to group the channels inside  NEWS.  NATIONAL, SATELLITE by their quality.
The other groups should get a number prefix for ordering.

```yaml
  group = Group ~ "(EU|SATELLITE|NATIONAL|NEWS|MUSIC|SPORT|RELIGION|FILM|KIDS|DOCU)"
  quality = Caption ~ "\b([F]?HD[i]?)\b"
  title_match = Caption ~ "(.*?)\:\s*(.*)"
  title_prefix = title_match.1
  title_name = title_match.2

  # suffix '*' for SATELLITE
  title_name = map title_prefix {
     "SATELLITE" =>  concat(title_name, "*"),
     _ => title_name,
  }

  quality = map group {
      "NEWS" | "NATIONAL" | "SATELLITE" => quality,
      _ => null,
  }

  prefix = map quality {
   "HD" => "01.",
   "FHD" => "02.",
   "HDi" => "03.",
   _ => map group {
      "NEWS" => "04.",
      "DOCU" => "05.",
      "SPORT" => "06.",
      "NATIONAL" => "07.",
      "RELIGION" => "08.",
      "KIDS" => "09.",
      "FILM" => "10.",
      "MUSIC" => "11.",
      "EU" => "12.",
      "SATELLITE" => "13.",
      _ => group
    },
  }

  name = match {
    quality => concat(prefix, " FR [", quality, "]"),
    group => concat(prefix, " FR [", group, "]"),
    _ => prefix
  }

  @Group = name
  @Caption = title_name
```
The transformation logic processes each entry and modifies two key fields:
- `Group`: The group or category the stream belongs to.
- `Caption`: The title or name of the stream.
It extracts data from these fields and applies structured transformations.

This extracts a known group keyword from `Group`
`group = Group ~ "(EU|SATELLITE|NATIONAL|NEWS|MUSIC|SPORT|RELIGION|FILM|KIDS|DOCU)"`

Quality subset detection -> HD, FHD, HDi
`quality = @Caption ~ "\b([F]?HD[i]?)\b"`

Title splitting. As you can see there are 2 captures, the first one is the prefix and the second one is the name.
You get something like 
- title_prefix = 'FR'
- title_name = 'TV5Monde'
from "FR: TV5Monde"```
title_match = @Caption ~ "(.*?)\:\s*(.*)"
title_prefix = title_match.1
title_name = title_match.2
```

We will later merge 3 groups together and want to keep the quality for the group name.
For example all channels from the groups "NEWS", "NATIONAL" amd "SATELLITE" will go
into new groups named by the previously extracted quality.
```dsl
quality = map group {
"NEWS" | "NATIONAL" | "SATELLITE" => quality,
_ => null,
}
```
is equivalent to
```python
if group in ["NEWS", "NATIONAL", "SATELLITE"]:
   keep quality
else:
   quality = null
```

Generate prefix. We have later 3 new groups named by the quality. 
We want to put them in some order and prefix them with a counter.
This could be later done with counter sequence too. (And would be better if some groups get empty)

if the current plalyist item has one of the qualities we set the prefix according to quality,
otherwise we use the group category.
```dsl
  prefix = map quality {
   "HD" => "01.",
   "FHD" => "02.",
   "HDi" => "03.",
   _ => map group {
      "NEWS" => "04.",
      "DOCU" => "05.",
      "SPORT" => "06.",
      "NATIONAL" => "07.",
      "RELIGION" => "08.",
      "KIDS" => "09.",
      "FILM" => "10.",
      "MUSIC" => "11.",
      "EU" => "12.",
      "SATELLITE" => "13.",
      _ => group
    },
  }
```

Final name construction

```dsl
  name = match {
    quality => concat(prefix, " FR [", quality, "]"),
    group => concat(prefix, " FR [", group, "]"),
    _ => prefix
  }
```

is equivalent to 

```python
if quality is set:
    name = prefix + " FR [" + quality + "]"
elif group is set:
    name = prefix + " FR [" + group + "]"
else:
    name = prefix
``` 

Update the playlist item
- Group is overwritten with the new formatted name.
- Caption is overwritten with the cleaned-up title name.

```dsl
  @Group = name
  @Caption = title_name
```

## Grouping by release year
We want to automatically group these channels by their release year, using the following logic:
- All movies released before 2020 should be grouped together under one label.
- Movies from 2020 onward should each be grouped by their specific year.
Example title: "Master Movie (2020)"
The result should look like
- FR | Movies < 2020
- FR | Movies 2020
- FR | Movies 2021
- FR | Movies <and so on>
```dsl
- filter: 'Group ~ "^FR" AND Caption ~ "\(?\d{4}\)?$"'
  script: |
    year_text = @Caption ~ "(\d{4})\)?$"
    year = number(year_text)
    year_group = map year {
     ..2019 => "< 2020",
     _ =>  year_text,
    }
    @Group = concat("FR | MOVIES ", year_group)
```
Filter: Matches channels where the Group starts with "FR" and the Caption ends in a 4-digit year (optionally inside parentheses).
Regexp extraction: Pulls the 4-digit year from the caption.
Mapping:
 If the year is ≤ 2019, it maps to " < 2020".
 Otherwise, the group is named by the actual year (e.g., "2021").
Assignment: Constructs a new group label like "FR | MOVIES 2021" and assigns it to @Group
