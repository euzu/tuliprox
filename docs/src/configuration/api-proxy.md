# API Proxy

`api-proxy.yml` tells Tuliprox which public server URLs to advertise and which users may access which targets.

## Top-level entries

- `server`
- `user`
- `use_user_db`
- `auth_error_status`

## `server`

You can define multiple named servers.
Usually one is local and one is external.
One server should be named `default`.

```yaml
server:
  - name: default
    protocol: http
    host: 192.168.1.9
    port: "8901"
    timezone: Europe/Paris
    message: Welcome to tuliprox
  - name: external
    protocol: https
    host: tv.example.com
    port: "443"
    timezone: Europe/Paris
    message: Welcome to tuliprox
    path: tuliprox
```

Fields:

- `name`
- `protocol`
- `host`
- `port`
- `timezone`
- `message`
- `path`

If Tuliprox is behind another reverse proxy, `path` simplifies URL rewriting.

## `user`

Users are defined per target.
Each target can expose multiple credentials.

```yaml
user:
  - target: xc_m3u
    credentials:
      - username: demo
        password: secret1
        token: token1
        proxy: reverse
        server: default
        exp_date: 1672705545
        max_connections: 1
        status: Active
        priority: 0
```

Credential fields:

- `username`
- `password`
- `token`
- `proxy`
- `server`
- `epg_timeshift`
- `max_connections`
- `status`
- `exp_date`
- `priority`
- `user_ui_enabled`
- `user_access_control`

`username` and `password` are mandatory.
`token` is optional and must be unique if set.

## Proxy mode

`proxy` can be:

- `redirect`
- `reverse`
- `reverse[live]`
- `reverse[live,vod]`

Meaning:

- `redirect`: Tuliprox returns provider URLs
- `reverse`: Tuliprox proxies stream traffic itself
- subset syntax: reverse only for selected content types

## Priority

User priority is optional and defaults to `0`.

Rules:

- lower number = higher priority
- negative values are allowed
- higher-priority users can preempt lower-priority traffic when provider capacity is exhausted
- equal priority does not preempt a different running stream
- `max_connections` is independent of priority

Probe tasks use the same style of priority scale via `metadata_update.probe.user_priority`.

## Access control

When `user_access_control` is enabled in `config.yml`, Tuliprox also evaluates:

- `status`
- `exp_date`
- `max_connections`

for each user.

## `use_user_db`

If `use_user_db: true` is enabled, users are stored in the user database instead of the YAML file.
The Web UI should then be used to add, edit or remove users.

Tuliprox migrates users automatically when switching between YAML and DB mode.

## `auth_error_status`

HTTP status code returned when authentication fails (invalid or missing credentials).
Defaults to `403` (Forbidden).

```yaml
auth_error_status: 403
```

This setting applies to the streaming and playlist API endpoints
(`player_api.php`, `get.php`, `xmltv.php`, stream paths, resource paths).
It does **not** affect the Web UI / REST API (`/api/v1/…`) or HDHomeRun endpoints,
which always use their own fixed status codes.

## Access URLs

Common access patterns:

Xtream:

```text
http://host:port/player_api.php?username=USER&password=PASS
http://host:port/player_api.php?token=TOKEN
```

M3U:

```text
http://host:port/get.php?username=USER&password=PASS
http://host:port/get.php?token=TOKEN
```

XMLTV:

```text
http://host:port/xmltv.php?username=USER&password=PASS
```

REST-friendly aliases also work:

- `m3u` instead of `get.php`
- `xtream` instead of `player_api.php`
- `epg` instead of `xmltv.php`

## Reverse proxy in front of Tuliprox

If another proxy sits in front of Tuliprox, make sure it forwards:

- `X-Real-IP`
- `X-Forwarded-For`

Example nginx block:

```nginx
location /tuliprox {
  rewrite ^/tuliprox/(.*)$ /$1 break;
  proxy_set_header X-Real-IP $remote_addr;
  proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
  proxy_pass http://192.168.1.9:8901/;
  proxy_redirect off;
  proxy_buffering off;
  proxy_request_buffering off;
  proxy_cache off;
  tcp_nopush on;
  tcp_nodelay on;
}
```

Example Traefik labels:

```yaml
labels:
  - "traefik.enable=true"
  - "traefik.http.routers.tuliprox.rule=Host(`tv.my-domain.io`) && (PathPrefix(`/tv`) || PathPrefix(`/tuliprox`))"
  - "traefik.http.middlewares.tuliprox-strip.stripprefix.prefixes=/tv"
```
