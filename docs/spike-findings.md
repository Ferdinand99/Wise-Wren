# Wise Wren phase 1 spike: findings

Spike run on 2026-10-23 on LANBox (Unraid 7.4.0-rc.1, Docker 29.9.2, Compose 5.6.1). Scripts are in the local throwaway repo `wren-spike`.

**Summary**
- **Q1 registry digests:** anonymous `HEAD` with an index-first `Accept` list returns the index digest
  (equal to `imagetools`) on Docker Hub, GHCR, Quay and lscr.io; Docker Hub HEADs cost no pull quota.
  Compare with the running image's `RepoDigests`; pinned, ID-only and locally built images are `unknown`.
- **Q2 Unraid templates:** use Unraid's own `update_container`. It works from the agent container with
  PHP 8.4 and the Unraid mounts, and keeps the UI in sync. A plain Docker-API recreate leaves Unraid's
  status stale and pins old image labels.
- **Q3 Fluxer:** the real installer installed, updated (exit 0, 40 s) and rolled back (exit 0, 25 s)
  a throwaway instance from an agent container, with no domain or open ports. Mounting the stack
  directory at a different path breaks the update (exit 6), so it must be mounted at the same path.
- **Q4 snapshots:** a reflink copy takes 0.1–0.2 s for 2 GB on xfs and btrfs, needs no capability,
  and is the default. btrfs snapshots only help for subvolumes (Tower's appdata is not one).
  `/mnt/user` cannot reflink.
- **Agent:** a container, not privileged: the socket, the Unraid paths, and the data roots at
  identical paths; `CAP_SYS_ADMIN` only as an opt-in for btrfs subvolume targets.

**Open items for the owner**
- Look at LANBox's Docker tab once: `wren-spike-whoami` should show its icon, a working WebUI link
  (`http://10.0.3.26:18080/`) and no "update ready". It was last updated by Unraid's
  `update_container` run from a container. The spike read the same data through `getAllInfo()`
  (up to date, icon and URL present); a screenshot was not possible. Afterwards remove it with
  `ssh root@10.0.3.26 sh /mnt/disk1/wren-spike/cleanup.sh`.
- Decide whether shipping PHP in the agent image (Method A) is acceptable, or whether the
  `nsenter`-into-host variant (`--privileged --pid=host`, untested) or the Method B fallback should
  be the default.

## Q1 Registry digests

**Answer:** an anonymous manifest `HEAD` with the OCI/Docker index+manifest `Accept` list returns the
*index* digest for every registry tested. It is identical to `docker buildx imagetools inspect`, and
Docker Hub's `ratelimit-remaining` did not move across about 25 HEADs (it stayed `100;w=3600`). Compare
that digest with the `RepoDigests` entry of the image the container actually runs.

**Method (Phase 3 implements this in Rust):**
1. Parse the ref: no registry part → `registry-1.docker.io`, one-part names get `library/`; default tag `latest`.
2. `HEAD /v2/<repo>/manifests/<tag>` with
   `Accept: application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json`.
3. On `401`, follow the `WWW-Authenticate: Bearer realm,service,scope` challenge (anonymous token) and
   repeat the HEAD. Realms seen: Docker Hub `auth.docker.io/token`, GHCR `ghcr.io/token`, and lscr.io →
   `ghcr.io/token` (lscr is a proxy in front of GHCR). Quay answered `200` to the first, unauthenticated
   HEAD (no token needed). Generic challenge handling covers all of them, so no per-registry code is needed.
4. Read `docker-content-digest` (always present on HEAD, so no GET is needed), `content-type` and
   `ratelimit-remaining`.
5. Local digest: `docker inspect <ctr> .Image` (the image ID the container runs, not what the tag points
   at now) → `docker image inspect <id> .RepoDigests`; pick the entry whose repo matches the ref
   (`docker.io/` and `library/` normalised). Equal → `current`, different → `update`.
6. **Unknown, never update (Review Focus 2):** a ref pinned with `@sha256:` or an image ID, or an image
   with empty `RepoDigests` (built locally), makes no registry request at all. A registry error →
   `unknown(registry)`.
   **Gotcha:** `docker ps --format {{.Image}}` shows `redis` for a container created from
   `redis@sha256:…`; the pin is only visible in `.Config.Image`. Wren must read `.Config.Image`. (The
   first version of `compare.sh` used `docker ps`, called the pinned container `update`, and sent a
   pointless Docker Hub lookup for the local-only image; both are fixed.)

**Multi-arch rule (Review Focus 1):** every multi-arch ref answered with the index media type
(`oci.image.index.v1` / `manifest.list.v2`), and `RepoDigests` also holds the index digest, so the
comparison is index to index. They could only differ if a HEAD returned a single-platform manifest type
for an image whose `RepoDigests` is an index, which was not seen. Rule: compare only when both sides are
the same kind; never compare an index digest with a platform manifest digest.

| Ref | HEAD digest | Media type | `imagetools` |
|---|---|---|---|
| nginx:1.27 | sha256:6784fb0834aa… | oci.image.index.v1 | same |
| ghcr.io/ferdinand99/keen-kestrel:latest | sha256:67d2d8db13da… | oci.image.index.v1 | same |
| quay.io/prometheus/node-exporter:latest | sha256:1b4e4438faca… | docker manifest.list.v2 | same |
| lscr.io/linuxserver/syncthing:latest | sha256:92fd6eb29759… | oci.image.index.v1 | same |

`compare.sh` on LANBox (the owner's 8 running containers, read-only, plus 3 spike containers):

| Container | Image | Local | Remote | Status |
|---|---|---|---|---|
| Newt | fosrl/newt | 0189c8287058 | 0189c8287058 | current |
| dockersocket | ghcr.io/tecnativa/docker-socket-proxy:latest | 1f5038b54f06 | 1f5038b54f06 | current |
| postiz | ghcr.io/gitroomhq/postiz-app:latest | d2b37802e967 | d2b37802e967 | current |
| postiz-postgres | postgres:17-alpine | b0f9560a2de0 | eee6c6e1b089 | **update** |
| postiz-redis | redis:7.2 | 1009f15ccd9e | 1009f15ccd9e | current |
| temporal | temporalio/auto-setup:1.28.1 | 607d68caa111 | 607d68caa111 | current |
| temporal-elasticsearch | elasticsearch:7.17.27 | 9a6443f55243 | 9a6443f55243 | current |
| temporal-postgresql | postgres:16 | a902560563d3 | 9e2cf6287f25 | **update** |
| wren-spike-bydigest | redis@sha256:1009f15c… | 1009f15ccd9e | - | unknown (pinned) |
| wren-spike-byid | sha256:3e4882ccf187 (image ID) | 1009f15ccd9e | - | unknown (pinned/id) |
| wren-spike-local | wren-spike-local:1 (built locally) | - | - | unknown (no repo digest) |

Hand check of `temporal-postgresql`: `imagetools inspect postgres:16` = `sha256:9e2cf6287f25…`, while the
running image `066167ff39c2` has `RepoDigests ["postgres@sha256:a902560563d3…"]`, so it is a real update
(the tag moved).

**Requests per run:** 8 refs → 8 HEAD probes + 8 token fetches + 8 authenticated HEADs. The Docker Hub
share was 6 refs → 18 requests (6 probes, 6 tokens, 6 HEADs), and none of them used pull quota
(`ratelimit-remaining` 100 before and after). Wren can skip the probe for Docker Hub (known realm) and
reuse one token per repo for its lifetime (`expires_in`), so at most 2 requests per Docker Hub ref.
Side note: `docker buildx imagetools inspect` runs in the client (this PC), not on LANBox; it needed a
temporary `DOCKER_CONFIG` without `credsStore: desktop`.

## Q2 Unraid template recreate

**Answer:** use **Method A, Unraid's own `update_container <name>`**, for template containers. It
recreates from the template XML, so every field comes out right and Unraid's status file, icon and WebUI
link stay in sync. It also works from inside a container (with host mounts, see below). The pure Docker
API recreate (Method B) keeps the container's configuration, but has two flaws Wren must fix if it ever
uses it for template containers: it pins the old image's labels/env, and it leaves Unraid's status file stale.

**How Unraid does it (read from the scripts, Unraid 7.4.0-rc.1):**
- `scripts/update_container <name>[*<name>…]` (PHP): finds the template whose `<Name>` matches in
  `/boot/config/plugins/dockerMan/templates-user/`, builds the `docker create/run` command with
  `xmlToCommand()`, pulls through the Docker API (and the pull's `Digest:` line →
  `DockerUpdate::setUpdateStatus(image, digest)` writes `local = remote = digest, status "true"` into
  `/var/lib/docker/unraid-update-status.json`), stops the container if it ran, removes it, runs the
  command (`docker run -d` if it was running), reconnects extra networks, and removes the old image if
  orphaned. Exit code 0 even on errors; errors only go to the nchan log (`publish()`).
- `scripts/rebuild_container <name>` recreates from the template **without pulling**, and stops the
  container afterwards unless it is in `/var/lib/docker/unraid-autostart`; not suitable for updates.
- The status file is keyed by **image ref with tag** (not by container). The "update ready" badge is
  `getUpdateStatus(image)`: stored `local` vs `remote`. `reloadUpdateStatus()` (the "Check for updates"
  button / cron) keeps the stored `local` value and only refreshes `remote`, so a stale `local` makes
  the tab show "update ready" for an image that is already current.
- Docker tab data = `DockerTemplates::getAllInfo()` → `/usr/local/emhttp/state/plugins/dynamix.docker.manager/docker.json`
  (`state` is a symlink to `/var/local/emhttp`). `unraid/ui.sh` calls it without a remote reload and
  prints what the tab shows.
- Creation for the test: the template was written by `setup.sh` and the container created with
  `update_container wren-spike-whoami` (container absent → pull + `docker create`), then `docker start`.

**Test container** (Review Focus 3): template `my-wren-spike-whoami.xml`, image
`localhost:5055/wren-spike/whoami:latest` (local `registry:2`), network `wren-spike-net` with fixed IP
`172.31.250.10`, port `18080→80/tcp`, env `WREN_SPIKE_NOTE=two words`, label `wren.spike=1`, bind
`/mnt/disk1/wren-spike/with space → /data`, restart `unless-stopped`, plus Unraid's own icon/WebUI labels.

| Run | Image after | Diff vs `before.txt` (besides image digest/ID) | Unraid status entry | Tab (`getAllInfo`) |
|---|---|---|---|---|
| A: `update_container` on host, v1.10→v1.11 | v1.11 (`4f90b33d…`) | none: IP, port, env, label, bind path with space, restart, Unraid labels identical; image labels correctly from the new image | `local = remote = 4f90…`, `"true"` | up to date, URL `http://10.0.3.26:18080/`, icon kept, template linked |
| B: `recreate-api.sh` on host, v1.11→v1.10 | v1.10 (`02d8fe03…`) | **image labels still say `version 1.11.0`, `revision 7e57…`** (copied from the old container's `Config.Labels`); everything else identical | **stale**: `local = 4f90…` (v1.11) while `02d8…` runs | "up to date" only because the cached `updated` is reused; the next real check compares `local 4f90…` with the registry and would say "update ready" (wrong) |
| B in `docker:29-cli`, socket only | v1.11 | same as B | unchanged | same as B |
| A in `alpine:3.22` + php84, mounts below | v1.10, then v1.11 | none | correct | correct |

Unraid's own update check could not reach the spike registry (`http://localhost:5055`, plain HTTP):
`reloadUpdateStatus` gave `remote: null, status: "undef"`. So "update ready" could not be shown live
for the spike image. The stale-`local` behaviour above is read from the code plus the status file
contents, not seen in the browser.

**Method A from a container needed:**
- mounts: `/var/run/docker.sock`, `/usr/local/emhttp` (scripts + PHP includes; read-only is enough apart
  from `state/`), `/var/local/emhttp` **read-write** (target of the `state` symlink; `docker.json` and
  `var.ini`; read-only gave `file_put_contents(... docker.json): Read-only file system`), `/boot/config`
  (templates, `docker.cfg`, `dynamix.cfg`), `/var/lib/docker` (only for `unraid-update-status.json`;
  mounting just `/var/lib/docker/unraid-update-status.json` + `unraid-autostart` should do, untested),
  and `/var/run/nginx.socket` (progress log; optional);
- PHP 8.4 in the image with `curl dom simplexml xml mbstring posix session ctype`, **`short_open_tag=On`**
  (Unraid's PHP uses `<?`; without it the script prints its source and does nothing), and **without the
  `gettext` extension** (it clashes with Unraid's own `_()`: `Cannot redeclare function _()`); `bash`, `docker-cli`;
- no `--privileged`, no extra capabilities, runs as root.
- It depends on Unraid's PHP code matching the PHP version in the agent image (host runs PHP 8.4.26).
  A host upgrade can break it; the agent should check `php -l` / exit status and fall back to Method B.
- One harmless stray usage message (`-s Suppress open and read errors`) from a host tool Alpine lacks
  (busybox) appears in the log.

**What Wren does for template containers:** pull through the Docker API first (preflight), then run
`update_container <name>` and verify the result itself (`update_container` exits 0 even on failure):
container running, image ID = the pulled one, status entry `local` = new digest. Fallback when the PHP
path is unavailable: Method B **plus** (1) drop `Config.Labels/Env/Cmd/Entrypoint/WorkingDir/User/Healthcheck`
values that equal the *old image's* defaults, so the new image's defaults apply (what Watchtower does),
and (2) write the status entry the way `setUpdateStatus` does (`local = remote = new index digest,
status "true"`, key = image ref with tag). Plain (non-template) containers use Method B with fix (1).

Timing: the whoami image is tiny; both methods took < 1 s after the pull. Real images are dominated by
the pull, which Wren does before stopping anything.

## Q3 Script stack (Fluxer)

**No stand-in needed.** The fresh install is the real installer (`https://fluxer.dev/install.sh`,
`sha256sum -c` OK, 2 318 lines). It ran on LANBox **without a public domain, DNS, certificates or
inbound ports**: `--domain 10.0.3.26` (LANBox's own IP passes `fluxer_valid_domain`, which wants
`[a-z0-9.-]` with a dot) with `--tls proxy --edge-bind 127.0.0.1:18088` (no Caddy ACME; the edge
publishes on loopback only). The Compose project name comes from `COMPOSE_PROJECT_NAME`, which the
installer honours (`fluxer_compose_project`), so the stack is `wren-spike-fluxer` with volumes
`wren-spike-fluxer_*`. Only LiveKit publishes on the LAN (`7881/tcp`, `7882/udp`); those ports were free.
Owner docs: https://docs.fluxer.app/operator/get-started/ (also `/operator/upgrading/`).

**What `install.sh` does (read from the script):**
- Flags: `--domain --email --engine --dir --ref --image-tag --tls bundled|proxy --edge-bind
  --non-interactive --dry-run --no-start --update --rollback --backup-dir --no-volume-backup
  --no-volume-compression --skip-backup-accept-data-loss --allow-root`. Interactive only for a missing
  `--domain` (and the "replace yourself with the current installer?" question below);
  `--non-interactive` turns both into errors.
- Exit codes: `0` ok, `1` usage, `2` unmet prerequisite (incl. running as root without
  `--allow-root`, no record to roll back), `3` refused to overwrite state (incl. "this installer is
  outdated" when non-interactive), `4` download/pull failed, `5` secret generation, `6` stack did not
  come up, `7` backup failed, `130` interrupted.
- **Self-check first:** every run compares itself with `https://fluxer.dev/install.sh.sha256`; if it
  differs and the run is non-interactive (or stdin is not a TTY) it exits **3** and changes nothing.
  So Wren's update script must download `install.sh` + `.sha256` and `sha256sum -c` *each time*
  before running it (as design §4 already says).
- `--update`, in order: fill missing required secrets in `.env` → record running image IDs per ref
  (`compose ps -aq` + `inspect`) and `FLUXER_IMAGE_TAG` into `backups/record-<ts>/` → copy `.env` and
  the stack files into the record → **backup**: `pg_dump --format=custom` through
  `compose exec -T postgres` (Postgres must run; started if not), then **stops the whole stack** and
  tars volume `<project>_seaweedfs-data` with a helper `alpine:3.22` container (`docker run -v
  <volume>:/data:ro -v <record dir>:/backup`), needing 110 % of the volume size free; starts the stack
  again → download fresh stack files from `github.com/fluxerapp/fluxer` at `--ref` (default `main` for
  tag `v1`) → refuse a Postgres major change → `compose pull` (fail = **4**, old images still run) →
  `compose up -d --remove-orphans` (fail = **6**) → restart `edge` if the mounted `Caddyfile` changed →
  wait up to **600 s** for every service `running` + `healthy|none` and `seaweedfs-init` exited 0
  (fail = **6**) → informational `/_health` probe.
- `--rollback`: newest record → if the recorded tag differs from `.env`, put the tag back in `.env`;
  otherwise `docker image tag <recorded ID> <ref>` for each ref → restore the stack files from the
  record → `compose up -d --remove-orphans` → restart `edge` → same readiness wait (fail = **6**).
  The database is **not** restored (the dump is only kept for a manual restore).
- Needs on the host/agent: `docker` + Compose ≥ 2.20.2 (≥ 2.24.4 with overlays), `curl`, `openssl`,
  `sh`; the helper image `alpine:3.22`; free space for the uploads tarball in `<dir>/backups`.

**Runs** (all from the agent container `docker:29-cli` + `apk add bash curl openssl`, socket mounted,
`/mnt/disk1/wren-spike/fluxer` mounted at the same path, `-w` that path,
`COMPOSE_PROJECT_NAME=wren-spike-fluxer`, `sh install.sh <mode> --allow-root --non-interactive --dir "$PWD"`):

| Step | Command | Exit | Duration | Result |
|---|---|---|---|---|
| 2 fresh install | `--domain 10.0.3.26 --tls proxy --edge-bind 127.0.0.1:18088` | 0 | 83 s (18 images pulled, ~5 GB) | 25 containers, all `healthy` (+ `seaweedfs-init` exited 0); `/_health` probe on `https://10.0.3.26` answered 000 (informational only, expected without TLS) |
| 3 update, same path | `--update` | 0 | 40 s | record `backups/record-…125934Z` with `.env`, stack files, `images` (19 refs), `image-tag`, `fluxer.dump` (custom format), `seaweedfs-data.tgz`; whole stack stopped and started for the copy, then recreated; all healthy |
| 4 update, **different path** (dir mounted at `/mnt/disk1/wren-spike/work-elsewhere`) | `--update` | **6** | 31 s | see below |
| 5 rollback, same path | `--rollback` | 0 | 25 s | restored the stack files from the newest record, `up -d`, restarted `edge`; every container's image ID identical to before step 3; `edge` back on the right bind |

**Same-path rule (Review Focus 4), what broke at a different path:** Compose runs in the agent, but
the **daemon resolves bind sources on the host**. `./Caddyfile` became
`/mnt/disk1/wren-spike/work-elsewhere/Caddyfile`, which did not exist on the host, so the daemon
**created it as an empty directory** and `edge` failed to start (`error mounting … Caddyfile … not a
directory`) → `compose up -d` failed → exit 6. The installer's helper container (`docker run -v
<record dir>:/backup`) also wrote `seaweedfs-data.tgz` to the host path, **outside the real record**,
so that record silently had no uploads backup. And the `com.docker.compose.project.working_dir` label
moved to the wrong path. (The plan said `/work`; a path under `/mnt/disk1/wren-spike/` was used so that
everything the daemon creates stays inside the spike's own directory. The behaviour is the same.)
Rule: **the agent must mount every stack directory at the identical host path** (Wren refuses a script
stack or Compose stack whose `working_dir` is not visible at the same path inside the agent).

**Failure method used for step 5:** the step-4 run (wrong mount path, a realistic misconfiguration)
was the failed update (exit 6, one service down). The rollback at the right path fixed it. Fluxer's
`v1` tag did not move during the spike, so "previous image IDs" equal the current ones: the rollback
mechanics (record → retag → restore files → `up -d` → readiness) were exercised, but not a real
image downgrade.

**Finding Compose working dirs (§14):** every Compose container carries
`com.docker.compose.project`, `com.docker.compose.project.working_dir` and
`com.docker.compose.project.config_files`. Wren reads them from the Docker API; the files are
reachable when that path is mounted at the same path in the agent (above), otherwise Wren shows the
stack as "files not reachable: mount <working_dir> into the agent" and offers notify-only.

**Agent config entry Wren needs for Fluxer** (in `agent.toml`, §8):

```toml
[[script_stack]]
name = "fluxer"
compose_project = "fluxer"                    # discovery, digests, health
working_dir = "/mnt/cache/appdata/fluxer"     # must be mounted at the same path
handles_own_backup = true                     # Wren skips stop + snapshot (Postgres must run for pg_dump)
env = { COMPOSE_PROJECT_NAME = "fluxer" }     # only if the project name differs from `name:` in the file
timeout = "20m"                               # 600 s readiness wait + pulls + volume copy

[script_stack.update]
command = """
curl -fsSLO https://fluxer.dev/install.sh && curl -fsSLO https://fluxer.dev/install.sh.sha256 \
  && sha256sum -c install.sh.sha256 \
  && sh install.sh --update --allow-root --non-interactive --dir "$PWD"
"""

[script_stack.rollback]
command = "sh install.sh --rollback --allow-root --non-interactive --dir \"$PWD\""
```

Exit-code mapping for Wren: `0` success; `4` (pull failed, old images still run) and `7` (backup
failed, untouched) = "update failed, nothing changed" (no rollback needed); `6` = "stack not up" →
run rollback; `3` = installer outdated or refused (re-download); `2` = prerequisite or no record. The
agent image needs `sh`, `curl`, `openssl`, `docker` CLI + Compose ≥ 2.24.4, and must allow the
installer to run `alpine:3.22` helper containers.

## Q4 Snapshots

**Answer:** reflink copies (`cp -a --reflink=always`) are near-instant and use almost no space on
both xfs and btrfs, need **no privileges** inside a container, and work for plain directories. btrfs
subvolume snapshots are even faster, but only apply when the target *is* a subvolume. On Tower it is
not (see below), and deleting a snapshot needs `CAP_SYS_ADMIN`. So Wren's default is the reflink copy,
with subvolume snapshots as an extra for targets that are subvolumes.

Test data (`make-data.sh`): 2.0 GB = one 1.5 GB random file, 5 000 × 100 KB files, a hard link to the
big file, a symlink, a SQLite DB with 3 rows, plus `SHA256SUMS`. The "damage" step overwrites 10 MB
in the big file, deletes 100 small files and adds a SQLite row; `verify.sh` then reports
`103 bad checksums, sqlite rows FAIL`, and after the restore `verify OK` (checksums, hard link
still one inode, symlink target, 3 rows).

| Filesystem (LANBox) | Method | Time | Extra space | Restore | Verify after restore |
|---|---|---|---|---|---|
| xfs `/mnt/disk1` (`reflink=1`) | `cp -a --reflink=always` | **0.21 s** | +3 MB | rename aside + reflink copy back: 0.21 s | OK |
| xfs `/mnt/disk1` | `cp -a --reflink=never` (plain) | 1.80 s (warm page cache, NVMe-class disk; cold reads will be slower) | **+2 027 MB** | n/a | n/a |
| btrfs (8 GB loop image on disk1) | `btrfs subvolume snapshot -r` of subvolume `appdata/target` | **0.01 s** | ~0 | rename aside + writable snapshot of the read-only one: 0.01 s | OK |
| btrfs | `cp -a --reflink=always` of a **plain directory** (not a subvolume) | 0.14 s | +5 MB | 0.13 s | OK |

The btrfs filesystem held target + snapshot + three reflinked copies of the plain dir with
`Data used = 1.99 GiB`, i.e. the copies share all blocks until written.

**Inside a container** (`alpine:3.22` + `coreutils btrfs-progs`, paths bind-mounted):

| Operation | Default caps | `--cap-add SYS_ADMIN` |
|---|---|---|
| reflink copy on xfs | OK | OK |
| reflink copy on btrfs | OK | OK |
| `btrfs subvolume snapshot -r` | OK | OK |
| `btrfs subvolume delete` | **EPERM** | OK |

(Busybox `cp` has no `--reflink`; the agent image needs GNU coreutils `cp`, or Wren calls the
`FICLONE` ioctl itself. Without `CAP_SYS_ADMIN`, a snapshot could still be removed if the filesystem
is mounted with `user_subvol_rm_allowed`; Unraid does not set that.)

**Tower, read-only:** `btrfs subvolume list /mnt/cache` printed **nothing**;
`btrfs subvolume show /mnt/cache/appdata` → `Not a Btrfs subvolume`; `stat -f /mnt/cache/appdata/fluxer`
→ `btrfs`; inode numbers: `/mnt/cache` 256 (top-level subvolume), `appdata` 259, `appdata/fluxer`
121065518 (subvolume roots are always inode 256). So **appdata and each app dir are plain directories
on the pool's top-level subvolume**. A btrfs snapshot there would have to snapshot the whole pool,
so Wren uses reflink copies on Tower.

**Chosen method per filesystem, in fallback order** (the agent probes each data path once):
1. The path is the root of a **btrfs subvolume** (inode 256 + `btrfs subvolume show` OK) and the agent
   has `CAP_SYS_ADMIN` → `btrfs subvolume snapshot -r`; restore by swap.
2. **Reflink copy** (`FICLONE`; btrfs, xfs with `reflink=1`, also bcachefs/ocfs2) into a sibling
   `.wren-snapshots/<target>/<run>` dir on the **same filesystem**; restore = move the live dir aside,
   reflink-copy the snapshot back, delete the aside dir after the health check. Probe: try one
   `FICLONE` on a temp file in that dir.
3. **Plain copy** preserving hard links, ACLs, xattrs and owners (`cp -a`), only after a free-space
   check (size × 1.1, as Fluxer does). Refuse the update otherwise (§7).
4. ZFS: untested (no pool on either server); treated as plain copy until someone tests `zfs snapshot`.

**Unraid user shares:** `cp --reflink=always` through `/mnt/user/wren-spike/…` (FUSE `shfs`) failed
with `Operation not supported`, while the same data under `/mnt/disk1/wren-spike/…` reflinks fine. Wren
must resolve a `/mnt/user/<share>/<rest>` data path to the real location (`/mnt/cache/<share>/<rest>`,
`/mnt/<pool>/…` or `/mnt/diskN/…`, whichever exists) before snapshotting. Unraid's own `docker_rm` script does this with
`getfattr -n system.LOCATIONS --absolute-names --only-values /mnt/user/<share>` (on LANBox it printed `disk1`). The agent therefore mounts
the pool/disk paths, not only `/mnt/user`. A share spread over several disks falls back to a per-disk
reflink or a plain copy.

## Decision: agent runtime form

**`wren-agent` runs as a container** (Alpine image with the static agent binary). No host binary is
needed: every operation in the spike worked from a container. It is **not `--privileged`**, and adds
no capabilities by default.

| Requirement | Evidence | Choice |
|---|---|---|
| Check digests, pull, recreate plain containers | Q1, Q2 Method B from `docker:29-cli` with only the socket | mount `/var/run/docker.sock` (root-equivalent, documented, §8) |
| Unraid template containers stay in sync with the UI | Q2: `update_container` from `alpine:3.22` + php84 worked; Method B leaves the status file stale and pins old image labels | Method A: the image carries `php84` (`curl dom simplexml xml mbstring posix session ctype`, **no `gettext`**, `short_open_tag=On`) and runs the host's own `/usr/local/emhttp/plugins/dynamix.docker.manager/scripts/update_container <name>`; mounts `/usr/local/emhttp` (ro), `/var/local/emhttp` (rw), `/boot/config` (rw), `/var/lib/docker` (rw; only `unraid-update-status.json` + `unraid-autostart` are used); then Wren verifies the result itself. Fallback when PHP fails (host PHP changed, script missing): Method B + clean image defaults + write the status entry |
| Script stacks (Fluxer) run their own installer against the host daemon | Q3: works from a container; a different mount path breaks binds and backups (exit 6) | **same-path rule**: every allowed root is mounted at the identical path (`-v /mnt/cache/appdata:/mnt/cache/appdata`, `-v /mnt/user/appdata:/mnt/user/appdata`, …); the image carries `sh bash curl openssl docker-cli docker-cli-compose` (Compose ≥ 2.24.4) |
| Snapshots of data paths | Q4: reflink copies need no capability; btrfs snapshots need `CAP_SYS_ADMIN` only to delete; Tower's appdata is no subvolume; `/mnt/user` (shfs) cannot reflink | reflink copy by default, done on the real pool/disk path (`/mnt/cache/...`, `/mnt/diskN/...`, mounted at the same path); GNU coreutils in the image (or `FICLONE` from Rust). `--cap-add SYS_ADMIN` is an opt-in only for targets that are btrfs subvolumes |
| Run as | the socket and preserving file owners in `cp -a` | root inside the container |

Unraid template for the agent (phase 6): `--restart unless-stopped`, the socket, the Unraid mounts
above, and the data roots at identical paths, all declared in the template so the owner sees exactly
what it gets.

Not tested (left open): running the host's PHP through `nsenter -t 1 -m` from a `--privileged
--pid=host` container (avoids shipping PHP, but needs full privileges); narrowing the
`/var/lib/docker` mount to the two files; Method A against a registry Unraid can actually check (the
spike registry was plain HTTP, so Unraid's own check returned `undef`).

## Cleanup

Done on 2026-10-23 with `cleanup.sh keep-whoami` (copy at `/mnt/disk1/wren-spike/cleanup.sh` on LANBox):
- Removed: `wren-spike-registry` (tmpfs storage), all agent/test containers (`--rm` or removed), the
  Fluxer stack `wren-spike-fluxer` with `down -v` (25 containers, 7 volumes, network), all images the
  spike pulled (`registry:2`, `docker:29-cli`, `traefik/whoami:v1.10/v1.11`, the 18 Fluxer images
  incl. `caddy`, `seaweedfs`, `meilisearch`, `livekit`, `nats`, `postgres:16-alpine`, `valkey`), the
  btrfs loop image and mount, all test data, `/tmp/wren-spike-tokens.log`. `alpine:3.22` existed
  before the spike and was kept.
- **Kept on purpose for the owner's Docker-tab check (Q2):** container `wren-spike-whoami` (image
  `localhost:5055/wren-spike/whoami:latest` = whoami v1.11, last updated by Method A from a container),
  network `wren-spike-net`, template `my-wren-spike-whoami.xml`, its icon in
  `/var/local/emhttp/plugins/dynamix.docker.manager/images/`, its status entry, and
  `/mnt/disk1/wren-spike/{with space,cleanup.sh}`. Since the registry is gone, Unraid's next update
  check shows that container's update state as "not available", which is expected.
  Final removal: `ssh root@10.0.3.26 sh /mnt/disk1/wren-spike/cleanup.sh`. It removes the container the
  way the tab's "remove container + image" does (`removeContainer(name, false, 2)` +
  `removeImage(ref)`, which also drops the status entry), then the template, icon, network and
  `/mnt/disk1/wren-spike`.
- `df -h /mnt/disk1`: before the spike `929G size, 89G used, 840G avail`; at the peak `94G used`;
  after cleanup `89G used, 840G avail`.
- Nothing on Tower was written; only `btrfs subvolume list/show` and `stat` were run there.
