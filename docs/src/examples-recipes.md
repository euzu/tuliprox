# 🧪 Examples, Recipes & Ecosystem Stacks

This chapter provides concrete copy & paste solutions for common scenarios spanning the different pillars of the Tuliprox architecture, concluding
with the ultimate Docker deployment stack.

## 1. Quickstart: Minimal Provider Setup

To use Tuliprox, you need to configure at least three core files:

1. `config.yml`: Defines the core server settings.
2. `source.yml`: Defines the upstream provider and your output targets.
3. `api-proxy.yml`: Defines your local/external server addresses and the users who can access the targets.

### Scenario: Minimal Xtream Setup

You have an Xtream provider (`http://fantastic.provider.xyz:8080`) and want to simply "pass through" the streams while utilizing the Web UI.

**1. `config.yml` (Core Engine):**

```yaml
api:
  host: 0.0.0.0
  port: 8901
  web_root: ./web
storage_dir: ./data
update_on_boot: true
```

*This configuration starts Tuliprox on port 8901. Downloaded playlists are stored inside the `data` folder.*
*Setting `update_on_boot: true` is helpful during initial setup to immediately pull provider data.*

**2. `source.yml` (Inputs & Targets):**

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
      - name: clean_list
        output:
          - type: xtream
        filter: "!ALL_CHAN!" # Lets everything through
```

*Here we define the input source based on the provider's information and create a 1:1 mapped target (`clean_list`).*

**3. `api-proxy.yml` (Servers & Users):**

```yaml
server:
  - name: default
    protocol: http
    host: 192.168.1.41
    port: 8901
    timezone: Europe/Berlin
    message: Welcome to tuliprox
  - name: external
    protocol: https
    host: tv.my-domain.com
    port: 443
    timezone: Europe/Berlin
    path: iptv

user:
  - target: clean_list
    credentials:
      - username: me
        password: mypass
        proxy: redirect  # Tuliprox only redirects the player; it does not proxy the video stream
        server: default
```

*We define two server endpoints (internal LAN and external HTTPS). We then bind a user to the `clean_list` target.*
*Using `proxy: redirect` saves bandwidth on your server during initial testing.*

**Resulting Client Endpoints:**
You can now enter the following credentials into your IPTV player (e.g., TiviMate, IPTV Smarters):

* **Portal URL:** `http://192.168.1.41:8901`
* **Username:** `me`
* **Password:** `mypass`

Start `tuliprox`,  fire up your IPTV-Application, enter credentials and watch.

### Scenario: Minimal M3U Setup

If your provider uses standard M3U instead of the Xtream Codes API, simply adjust the input type in `source.yml`.
Tuliprox automatically extracts credentials from the URL if present.

**`source.yml` (M3U Variation):**

```yaml
inputs:
  - type: m3u
    name: my_m3u_provider
    url: "http://fantastic.provider.xyz:8080/get.php?username=tvjunkie&password=junkie.secret&type=m3u_plus&output=ts"
    cache_duration: 1d
```

---

## 2. Advanced Filtering (Exclusion Logic & Templates)

A 1:1 pass-through is rarely what you want. Upstream provider lists are often cluttered with countries or VOD sections you don't need.

Tuliprox uses a custom DSL (Domain Specific Language) for filtering. You need a basic understanding of Regular Expressions (Regex).
A good site for learning and testing is[regex101.com](https://regex101.com/) (ensure you select the **Rust** flavor).

### The Basics

DSL filters are applied in `source.yml` at the `target` level. The tilde `~` operator executes a Regex match.
Evaluatable fields include `Group`, `Title`, `Name`, `Url`, and `Type`.

* **Simple Inclusion:**
  Include all categories: `Group ~ ".*"`
* **Case-Insensitive Inclusion:**
  Include all categories whose name contains "shopping": `Group ~ "(?i).*Shopping.*"`
* **Reversing Logic (Exclusion):**
  I don't want any shopping categories: `NOT (Group ~ "(?i).*Shopping.*")`
* **Specific Channel Exclusion:**
  Filtering a specific channel, even if it appears in multiple groups:
  `NOT (Title ~ "FR: TV5Monde" AND Group ~ "FR: TF1")`

### Complex Matrix (Bracket Logic)

Allow everything from Germany (DE) and France (FR), except Series and Commercials. Always allow Australia (AU).

```text
((Group ~ "^(DE|FR).*") AND NOT (Group ~ "(?i).*SERIES.*" OR Group ~ "(?i).*COMMERCIAL.*")) OR (Group ~ "^AU.*")
```

### Refactoring with Templates (The DRY Principle)

As you can see, filter strings can quickly become massive and unmaintainable.
This is where **Templates**[**Templates**](./configuration/template.md) come into play.
You can disassemble a monolithic filter into smaller, readable parts and combine them.

```yaml
templates:
  - name: NO_SHOPPING
    value: 'NOT (Group ~ "(?i).*Shopping.*" OR Group ~ "(?i).*Einkaufen.*" OR Group ~ "(?i).*téléachat.*")'
  - name: GERMAN_CHANNELS
    value: 'Group ~ "^DE: .*"'
  - name: FRENCH_CHANNELS
    value: 'Group ~ "^FR: .*"'
  - name: NO_TV5MONDE_IN_TF1
    value: 'NOT (Group ~ "FR: TF1" AND Title ~ "FR: TV5Monde")'
  - name: EXCLUDED_CHANNELS
    value: '!NO_TV5MONDE_IN_TF1! AND !NO_SHOPPING!'

  # Final combination
  - name: MY_CHANNELS
    value: '!EXCLUDED_CHANNELS! AND (!GERMAN_CHANNELS! OR !FRENCH_CHANNELS!)'

inputs:
  - type: xtream
    name: my_provider
    url: http://fantastic.provider.xyz:8080

sources:
  - inputs:
      - my_provider
    targets:
      - name: curated_list
        output:
          - type: xtream
        filter: "!MY_CHANNELS!"
```

*The resulting playlist now cleanly contains all French and German channels, minus any shopping channels or specifically blacklisted streams.*

---

## 3. Generating Custom Fallback Videos (FFmpeg)

If a provider stream returns a 404 or the user hits their connection limit, it is much more elegant to play an info video ("Channel Offline") than letting
the player hang endlessly on a dropped HTTP connection. Tuliprox searches the `custom_stream_response_path` folder for exactly named `.ts` files
(MPEG-TS).

You can turn a simple image (`blank_screen.jpg`) into a clean, 10-second fallback stream with a silent audio track (crucial for player A/V
sync!) using FFmpeg:

```bash
ffmpeg -y -nostdin -loop 1 -framerate 30 -i blank_screen.jpg -f lavfi \
  -i anullsrc=channel_layout=stereo:sample_rate=48000 -t 10 -shortest -c:v libx264 \
  -pix_fmt yuv420p -preset veryfast -crf 23 -x264-params "keyint=30:min-keyint=30:scenecut=0:bframes=0:open_gop=0" \
  -c:a aac -b:a 128k -ac 2 -ar 48000 -mpegts_flags +resend_headers -muxdelay 0 -muxpreload 0 -f mpegts channel_unavailable.ts
```

Move `channel_unavailable.ts` into your resources folder. Tuliprox will loop this file from RAM until the user switches channels or the
`custom_stream_response_timeout_secs` limit is reached.

---

## 4. The Ultimate Docker Ecosystem Stack

Running Tuliprox bare-metal is fine, but running it behind a modern DevSecOps stack provides SSL termination, VPN routing to bypass ISP
blocks, and firewall protection against brute-force attacks.

In the repository under `docker/container-templates/`, you will find ready-to-use Docker Compose files.

### Components of the Stack

1. **Traefik (Reverse Proxy):** Handles incoming port 443 traffic, auto-renews Let's Encrypt certificates (via Cloudflare DNS-01 challenge), and
   applies strict Content-Security-Policy headers.
2. **Gluetun (VPN Egress):** A WireGuard/OpenVPN client container. It connects to Mullvad/ProtonVPN. We attach a **Socks5 Sidecar** to its network
   namespace. Tuliprox can then be configured to route its upstream provider requests through this Socks5 proxy, completely hiding your server's IP
   from the IPTV provider.
3. **CrowdSec (WAF & Bouncer):** Analyzes Traefik access logs in real-time. If someone tries to brute-force your Tuliprox Web UI or run
   path-traversal attacks, CrowdSec instructs the Traefik Bouncer plugin to drop their IP at the edge.

### How to wire it up

**1. Create the Docker Networks:**

```bash
docker network create proxy-net
docker network create crowdsec-net
```

**2. Configure Gluetun & Socks5:**
In `container-templates/gluetun/gluetun-01/.env.wg-01`, add your Wireguard details.
In `container-templates/gluetun/.env.socks5-proxy`, set user/pass for the proxy.
Start it: `docker-compose up -d`. It exposes port 1388 internally.

**3. Configure Tuliprox to use the VPN:**
In your `config.yml`, point the global proxy setting to the Socks5 container:

```yaml
proxy:
  url: socks5://socks5-01:1388
  username: "<socks5-proxy-user>"
  password: "<socks5-proxy-password>"
```

*Note: Ensure Tuliprox and the Socks5 container share the `proxy-net` network.*

**4. Protect Tuliprox with Traefik Labels:**
In your Tuliprox `docker-compose.yml`, add the Traefik labels to route traffic securely:

```yaml
    labels:
      - "traefik.enable=true"
      - "traefik.http.routers.tuliprox-secure.entrypoints=websecure"
      - "traefik.http.routers.tuliprox-secure.rule=Host(`tv.yourdomain.com`)"
      - "traefik.http.routers.tuliprox-secure.tls=true"
      - "traefik.http.routers.tuliprox-secure.tls.certresolver=cloudflare"
      # Attach CrowdSec Bouncer and Security Headers
      - "traefik.http.routers.tuliprox-secure.middlewares=cs-bouncer-traefik-plugin@file,default-security-headers@file"
      - "traefik.http.services.tuliprox.loadbalancer.server.port=8901"
```

This architecture ensures your IPTV provider only sees the VPN IP, your clients only see your secure `tv.yourdomain.com` domain, and malicious
bots are blocked instantly by CrowdSec before they even reach the Rust backend.
