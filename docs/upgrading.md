# Upgrading

Wren doesn't update itself or its agents yet: their targets show new versions but can't be approved.

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

## Agents

Recreate each agent
 with the new image (Unraid: "Check for updates" on the agent's container). The agent reports
its version with every heartbeat; the server's page shows it. Keep the agents on the same version as the server.

## Pinning

`:latest` follows releases, `:0.2` and `:0` follow a minor or major line, `:0.2.1` (an exact version) never moves, `:edge` is the newest `main`
build (for testing only).
