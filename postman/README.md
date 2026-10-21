# Shared HLS HTTP regression fixture

The repository-owned `postman/mocks/hls-window` mock supports `targetDuration`,
`segmentDurationMs`, `failures` and `failAlways` in its loopback-only `/configure`
control. A finite failure returns `502` with `Retry-After: 1`, then normal live
metadata resumes. Permanent failure keeps returning the same retryable error.
The real process fixture supplies complete TS bodies and local successor gates.
No external origin or credentials are needed.

```sh
~/.local/bin/postman simulation run postman/simulations/hls-window.sim.yaml \
  --port 4926 --no-history --output ndjson > /tmp/hls-window.ndjson
```

Run both ignored process tests serially through the required test wrapper:

```sh
TULIPROX_HLS_WINDOW_SIMULATION_ORIGIN=http://127.0.0.1:4926/origin \
TULIPROX_HLS_WINDOW_EVIDENCE_DIR="$PWD/dev/test/reserve-csv" \
  ./bin/test.sh -p tuliprox --profile dev-slim --test hls_manifest_window \
  -- --ignored --test-threads=1
```

The retained-window test gates successor bytes and checks the prior lease body
remains HTTP 200 until publication can advance. The reserve test first recovers
from one manifest error with positive reserve, then exercises a permanent outage
without player polls after the outage is confirmed. It verifies autonomous CSV
publication at zero reserve, the exact previous last live URI, readable live/CSV
bytes, immutable replay and `ENDLIST` in all three startup modes. Its TS duration
and EXTINF agree. Keep raw SUT/simulation logs local; the URL-free client NDJSON
is the shareable protocol.
