# Adding an agent

Every Docker host Wren looks after runs a small agent. It connects **out** to Wren (no open ports), sends a
heartbeat every 30 seconds and an inventory of what runs, and waits for jobs: updates and rollbacks someone
approved in Wren. It checks every job against its own `agent.toml` and against Docker before it acts, and it never
takes a command line from Wren. Read [Security notes](security.md) first: **the agent is root on its host**.

Since 0.8 you don't have to edit `agent.toml` for most of this: an admin sets what the agent may do in Wren's web
UI ([Managed settings](#managed-settings)), and the agent checks those settings against limits of its own. The file
stays the place for `server`, `token`, scripts with commands of their own, and for agents you lock.

## 1. Add the server in Wren

Servers → Add server (admins). Wren shows the server's agent token (`wwa_…`) **once**, with a ready `docker run`
line for Unraid and one for other Linux hosts. It keeps only the token's SHA-256. On the server's page an admin can
make a new token (the old one stops working at once) or revoke it.

## 2. Start the agent

### Unraid

Install **Wise Wren Agent** from Community Applications (template `wise-wren-agent.xml`), fill in `WW_SERVER_URL`
and `WW_AGENT_TOKEN`, and keep the paths as they are. Or run the line from Wren's server page:

```bash
# agent.toml needs an [updates] allow list: without one the agent refuses every update and
# rollback. Put this in /config/agent.toml (a list of just the names you want updated is safer):
#   [updates]
#   allow = ["*"]
# Add -e WW_AGENT_SELF_UPDATE=1 to let Wren update this agent (see Agent self-update below).
docker run -d --name wren-agent --restart unless-stopped \
  -e WW_SERVER_URL=http://10.0.3.26:3003 \
  -e WW_AGENT_TOKEN=wwa_… \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v /mnt/user/appdata/wren-agent:/config \
  -v /mnt/user/appdata:/mnt/user/appdata \
  -v /mnt/cache/appdata:/mnt/cache/appdata \
  -v /usr/local/emhttp:/usr/local/emhttp:ro \
  -v /var/local/emhttp:/var/local/emhttp \
  -v /boot/config:/boot/config \
  -v /var/lib/docker:/var/lib/docker \
  ghcr.io/ferdinand99/wise-wren-agent:latest
```

The Unraid mounts let the agent run Unraid's own `update_container` for template containers, so the Docker tab
stays in sync. If your appdata pool isn't called `cache`, mount it at its own path too (`/mnt/<pool>/appdata`).
Then create `/mnt/user/appdata/wren-agent/agent.toml` with at least `[updates] allow = ["*"]` (see
[agent.toml](#agenttoml)) and restart the agent: until it has an allow list the agent refuses every update. (Since 0.8 you can skip the file's `[updates]` and `roots`: once the agent has connected, set them on the server's **Agent settings** page.)
Stacks from the **Compose Manager** plugin live in `/boot/config/plugins/compose.manager/projects/<name>`: the
`/boot/config` mount reaches them, and `roots` must include that folder (below), or Wren can't update them.

### Other Linux hosts (best-effort)

```bash
# Add -e WW_AGENT_SELF_UPDATE=1 to let Wren update this agent (see Agent self-update below).
docker run -d --name wren-agent --restart unless-stopped \
  -e WW_SERVER_URL=https://wren.example.com \
  -e WW_AGENT_TOKEN=wwa_… \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v /etc/wren-agent:/config \
  -v /srv/apps:/srv/apps \
  ghcr.io/ferdinand99/wise-wren-agent:latest
```

Add one `-v <folder>:<same folder>` for every folder your containers and Compose stacks keep their files and data
in. Snapshots fall back to plain copies on ext4 and other filesystems without reflinks. Create
`/etc/wren-agent/agent.toml` with at least `[updates] allow = ["*"]` and `roots` listing those folders.

## The same-path rule

Every folder Wren may touch must be mounted **at the same path** inside the agent as on the host. Docker resolves
a Compose stack's bind mounts on the host, so `docker compose up` inside the agent only works when the stack's
folder has the same path in both places; snapshots and rollbacks use those paths too. A Compose stack whose folder
isn't mounted shows as "files not reachable: mount <working_dir> into the agent" and is notify-only.

## agent.toml

You need the file for `server`/`token` (or their variables), for custom script stacks, for locked agents, and as the
starting point the first import reads. This section describes every key; with managed settings, the settings page
sets the managed ones.

`WW_SERVER_URL` and `WW_AGENT_TOKEN` can replace the `server` and `token` lines (they win over the file), but the
file itself is needed for `[updates]`: without an allow list the agent refuses every update. The agent reads
`/config/agent.toml` (or `--config PATH`, or `WW_AGENT_CONFIG`). `docker exec wren-agent wren-agent --check` checks
it and says what it allows. The agent refuses to start (exit code 2) and names the field for any mistake, including
unknown keys.

```toml
server = "http://10.0.3.26:3003"   # or WW_SERVER_URL
token = "wwa_…"                    # or WW_AGENT_TOKEN

# Folders Wren may touch: snapshots, Compose files, script stacks. Each one
# mounted at the same path in the agent. Never "/". On Unraid also Compose
# Manager's stacks.
roots = [
  "/mnt/user/appdata",
  "/mnt/cache/appdata",
  "/boot/config/plugins/compose.manager/projects",
]

# docker_socket = "/var/run/docker.sock"
# scan_interval = "30m"            # how often the agent sends an inventory (1m–24h)

# Which targets Wren may update at all, as globs on the target's name (* and ?).
# Required: without an allow list the agent refuses every update and rollback.
[updates]
allow = ["nextcloud", "immich", "media-*"]   # or ["*"] to get started
deny = ["media-db*"]               # deny wins over allow
# min_health_seconds = 30          # a job never gets a shorter health period
# require_snapshot = true          # default false: true makes every update need a snapshot

# How many rollback points stay per target (optional).
[retention]
rollback_images = 1                # updates whose old images stay tagged (1-10)
snapshots = 2                      # data snapshots (1-10)

# Let Wren replace this agent with a newer official release (off by default;
# WW_AGENT_SELF_UPDATE=1 does the same and wins when set). See Agent self-update.
# [self_update]
# enabled = true
# confirm_minutes = 5              # how long the new agent has to reach Wren (2–30)
```

| Key | What |
|---|---|
| `server`, `token` | Wren's address and this server's agent token. |
| `roots` | Absolute folders, no `..`, not `/`. Without roots the agent can still update single containers, but takes no snapshots and refuses Compose and script stacks. |
| `docker_socket` | Default `/var/run/docker.sock`. |
| `scan_interval` | Default `30m`. "Check now" in Wren asks for an inventory at once. |
| `[updates] allow` / `deny` | **Required.** Globs on target names. Without an `allow` list (no `[updates]` section, only `deny`, or `allow = []`) the agent refuses every update and rollback. A job outside `allow` or inside `deny` ends as **refused by agent config** before the agent even reads Docker. Wren still shows those targets and their new versions. |
| `[updates] min_health_seconds` | Default `30`, at most `3600`. A job's health period is raised to this; Wren can ask for more, never less. |
| `[updates] require_snapshot` | Default `false`. When `true`, an update whose target has no data folder the agent can snapshot stops before anything is touched, whatever Wren asks; stateless targets (a proxy, a runner with a named volume) then never update, so turn it on only where every target keeps data under `roots`. Wren's per-target "Require a snapshot" works either way. |
| `[[script_stack]] min_interval` | Default `1h`, at most `24h`. The shortest time between two updates of a script stack. |
| `[retention]` | `rollback_images` (default 1): after a successful update the old images of the newest N updates stay tagged `wren-rollback/…`, older ones go. `snapshots` (default: Wren's 2). See [Snapshots](snapshots.md). |
| `[[script_stack]]` | A Compose stack that updates with its own script (below). |
| `[self_update]` | `enabled` (default `false`; `WW_AGENT_SELF_UPDATE` wins when set) lets Wren replace this agent with a newer official release once an admin approves it; `confirm_minutes` (2–30, default 5) is how long the new agent has to reach Wren. Local only: never part of the managed settings. See [Agent self-update](#agent-self-update). |

**`[updates]` is your last word, and it is required.** Whatever happens to the Wren server, the agent only touches
what this file allows. A new agent with no `[updates]` allow list starts, reports its inventory and refuses every
update and rollback ("refused by agent config: agent.toml has no [updates] allow list"); `--check` says so, and
Wren shows every target as **Blocked**. Start with `allow = ["*"]` and narrow it to a list of the names you want
updated when you know them. The wizard text on the server's page (the **agent.toml helper**, and the comment above
the `docker run` line) starts with `allow = ["*"]` for the same reason.

The server can ask for more safety than the agent requires, never less: the health period of a job is raised to
`min_health_seconds`, and `require_snapshot` from Wren is treated as true when the agent requires it. The preflight
step's log shows the values the run really used ("health period 30 s (Wren asked 5 s; the agent's minimum is
30 s)").

### The run ledger

The agent keeps a small record of what **it** ran in `runs.json` beside `agent.toml` (so mount `/config` from the
host; the file is root-owned, mode 0600, written whole and renamed into place, and holds the last 20 runs of each
target). Wren decides what to ask for, but the agent checks it against this record:

- An update's run id must be greater than every run id the agent already ran for that target. A Wren database
  restored from a backup (its ids start again) is refused until its ids pass the agent's.
- A rollback is accepted only for the **newest successful update this agent ran** for that target, and only once.
  A rollback of an update from before the agent had a ledger (or one that ran on another agent) is refused.
- A script stack's update runs at most once per `min_interval`, whatever Wren asks.
- A run that can't be recorded (the config folder isn't writable) isn't started.

A missing ledger starts a new one with a warning in the log; a corrupt one is moved to `runs.json.bad` first.
`--check` shows its state. After a restore, the data the restore replaced stays beside it as
`<folder>.wren-aside-<run>` until the target's next run, so a rollback that turns out wrong can still be undone
by hand; the next run removes it.

For a script stack the globs are checked against both names, the stack's `name` and its `compose_project`: a `deny`
on either refuses the job, and an `allow` on the stack's name is enough. A Compose job for a project that a
`[[script_stack]]` owns (its `compose_project`) is refused, so a plain `docker compose up` can't go round the
stack's script. An ordinary update or rollback job never touches the agent's own container: a job whose target
includes it (alone, or in the same Compose project) is refused with "Wren never updates its own agent". The agent
replaces itself only through [Agent self-update](#agent-self-update), which the host's owner has to switch on.

**The server address must be `https://`, except on your own network.** Plain `http://` is accepted only when the
host is a loopback, private (RFC 1918), link-local or unique-local IPv6 address, a single-label name (`tower`), or
a name ending in `.local`, `.lan`, `.home.arpa`, `.internal` or `.ts.net`, or a Tailscale address (100.64.0.0/10). Any other `http://` address is a configuration
error (the agent won't start; `--check` says why), because the token and every job would travel unencrypted. Put
Wren behind a reverse proxy with a certificate for agents that reach it from elsewhere. Addresses with a
`user:password@` part are refused, and the agent doesn't follow redirects.

The agent reports its `[updates]` globs and its `roots` (never its token or scripts) to Wren, so Wren knows ahead what
it would refuse: those targets show **Blocked** with the reason ("not allowed by [updates] in agent.toml", "folder …
not under the agent's roots", "files not reachable") and can't be approved or scheduled. Agents before 0.4.0 don't
report it; then Wren blocks nothing it can't see.

`wren-agent --check` also says when `/boot/config/plugins/compose.manager/projects` exists but isn't under `roots`.

### The agent.toml helper

For locked agents and agents before 0.8 (which can't take settings from Wren), admins find **agent.toml helper** on
a server's page: it builds the `[updates]` list (tick the targets to allow, or
allow `*` and tick the ones to deny), suggests `roots` from the folders the agent's inventory showed (data folders
and Compose folders), and fills a `[[script_stack]]` (with a Fluxer preset). Wren only shows the text to copy; it
stores nothing and sends nothing to the agent. Put it in `/mnt/user/appdata/wren-agent/agent.toml` (keep your server
and token lines), run `docker exec wren-agent wren-agent --check`, and restart the agent while no update runs.

## Managed settings

On a server's page, **Agent settings** (admins edit, everyone signed in can read) manages what `agent.toml` used to:

- `[updates]`: which targets may be updated, as ticks in a list of the server's targets (or "every target except
  the ones you deny"), plus `min_health_seconds` and `require_snapshot`;
- `roots`: the folders Wren may touch, **chosen from the folders mounted into the agent** (no free text);
- `[retention]`: how many rollback images and snapshots stay per target;
- **script stacks from presets** compiled into the agent (Fluxer so far): you choose the preset, a name, the Compose
  project, a folder inside the roots and the shortest time between updates.

Maintenance windows already live in Wren and are not part of this.

### How it works

1. **First connect.** An agent without managed settings sends what its `agent.toml` says (allow and deny lists, roots,
   minimums, retention, script stacks that match a preset, and only the *names* of the others; never the token or any
   command text). Wren stores that as **version 1** and offers the same values back. Nothing changes on the agent, and
   nothing is lost: the History on the settings page shows "Imported from agent.toml".
2. **Saving.** Each change is the next version number (Wren counts past any version the agent reports, so a Wren
   database restored from a backup still moves forward). Wren sends the settings in the heartbeat's answer while the
   agent reports another version, as data (JSON, no command line).
3. **The agent decides.** It checks the whole update against its hard limits (below), writes `managed.json` beside
   `agent.toml` (a temporary file, then a rename; mode 0600), applies it **without a restart** and reports the version
   in force with its next heartbeat. If any limit is broken it refuses *all* of it, keeps what it had and reports the
   reason. The page shows "Active on the agent (version N)", "Waiting for the agent" or "The agent refused version N: ...".
4. **After a restart** the agent reads `managed.json` again. A damaged or no-longer-valid file is reported, and the
   agent falls back to `agent.toml`.

What stays in `agent.toml`: `server`, `token`, `docker_socket`, `scan_interval`, `[self_update]`, and every `[[script_stack]]` with
commands of its own (the settings page lists their names as custom stacks you edit in `agent.toml` on the host). Once
an agent has managed settings, they **replace** the file's `[updates]`, `roots`, `[retention]` and preset-like stacks,
so editing those keys in `agent.toml` no longer does anything (use the page, or lock the agent). A stack in
`agent.toml` counts as a preset only when its commands, environment, time limit and own-backup flag are exactly the
preset's (the older Fluxer example in these docs included).

### Hard limits

The agent refuses the whole settings update when one of these is broken:

- A root must be an absolute folder, not `/`, with no `.` or `..`, **inside a folder mounted into the agent** (a bind
  mount of its own container at the same path as on the host; the agent reads its mounts from Docker and reports
  them), and not under `/boot`, `/etc`, `/proc`, `/sys`, `/dev`, `/var/lib/docker`, `/var/run`, `/run`, `/usr`,
  `/bin`, `/sbin`, `/lib*` or `/root`. The one exception is Unraid's Compose Manager folder
  (`/boot/config/plugins/compose.manager/projects`), which these docs ask for anyway. The agent's own config folder
  can't be a root. A root that `agent.toml` itself lists stays valid: you chose it on the host.
- Script stacks only by **preset name**; the preset's commands are constants in the agent. The only parameters are
  the stack's name and Compose project (`a-z`, `0-9`, `-`, `_`), its folder (inside a root) and the shortest time
  between updates (1 minute to 24 hours). There is no field for a command, and settings with a field the agent
  doesn't know are refused.
- Numbers within the bounds `agent.toml` has: `min_health_seconds` 0-3600, `rollback_images` and `snapshots` 1-10;
  allow and deny entries are names with `*` and `?`.
- A custom stack from `agent.toml` must stay under the roots.
- An ordinary job never touches the agent's own container, and the settings have no field that switches
  [self-update](#agent-self-update) on, whatever Wren sends.
- A settings version older than the one in force is refused (a replay).

### Locked agents

Set `WW_AGENT_LOCKED=1` on the agent and it **ignores managed settings entirely**, even a `managed.json` it finds:
only `agent.toml` counts, as before 0.8. It reports `locked: true`, and Wren shows the settings read-only with
"Locked on the agent" and never sends it settings. Use this when the host's owner, not Wren's admins, must decide
what the agent touches. Remove the variable to manage the agent from Wren again; the next heartbeat applies the
settings Wren holds. `wren-agent --check` says whether the agent is locked and which settings are in force.

### Older agents

An agent before 0.8 sends no settings report. Wren says "this agent version can't take settings from Wren; update
it", sends it nothing and keeps the agent.toml helper (below) for it. A new agent with an older Wren just never
receives settings and keeps using `agent.toml`.

### History and notifications

Every change is kept (who, when, what; the first row is the import). A change that **widens** what the agent may do,
that is a new root, an allow entry the list didn't already cover, a removed deny entry, a new script stack or a
changed folder, a lower health period or interval, or snapshots no longer required, is marked "Allows more" and sent
to **every active notification channel** (admins manage those). Narrowing changes are only logged.

## Script stacks (Fluxer)

Some apps update with the vendor's own script instead of `docker compose pull`. Pick the built-in preset on the
settings page, or define a script of your own in `agent.toml`; Wren can only ask for it **by name**.

### The Fluxer preset

The preset `fluxer` is built into the agent and runs Fluxer's own installer, with the commands of the example below
and one difference: the installer is downloaded into a temporary folder inside the stack's folder
(`.wren-install.XXXXXX`), checked against Fluxer's published checksum and removed when the command ends, however it
ends. It keeps Fluxer's `handles_own_backup`, a 20 minute limit and `COMPOSE_PROJECT_NAME` in the environment. The
agent detects versions itself: the newest from Fluxer's release document (`https://fluxer.dev/rexctl/release.json`,
field `version`; the agent follows its redirect only within fluxer.dev and fluxer.app) and the installed one from
`release` in `.rexctl/state.json` in the stack's folder (the file `rexctl` keeps), so Wren can show "update available".

### A script of your own


```toml
roots = ["/mnt/cache/appdata/fluxer"]

[[script_stack]]
name = "fluxer"                               # a-z 0-9 - _
compose_project = "fluxer"                    # its containers' Compose project (default: name)
working_dir = "/mnt/cache/appdata/fluxer"     # under one of roots, mounted at the same path
handles_own_backup = true                     # the script stops and backs up itself: Wren skips its stop and snapshot
env = { COMPOSE_PROJECT_NAME = "fluxer" }
timeout = "20m"                               # 90s, 20m, 1h; at most 24h
min_interval = "1h"                           # shortest time between two updates (default 1h)

[script_stack.check]                          # optional: can the update run now?
command = "curl -fsSL -o /dev/null https://fluxer.dev/rexctl/release.json"

[script_stack.update]
command = """
curl -fsSLO https://fluxer.dev/install.sh && curl -fsSLO https://fluxer.dev/install.sh.sha256 \
  && sha256sum -c install.sh.sha256 \
  && sh install.sh --update --allow-root --non-interactive --dir "$PWD"
"""

[script_stack.rollback]
command = "sh install.sh --rollback --allow-root --non-interactive --dir \"$PWD\""
```

The commands run with `sh -c` in `working_dir`, with `env` and the timeout (`check`: at most 5 minutes).


- **`check`** runs in the preflight, before anything stops. When it fails the run ends there: nothing was stopped
  and the stack keeps running. The Fluxer check asks whether rexctl, which `install.sh` downloads, is released.
- **When the update script fails**, the agent first looks at what changed. If every container still runs as before
  (same containers, same running state) and the Compose files and `.env` are unchanged, nothing is rolled back: the
  run ends as failed and the stack keeps running.
- **Otherwise the rollback runs.** With `handles_own_backup = true` the agent doesn't stop anything first; the
  rollback script does what it needs. If the rollback fails too, the agent leaves everything as it is for a person
  to decide, and the run's detail lists the containers that ran before and are stopped now.

The agent image has `bash`, `curl`, `openssl`, GNU `coreutils`, `jq` and the Docker CLI with Compose.

### Version sources

Without a version source, a script stack gets "update available" from its images' digests like any Compose stack
(right for Fluxer, whose images use the moving tag `v1`). Otherwise give exactly one source:

```toml
[script_stack.version]
github = "owner/repo"                 # the newest GitHub release
# url = "https://example.com/version.json"
# json_pointer = "/version"           # or: regex = "v([0-9.]+)"
# latest_command = "…"                # the agent runs it and reports the first line
installed_command = "cat VERSION"     # what is installed
# timeout = "30s"                     # per command, at most 10m
```

The Wren **server** fetches `github` and `url` sources. A `url` must be a public address: Wren refuses private,
loopback, link-local and other local addresses (also after redirects), reads at most 256 KB within 20 seconds and
keeps only the version. For a version file on your LAN, use `latest_command` (for example `curl -fsS
http://nas.lan/version.txt`): the agent runs it on its own host. Or set `WW_ALLOW_PRIVATE_VERSION_URLS=1` on the
Wren server to let `url` sources reach private and local addresses. Leave it off unless you need it: anyone who can
edit an agent's `agent.toml` could then make the server fetch addresses on its own network.

## Agent self-update

Since 0.10 Wren can replace its own agents with a newer release. It can do that **only where the host's owner has
allowed it**, only with the official image, and only to the version of Wren itself. Everything else about an update
stays as it was: an admin approves it, and the agent checks it before anything is touched.

### Turn it on (on the host)

Self-update is **off** until someone with access to the host switches it on, in one of two places:

- the environment variable `WW_AGENT_SELF_UPDATE=1` on the agent container (`1`/`0`, `true`/`false`, `yes`/`no`
  and `on`/`off` are understood), or
- `agent.toml`:

  ```toml
  [self_update]
  enabled = true
  confirm_minutes = 5      # how long the new agent has to reach Wren (2–30, default 5)
  ```

The variable wins when it is set (`WW_AGENT_SELF_UPDATE=0` switches it off whatever the file says). Prefer the
variable if you may ever go back to an agent before 0.10: those refuse to start on an `agent.toml` that has a
`[self_update]` section. `wren-agent --check` says whether it is on.

Wren can't switch it on. The switch is not part of the managed settings (a settings update that carries the field is
refused as a whole), and `[updates] allow` / `deny` don't apply to it, so `allow = ["*"]` doesn't turn it on either.
`WW_AGENT_LOCKED` makes no difference: the opt-in is already local, so a locked agent with the switch on can be
updated and one without it can't.

### What Wren can and can't do

An admin approves the update on the server's page (**Agent version**: now, or at a set time), through
`POST /api/v1/servers/{id}/agent-update` or the matching MCP tool. In 0.10 it is **approval only**: there is no
automatic mode for agents, and the auto window doesn't touch them. Wren sends the agent a version and an image
digest, nothing else: no image name, no repository, no command line.

- The target is always **Wren's own version**. An agent is never offered a version the server doesn't know, so
  update Wren first and the agents after. When a newer Wren is out than the one you run, the card says so.
- Only the **official repository** (`ghcr.io/ferdinand99/wise-wren-agent`, fixed in the agent) is accepted. A local
  build (`wise-wren-agent:dev`), a fork or a mirror is refused.
- Only **upwards**, to a release (`X.Y.Z`, no pre-release): never the same version or an older one.
- The agent pulls the digest Wren named and checks that the image says it is that version and that the release tag
  (`:X.Y.Z`) points at the same digest. A server can only choose between images GHCR publishes as a release.
- The agent has to be replaceable: not started with `--rm`, not sharing another container's network, and with its
  config folder (`/config`) mounted from the host and writable. Otherwise the card says why it can't.
- An agent before 0.10.0 can't do any of this. Update it by hand once (see [Upgrading](upgrading.md#to-010)). The
  first real self-update is therefore 0.10.0 → 0.10.1.

### The image tag

Wren never moves a tag that promises a version, so the tag the agent runs from has to follow releases:

| Tag | Self-update |
|---|---|
| `:latest` (or no tag) | yes |
| `:0` (the major version) | yes, within that major version |
| `:0.10` (major and minor) | yes, to patch releases of 0.10 only; 0.11.0 is refused until you change the tag |
| `:0.10.1` (exact) or `@sha256:…` | no: the card says the tag pins a version and asks you to change it to `:latest` or `:0` |
| `:edge`, a local tag | no |

### What happens

1. The old agent checks everything (opt-in, own container, repository, tag, version, digest, config folder) before
   anything changes, pulls the new image and keeps the old one tagged `wren-rollback/agent-<name>:<run>-<name>` (the
   repository part is the lower-cased name; see [below](#going-back-by-hand-after-a-good-update)). It
   copies `managed.json` and `runs.json` aside and writes `selfupdate.json` in the config folder.
2. It starts a **helper container**, `wren-agent-selfupdate-<run>`, made from the *old* image (known to work on this
   host, so a broken release can't break its own rollback). The helper has the Docker socket and the config folder,
   no network, and none of the agent's environment except `TZ` and `RUST_LOG`: it never talks to Wren. It restarts
   itself after a crash and carries on from the state file.
3. The helper stops the old agent, renames it `<name>-wren-old-<run>` and creates the new agent under the original
   name with the same configuration (environment, mounts, labels, restart policy, networks). It starts the new agent
   only when its image is the release the old agent checked; any other image is rolled back.
4. The new agent is on **probation**: it reports to Wren as usual but takes no jobs and no settings. It confirms
   when Wren answers with the same server id, Docker is readable, one job poll worked and it has run without a
   restart for `max(min_health_seconds, 60 s)`.
5. Confirmed: the helper removes the old container and the backup, then itself, and the new agent reports the
   finished update to Wren. Not confirmed in time, or the new agent stops or restarts: the helper **rolls back**
   (below).

Wren has no contact with the host for about a minute while this runs. The run page shows it, and the notification
channels get the result. **Don't update Wren itself while an agent update runs**: the new agent needs Wren to confirm
it. If Wren is down for the whole probation, the update is rolled back (and can be tried again); the old agent
reports it when Wren is back.

| Step | Time allowed |
|---|---|
| The helper starts | 2 minutes |
| The old agent stops | 60 seconds |
| The new agent is created and started | 2 minutes |
| The new agent confirms | `confirm_minutes`, default 5 (2–30) |
| It must then run stable | `max(min_health_seconds, 60 s)` |
| The whole update, hard limit | 30 minutes |
| Wren closes a run it hears nothing about as "needs you" | 45 minutes |

If the host reboots or Docker restarts during the probation, the helper gives the new agent a fresh confirmation
period; if that happens before the new agent was started, it rolls back instead.

### What you see on the host

For a minute or two the Docker tab (Unraid) or `docker ps -a` shows two extra containers: the helper
`wren-agent-selfupdate-<run>` and the stopped old agent `<name>-wren-old-<run>`. Unraid lists them as containers it
doesn't know; that is expected, and both go away after a good update.

- **`docker run`:** add `-e WW_AGENT_SELF_UPDATE=1` and recreate the container once.
- **Compose:** the new container keeps its Compose labels, so `docker compose ps` knows it. The compose file must use
  `:latest`, `:0` or `:0.10`, not an exact version: the next `docker compose up -d` would put the old version back,
  so Wren refuses it (the card says the tag pins a version). During an update `compose ps` shows the extra stopped `-wren-old-` container; a
  later `docker compose up -d` either does nothing or recreates the agent once with the same tag. An agent in the
  same Compose project as the server is fine: only the agent's container is touched.
- **Unraid:** the container's repository must be `ghcr.io/ferdinand99/wise-wren-agent:latest` (or `:0`); add the
  variable `WW_AGENT_SELF_UPDATE` with the value `1` to the container. The agent keeps its name, so Unraid's template
  link and autostart keep working. The helper also gets the `/var/lib/docker` mount (the `docker run` line for
  Unraid has it already) so that the helper can write Unraid's update status for the new image and the Docker tab stays in
  sync.

### When something goes wrong

The helper rolls back when the new agent never confirms, stops or restarts, or can't be created or
started: it removes the new container, renames `<name>-wren-old-<run>` back to `<name>`, restores `managed.json` and
`runs.json`, points the image tag back at the old image and starts the old agent. That agent reports the rollback
and its reason to Wren. Nothing is deleted before the new agent has proved it can talk to Wren.

**Break glass.** If the helper itself can't finish (the rollback failed, or the helper is stuck), Wren closes the run
as one that needs you and shows the command below on one line; it is also in the agent's log and in `selfupdate.json`.
Run it on the host (`<name>` is the agent's container name, `<run>` the run number Wren shows):

```bash
docker rm -f wren-agent-selfupdate-<run>
if docker container inspect <name>-wren-old-<run> >/dev/null 2>&1; then
  docker rm -f <name>; docker rename <name>-wren-old-<run> <name>
fi
docker start <name>
```

It is safe whatever state the update was left in. It removes the helper first, so nothing works on the containers
while you do. Only while the renamed old agent `<name>-wren-old-<run>` still exists does it remove what has the name
`<name>` (the new agent) and give the name back; otherwise `<name>` already is the old agent and is only started. An
error from a `docker rm -f` of a container that is already gone is expected. Then read the agent's log before you try
again.

You don't need to tidy up after it: when the old agent starts and finds the run unfinished, with no helper, no
renamed copy and no new container left, it points the image tag back at the old image, marks the run as rolled back
by hand in `selfupdate.json` and reports it to Wren. The next agent update can then start as usual.

### Going back by hand after a good update

The old image stays tagged `wren-rollback/agent-<name>:<run>-<name>` (the newest `[retention] rollback_images` of
them, default 1). The repository part is the **lower-cased** container name (anything but letters and digits becomes
`-`), while the tag after the colon keeps the name as it is: for an Unraid container called `Wise-Wren-Agent` the tag
is `wren-rollback/agent-wise-wren-agent:57-Wise-Wren-Agent`. Look up the exact tag:

```bash
docker image ls 'wren-rollback/agent-*'
```

To go back, tag that image as the reference the container uses and recreate the container with the same options as
before (your `docker run` line, Compose file or Unraid template):

```bash
docker tag <the tag you found> ghcr.io/ferdinand99/wise-wren-agent:latest
docker rm -f <name>
```

Wren has no "roll back the agent" button: it would be a downgrade. An agent older than the server keeps working, but
Wren will offer the update again.

### Hard limits

Wren can never make an agent update itself. Only the owner of the host can allow it, and even then Wren can only
choose *when* the agent moves to a *newer official release*.

- An ordinary `update` or `rollback` job for the agent's own container (alone, or in its Compose project) is still
  refused ("Wren never updates its own agent"). Self-update is a separate kind of job with its own checks.
- The switch is local (variable or `agent.toml`) and can't be set from Wren.
- The repository is built into the agent. It accepts only newer releases and checks the label and the release tag of
  what it pulled.
- The old agent is kept, renamed, until the new one has reached Wren.
- A run id has to be newer than any the agent already ran for itself (the run ledger), as for every other job.
