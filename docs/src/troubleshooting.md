# 🛠️ Troubleshooting & Resilience

Running a reverse proxy for IPTV involves navigating the quirks of various video players and the strict limitations of upstream providers. This page
documents common "real-world" behaviors that can lead to stream interruptions or provider bans, and how to mitigate them using Tuliprox's resilience
features.

The solutions below will help you fine-tune your configuration for a seamless experience.

---

## 1. The VLC "Seek" Problem (Grace Periods)

**The Problem:** A user watches a VOD movie via reverse proxy. They press "Fast forward 10 seconds" in VLC. VLC calculates the new byte offset, kills
the TCP connection, and instantly fires a new HTTP GET request (with a `Range` header) to your Tuliprox server.

Tuliprox opens a new connection to the upstream provider. However, since the old connection takes milliseconds to officially close at the provider
side, the provider sees **two** active streams. If you only paid for 1 connection, the provider throws a 509 Bandwidth Exceeded error or bans your IP!

**The Solution in `config.yml`:**

```yaml
reverse_proxy:
  stream:
    grace_period_hold_stream: true
    grace_period_millis: 2000
    grace_period_timeout_secs: 5
```

**What happens now?**
Tuliprox detects the bottleneck and grants a temporary "grace" state:

* **Hold State:** Because `grace_period_hold_stream: true` is set, Tuliprox keeps the client connection "warm" but
  waits before requesting new bytes from the provider.
* **The Handover:** It waits for `grace_period_millis` (2000ms) to give the provider's server time to register the old connection as closed.
* **Resolution:**
  * **Success:** If the old "ghost" connection dies within the window ➔ The new stream flows instantly.
  * **Timeout:** If the old connection persists beyond `grace_period_timeout_secs` (5s) ➔ Grace is revoked, and the client receives the
    `user_connections_exhausted.ts` video.

---

## 2. Zombie Connections After a Client IP Change (Wi-Fi → Mobile Data)

**The Problem:** A user starts a stream on their mobile phone while connected to home Wi-Fi. They then walk outside, and the phone automatically
switches to 4G/5G. The phone's public IP address changes, and the old stream stalls.

A new connection attempt from the new IP address hits the `max_connections` limit and starts playing the `user_connections_exhausted.ts` video —
even though the user is the only viewer. The original stream slot appears "stuck" and is not released, even though no data is flowing to the client
anymore.

**Root Cause — Why the Slot Gets Stuck:**

When a phone switches networks without explicitly closing its TCP connection (no FIN/RST packet), the server has no way to know the client is gone.
The kernel keeps trying to deliver buffered stream data with exponential retransmission back-off.

TCP Keepalive probes — while configured in Tuliprox — only fire on **idle** connections (no data sent for a period of time). A live IPTV stream is
never idle from the server's perspective, so keepalive probes never trigger. The result is that the kernel can keep retransmitting for **2–15 minutes**
before it finally gives up and closes the connection.

**The Solution — `TCP_USER_TIMEOUT` (Linux only):**

Tuliprox automatically sets the `TCP_USER_TIMEOUT` socket option on every accepted connection on Linux. This option instructs the kernel to forcibly
close a connection once transmitted data has been unacknowledged for a defined period — regardless of whether the connection was idle or actively
sending.

With the default value of **30 seconds**, a dead streaming connection is detected and its slot is freed within 30 seconds of the client disappearing.

```code
t = 0 s    Phone switches from Wi-Fi to 4G (old TCP connection dies silently)
t = 0 s    Server continues sending stream data; kernel buffers it (no ACKs)
t ≤ 30 s   TCP_USER_TIMEOUT exceeded → kernel forcibly closes the connection
t ≤ 30 s   Tuliprox releases the user connection slot
t ≤ 30 s   New connection from the 4G IP can now acquire the slot normally
```

> **Platform Note:** `TCP_USER_TIMEOUT` is a Linux-specific feature (available since kernel 2.6.37). On **Windows** and **macOS**, this option is not
> available. Those platforms handle dead connections through TCP Keepalive probes or platform-specific socket options, which are less effective for
> active streaming connections. In practice this is not an issue since Tuliprox is designed to run on Linux servers and Docker containers.

---

## 3. Database Migration Failures (`LZ4 decompression failed: 0 is not a valid match offset`)

**The Problem:**
During startup migration of legacy V2 databases (e.g., `series.db`, `live.db`), the server or container fails with:

```text
ERROR tuliprox::repository::bplustree::migration] Startup migration failed: Failed to migrate B+Tree /app/data/input_dir/series.db: LZ4 decompression failed: 0 is not a valid match offset
```

**Root Cause:**
In the legacy B+Tree V2 storage format, entries were updated in-place without page-level checksums or write-ahead logging.
If an unexpected power loss, kill signal, or torn disk write occurred during an update, a single value's LZ4 block could
contain zeroed bytes (offset `0` is invalid according to the LZ4 format specification). In older versions, a single unreadable
record caused the entire database migration to abort.

**The Solution & Resilience:**

1. **Automatic Error Recovery:** Tuliprox's migration engine now automatically catches decompression and decoding errors on
   damaged individual records, logs a warning with the path of the skipped record, and continues migrating all healthy records
   into a valid, crash-safe V3 database.
2. **Manual Inspection & Repair:** You can inspect the health of any database or manually trigger the migration using the
   included helper script:

```bash
# Inspect the database (shows healthy vs. corrupt records without changing anything):
./bin/migrate_db.sh --inspect /path/to/series.db

# Migrate and salvage all healthy records (creates a timestamped backup automatically):
./bin/migrate_db.sh /path/to/series.db
```

If Tuliprox runs in Docker:

```bash
docker exec -it tuliprox tuliprox --migrate-db /app/data/input_dir/series.db
```

See [Database Migration & Inspection](./operations-debugging.md#6-database-migration--inspection-migrate_dbsh--cli) for full details.

---

## 4. File Descriptor Exhaustion (`No file descriptors available (os error 24)`)

**The Problem:**

The server logs show cascade errors failing to open files or establish network connections:

```text
[WARN tuliprox_session::qos_aggregation_manager] QoS aggregation run failed: I/O error: No file descriptors available (os error 24)
[INFO tuliprox_processing::processor::playlist] 🌷 Update process started.
[WARN tuliprox_processing::input_cache] Failed to read input status file /app/data/input_PrimeTrial/status.json: No file descriptors available (os error 24)
[WARN tuliprox_iptv::xtream] Failed to login xtream account PrimeTrial repository Network error: can't download input PrimeTrial => Request error: error sending request for url (http://***/player_api.php?...)
[ERROR tuliprox_processing::input_cache] Failed to write input status file /app/data/input_PrimeTrial/status.json: No file descriptors available (os error 24)
```

**Root Cause:**

The operating system error `os error 24` is **`EMFILE`** (*"Too many open files"*). On Unix and Linux systems,
all I/O abstractions consume file descriptors (FDs):

* **Client TCP Sockets:** Every media player connected to a live stream or VOD session (MPEG-TS, HLS chunk streaming).
* **Upstream TCP Sockets:** Every active proxy connection to an IPTV provider.
* **HTTP Client Connection Pools:** Reqwest and Hyper keep-alive connections held open for API requests and playlist downloads.
* **B+Tree Databases & Sidecars:** Every database (`xtream_*.db`, `m3u_*.db`, `epg_*.db`, `target_id_mapping.db`,
  `qos_snapshot.db`), along with WAL files and `.sidecar` lock handles.
* **Async Runtime:** Tokio worker threads, event notification file descriptors (`epoll`, `eventfd`, `timerfd`).

By default, Docker daemons and Linux user sessions often configure a low limit of only **1024** file descriptors
per container/process (`ulimit -n`). In a streaming proxy where background jobs (such as QoS aggregation or scheduled
playlist updates) run concurrently with active media streams, 1024 descriptors can be exhausted quickly. Once exhausted,
any operation requiring a new descriptor (such as opening a file or creating a socket for an HTTP request) fails immediately.

**The Solution:**

1. **Configure `ulimits` in `docker-compose.yml` (Recommended):**
   Increase the maximum open file limit for the Tuliprox container by defining `ulimits.nofile`:

   ```yaml
   services:
     tuliprox:
       image: ghcr.io/euzu/tuliprox:latest
       container_name: tuliprox
       restart: unless-stopped
       ulimits:
         nofile:
           soft: 65535
           hard: 65535
       # ... remaining configuration
   ```

   Apply the updated configuration by recreating the container:

   ```bash
   docker compose up -d
   ```

2. **Host / Systemd Service Limit (Native Deployment):**
   If running Tuliprox natively outside Docker via `systemd`, set `LimitNOFILE` in your unit file (e.g. `/etc/systemd/system/tuliprox.service`):

   ```ini
   [Service]
   LimitNOFILE=65535
   ```

   Then reload systemd and restart the service:

   ```bash
   sudo systemctl daemon-reload
   sudo systemctl restart tuliprox
   ```

3. **Diagnosing Open File Descriptors:**
   To check how many descriptors are currently open and identify what is using them:

   ```bash
   # Find the process ID
   PID=$(pgrep tuliprox)

   # Count active open descriptors
   ls -1 /proc/$PID/fd | wc -l

   # List open files and sockets
   ls -l /proc/$PID/fd
   ```

---

## 5. Server Stops Responding While the Container Keeps Running

**The Problem:** After running normally for a while (often hours), Tuliprox stops processing and logging. The container
still shows `running` with `ExitCode: 0` and low CPU/memory; there is no panic, no shutdown message and no final log
entry. Only a manual restart recovers it.

**Root Cause:** A wedged async runtime. Every Tokio worker is parked on a futex and the I/O driver sits in `epoll_wait`,
which the OS cannot distinguish from an idle process. This is consistent with a task holding a shared lock while waiting
forever on a network operation that has no timeout (for example an upstream that accepts the connection but never sends a
response).

**How to confirm:** Enable the runtime liveness watchdog (off by default):

```yaml
services:
  tuliprox:
    environment:
      - TULIPROX_WATCHDOG=1   # observe only
      # - TULIPROX_WATCHDOG=2 # observe and restart on a confirmed stall
```

When the runtime stops making progress, the watchdog logs a `Runtime liveness stall` line with runtime metrics and a
per-thread `/proc/self/task` inventory, and `GET /healthcheck` reports `runtime.status: stalled`.

In mode `1` the watchdog only reports. In mode `2` it exits the process (code `75`) once the stall has persisted past
the grace period (`TULIPROX_WATCHDOG_RESTART_GRACE_MS`, default 30 s), so a configured `restart: unless-stopped`
brings the container back automatically. Use mode `1` while you are still investigating a cause, and mode `2` once a
restart is the acceptable recovery.

**How to find the exact task and lock:** use the `tokio-console` diagnostic build (see
[Runtime Liveness Watchdog](./operations-debugging.md#8-runtime-liveness-watchdog)) or run the experimental image.

**Immediate mitigation:** restart the container. The watchdog log is what tells you the stall happened on its own and
captures what every thread was doing at that moment.

---
