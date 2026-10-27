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
- `[updates] allow` / `deny` in `agent.toml` decides which targets may be updated at all, whatever Wren asks
  (a script stack's Compose project counts too, and a plain Compose job can't go round a script stack).
- The agent never updates its own container, and a rollback job's digests must be digests.
- The agent connects out; the host opens no ports. It doesn't follow redirects, reads at most 64 to 256 KiB of any
  answer from Wren, and starts none of its child processes (scripts, Docker, `cp`) with its token or Wren's
  address in their environment, so a vendor installer a script stack downloads can't take them. Prefer `https://`
  for the server address; the agent warns about plain `http://` beyond the local network.

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
- The **Vigilant Vole token** belongs to its URL: changing the URL needs the token typed again (like a
  notification channel's destination), so a saved token can't be pointed at another address. **Test connection**
  shows a status code, never what the other end answered.

## The server

- Sign-in with roles (viewer, editor, admin), two-factor sign-in, CSRF protection on every form, a strict Content
  Security Policy and the usual security headers, and limits on failed sign-ins (from the family kit, shared with
  Vigilant Vole). Only editors and admins approve or run updates; only admins add agents, change registries,
  manage **notification channels** (a channel makes the server send requests to an address of its own choosing,
  which editors may not do) and the
  VV connection, and turn on automatic updates. Every action is logged with who did it.
- Passkeys (WebAuthn) sign in without a password or code; each user adds them under **Settings → Security**.
  They need an HTTPS public URL (`WW_PUBLIC_URL`, or `http://localhost`), which sets their relying party; the
  password (with two-factor sign-in) remains the fallback.
- Version URLs from `agent.toml` are fetched by the server from **public addresses only** (every address the name
  resolves to, and every redirect, is checked; at most 256 KB in 20 seconds; only the version is kept).
- **Registry checks** use image references that agents report, so they follow the same rules. A reference must be
  well-formed (a host of letters, digits, `.` and `-`, a lowercase repository without `..`, a tag of at most 128
  safe characters); anything else is "unknown" and sends no request. Plain `http://` is used only for `localhost`
  and `127.0.0.1`. Requests go to a checked address (DNS pinned, no proxy) and **redirects are not followed**.
  A registry on a private, loopback or link-local address is asked only if it has an entry on the **Registries**
  page (Settings → Registries; credentials can be a placeholder), so an agent can't aim Wren at the internal
  network by naming a host in an image. A registry's token service (`WWW-Authenticate: Bearer realm=…`) must use
  HTTPS and belong to the registry: the same host, the same registrable domain (`ghcr.io`, `auth.docker.io` for
  `registry-1.docker.io`; the last two labels, three under `co.uk`-style names), or a pair on a short list
  (`lscr.io` → `ghcr.io`). Token answers are cut off at 64 KiB and a token is replayed only if it is made of
  URL-safe or base64 characters (at most 8 KiB); a digest must be `sha256:` and 64 lowercase hex digits.
- **Inventory limits.** An inventory over 2000 containers or targets, 50 images in one target, 100 labels on a
  container, or with an image reference over 500 characters or any other text far over a sane length is refused
  as a whole (HTTP 400), not cut down. A registry lookup has 20 seconds; a check run spends at most 10 minutes
  on lookups and leaves the rest for the next run (a host that timed out is skipped for the rest of the run).
  Registry answers no target refers to are forgotten after each run.
- Agents are limited to 20 heartbeats, 6 inventories and 120 job requests a minute per server.
- Keep Wren off the open internet when you can. When agents must reach it from elsewhere, use HTTPS, set
  `WW_TRUST_PROXY=1` only behind your own proxy, use `WW_SETUP_TOKEN` until the first admin exists, and turn on
  two-factor sign-in.

Found a security problem? Please report it privately to the maintainer rather than in a public issue.
