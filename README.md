# <img src="https://github.com/user-attachments/assets/8ef9ea79-62ff-4298-978f-22326c5c3d02" height="24" /> **tuliprox** - A Powerful Playlist Processing & Proxy Tool

**tuliprox** is a flexible and efficient tool built in Rust 🦀,
designed for processing, filtering, and serving media playlists with minimal resource usage.

<p align="center">
<img src="https://github.com/user-attachments/assets/8ef9ea79-62ff-4298-978f-22326c5c3d02" height="128" />
</p>

## 🔧 Core Features:

- **Advanced Playlist Processing**: Filter, rename, map, and sort entries with ease.
- **Flexible Proxy Support**: Acts as a reverse/redirect proxy for EXTM3U, Xtream Codes, HDHomeRun, and STRM (Kodi) formats.
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
* `threads` _optional_
* `messaging`  _optional_
* `video` _optional_
* `schedules` _optional_
* `backup_dir` _optional_
* `update_on_boot` _optional_
* `web_ui_enabled` _optional_
* `web_auth` _optional_
* `reverse_proxy` _optional_
* `log` _optional
* `user_access_control` _optional_
* `connect_timeout_secs`: _optional_ and used for provider requests connection timeout.
* `custom_stream_response` _optional_
* `hdhomerun` _optional_
* `proxy` _optional_
* `ipcheck` _optional_

### 1.1. `threads`
If you are running on a cpu which has multiple cores, you can set for example `threads: 2` to run two threads.
Don't use too many threads, you should consider max of `cpu cores * 2`.
Default is `0`.
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
Currently `telegram`, `rest` and `pushover.net` is supported.

Messaging is Opt-In, you need to set the `notify_on` message types which are
- `info`
- `stats`
- `error`

`telegram`, `rest` and `pushover.net` configurations are optional.

```yaml
messaging:
  notify_on:
    - info
    - stats
    - error
  telegram:
    bot_token: '<telegram bot token>'
    chat_ids:
      - '<telegram chat id>'
  rest:
    url: '<api url as POST endpoint for json data>'

  pushover:
    token: <api_token>
    user: <api_username>
    url: `optional`, default is `https://api.pushover.net/1/messages.json`
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
- `grace_period_timeout_secs` efault set to 2 seconds.

##### 1.6.1.1 `retry`
If set to `true` on connection loss to provider, the stream will be reconnected.

##### 1.6.1.2 `buffer`
Has 2 attributes
- `enabled`
- `size`

If `enabled` = true The stream is buffered. This is only possible if the provider stream is faster than the consumer.
The stream is buffered with the configured `size`.
`size` is the amount of `8192 byte` chunks. In this case the value `1024` means approx `8MB` for `2Mbit/s` stream.

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

#### 1.6.5 `disable_referer_header`
This option, when set to `true`, prevents tuliprox from sending the Referer header in requests made when acting as a reverse proxy. This can be particularly useful when dealing with certain Xtream Codes providers that might restrict or behave differently based on the Referer header. Default is `false`.

```yaml
reverse_proxy:
  disable_referer_header: false
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
- enabled: default is true, if set to false the web_ui is disabled
- user_ui_enabled, true or false,  for user bouquet editor
- path is for web_ui path like `/ui` for reverse proxy integration if necessary.
- auth for authentication settings
  - `enabled` can be deactivated if `enabled` is set to `false`. If not set default is `true`.
  - `issuer`
  - `secret` is used for jwt token generation.
  - `userfile` is the file where the ui users are stored. If the filename is not absolute, `tuliprox` will look into the `config_dir`. If `userfile` is not given, the default value is `user.txt`.
```yaml
web_ui:
  enabled: true
  user_ui_enabled: true
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
- ` provider_connections_exhausted`: _optional_

Video files with name `channel_unavailable.ts`, `user_connections_exhausted`, `provider_connections_exhausted`
are already available in the docker image.

You can convert an image with `ffmpeg`.

`ffmpeg -loop 1 -i blank_screen.jpg -t 10 -r 1 -an -c:v libx264 -preset veryfast -crf 23 -pix_fmt yuv420p blank_screen.ts`

and add it to the `config.yml`.

```yaml
custom_stream_response:
  channel_unavailable: /home/tuliprox/channel_unavailable.ts
  user_connections_exhausted: /home/tuliprox/user_connections_exhausted.ts
  provider_connections_exhausted: /home/tuliprox/provider_connections_exhausted.ts
```

### 1.15 `user_config_dir`
It is the storage path for user configurations (f.e. bouquets).

### 1.16 `hdhomerun`

It is possible to define `hdhomerun` target for output. To use this outputs we need to define HdHomeRun devices.
Supports now basic auth like <http://user:password@ip:port/lineup.json>.

The simplest config looks like:
```yaml
hdhomerun:
  enabled: true
  auth: true
  devices:
  - name: hdhr1
  - name: hdhr2
```

The `name` must be unique and is used in the target configuration in `source.yml` like.

```yaml
sources:
- inputs:
  - name: ...
    ...
  targets:
  - name: xt_m3u
    output:
      - type: xtream
      - type: hdhomerun
        username: xtr
        output: hdhr1
    filter: "!ALL_FILTER!"
```

The HdHomerun config has the following attribute:
`enabled`:  default is `false`,  you need to set it to `true`
`devices`: is a list of HdHomeRun Device configuraitons.
For each output you need to define one device with a unique name. Each output gets his own port to connect.

HdHomeRun device config has the following attributes:

- `name`: _mandatory_ and must be unique
- `tuner_count`: _optional_, default 1
- `friendly_name`: _optional_
- `manufacturer`: _optional_
- `model_name`: _optional_
- `model_number`: _optional_
- `firmware_name`: _optional_
- `firmware_version`: _optional_
- `device_type`: _optional_
- `device_udn`: _optional_
- `port`: _optional_, if not given the tuliprox-server port is incremented for each device.

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


## 2. `source.yml`

Has the following top level entries:
* `templates` _optional_
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

### 2.2. `sources`
`sources` is a sequence of source definitions, which have two top level entries:
-`inputs`
-`targets`

### 2.2.1 `inputs`
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
- `pasword`only mandatory for type `xtream`
- `prefix` is optional, it is applied to the given field with the given value
- `suffix` is optional, it is applied to the given field with the given value
- `options` is optional,
  + `xtream_skip_live` true or false, live section can be skipped.
  + `xtream_skip_vod` true or false, vod section can be skipped.
  + `xtream_skip_series` true or false, series section can be skipped.
  + `xtream_live_stream_without_extension` default false, if set to true `.ts` extension is not added to the stream link.
  + `xtream_live_stream_use_prefix` default true, if set to true `/live/` prefix is added to the stream link.
- `aliases`  for alias definitions for the same provider with different credentials

`persist` should be different for `m3u` and `xtream` types. For `m3u` use full filename like `./playlist_{}.m3u`.
For `xtream` use a prefix like `./playlist_`

`prefix` and `suffix` are appended after all processing is done, but before sort.
They have 2 fields:
- `field` can be `name` , `group`, `title`, `caption`
- `value` a static text

Example `epg` config 

```yaml
epg:
  auto_epg: true
  url: ['http://localhost:3001/xmltv.php?epg_id=1', 'http://localhost:3001/xmltv.php?epg_id=2']
  smart_match:
    enabled: true
    fuzzy_matching: true
    match_threshold: 80
    best_match_threshold: 99
    name_prefix: !suffix "."
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
The `!suffix '.'` setting means: if a prefix is found, append it to the name using the given separator character (in this case, a dot).

```yaml
# single epg
url: 'https://localhost.com/epg.xml'
```
```yaml
# multi local file  epg
url: ['file:///${env:TULIPROX_HOME}/epg.xml', 'file:///${env:TULIPROX_HOME}/epg2.xml']
```
```yaml
# multi url  epg
url: ['http://localhost:3001/xmltv.php?epg_id=1', 'http://localhost:3001/xmltv.php?epg_id=2']
```

Example input config for `m3u`
```yaml
sources:
- inputs:
    - url: 'http://provder.net/get_php?...'
      name: test_m3u
      epg: 'test-epg.xml'
      enabled: false
      persist: 'playlist_1_{}.m3u'
      options: {xtream_skip_series: true}
    - url: 'https://raw.githubusercontent.com/iptv-org/iptv/master/streams/ad.m3u'
    - url: 'https://raw.githubusercontent.com/iptv-org/iptv/master/streams/au.m3u'
    - url: 'https://raw.githubusercontent.com/iptv-org/iptv/master/streams/za.m3u'
  targets:
   - name: test
     output:
       - type: m3u
         filename: test.m3u
```

Example input config for `xtream`
```yaml
sources:
  inputs:
    - type: xtream
      persist: 'playlist_1_1{}.m3u'
      headers:
        User-Agent: "Mozilla/5.0 (AppleTV; U; CPU OS 14_2 like Mac OS X; en-us) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/14.0.1 Safari/605.1.15"
        Accept: application/json
        Accept-Encoding: gzip
      url: 'http://localhost:8080'
      username: test
      password: test
```

Input alias definition for same provider with same content but different credentials.
`max_connections` default is unlimited
```yaml
- sources:
- inputs:
  - type: xtream
    name: my_provider
    url: 'http://provider.net'
    username: xyz
    password: secret1
    aliases:
    - name: my_provider_2 
      url: 'http://provider.net'
      username: abcd
      password: secret2
      max_connections: 2
  targets:
  - name: test
```

Input aliases can be defined as batches in csv files with `;` separator.
There are 2 batch input types  `xtream_batch` and `m3u_batch`.

##### `XtreamBatch`

```yaml
- sources:
- inputs:
  - type: xtream_batch
    url: 'file:///home/tuliprox/config/my_provider_batch.csv'
  targets:
  - name: test
```

```csv
#name;username;password;url;max_connections;priority
my_provider_1;user1;password1;http://my_provider_1.com:80;1;0
my_provider_2;user2;password2;http://my_provider_2.com:8080;1;0
```

##### `M3uBatch`
```yaml
- sources:
- inputs:
  - type: m3u_batch
    url: 'file:///home/tuliprox/config/my_provider_batch.csv'
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

### 2.2.2 `targets`
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

### 2.2.2.1 `sort`
Has three top level attributes
- `match_as_ascii` _optional_ default is `false`
- `groups`
- `channels`

#### `groups`
has one top level attribute `order` which can be set to `asc`or `desc`.
#### `channels`
is a list of sort configurations for groups. Each configuration has 3 top level entries.
- `field` can be  `group`, `title`, `name`, `caption` or `url`.
- `group_pattern` is a regular expression like `'^TR.:\s?(.*)'` which is matched against group title.
- `order` can be `asc` or `desc`
- `sequence` _optional_  is a list of regexp matching field values (based on `field`) which are used to sort based on index. The `order` is ignored for this entries.

The pattern should be selected taking into account the processing sequence.

```yaml
  groups:
  order: asc
  sequence:
    - '^Freetv'
    - '^Shopping'
    - '^Entertainment'
    - '^Sunrise'
  channels:
    - field: caption
      group_pattern: '^Freetv'
      order: asc
      sequence:
        - '(?P<c1>.*?)\bUHD\b'
        - '(?P<c1>.*?)\bFHD\b'
        - '(?P<c1>.*?)\bHD\b'
        - '(?P<c1>.*?)\bSD\b'
```
In the example above, groups are sorted based on the specified sequence.
Channels within the `Freetv` group are first sorted by `quality` (as matched by the regex sequence), and then by the `captured prefix`.

To sort by specific parts of the content, use named capture groups such as `c1`, `c2`, `c3`, etc.
The numeric suffix indicates the priority: `c1` is evaluated first, followed by `c2`, and so on.


### 2.2.2.2 `output`

Is a list of output format:
Each format has different properties

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

`m3u`
- type: m3u
- filename: _optional_
- include_type_in_url: _optional_, true|false, default false
- mask_redirect_url: _optional_,  true|false, default false

`strm`
- directory: _mandatory_,
- username: _optional_,
- underscore_whitespace: _optional_,  true|false, default false
- cleanup:  _optional_,  true|false, default false
- kodi_style:  _optional_,  true|false, default false
- strm_props: _optional_,  list of strings,

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

`strm` output has additional options
- `underscore_whitespace` replaces all whitespaces with `_` in the path.
- `cleanup` deletes the directory given at `filename`. Don't point at existing media folder or everything will be deleted.
- `kodi_style` tries to rename `filename` with [kodi style](https://kodi.wiki/view/Naming_video_files/TV_shows).
- `strm_props` is a list of properties written to the strm file.
  If `kodi_style` set to `true` the property `#KODIPROP:seekable=true|false` is added. If `strm_props` is not given `#KODIPROP:inputstream=inputstream.ffmpeg`, `"#KODIPROP:http-reconnect=true` are set too for `kody_style`.

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


### 2.2.2.5 `filter`
The filter is a string with a filter statement.
The filter can have UnaryExpression `NOT`, BinaryExpression `AND OR`, Regexp Comparison `(Group|Title|Name|Url) ~ "regexp"`
and Type Comparsison `Type = vod` or `Type = live` or `Type = series`.
Filter fields are `Group`, `Title`, `Name`, `Caption`, `Url`, `Input` and `Type`.
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
The mappings are defined in a file `mapping.yml`. The filename can be given as `-m` argument. (See Mappings section)

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
sources:
  - inputs:
      - enabled: true
        url: http://myserver.net/playlist.m3u
        persist: ./playlist_{}.m3u
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
          kodi_style: true
          cleanup: true
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

### 2.5.2.8 `watch`
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

## 2. `mapping.yml`
Has the root item `mappings` which has the following top level entries:
- `templates` _optional_
- `mapping` _mandatory_

Instead of using a single `mapping.yml` file, you can use multiple mapping files
when you set `mapping_path` in `config.yml` to a directory.

### 2.1 `templates`
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
- `mapper` _mandatory_
- `counter` _optional_

### 2.3.1 `id`
Is referenced in the `config.yml`, should be a unique identifier

### 2.3.2 `match_as_ascii`
If you have non ascii characters in you playlist and want to
write regexp without considering chars like `é` and use `e` instead, set this option to `true`.
[unidecode](https://crates.io/crates/unidecode) is used to convert the text.

### 2.3.3 `mapper`
Has the following top level entries:
- `filter`
- `script`

#### 2.3.3.1 `filter`
The filter  is a string with a statement (@see filter statements).
It is optional and allows you to filter the content.

#### 2.3.3.2 `script`
Script has a custom DSL syntax. 

This Domain-Specific Language (DSL) supports simple scripting operations including variable assignment, 
string operations, pattern matching, conditional mapping, and structured data access. 
It is whitespace-tolerant and uses familiar programming concepts with a custom syntax.

**Basic elements:**
- Identifiers: `Variable Names` composed of ASCII alphanumeric characters and underscores.
- FieldNames: `Playlist Field Names` starting with `@` following compose of ASCII alphanumeric characters and underscores.
- Strings / Text: Enclosed in double quotes. "example string" 
- Null value `null`
- Regex Matching:   `@FieldName ~ "Regex"` like in filter statements. You can match a `FieldName` or a existing `variable`.
- Access a field in a regex match result:  with `result.capture`. For example, if you have multiple captures you can access them by their name, or their index beginning at 1.
- Builtin String functions: 
  - concat(a, b, ...)
  - uppercase(a)
  - lowercase(a)
  - capitalize(a)
  - trim(a)
  - print(a)
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
### 2.3.4 counter

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
    tags:
      - name: quality
        captures:
          - quality
        concat: '|'
        prefix: ' [ '
        suffix: ' ]'
    mapping:
      - id: France
        match_as_ascii: true
        mapper:
          - filter: 'Name ~ "^TF.*"'
            pattern: '!source!'
            attributes:
              url: http://my.iptv.proxy.com/<query> 
          - pattern: 'Name ~ "^TF1$"'
            attributes:
              name: TF1
              id: TF1.fr
              chno: '1'
              logo: https://upload.wikimedia.org/wikipedia/commons/thumb/3/3c/TF1_logo_2013.svg/320px-TF1_logo_2013.svg.png
            suffix:
              title: '<tag:quality>'
              group: '|FR|TNT'
            assignments:
              title: name
          - pattern: 'Name ~ "^TF1!delimiter!!quality!*Series[_ ]*Films$"'
            attributes:
              name: TF1 Series Films
              id: TF1SeriesFilms.fr
              chno: '20'
              logo: https://upload.wikimedia.org/wikipedia/commons/thumb/3/3c/TF1_logo_2013.svg/320px-TF1_logo_2013.svg.png,
            suffix:
              group: '|FR|TNT'
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
`epg_timeshift` is _optional_. It is only applied when source has `epg_url` configured. `epg_timeshift: [-+]hh:mm`, example  
`-2:30`(-2h30m), `1:45` (1h45m), `+0:15` (15m), `2` (2h), `:30` (30m), `:3` (3m), `2:` (3h)
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

Now you can do `nginx`  configuration like
```config
   location /tuliprox {
      rewrite ^/tuliprox/(.*)$ /$1 break;
      proxy_set_header X-Real-IP $remote_addr;
      proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
      proxy_set_header X-NginX-Proxy true;
      proxy_pass http://192.169.1.9:8901/;
      proxy_ssl_session_reuse off;
      proxy_set_header Host $http_host;
      proxy_redirect off;
   }
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


## 4. Logging
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
env  RUSTFLAGS="--remap-path-prefix $HOME=~" cross build --release --target x86_64-unknown-linux-musl
```

#### Manual compile - install prerequisites
```shell
rustup update
sudo apt-get install pkg-config musl-tools libssl-dev
rustup target add x86_64-unknown-linux-musl
```
#### Build statically linked binary
```shell
cargo build --target x86_64-unknown-linux-musl --release
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
cargo build --release --target x86_64-pc-windows-gnu
```

### Cross compile for raspberry pi 2/3/4

Ease way to compile is a docker toolchain `cross`

```shell
rust install cross
env  RUSTFLAGS="--remap-path-prefix $HOME=~" cross build --release --target armv7-unknown-linux-musleabihf
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
sources:
- inputs:
    - type: xtream
      url: 'http://fantastic.provider.xyz:8080'
      epg_url: 'http://fantastic.provider.xyz:8080/xmltv.php?username=tvjunkie&password=junkie.secret'
      username: tvjunkie
      password: junkie.secret
      options: {xtream_info_cache: true}
  targets:
    - name: all_channels
      output:
        - type: xtream
      filter: "!ALL_CHAN!"
      options: {ignore_logo: false, skip_live_direct_source: true, skip_video_direct_source: true}
      sort:
        match_as_ascii: true
        groups:
          order: asc
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
sources:
- inputs:
    - type: xtream
      ...
  targets:
    - name: all_channels
      output:
        - type: xtream
      filter: "!ALL_CHAN!"
      ...
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

sources:
- inputs:
    - type: xtream
      ...
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
from "FR: TV5Monde"
```
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