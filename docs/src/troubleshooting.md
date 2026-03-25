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
