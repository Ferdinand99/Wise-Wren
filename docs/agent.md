# Adding an agent

Every Docker host Wren looks after runs a small agent. It connects **out** to Wren (no open ports), sends a
heartbeat every 30 seconds and an inventory of what runs, and waits for jobs: updates and rollbacks someone
approved in Wren. It checks every job against its own `agent.toml` and against Docker before it acts, and it never
takes a command line from Wren. Read [Security notes](security.md) first: **the agent is root on its host**.

## 1. Add the server in Wren

Servers → Add server (admins). Wren shows the server's agent token (`wwa_…`) **once**, with a ready `docker run`
line for Unraid and one for other Linux hosts. It keeps only the token's SHA-256. On the server's page an admin can
make a new token (the old one stops working at once) or revoke it.

## 2. Start the agent

### Unraid

Install **Wise Wren Agent** from Community Applications (template `wise-wren-agent.xml`), fill in `WW_SERVER_URL`
and `WW_AGENT_TOKEN`, and keep the paths as they are. Or run the line from Wren's server page:

```bash
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
in. Snapshots fall back to plain copies on ext4 and other filesystems without reflinks.

## The same-path rule

Every folder Wren may touch must be mounted **at the same path** inside the agent as on the host. Docker resolves
a Compose stack's bind mounts on the host, so `docker compose up` inside the agent only works when the stack's
folder has the same path in both places; snapshots and rollbacks use those paths too. A Compose stack whose folder
isn't mounted shows as "files not reachable: mount <working_dir> into the agent" and is notify-only.

## agent.toml

Optional while `WW_SERVER_URL` and `WW_AGENT_TOKEN` are set (they win over the file). The agent reads
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
# Without this section, every target may be updated once someone approves it.
[updates]
allow = ["nextcloud", "immich", "media-*"]
deny = ["media-db*"]               # deny wins over allow

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
| `[updates] allow` / `deny` | Globs on target names. A job outside `allow` or inside `deny` ends as **refused by agent config** before the agent even reads Docker. Wren still shows those targets and their new versions. |
| `[retention]` | `rollback_images` (default 1): after a successful update the old images of the newest N updates stay tagged `wren-rollback/…`, older ones go. `snapshots` (default: Wren's 2). See [Snapshots](snapshots.md). |
| `[[script_stack]]` | A Compose stack that updates with its own script (below). |

**`[updates]` is your last word.** Whatever happens to the Wren server, the agent only touches what this file
allows. Use it on production servers.

The agent reports its `[updates]` globs and its `roots` (never its token or scripts) to Wren, so Wren knows ahead what
it would refuse: those targets show **Blocked** with the reason ("not allowed by [updates] in agent.toml", "folder …
not under the agent's roots", "files not reachable") and can't be approved or scheduled. Agents before 0.4.0 don't
report it; then Wren blocks nothing it can't see.

`wren-agent --check` also says when `/boot/config/plugins/compose.manager/projects` exists but isn't under `roots`.

### The agent.toml helper

On a server's page, admins find **agent.toml helper**: it builds the `[updates]` list (tick the targets to allow, or
allow `*` and tick the ones to deny), suggests `roots` from the folders the agent's inventory showed (data folders
and Compose folders), and fills a `[[script_stack]]` (with a Fluxer preset). Wren only shows the text to copy; it
stores nothing and sends nothing to the agent. Put it in `/mnt/user/appdata/wren-agent/agent.toml` (keep your server
and token lines), run `docker exec wren-agent wren-agent --check`, and restart the agent while no update runs.

## Script stacks (Fluxer)

Some apps update with the vendor's own script instead of `docker compose pull`. Define the script in `agent.toml`;
Wren can only ask for it **by name**.

```toml
roots = ["/mnt/cache/appdata/fluxer"]

[[script_stack]]
name = "fluxer"                               # a-z 0-9 - _
compose_project = "fluxer"                    # its containers' Compose project (default: name)
working_dir = "/mnt/cache/appdata/fluxer"     # under one of roots, mounted at the same path
handles_own_backup = true                     # the script stops and backs up itself: Wren skips its stop and snapshot
env = { COMPOSE_PROJECT_NAME = "fluxer" }
timeout = "20m"                               # 90s, 20m, 1h; at most 24h

[script_stack.update]
command = """
curl -fsSLO https://fluxer.dev/install.sh && curl -fsSLO https://fluxer.dev/install.sh.sha256 \
  && sha256sum -c install.sh.sha256 \
  && sh install.sh --update --allow-root --non-interactive --dir "$PWD"
"""

[script_stack.rollback]
command = "sh install.sh --rollback --allow-root --non-interactive --dir \"$PWD\""
```

The commands run with `sh -c` in `working_dir`, with `env` and the timeout. The agent image has `bash`, `curl`,
`openssl`, GNU `coreutils`, `jq` and the Docker CLI with Compose.

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
can't be approved. Pull the new image and recreate the container (Unraid: "Check for updates" → apply).
