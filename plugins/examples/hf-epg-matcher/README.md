# HF EPG Matcher

An example tuliprox plugin: it matches playlist channels that the built-in EPG matcher could not resolve to a guide
channel, by asking a HuggingFace sentence-similarity model. It shows a `match-channel` hook, outbound HTTP with a
bearer secret, a decision cache in the plugin KV, and a settings-only UI: `ui.config_section` points at
`config-schema.json` and there is no UI code at all.

## What it does

The host calls `epg.match-channel` only for live channels the built-in matcher left without a guide id, and offers each a
shortlist of guide channels (`candidates`, best first) that kept programmes. For every request the plugin:

1. normalizes the channel name (lowercase, alphanumerics, single spaces) and looks up
   `match:<model_id>:<fnv1a hash of the normalized name>` in the KV;
2. on a hit made over the same shortlist, answers from it without any HTTP;
3. on a miss, calls `POST {api_base_url}/models/{model_id}` with
   `{"inputs": {"source_sentence": "<channel name>", "sentences": ["<candidate name>", ...]}}` and
   `Authorization: Bearer <hf_token>`, in batches of at most `batch_size` sentences, and scores each candidate by its
   best name;
4. caches the best candidate and its score, and answers it when the score is at least `threshold` (ties go to the
   earlier candidate). The threshold is applied when a cached entry is read, so changing it needs no new model call.

The answers are index-aligned with the requests. The host still applies its own safety rules to an answer (country and
digit checks, the id must be on the shortlist), so the plugin can choose among candidates, never override them.

Fail open: when the API is down or answers an error or something unexpected, the uncached requests of that call are
answered `None` and nothing is cached for them; the first failure ends the API use of the call so a dead API costs one
timeout (4 s), not one per channel. An unavailable KV only costs repeated model calls. The output is a pure function of
the requests, the settings, the KV and the API answers: no randomness, no clock, no hash-map order.

## Settings (`config-schema.json`)

| Setting | Default | |
| --- | --- | --- |
| `model_id` | `sentence-transformers/all-MiniLM-L6-v2` | model path appended to `/models/` |
| `hf_token` | none | write-only secret, sent as the bearer token; `${env:HF_TOKEN}` works |
| `threshold` | `0.6` | 0..1, minimum best score to accept a candidate |
| `batch_size` | `16` | 1..64, most candidate names per HTTP request |
| `api_base_url` | `http://127.0.0.1:18971` | the mock; use `https://api-inference.huggingface.co` for the real API |

Capabilities: `network` (the model API) and `storage` (the decision cache).

## Run it

Build and package (needs the `wasm32-wasip2` target):

```sh
bin/package-plugin.sh plugins/examples/hf-epg-matcher     # -> target/plugin-examples/hf-epg-matcher.tlpx
```

The mock of the HF API (stdlib python3; a deterministic character-trigram similarity, a bearer check, `GET /_stats`
for the call count, `--down` to simulate an outage):

```sh
python3 -I plugins/examples/hf-epg-matcher/mock/hf_api.py --port 18971
```

`demo/` has a playlist, an XMLTV guide and the config fragments built around the case the plugin can actually fire
on (`BBC Too` against the guide's `BBC Two`, with a `BBC Two` playlist channel already keeping `bbc.two`);
`demo/CHECKS.md` has the exact steps and the expected results.

## Tests

`src/tests.rs` runs on the SDK mock host (native target): cache hit skips HTTP, threshold, ties, index alignment,
batching, API down / error status / malformed answer, unavailable KV, settings and the bearer token.

```sh
cargo test --manifest-path plugins/examples/hf-epg-matcher/Cargo.toml
```
