# 🧪 Examples, Recipes & Ecosystem Stacks

This chapter provides concrete copy & paste solutions for common scenarios spanning the different pillars of the Tuliprox architecture, concluding with the ultimate Docker deployment stack.

## 1. Quickstart: Minimal Xtream Setup

You want to simply "pass through" your Xtream provider while utilizing the Web UI.

**Minimal `config.yml`:**
```yaml
api:
  host: 0.0.0.0
  port: 8901
  web_root: ./web
storage_dir: ./data
update_on_boot: true
```

**Minimal `source.yml`:**
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

**Minimal `api-proxy.yml`:**
```yaml
server:
  - name: default
    protocol: http
    host: 192.168.1.41
    port: 8901
    timezone: Europe/Berlin
    message: Welcome to tuliprox
user:
  - target: clean_list
    credentials:
      - username: me
        password: mypass
        proxy: redirect  # Tuliprox does not proxy the stream, it just redirects the player
        server: default
```

---

## 2. Advanced Filtering (Exclusion Logic)

DSL filters are applied in `source.yml` at the `target` level. Regular expressions require the tilde `~` operator.

**Allow if the word "Shopping" is included (Case-Insensitive):**
```text
Group ~ "(?i).*Shopping.*"
```

**Reverse the logic (No Shopping!):**
```text
NOT (Group ~ "(?i).*Shopping.*")
```

**Complex Matrix (Bracket Logic):**
Allow everything from DE and FR, except Series and Commercials. Always allow Australia.
```text
(Group ~ "^(DE|FR).*" AND NOT (Group ~ "(?i).*SERIES.*" OR Group ~ "(?i).*COMMERCIAL.*")) OR (Group ~ "^AU.*")
```

**Filtering a specific channel, even if it appears in multiple groups:**
```text
NOT (Title ~ "FR: TV5Monde" AND Group ~ "FR: TF1")
```

---

## 3. Generating Custom Fallback Videos (FFmpeg)

If a provider stream returns a 404 or the user hits their limit, it is much more elegant to play an info video ("Channel Offline") than letting the player hang endlessly on a dropped HTTP connection. Tuliprox searches the `custom_stream_response_path` folder for exactly named `.ts` files (MPEG-TS).

You can turn a simple image (`blank_screen.jpg`) into a clean, 10-second fallback stream with a silent audio track (crucial for player A/V sync!) using FFmpeg:

```bash
ffmpeg -y -nostdin -loop 1 -framerate 30 -i blank_screen.jpg -f lavfi \
  -i anullsrc=channel_layout=stereo:sample_rate=48000 -t 10 -shortest -c:v libx264 \
  -pix_fmt yuv420p -preset veryfast -crf 23 -x264-params "keyint=30:min-keyint=30:scenecut=0:bframes=0:open_gop=0" \
  -c:a aac -b:a 128k -ac 2 -ar 48000 -mpegts_flags +resend_headers -muxdelay 0 -muxpreload 0 -f mpegts channel_unavailable.ts
```

Move `channel_unavailable.ts` into your resources folder. Tuliprox will loop this file from RAM until the user switches channels or the `custom_stream_response_timeout_secs` limit is reached.

---

## 4. The Ultimate Docker Ecosystem Stack

Running Tuliprox bare-metal is fine, but running it behind a modern DevSecOps stack provides SSL termination, VPN routing to bypass ISP blocks, and firewall protection against brute-force attacks.

In the repository under `docker/container-templates/`, you will find ready-to-use Docker Compose files.

### Components of the Stack

1. **Traefik (Reverse Proxy):** Handles incoming port 443 traffic, auto-renews Let's Encrypt certificates (via Cloudflare DNS-01 challenge), and applies strict Content-Security-Policy headers.
2. **Gluetun (VPN Egress):** A WireGuard/OpenVPN client container. It connects to Mullvad/ProtonVPN. We attach a **Socks5 Sidecar** to its network namespace. Tuliprox can then be configured to route its upstream provider requests through this Socks5 proxy, completely hiding your server's IP from the IPTV provider.
3. **CrowdSec (WAF & Bouncer):** Analyzes Traefik access logs in real-time. If someone tries to brute-force your Tuliprox Web UI or run path-traversal attacks, CrowdSec instructs the Traefik Bouncer plugin to drop their IP at the edge.

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

This architecture ensures your IPTV provider only sees the VPN IP, your clients only see your secure `tv.yourdomain.com` domain, and malicious bots are blocked instantly by CrowdSec before they even reach the Rust backend.