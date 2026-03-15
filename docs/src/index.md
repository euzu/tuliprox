# tuliprox

`tuliprox` is an IPTV proxy and playlist processor for people who need more control than a raw provider playlist can offer.

It sits between provider inputs and client applications and gives you a place to:

- clean up channels
- normalize outputs
- protect provider accounts
- manage user access
- handle HLS, catchup and shared streaming behavior
- mix IPTV with a local media library

## What makes it different

Tuliprox is not only a playlist rewriter.
It is also a runtime stream broker with provider-aware logic.

That matters when you need things like:

- connection limits per user
- provider account reuse for HLS or catchup
- priority-based stream preemption
- shared live streams
- custom fallback videos when a stream cannot be delivered

## Documentation map

- [Getting Started](getting-started.md): first run, main commands, file layout
- [Core Features](features.md): what tuliprox can do at a high level
- [Configuration](configuration/overview.md): config file layout and field reference
- [Sources And Targets](configuration/sources-and-targets.md): inputs, aliases, outputs and processing
- [API Proxy](configuration/api-proxy.md): users, servers, reverse/redirect and access URLs
- [Streaming And Proxy](streaming-and-proxy.md): runtime behavior, HLS/catchup affinity and reverse-proxy options
- [Mapping And Templates](mapping-and-templates.md): DSL, mapping files, counters and grouping examples
- [Deployment](deployment.md): build and ship backend, frontend and docs
- [Examples And Recipes](examples-and-recipes.md): practical setup and filtering examples

## Source format

The documentation source is plain Markdown in `docs/src`.
Static HTML is generated from it with `mdBook` and can be shipped together with the Web UI under `/static/docs/`.
