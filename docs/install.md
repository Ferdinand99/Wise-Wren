# Installing Wise Wren

Wren has two parts:

- **the server** (`ghcr.io/ferdinand99/wise-wren`): the web UI, the API and the database, once per household;
- **an agent** (`ghcr.io/ferdinand99/wise-wren-agent`) on every Docker host Wren looks after, including the one the
  server runs on. Adding agents is in [Adding an agent](agent.md).

Wren is an early release: both are Docker images for amd64 and arm64. Static binaries and a Linux installer come
later. Wren is tested on Unraid 7 (the owner's two servers); other Linux Docker hosts are best-effort for now.

## Docker Compose

```bash
mkdir wise-wren && cd wise-wren
curl -fsSLO https://raw.githubusercontent.com/Ferdinand99/Wise-Wren/main/docker-compose.yml
docker compose up -d
```

Open `http://<host>:3003/setup` and create the first admin. Until the first admin exists, `/setup` asks for a setup
token: set `WW_SETUP_TOKEN`, or read the one Wren makes up and logs once at the first start
(`docker compose logs wise-wren`, a line with `/setup?token=…`). The locked page also has a field for it. Everything
lives in `./data` (`wren.sqlite`, the session key that also seals registry passwords, the Vigilant Vole token and
your users' two-factor secrets, and pre-migration backups).

## Unraid

Install **Wise Wren** from Community Applications (template `wise-wren.xml` in
[Ferdinand99/unraid-templates](https://github.com/Ferdinand99/unraid-templates)), or add the template URL
`https://raw.githubusercontent.com/Ferdinand99/unraid-templates/main/templates/wise-wren.xml` under
Docker → Add Container → Template repositories. Data goes to `/mnt/user/appdata/wise-wren`; PUID/PGID default to
Unraid's `99:100`. The agent has its own template, **Wise Wren Agent** (see [Adding an agent](agent.md)).

## Plain Linux

Any host with Docker 23 or later:

```bash
docker run -d --name wise-wren --restart unless-stopped \
  -p 3003:3003 -v /opt/wise-wren:/data \
  -e WW_PUBLIC_URL=http://192.168.1.10:3003 \
  ghcr.io/ferdinand99/wise-wren:latest
```

## Settings at start

| Variable | Default | What |
|---|---|---|
| `WW_PUBLIC_URL` | (none) | The address agents and people use. Stored as Settings → General → public URL at every start; it goes into the agents' `docker run` lines and notification links. |
| `WW_TRUST_PROXY` | off | `1` behind a reverse proxy that sets `X-Forwarded-*` (Caddy, Traefik, nginx, SWAG, Pangolin). |
| `WW_SETUP_TOKEN` | (random, logged at the first start) | `/setup` only opens with `?token=<value>` until the first admin exists, so nobody else can create it. |
| `WW_SETUP_OPEN` | off | `1`: `/setup` needs no token (scripted installs). |
| `WW_ALLOW_PRIVATE_VERSION_URLS` | off | `1` lets script stacks' `url` version sources reach private and local addresses (a version file on your LAN). See [the agent guide](agent.md). |
| `PUID` / `PGID` | 1000 / 1000 | The user the app runs as (Unraid: 99 / 100). The data folder is handed to it at start. |
| `TZ` | UTC | Time zone of log timestamps (pages use Settings → General → time zone). |
| `RUST_LOG` | `info` | Log filter, e.g. `info,wise_wren=debug`. |
| `WW_PORT`, `WW_HOST`, `WW_DATA_DIR` | `3003`, `0.0.0.0`, `/data` | Listener and data folder. |

## Reaching Wren

Agents connect **out** to Wren over HTTP(S); the hosts they run on open no ports. Keep Wren on your LAN or behind
a VPN when you can. If agents must reach it over the internet, put it behind HTTPS with `WW_TRUST_PROXY=1`, set
`WW_PUBLIC_URL` to the HTTPS address, and turn on two-factor sign-in for every admin (Settings → Security).

## After the first sign-in

1. Settings → General: site name, public URL and time zone (auto windows and scheduled times use it).
2. Servers → Add server: Wren shows the agent token once with the `docker run` line. Start the agent
   ([Adding an agent](agent.md)); the server turns **online** within 30 seconds.
3. Settings → Notifications: add a channel (ntfy, email, Discord and ~40 more) for new versions and run results.
4. Optional: Settings → Registries for private images or Docker Hub's rate limit, and Settings → Vigilant Vole
   ([Vigilant Vole](vigilant-vole.md)).
5. Read [Updates, approvals and schedules](updates.md) before you approve the first update, and
   [Snapshots and rollback](snapshots.md) for what a rollback can and can't undo.

The REST API and MCP: Settings → API tokens, and the API reference linked there (`/api-docs`, OpenAPI at
`/api/v1/openapi.json`, MCP at `/mcp`).

## Upgrading from Wren 0.7.0 (vole-kit 0.6)

- The first start seals your users' two-factor secrets in the database with the instance key (`session.key`),
  after taking a copy of the database in `backups/` (with a `.plaintext-secrets` marker next to it). That copy is
  deleted automatically once the database holds no plaintext secret, or after 7 days. The previous release can't
  open a database the new one has started on: **to go back, restore that copy**. Keep `session.key` with your
  backups, or the secrets can't be read.
- Creating a write or never-expiring API token, adding or promoting an admin, an admin's password or two-factor
  reset and deleting a passkey ask for your password. New API tokens default to 90 days.
- Changing your password or an admin's password reset offers "Also revoke API tokens" (on by default).
- A fresh instance asks for a setup token at `/setup` (see above). Instances that already have users are unchanged.
