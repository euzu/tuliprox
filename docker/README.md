# Docker

## Build docker image

Targets are:

- `scratch-final`
- `alpine-final`

Change into the root directory and run:

```shell
# Build for a specific architecture
docker build --rm -f docker/Dockerfile -t tuliprox --target scratch-final --build-arg RUST_TARGET=x86_64-unknown-linux-musl .
docker build --rm -f docker/Dockerfile -t tuliprox --target scratch-final --build-arg RUST_TARGET=aarch64-unknown-linux-musl .
docker build --rm -f docker/Dockerfile -t tuliprox --target scratch-final --build-arg RUST_TARGET=armv7-unknown-linux-musleabihf .
docker build --rm -f docker/Dockerfile -t tuliprox --target scratch-final --build-arg RUST_TARGET=x86_64-apple-darwin .
```

Both targets have the path prefix: `/app`

This will build the complete project and create a docker image.

To start the container, you can use the `docker-compose.yml`
But you need to change `image: ghcr.io/euzu/tuliprox:latest` to `image: tuliprox`

## Manual docker image

You want to build the binary and web folder manually and create a docker image.

To dockerize tuliprox, you need to compile a static build.
The static build can created with `bin\build_lin_static.sh`.
Description of static binary compiling is in the main `README.md`

Then you need to compile the frontend with `yarn build`

Change into the `docker` directory and copy all the needed files (look at the Dockerfile) into the current directory.

To create a docker image type:
`docker -f Dockerfile.manual build -t tuliprox  .`

To start the container, you can use the `docker-compose.yml`
But you need to change `image: ghcr.io/euzu/tuliprox:latest` to `image: tuliprox`

Set timezone in docker-compose.yml like

```dockerfile
    environment:
      - TZ=${TZ:-Europe/Paris}
```

## Docker Container Templates — Deployment Guide

This repository contains ready-to-use Docker Compose templates for a secure reverse proxy stack with VPN egress and CrowdSec protection. It includes
**Traefik**, **Gluetun** (WireGuard) with optional proxy sidecars, **CrowdSec** with Traefik integration, an **IPTV-org-epg** service, and an example
**Tuliprox** app wired for reverse proxying.

> **Software baseline:** Traefik v3.5, a current Rust toolchain, and a current Docker/Compose setup.

---

## Legend

| Template         | Folder                          | Purpose                                                                                                                        | Notable Ports (internal unless published)                          |
| ---------------- | ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------ |
| **Traefik**      | `container-templates/traefik/`  | Reverse proxy & TLS (ACME/DNS), dashboard, dynamic security middlewares, optional CrowdSec bouncer.                            | 80 `web`, 443 `websecure`                                          |
| **Gluetun**      | `container-templates/gluetun/`  | VPN egress via WireGuard; sidecars provide **SOCKS5**, **HTTP**, and **Shadowsocks** proxies bound to Gluetun’s network stack. | 1080/tcp (HTTP), 1388/tcp+udp (SOCKS5), 9388/tcp+udp (Shadowsocks) |
| **CrowdSec**     | `container-templates/crowdsec/` | LAPI + bouncers (Traefik & firewall) to protect services.                                                                      | LAPI on `127.0.0.1:8080` (host)                                    |
| **Tuliprox**     | `container-templates/tuliprox/` | Example application container with Traefik labels and `expose: 8901` for reverse proxying.                                     | 8901 (internal)                                                    |
| **IPTV-org-epg** | `container-templates/iptv-org/` | Generates and serves a local XMLTV guide from a manually curated channel list.                                                 | 3000 (internal)                                                    |

---

## Repository Layout (verified)

```shell
container-templates/
├─ traefik/
│  ├─ .env
│  ├─ cf-token
│  ├─ config/
│  │  ├─ traefik.yml
│  │  ├─ acme.json        (create & chmod 600 if missing)
│  │  └─ dynamic/
│  │     ├─ cdn-default-router.yml
│  │     ├─ crowdsec.yml
│  │     ├─ default-security-headers.yml
│  │     ├─ gluetun-proxys.yml
│  │     ├─ https-redirect.yml
│  │     ├─ real-ip-header.yml
│  │     ├─ strip-ip-header.yml
│  │     └─ tls-security.yml
│  └─ docker-compose.yml
├─ gluetun/
│  ├─ .env.http-proxy
│  ├─ .env.socks5-proxy
│  ├─ .env.ss-proxy
│  ├─ gluetun/
│  │  ├─ .env.wg
│  │  └─ docker-compose.yml
├─ crowdsec/
│  ├─ .env.cs-bouncer-firewall
│  ├─ .env.cs-bouncer-traefik
│  ├─ crowdsec/
│  │  └─ acquis.d/
│  │     ├─ appsec.yml
│  │     ├─ docker.yml
│  │     ├─ iptables.yml
│  │     ├─ mail.yml
│  │     ├─ sshd.yml
│  │     ├─ system.yml
│  │     └─ traefik.yml
│  ├─ firewall-bouncer/
│  │  └─ config/crowdsec-firewall-bouncer.yaml
│  └─ docker-compose.yml
├─ tuliprox/
│  └─ docker-compose.yml
└─ iptv-org/
   ├─ data/
   │  └─ channels.xml
   └─ docker-compose.yml
```

---

## Prerequisites

1. **Docker & Compose**
2. **Create external networks** used across templates:

   ```bash
   docker network create proxy-net
   docker network create crowdsec-net
   ```

3. **DNS provider token** (e.g., Cloudflare) if you use ACME DNS-01 with Traefik.

---

## 1) Traefik (reverse proxy)

**Folder:** `container-templates/traefik/`

### Files to review

- `.env`
  - Fix and fill:
    - `TZ=...`
    - `TRAEFIK_DASHBOARD_CREDENTIALS=<user:hashed-password>`
    - `CF_API_EMAIL=<cloudflare-email>`
    - `CF_DNS_API_TOKEN_FILE=/run/secrets/cloudflare`
- `cf-token`
  Put **only** your DNS API token string here, then:

  ```bash
  chmod 600 container-templates/traefik/cf-token
  ```

- `config/traefik.yml`
  - Set your ACME email.
  - Under `dnsChallenge.provider`, fix provider name if needed (**template shows `cloudclare`; use `cloudflare` or your actual provider**).
  - EntryPoints `web`/`websecure` are defined; dynamic files add middlewares.
- `config/acme.json`
  - Create if missing and lock down permissions:

    ```bash
    touch container-templates/traefik/config/acme.json
    chmod 600 container-templates/traefik/config/acme.json
    ```

### Start

```bash
docker compose -f container-templates/traefik/docker-compose.yml up -d
docker logs -f traefik
```

### Security middlewares already included

- `https-redirect.yml` (force HTTPS)
- `default-security-headers.yml` (strict defaults for CSP, HSTS, etc.)
- `tls-security.yml` (TLS options)

> Optional: `crowdsec.yml` enables the Traefik bouncer plugin if CrowdSec is running.

1. Add bouncer to your crowdsec engine

   ```shell
   user:~$ docker exec -it crowdsec cscli bouncer add traefik-bouncer
   API key for 'traefik-bouncer':

     2PbAzuGn9ynn6pYsqoqd98wMJYPA/CIynySN1Lva5H8

   Please keep this key since you will not be able to retrieve it!
   ```

2. Copy the provided key and paste it to your `crowdsec.yml` file

   ```yaml
   crowdsecLapiKey: 2PbAzuGn9ynn6pYsqoqd98wMJYPA/CIynySN1Lva5H8
   ```

---

## 2) Gluetun (VPN egress + proxy sidecars)

**Folder:** `container-templates/gluetun/`

Each instance (`gluetun`) has its own `.env.wg` with WireGuard settings. Sidecars (e.g., `socks5`) use
`network_mode: service:gluetun` to share Gluetun’s network. Otherwise connect the provided proxies within your tuliprox instance through traefik.

### Configure minimum one instance (example: gluetun)

1. Edit WireGuard values:

   ```bash
   nano container-templates/gluetun/.env.wg
   # WIREGUARD_PRIVATE_KEY=...
   # WIREGUARD_ADDRESSES=...
   # WIREGUARD_PUBLIC_KEY=...
   # WIREGUARD_ENDPOINT_IP=...
   # WIREGUARD_ENDPOINT_PORT=51820
   # WIREGUARD_MTU=1420
   # WIREGUARD_PERSISTENT_KEEPALIVE_INTERVAL=25s
   ```

2. (Optional) Enable proxy sidecars by editing:
   - `container-templates/gluetun/.env.socks5-proxy` (username/password & port 1388)
   - `container-templates/gluetun/.env.http-proxy` (HTTP proxy on 1080)
   - `container-templates/gluetun/.env.ss-proxy` (Shadowsocks on 9388)

3. Start:

   ```bash
   docker compose -f container-templates/gluetun/docker-compose.yml up -d
   docker logs -f gluetun
   ```

### Test from your Docker host

```bash
# Test SOCKS5(H) via traefik:
docker run --rm curlimages/curl:latest \
  -sS -x "socks5h://<USER>:<PASS>@proxy.tuliprox.io:<SOCKS5_PORT>" \
  https://ipinfo.io/ip

# Test HTTP(S) proxy via traefik:
docker run --rm curlimages/curl:latest \
  -sS -x "https://<USER>:<PASS>@proxy.tuliprox.io:<HTTPS_PROXY_PORT>" \
  https://ipinfo.io/ip
```

> Gluetun services are **exposed** to the Docker network by default, not to the host. Publish ports via Traefik or Compose if you really need external
> access (watch out for abuse/security) or want to use load balancing between your upstream proxy server. Be aware, however, that this can lead to a
> temporary block if the IP addresses change too quickly between two requests.

---

## 3) CrowdSec (LAPI + bouncers)

**Folder:** `container-templates/crowdsec/`

### Start

```bash
docker compose -f container-templates/crowdsec/docker-compose.yml up -d
docker logs -f crowdsec
```

### Configure

1. Register your crowdsec engine

   ```bash
   docker exec -it crowdsec cscli console enroll -e context cadsgfv0hadfgoisdfhuip
   ```

2. Add firewall bouncer

   ```shell
   user:~$ docker exec -it crowdsec cscli bouncer add firewall-bouncer
   API key for 'firewall-bouncer':

     2PbAzuGn9ynn6pYsqoqd98wMJYPA/CIynySN1Lva5H8

   Please keep this key since you will not be able to retrieve it!
   ```

3. Edit your env file

- `.env.cs-bouncer-firewall`
  - `CROWDSEC_API_KEY=2PbAzuGn9ynn6pYsqoqd98wMJYPA/CIynySN1Lva5H8`
  - `CROWDSEC_LAPI_URL=http://crowdsec:8080`

> The `docker-compose.yml` maps LAPI to `127.0.0.1:8080` on the host, and mounts logs (e.g., `/var/log/traefik/`) plus acquisition files under `crowdsec/acquis.d`.

Once healthy, the Traefik (from `traefik/config/dynamic/crowdsec.yml`) and firewall bouncer can enforce decisions.

---

## 4) Tuliprox (example app)

**Folder:** `container-templates/tuliprox/`

- `docker-compose.yml`:
  - Attaches to `proxy-net`.
  - `expose: 8901` for reverse proxying.
  - Traefik labels are included; adjust hostnames and middlewares as needed.

### Start

```bash
docker compose -f container-templates/tuliprox/docker-compose.yml up -d
docker logs -f tuliprox
```

### Example Traefik labels (adjust to your domain)

If you need to (re)apply labels, here’s a minimal pattern you can adapt:

```yaml
labels:
  - "traefik.enable=true"

  # HTTP
  - "traefik.http.routers.tuliprox.entrypoints=web"
  - "traefik.http.routers.tuliprox.rule=Host(`cdn.example.com`)"
  - "traefik.http.routers.tuliprox.middlewares=redirect-to-https@file"

  # HTTPS
  - "traefik.http.routers.tuliprox-secure.entrypoints=websecure"
  - "traefik.http.routers.tuliprox-secure.rule=Host(`cdn.example.com`)"
  - "traefik.http.routers.tuliprox-secure.tls=true"
  - "traefik.http.routers.tuliprox-secure.tls.certresolver=cloudflare"
  - "traefik.http.routers.tuliprox-secure.middlewares=default-security-headers@file"

  # Internal service port
  - "traefik.http.services.tuliprox.loadbalancer.server.port=8901"
```

> The dynamic file `cdn-default-router.yml` includes placeholder hosts (e.g., `cdn.tuliprox.io`). Change these to your domain or disable that router
> by renaming the file if not used.

---

## 5) IPTV-org-epg (EPG guide)

**Folder:** `container-templates/iptv-org/`

### Configure

1. Copy the desired `<channel>` entries from the IPTV-org `sites` files to
   `container-templates/iptv-org/data/channels.xml`.
2. Ensure every `xmltv_id` exactly matches the corresponding `@epg_channel_id` in Tuliprox. For example,
   `xmltv_id="BBCOne.uk@LondonHD"` matches `@epg_channel_id = "BBCOne.uk@LondonHD"`. If necessary, adjust `xmltv_id` in
   `channels.xml` or set the corresponding `@epg_channel_id` in `mapping.yml`.
3. Add the generated guide to the corresponding input in your `source.yml`:

   ```yaml
   epg:
     sources:
       - url: http://iptv-org-epg:3000/guide.xml
   ```

### Start

```bash
docker compose -f container-templates/iptv-org/docker-compose.yml up -d
docker logs -f iptv-org-epg
```

The generated `guide.xml` is written to `container-templates/iptv-org/data/`.

---

## Quick Start (end-to-end)

```bash
# 0) Networks (once)
docker network create proxy-net
docker network create crowdsec-net

# 1) Traefik
cd container-templates/traefik
touch config/acme.json && chmod 600 config/acme.json
echo "<your-dns-api-token>" > cf-token && chmod 600 cf-token
# Fix .env and config/traefik.yml (ACME email, dnsChallenge provider, etc.)
docker compose up -d

# 2) Gluetun
cd ../gluetun
# Fill .env.wg and configure the main-container proxies in the local .env.* proxy files
docker compose up -d

# 3) Tuliprox
cd ../../tuliprox
docker compose up -d

# 4) CrowdSec
cd ../crowdsec
# Fill .env.cs-bouncer-*
docker compose up -d

# 5) IPTV-org-epg
cd ../iptv-org
# Fill data/channels.xml and match every xmltv_id with @epg_channel_id
docker compose up -d
```

---

## Troubleshooting & Notes

- **External networks missing:**
  `network proxy-net/crowdsec-net not found` → create them first (see prerequisites).

- **ACME permissions:**
  `config/acme.json` **must** exist and be `chmod 600`, or certificate storage fails.

- **DNS provider typos:**
  In `config/traefik.yml`, fix `dnsChallenge.provider` (template shows `cloudclare`; use `cloudflare` or your provider).

- **Expose vs publish:**
  Many services are **exposed** to Docker networks only. To reach from the host/Internet, publish ports or front them with Traefik (recommended).

- **Security:**
  Be very careful exposing proxy endpoints (SOCKS5/HTTP/Shadowsocks). Require auth, rate-limit, and restrict scope as needed.

---

## Credits / Maintenance

These templates are designed to be composable and conservative by default (HTTPS redirect, strict headers, isolated networks). Review all placeholders
marked with `<...>` and domain names like `cdn.tuliprox.io` / `proxy.tuliprox.io` and adjust to your environment.
