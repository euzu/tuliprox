# Configuration Overview

Tuliprox is driven by a small set of files rather than one giant document.

## Main layers

- `config.yml`: server, runtime, reverse proxy, scheduling, metadata, Web UI
- `source.yml`: inputs, providers, aliases, targets
- `mapping.yml`: optional mapping logic
- `template.yml`: reusable expressions and templates

## Practical split

Use `config.yml` for:

- how the application runs
- where it stores data
- how it serves users and streams

Use `source.yml` for:

- what data comes in
- how providers are grouped
- what outputs are exposed

Use mappings/templates for:

- content shaping
- repeated logic
- reusable naming or filtering expressions

## Reading order for newcomers

1. `config.yml`
2. `source.yml`
3. targets
4. reverse proxy settings
5. mapping/templates

## More detail

- [Config Reference](main-config.md)
- [Sources And Targets](sources-and-targets.md)
- [API Proxy](api-proxy.md)
