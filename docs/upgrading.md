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
  name like `tower`, or a name ending in `.local`, `.lan`, `.home.arpa`, `.internal`). `wren-agent --check` says
  which.
- The agent needs `/config` writable: it keeps `runs.json` there (its record of what it ran). Updates made before
  the upgrade have no record, so Wren can't roll them back.
- With `roots` set, every update now requires a snapshot (`[updates] require_snapshot`, default on): a target with
  no data folder under `roots` stops before anything is touched. Set `require_snapshot = false` under `[updates]`
  to keep the old behaviour.
- A health period shorter than 30 s is raised to 30 s (`min_health_seconds`), and a script stack's update runs at
  most once an hour (`min_interval` on the `[[script_stack]]`).

An older agent works with the new server, and a new agent with an older server; the new refusals just show in
English there. On a server in `auto` mode, targets Wren has never seen start on `approve` until an admin allows
automatic updates for them (Targets card → **Allow automatic updates**).

## Agents

Recreate each agent
 with the new image (Unraid: "Check for updates" on the agent's container). The agent reports
its version with every heartbeat; the server's page shows it. Keep the agents on the same version as the server.

## Pinning

`:latest` follows releases, `:0.2` and `:0` follow a minor or major line, `:0.2.1` (an exact version) never moves, `:edge` is the newest `main`
build (for testing only).
