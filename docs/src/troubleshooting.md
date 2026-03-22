# 🛠️ Troubleshooting & Resilience

Running a reverse proxy for IPTV involves navigating the quirks of various video players and the strict limitations of upstream providers. This page documents common "real-world" behaviors that can lead to stream interruptions or provider bans, and how to mitigate them using Tuliprox's resilience features.

The solutions below will help you fine-tune your configuration for a seamless experience.

---

## 1. The VLC "Seek" Problem (Grace Periods)


**The Problem:** A user watches a VOD movie via reverse proxy. They press "Fast forward 10 seconds" in VLC. VLC calculates the new byte offset, kills the TCP connection, and instantly fires a new HTTP GET request (with a `Range` header) to your Tuliprox server.


Tuliprox opens a new connection to the upstream provider. However, since the old connection takes milliseconds to officially close at the provider side, the provider sees **two** active streams. If you only paid for 1 connection, the provider throws a 509 Bandwidth Exceeded error or bans your IP!


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
* **Hold State:** Because `hold_stream: true` is set, Tuliprox keeps the client connection "warm" but waits before requesting new bytes from the provider.
* **The Handover:** It waits for `grace_period_millis` (2000ms) to give the provider's server time to register the old connection as closed.
* **Resolution:** 
    * **Success:** If the old "ghost" connection dies within the window ➔ The new stream flows instantly.
    * **Timeout:** If the old connection persists beyond `grace_period_timeout_secs` (5s) ➔ Grace is revoked, and the client receives the `user_connections_exhausted.ts` video.
--- 