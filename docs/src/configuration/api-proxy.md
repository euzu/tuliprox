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
| `epg_request_timeshift` | String   |    No    | `None`     | Shifts EPG times for users in different time zones specifically to adjust catchup requests                                                                                                                                                                                         |
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

### EPG Timeshift Configuration Guide

#### What is an EPG Timeshift?

**EPG (Electronic Program Guide)** timeshift allows you to adjust TV program times to match your local time zone. This
is especially useful when:

* You live in a different time zone than your IPTV provider
* You want to view programs as if they were aired at a different time
* Your EPG data is in one time zone, but you need it displayed in another

---

#### The Two EPG Timeshift Fields

When configuring users in Tuliprox, you'll find two separate fields for EPG timeshift:

| Field                   | Purpose                                                         | When to Use                                                        |
|-------------------------|-----------------------------------------------------------------|--------------------------------------------------------------------|
| `epg_timeshift`         | Shifts EPG times for **XMLTV/EPG requests** (regular EPG files) | Use when you want ALL EPG times globally shifted for this user     |
| `epg_request_timeshift` | Adjusts client-provided time ranges for **XTream Catchup API**  | Use when you need to shift catchup time requests (start/end times) |

---

#### When to Use Which Field?

##### Use `epg_timeshift` for

✅ **XMLTV EPG Requests**

* When clients request EPG via `/xmltv.php` or `/epg` endpoints
* When serving EPG files to applications
* When you want ALL program times adjusted to your time zone

**Example:** You're in Paris (UTC+2) and your provider is in UTC. All EPG times should be 2 hours earlier.

---

##### Use `epg_request_timeshift` for

✅ **XTream Catchup API**

* When clients request catchup via `/timeshift` or `/streaming/timeshift.php`
* When clients provide their own time ranges (`start`, `end`, `duration`)
* When you need to shift those client-provided times to match your needs

**Example:** A client requests catchup from 14:00-16:00. You want these times shifted to match your local time zone.

---

##### Key Difference

| Feature               | `epg_timeshift`             | `epg_request_timeshift`                     |
|-----------------------|-----------------------------|---------------------------------------------|
| Affects               | All EPG program times       | Client-provided catchup time ranges only    |
| Used in               | XMLTV/EPG endpoints         | XTream Catchup API                          |
| Global or Per-Request | Global (affects entire EPG) | Per-request (adjusts client-provided times) |

---

#### Supported Time Formats

Both fields support the same time formats:

| Format      | Example                            | Meaning                           |
|-------------|------------------------------------|-----------------------------------|
| `[-+]hh:mm` | `-2:30`, `+1:00`                   | Fixed offset in hours and minutes |
| `hh:mm`     | `2:00`, `:30`                      | Positive offset (implies +)       |
| `TimeZone`  | `Europe/Paris`, `America/New_York` | IANA timezone name                |

##### Common Examples

| Format        | Value              | Description             |
|---------------|--------------------|-------------------------|
| Fixed offset  | `-2:00`            | Minus 2 hours           |
|               | `+1:30`            | Plus 1 hour 30 minutes  |
|               | `2:00`             | Plus 2 hours (positive) |
|               | `:30`              | Plus 30 minutes         |
|               | `:3`               | Plus 3 minutes          |
| Timezone name | `Europe/Paris`     | Paris time zone         |
|               | `America/New_York` | New York time zone      |
|               | `Asia/Tokyo`       | Tokyo time zone         |

---

#### Configuration Examples

##### Example 1: User in Paris, No Additional Catchup Offset

**Scenario:** You're in Paris (UTC+2) and your IPTV provider uses UTC. You want all EPG programs shown in Paris time.
Catchup times should not be adjusted.

```yaml
# config.yml
api_proxy:
  users:
    - username: paris_user
      password: ***
      epg_timeshift: Europe/Paris          # ✅ All EPG in Paris time zone
      epg_request_timeshift: None        # ✅ No adjustment for catchup
```

---

##### Example 2: User in New York, -1h30 for Everything

**Scenario:** You're in New York (UTC-5 in winter, UTC-4 in summer). Your EPG is in UTC. You want to shift everything by
-1h30.

```yaml
# config.yml
api_proxy:
  users:
    - username: ny_user
      password: ***
      epg_timeshift: America/New_York    # ✅ EPG in New York time zone
      epg_request_timeshift: -1:30     # ✅ Catchup also shifted by -1h30
```

---

##### Example 3: User in Berlin, -2h Only for Catchup

**Scenario:** You're in Berlin (UTC+1). Your EPG is already in Berlin time zone. However, when clients request catchup,
you want to adjust their times by -2 hours.

```yaml
# config.yml
api_proxy:
  users:
    - username: berlin_user
      password: ***
      epg_timeshift: Europe/Berlin        # ✅ EPG in Berlin time zone
      epg_request_timeshift: -2:00      # ✅ Catchup shifted by -2h
```

---

##### Example 4: User in London, +3h Global, No Catchup Shift

**Scenario:** You're in London (UTC+0 or UTC+1 depending on DST). Your EPG is 3 hours behind. You want to catch up on
all EPG times.

```yaml
# config.yml
api_proxy:
  users:
    - username: london_user
      password: ***
      epg_timeshift: +3:00                # ✅ All EPG times +3 hours
      epg_request_timeshift: None          # ✅ Catchup uses client times as-is
```

---

##### Example 5: User in Sydney, Timezone for EPG, Catchup Unchanged

**Scenario:** You're in Sydney (UTC+10/UTC+11). You want EPG in Sydney time zone. Catchup should use exact times as
clients request.

```yaml
# config.yml
api_proxy:
  users:
    - username: sydney_user
      password: ***
      epg_timeshift: Australia/Sydney    # ✅ EPG in Sydney time zone
      epg_request_timeshift: None        # ✅ No catchup adjustment
```

---

#### Real-World Scenarios

##### Scenario 1: Watching EPG in Different Time Zone

**Problem:** Your provider's EPG shows programs in UTC time, but you're in Tokyo (UTC+9).

**Solution:** Use `epg_timeshift: Australia/Sydney` or `epg_timeshift: +9:00`.

**Result:** All EPG programs appear in Tokyo local time, making it easy to find what's on TV right now.

---

##### Scenario 2: Requesting Catchup for Specific Times

**Problem:** A client requests catchup from 14:00-16:00 (2pm-4pm), but you want to adjust this for your time zone.

**Setup:** `epg_request_timeshift: -1:00`

**When client requests:** `start=14:00&end=16:00`

**Tuliprox adjusts to:** `start=13:00&end=15:00` (shifted by -1 hour)

**Result:** Provider receives catchup request for 1pm-3pm (your local time).

---

##### Scenario 3: Same User Needs Different Shifts for Different Features

**Problem:** You want EPG in your local time zone, but catchup requests should use a different offset (or no offset at
all).

**Solution:** Configure different values for each field.

**Result:** XMLTV requests use one shift, XTream catchup uses another shift. Full flexibility!

---

#### Common Questions (FAQ)

##### Q: Can I leave both fields empty?

**A:** Yes! Both `epg_timeshift` and `epg_request_timeshift` are optional (`None`). When empty:

* EPG/EPG times remain unchanged
* Catchup times are exactly as requested by client
* No time shifting is applied

---

##### Q: What's the difference between `+2:00` and `Europe/Paris`?

**A:** Both are valid formats, but they work differently:

* `+2:00`: A fixed +2 hour offset. This is always +2 hours, regardless of Daylight Saving Time (DST).
* `Europe/Paris`: Uses the actual Paris time zone, which automatically handles DST (UTC+1 in winter, UTC+2 in summer).

**Recommendation:** Use timezone names (`Europe/Paris`) for locations with DST. Use fixed offsets (`+2:00`) when you
want a constant shift.

---

##### Q: Which field should I use if I don't know?

**A:** Start with `epg_timeshift`. This is the most common field and affects XMLTV/EPG requests, which are used by most
IPTV applications. Only configure `epg_request_timeshift` if you specifically need to adjust catchup time requests.

---

##### Q: Can I use both fields with different values?

**A:** Absolutely! This is the intended design. For example:

* `epg_timeshift: Europe/Berlin` (EPG in Berlin time)
* `epg_request_timeshift: -2:00` (Catchup shifted by -2 hours)

Each field affects only its specific use case, giving you complete control.

---

##### Q: How do negative timeshifts work?

**A:** Negative values shift times **backwards** in time.

**Example:** `epg_timeshift: -2:30`

If EPG shows a program at 20:00, it will appear at 17:30 to your client.

**Common use:** You're ahead of your provider's time zone and need to go back.

---

##### Q: What is the maximum/minimum timeshift I can set?

**A:** There is no hard limit in Tuliprox, but practical limits apply:

* **For fixed offsets:** Typically -12 to +14 hours (covering most global time differences)
* **For timezone names:** Any IANA timezone name (e.g., `Pacific/Honolulu` to `Etc/GMT+14`)

---

##### Q: Will timeshift affect recording/catchup?

**A:** Only `epg_request_timeshift` affects catchup requests. `epg_timeshift` only affects EPG/EPG program times and
does not modify actual stream or recording times.

---

##### Q: Do I need to restart Tuliprox after changing these settings?

**A:** No! Configuration changes are detected automatically and applied without restart. The new timeshift values will
be used for the next request.

---

#### Quick Reference

| I Want To...                         | Use This Field                | Format Example                                           |
|--------------------------------------|-------------------------------|----------------------------------------------------------|
| Shift all EPG times globally         | `epg_timeshift`               | `Europe/Paris` or `-2:00`                                |
| Adjust catchup time requests         | `epg_request_timeshift`       | `America/New_York` or `+1:30`                            |
| No time shifting                     | Leave both empty              | `None` or omit field                                     |
| Handle DST automatically             | Use timezone name             | `Europe/london`                                          |
| Constant offset regardless of DST    | Use fixed offset              | `+1:00`                                                  |
| EPG in local time, catchup different | Set different values for both | `epg_timeshift: Berlin` / `epg_request_timeshift: -1:00` |

---

#### Getting Help

If you're unsure which values to use:

1. **Check your local time zone** and compare with your IPTV provider's EPG
2. **Calculate the difference** in hours (e.g., "I'm 2 hours ahead of EPG")
3. **Set `epg_timeshift` to that value** if you want EPG adjusted
4. **Set `epg_request_timeshift`** only if you specifically need to adjust catchup requests
5. **Test with your IPTV app** to verify times appear correctly

For timezone names, see: [IANA Time Zone Database](https://en.wikipedia.org/wiki/List_of_tz_database_time_zones)

---

**Note:** These settings only affect EPG (Electronic Program Guide) times. They do not change when streams are actually
aired - that's controlled by your IPTV provider.

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
