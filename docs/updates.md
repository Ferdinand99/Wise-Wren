# Updates, approvals and schedules

**Nothing updates unless someone allowed it.** An update runs only because an editor or admin approved one exact
version of a target (now or for a chosen time), or because an admin turned on automatic updates for it and its
server's auto window is open. And the agent on that server only runs what its own `agent.toml` allows
([Adding an agent](agent.md#agenttoml)).

## Targets

Wren groups what runs on a server into **targets**: a plain container, an Unraid template container, a Compose
stack (all its containers update together) or a script stack (updated by its own script). It checks every
target's images against their registries every 6 hours (per server on its Targets card, per target on its page,
1–168 hours) with a manifest `HEAD` only, never a pull. **Check now** asks the agent for a fresh inventory and
checks at once.

A target is **update** when a registry has a different digest for the tag it runs (or its version source reports
a newer version), **current**, or **unknown** (pinned by digest, built locally, registry unreachable). Release
notes come from GitHub when the image's `org.opencontainers.image.source` label points there.

## Modes

Each server has a mode, and each target can override it (target page):

| Mode | What Wren does |
|---|---|
| **approve** (default) | Tells you, and updates when an editor or admin approves the version. |
| **notify** | Only tells you. Never updated, not even by an approval. |
| **auto** | Updates on its own inside the server's auto window. Admins only. |

A target set to `approve` or `notify` on an `auto` server is **excluded** from automatic updates.

**New targets on an `auto` server wait for an admin.** A target Wren has never seen before (a new container, a new
stack) starts with the explicit mode `approve` and a **New: approve first** badge on the server's Targets card and
its page, so nothing new updates itself unreviewed. An admin chooses **Allow automatic updates** for the target, or
**Allow automatic updates for all new targets** on the server's card; the target then follows the server's mode.
Saving the target's settings by hand ends the wait too (without turning automatic updates on). The REST API shows
`awaiting_auto_confirmation` on a target.

**Settings outlive a missing target.** When an inventory leaves a target out (it was removed, re-created, or the
scan was wrong), Wren keeps its settings (mode, health period, "require" switches, skip and snooze, Vigilant Vole
links) for 30 days. If the same name and kind returns within that time, everything comes back with it, including a
mode an admin had confirmed. After 30 days it is a new target again.


## Approving

Updates page, target page, `POST /api/v1/targets/{id}/approve` or the MCP tool `approve_update`:

- An approval is for **the version shown**. If a newer version appears before the run starts, the approval
  **lapses** and nothing happens; approve again.
- Several targets can be approved together; one run at a time per server.
- **Skip** hides this version (a newer one shows again); **snooze** hides the update for a day or a week; **Show
  again** undoes both.
- Wren never updates itself, its agents or Vigilant Vole yet; their pages say so.
- What the server's agent would refuse shows **Blocked** with the reason and can't be approved or scheduled: a target
  outside its `[updates]` allow list, a stack whose folder isn't under its `roots`, a stack whose files it can't
  reach. Approving several together skips those and says how many. The API answers `409` with the reason
  (`blocked_reason` on every target).

## Scheduled updates

Approve **for a time** on the target page (`POST /api/v1/targets/{id}/schedule`): approve during the day, update
tonight. Times are in the site's time zone (Settings → General). Until it starts, a scheduled run can be moved or
cancelled on the **Schedules** page. When the target has linked Vigilant Vole monitors, VV shows the update as
planned maintenance ahead of time ([Vigilant Vole](vigilant-vole.md)).

## Automatic updates

Off by default for every server and target. To turn them on, an admin:

1. sets the server's **auto window** (`HH:MM`–`HH:MM`, site time zone, may run past midnight) on its Targets
   card;
2. sets the server's mode to **auto** (or single targets to `auto`).

Every minute, inside the window, Wren queues one run per target whose effective mode is `auto`, for the version it
sees then. A run still waiting when the window closes lapses. Skipped and snoozed versions are left alone, and a
version whose automatic run failed, rolled back or was refused is not tried again on its own. Every mode change is
logged with who made it.

## A run

1. **Preflight**: free space checked, the old image kept, the approved digest pulled. Nothing has stopped yet;
   this is the last moment to cancel.
2. A Vigilant Vole maintenance window opens for the linked monitors.
3. **Stop** the target.
4. **Snapshot** of its data ([Snapshots and rollback](snapshots.md)).
5. **Update**: a container is recreated with its own settings (Unraid template containers through Unraid's own
   `update_container`), a Compose stack `docker compose up -d`, a script stack its script.
6. **Health check**: every container runs and is healthy, without restarts, for the health period (default 180 s,
   per target), and the linked VV monitors stay up.
7. **On failure, rollback**: the old image or Compose files and the data snapshot go back, and health is checked
   again. If the rollback fails too, the run is **rollback failed** and the target is left as it is for you.
8. The window closes and you get the result.

Each step's log is on the run page, live. Runs whose agent goes silent for 10 minutes end as rollback failed and
are never retried. When the agent restarts during a run (say, after an edit of `agent.toml`), its next heartbeat
ends the run as rollback failed at once, with a note to check the target, and the next approved update goes. An
approved version that is already running ends as succeeded ("already up to date") without stopping anything.

## Notifications

One "new version available" per target and version (or one digest per check run per channel), run succeeded,
failed and rolled back, failed with nothing changed, rollback failed (urgent), agent offline and back. A run the agent
refused because of its own `agent.toml` sends nothing (the run page shows it). Run results say who or what started
the run ("Approved by …", "Scheduled by …", "Automatic"), in the channel's language like the agent's texts, and when
no data snapshot was taken, why.
