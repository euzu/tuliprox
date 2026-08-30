# 🔐 Secrets & Environment Variables

Every deployment needs credentials: provider logins, output-user passwords, webhook tokens, web server secrets.
Tuliprox keeps the repository free of real credentials while making it easy to inject them at runtime.

> **Host-agnostic.** These instructions deliberately do not assume any particular host, container platform, or
> cloud provider. Secrets are read from the process environment and from files we generate on the machine itself,
> so the same workflow works whether you run a container, a service manager, a process supervisor, a small server,
> or a hosted secret store of your choice. The mechanism to supply an environment variable is always the same:
> it must be present in the environment of the `tuliprox` process at startup.

## What counts as a secret

| File              | Field                                                                                                                                                                        | Example env var                                                                  |
|:------------------|:-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:---------------------------------------------------------------------------------|
| `source.yml`      | input `url`, `username`, `password`                                                                                                                                          | `CLOUDTV_1_URL`, `CLOUDTV_1_USER`, `CLOUDTV_1_PASS`                              |
| `source.yml`      | input `epg.sources[].url`                                                                                                                                                    | `CLOUDTV_1_EPG_URL`                                                              |
| `source.yml`      | input `panel_api.api_key` (account management)                                                                                                                               | `PROVIDER_PANEL_API_KEY`                                                         |
| `api-proxy.yml`   | output user `password` / `token` for every published user                                                                                                                    | `XTR_USER_LOCAL_PASS`, `XTR_USER_LOCAL_TOKEN`                                    |
| `config.yml`      | `web_auth.secret` (JWT signing, 64-hex)                                                                                                                                      | `TULIPROX_WEB_SECRET`                                                            |
| `config.yml`      | messaging webhooks/tokens: Telegram bot token, Discord URL, Pushover token/key, ntfy/Gotify tokens, Slack URL, generic REST URL + `signing_secret` + `Authorization` headers | `TULIPROX_DISCORD_WEBHOOK`, `TULIPROX_TELEGRAM_TOKEN`, `TULIPROX_SIGNING_SECRET` |
| `config.yml`      | `metadata_update.tmdb.api_key`                                                                                                                                               | `TULIPROX_TMDB_API_KEY`                                                          |
| `config.yml`      | `proxy_security.rewrite_secret`                                                                                                                                              | `TULIPROX_PROXY_REWRITE_SECRET`                                                  |
| `config/user.txt` | Web UI Argon2 password hashes (see below — not env-injectable)                                                                                                               | *file only*                                                                      |

Anything a provider, a player, a notification bot, or a browser authenticates with is a secret and must not be in git.

## `${env:VAR}` interpolation

Every config file that Tuliprox reads (`config.yml`, `source.yml`, `api-proxy.yml`, `mapping.yml`, `template.yml`)
supports environment-variable interpolation with the syntax:

```text
${env:VAR_NAME}
```

Variable names match `[a-zA-Z_][a-zA-Z0-9_]*`.

- **How it works:** the placeholder is resolved *before* the file is parsed as YAML, from the environment of the
  running process. If the variable is missing, Tuliprox logs
  `Could not resolve env var 'VAR_NAME'` and leaves the literal `${env:VAR_NAME}` in place so the problem is visible.
- **Quoting:** keep string values quoted, exactly like the value the variable will expand to:

  ```yaml
  inputs:
    - name: provider
      type: xtream
      url: "${env:CLOUDTV_URL}"
      username: "${env:CLOUDTV_USER}"
      password: "${env:CLOUDTV_PASS}"
  ```

  This stays valid YAML before and after substitution.
- **Use only for string fields.** Numbers (`exp_date`, ports, `token_ttl_mins`) and booleans (`enabled`) still belong
  directly in the file; injecting them through env vars is fragile.
- **Not supported in `user.txt`.** Web UI credential files are read directly and must be written on the machine
  (see below).
- **Also works in CLI paths:** any `--home` / `-c` / `-i` / `-a` argument can be `${env:...}` too.

### Supplying the variables

Because Tuliprox reads from the process environment, any mechanism that sets environment variables works:

- `export TULIPROX_WEB_SECRET=...` in the shell / init script that starts the process,
- `environment:` entries in a container or compose file,
- `Environment=` lines of a service unit,
- the secret store / environment settings of whatever runtime you picked.

> Tuliprox does **not** read a `.env` file automatically. If your runtime auto-loads `.env` files, that is fine — the
> process environment is what matters. For a quick manual check on a Linux shell:
> `VAR=value ./tuliprox -s` or `export VAR=value && ./tuliprox -s`.

## Web UI credentials (`user.txt`)

Tuliprox stores password *hashes*, never plain text. Each line is `username:argon2_hash[:group1,group2]`; without
groups the user defaults to `admin`.

1. Generate a hash with the interactive CLI prompt (it needs a real terminal and cannot read from stdin):

   ```bash
   tuliprox --genpwd
   ```

2. Write the line into `config/user.txt` (or the user file configured in `config.yml`):

   ```text
   myuser:$argon2id$v=19$m=19456,t=2,p=1$...
   ```

3. Restart the server (or reload) and verify the login.

`user.txt` is environment-injectable only in the sense that the *path* may come from a CLI/`${env:...}` path; the
hashes themselves are generated on the machine and are never committed. The `config/user.txt` shipped in this
repository contains sample hashes for the demo accounts `test` / `nobody` documented in `config/README.md` — replace
them before going live.

## JWT secret (`web_auth.secret`)

If `web_auth.secret` is omitted, Tuliprox generates one in-memory and **all active logins are invalidated on every
restart**. For production, pin a static 64-character hexadecimal string and keep it stable:

```bash
node -e "console.log(require('crypto').randomBytes(32).toString('hex'))"
```

Put the result in an environment variable and reference it from `config.yml`:

```yaml
web_auth:
  secret: "${env:TULIPROX_WEB_SECRET}"
```

Rotating it invalidates all sessions — do it deliberately, not on a whim.

## Pre-publish checklist

Run these before pushing anything to a public repository:

```bash
# Anything obviously credential-like in tracked configs
git grep -n -i -E "(password|secret|token|api_key)[[:space:]]*:" -- config/

# Explicit values that should have been placeholders or generated on-site
git grep -n -i -E "your_|TODO|changeme|example|\.secret|localsecret" -- config/

# Staged files (never a secret should be here)
git diff --cached --stat
git status --porcelain
```

Other good habits:

- Keep `logging.sanitize_sensitive_info: true` (default). It masks passwords, provider URLs and client IPs in logs
  so shared logs can't leak credentials.
- Keep `runtime_config_report_enabled: false` unless you need a startup dump; when enabled it masks sensitive values
  as `***` anyway.
- Never commit `data/`, `target/`, `downloads/`, `cache/`, `backup/`, `.env`, or any runtime directory — the
  repository `.gitignore` already covers the common ones.
- If a secret ever lands in history, rotate it (it is compromised history, not just a file) and rewrite history
  with `git filter-repo` rather than committing new secrets on top.

## Related

- [`config.yml` core configuration](config.md) — `web_auth`, messaging, `metadata_update.tmdb`
- [`source.yml` inputs & providers](source.md) — provider credentials, EPG, backup URLs
- [`api-proxy.yml` published users](api-proxy.md) — output user credentials and tokens
- [Getting Started](../getting-started.md) — where config files live and how the home directory is resolved
