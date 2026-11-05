# Upgrading

Wren doesn't update itself yet: update the server by hand (below). From 0.10 it can update its own agents where the
host's owner allows it ([Agent self-update](agent.md#agent-self-update)); agents before 0.10 are updated by hand.

## The server

1. Note the version you run (`info.version` in `/api/v1/openapi.json`, or the image tag).
2. Read the [release notes](https://github.com/Ferdinand99/Wise-Wren/releases) between your version and the new
   one.
3. Pull and recreate:
   - Compose: `docker compose pull && docker compose up -d`
   - Unraid: Docker tab → "Check for updates" → apply
   - `docker run`: `docker pull ghcr.io/ferdinand99/wise-wren:latest`, then remove and start the container with
     the same options.

Database migrations run at start. Before any migration Wren copies the database to `backups/wren-before-…` in the
data folder. To go back to an older version, stop Wren, put that copy back as `wren.sqlite` and start the older
image.

Upgrade while no update run is in progress (the Overview shows running runs). A run that loses Wren half-way keeps
going on its agent, but Wren may miss its result and close it as "rollback failed" after 10 minutes of silence;
then look at the target yourself. Restarting an agent (a new image, an edited `agent.toml`) ends its running run:
from 0.2.1 Wren closes it as "rollback failed" at the agent's next heartbeat and goes on with the next approved
update.

### To 0.5

Notification channel secrets are sealed with the instance key (`session.key` in the data folder) at the first
start; the copy in `backups/wren-before-…` keeps them in plaintext for going back. A database copied without
`session.key` loses the channel secrets: enter them again on each channel's edit page.

### To 0.7

Server and agent change together; recreate both. **Before you upgrade an agent**, add this to its `agent.toml`
(`/mnt/user/appdata/wren-agent/agent.toml` on Unraid; create the file if you only used `WW_SERVER_URL` and
`WW_AGENT_TOKEN`), or the upgraded agent refuses every update and rollback:

```toml
[updates]
allow = ["*"]        # or a list of the names you want updated
```

Also check these, or the agent won't start or will refuse runs:

- `server` must be `https://`, unless the host is on your own network (private or loopback address, a single-label
  name like `tower`, a name ending in `.local`, `.lan`, `.home.arpa`, `.internal` or `.ts.net`, or a Tailscale address). `wren-agent --check` says
  which.
- The agent needs `/config` writable: it keeps `runs.json` there (its record of what it ran). Updates made before
  the upgrade have no record, so Wren can't roll them back.
- New optional `[updates] require_snapshot = true` makes every update need a snapshot (default `false`, as before).
- A health period shorter than 30 s is raised to 30 s (`min_health_seconds`), and a script stack's update runs at
  most once an hour (`min_interval` on the `[[script_stack]]`).

An older agent works with the new server, and a new agent with an older server; the new refusals just show in
English there. On a server in `auto` mode, targets Wren has never seen start on `approve` until an admin allows
automatic updates for them (Targets card → **Allow automatic updates**).

### To 0.8

Server and agent change together; recreate both. **Agent settings are now managed in Wren** (server page → **Agent
settings**, see [Managed settings](agent.md#managed-settings)):

- The first time an upgraded agent connects, Wren imports its `agent.toml` as version 1 of the settings and offers the
  same values back: nothing changes on the agent, nothing is lost. From then on the page, not the file, decides the
  agent's `[updates]`, `roots`, `[retention]` and Fluxer-like script stacks. Script stacks with commands of their own,
  `server` and `token` stay in `agent.toml`.
- Roots must be inside folders mounted into the agent (the same-path rule), and the agent reports its mounts. Roots
  already in `agent.toml` stay valid; new ones are chosen from the mounts.
- To keep deciding on the host, set `WW_AGENT_LOCKED=1` on the agent **before** you upgrade it (or right after: a
  locked agent ignores the settings and a `managed.json`). The agent then behaves exactly as in 0.7.
- Wren shows an agent before 0.8 as "can't take settings from Wren" and keeps the agent.toml helper for it. An older
  agent works with the new server, and a new agent with an older server (it just never gets settings).
- The Fluxer preset downloads the installer into a temporary folder inside the stack's folder instead of leaving
  `install.sh` there. A stack in `agent.toml` with the commands from the 0.7 docs is recognised as the preset when
  imported and keeps working.

### The release that adds the Hedgerow app channel

The server records `kit/0006_push.sql`: three new, empty tables (`push_devices`, `push_events`,
`push_registrations`), and nothing is pushed until an admin adds the Hedgerow app channel. The upgrade takes the usual
copy first. The release before it refuses a database this one has started on; to go back, restore that copy. Agents
are not affected.

The same release renames Wren's internal slug from `wren` to `wise-wren` (the push relay knows the products by their full
names). It shows up as the MCP server name, the API reference download names and the source or tag Keep, ClickSend and
Opsgenie receive: update a rule that matched `wren`.

### To 0.10

Wren can now update its own agents (see [Agent self-update](agent.md#agent-self-update)). Server and agent change
together; recreate both **by hand this time**: an agent before 0.10.0 can't update itself, so this is the last
manual agent update.

- **The server** adds three columns to the servers table at the first start, after the usual copy in
  `backups/wren-before-…`. The release before 0.10 refuses a database this one has started on; to go back, restore
  that copy.
- **The agent**: when you recreate it, add `WW_AGENT_SELF_UPDATE=1` to its environment (Unraid: add a variable to the
  container) if you want Wren to update it from now on. It is off otherwise, and Wren can't switch it on. The
  alternative is `[self_update] enabled = true` in `agent.toml`, but an agent before 0.10 refuses to start on that
  section, so the variable is the better choice if you may need to go back.
- **Keep the agent's image tag at `:latest` or `:0`** (`:0.10` follows 0.10.x only). Wren never moves a tag that
  promises a version: an exact tag such as `:0.10.0` or a digest pins one, the card says so and the agent
  isn't offered the update. Compose files and Unraid templates need the same.
- **The agent's config folder** (`/config`) must be a writable mount from the host: the agent keeps its update state
  there. Mounted read-only, the agent shows why it can't update itself.
- **The first real self-update is 0.10.0 → 0.10.1.** An agent is only ever offered Wren's own version, so update
  Wren first, then approve the agent on the server's page (**Agent version**). Update agents one at a time, and don't
  update Wren while an agent update runs.
- On the server's page the agent card says that self-update is off on that server until the variable is set, and
  that an older agent has to be updated by hand once (agents before 0.10).
- The release images are signed with cosign from this release; the agent doesn't verify the signature yet
  ([Security notes](security.md#agent-self-update)).

## Agents

Update the server first, then the agents. The agent reports its version with every heartbeat; the server's page
shows it. Keep the agents on the same version as the server.

- **An agent from 0.10 with self-update switched on** (`WW_AGENT_SELF_UPDATE=1` or `[self_update]`, see
  [To 0.10](#to-010)): approve the update on the server's page (**Agent version**, now or at a set time). Wren offers
  its own version, and the agent replaces itself and rolls back if the new one can't reach Wren. Don't update Wren
  while this runs.
- **Any other agent** (before 0.10, or self-update off): recreate it with the new image (Unraid: "Check for updates"
  on the agent's container; Compose: `docker compose pull && docker compose up -d`; `docker run`: pull, remove and
  start with the same options).

Managed settings and the run ledger live in the agent's config folder, so they survive an update either way.

## Pinning

`:latest` follows releases, `:0.2` and `:0` follow a minor or major line, `:0.2.1` (an exact version) never moves, `:edge` is the newest `main`
build (for testing only). An agent that Wren should update itself has to run `:latest`, `:0` or `:0.10` (the
minor line follows patch releases only): an exact version or a digest pins it, and Wren refuses to move it.
