# Wise Wren – Phase 5 Vigilant Vole integration and scheduling implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.
> Each task is test-first: write the test named under **Proof**, see it fail, then make it pass.

**Goal:** every run opens a Vigilant Vole maintenance window for the target's linked monitors and closes it at the
end (also after a rollback), health includes the monitors' *real* results during maintenance, an update can be
approved for a chosen time (shown in VV as planned maintenance ahead of time), and `auto` mode updates allowed
targets inside a per-server window (design §12, phase 5: "a scheduled update shows as planned maintenance in VV,
runs in its window, and a failed update rolls back without showing as down").

**Architecture:** Wren talks to VV's REST API v1 with one API token (`vv_…`), sealed like registry credentials.
VV is never contacted by the agent: the agent only asks Wren (`POST /api/agent/v1/runs/{id}/monitors`) after its
container checks passed, and Wren answers `ok`, `wait`, `failed` or `skipped` from VV's heartbeats. Windows follow a
small state machine on the run (`vv_state`), and a reconciler (every 30 s, and at once after a finish, cancel or
reschedule) closes or deletes what a run no longer needs, so every way a run ends (finish, lapse, cancel, the
silent-agent watchdog) closes its window. Scheduled and auto runs are ordinary `runs` rows: a scheduled run has
`scheduled_for`, an auto run `trigger = 'auto'`; `dispatch` keeps both until they may go and re-checks them then.

**Tech Stack:** as phase 4, plus `jiff` (already in the tree through the kit) for the site time zone.

**Spec:** `docs/design.md` §2 (anti-goals), §5, §6 (steps 2, 6, 8), §8, §9; VV's `src/api/v1/{maintenance,monitors,
pages,system}.rs`.

## File structure

```
migrations/0005_vv_scheduling.sql  # servers.auto_start/auto_end, targets.require_vv, runs.trigger/scheduled_for/
                                   # vv_window_id/vv_state, target_monitors
src/vv.rs          # the VV client (me, health, monitors, heartbeats, status pages, windows), settings, the monitor
                   # verdict, opening/closing windows, the reconciler
src/vv_pages.rs    # Settings → Vigilant Vole (URL, token, Test connection), monitor links on the target page
src/schedule.rs    # auto windows (site time zone), the auto picker, own-image rule, the Schedules page
src/runs.rs        # modes incl. auto, approve_at/reschedule, dispatch rules, before_stop → async, monitor endpoint
src/run_pages.rs, src/api.rs, src/run_notify.rs, templates, locales/*.toml (26)
agent/src/run.rs, agent/src/jobs.rs  # the monitor wait after the container checks
```

## Global constraints

- **Auto is never on by default.** `servers.mode` keeps its default `approve`; only an admin sets `auto` (server
  or target), only with an auto window on that server, and every change is an `actions` row and a log line.
  Setting a target to "as the server" while the server is `auto` counts as turning auto on (admins only).
- **Never outside a window.** The picker creates auto runs only while the server's window is open (site time zone),
  and `dispatch` lapses an auto run whose window closed or whose target is no longer `auto` before it starts.
- **Exclusions.** The effective mode decides: a target set to `notify` or `approve` on an `auto` server is never
  auto-updated. A target's auto run that failed, rolled back or was refused is not retried for that version.
- **Exact versions.** Scheduled and auto runs carry the version key seen at decision time; a newer version lapses
  them (phase 4's rule).
- **Not updated by Wren in v1:** images `ghcr.io/ferdinand99/wise-wren*` and `…/vigilant-vole*` (any registry, any
  tag, also local builds of those names) can't be approved, scheduled or auto-updated; the UI says so.
- **VV down:** the run continues without a window (logged on the run, notified), unless the target is set to
  "require VV": then the run is cancelled before anything stops. Monitor health with VV unreachable falls back to
  the container checks (or fails, with "require VV").
- **VV too old** (monitor JSON without `raw_status`): health uses container checks only, and the run log and the
  settings page say "Vigilant Vole is too old to report results during maintenance; update to 0.9.0 or later".

## Review Focus

1. **Auto mode never on by default:** migration defaults, no code path writes `auto` except the admin-checked
   setters (web, REST, MCP), each logged.
2. **Never outside a window:** `schedule::in_window` (wrap past midnight, site time zone) gates both the picker and
   `dispatch`; nothing else creates `trigger = 'auto'` runs.
3. **Excluded targets never auto-updated:** effective mode, own images, skip/snooze, failed-before, agent allowlist.
4. **Approvals for an exact version only:** `approve_at` and the picker store `to_key = available_key`; `dispatch`
   lapses on any change; rescheduling keeps the key.

## Data model

- `servers.auto_start`, `servers.auto_end` (`HH:MM`, site time zone; NULL = no window, so no auto).
- `targets.require_vv` (0/1).
- `runs.trigger` (`user` | `schedule` | `auto`), `runs.scheduled_for` (UTC, NULL = as soon as possible),
  `runs.vv_window_id`, `runs.vv_state` (`''` nothing yet, `planned`, `replan`, `open`, `closed`, `none`).
- `target_monitors(target_id, kind monitor|tag, ref, label)`.
- Settings `vv_url`, `vv_token` (sealed, base64).
- Server-side run steps `maintenance`, `monitors` and `close` in `run_steps` next to the agent's.

## Protocol

```
POST /api/agent/v1/runs/{id}/monitors   (agent token; after the container checks passed)
200 {"verdict": "ok" | "wait" | "failed" | "skipped", "log": "…"}
```

Verdict: from the start of the `health` step (T0) every linked monitor must have results after T0, the latest
`up`, and an unbroken run of `up` for the health period; `failed` when the latest is `down` after T0 + the period,
or when T0 + 2 × the period + 5 min passes; `wait` otherwise. The agent asks every 10 s; when Wren can't be
reached three times in a row it keeps the container checks' verdict.

---

### Task 1: Data model and own images

**Proof:** `own_images_are_never_updated` (approve, schedule, auto refused for `ghcr.io/ferdinand99/wise-wren-agent`).

### Task 2: Auto windows and the picker

**Proof:** `in_window_wraps_midnight`; `auto_runs_only_inside_the_window_and_skips_exclusions`;
`server_default_stays_approve`; `only_admins_turn_auto_on`.

### Task 3: Scheduled approvals

**Proof:** `scheduled_run_waits_for_its_time`; `reschedule_and_cancel`.

### Task 4: VV client, windows and the reconciler

**Proof (fake VV):** `run_opens_and_closes_a_window`; `scheduled_run_plans_a_window_and_activates_it`;
`vv_down_continues_or_cancels_with_require_vv`; `rollback_closes_the_window`.

### Task 5: Monitor health

**Proof:** `monitor_verdict_rules` (pure); `monitors_endpoint_uses_raw_status`; `old_vv_falls_back`; agent
`monitor_failure_rolls_back`.

### Task 6: Pages, REST, MCP, notifications

**Proof:** `pages_show_schedules_and_auto`; `api_and_mcp_schedules_and_modes`; `run_results_name_the_trigger`.

### Task 7: Translations, docs, LANBox

**Proof:** `tests/i18n.rs` green; the LANBox checks in the final report.

## Deferred

- The optional "scheduled update coming up" notification.
- Per-server time zones (the site time zone applies to every server).
- Auto windows as VV planned maintenance (only scheduled runs are announced ahead of time).
