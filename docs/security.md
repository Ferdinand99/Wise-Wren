# Security notes

## The agent is root on its host

The agent talks to the host's Docker socket. **Access to the Docker socket is access to the host as root**: anyone
who controls the agent can start a privileged container. The agent also runs as root inside its container (it
must, to restore snapshots with their owners and to run Unraid's own scripts). It is not privileged and has no
added capabilities, but treat it as root anyway, and treat its token like a root password.

What limits the damage if the **Wren server** is compromised (design: Wren must never become a remote shell):

- Wren can't send a command line. A job is data: a target's kind and name, version keys and image digests. Script
  stacks' commands live only in the agent's own `agent.toml`, and Wren can only ask for them by name.
- The agent re-checks every job against Docker: the target exists, each image is the one that container runs now,
  at the digest Wren saw, and the new one is a digest of the **same repository**. It never pulls an image a target
  doesn't already run, and it only touches folders under its `roots` (no `..`, never `/`).
- `[updates] allow` / `deny` in `agent.toml` decides which targets may be updated at all, whatever Wren asks.
- The agent connects out; the host opens no ports.

## Tokens and secrets

- **Agent tokens** (`wwa_…`): one per server, shown once, stored as SHA-256 and looked up by that hash. A new token
  or a revoke takes effect on the agent's next request, also for an agent already waiting for a job.
- **API tokens** (`ww_…`, Settings → API tokens): read or write, capped at their user's role. Writes through the
  REST API need a write token; a browser session cookie is never enough there. MCP gets the same rules, and nothing
  that would put a secret into an assistant's context (agent tokens, registry passwords, the VV token, notification
  channel settings).
- **Registry passwords and the Vigilant Vole token** are sealed with AES-256-GCM under a key derived from the
  instance's `session.key` in the data folder, and never shown again in pages, API answers, logs or notifications.
- **Notification channel secrets** (tokens, passwords, webhook URLs with a key, headers) are sealed at rest
  since 0.5 with the same instance key (`session.key` in the data folder, through vole-kit's sealed secrets) and
  masked in pages and API answers. Channels saved by an older version are sealed at the first start.
- Back up the data folder as a whole: a copy of the database without `session.key` loses these secrets. They
  can't be opened and must be entered again (the channel's edit page says so; nothing is sent until then).
- Registry credentials go to a registry's token service only over HTTPS (or to the registry's own address).

## The server

- Sign-in with roles (viewer, editor, admin), two-factor sign-in, CSRF protection on every form, a strict Content
  Security Policy and the usual security headers, and limits on failed sign-ins (from the family kit, shared with
  Vigilant Vole). Only editors and admins approve or run updates; only admins add agents, change registries and the
  VV connection, and turn on automatic updates. Every action is logged with who did it.
- Passkeys (WebAuthn) sign in without a password or code; each user adds them under **Settings → Security**.
  They need an HTTPS public URL (`WW_PUBLIC_URL`, or `http://localhost`), which sets their relying party; the
  password (with two-factor sign-in) remains the fallback.
- Version URLs from `agent.toml` are fetched by the server from **public addresses only** (every address the name
  resolves to, and every redirect, is checked; at most 256 KB in 20 seconds; only the version is kept).
- Agents are limited to 20 heartbeats, 6 inventories and 120 job requests a minute per server.
- Keep Wren off the open internet when you can. When agents must reach it from elsewhere, use HTTPS, set
  `WW_TRUST_PROXY=1` only behind your own proxy, use `WW_SETUP_TOKEN` until the first admin exists, and turn on
  two-factor sign-in.

Found a security problem? Please report it privately to the maintainer rather than in a public issue.
