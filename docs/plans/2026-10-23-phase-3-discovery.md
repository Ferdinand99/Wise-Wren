# Wise Wren – Phase 3 discovery and version checks implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.
> Each task is test-first: write the test named under **Proof**, see it fail, then make it pass.

**Goal:** Wren lists every target on a server (containers, Unraid template containers, Compose stacks, script
stacks) with its current and available version, checks for new versions on an interval, shows release notes and
sends one notification per new version (design §12, phase 3: "Wren lists everything on LANBox with correct
'update available'"; Tower comes later).

**Architecture:** The agent stays read-only and gets no commands. It scans Docker (container list, container and
image inspect), the Unraid templates and its `agent.toml` script stacks, runs only its own version commands, and
pushes the result (`POST /api/agent/v1/inventory`) on start, on its own schedule and when the heartbeat reply asks
for a scan (`"scan": true`, a fixed flag set by "Check now"). The server turns the inventory into targets, checks
registries itself (anonymous index-first manifest `HEAD` with the Bearer challenge, from the spike), fetches
version sources and GitHub release notes, and notifies through the kit's providers.

**Tech Stack:** as phase 2; new crates on the server: `regex`, `semver` (both already in the lock through the kit),
`aws-lc-rs` (already in the tree through rustls; AES-256-GCM for registry credentials), `time` (the kit's `Alert`).

**Spec:** `docs/design.md` §4, §8, §9; `docs/spike-findings.md` Q1 (method and gotchas), Q2 (templates), Q3
(Compose labels, `agent.toml`).

## File structure

```
migrations/0003_targets.sql   # servers.check_interval_hours/scan_requested_at/inventory_at, targets,
                              # target_images, remote_digests, registries, notification_channels.digest
src/inventory.rs     # POST /api/agent/v1/inventory: the agent's report → targets (grouping rules)
src/targets.rs       # targets tables: upsert, status from local + remote digests and versions, queries
src/registry.rs      # image refs, local digest choice, manifest HEAD with challenge, credentials, Retry-After
src/versions.rs      # script stack version sources (GitHub releases, URL + regex / JSON pointer) and
                     # release notes (GitHub releases from org.opencontainers.image.source), cached
src/checks.rs        # check runs: what is due, one cache per run, scheduler task, "Check now"
src/registries.rs    # Settings → Registries (admin): credentials stored encrypted, masked
src/updates_notify.rs # "new version available": once per version, per channel digest option
src/pages.rs         # Updates page, target page, the server page's Targets card, check-now/interval actions
src/api.rs           # GET /api/v1/targets, /api/v1/updates; MCP tools list_targets, list_updates
agent/src/config.rs  # [script_stack.version], scan_interval
agent/src/docker.rs  # GET /containers/json, /containers/{id}/json, /images/{id}/json
agent/src/discover.rs # the inventory: containers, templates, Compose projects, script stacks + version commands
templates/updates.html, target.html, partials/targets_card.html, settings_registries*.html
locales/*.toml (26)
```

## Global constraints

- The agent changes nothing: Docker `GET`s only; the only processes it starts are its own `agent.toml` version
  commands (`sh -c`, in the stack's `working_dir`, with its `env`, a timeout, output clipped). The server never sends
  a command line; "Check now" only sets the fixed `scan` flag (§8).
- Version checks never pull and never `GET` a manifest: `HEAD` only. Refs pinned by digest, image IDs and images
  without a repo digest (built locally) are `unknown` with no registry request. One result per image ref per check
  run; a 429 honours `Retry-After` (wait once when ≤ 10 s, else skip that registry for the run).
- Status values are exactly `current`, `update`, `unknown`.
- Registry passwords never leave the server: AES-256-GCM with a key derived from the instance's key, shown as the
  kit's mask, kept when the field is left empty. Admins only.
- GitHub (release notes, `github` version sources) anonymously, cached 6 h per repo, paused until the rate limit's
  reset after a 403/429.
- One "new version available" notification per target and version (the version key is stored before sending).
- Every new string in all 26 languages with `locales/glossary/`.

## Review Focus

1. **Read-only:** nothing in the agent or the server can change a container or file; the inventory endpoint
   accepts only an agent token and only writes Wren's own tables.
2. **Unknown, never a false "update":** pinned, ID-only and local images make no request; a registry error is
   `unknown`; `docker ps` names are never used (`.Config.Image`).
3. **No spam:** the same version never notifies twice, also across restarts and re-scans.
4. **Secrets:** registry passwords encrypted at rest, masked in pages and API, absent from logs.

## Data model

- `servers` + `check_interval_hours` (default 6), `scan_requested_at`, `inventory_at`, `inventory_error`.
- `targets(id, server_id, kind container|unraid|compose|script, name, working_dir, files_reachable, source_url,
  version_source JSON, installed_version, latest_version, version_error, status, status_detail, available,
  check_interval_hours NULL=server's, check_requested, last_checked_at, notes_url, notes_title,
  notified_version, UNIQUE(server_id, kind, name))`.
- `target_images(target_id, container, service, image, state, local_digest, UNIQUE(target_id, container))`.
- `remote_digests(image PRIMARY KEY, digest, media_type, error, checked_at)`: the last answer per image ref, shared
  by every target; a target's status is derived from it and its images' local digests whenever either changes.
- `registries(id, host UNIQUE, username, secret BLOB)`.
- `notification_channels.digest`: one message per check run instead of one per target.

Grouping: a container with `com.docker.compose.project` belongs to that project's `compose` target (or to the
script stack whose `compose_project` it is); otherwise it is `unraid` when an Unraid template's `<Name>` matches,
else `container`.

---

### Task 1: Registry checks

**Files:** `src/registry.rs`.

**Interfaces:** `ImageRef::parse(&str) -> Result<ImageRef, Unknown>` (registry, repo, tag; `docker.io` →
`registry-1.docker.io`, `library/`); `local_digest(&ImageRef, &[String]) -> Option<String>`;
`Checker::new(http, creds) .digest(&ImageRef) -> Result<Remote, String>` (cache per ref, token per realm+scope,
Docker Hub's realm known up front, Basic credentials for the token request, 429 + `Retry-After`).

**Proof:** `refs_parse_like_the_spike`; `pinned_ids_and_local_images_are_unknown`; `local_digest_matches_repo`;
`head_follows_the_challenge_and_caches` (a fake registry on 127.0.0.1: 401 → token → HEAD, one HEAD per ref, no
GET); `credentials_reach_the_token_request`; `retry_after_is_honoured`.

### Task 2: Agent discovery

**Files:** `agent/src/{config,docker,discover,lib,heartbeat}.rs`.

**Interfaces:** `agent.toml`: `scan_interval` (default `30m`), `[script_stack.version]` with one of `github`,
`url` (+ `regex` or `json_pointer`), `latest_command`, and optional `installed_command`, `timeout` (default `30s`);
`discover::scan(&Config, root) -> Inventory`; `Client::inventory(&Inventory)`; heartbeat `Reply.scan`.

**Proof:** `version_config_errors_name_the_field`; `scan_reads_docker_templates_and_stacks` (fake socket and a temp
root: `.Config.Image` kept, `RepoDigests` per image, labels filtered, template match by `<Name>`, Compose files
reachable only when the working dir exists, version commands run in the working dir with a timeout).

### Task 3: Inventory and targets

**Files:** `migrations/0003_targets.sql`, `src/{db,inventory,targets}.rs`.

**Interfaces:** `POST /api/agent/v1/inventory` (agent token, 1 MB, its own budget) → `200 {"targets": n}`;
`targets::apply(c, server_id, &Inventory)`, `targets::refresh(c, target_id)`; heartbeat reply `scan`.

**Proof:** `inventory_builds_targets` (each kind, files not reachable, script stack claims its project, vanished
targets go away, remote digests already known give the status at once); `inventory_needs_an_agent_token`;
`check_now_sets_the_scan_flag`.

### Task 4: Check runs, version sources, release notes

**Files:** `src/{checks,versions}.rs`, `src/server.rs` (scheduler).

**Interfaces:** `checks::run_due(&Wren) -> Report`; due = requested, never checked, or older than the target's or
the server's interval; waits up to 60 s for a requested scan from an online agent; `versions::latest(source)`,
`versions::release_notes(source_url)`; `Wren.github_api` (tests point it at a fake).

**Proof:** `check_run_marks_updates` (fake registry: current/update/unknown, one HEAD per ref);
`intervals_decide_what_is_due`; `script_stack_versions` (GitHub release newer than installed → update; URL + JSON
pointer; agent-reported latest); `release_notes_for_updates_only`.

### Task 5: Registry credentials

**Files:** `src/registries.rs`, `templates/settings_registries*.html`.

**Interfaces:** `/settings/registries` (admin): list, add, edit (empty password keeps it), delete;
`registries::credentials(core) -> Creds` for a run.

**Proof:** `registry_credentials_are_encrypted_and_masked` (no clear text in the database or the edit page);
`only_admins_manage_registries`.

### Task 6: Notifications

**Files:** `src/updates_notify.rs`, `src/channels.rs` (digest switch).

**Proof:** `new_version_notifies_once` (webhook fake: one message, none on the next run, one for a newer version);
`digest_channel_gets_one_message`.

### Task 7: Pages, REST and MCP

**Files:** `src/pages.rs`, `src/api.rs`, `src/servers.rs`, templates, `src/product.rs` (nav: Updates).

**Interfaces:** `/updates`, `/targets/{id}`, `POST /servers/{id}/check`, `POST /targets/{id}/check`,
`POST /servers/{id}/interval`, `POST /targets/{id}/interval` (editors); `GET /api/v1/targets[?server_id=]`,
`GET /api/v1/updates`; MCP `list_targets`, `list_updates`.

**Proof:** `pages_show_targets_and_updates`; `viewers_cannot_check_or_change_intervals`; `api_and_mcp_list_targets`.

### Task 8: Translations, docs, LANBox

**Files:** `locales/*.toml`, `docs/development.md`, `README.md` (status line).

**Proof:** `tests/i18n.rs` green; on LANBox the dev instance lists the owner's containers, the Unraid template
containers, the Compose Manager stack `postiz` and the script stack `ww-test-stack` with the right status.

## Deferred

- **Tag following** (semver ranges per target): left for later to keep the phase small; nothing in the data model
  blocks it (a `follow` column on `targets` and a tag list request per registry).
- Agent offline notifications (design §9) belong with update runs (phase 4).
- Release notes for a Compose stack come from the first image with a source label, not per updated image.
- The multi-arch rule "never compare an index digest with a platform digest" is not enforced: the agent can't
  tell which kind a `RepoDigests` entry is without more requests; the spike saw index digests on both sides.
