# Examples And Recipes

This page collects the practical examples that previously lived in the monolithic README.

## Xtream provider quick setup

Provider data:

- URL: `http://fantastic.provider.xyz:8080`
- username: `tvjunkie`
- password: `junkie.secret`

Minimal `config.yml`:

```yaml
api:
  host: 0.0.0.0
  port: 8901
  web_root: ./web
storage_dir: ./data
update_on_boot: true
```

Minimal `source.yml`:

```yaml
templates:
  - name: ALL_CHAN
    value: 'Group ~ ".*"'
inputs:
  - type: xtream
    name: my_provider
    url: http://fantastic.provider.xyz:8080
    username: tvjunkie
    password: junkie.secret
sources:
  - inputs:
      - my_provider
    targets:
      - name: all_channels
        output:
          - type: xtream
        filter: "!ALL_CHAN!"
```

Minimal `api-proxy.yml`:

```yaml
server:
  - name: default
    protocol: http
    host: 192.168.1.41
    port: 8901
    timezone: Europe/Berlin
    message: Welcome to tuliprox
user:
  - target: all_channels
    credentials:
      - username: xt
        password: xt.secret
        proxy: redirect
        server: default
```

## Filtering examples

Include all categories:

```text
Group ~ ".*"
```

Only shopping:

```text
Group ~ "(?i).*Shopping.*"
```

Exclude shopping:

```text
NOT(Group ~ "(?i).*Shopping.*")
```

More complex example:

```text
(Group ~ "^FR.*" AND NOT(Group ~ "^FR.*SERIES.*" OR Group ~ "^DE.*EINKAUFEN.*")) OR (Group ~ "^AU.*")
```

## Template-based filter composition

```yaml
templates:
  - name: NO_SHOPPING
    value: 'NOT(Group ~ "(?i).*Shopping.*" OR Group ~ "(?i).*Einkaufen.*")'
  - name: GERMAN_CHANNELS
    value: 'Group ~ "^DE: .*"'
  - name: FRENCH_CHANNELS
    value: 'Group ~ "^FR: .*"'
  - name: MY_CHANNELS
    value: '!NO_SHOPPING! AND (!GERMAN_CHANNELS! OR !FRENCH_CHANNELS!)'
```

## Excluding a single channel

```text
NOT(Title ~ "FR: TV5Monde")
```

Or only inside one group:

```text
NOT(Group ~ "FR: TF1" AND Title ~ "FR: TV5Monde")
```

## VLC seek problem with `user_access_control`

Seeking can generate very fast reconnects and byte-range requests.
If stale provider connections have not yet disappeared, the user can briefly appear above `max_connections`.

Typical mitigation:

```yaml
reverse_proxy:
  stream:
    grace_period_millis: 2000
    grace_period_timeout_secs: 5
```

## Enable per-stream metrics in the Web UI

To show live bandwidth and transferred bytes in the streams table, enable stream metrics:

```yaml
reverse_proxy:
  stream:
    metrics_enabled: true
```

This is useful for operator troubleshooting and live monitoring of active reverse-proxied streams.

## Local library CLI examples

```bash
./tuliprox --scan-library
./tuliprox --force-library-rescan
./tuliprox --dbx /opt/tuliprox/data/all_channels/xtream/video.db
./tuliprox --dbm /opt/tuliprox/data/all_channels/m3u.db
./tuliprox --dbe /opt/tuliprox/data/all_channels/xtream/epg.db
```

## Custom fallback video generation

You can turn a still image into a `.ts` fallback video:

```bash
ffmpeg -y -nostdin -loop 1 -framerate 30 -i blank_screen.jpg -f lavfi \
  -i anullsrc=channel_layout=stereo:sample_rate=48000 -t 10 -shortest -c:v libx264 \
  -pix_fmt yuv420p -preset veryfast -crf 23 -x264-params "keyint=30:min-keyint=30:scenecut=0:bframes=0:open_gop=0" \
  -c:a aac -b:a 128k -ac 2 -ar 48000 -mpegts_flags +resend_headers -muxdelay 0 -muxpreload 0 -f mpegts blank_screen.ts
```
