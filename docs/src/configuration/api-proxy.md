# 🛡️ Pillar 3: `api-proxy.yml` (Server, Users & RBAC)

The `api-proxy.yml` file acts as your Edge Gateway. It defines the public-facing URLs (virtual servers) that Tuliprox
advertises
in its playlists, manages your end-users, and dictates which playlists (`targets`) those users can access, along with
their
specific permissions, proxy modes, and priorities.

## Top-level entries

```yaml
auth_error_status: 403
use_user_db: false
server:
user:
```

### Global Parameters

| Parameter           | Type | Required | Default | Technical Impact & Background                                                                                                                                                                                                                                                                                                                                                                                                   |
|:--------------------|:-----|:--------:|:--------|:--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `auth_error_status` | Int  |    No    | `403`   | The HTTP status code Tuliprox returns when a player sends invalid credentials or tokens. (Only applies to [Xtream/M3U API Endpoints](#api-endpoints-for-clients-players), stream paths, and resource paths, NOT the Web UI / REST API).                                                                                                                                                                                         |
| `use_user_db`       | Bool |    No    | `false` | If set to `true`, Tuliprox migrates all users from this YAML file into a highly performant SQLite database (`api_user.db`). **From then on, Tuliprox ignores the users in the YAML file!** You **must** subsequently manage users entirely via the Web UI Dashboard. Switching the option to `false` or `true` automatically migrates users back to the corresponding file (`false` → `api-proxy.yml`, `true` → `api_user.db`). |
| `server`            | List |   Yes    | `[]`    | See [Server Definitions](#1-server-definitions-server) for how to define servers.                                                                                                                                                                                                                                                                                                                                               |
| `user`              | List |    No    | `[]`    | See [User Definitions](#2-user-definitions-user) for how to define users & permissions.                                                                                                                                                                                                                                                                                                                                         |

### Subsections (Object Keys)

| Block    | Description                                           | Link                                        |
|:---------|:------------------------------------------------------|:--------------------------------------------|
| `server` | Virtual server endpoints exposed to clients.          | [See section](#1-server-definitions-server) |
| `user`   | User credentials, proxy modes, and access management. | [See section](#2-user-definitions-user)     |

---

## 1. Server Definitions (`server`)

Here you define multiple named "virtual servers". A server object describes the host structure that Tuliprox injects
into the
stream URLs when generating M3U or Xtream playlists.

Typically, you define at least two: one for internal LAN access and one for external access via a reverse proxy (like
Traefik or Nginx).
One server **must** strictly be named `default`.

This example matches a setup with an internal default server and an external server using the path `/tuliprox`.

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
    host: my-external-domain.com
    port: 443
    timezone: Europe/Paris
    message: Welcome to tuliprox
    path: tuliprox
```

### Server Parameters

| Parameter  | Type   | Required | Default               | Technical Impact & Background                                                                                                                                                                             |
|:-----------|:-------|:--------:|:----------------------|:----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `name`     | String |   Yes    |                       | Internal reference ID (e.g., `external`).                                                                                                                                                                 |
| `protocol` | String |   Yes    |                       | `http` or `https`. **Note:** Tuliprox does not perform TLS termination itself; it does not support native HTTPS traffic. You need an SSL terminator/proxy like Traefik or Nginx in front of it for HTTPS. |
| `host`     | String |   Yes    |                       | The domain or IP address transmitted to the client.                                                                                                                                                       |
| `port`     | String |    No    | `None`                | The port your external proxy listens on (usually `443` for HTTPS).                                                                                                                                        |
| `timezone` | String |    No    | `UTC`                 | Defines the timezone sent to the client via the Xtream API.                                                                                                                                               |
| `message`  | String |    No    | `Welcome to tuliprox` | The welcome message displayed in IPTV players supporting the Xtream API.                                                                                                                                  |
| `path`     | String |    No    | `None`                | **Background:** If you host Tuliprox not on a subdomain (`tv.dom.com`) but in a subdirectory (`dom.com/iptv`), specify `iptv` here. Tuliprox will automatically prefix all output URLs with this path.    |

---

## 2. User Definitions (`user`)

Users in Tuliprox are strictly bound to a specific `target` (defined in `source.yml`). A single target can have multiple
user
credentials attached to it.

> **Important:** When you define credentials for a `target`, ensure that this target has an output format of `xtream` or
`m3u` configured in your `source.yml`.

```yaml
user:
  - target: my_livingroom_target
    credentials:
      - username: john
        password: mysecurepassword
        token: auth_token_abc
        proxy: reverse
        server: default
        max_connections: 1
        epg_timeshift: Europe/Paris
        status: Active
        ui_enabled: true
        priority: -10

      # Compact inline syntax is also supported:
      - { username: x3452, password: p, token: 4342sd, proxy: redirect, server: external, epg_timeshift: -2:30 }
```

**Crucial Concept:** By default, Tuliprox acts purely as a stream mapper. If you want Tuliprox to actively evaluate the
`status`,
enforce the `exp_date`, or kick users who breach their `max_connections`, you **must** set `user_access_control: true`
globally
in your `config.yml`. Without it, these fields are purely cosmetic!

### Credential Parameters (Deep-Dive)

| Parameter               | Type     | Required | Default    | Technical Impact & Background                                                                                                                                                                                                                                                      |
|:------------------------|:---------|:--------:|:-----------|:-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `username` / `password` | String   |   Yes    |            | The standard Xtream-Codes / M3U credentials used for authentication. Must be unique.                                                                                                                                                                                               |
| `token`                 | String   |    No    | `None`     | Allows login via a URL parameter (`?token=XYZ`) instead of user/pass. Must be globally unique if set.                                                                                                                                                                              |
| `proxy`                 | Enum     |    No    | `redirect` | Defines the proxy mode for this user (see [proxy modes](#proxy-modes-proxy) below).                                                                                                                                                                                                |
| `server`                | String   |    No    | `default`  | Which server block (host/port) is rendered into the playlist for this user.                                                                                                                                                                                                        |
| `epg_timeshift`         | String   |    No    | `None`     | Shifts EPG times for users in different time zones. Formats supported: `[-+]hh:mm` or `TimeZone`. Examples: `-2:30` (minus 2h30m), `1:45` (1h45m), `+0:15` (15m), `2` (2h), `:30` (30m), `:3` (3m), `Europe/Paris`, `America/New_York`. Only applies when `epg_url` is configured. |
| `max_connections`       | Int      |    No    | `0`        | Hard limit of concurrent streams for *this* user. `0` = Unlimited. **Requires** `user_access_control: true` in `config.yml` to be enforced.                                                                                                                                        |
| `status`                | Enum     |    No    | `Active`   | Possible values: `Active`, `Trial`, `Expired`, `Banned`, `Disabled`, `Pending`. **Requires** `user_access_control: true` in `config.yml` to block non-active streaming.                                                                                                            |
| `exp_date`              | UnixTs   |    No    | `None`     | Locks the user out after this Unix timestamp. **Requires** `user_access_control: true` in `config.yml` to be enforced.                                                                                                                                                             |
| `ui_enabled`            | Bool     |    No    | `true`     | Allows this specific user to log into the Web UI to manage their own favorites/bouquets.                                                                                                                                                                                           |
| `priority`              | Int (i8) |    No    | `0`        | Stream preemption priority. Priority range: `-128` to `127`, where `-128` has the highest priority. Negative numbers are explicitly allowed for top-tier access. (see [user priority](#user-priorities-priority) below)                                                            |

---

### Proxy Modes (`proxy`)

This is the most crucial field governing traffic flow for the user. When to use which?

* **`redirect`** *(Default)*: Tuliprox responds to the client with an HTTP 302 Redirect,
  pointing directly to the upstream provider's URL (or rotating through DNS failover IPs).
  * *When to use:* To save massive bandwidth on your server (Tuliprox only acts as a matchmaker).
  * *Tradeoff:* **No** connection limits, buffering, bandwidth throttling, or custom fallback videos are applied!
* **`reverse`**: Tuliprox downloads the video stream from the provider onto your server and pipes it to the client.
  * *When to use:* This is required for connection limits, fallback videos, caching, bandwidth throttling, and shared
    streams to function.
* **Partial Syntax**: You can mix and match! `reverse[live]` forces Live-TV through Tuliprox
  (allowing shared streams) but redirects VODs (saving bandwidth).
  `reverse[live,vod]` routes everything except Series episodes through Tuliprox.

### User Priorities (`priority`)

**Architecture Detail:** Tuliprox utilizes a *Unix Nice-Scale* (value range `-128` to `127`). A **lower** number means a
**higher**
priority. The default is `0`.

**Practical Use Cases:**

1. **Admin Override:** Set your personal user to `-10`. Set your friends to `0`.
   If provider limits are exhausted, you will forcefully kick a friend to watch TV.
2. **Family vs Guests:** Set your TV to `0`, kids to `10`, and guests to `20`. Guests get kicked first.

**The Preemption Scenario:**
Your upstream provider allows 2 concurrent connections. User A (Priority `0`) is watching TV.
User B (Priority `0`) is watching a VOD. The provider limit is exhausted.
Now you, the Admin (User C with Priority `-10`), want to watch.

1. Because your priority is *higher* (lower number), Tuliprox scans for active connections with the *lowest* priority on
   that specific provider.
2. Since A and B are tied (both `0`), Tuliprox targets the stream that has been running the longest (tie-breaker based
   on stream age).
3. Tuliprox forcefully terminates User A's provider connection, serves User A a fallback video
   (`low_priority_preempted.ts`), and instantly claims the freed provider slot for you.

*(Note: Only connections with exactly one active listener are eligible for eviction — shared connections with multiple
listeners are not interrupted.
Internal FFprobe metadata probe tasks run by default at the absolute lowest priority (`127`) and are immediately
preempted/killed if any real user needs the slot.)*

---

&nbsp;

## Additional Information

### API Endpoints for Clients (Players)

After configuring the api-proxy, you can use these endpoints in players like TiviMate, IPTV Smarters, or VLC.

*(Replace `<host>:<port>` with your Server definition).*

**Xtream Codes API:**

```text
http://<host>:<port>/player_api.php?username=<USER>&password=<PASS>
http://<host>:<port>/player_api.php?token=<TOKEN>
```

**M3U Playlist URL:**

```text
http://<host>:<port>/get.php?username=<USER>&password=<PASS>
http://<host>:<port>/get.php?token=<TOKEN>
```

**XMLTV EPG URL:**

```text
http://<host>:<port>/xmltv.php?username=<USER>&password=<PASS>
```

Tuliprox also offers **REST-friendly aliases** in case restrictive firewalls or ISP blocks
target `.php` extensions. For the sake of simplicity, you can also use `token` in place of
the `username` and `password` combination on these endpoints:

* `/xtream` instead of `player_api.php`
* `/m3u` instead of `get.php`
* `/epg` instead of `xmltv.php`

---

### Reverse Proxy in front of Tuliprox

If another proxy sits in front of Tuliprox (like Nginx or Traefik), you must ensure it forwards the
correct headers. Without these, Tuliprox's IP-based rate limiting, Geo-IP validation,
and connection kicking will see the proxy's internal IP instead of the actual client.

#### Required Headers

Ensure your proxy forwards the following:

* `X-Real-IP`
* `X-Forwarded-For`

#### Example: Nginx

When using Nginx, ensure that buffering is disabled to prevent stream stuttering
or high memory usage on the proxy.

```nginx
location /tuliprox {
  rewrite ^/tuliprox/(.*)$ /$1 break;
  proxy_set_header X-Real-IP $remote_addr;
  proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
  proxy_set_header Host $http_host;
  proxy_pass http://<host>:<port>/;

  # ABSOLUTELY CRITICAL FOR VIDEO STREAMS:
  proxy_redirect off;
  proxy_buffering off;
  proxy_request_buffering off;
  proxy_cache off;
  tcp_nopush on;
  tcp_nodelay on;
}
```

#### Example: Traefik (Docker Labels)

You can use Traefik as a reverse proxy in front of your Tuliprox instance. This is especially useful for handling TLS
termination (HTTPS).

When using subdirectories (paths) instead of subdomains, you must ensure that Traefik
strips the prefix before forwarding the request. This allows Tuliprox to handle the
request as if it were at the root level (`/`).

⚠️ **Connection Note:** Ensure the path defined in your `config.yml` (under `server: external`)
matches the path you use in Traefik. If you set `path: tuliprox` in Tuliprox,
your clients must connect via `my-external-domain.com/tuliprox/...`.

**Configuration Strategy:**
In this example, we use two paths:

* `/tuliprox`: Used for the **Web-UI** and as the base for the `external` server definition.
* `/tv`: An optional **shorter alias** for API/Playlist access to keep M3U URLs compact.

```yaml
labels:
  - "traefik.enable=true"
  # Internal Tuliprox port
  - "traefik.http.services.tuliprox.loadbalancer.server.port=8901"

  # ----- HTTP (Port 80) -----
  - "traefik.http.routers.tuliprox.entrypoints=web"
  - "traefik.http.routers.tuliprox.rule=Host(`my-external-domain.com`) && (PathPrefix(`/tv`) || PathPrefix(`/tuliprox`))"

  # ----- HTTPS (Port 443) -----
  - "traefik.http.routers.tuliprox-secure.entrypoints=websecure"
  - "traefik.http.routers.tuliprox-secure.rule=Host(`my-external-domain.com`) && (PathPrefix(`/tv`) || PathPrefix(`/tuliprox`))"
  - "traefik.http.routers.tuliprox-secure.tls=true"
  - "traefik.http.routers.tuliprox-secure.tls.certresolver=myresolver"

  # ----- Middlewares -----
  # Strip prefixes so Tuliprox receives requests at root ("/")
  - "traefik.http.middlewares.tuliprox-strip.stripprefix.prefixes=/tv,/tuliprox"

  # Apply stripping and forward real client IPs
  - "traefik.http.routers.tuliprox.middlewares=tuliprox-strip@docker,forward-real-ip@file"
  - "traefik.http.routers.tuliprox-secure.middlewares=tuliprox-strip@docker,forward-real-ip@file"
```

> **Pro-Tip:** Ensure your Traefik static configuration (`entryPoints`) includes
> your Docker network range in `trustedIPs`. Otherwise, Traefik might strip the
> forwarded headers before they reach Tuliprox.
