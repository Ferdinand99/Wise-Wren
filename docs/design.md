# Wise Wren – design

Wise Wren ("Wren") keeps the Docker containers on your servers up to date without surprises. It checks
for new versions on a schedule you choose, tells you about them, and updates only what you approve, or
what you have explicitly allowed it to update on its own. Every update is wrapped in a maintenance
window in [Vigilant Vole](https://github.com/Ferdinand99/Vigilant-Vole), backed by a snapshot of the
service's data, checked for health afterwards and rolled back if it fails.

It is part of the Hedgerow family (Vigilant Vole, Nimble Newt, Keen Kestrel): one Rust binary with
SQLite, built on the shared `vole-kit` crate, in the family's design.

## 1. What and why

The owner runs Unraid servers: **Tower** (production) and **LANBox** (testing), each with many containers.
Today he checks for updates by hand about once a day in the Unraid web UI and uses Unraid's update button.
That works for containers created from Unraid templates, but not for everything:

- **Compose stacks** are not covered by the Unraid button.
- **Script-updated stacks** such as Fluxer are updated with the vendor's own installer:

  ```sh
  cd /mnt/cache/appdata/fluxer
  curl -fsSLO https://fluxer.dev/install.sh
  curl -fsSLO https://fluxer.dev/install.sh.sha256
  sha256sum -c install.sh.sha256
  sh install.sh --update --allow-root
  ```

Nothing tells him when there is a new version. Existing tools either only notify (Diun, WUD) or update
blindly (Watchtower, which is no longer actively maintained). None of them pause monitoring, take a
snapshot first, verify health afterwards and roll back.

**Success, in the owner's words:** Wren does the checks for him on an interval he chooses, notifies
him, and updates. He stops the daily manual check and stops using the Unraid button.

## 2. Goals, non-goals, anti-goals

**Goals (v1)**

- One central Wren instance that watches **several servers** (Tower and LANBox) through a small agent
  on each.
- Detect new versions for three kinds of targets:
  1. single containers (including Unraid template containers),
  2. Docker Compose stacks,
  3. script-updated stacks (like Fluxer), whose version comes from a source such as GitHub releases.
- Check on an interval the user chooses (per server, overridable per target).
- Notify through vole-kit's ~40 notification providers (ntfy gives phone push from day one).
- Three modes per target: **notify only**, **approve** (one click in the web UI), **automatic**.
- Exclusions: with automatic updates on for a server, chosen containers or stacks can be excluded.
- **Scheduled updates:** approve now, run at a chosen time; a scheduled update creates a *planned*
  maintenance window in Vigilant Vole ahead of time.
- Safe updates: VV maintenance window → snapshot of the service's data → update → health check →
  automatic rollback (image/compose file and data) on failure → result notification.
- A REST API and MCP tools that cover everything the web UI can do, so an iOS app can be built later.

**Non-goals (v1)**

- The iOS app itself (planned; see §10). The push relay it needs is also later.
- Updating Unraid OS, Unraid plugins, VMs, LXC, or Kubernetes.
- Being an offsite backup tool. The owner already backs up appdata to Infomaniak kDrive with rclone;
  Wren's snapshots are short-lived local rollback points only.
- Building images from source.

**Anti-goals (what would make it a failure even if it works)**

- **Wren must never change anything the user hasn't allowed.** With automatic updates off, Wren
  updates nothing on its own: not a container, not a stack, not a "small" dependency. A production
  server updated "wrongly" behind the owner's back is the worst outcome. Automatic mode is **off by
  default** for every server and target.
- A rollback must only touch the target whose update it is undoing.
- Wren must not become a remote root shell: compromising the Wren server must not let an attacker run
  arbitrary commands on the servers (see §8).
- No "update available" spam: one notification per new version per target, with digest options.

## 3. Shape of the system

```
                    ┌──────────── Wren server (Docker, vole-kit) ────────────┐
 Registries ◄─────  │ version checks · policies · approvals · schedules      │ ──► Vigilant Vole API
 (Docker Hub, GHCR, │ web UI · REST API v1 · MCP · notifications · history   │     (maintenance windows,
  private) & GitHub │ SQLite (wren.sqlite)                                    │      monitor health)
                    └──────────────▲───────────────────────────▲──────────────┘
                                   │ agent protocol (outbound   │
                                   │ from agent, token)         │
                        ┌──────────┴─────────┐       ┌──────────┴─────────┐
                        │ wren-agent (Tower) │       │ wren-agent (LANBox)│
                        │ Docker socket,     │       │ …                  │
                        │ appdata, scripts   │       │                    │
                        └────────────────────┘       └────────────────────┘
```

- **Server:** Rust (edition 2024), axum, askama, htmx + SSE, rusqlite with deadpool-sqlite, rustls, on
  `vole-kit` by git tag (sign-in, users, API tokens with scopes, 2FA, settings, i18n with 26 languages,
  notifications, docs/OpenAPI, MCP, design system). Static musl binary, Alpine image, multi-arch.
- **Agent (`wren-agent`):** a small separate binary in the same repository (Cargo workspace), shipped as
  its own image and as a static binary. It connects **outbound** to the server (works behind NAT, no
  open ports on the servers), modelled on Vigilant Vole's remote agents (`src/agent/` in that repo):
  token per agent, long-poll or WebSocket for jobs, results posted back. It talks to the local Docker
  socket, reads/writes the data paths it's given, and runs only the scripts defined in its own local
  config (§8).
  **Runtime form (decided by the Phase 1 spike, `docs/spike-findings.md`):** the agent runs as a
  **container, not privileged and with no added capabilities by default**. It mounts:
  - `/var/run/docker.sock`;
  - every allowed data root and stack directory **at the identical host path** (e.g.
    `/mnt/cache/appdata:/mnt/cache/appdata`), because the daemon resolves Compose bind paths on the
    host, and a different path breaks script stacks (Fluxer's update failed with exit 6);
  - on Unraid also `/usr/local/emhttp` (ro), `/var/local/emhttp` (rw), `/boot/config` (rw) and
    `/var/lib/docker` (rw, for `unraid-update-status.json`), so it can run Unraid's own
    `update_container <name>` for template containers. The image ships PHP 8.4 for that (no `gettext`
    extension, `short_open_tag=On`).
  The image also carries `docker-cli` + Compose ≥ 2.24.4, `sh`, `bash`, `curl`, `openssl` and GNU
  coreutils (`cp --reflink`). `CAP_SYS_ADMIN` is an opt-in, needed only to delete btrfs subvolume
  snapshots.
- **Identifiers:** product "Wise Wren", short name "Wren", slug `wren`, cookies `ww_session` etc.,
  token prefix `ww_`, agent token prefix `wwa_`, env prefix `WW`, user agent `WiseWren/<version>`,
  default port 3003.

## 4. Targets and version detection

A **target** is the unit Wren updates. The agent discovers containers and Compose projects
automatically (Compose labels `com.docker.compose.project`); script stacks are added by hand.

| Kind | Discovered by | "New version" means | Update action |
|---|---|---|---|
| Container | Docker API on the agent | the registry digest for the container's image reference differs from the running image's digest | pull, then recreate with the same configuration |
| Unraid template container | container + Unraid template XML (`/boot/config/plugins/dockerMan/templates-user/`) | same as above | recreate the way Unraid does, so the Unraid UI stays in sync (see Phase 1 spike) |
| Compose stack | Compose labels + the project's working dir and files | any service image digest changed | `docker compose pull` then `up -d` for the changed services (whole stack by default) |
| Script stack | added by hand: a Compose project plus scripts defined on the agent | any service image digest changed (like a Compose stack), or an optional version source reports a newer version | run the agent-defined update script; roll back with the agent-defined rollback script |

**Script stacks, learned from Fluxer's installer.** Fluxer is a Compose project in
`/mnt/cache/appdata/fluxer` (`docker-compose.yml` + `.env`, images on the moving tag `v1`). Its
`install.sh --update` is a complete, careful upgrade on its own: it records the running image IDs, takes
a `pg_dump` (so Postgres must be **running**), stops the stack for a volume copy, refreshes the stack
files, pulls, recreates and verifies health, exiting non-zero (6 = "the stack did not come up") on
failure. `install.sh --rollback` restores the images and stack files of the last recorded upgrade. So a
script stack in Wren is:

- a **Compose project** (for discovery, "update available" from image digests, container health and
  data paths), plus
- an **update script** and an optional **rollback script**, both defined in the agent's local config,
  e.g. for Fluxer: download `install.sh` and its `.sha256`, `sha256sum -c`, then
  `sh install.sh --update --allow-root`; rollback `sh install.sh --rollback --allow-root`, plus
- a flag **"the script handles stop, backup and verification itself"**. When set, Wren does **not**
  stop the stack or take its own snapshot before running the script (Fluxer needs Postgres running for
  its dump, and keeps its own backup); Wren still opens the VV window, runs the health check afterwards
  and, on failure, runs the rollback script (or, without one, restores the previous images and the
  working directory's stack files).
- Version shown in the UI: the image digests/tags of the project, or the optional version source.

- **Registries:** Docker Hub, GHCR, Quay, and any OCI registry; credentials per registry, stored
  encrypted. Use `HEAD` on manifests with token auth to spare Docker Hub's rate limits; respect
  `Retry-After`; cache results per image reference within one check run.
- **Tag following (optional per target):** besides "same tag, new digest", a target can follow a
  semver range (e.g. `1.x`), and Wren then **tells you** about the newest matching tag. In v1 Wren
  never edits your Compose files, `.env` files or Unraid templates to move a tag; you change the tag,
  and Wren handles "same tag, new digest" updates. *(Decided by the designer; may be revisited later.)*
- **Version sources for script stacks:** GitHub releases (`owner/repo`), a URL plus a regex or JSON
  pointer, or a command defined on the agent that prints the installed and the latest version.
- **Release notes:** shown with each available update when Wren can find them: the image's
  `org.opencontainers.image.source` label → GitHub releases; for script stacks, the version source.
- **Check interval:** set per server (default every 6 hours), overridable per target; "Check now"
  button. Checks never change anything.

## 5. Policies, approvals and schedules

- **Mode per target:** `notify` | `approve` | `auto`. The server's default mode applies to targets that
  don't override it. **Default: `approve`; `auto` is never on unless the user turns it on.**
- **Exclusions:** a target with mode `notify` or `approve` on a server whose default is `auto` is
  excluded from automatic updates. The UI shows clearly which targets will update on their own.
- **Approvals:** an available update can be approved now (runs immediately), approved for a scheduled
  time, skipped (this version), or snoozed. Several can be approved together.
- **Auto windows:** automatic updates only run inside a window per server (e.g. "nightly 03:00–05:00"),
  never outside it.
- **Scheduled updates** create a **planned maintenance window in Vigilant Vole** ahead of time
  (title, start, expected duration, affected monitors), so it shows on status pages.
- **Locks:** one update at a time per server by default (configurable); a stack updates as one unit.
- **Anything not approved is never executed.** An approval is for one specific version (digest or
  version string); if a newer version appears before it runs, it needs a new approval (in `approve`
  mode).

## 6. The update run

Every run, manual, scheduled or automatic, goes through the same steps and is recorded step by step
in the target's history:

1. **Preflight:** agent online, enough free disk for the snapshot, the new image can be pulled (pull
   first, before anything stops).
2. **Maintenance window** in VV for the target's linked monitors (created via VV's API, or the planned
   window becomes active).
3. **Stop** the target (container or stack) gracefully.
4. **Snapshot** the target's data paths (§7).
5. **Update** (pull/recreate, compose up, or the script).
6. **Health check:** the container(s) are running, Docker's own healthcheck (if any) is healthy, and the
   linked VV monitors are up for a stable period (default 3 minutes; configurable per target). A target
   with no linked monitors relies on the container checks only, and the UI says so.
   **The window stays open during the check** (decided by the owner): Vigilant Vole is extended so that a
   monitor in maintenance keeps checking, records and exposes its *real* result through the API, and
   still shows "maintenance" publicly and sends no alerts. A failed update is therefore never visible
   on status pages. Today VV replaces the result with `maintenance` (`src/monitor/alerting.rs`), so this
   VV change ships before Wren's Phase 5.
7. **On failure → rollback:** stop, restore the previous image/compose files and the snapshot, start,
   re-check health, and notify. If the rollback itself fails, stop, keep everything as it is, and notify
   urgently ("needs you").
8. **Close** the VV maintenance window and send a result notification.

Timeouts for every step; a run never waits forever. A run can be cancelled before step 3.
Script stacks that handle their own stop, backup and verification skip steps 3–4 (see §4).

**Wren, its agents and Vigilant Vole are not updated by Wren in v1** (decided by the owner). Wren runs on
**Tower**. Updating itself would end the run in progress, an agent can't snapshot or roll back itself,
and updating VV removes the maintenance and health source mid-run. For these Wren only **notifies**
about new versions, and the owner updates them by hand. Later, the agent can update the server and the
server can update agents (one at a time, never both).

## 7. Snapshots

- **What:** each target has data paths (detected from bind mounts and volumes; editable). Large paths
  that don't need to be part of a rollback (media, caches, transcodes) can be excluded.
- **How:** if the path is on btrfs or ZFS, take a filesystem snapshot (seconds); otherwise copy with
  reflinks or hard-link-aware copying, falling back to a plain copy. The target is stopped first so
  databases are consistent.
- **Keep:** the last N snapshots per target (default 2), and only while disk space allows; never fill
  the disk (refuse the update with a clear message instead).
- **Not a backup:** documented clearly; the owner's rclone backups stay the offsite copy.

## 8. Security

- **Agents connect out; servers open no ports.** Each agent has its own token (`wwa_…`), revocable.
- **No remote shell:** scripts (and version commands) are defined in the **agent's local config file**
  on the server (e.g. `/mnt/user/appdata/wren-agent/agent.toml`), with name, command, working
  directory, environment and timeout. The Wren server can only ask an agent to run a script **by
  name**; it can never send a command line. The same goes for data paths: the agent only touches paths
  under roots allowed in its local config.
- **Managed settings (0.8):** an admin can set an agent's `[updates]`, roots, retention and script stacks from
  presets compiled into the agent in Wren's UI. Wren sends them as data in the heartbeat answer; the agent
  validates them against hard limits of its own (roots inside its mounts and outside system folders, presets only,
  bounded numbers, no replay), stores them in `managed.json` and reports the version in force. `WW_AGENT_LOCKED=1`
  makes the agent ignore them and use `agent.toml` alone. See docs/agent.md and docs/security.md.
- **Docker socket access is root-equivalent;** the docs say so plainly, and the agent image is minimal.
- **Registry credentials and the VV API token** are stored encrypted with the instance key; masked
  in the UI and API like vole-kit's notification secrets.
- Sign-in, roles (viewer/editor/admin), API tokens with scopes, 2FA, CSRF, CSP from vole-kit. Only
  editors and admins approve or run updates; only admins add agents and change modes to `auto`.
- Every action (approve, run, auto run, rollback) is logged with who/what triggered it.

## 9. Interface

Family design (vole-kit's design system, dark-first). Brand colour: chosen with the logo.

- **Overview:** servers and agents (online/offline), updates available, scheduled updates, recent runs.
- **Updates:** everything with a new version, with release notes, approve / schedule / skip / snooze,
  bulk actions.
- **Targets:** per server; kind, current and available version, mode, linked VV monitors, data paths,
  snapshots, history.
- **Target detail:** versions, release notes, the run history with each step's log, snapshots,
  settings.
- **Schedules:** upcoming updates and auto windows per server; they match VV's planned maintenance.
- **Settings:** servers and agents (add an agent: shows a token and a one-line `docker run`),
  registries, the Vigilant Vole connection (URL + API token, monitor linking by monitor or tag),
  notification channels and rules, check intervals.
- Notifications: new version available (one per version, digest option), run succeeded, failed and
  rolled back, rollback failed, agent offline.
- **REST API v1 and MCP** for everything above (list updates, approve, schedule, run, history), so the
  iOS app (§10) can be built on it. 26 languages; Wren's own terms go in `locales/glossary/`.

## 10. Later: iOS app and push relay

The iOS app (for everyone, on the App Store) shows updates and lets you approve them, with push
notifications. Apple push needs the developer's key, so self-hosted Wren instances will send pushes
through a small relay service run by the owner (like ntfy's upstream). v1 prepares for this: the API
covers every action the app needs, and the notification layer has a slot for a "Wren app" channel.
The app and the relay are not part of v1.

## 11. Delivery

Same as the family: release-please, GHCR images for the server and the agent (amd64, arm64), static
binaries, a Linux installer, Unraid templates for both server and agent, a GitHub Pages site under
`hedgerow.tools/Wise-Wren/`, and a tile on the Hedgerow hub.

## 12. Phases (risk first)

| # | Phase | Done when |
|---|---|---|
| 1 | **Spike on LANBox** (throwaway): recreate an Unraid template container the way Unraid does and keep the Unraid UI in sync; install a throwaway Fluxer on LANBox and run its real `install.sh --update` and `--rollback` from an agent container against the host's Docker (same paths inside and outside the container); btrfs/ZFS snapshot vs copy on the cache pool; registry digest checks against Docker Hub/GHCR within rate limits | each question answered with evidence; the agent's runtime form (container vs host binary, privileges) decided |
| 2 | Skeleton on vole-kit: auth, users, tokens, settings, i18n, layout, servers and agents (agent protocol, registration, online status), Docker, CI | an agent on LANBox shows as online in the family design |
| 3 | Discovery and version checks: containers, Unraid templates, Compose stacks, script stacks with version sources; registries; release notes; notifications for new versions | Wren lists everything on LANBox and Tower with correct "update available" |
| 4 | Update runs: approvals, the run pipeline, snapshots, health checks, rollback, history | an approved update runs, and a deliberately broken one rolls back, on LANBox |
| 5 | Vigilant Vole integration and scheduling (needs the VV release from §6 step 6: real monitor results during maintenance via the API): maintenance windows, planned windows, monitor-based health, auto mode with windows and exclusions | a scheduled update shows as planned maintenance in VV, runs in its window, and a failed update rolls back without showing as down |
| 6 | API + MCP, hardening, docs, site, templates | **v0.1** |

## 13. Audience and platforms

v1 targets the owner's Unraid servers first and is tested there. Other Linux Docker hosts (ext4/xfs,
other Compose layouts) get best-effort support, with the plain-copy snapshot fallback; they are tested
more before v1.0. *(Designer's call.)*

## 14. Left to the builder

- The agent protocol details (long-poll vs WebSocket), reusing Vigilant Vole's agent code where it fits.
- ~~How Compose working directories are found~~ *Settled by the spike:* from the container labels
  `com.docker.compose.project.working_dir` and `…config_files`. The files count as reachable only when
  that path exists at the same path inside the agent. Otherwise the stack is shown as "files not
  reachable: mount <working_dir> into the agent" and is notify-only.
- ~~The exact snapshot method per filesystem~~ *Settled by the spike:* (1) a btrfs **subvolume**
  target with `CAP_SYS_ADMIN` → `btrfs subvolume snapshot -r`, restore by swap; (2) otherwise a
  **reflink copy** (`FICLONE`; btrfs, xfs `reflink=1`) on the real pool/disk path, which takes
  0.1–0.2 s for 2 GB with no capability needed; (3) otherwise a plain hard-link-preserving copy
  after a free-space check; ZFS is treated as (3) until tested. `/mnt/user/…` (shfs) cannot reflink,
  so the agent resolves it to `/mnt/cache|<pool>|diskN/…` first (`getfattr -n system.LOCATIONS`).
  Tower's `appdata` is not a subvolume, so Tower uses reflink copies.
- *Settled by the spike:* the agent's runtime form (§3), the Unraid template update method (Unraid's
  `update_container`, verified by Wren afterwards; fallback: an API recreate that drops the old
  image's defaults and writes Unraid's status entry), and the registry check (anonymous index-first
  manifest `HEAD`, compared with the running image's `RepoDigests`).
- Data model and table names.
