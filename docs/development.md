# Development

Wise Wren is a Cargo workspace with two crates:

- **`wise-wren`** (the root, binary `wise-wren`): the server, built on
  [vole-kit](https://github.com/Ferdinand99/Vigilant-Vole/blob/main/docs/vole-kit.md), the shared kit in the
  Vigilant Vole repository (tag `vole-kit-v0.2.0`). The kit brings sign-in, setup, users and roles, API tokens,
  2FA, Security, General settings, the API reference, i18n in 26 languages, the database plumbing, request
  budgets, live updates, the notification providers and the design system. Wren adds servers and their agents,
  the notification channels page and the REST/MCP stubs.
- **`wren-agent`** (`agent/`): the small agent that runs on each Docker host. It has no kit dependency: tokio,
  reqwest (rustls), serde and toml. It reads `agent.toml`, reads Docker's `/version` and `/info`, and sends a
  heartbeat to Wren every 30 seconds. In this phase it changes nothing and takes no orders.

## Layout

```
Cargo.toml            # workspace + wise-wren; vole-kit by git tag
build.rs              # copies the kit's templates into templates/kit/ (git-ignored)
rust-toolchain.toml   # Rust 1.99
src/
  main.rs             # `wise-wren`: flags/env, logging, server
  lib.rs              # start() (config, database, state) and router()
  product.rs          # KitConfig: names, cookies, token prefixes, CSRF exemptions, nav, settings tabs,
                      # locales, static files
  config.rs           # flags and WW_* environment variables
  server.rs           # listener, the offline watcher, graceful shutdown
  db.rs               # migration set, agent tokens, the servers table, online/offline
  servers.rs          # Overview and Servers pages, add/token/revoke/delete, live updates (SSE)
  agent_api.rs        # POST /api/agent/v1/heartbeat, the agent's `docker run` lines
  channels.rs         # Settings → Notifications (from Keen Kestrel, without alert rules)
  api.rs              # API reference (DocsSpec), GET /api/v1/servers, MCP (no tools yet)
templates/            # Wren's pages (extend kit/layout.html)
static/               # favicon, icons, web manifest, wren.css (the wren's rose)
migrations/           # Wren's migrations (after the kit's core set)
locales/              # Wren's strings, 26 languages; glossaries in locales/glossary/
agent/src/            # config.rs (agent.toml), docker.rs (socket), heartbeat.rs, lib.rs (the loop)
tests/                # wren.rs (in-process app), agent.rs (the real agent against Wren), i18n.rs
Dockerfile            # targets `server` and `agent`
docker/               # the server's entrypoint (PUID/PGID), the agent's PHP setting
```

## Build, run, test

```bash
cargo run                      # http://localhost:3003, data in ./data
cargo run -- --help            # every flag and its WW_ variable
cargo run -p wren-agent -- --check --config agent.toml
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The first start opens `/setup`, which creates the admin account.

| Flag | Variable | Default | What |
|---|---|---|---|
| `--data-dir` | `WW_DATA_DIR` | `./data` | `wren.sqlite`, the session key, pre-migration backups |
| `--host` | `WW_HOST` | `0.0.0.0` | address to listen on |
| `--port` | `WW_PORT` | `3003` | port to listen on |
| `--public-url` | `WW_PUBLIC_URL` | (none) | the address agents and people use; stored as the General page's public URL |
| | `WW_TRUST_PROXY` | off | honour `X-Forwarded-*` from a reverse proxy (read by vole-kit) |
| | `WW_SETUP_TOKEN` | (none) | `/setup` only opens with `?token=<value>` (read by vole-kit) |

To work against a local checkout of the kit, create `.cargo/config.toml` (git-ignored):

```toml
[patch."https://github.com/Ferdinand99/Vigilant-Vole"]
vole-kit = { path = "../Vigilant-Vole/kit" }
vole-kit-build = { path = "../Vigilant-Vole/kit/kit-build" }
```

Images:

```bash
docker build --target server -t wise-wren:dev .
docker build --target agent  -t wise-wren-agent:dev .
```

## Servers and agents

An admin adds a server (Servers → Add server). Wren shows the server's agent token (`wwa_…`) once, together
with the `docker run` line that starts the agent; it keeps only the token's SHA-256. On the server's page an
admin can make a new token (the old one stops working at once) or revoke it. Viewers and editors see servers
but no tokens.

The agent connects **out** to Wren, so servers open no ports:

```
POST /api/agent/v1/heartbeat
Authorization: Bearer wwa_…
{"agent_version": "0.1.0", "hostname": "Tower", "os": "Unraid OS 7.4 x86_64",
 "docker_version": "29.8.2", "docker_error": "", "unraid": true}

200 {"server": {"id": 1, "name": "Tower"}, "interval_seconds": 30}
```

- A heartbeat every 30 seconds. A server is **online** while its last heartbeat is at most 90 seconds old
  (three missed beats), **offline** after that, **waiting for the agent** before the first one, and **no token**
  once revoked. Overview and Servers update live (server-sent events): on every heartbeat, and when a watcher
  that looks every 5 seconds sees a server drop out of the window.
- 401 for a missing, unknown, replaced or revoked token (API tokens `ww_…` don't work here, and agent tokens
  don't work on the API); 429 above 20 heartbeats a minute per server.
- Why plain HTTP requests (VV's remote-agent pattern) and not a WebSocket: they pass every reverse proxy and
  tunnel unchanged, keep the agent small (no WebSocket stack) and keep the server stateless. When jobs come
  (phase 4), the agent fetches them with a long-poll `GET` on the same footing; until then the server has no
  way to make an agent do anything (design §8).

### Starting an agent

The `docker run` line on the server's page, for Unraid:

```bash
docker run -d --name wren-agent --restart unless-stopped \
  -e WW_SERVER_URL=http://10.0.3.26:3003 \
  -e WW_AGENT_TOKEN=wwa_… \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v /mnt/user/appdata/wren-agent:/config \
  -v /mnt/user/appdata:/mnt/user/appdata \
  -v /usr/local/emhttp:/usr/local/emhttp:ro \
  -v /var/local/emhttp:/var/local/emhttp \
  -v /boot/config:/boot/config \
  -v /var/lib/docker:/var/lib/docker \
  ghcr.io/ferdinand99/wise-wren-agent:latest
```

It follows the spike's decision ([spike-findings.md](spike-findings.md#decision-agent-runtime-form)): a container,
not privileged, no added capabilities, running as root. **Access to the Docker socket is root access to the
host.** Every folder Wren may touch is mounted at the **same path** inside the agent (Compose binds are resolved
on the host). The Unraid mounts let later phases run Unraid's own `update_container`; this phase only checks
that `/boot/config/plugins/dockerMan` and `/usr/local/emhttp/plugins/dynamix.docker.manager` exist to report
"Unraid". On other Linux hosts the page's second line drops them.

### agent.toml

Optional while `WW_SERVER_URL` and `WW_AGENT_TOKEN` are set (they override the file). Its place:
`--config PATH`, else `WW_AGENT_CONFIG`, else `/config/agent.toml`. `wren-agent --check` checks it and exits.

```toml
server = "http://10.0.3.26:3003"
token = "wwa_…"
# Folders Wren may touch, each mounted at the same path in the agent.
roots = ["/mnt/user/appdata", "/mnt/cache/appdata"]
# docker_socket = "/var/run/docker.sock"

# A Compose stack updated by a script of its own (design §4; Fluxer from the spike).
[[script_stack]]
name = "fluxer"                               # a-z 0-9 - _
compose_project = "fluxer"                    # defaults to name
working_dir = "/mnt/cache/appdata/fluxer"     # under one of roots
handles_own_backup = true                     # Wren skips its own stop and snapshot
env = { COMPOSE_PROJECT_NAME = "fluxer" }
timeout = "20m"                               # 90s, 20m, 1h; at most 24h

[script_stack.update]
command = """
curl -fsSLO https://fluxer.dev/install.sh && curl -fsSLO https://fluxer.dev/install.sh.sha256 \
  && sha256sum -c install.sh.sha256 \
  && sh install.sh --update --allow-root --non-interactive --dir "$PWD"
"""

[script_stack.rollback]
command = "sh install.sh --rollback --allow-root --non-interactive --dir \"$PWD\""
```

The agent refuses to start (exit code 2) and names the field for: a missing or malformed server or token,
relative paths or `..` in roots and `working_dir`, `/` as a root, a `working_dir` outside the roots, a script
stack without a name (or a name used twice), without `update.command` or without a valid `timeout`, an
environment name that isn't a variable name, and any unknown key. In this phase the scripts are only checked,
never run.

The agent image carries what later phases need on the host: `docker-cli` with Compose (≥ 2.24.4), `bash`,
`curl`, `openssl`, GNU `coreutils` (`cp --reflink`), `jq`, and PHP 8.4 with `short_open_tag=On` and no
`gettext` (for Unraid's scripts).

## Translations

Every string is in all 26 languages (`tests/i18n.rs` checks coverage, placeholders and that no language just
copies English). Wren's own terms per language are in [`locales/glossary/`](../locales/glossary/); Docker,
Compose and Unraid stay untranslated.
