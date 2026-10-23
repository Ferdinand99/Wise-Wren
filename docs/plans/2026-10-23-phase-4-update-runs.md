# Wise Wren – Phase 4 update runs implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.
> Each task is test-first: write the test named under **Proof**, see it fail, then make it pass.

**Goal:** an approved update runs end to end on the agent (preflight, stop, snapshot, update, health check), a
failed one rolls back on its own, and everything is recorded step by step (design §12, phase 4: "an approved update
runs, and a deliberately broken one rolls back, on LANBox"). This is the first phase in which Wren **changes**
things on a server.

**Architecture:** The server never sends a command. The agent fetches **named, typed jobs** with a long-poll
`GET /api/agent/v1/jobs` (decided in phase 2) and runs the whole run itself: a lost connection to Wren can't leave a
target stopped half-way, and every step's timeout is enforced next to the target. After each step it posts the
result (`POST /api/agent/v1/runs/{id}/steps`); the answer to the preflight result is the last moment a run can be
cancelled (before anything stops) and the hook for phase 5's Vigilant Vole maintenance window. The agent re-checks
every job against its own discovery and `agent.toml` before acting, and refuses what they don't allow.

**Tech Stack:** as phase 3. The agent gains `docker` CLI calls (argv, never a shell string) and Docker API `POST`s
over its socket; snapshots use GNU `cp --reflink`, `getfattr` (Unraid user shares) and `btrfs` when present.

**Spec:** `docs/design.md` §2 (anti-goals), §4–§9; `docs/spike-findings.md` Q2 (Unraid `update_container` and
the API fallback), Q3 (script stacks, same-path rule), Q4 (snapshot method and fallback order).

## File structure

```
migrations/0004_runs.sql   # servers.mode/offline_notified_at, targets.mode/health_seconds/skipped_key/
                           # snoozed_until, runs, run_steps, actions
src/runs.rs        # approvals (approve/skip/snooze/cancel/rollback), dispatch, lapse rule, steps, finish,
                   # watchdog; the agent endpoints GET /jobs and POST /runs/{id}/steps|finish
src/run_notify.rs  # run results and agent offline/back notifications (channels shared with updates_notify)
src/pages.rs       # Updates page actions, target page (mode, health period, history), run page (+ live)
src/api.rs         # GET /api/v1/runs[/{id}], POST /api/v1/targets/{id}/{approve,skip,snooze}; MCP list_runs,
                   # approve_update
agent/src/config.rs   # [updates] allow/deny (globs on the target name)
agent/src/docker.rs   # POST/DELETE on the socket, the docker CLI with a timeout
agent/src/jobs.rs     # the job types, the long-poll loop, the re-check against discovery and agent.toml
agent/src/run.rs      # the run: steps, rollback, per-kind update methods behind a Host trait
agent/src/snapshot.rs # method choice (btrfs subvolume → reflink → plain copy), space check, restore, retention
templates/run.html, partials/run_steps.html, partials/runs_table.html; updates.html, target.html, overview_body.html
locales/*.toml (26)
```

## Global constraints

- **Nothing runs unless approved.** A run exists only because an editor or admin approved one exact version key
  (or asked to roll back one run). There is no `auto` mode yet; `notify` targets can't be approved; a target whose
  Compose files aren't reachable can't be approved.
- **The approval is for one version.** If the target's available version key changes before the run starts, the
  run lapses ("a newer version appeared") and nothing happens. The agent pulls the approved digest
  (`repo@sha256:…` of the image the container already runs, then tags it), never "whatever the tag is now".
- **The job is data, not a command.** Kinds `update` and `rollback`; fields are a target kind and name, the version
  keys, per image the container, its reference and the two digests, the health period and the snapshots to keep.
  No command line, no path, no image the target doesn't already use. Script stacks are named by their
  `agent.toml` name and run only the scripts defined there.
- **The agent decides last.** Before acting it re-reads Docker and checks: `[updates]` allows the target (deny wins,
  default allow all), the target exists with that kind, every job image is the image that container runs now, the
  local digest still equals the job's "from" digest, Compose files are reachable at the same path and every data
  path is under `roots`. Anything else: `refused by agent config` / `refused: …`, nothing changed.
- **A rollback touches only its own target:** its own containers, its own data paths (a path any other container
  also mounts is never snapshotted or restored, and the log says so), its own `.wren-snapshots/<target>/` folder and
  its own `wren-rollback/<target>` image tags.
- One run at a time per server; a stack updates as one unit. Every step has a timeout. A run can be cancelled until
  the agent reports its preflight (before anything stops).
- Every action (approve, skip, snooze, cancel, rollback, mode) is stored in `actions` with who did it, and logged.

## Review Focus

1. **Anti-goal "never change anything not allowed":** no code path starts a run without an `approved` row created
   by an editor/admin for the exact version key; lapse on a newer version; `notify` mode and unreachable files
   refused; the agent's own allowlist refuses before any Docker call that changes something.
2. **Anti-goal "a rollback touches only its own target":** snapshot and restore only of the target's own,
   unshared bind paths under `roots`; retention deletes only that target's own snapshots and tags.
3. **No remote shell:** the job carries no command, path or foreign image; refs and names are validated; the agent
   calls `docker` with argv and the scripts from `agent.toml` only.
4. **Fail safe:** preflight (agent online, space, the new image pulled) before anything stops; a failed update rolls
   back; a failed rollback stops, keeps everything and says "needs you"; a run whose agent went silent is closed
   as failed with an urgent notification, never retried on its own.

## Data model

- `servers.mode` (`approve`, the default, or `notify`); `servers.offline_notified_at` (once per outage).
- `targets.mode` (NULL = the server's), `targets.health_seconds` (NULL = 180), `skipped_key`, `snoozed_until`.
  `targets::apply` keeps a target that has a queued or running run, even when an inventory taken mid-run misses it.
- `runs(id, server_id, target_id NULL on delete, target_kind, target_name, kind update|rollback, rollback_of,
  from_key, to_key, shown, status queued|running|succeeded|rolled_back|failed|rollback_failed|refused|cancelled|
  lapsed, detail, requested_by, created_at, started_at, finished_at, last_report_at)`.
- `run_steps(run_id, step preflight|stop|snapshot|update|health|rollback, status running|ok|failed|skipped,
  started_at, duration_ms, log)`: one row per step, the log tail kept to 4 000 characters.
- `actions(at, username, action, target_id, run_id, detail)`.

## Job protocol

```
GET  /api/agent/v1/jobs                    (agent token; waits up to 25 s)
204                                        nothing to do
200 {"job": {"run_id": 12, "kind": "update", "target": {"kind": "container", "name": "ww-test-c"},
     "from_key": "ww-test-c=sha256:aa…", "to_key": "ww-test-c=sha256:bb…",
     "images": [{"container": "ww-test-c", "image": "localhost:5056/ww-test/app:latest",
                 "from": "sha256:aa…", "to": "sha256:bb…"}],
     "health_seconds": 180, "keep_snapshots": 2, "rollback_of": null}}

POST /api/agent/v1/runs/{id}/steps  {"step": "preflight", "status": "ok", "log": "…", "duration_ms": 812}
200 {"cancel": false}
POST /api/agent/v1/runs/{id}/finish {"outcome": "succeeded", "detail": ""}
200 {}
```

Outcomes: `succeeded`, `rolled_back` (the update failed and the rollback worked), `failed` (failed before anything
changed), `rollback_failed` (needs you), `refused`, `cancelled`.

---

### Task 1: Agent allowlist

**Files:** `agent/src/config.rs`. **Interfaces:** `[updates] allow = ["ww-*"]`, `deny = [...]`;
`Config::update_allowed(name) -> bool` (`*` and `?` globs; deny wins; no section allows all).

**Proof:** `update_allowlist_globs_and_deny_wins`; `bad_globs_name_the_field`.

### Task 2: Snapshots

**Files:** `agent/src/snapshot.rs`. **Interfaces:** `real_path(path)` (`/mnt/user` → `getfattr -n
system.LOCATIONS`), `method(path)`, `take(paths, dir) -> Snapshot`, `restore(&Snapshot)`, `prune(target_dir,
keep)`, `need_bytes`/space check.

**Proof:** `snapshot_and_restore_round_trip` (plain copy in a temp dir: changed, deleted and added files come back,
hard link kept); `space_check_refuses`; `prune_keeps_the_newest`.

### Task 3: The run on the agent

**Files:** `agent/src/{jobs,run,docker}.rs`. **Interfaces:** `Job`, `jobs::check(&Job, &Config, &[Container])
-> Result<Checked, String>`; `run::execute(host: &impl Host, job, reporter)`; `DockerHost` implements `Host`.

**Proof:** `job_outside_allowlist_is_refused`; `job_for_a_foreign_image_is_refused`; `preflight_failure_stops_nothing`;
`health_failure_rolls_back`; `rollback_failure_needs_you`; `cancel_after_preflight_stops_nothing`;
`own_backup_skips_stop_and_snapshot`; `recreate_body_drops_old_image_defaults`; `health_rules`.

### Task 4: Approvals, dispatch and results on the server

**Files:** `migrations/0004_runs.sql`, `src/{db,runs,targets}.rs`, `src/lib.rs` (routes, `jobs` notify).

**Proof:** `approval_runs_once_for_that_version` (approve → job → steps → finish; second poll empty);
`newer_version_lapses_the_approval`; `notify_targets_and_viewers_cannot_approve`; `one_run_per_server`;
`cancel_before_stop`; `skip_and_snooze_hide_until_newer`; `job_for_another_servers_run_is_refused`;
`silent_agent_fails_the_run`.

### Task 5: Notifications

**Files:** `src/run_notify.rs`, `src/updates_notify.rs` (shared channel loop), `src/servers.rs` (watcher).

**Proof:** `run_results_notify` (succeeded, rolled back, rollback failed is urgent); `agent_offline_once_and_back`.

### Task 6: Pages, REST, MCP, UX fixes

**Files:** `src/{pages,api,servers}.rs`, templates, `static/wren.css`.

**Proof:** `pages_show_runs_and_actions` (Updates with approve/skip/snooze, run page with steps, target history,
Overview recent runs, amber "Update available"); `api_and_mcp_runs_and_approve`.

### Task 7: Translations, docs, LANBox

**Files:** `locales/*.toml` (incl. `targets.image_count` → "6 images" with plural forms), `docs/development.md`,
`agent_api.rs` (`docker run` line), `Dockerfile` (`attr`, `btrfs-progs`).

**Proof:** `tests/i18n.rs` green; on LANBox with `ww-test-*` targets only: an approved update of a container, an
Unraid template container, a Compose stack and the script stack ends on the new digest with data intact; a broken
image rolls back with identical snapshot checksums; a target outside the allowlist is refused.

## Deferred (phase 5)

- Modes `auto`, auto windows, scheduled approvals and Vigilant Vole maintenance windows (the hook is the answer to
  the preflight step) and monitor-based health.
- Per-target data-path editing and exclusions (§7): this phase snapshots every unshared bind path under `roots`.
- ZFS snapshots (treated as a plain copy, as the spike decided).
