# Development

Wise Wren is a Cargo workspace with two crates:

- **`wise-wren`** (the root, binary `wise-wren`): the server, built on
  [vole-kit](https://github.com/Ferdinand99/Vigilant-Vole/blob/main/docs/vole-kit.md), the shared kit in the
  Vigilant Vole repository (tag `vole-kit-v0.3.0`). The kit brings sign-in, setup, users and roles, API tokens,
  2FA, Security, General settings, the API reference, i18n in 26 languages, the database plumbing, request
  budgets, live updates, the notification providers and the design system. Wren adds servers and their agents,
  targets and version checks, registry credentials, notifications for new versions, the notification channels
  page, REST and MCP.
- **`wren-agent`** (`agent/`): the small agent that runs on each Docker host. It has no kit dependency: tokio,
  reqwest (rustls), serde and toml. It reads `agent.toml`, sends a heartbeat to Wren every 30 seconds and an
  inventory of what runs on the host (see [Discovery](#discovery-and-version-checks)). Beside them it waits for
  jobs: updates and rollbacks someone approved in Wren, which it checks against its own `agent.toml` and Docker
  before it runs them (see [Update runs](#update-runs)). It never takes a command line.

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
  runs.rs             # approvals (now or scheduled), auto runs, skip/snooze, the job channel
                      # (GET /api/agent/v1/jobs), step reports, lapses, modes
  run_pages.rs        # Updates actions, the run page (live), cancel, rollback, modes, auto windows, Schedules
  schedule.rs         # auto windows (site time zone), the auto picker, Wren's and VV's own images
  vv.rs               # Vigilant Vole: client, maintenance windows, monitor health, the reconciler
  vv_pages.rs         # Settings → Vigilant Vole, monitor links on the target page
  policy.rs           # the agent's reported [updates] and roots: blocked targets, roots to suggest
  texts.rs            # the agent's coded run texts and step notes, in the reader's language
  agent_toml.rs       # the agent.toml helper (admins): builds a snippet to copy, stores nothing
  run_notify.rs       # run results, agent offline/back
  targets.rs          # targets, their images, remote digests; status derived from both
  registry.rs         # image refs, manifest HEAD with the Bearer challenge, Retry-After
  versions.rs         # version sources (GitHub releases, URL + regex/JSON pointer), release notes
  checks.rs           # check runs and the scheduler (intervals, "Check now")
  updates_notify.rs   # "new version available", once per version; digest per channel
  registries.rs       # Settings → Registries: credentials sealed with AES-256-GCM
  pages.rs            # Updates page, target page, Targets card, check-now and interval actions
  channels.rs         # Settings → Notifications (from Keen Kestrel, without alert rules)
  api.rs              # API reference (DocsSpec), targets, updates, runs, approvals, schedules, modes; MCP tools
  api_settings.rs     # REST for servers and agent tokens, Check now, intervals, registries, VV, monitor links,
                      # notification channels
  fetch.rs            # guarded fetch of user-influenced URLs (public addresses only, pinned DNS, redirects
                      # re-checked, size and time caps)
templates/            # Wren's pages (extend kit/layout.html)
static/               # favicon, icons, web manifest, wren.css (the wren's rose)
migrations/           # Wren's migrations (after the kit's core set)
locales/              # Wren's strings, 26 languages; glossaries in locales/glossary/
agent/src/            # config.rs (agent.toml), docker.rs (socket, CLI), discover.rs (inventory), heartbeat.rs,
                      # jobs.rs (job channel, the re-check), run.rs (the run), snapshot.rs, lib.rs (the loops)
tests/                # wren.rs (in-process app), discovery.rs (targets, checks, notifications against fakes),
                      # agent.rs (the real agent against Wren), runs.rs (approvals, jobs, runs),
                      # vv.rs (VV windows and health against a fake VV, schedules, auto mode), i18n.rs
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
# agent.toml needs an [updates] allow list: without one the agent refuses every update and
# rollback. Put this in /config/agent.toml (a list of just the names you want updated is safer):
#   [updates]
#   allow = ["*"]
docker run -d --name wren-agent --restart unless-stopped \
  -e WW_SERVER_URL=http://10.0.3.26:3003 \
  -e WW_AGENT_TOKEN=wwa_… \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v /mnt/user/appdata/wren-agent:/config \
  -v /mnt/user/appdata:/mnt/user/appdata \
  -v /mnt/cache/appdata:/mnt/cache/appdata \
  -v /usr/local/emhttp:/usr/local/emhttp:ro \
  -v /var/local/emhttp:/var/local/emhttp \
  -v /boot/config:/boot/config \
  -v /var/lib/docker:/var/lib/docker \
  ghcr.io/ferdinand99/wise-wren-agent:latest
```

It follows the spike's decision ([spike-findings.md](spike-findings.md#decision-agent-runtime-form)): a container,
not privileged, no added capabilities, running as root. **Access to the Docker socket is root access to the
host.** Every folder Wren may touch is mounted at the **same path** inside the agent (Compose binds are resolved
on the host); an Unraid share is also mounted at its pool path (`/mnt/cache/appdata`) so snapshots can be reflink
copies on the real filesystem. The Unraid mounts let the agent run Unraid's own `update_container` for template
containers. On other Linux hosts the page's second line drops them. The line starts with a reminder: **without
an `[updates]` allow list in `agent.toml` (`allow = ["*"]` to start), the agent refuses every update and rollback.**


### agent.toml

`WW_SERVER_URL` and `WW_AGENT_TOKEN` override the file's `server` and `token`, but `[updates]` is required (the agent
refuses every update without an allow list). Its place:

`--config PATH`, else `WW_AGENT_CONFIG`, else `/config/agent.toml`. `wren-agent --check` checks it and exits.

```toml
server = "http://10.0.3.26:3003"
token = "wwa_…"
# Folders Wren may touch, each mounted at the same path in the agent
# (on Unraid also Compose Manager's stacks).
roots = ["/mnt/user/appdata", "/mnt/cache/appdata", "/boot/config/plugins/compose.manager/projects"]
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

The agent refuses to start (exit code 2) and names the field for: a missing or malformed server or token (plain `http://` is accepted only for loopback, private, link-local
and `.local`/`.lan`/`.home.arpa`/`.internal` hosts or single-label names),
relative paths or `..` in roots and `working_dir`, `/` as a root, a `working_dir` outside the roots, a script
stack without a name (or a name used twice), without `update.command` or without a valid `timeout`, an
environment name that isn't a variable name, a bad `[updates]` glob or `min_health_seconds`, a bad `min_interval`, and any
unknown key. A missing `[updates]` allow list is not a start error: the agent runs, reports and refuses every job.

```toml
# Required: which targets Wren may update, as globs on the target's name (* and ?).
# Without an allow list the agent refuses every update and rollback.
[updates]
allow = ["ww-test-*", "nextcloud"]
deny = ["ww-test-keep*"]          # wins over allow
min_health_seconds = 30           # optional: a job never gets a shorter health period
require_snapshot = true           # optional: default true when roots are set
```

A job for a target outside `allow` (or inside `deny`) is refused with "refused by agent config" before the agent
even reads Docker, and every job is refused (`refused.updates_required`) when there is no allow list;
`wren-agent --check` prints what the section allows, the minimums and the run ledger's state, and hints when
`/boot/config/plugins/compose.manager/projects` exists outside `roots`.

**The run ledger** (`agent/src/ledger.rs`, `runs.json` beside `agent.toml`, mode 0600, written to a temp file and
renamed) records per target the last 20 runs this agent started (`run_id`, kind, outcome, start time, whether the
update's rollback was used). After the allow-list and `jobs::check` and before `execute`, `run::admit` asks it:
a run id must exceed every recorded one (`refused.run_old`), a rollback must name the newest succeeded update and
only once (`refused.rollback_unknown`, `refused.rollback_used`), a script stack waits `min_interval`
(`refused.too_soon`), and a ledger that can't be written refuses the job (`refused.ledger_write`). The effective
health period and snapshot requirement (`[updates] min_health_seconds`, `require_snapshot`) are applied in
`jobs::check` and listed at the top of the preflight step's log. A restore keeps the replaced data as
`<name>.wren-aside-<run>` until the target's next preflight (`snapshot::clear_asides`).

**Target lifecycle on the server** (`targets::apply`, migration 0008): a target missing from an inventory is
copied to `target_memory` (settings and monitor links as JSON) for 30 days and restored by `settle_new` when
the same server, kind and name returns; a target never seen on an `auto` server gets `mode = 'approve'` and
`auto_pending = 1` until `runs::allow_auto` (admin; per target or `allow_auto_new` for the server) or a saved
policy clears it.

```toml
# Optional: rollback points kept per target.
[retention]
rollback_images = 1               # updates whose old images stay tagged (1-10, default 1)
snapshots = 2                     # data snapshots (1-10; default: the job's keep_snapshots)
```

**What Wren knows ahead.** The inventory carries the agent's `policy` (`{"allow": [...] | null, "deny": [...],
"roots": [...]}`; never the token or scripts) and each container's bind sources (`binds`). Wren stores the policy per
server (`servers.policy`, NULL from agents before 0.4.0) and computes per target a **blocked** reason
(`policy::blocked`): outside allow or inside deny, a stack folder outside the roots, stack files not reachable. Blocked
targets show a badge (Updates, Targets card, target page), can't be approved or scheduled (pages, `409` from
`POST /api/v1/targets/{id}/approve|schedule`, an MCP error), are skipped by bulk approval, and their queued runs lapse;
`blocked_reason` is on every target in the API. The bind sources and Compose folders become the roots the
**agent.toml helper** suggests (`servers.folders`; `GET /servers/{id}/agent-toml`, admins, a GET form that only renders
text).

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
run). Never a pull or a manifest `GET`. Status per image: `current` (the registry's digest equals one of the running
image's repo digests for that repository: an image pulled again by a new index digest over the same content keeps
both), `update` (none does) or `unknown` (pinned by digest, an image ID, no repo
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
  never shown again, and kept when the field is left empty. Other registries stay anonymous. A registry on a
  private, loopback or link-local address (a LAN registry) is only asked when it has an entry here (any login
  will do for one that needs none); `localhost` and `127.0.0.1` need no entry. Image references are validated,
  token services must belong to the registry, redirects are not followed, and a lookup has 20 seconds and a run
  10 minutes (`registry.rs`, `checks.rs`; `docs/security.md`).
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

`wise-wren-dev` (port 3003, host network so it reaches the test registry on `localhost:5056` like the daemon does,
data in `/mnt/user/appdata/wise-wren-dev`) and `wren-agent-dev`, started with:

```bash
docker run -d --name wren-agent-dev --restart unless-stopped --label owner=wise-wren \
  -e WW_SERVER_URL=http://10.0.3.26:3003 -e WW_AGENT_TOKEN=wwa_… \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v /mnt/user/appdata/wren-agent-dev:/config:ro \
  -v /boot/config:/boot/config \
  -v /usr/local/emhttp:/usr/local/emhttp:ro \
  -v /var/local/emhttp:/var/local/emhttp \
  -v /var/lib/docker:/var/lib/docker \
  -v /mnt/user/appdata/ww-test:/mnt/user/appdata/ww-test \
  -v /mnt/disk1/appdata/ww-test:/mnt/disk1/appdata/ww-test \
  -v /mnt/user/appdata/ww-test-stack:/mnt/user/appdata/ww-test-stack \
  -v /mnt/disk1/appdata/ww-test-stack:/mnt/disk1/appdata/ww-test-stack \
  wise-wren-agent:dev
```

Its `agent.toml` has `roots = ["/mnt/user/appdata/ww-test", "/mnt/user/appdata/ww-test-stack"]` and
`[updates] allow = ["ww-test-*"]`: the owner's containers (postiz, Newt, dockersocket, …) are refused even if
someone approved them. Read-write mounts exist only for the test targets (plus Unraid's paths, as the spike
decided). The throwaway targets, all on the test registry `ww-test-registry` (`localhost:5056/ww-test/app:<tag>`,
busybox images with the variants `v1`, `v2`, `v3`, `broken` (damages `/data` and exits) and `unhealthy` (damages
`/data`, healthcheck fails)):

| Target | Kind | Data |
|---|---|---|
| `ww-test-container` | container | `/mnt/user/appdata/ww-test/container/data` |
| `ww-test-unraid` | Unraid template (`my-ww-test-unraid.xml`) | `/mnt/user/appdata/ww-test/unraid/data` |
| `ww-test-compose` | Compose stack (`/mnt/user/appdata/ww-test/compose`) | `…/compose/data` |
| `ww-test-stack` | script stack (`docker compose pull && docker compose up -d`) | `/mnt/user/appdata/ww-test-stack/data` |
| `ww-other-refused` | container outside the allowlist | none |

The scripts in `/mnt/disk1/ww-test-scripts/` on LANBox: `push.sh <tag> <variant>` publishes a variant under the
same tag (a new digest, so "Update available" after Check now), `sums.sh <dir>` checksums a data folder, `run.sh`
pushes, waits for the check, approves through the REST API and prints the run. The health period of the test
targets is 15 s. The Compose Manager stack `postiz` has its files in
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

## Update runs

**Nothing runs unless someone allowed it** (design §2). An update runs only because an editor or admin approved
**one exact version** of a target (now or for a chosen time), or because an admin set the target to `auto` and its
server's auto window is open; and the agent on that server only runs what its own `agent.toml` allows.

- **Modes.** Each server has a mode (`approve`, the default, `notify` or `auto`) on its Targets card; a target can
  override it on its page. `notify` targets are never updated: Wren only tells you about new versions. `auto` is
  described under [Scheduling and automatic updates](#scheduling-and-automatic-updates).
- **Approve, skip, snooze** (Updates page, target page, `POST /api/v1/targets/{id}/{approve,skip,snooze}`, MCP
  `approve_update`). An approval is for the version key shown (`version_key` in the API): if a newer version
  appears before the run starts, the approval **lapses** and nothing happens. Several updates can be approved
  together; each runs for its own version, one at a time per server. Skip hides this version (a newer one shows
  again), snooze hides the update for a day or a week, "Show again" undoes both. Stacks whose files the agent
  can't reach can't be approved. Every action is stored in `actions` with who did it.
- **Roll back** (run page, editors): the newest successful update of a target can be rolled back to the image and
  snapshot taken before it, while the agent still keeps them (the last 2 per target). Data written since is lost;
  the page says so.

### The job channel

The agent waits on `GET /api/agent/v1/jobs` (Wren holds it up to 25 s, `204` when there is nothing). A job is
data, never a command:

```json
{"job": {"run_id": 12, "kind": "update", "target": {"kind": "container", "name": "ww-test-container"},
 "from_key": "ww-test-container=sha256:02d8…", "to_key": "ww-test-container=sha256:4f90…",
 "images": [{"container": "ww-test-container", "image": "localhost:5056/ww-test/app:container",
             "from": "sha256:02d8…", "to": "sha256:4f90…"}],
 "health_seconds": 180, "keep_snapshots": 2, "rollback_of": null, "require_snapshot": false}}
```

Kinds: `update` and `rollback` (of run `rollback_of`). Before acting, the agent checks the job against `[updates]`,
then re-reads Docker: the target exists with that kind (an Unraid template container only when its template
exists), every image is the one that container runs now at the `from` digest, the `to` digest is a digest, a
Compose stack's folder and files are under `roots` and mounted at the same path, a script stack is in
`agent.toml` (its scripts are the ones there, run by name). Anything else: the run ends as `refused` and nothing
changed. It posts each step to `POST /api/agent/v1/runs/{id}/steps` (`{"step", "status", "log", "duration_ms"}`)
and the end to `…/finish` (`succeeded`, `rolled_back`, `failed`, `rollback_failed`, `refused`, `cancelled`).

**Coded texts (0.4.0+).** Besides the English `detail`, the finish body carries `code` and `params`
(`refused.not_allowed {target}`, `refused.not_under_roots {folder}`, `done.already_on {target}`, …), and a step report
`notes` (`[{"code", "params"}]`: for the snapshot `path.shared {path, with}`, `path.not_under_roots {path}`,
`path.root {path}`, `path.volume {volume, container}` and `snapshot.none`). Wren keeps known codes only
(`texts::key`) and renders them in the page's or the channel's language (`[agent_msg]` in the locales); free text from
older agents shows as it came. Refusals that come from the agent's own `agent.toml` send no notification.

### A run

The agent runs every step itself, so a lost connection to Wren can't leave a target stopped half-way; each step
has a time limit.

1. **Preflight**, nothing stopped yet: free space for the snapshot (below), the old images kept as
   `wren-rollback/<target>:<run>-<container>`, and the **approved digest pulled** (`repo@sha256:…` of the image the
   container already runs, then tagged as its reference: never "whatever the tag is now"). Wren's answer to the
   preflight report is the last moment to cancel; before it answers, Wren opens the Vigilant Vole maintenance window
   (`vv::before_stop`, see [Vigilant Vole](#vigilant-vole)). If Wren can't be reached then, the run stops with nothing changed.
2. **Stop** what runs of the target (`docker stop -t 60`).
3. **Snapshot** of the target's data paths: its own bind mounts under `roots`, never a root itself and never a path
   another container also uses, i.e. mounts the same folder or one inside it (a container that mounts a folder
   *above* it, like a backup tool or the agent itself, doesn't count; the agent also knows its own container id
   from `/proc/self/cgroup`, `/proc/self/mountinfo` or its hostname). Every path left out is noted with why; when
   none was snapshotted the step is a warning (`snapshot.none`), and with the target's **Require a snapshot** the run
   stops before anything is touched (`failed.no_snapshot`). `/mnt/user/…` is resolved to its disk or pool with
   `getfattr -n system.LOCATIONS` when that path is mounted too. Method per path, in the spike's order: btrfs
   subvolume snapshot (subvolume root and `CAP_SYS_ADMIN`), reflink copy (`cp --reflink=always`), plain copy
   (`cp -a`, after checking size + 10 % fits). The run is refused when the space doesn't fit with 1 GB (or 2 %)
   left. Snapshots live in `<root>/.wren-snapshots/<target>/<run>/` on the same filesystem; the last 2 per target
   are kept (`[retention] snapshots`).
4. **Update**: a container is recreated with its own configuration through the Docker API (values that were only
   the old image's defaults are dropped, the old container is kept renamed `<name>-wren-old-<run>` until the run
   ends); an Unraid template container goes through Unraid's own `update_container <name>` (falling back to the API
   recreate plus Unraid's status entry when its PHP can't run), then Wren checks it runs the approved image; a
   Compose stack `docker compose up -d --pull never` in its folder at the same path; a script stack its
   `[script_stack.update]` command.
5. **Health check**: every container that ran before is running, its Docker healthcheck (if any) healthy, with no
   restart, for the target's health period (default 180 s, per target on its page) within 5 more minutes.
6. **On failure, rollback**: stop, restore the snapshot (the live folder is moved aside and deleted only once the
   copy is back), put the old container or images back (a script stack runs its rollback script, or gets its
   Compose files and images back), start, check health again. If the rollback fails, the agent stops and leaves
   everything as it is: the run is `rollback_failed` ("needs you").
7. **Close**: the old container is removed after success; older snapshots of the target go, and after a successful
   update its kept images but the newest `[retention] rollback_images` updates' (default 1; never an image a container
   uses, never with force). The agent also prunes every target's kept images this way once at start.

Script stacks with `handles_own_backup = true` skip steps 2 and 3. One run at a time per server; a stack updates
as one unit. A running run whose agent goes offline and stays silent for 10 minutes (or any run silent for 26
hours) is closed as `rollback_failed`, never retried.

**Agent restarts.** Each agent process has a random boot id; it sends it with every heartbeat (`boot_id`, with
`uptime_seconds`) and on its job polls (`GET /api/agent/v1/jobs?boot=…`), and the run remembers the process it was
handed to. When the agent reports another boot id, its running runs are closed at once as `rollback_failed` ("The
agent restarted during the run; check <target>: it may be on the new or the old version.", with the name of a
`<name>-wren-old-<run>` container that may be left), the urgent notification goes out and the next approved run
goes. Runs handed out before 0.2.1 (no boot id) are closed when the agent reports it started after the run did.
Agents before 0.2.1 send no boot id and keep the old behaviour. The agent leaves a `-wren-old-` container alone:
it is the way back by hand.

**Already up to date.** An approved version that already runs (its digest is among the image's repo digests) ends
as `succeeded` without stopping anything, in Wren before the job goes out and in the agent after its check.

**Notifications:** run succeeded, failed and rolled back, failed with nothing changed, rollback failed (urgent),
and the agent offline (once per outage, after 90 s without a heartbeat) and back.

**Pages and API:** the run page shows every step live (status, duration, log), the target page its run history,
mode and health period, the Overview the running run, then the queued ones, then the 8 newest finished.
`GET /api/v1/runs[?target_id=&server_id=&limit=]`,
`GET /api/v1/runs/{id}` (with steps), `POST /api/v1/targets/{id}/{approve,skip,snooze}` (an API token with write
access of an editor or admin; session cookies don't work here), MCP `list_runs` and `approve_update`.

## Scheduling and automatic updates

- **Approve for a time** (target page, `POST /api/v1/targets/{id}/schedule`, MCP `approve_update` with `at`): the
  run waits until then, for the version key approved (a newer version makes it lapse). Times on pages are in the
  site's time zone (Settings → General); the API takes RFC 3339 with an offset or UTC. A scheduled run can be moved
  or cancelled until it starts (Schedules page, `POST /api/v1/runs/{id}/reschedule|cancel`).
- **Automatic updates are off by default** and only admins turn them on: a server's mode `auto` (or a target's)
  needs the server's **auto window** (`HH:MM`–`HH:MM`, site time zone, may run past midnight). A picker looks every
  minute and queues one run per target whose effective mode is `auto`, for the version key it sees then, only
  while the window is open; a run still queued when the window closes lapses. A target set to `approve` or
  `notify` on an `auto` server is excluded; skipped and snoozed versions are left alone; a version whose auto run
  failed, rolled back or was refused is not tried again on its own. Every mode change is logged (`actions`).
- **Not updated by Wren in v1:** images named `…/wise-wren*` and `…/vigilant-vole*` (Wren, its agents, VV) can't be
  approved, scheduled or auto-updated; their pages say so.
- **Schedules** (nav): upcoming scheduled updates with their VV window, and per server the auto window and the
  targets that update on their own. `GET /api/v1/schedules`, MCP `list_schedules`; modes through
  `POST /api/v1/targets/{id}/mode`, `POST /api/v1/servers/{id}/mode` and MCP `set_mode`.
- Run results name who or what started them ("Approved by …", "Scheduled by …", "Automatic (auto window)").

## Vigilant Vole

Settings → Vigilant Vole (admins): VV's address and an API token, sealed like registry credentials (AES-256-GCM
under the instance key), never shown again; **Test connection** checks the token, the version and the monitors.

**The token:** in Vigilant Vole, Settings → API tokens, create a token with access **`write`** for a user whose role
is **editor** or **admin** (a `read` token, or a viewer's, can't change maintenance windows). Wren uses
`GET /api/v1/me`, `/health`, `/monitors`, `/monitors/{id}/heartbeats`, `/status-pages` and
`POST/PATCH/DELETE /api/v1/maintenance[/{id}]`. VV has no finer token scopes.

**VV 0.9.0 or later** reports a monitor's real result during maintenance (`raw_status` on monitors and heartbeats).
With an older VV the monitor health check is skipped: the container checks decide, and the run log and Test
connection say "Vigilant Vole is too old to report results during maintenance; update to 0.9.0 or later".

- **Links** (target page, admins): monitors by monitor or by VV tag (every monitor with that tag at run time).
  A target without links relies on the container checks only, and its page says so.
- **Step 2:** when the agent's preflight is through, Wren opens a `once` window for the linked monitors (also on
  the status pages that show them) from now for at least two hours, or makes the run's planned window start now.
  If VV can't be reached the run goes on without a window (the run's *Maintenance window* step says why, and a
  notification goes out), unless the target is set to **Require Vigilant Vole**: then the run is cancelled before
  anything stops.
- **Step 6:** after its container checks passed, the agent asks `POST /api/agent/v1/runs/{id}/monitors` every 10 s.
  Wren reads each monitor's heartbeats since the health step began: `ok` once every monitor's latest `raw_status`
  is `up` and has been for the health period, `failed` (→ rollback) when a monitor is `down` after the period or
  the period isn't reached within twice it plus 5 minutes, else `wait`. When Wren can't be reached three times in a
  row, the container checks decide.
- **Step 8:** a reconciler (every 30 s, and at once when a run ends) ends the window a minute after the run ended,
  whatever the outcome (rolled back too), so the status page never shows the update as down.
- **Planned windows:** a scheduled run with linked monitors gets a planned window in VV ahead of time (title, start,
  twice the health period plus 15 minutes, monitors, status pages); moving the run moves it, cancelling it (or a
  lapse) deletes it, and it becomes the run's window when the run starts.

## REST API and MCP

REST v1 covers what the web UI does (design §9); the reference is generated from `api::DOCS` (`/api-docs`,
`/api/v1/openapi.json`), and `tests/api.rs` fails when a served route is missing from it. Roles as on the pages:
viewers read; editors approve, schedule, skip/snooze/resume, cancel, roll back, Check now and intervals;
admins add and delete servers and agent tokens, registries, notification channels, the Vigilant Vole connection
and monitor links, and turn automatic updates on. **Writes need an API token with write access**; a session cookie
on an `/api/v1` write is refused (those paths skip the CSRF check). Secrets are write-only: an agent token appears
only in the answer that made it, registry passwords, the VV token and channel secrets never come back (masked or
`has_password`/`token_saved`). MCP (`/mcp`) has the same reads and the operational actions, filtered by the token
(read: list and get tools; write: approvals, skip/snooze, Check now, intervals, runs, modes; admin: monitor
links, Test connection); agent tokens, registries and channels stay REST only, so no secret lands in an
assistant's context.

**Server-side fetches.** A script stack's version `url` comes from an agent, so `fetch::get_limited` only connects
to public addresses: every address of the host is checked (loopback, private, link-local, CGNAT, multicast,
unspecified, reserved, documentation, ULA, site-local, and IPv4 inside IPv6), the connection is pinned to the
checked address, redirects are followed by hand (at most 3, each checked), at most 256 KB are read in 20 s, and only
the version is kept. GitHub requests go to a fixed API host with a validated `owner/repo`, and only
`https://github.com/…` release links are kept. Registry credentials are sent to a token realm only over https or
on the registry's own host. Tests open the guard with `Wren::allow_private_fetch`.

The user guides are in `docs/` (install, agent, updates, snapshots, vigilant-vole, security, upgrading).

## Translations

Every string is in all 26 languages (`tests/i18n.rs` checks coverage, placeholders and that no language just
copies English). Wren's own terms per language are in [`locales/glossary/`](../locales/glossary/); Docker,
Compose and Unraid stay untranslated.
