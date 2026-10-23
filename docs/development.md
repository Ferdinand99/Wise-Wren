# Development

Wise Wren is a Cargo workspace with two crates:

- **`wise-wren`** (the root, binary `wise-wren`): the server, built on
  [vole-kit](https://github.com/Ferdinand99/Vigilant-Vole/blob/main/docs/vole-kit.md), the shared kit in the
  Vigilant Vole repository (tag `vole-kit-v0.2.0`). The kit brings sign-in, setup, users and roles, API tokens,
  2FA, Security, General settings, the API reference, i18n in 26 languages, the database plumbing, request
  budgets, live updates, the notification providers and the design system. Wren adds servers and their agents,
  targets and version checks, registry credentials, notifications for new versions, the notification channels
  page, REST and MCP.
- **`wren-agent`** (`agent/`): the small agent that runs on each Docker host. It has no kit dependency: tokio,
  reqwest (rustls), serde and toml. It reads `agent.toml`, sends a heartbeat to Wren every 30 seconds and an
  inventory of what runs on the host (see [Discovery](#discovery-and-version-checks)). It changes nothing and
  takes no orders; the only processes it starts are its own `agent.toml` version commands.

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
  inventory.rs        # POST /api/agent/v1/inventory: the agent's report → targets (grouping rules)
  targets.rs          # targets, their images, remote digests; status derived from both
  registry.rs         # image refs, manifest HEAD with the Bearer challenge, Retry-After
  versions.rs         # version sources (GitHub releases, URL + regex/JSON pointer), release notes
  checks.rs           # check runs and the scheduler (intervals, "Check now")
  updates_notify.rs   # "new version available", once per version; digest per channel
  registries.rs       # Settings → Registries: credentials sealed with AES-256-GCM
  pages.rs            # Updates page, target page, Targets card, check-now and interval actions
  channels.rs         # Settings → Notifications (from Keen Kestrel, without alert rules)
  api.rs              # API reference (DocsSpec), GET /api/v1/{servers,targets,updates}, MCP tools
templates/            # Wren's pages (extend kit/layout.html)
static/               # favicon, icons, web manifest, wren.css (the wren's rose)
migrations/           # Wren's migrations (after the kit's core set)
locales/              # Wren's strings, 26 languages; glossaries in locales/glossary/
agent/src/            # config.rs (agent.toml), docker.rs (socket), discover.rs (inventory), heartbeat.rs,
                      # lib.rs (the loop)
tests/                # wren.rs (in-process app), discovery.rs (targets, checks, notifications against fakes),
                      # agent.rs (the real agent against Wren), i18n.rs
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
environment name that isn't a variable name, and any unknown key. Update and rollback scripts are only
checked, never run (phase 4 runs them).

## Discovery and version checks

**Inventory.** On start, every `scan_interval` (default `30m`, 1m–24h) and when a heartbeat's answer says
`"scan": true`, the agent sends `POST /api/agent/v1/inventory`:

- every container with `.Config.Image` (never `docker ps`, which hides `@sha256:` pins), the `RepoDigests` of the
  image it runs, its state and its `org.opencontainers.image.*`, `com.docker.compose.*` and `net.unraid.docker.*`
  labels;
- the `<Name>` of every Unraid template `/boot/config/plugins/dockerMan/templates-user/my-*.xml`;
- the Compose projects (`com.docker.compose.project`, `…working_dir`, `…config_files`), with **files reachable**
  only when the working dir exists at the same path inside the agent;
- the script stacks from `agent.toml`, with what their version commands printed.

Wren turns it into **targets**: a container with a Compose project label belongs to that project's **Compose
stack** (or to the **script stack** whose `compose_project` it is); any other container is an **Unraid template**
container when a template's `<Name>` is its name, else a plain **container**. Targets that vanish from the
inventory go away; a failed Docker read keeps the last ones.

**Checks** run on the server, never on the agent: an anonymous manifest `HEAD` with the index-first `Accept`
list and the `WWW-Authenticate: Bearer` challenge (spike Q1), one answer per image reference per run, Docker Hub's
token fetched without a probe, a `Retry-After` up to 10 s waited out once (longer: the registry is skipped for the
run). Never a pull or a manifest `GET`. Status per image: `current` (the registry's digest equals the running
image's repo digest for that repository), `update` (they differ) or `unknown` (pinned by digest, an image ID, no repo
digest because it was built locally, not checked yet, or the registry failed). A target is `update` when any image
is (or its version source reports a newer version), else `current` when any part is, else `unknown`.

- **Intervals:** every server's targets are checked every 6 hours (Targets card on the server's page, 1–168 h); a
  target can override it on its page. A scheduler looks every minute.
- **Check now** (server or target, editors and admins) sets the server's scan flag; the heartbeat's answer asks the
  agent for a fresh inventory, and the check runs as soon as it arrives (at most a minute's wait for an online
  agent). It is a fixed flag, never a command line (design §8).
- **Release notes:** for a target with an update whose `org.opencontainers.image.source` is a GitHub repository,
  the newest GitHub release (anonymous; one answer per repository kept 6 hours; after a 403/429 GitHub is left
  alone until its limit resets).
- **Registries** (Settings → Registries, admins): user name and password or token per registry host (`docker.io`
  for Docker Hub). The secret is sealed with AES-256-GCM under a key derived from the instance's `session.key`,
  never shown again, and kept when the field is left empty. Other registries stay anonymous.
- **Notifications:** one "new version available" per target and version (the version is marked as announced
  before sending, so it never repeats, also across restarts), through every active channel in its language. A
  channel with **One message per check** gets one digest per check run instead.
- **Pages and API:** the server's Targets card, a page per target (images with running and registry digests,
  version source, release notes, interval, Check now), **Updates** (everything with a new version) and an
  "Updates available" counter on the Overview. `GET /api/v1/targets[?server_id=]`, `GET /api/v1/updates` and the
  MCP tools `list_targets` and `list_updates` (read-only).

### Version sources for script stacks

```toml
[script_stack.version]
# Exactly one source of the newest version:
github = "owner/repo"                 # the newest GitHub release; Wren looks it up
# url = "https://example.com/version.json"
# json_pointer = "/version"           # or: regex = "v([0-9.]+)" (first group, else the whole match)
# latest_command = "…"                # the agent runs it; prints the newest version
# Optional: what is installed (else the version is unknown and only image digests count).
installed_command = "cat VERSION"
# timeout = "30s"                     # per command, at most 10m
```

The agent runs `installed_command` and `latest_command` itself with `sh -c` in the stack's `working_dir`, with the
stack's `env` and the timeout; Wren keeps the first line of their output. Versions compare as semver when both
parse (`v1.2` counts as `1.2.0`), otherwise any difference counts as newer.

### The dev instance on LANBox

`wise-wren-dev` (port 3003, data in `/mnt/user/appdata/wise-wren-dev`) and `wren-agent-dev`, started with:

```bash
docker run -d --name wren-agent-dev --restart unless-stopped --label owner=wise-wren \
  -e WW_SERVER_URL=http://10.0.3.26:3003 -e WW_AGENT_TOKEN=wwa_… \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v /mnt/user/appdata/wren-agent-dev:/config:ro \
  -v /boot/config:/boot/config:ro \
  -v /usr/local/emhttp:/usr/local/emhttp:ro \
  -v /mnt/user/appdata/ww-test-stack:/mnt/user/appdata/ww-test-stack:ro \
  wise-wren-agent:dev
```

Its `agent.toml` defines the throwaway script stack `ww-test-stack` (`/mnt/user/appdata/ww-test-stack`: a
`compose.yaml` with `traefik/whoami:v1.10.3` and a `VERSION` file) with `github = "traefik/whoami"` and
`installed_command = "cat VERSION"`. The Compose Manager stack `postiz` has its files in
`/boot/config/plugins/compose.manager/projects/postiz`, which the `/boot/config` mount makes reachable.

### Tower and Fluxer

When Tower gets its agent, mount the Fluxer folder at the same path (read-only is enough for checks; phase 4's
updates need it writable) and put this in Tower's `agent.toml` (`/mnt/user/appdata/wren-agent/agent.toml`):

```toml
roots = ["/mnt/cache/appdata/fluxer"]

[[script_stack]]
name = "fluxer"
compose_project = "fluxer"
working_dir = "/mnt/cache/appdata/fluxer"
handles_own_backup = true
env = { COMPOSE_PROJECT_NAME = "fluxer" }
timeout = "20m"

[script_stack.update]
command = """
curl -fsSLO https://fluxer.dev/install.sh && curl -fsSLO https://fluxer.dev/install.sh.sha256 \
  && sha256sum -c install.sh.sha256 \
  && sh install.sh --update --allow-root --non-interactive --dir "$PWD"
"""

[script_stack.rollback]
command = "sh install.sh --rollback --allow-root --non-interactive --dir \"$PWD\""
```

No `[script_stack.version]`: Fluxer's images use the moving tag `v1`, so "update available" comes from their
digests, like any Compose stack. Fluxer's GitHub releases are per component (`fluxer-api@2026.1020…`,
`fluxer-docs@…`) and say nothing about the `v1` stack as a whole, so they make a poor version source. The `docker
run` line needs `-v /mnt/cache/appdata/fluxer:/mnt/cache/appdata/fluxer` (or the whole `/mnt/cache/appdata`) and
`-v /mnt/user/appdata/wren-agent:/config`.

The agent image carries what later phases need on the host: `docker-cli` with Compose (≥ 2.24.4), `bash`,
`curl`, `openssl`, GNU `coreutils` (`cp --reflink`), `jq`, and PHP 8.4 with `short_open_tag=On` and no
`gettext` (for Unraid's scripts).

## Translations

Every string is in all 26 languages (`tests/i18n.rs` checks coverage, placeholders and that no language just
copies English). Wren's own terms per language are in [`locales/glossary/`](../locales/glossary/); Docker,
Compose and Unraid stay untranslated.
