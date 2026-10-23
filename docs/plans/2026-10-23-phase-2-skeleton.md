# Wise Wren – Phase 2 skeleton implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.
> Each task is test-first: write the test named under **Proof**, see it fail, then make it pass.

**Goal:** Wren on vole-kit 0.2 with sign-in, settings, notification channels, 26 languages and the family
layout, plus servers with their agents: an admin adds a server, starts `wren-agent` on it, and the server shows
as online (design §12, phase 2: "an agent on LANBox shows as online in the family design").

**Architecture:** A Cargo workspace. The root crate `wise-wren` (binary `wise-wren`) is built like Keen Kestrel on
`vole-kit` (tag `vole-kit-v0.2.0`, reference consumer `examples/kit-demo`). The member crate `wren-agent`
(`agent/`) is a small binary with no kit dependency: it reads `agent.toml`, reads Docker's `/version` and `/info`
from the socket, and posts a heartbeat. Agent → server only; the server has no job channel yet (§8 still holds).

**Tech Stack:** Rust 1.99.1, edition 2024; axum 0.8, askama 0.16, htmx + SSE, rusqlite + deadpool-sqlite, rustls;
the agent: tokio, reqwest (rustls), serde, toml.

**Spec:** `docs/design.md` §3 (identifiers, agent runtime form), §8 (security), §9 (interface), §12;
`docs/spike-findings.md` (agent mounts, `agent.toml` entries).

## File structure

```
Cargo.toml, Cargo.lock, build.rs, rust-toolchain.toml      # workspace + wise-wren; vole-kit by tag
src/main.rs           # flags/env (WW_*), logging, server
src/lib.rs            # start(), router(), Wren state (Core, live changes, agent budgets)
src/product.rs        # KitConfig: names, cookies, prefixes, nav, settings tabs, locales, static files
src/config.rs         # clap flags with WW_ variables (port 3003)
src/server.rs         # listener, offline watcher, graceful shutdown
src/db.rs             # migration set; servers table access
src/servers.rs        # server pages: Overview, Servers, add, detail, token, revoke, delete, live
src/agent_api.rs      # POST /api/agent/v1/heartbeat (wwa_ tokens, budget), docker run snippets
src/channels.rs       # Settings → Notifications (Kestrel's page, without alert rules)
src/api.rs            # DocsSpec, GET /api/v1/servers, MCP stub
templates/            # overview, servers, server, server_new, notification pages, partials
static/               # favicon, icons, manifest, wren.css (rose accent)
migrations/0001_servers.sql, 0002_channels.sql
locales/*.toml (26), locales/glossary/ (README + 25)
agent/Cargo.toml, agent/src/{main,lib,config,docker,heartbeat}.rs
Dockerfile (targets `server`, `agent`), docker/entrypoint.sh, docker/php-wren.ini
.github/workflows/{ci,docker-publish,release-please}.yml, release-please-config.json, .release-please-manifest.json
tests/common/mod.rs, tests/wren.rs, tests/agent.rs, tests/i18n.rs
docs/development.md
```

## Global constraints

- Identifiers exactly as §3: product "Wise Wren", short "Wren", slug `wren`, cookies `ww_session`, `ww_2fa`,
  `ww_flash`, API tokens `ww_`, agent tokens `wwa_`, env prefix `WW` (`WW_PUBLIC_URL`, `WW_TRUST_PROXY`,
  `WW_SETUP_TOKEN`, `WW_DATA_DIR`, `WW_PORT`), UA `WiseWren/<version>` (agent `WiseWrenAgent/<version>`),
  port 3003, database `wren.sqlite`.
- Versions start at `0.0.0`; release-please `initial-version` `0.1.0`. Nothing is tagged, released or pushed.
- Only admins add servers and see, regenerate or revoke agent tokens (§8). Agent tokens are shown once and
  stored as SHA-256.
- The agent never writes anything and never calls Docker beyond `GET /version` and `GET /info`.
- All builds and tests run on LANBox (`rust:1.99` container, `ww-*` volumes); nothing heavy on the PC.
- Every new string in all 26 languages, worded with `locales/glossary/`; no plural keys unless needed.

## Review Focus

1. **Token handling:** an agent token is never stored or logged in clear; a revoked or regenerated token gets
   401 at once; API tokens (`ww_`) and session cookies can't post heartbeats, and agent tokens can't use the API.
2. **Online/offline:** online means a heartbeat within the last 90 s (3 missed 30 s beats). The watcher must turn
   a silent agent offline on the live pages without a page reload.
3. **Agent config errors:** relative paths, scripts without a name, command or timeout, a `working_dir`
   outside the allowed roots and unknown keys all stop the agent with a message naming the field, exit code 2.
4. **The `docker run` line** carries the spike's mounts at identical paths and nothing privileged.

---

### Task 1: Workspace, product config and the kit's pages

**Files:** `Cargo.toml`, `build.rs`, `rust-toolchain.toml`, `src/{main,lib,product,config,server}.rs`,
`static/*`, `locales/en.toml` (product keys), `tests/common/mod.rs`, `tests/wren.rs`.

**Interfaces:** `wise_wren::install()`, `start(data_dir, public_url) -> Result<Wren, StartError>`,
`router(Wren) -> Router`, `Wren { core, changes: broadcast::Sender<()>, budgets }`, `config::Config`.

**Proof:** `setup_then_sign_in` (setup creates the admin, `/login` works, `ww_session` set);
`pages_name_wren_not_vole` (every kit page and `/api-docs` says "Wise Wren" and never "Vigilant", "Vole" or
`vv_`); `healthz_and_static` (`/healthz` ok, `/static/style.css` and `/static/wren.css` served);
`flags_and_defaults` (port 3003, `WW_PUBLIC_URL` checked).

### Task 2: Servers and agent tokens

**Files:** `migrations/0001_servers.sql`, `src/db.rs`, `src/servers.rs`, `templates/{servers,server,server_new}.html`.

**Interfaces:** table `servers(id, name UNIQUE NOCASE, token_hash UNIQUE, token_prefix, created_at,
last_seen_at, agent_version, hostname, os, docker_version, docker_error, unraid)`; `db::new_agent_token() ->
(token, hash, prefix)`; routes `GET/POST /servers`, `GET /servers/new`, `GET/DELETE /servers/{id}`,
`POST /servers/{id}/token` (new token), `POST /servers/{id}/revoke`; `agent_api::docker_run(url, token, unraid)`.

**Proof:** `admin_adds_a_server_and_sees_the_token_once` (token `wwa_` + 43 chars in the page, only its hash in
the database, not on a reload); `viewers_and_editors_cannot_manage_servers` (403); `duplicate_and_empty_names`
(422 with the message); `docker_run_has_the_spike_mounts` (socket, identical-path roots, Unraid paths, no
`--privileged`).

### Task 3: Heartbeat, status and live pages

**Files:** `src/agent_api.rs`, `src/servers.rs` (Overview, live), `templates/overview.html`,
`templates/partials/{server_list,overview_body}.html`.

**Interfaces:** `POST /api/agent/v1/heartbeat` with `Authorization: Bearer wwa_…` and
`Heartbeat { agent_version, hostname, os, docker_version, docker_error, unraid }` → `200 {server: {id, name},
interval_seconds: 30}`; 401 unknown/revoked, 429 over 20 a minute; `Status::{Online, Offline, Waiting, NoToken}`
from `last_seen_at` (`ONLINE_WINDOW` 90 s); `GET /live` and `GET /servers/live` (SSE, events
`overview-updated`, `servers-updated`); `server::spawn_watcher` broadcasts when the online set changes.

**Proof:** `heartbeat_marks_online_and_records_facts`; `bad_tokens_are_refused` (none, `ww_` API token,
unknown, revoked, regenerated-old → 401); `heartbeats_are_rate_limited`; `stale_heartbeat_is_offline`;
`live_view_pushes_on_heartbeat` (an SSE stream gets `servers-updated` after a heartbeat).

### Task 4: Notification channels

**Files:** `migrations/0002_channels.sql`, `src/channels.rs`, `templates/settings_notifications*.html`,
`templates/partials/notification_{list,form,language}.html`.

**Interfaces:** Kestrel's `/settings/notifications` routes and settings tab, minus alert rules; test messages
with the kit's `Alert::test`.

**Proof:** `channel_add_edit_delete` (add a webhook channel via the form, secrets masked on edit, delete);
`viewers_cannot_open_channels` (403).

### Task 5: API reference, REST and MCP stubs

**Files:** `src/api.rs`.

**Interfaces:** `DOCS: DocsSpec` (strict access, `agent token` credential), `GET /api/v1/servers` (viewer),
`POST /mcp` (no tools yet).

**Proof:** `api_lists_servers_with_a_token` (a `ww_` read token gets the list; an agent token gets 401);
`openapi_and_mcp_say_wren` (OpenAPI `info.title`/version, MCP `serverInfo.name` = `wren`).

### Task 6: `wren-agent`

**Files:** `agent/Cargo.toml`, `agent/src/{main,lib,config,docker,heartbeat}.rs`, `tests/agent.rs`.

**Interfaces:** `Config::load(path) -> Result<Config, ConfigError>` (TOML: `server`, `token`, `roots`,
`[[script_stack]]` as in the spike, `deny_unknown_fields`; env `WW_SERVER_URL`/`WW_AGENT_TOKEN` override
`server`/`token`; file `WW_AGENT_CONFIG`, default `/config/agent.toml`); `docker::facts(socket) -> Facts`
(HTTP/1.0 over the Unix socket); `heartbeat::collect()`, `Client::send(&Heartbeat)`; `wren-agent [--check]
[--config PATH]`, exit 2 on config errors.

**Proof:** `config_errors_name_the_field` (one case per rule); `fluxer_entry_from_the_spike_loads`;
`docker_facts_over_a_unix_socket` (a fake daemon); `unraid_detected_from_paths`; in `tests/agent.rs`:
`agent_heartbeat_reaches_wren` (the real client against an in-process Wren on a TCP port shows the server online).

### Task 7: Translations and glossary

**Files:** `locales/*.toml` (25 more), `locales/glossary/` (README + 25), `tests/i18n.rs`.

**Proof:** `every_language_is_complete` (`assert_full_coverage`); `wren_ships_every_language_the_kit_knows`;
`every_used_key_exists`.

### Task 8: Images, CI, release config, docs, dev instance

**Files:** `Dockerfile` (targets `server` and `agent`), `docker/entrypoint.sh`, `docker/php-wren.ini`,
`.dockerignore`, `docker-compose.yml`, workflows, release-please files, `docs/development.md`, `README.md`.

**Proof:** both images build on LANBox; sizes reported; `wise-wren-dev` on port 3003
(`/mnt/user/appdata/wise-wren-dev`) and `wren-agent-dev` registered to it, shown online on Overview.
