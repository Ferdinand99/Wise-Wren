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
stack's script. The agent also never updates **itself**: a job whose target includes the agent's own container
(alone, or in the same Compose project) is refused with "Wren never updates its own agent".

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

What stays in `agent.toml`: `server`, `token`, `docker_socket`, `scan_interval`, and every `[[script_stack]]` with
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
- The agent never updates its own container, whatever the settings say.
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

## Upgrading the agent

Wren doesn't update its own agents (or itself, or Vigilant Vole) yet: their targets show the new version but
can't be approved. Pull the new image and recreate the container (Unraid: "Check for updates" → apply). Managed settings live in
`managed.json` in the config folder, so they survive an update.
