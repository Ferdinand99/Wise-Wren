# Security notes

## The agent is root on its host

The agent talks to the host's Docker socket. **Access to the Docker socket is access to the host as root**: anyone
who controls the agent can start a privileged container. The agent also runs as root inside its container (it
must, to restore snapshots with their owners and to run Unraid's own scripts). It is not privileged and has no
added capabilities, but treat it as root anyway, and treat its token like a root password.

Who decides what the agent may touch depends on one switch:

- **Unlocked (the default, since 0.8):** Wren's admins decide, through the settings page, **within hard limits the
  agent enforces itself** (below). The agent applies what Wren sends only after it passed those limits and keeps
  the result in `managed.json`.
- **Locked (`WW_AGENT_LOCKED=1`):** only the agent's local `agent.toml` decides, as before 0.8. The agent ignores
  managed settings completely, also a `managed.json` it finds, and tells Wren that it is locked. Choose this where the
  people with access to the host must be the ones who decide, not whoever can sign in to Wren as an admin.

The trade-off, in plain words: with managed settings, **an attacker who controls Wren (or an admin account) can widen
what an unlocked agent may do**, up to the hard limits: allow every target, lower the safety minimums, add a root
under any folder mounted into the agent, or start a preset script stack. They still can't make the agent run a command
of their choosing, touch system folders, switch on its self-update or reach a folder that isn't mounted into it. Wren makes this
visible (every change is in the history, and changes that widen access notify all notification channels), and
locking removes the exposure. Mount only the folders Wren really needs into the agent: the mounts are the outer limit.

What limits the damage if the **Wren server** is compromised (design: Wren must never become a remote shell):

- Wren can't send a command line. A job is data: a target's kind and name, version keys and image digests. Script
  stacks' commands live only in the agent (compiled-in presets, or its own `agent.toml`), and Wren can only ask for
  them by name. Managed settings are data too: a closed set of fields, and a field the agent doesn't know makes it
  refuse the whole update.
- **Hard limits on managed settings.** The agent refuses a whole update (and keeps what it had) when a root isn't
  inside a folder mounted into the agent, is `/`, is under `/boot` (but Unraid's Compose Manager folder), `/etc`,
  `/proc`, `/sys`, `/dev`, `/var/lib/docker`, `/var/run`, `/run`, `/usr`, `/bin`, `/sbin`, `/lib*` or `/root`, or is
  the agent's config folder; when a script stack names a preset it doesn't have, or a folder outside the roots; when a
  number is out of the bounds `agent.toml` has; or when the version is older than the one in force. A compromised
  server can't send a command line, can't make the agent touch system paths, and can't switch on the agent's self-update.
- The agent re-checks every job against Docker: the target exists, each image is the one that container runs now,
  at the digest Wren saw, and the new one is a digest of the **same repository**. It never pulls an image a target
  doesn't already run, and it only touches folders under its `roots` (no `..`, never `/`).
- `[updates] allow` / `deny` (in `agent.toml`, or the managed settings of an unlocked agent) decides which targets
  may be updated at all, whatever a job asks
  (a script stack's Compose project counts too, and a plain Compose job can't go round a script stack). The list
  is **required**: an agent without an `allow` list refuses every update and rollback.
- The agent keeps its own record of what it ran (the run ledger, `runs.json` beside `agent.toml`). A run id must
  be newer than any it already ran for the target, a rollback is accepted only once and only for the newest
  successful update **this agent** ran, and a script stack's update runs at most once per `min_interval`
  (default an hour). A compromised Wren can't replay old runs, roll back an update that never happened or start
  a vendor installer in a loop.
- Wren can ask for more safety, never less: the agent raises a job's health period to `min_health_seconds`
  (default 30 s) and requires a snapshot whenever `require_snapshot` is on (off by default: stateless targets
  have nothing to snapshot).
- After a restore, the data it replaced is kept next to it (`<folder>.wren-aside-<run>`) until the target's next
  run, so a wrong rollback can be undone.
- An ordinary job never touches the agent's own container, and a rollback job's digests must be digests. The only
  way the agent replaces itself is [self-update](#agent-self-update), below.
- A script's or command's output is held to a fixed size, and a timeout kills the whole process group the command
  started, not only its shell. Before a step's log goes to Wren the agent hides what looks like a secret: the value
  of a `password=`, `token:` or `"api_key": "…"` pair, a `Bearer` or `Basic` credential, the password in
  `scheme://user:password@host` and a private key block. This is pattern-based and best effort; a secret printed on
  its own line, with no name in front of it, is not recognised, so don't run installers that print one.
- Folders are checked against the real paths too: a symlink under a root that points out of it doesn't make what is
  behind it part of the root.
- **A tag can be shared.** An update re-tags an image reference (`app:latest`) and the agent moves the target's
  own containers to the new digest. Another container on the host that runs the same reference but is outside the
  target (not part of the Compose project or the container Wren updates) keeps running its old image until someone
  recreates it; the next time it is recreated it picks up the new image. Give targets their own references if that
  matters.
- The agent connects out; the host opens no ports. It doesn't follow redirects, reads at most 64 to 256 KiB of any
  answer from Wren, and starts none of its child processes (scripts, Docker, `cp`) with its token or Wren's
  address in their environment, so a vendor installer a script stack downloads can't take them. The server
  address must be `https://` unless the host is on your own network (loopback, private or link-local address,
  a single-label name, `.local`, `.lan`, `.home.arpa`, `.internal`, `.ts.net` or a Tailscale 100.64.0.0/10 address): the agent refuses to start otherwise.

## Agent self-update

Since 0.10 an agent can replace itself with a newer release (see [Agent self-update](agent.md#agent-self-update)).
It is off until the host's owner switches it on locally (`WW_AGENT_SELF_UPDATE=1` or `[self_update] enabled = true`),
and Wren has no way to do that: the setting isn't part of the managed settings, and a settings update that carries
it is refused whole.

**What a compromised Wren can do with it:** only choose *when* an agent that its owner has allowed goes to a newer
official release, and only to one that GHCR publishes under a release tag. It can't pick an image, a repository or a
command, can't switch the feature on, can't move an agent to the same or an older version and can't replay an old
run. (Starting an update takes an admin's approval, so a compromised admin account can do the same; the approval is
the server-side control.)

The agent checks all of this itself, before anything is stopped or changed:

1. The local opt-in is on.
2. The agent can find and read its own container, and that container can be recreated safely: not started with
   `--rm`, not sharing another container's network, with a writable config folder.
3. The container runs the **official repository**, `ghcr.io/ferdinand99/wise-wren-agent`. That is a constant in the
   agent, changed only by a build, never at runtime.
4. The tag follows releases (`:latest`, `:X` or `:X.Y`), also for the target version. Wren never moves a tag that
   promises a version, so an exact tag or a digest is refused.
5. The version is a plain release (`X.Y.Z`, no pre-release) and newer than the agent's own, and the digest is a
   `sha256:` digest.
6. The agent pulls that digest itself, and the image's version label must equal the version.
7. The release tag (`:X.Y.Z`) must point at the same digest, so a server can only choose digests that GHCR currently
   publishes as a release.
8. The run id has to be newer than any the agent already ran for itself (the run ledger).

After that a helper made from the old image replaces the agent, and the old agent is kept, renamed, until the new
one has reached Wren. A new agent that can't talk to Wren is rolled back. The helper has the Docker socket and the
config folder, no network and none of the agent's secrets.

**There is no signature check yet.** The release images (the version tags, not `:edge`) are signed with cosign from
0.10.0, keyless, with the GitHub OIDC identity of this repository's Docker images workflow
(`.github/workflows/docker-publish.yml`), so you can verify them by hand with `cosign verify`. The agent doesn't check
the signature: today the checks above end in trusting what the maintainer's GitHub account publishes under a release
tag. Verification in the agent may follow later.

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
  Vigilant Vole). Only editors and admins approve or run updates; only admins add agents, **change an agent's settings**, change registries,
  manage **notification channels** (a channel makes the server send requests to an address of its own choosing,
  which editors may not do) and the
  VV connection, and turn on automatic updates. Every action is logged with who did it.
- Since 0.7.1 (vole-kit 0.6.0): a fresh instance needs a setup token at `/setup` (`WW_SETUP_TOKEN`, or the random
  one logged at first start); creating a write or never-expiring API token, adding or promoting an admin, resetting
  another user's password or two-factor sign-in and deleting a passkey ask for the password again; two-factor
  secrets are sealed at rest; and session-cookie writes to `/api` must come from the same origin.
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
- **A target's settings outlive an inventory that misses it.** A target that drops out of an inventory (a
  container re-created, a wrong scan, a compromised agent leaving one out and adding it back) keeps its mode,
  health period, requirements, skip and snooze and Vigilant Vole links for 30 days; they return with it. A target
  **never seen before** on a server in `auto` mode starts on `approve` and shows a "New: approve first" badge until
  an admin chooses **Allow automatic updates** for it (or for all new targets of the server), so a new container
  doesn't update itself unreviewed.
- Agents are limited to 20 heartbeats
, 6 inventories and 120 job requests a minute per server.
- Keep Wren off the open internet when you can. When agents must reach it from elsewhere, use HTTPS, set
  `WW_TRUST_PROXY=1` only behind your own proxy, use the setup token (`WW_SETUP_TOKEN`, or the random one logged at the first start; `WW_SETUP_OPEN=1` turns it off) until the first admin exists, and turn on
  two-factor sign-in.

A record of the October 2026 security review (what was found, what was fixed, what is still open) is in
[security-review-2026-10.md](security-review-2026-10.md).

Found a security problem? Please report it privately to the maintainer rather than in a public issue.
