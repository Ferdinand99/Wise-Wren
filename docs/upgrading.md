# Upgrading

Wren doesn't update itself or its agents in v0.1: their targets show new versions but can't be approved.

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

## Agents

Recreate each agent with the new image (Unraid: "Check for updates" on the agent's container). The agent reports
its version with every heartbeat; the server's page shows it. Keep the agents on the same version as the server.

## Pinning

`:latest` follows releases, `:0.1` and `:0` follow a line, `:0.1.0` never moves, `:edge` is the newest `main`
build (for testing only).
