# Security review, October 2026

Date: 27 October 2026. Covers Wise Wren (server and agent) up to the 0.7.1 release.

## Method and limits

This was a read-only code review done by an AI reviewer, one pass for the agent and its protocol and one for the
server. It was not an external audit and it was not done by a professional security firm. The reviewer read the code
and traced every candidate finding to the lines that cause it, tried to refute it, and rated it. Each fix was written
with tests and merged after review. Nothing was run against a live system: there was no dynamic testing, fuzzing,
or scanner run (no CodeQL or Semgrep). Scenarios that depend on timing or on third-party behaviour (for example
registry answers) come from reading the code and were not reproduced end to end.

Treat this as one careful review, not as a guarantee.

## Scope

- **Agent and protocol**, reviewed on `main` at `aa9a913` (agent 0.6.0): all of `agent/src`, `src/agent_api.rs`,
  `src/agent_toml.rs`, the job builder and the agent endpoints in `src/runs.rs`, and the docs that describe the
  agent.
- **Server**, reviewed on `main` at about the same commit: the web, REST, MCP and agent-facing routes, inventory
  and target ingestion, registry and version checks, Vigilant Vole integration, notification channels, secrets
  handling, templates, migrations and the Docker setup. From vole-kit only the places where Wren's choices
  interact with it were read.

The threat model for the agent is a compromised or malicious Wren server, a stolen agent token, and a local
attacker who shares the appdata tree. The central design claim held up: a job is data, the server cannot make the
agent run a command line, and every path the agent touches comes from local sources.

No Critical or High finding was reported for either component.

## Findings

Status: "Fixed in #N" means merged and released. All fixes shipped in 0.7.0 (#33, #35, #36) unless noted; vole-kit
related changes shipped in 0.7.1 (#37, #40).

### Agent and protocol

| ID | Severity | Title | Status |
|---|---|---|---|
| WA-01 | Medium | Plain `http://` server URL accepted; token and jobs in clear | Fixed in #33 (warning), #36 (config error unless the host is on a local network) |
| WA-02 | Medium | Agent token inherited by every child process | Fixed in #33 |
| WA-03 | Medium | Rollback job needs no local proof that the agent ran the update | Fixed in #36 (run ledger) |
| WA-04 | Low | Script stack update and check run on demand, with no pending update | Fixed in #36 (ledger, `min_interval`) |
| WA-05 | Medium (local attacker) | Snapshot probe files and destination follow planted symlinks | Fixed in #33 |
| WA-06 | Low-Medium (local attacker) | Roots are enforced lexically, no realpath | Open, planned |
| WA-07 | Low | Server responses read without a size limit | Fixed in #33 |
| WA-08 | Low | Server-chosen `run_id` can collide with existing rollback points | Fixed in #36 (ledger) |
| WA-09 | Low | Reflink falls back to a full copy without re-checking space | Open |
| WA-10 | Low | Unraid `update_container` pulls the tag again before `verify` | Open |
| WA-11 | Medium (defaults) | Allow-all is the default; the agent can update its own container | Fixed in #33 (own container refused), #36 (`[updates]` allow list required) |
| WA-12 | Low | Allow/deny keyed on the target name only | Fixed in #33 |
| WA-13 | Low | `images[].from` of a rollback job not validated | Fixed in #33 |
| WA-14 | Low | Step logs and inventory can carry secrets and host details | Open, planned (output redaction) |
| WA-15 | Low | The server can trigger inventories and version commands continuously | Open |
| WA-16 | Low | The server controls the safety nets around an update | Fixed in #36 for the health period and the snapshot requirement; see below for the digest choice |
| WA-17 | Low | Timeouts do not kill process trees; output buffered unbounded | Open, planned (process-tree kill) |
| WA-18 | Low | agent.toml helper: server name unescaped in a comment; GET parameters prefill commands | Partly fixed in #33 (control characters rejected and stripped); the helper is still a GET page. Open |
| WA-19 | Info | Redirects and environment proxies on the agent's HTTP clients | Fixed in #33 (redirects off, URL userinfo rejected). The environment proxy setting was not changed |
| WA-20 | Info | Log forging with server-supplied strings | Fixed in #33 |
| WA-21 | Info | Blast radius of the documented mounts and container options | Open, planned (narrower mounts) |
| WA-22 | Info | Re-tagging a shared reference affects containers outside the target | Open (not yet documented) |
| WA-23 | Info | Background tasks die silently on panic | Open |

### Server

| ID | Severity | Title | Status |
|---|---|---|---|
| WS-01 | Medium | SSRF from agent-supplied image references (registry host, token realm, redirects) | Fixed in #35 |
| WS-02 | Medium | One agent can stall version checks for all servers and grow the database | Fixed in #35 |
| WS-03 | Medium | Editors could make the server send arbitrary requests through notification channels | Fixed in #35 (channels are admin-only); kit-level address guard in vole-kit 0.5.1 (#37, 0.7.1) |
| WS-04 | Low-Medium | A target's settings vanish when it drops out of an inventory; the comeback inherits `auto` | Fixed in #36 |
| WS-05 | Low-Medium | MCP hands agent-controlled text to assistants with write tools; destructive tools marked non-destructive | Fixed in #35 |
| WS-06 | Low | A running agent can re-trigger Vigilant Vole window creation without limit | Fixed in #35 |
| WS-07 | Low (admin only) | The Vigilant Vole token is not bound to its URL; test connection echoes answers | Fixed in #35 |
| WS-08 | Low | Registry answers not validated before being stored | Fixed in #35 |
| WS-09 | Low | "Wren never updates itself" is a name match on agent-reported image strings | Open (the agent now refuses its own container, #33; no "never update" setting yet) |
| WS-10 | Low | Editors can pin a target to `auto` and weaken auto-mode safeguards | Fixed in #35 |
| WS-11 | Info | Smaller observations | See below |

WS-11 in detail:

- First-run takeover default: fixed through vole-kit 0.6.0 (setup token required), shipped in 0.7.1 (#37).
- `/api/v1` accepted cookie-authenticated writes without a same-origin check (reported in the kit review as VK-01,
  High for Wren as shipped): fixed in vole-kit 0.5.1, shipped in Wren 0.7.1 (#37). Wren 0.7.0 still used kit 0.5.0,
  so upgrade to 0.7.1 or later.
- `back` redirect parameter: fixed in #35.
- One key (`session.key`) for cookie signing and for sealed secrets: open, see the vole-kit section of the Vigilant
  Vole review (VK-11).
- Agent text in chat notifications (mentions, links): Discord is covered by vole-kit; other providers are open
  (VK-14).
- Body read before authentication on agent endpoints (2 MiB default), the shared pool of 64 live-page streams,
  and case-sensitive registry credential lookup: open, low value.
- Release-note links can point at any GitHub repository named by an agent's label: accepted, links are limited to
  `https://github.com/`.

## What the fixes changed, briefly

- Agent: scrubbed child environment, size-capped and redirect-free HTTP, own-container refusal, a run ledger
  (`runs.json` beside `agent.toml`) that gates updates and rollbacks, a required `[updates]` allow list,
  `min_health_seconds` (default 30) and an optional `require_snapshot`, https required unless the host is on a local
  network, symlink-safe snapshot probes.
- Server: registry references validated and fetched through the same address checks as version URLs, private
  registries need an entry on the Registries page, inventory limits, notification channels admin-only, the Vigilant
  Vole token bound to its URL, settings of vanished targets kept for 30 days, new targets on `auto` servers wait for
  an admin.

## Accepted and open items

- **Digest choice (WA-16, remainder).** The server still picks which digest of the same repository the agent
  updates to, including an older one. The agent only accepts a digest of the repository the container already runs.
  A "newer only" rule was suggested and not built.
- **WA-06, canonical roots.** Open and planned. Needs a local attacker who can replace a directory under a root by a
  symlink. The fix is canonicalising paths against the canonical root at the point of use.
- **WA-14, output redaction.** Open and planned. Until then, script output (last few KB per step) is uploaded to Wren
  and visible to every role. Do not run installers that print secrets.
- **WA-17, process-tree kill.** Open and planned. A timeout kills the shell, not necessarily its children.
- **WA-21, narrower mounts.** Open and planned. The documented `docker run` mounts more than the agent needs.
- **Agent settings managed in the web UI.** Decided and planned by the owner, to replace copy-and-paste of the
  agent.toml helper output (WA-18). Not built yet.
- **WA-09, WA-10, WA-15, WA-22, WA-23, WS-09.** Open, low severity, no change yet.
- **Not fixed by design.** The Docker socket is root-equivalent and the agent runs as root; this is documented in
  `docs/security.md`.

## Not covered

- vole-kit internals (sign-in, 2FA, passkeys, rate limits, sessions, notification providers). These are reviewed in
  the Vigilant Vole document, which hosts the kit.
- Unraid's own PHP scripts, Compose Manager and the `docker compose` CLI. Their behaviour was taken from the project's
  spike notes, not from source.
- The agent tests, and reqwest and rustls behaviour (redirect and header handling was stated from memory).
- The CI and release pipeline, image signing and provenance, reverse-proxy configuration, and a dependency audit
  (`cargo deny` is configured but was not run for this review).
- Translations of locale strings, and the notification text builders beyond a skim.
- Dynamic behaviour: nothing was run, fuzzed or load-tested.

## Notes for operators

Release notes: [0.7.0](https://github.com/Ferdinand99/Wise-Wren/releases/tag/v0.7.0) and
[0.7.1](https://github.com/Ferdinand99/Wise-Wren/releases/tag/v0.7.1). See also [upgrading](upgrading.md).

- **Before upgrading an agent to 0.7.0**, add an `[updates]` section with an `allow` list to its `agent.toml`
  (`allow = ["*"]` keeps the old behaviour). Without it the agent refuses every update and rollback, and Wren
  shows the targets as Blocked.
- **Agent server URL.** `http://` is a configuration error unless the host is loopback, private, link-local, a
  single-label name, `.local`, `.lan`, `.home.arpa`, `.internal`, `.ts.net` or a Tailscale address. `wren-agent
  --check` says which. `/config` must be writable (the ledger lives there).
- **Rollbacks of older updates.** Updates run before 0.7.0 have no ledger entry and cannot be rolled back from
  Wren. The health period is raised to 30 seconds if lower. `require_snapshot` defaults to false.
- **Notification channels** are admin-only. Editors lose access to the Channels tab and the REST routes.
- **Private registries** (a registry on a LAN address) need an entry on Settings, Registries, otherwise their images
  show as unknown. Redirecting registries and token services on another domain are no longer followed.
- **Inventory limits.** An agent that reports more than 2000 containers, 50 images in one target or 100 labels on a
  container gets an error, and its targets stop updating until it fits.
- **Editors** can no longer set a target to an explicit `auto`; turning a server's `auto` off also resets explicit
  `auto` targets. New targets on `auto` servers start on `approve` until an admin allows automatic updates.
- **Vigilant Vole URL.** Changing it requires entering the token again.
- **0.7.1 (vole-kit 0.6.0).** A fresh instance asks for a setup token at `/setup`. Two-factor secrets are sealed at
  rest, and the first start makes a pre-migration copy in `backups/`; going back to 0.7.0 needs that copy. Creating
  write or never-expiring API tokens, adding or promoting admins, resetting another user's password or two-factor
  sign-in, and deleting a passkey ask for the password. Cookies are `Secure` when the public URL is https, and a
  damaged `session.key` stops the start instead of being replaced.
