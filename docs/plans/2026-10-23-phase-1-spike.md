# Wise Wren – Phase 1 spike implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. This is a throwaway spike: each task ends in **evidence written to `FINDINGS.md`**, not in production code, so the test step of each task is an evidence check rather than a unit test.

**Goal:** Answer the four risky questions in the design (§12, phase 1) with evidence from LANBox, and decide the agent's runtime form, before any Wise Wren code is written.

**Architecture:** A throwaway local repository `E:\GitHub Repos\wren-spike` holds shell scripts and the findings. Everything runs on LANBox through `docker --context unraid` and `ssh root@10.0.3.26`. Tower is only read, never written.

**Tech Stack:** POSIX sh + curl + jq, Docker CLI 29 / Compose v5 on LANBox (Unraid 7.4.0-rc.1), Unraid dockerMan scripts (PHP), btrfs-progs, xfs reflink.

**Spec:** `docs/design.md` (§4 targets, §6 update run, §7 snapshots, §8 security, §12 phase 1, §14 left to the builder).

## Global Constraints

- Docker work only on LANBox (`docker --context unraid`, `ssh -o BatchMode=yes root@10.0.3.26`). **Tower (10.0.3.25, `sylo-unraid`) is read-only:** `ls`, `cat`, `stat`, `btrfs subvolume list/show`, `docker inspect` only.
- Every container, network, volume, image tag, template and directory the spike creates is named `wren-spike-*` (templates: `my-wren-spike-*.xml`; data: `/mnt/disk1/wren-spike/`). Never stop, change, remove or prune anything else.
- Disk: at most 100 GB used by the spike; `/mnt/disk1` keeps ≥ 300 GB free (`df -h /mnt/disk1` before each heavy step).
- Registry checks are anonymous and **must not pull**: manifest `HEAD` (or `GET` only where `HEAD` gives no digest). Stay under 20 Docker Hub requests per run.
- ZFS is not tested: neither server has a ZFS pool. Wren treats ZFS like "plain copy" until someone tests it (§13 best-effort).
- Nothing from the spike is merged into `Wise-Wren`; only `FINDINGS.md` is copied to `Wise-Wren/docs/spike-findings.md` at the end.
- Facts already known (2026-10-23): LANBox `/mnt/disk1` is xfs with `reflink=1`, no cache pool, `mkfs.btrfs` and `php` present, dockerMan scripts in `/usr/local/emhttp/plugins/dynamix.docker.manager/scripts/` (`update_container`, `rebuild_container`, …), 14 user templates in `/boot/config/plugins/dockerMan/templates-user/`. Tower `/mnt/cache` is btrfs; Fluxer lives in `/mnt/cache/appdata/fluxer` (`docker-compose.yml`, `install.sh`, `install.sh.sha256`, extra compose files).

## Review Focus

1. **Multi-arch images:** a container's `RepoDigests` holds the *index* digest, while a `HEAD` on a platform tag can return a different media type. Expected: "update available" only when the index digest changes. Pinned in Task 1 (`nginx`, `keen-kestrel`, both multi-arch).
2. **Containers with no usable digest** (built locally, or pulled by digest): expected "unknown", never "update available". Pinned in Task 1.
3. **Unraid custom networks with fixed IPs** (`br0`-style) and ports, env values with spaces, labels and a bind path with a space: a recreate must keep all of them. Pinned in Task 2 (the test container uses all five).
4. **Compose bind mounts with relative paths** run from inside an agent container resolve on the *host*. Expected: they only work when the agent mounts the stack directory at the same path. Pinned in Task 3.
5. **Snapshot restore while the target is stopped** must give back exactly the old bytes (checksums), on xfs reflink and on btrfs. Pinned in Task 4.

---

### Task 0: Spike repository and findings skeleton

**Files:**
- Create: `E:\GitHub Repos\wren-spike\README.md` (one paragraph: throwaway, see FINDINGS.md)
- Create: `E:\GitHub Repos\wren-spike\FINDINGS.md` with headings `## Q1 Registry digests`, `## Q2 Unraid template recreate`, `## Q3 Script stack (Fluxer)`, `## Q4 Snapshots`, `## Decision: agent runtime form`, `## Cleanup`
- Create: `E:\GitHub Repos\wren-spike\lib.sh` with `lanbox() { ssh -o BatchMode=yes root@10.0.3.26 "$@"; }`, `dk() { docker --context unraid "$@"; }` and `free_ok()` (fails when `/mnt/disk1` has < 300 GB free)

- [ ] **Step 1:** `git init` the folder, add the three files, run `. ./lib.sh && free_ok && echo ok`. Expected: `ok`.
- [ ] **Step 2:** `lanbox mkdir -p /mnt/disk1/wren-spike` and commit (`chore: spike skeleton`).

### Task 1: Registry digest checks (Q1)

**Files:**
- Create: `registry/digest.sh <image-ref>` → prints `ref<TAB>registry-digest<TAB>media-type<TAB>ratelimit-remaining` (empty fields when not given)
- Create: `registry/compare.sh` → for every running container on LANBox: `name<TAB>image<TAB>local-digest<TAB>remote-digest<TAB>status` with status `current` | `update` | `unknown`

**Interfaces:**
- Produces: the decided check method (anonymous token endpoints per registry, `Accept` header list, which digest to compare) recorded in FINDINGS Q1; Phase 3 implements it in Rust.

- [ ] **Step 1: `digest.sh`**: parse the ref into registry/repo/tag (Docker Hub default `registry-1.docker.io`, `library/` for one-part names), fetch an anonymous pull token (`auth.docker.io/token?service=registry.docker.io&scope=repository:<repo>:pull`; GHCR `ghcr.io/token?scope=repository:<repo>:pull`), then `curl -sI` the manifest with `Accept: application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json` and read `docker-content-digest`, `content-type`, `ratelimit-remaining`.
- [ ] **Step 2: Evidence:** run it for `nginx:1.27`, `ghcr.io/ferdinand99/keen-kestrel:latest`, `quay.io/prometheus/node-exporter:latest`, `lscr.io/linuxserver/syncthing:latest`. Expected: each prints a `sha256:` digest; Docker Hub `ratelimit-remaining` is the same before and after the `HEAD`s (HEAD does not count). Compare each digest with `dk buildx imagetools inspect <ref> --format '{{json .Manifest.Digest}}'` (run on LANBox): they must match.
- [ ] **Step 3: `compare.sh`**: `dk ps --format '{{.Names}} {{.Image}}'`, `dk image inspect --format '{{json .RepoDigests}}'` for the local digest, `digest.sh` for the remote one; cache per image ref in one run. Images with empty `RepoDigests` or a ref that is itself `@sha256:` → `unknown`.
- [ ] **Step 4: Evidence:** run `compare.sh` (read-only on the user's containers). Expected: every row has a status; at least one `current`; record any `update` rows and verify one by hand with `imagetools inspect`. Record the request count and the Docker Hub remaining limit.
- [ ] **Step 5:** Write FINDINGS Q1 (method, the table, rate-limit numbers, the multi-arch rule) and commit (`spike: registry digest checks`).

### Task 2: Recreate an Unraid template container and keep the UI in sync (Q2)

**Files:**
- Create: `unraid/setup.sh`: starts `wren-spike-registry` (`registry:2`, port 5055 on LANBox), pushes `traefik/whoami:v1.10` as `localhost:5055/wren-spike/whoami:latest`, creates network `wren-spike-net` (subnet `172.31.250.0/24`), writes `/boot/config/plugins/dockerMan/templates-user/my-wren-spike-whoami.xml` and creates the container from it with Unraid's own `rebuild_container`/`update_container` script (inspect the script for the exact call; record it)
- Create: `unraid/bump.sh`: pushes `traefik/whoami:v1.11` to the same tag (new digest = "update available")
- Create: `unraid/recreate-api.sh <name>`: recreate through the Docker API only (inspect → stop → rename old to `<name>-wren-old` → create with the same config and the new image → start → remove old)
- Create: `unraid/state.sh <name>`: prints a normalised `docker inspect` (image digest, env, labels, mounts, ports, networks + IP, restart policy, `net.unraid.docker.*` labels) and the container's entry in `/var/lib/docker/unraid-update-status.json`

The test container must carry: fixed IP `172.31.250.10` on `wren-spike-net`, port `18080→80`, env `WREN_SPIKE_NOTE="two words"`, a label `wren.spike=1`, and a bind mount from `/mnt/disk1/wren-spike/with space` to `/data` (Review Focus 3).

- [ ] **Step 1:** Run `setup.sh`, then `state.sh wren-spike-whoami > before.txt`. Expected: container running with all five properties; Unraid status entry present.
- [ ] **Step 2: Method A (Unraid's script):** `bump.sh`, then `lanbox /usr/local/emhttp/plugins/dynamix.docker.manager/scripts/update_container wren-spike-whoami`; `state.sh > after-a.txt`. Expected: new digest; every other field identical to `before.txt`; status entry says up to date. Ask the owner to look at LANBox's Docker tab once and confirm the container shows no "update ready" and keeps its icon and WebUI link (one screenshot).
- [ ] **Step 3: Method B (Docker API):** `bump.sh` back to `v1.10`, run `recreate-api.sh wren-spike-whoami`, `state.sh > after-b.txt`. Expected: same field check; record what the Unraid status entry and the Docker tab show (they may still say "update ready" until Unraid rechecks; record exactly).
- [ ] **Step 4: Inside a container:** repeat Method B from `docker:29-cli` with `/var/run/docker.sock` mounted. Then try Method A from a container with `/usr/local/emhttp`, `/boot/config` and `/var/lib/docker` mounted read-write plus `php` installed; record whether it works and which mounts/privileges it needed.
- [ ] **Step 5:** Write FINDINGS Q2 (diffs, UI result, which method Wren uses for template containers and what it needs on the host) and commit (`spike: unraid template recreate`).

### Task 3: Script stack with Fluxer's real installer (Q3)

**Files:**
- Create: `fluxer/fetch.sh`: downloads `install.sh` and `install.sh.sha256` from `https://fluxer.dev/` into `/mnt/disk1/wren-spike/fluxer`, runs `sha256sum -c`
- Create: `fluxer/agent-run.sh <args…>`: runs `sh install.sh <args…> --allow-root` from a container (`docker:29-cli` plus `bash curl` and whatever `install.sh` needs) with the Docker socket and `/mnt/disk1/wren-spike/fluxer` mounted **at the same path**, working directory the same; prints the exit code and the duration

- [ ] **Step 1:** `fetch.sh`, then read `install.sh` and record in FINDINGS Q3: the fresh-install flags, what it asks interactively, what `--update` and `--rollback` do (in order), its exit codes, and what it needs from the host (`pg_dump` via the container, volume copy paths, free space). **Stop and ask the owner** if a fresh install needs a public domain, DNS or ports LANBox can't give; in that case write a stand-in stack (`fluxer/mock/`: Postgres + whoami, plus `install.sh` that mimics the recorded steps and exit codes) and continue with it, saying so in FINDINGS.
- [ ] **Step 2:** Fresh install of a throwaway instance with Compose project name `wren-spike-fluxer` (via the installer's own flags/env, or `COMPOSE_PROJECT_NAME`). Expected: the stack is up and healthy; `df` still ≥ 300 GB free.
- [ ] **Step 3: Update from an agent container:** `agent-run.sh --update`. Expected: exit 0, the stack healthy afterwards, the installer's own backup present. Record duration and the log tail.
- [ ] **Step 4: Relative paths (Review Focus 4):** run `agent-run.sh --update` once more with the directory mounted at a *different* path (`/work`). Expected: failure or wrong mounts; record exactly what breaks.
- [ ] **Step 5: Failure and rollback:** make the update fail (e.g. point one service at a tag that doesn't exist in the stack file the installer refreshes, or stop Postgres mid-run; record the method), check the exit code (expect 6 or another non-zero), then `agent-run.sh --rollback`. Expected: the stack is back on the previous image IDs and healthy.
- [ ] **Step 6:** Write FINDINGS Q3 (the agent config entry Wren needs for Fluxer, timings, exit codes, the same-path rule) and commit (`spike: fluxer script stack`).

### Task 4: Snapshots: btrfs, xfs reflink, plain copy (Q4)

**Files:**
- Create: `snap/make-data.sh <dir>`: 2 GB of mixed data (a 1.5 GB file, 5 000 small files, a hard link, a symlink, a SQLite file with a few rows) and a `SHA256SUMS` file
- Create: `snap/xfs.sh`: times `cp -a --reflink=always` and `cp -a` of the data dir on `/mnt/disk1`, restores into place, verifies `SHA256SUMS`
- Create: `snap/btrfs.sh`: makes `/mnt/disk1/wren-spike/btrfs.img` (8 GB), `mkfs.btrfs`, loop-mounts it at `/mnt/disk1/wren-spike/btrfs`, creates subvolume `appdata/target`, runs `make-data.sh` in it, times `btrfs subvolume snapshot -r`, changes data, restores by subvolume swap, verifies `SHA256SUMS`; also times `cp -a --reflink=always` inside the same filesystem for a plain directory that is not a subvolume

- [ ] **Step 1:** Run `xfs.sh`. Expected: reflink copy ≪ plain copy; checksums OK after restore; `df` shows little extra space used by the reflink copy.
- [ ] **Step 2:** Run `btrfs.sh`. Expected: snapshot < 1 s; checksums OK after restore; reflink copy of a non-subvolume directory works.
- [ ] **Step 3: Inside a container:** repeat one reflink copy and one btrfs snapshot from a `wren-spike` container with the paths bind-mounted; record which capabilities were needed (`--privileged`, `CAP_SYS_ADMIN`, none).
- [ ] **Step 4: Tower, read-only:** `ssh sylo-unraid 'btrfs subvolume list /mnt/cache; btrfs subvolume show /mnt/cache/appdata; stat -f -c %T /mnt/cache/appdata/fluxer'`. Record whether appdata (or each app dir) is a subvolume, which tells whether Wren can use snapshots there or reflink copies.
- [ ] **Step 5:** Write FINDINGS Q4 (timings table, chosen method per filesystem, fallback order), unmount and delete the btrfs image, commit (`spike: snapshots`).

### Task 5: Decide the agent's runtime form, clean up, hand over

**Files:**
- Modify: `FINDINGS.md` (`## Decision: agent runtime form`, `## Cleanup`)
- Create (in the real repo): `E:\GitHub Repos\Wise-Wren\docs\spike-findings.md` (copy of FINDINGS.md)
- Modify: `E:\GitHub Repos\Wise-Wren\docs\design.md` §3 "Agent" and §14 bullets that the spike settles (snapshot method, agent form, how Compose working dirs are found)

- [ ] **Step 1:** Fill the decision from Q2–Q4: container or host binary (or container with listed mounts/capabilities), the mounts and privileges, how it reaches the Unraid scripts if Method A won, and the same-path rule. One table: requirement → evidence → choice.
- [ ] **Step 2: Cleanup:** remove `wren-spike-*` containers, network, registry and image tags, the `my-wren-spike-*.xml` template, the spike entry in `unraid-update-status.json` (via Unraid's own removal, `docker_rm` if that is what the UI uses), and `/mnt/disk1/wren-spike`. Expected: `dk ps -a --filter name=wren-spike` empty, `lanbox ls /mnt/disk1/wren-spike` fails, the Docker tab shows no spike container, `df` back to the starting value ± 1 GB.
- [ ] **Step 3:** Copy the findings and update `design.md` in `Wise-Wren` on branch `docs/spike-findings`; open a PR (`docs: record the phase 1 spike findings`).

---

## After the spike: the remaining phases

Each later phase gets its own plan, written when the previous one is merged, because the spike decides the agent's form and protocol details they depend on. Sizes follow Keen Kestrel's experience on vole-kit.

| Phase | Plan written | Builds on | Rough size |
|---|---|---|---|
| 2 Skeleton on vole-kit 0.2 + agent registration/online status | after the spike PR | `examples/kit-demo` in Vigilant-Vole, Kestrel's phase 4, VV's `src/agent/` | 1 agent run |
| 3 Discovery + version checks + new-version notifications | after phase 2 | Q1 method, Q2/Q3 discovery | 1–2 agent runs |
| 4 Update runs, snapshots, health, rollback, history | after phase 3 | Q2 recreate method, Q3 script contract, Q4 snapshot method | 2 agent runs |
| VV change (before 5) Monitors keep checking during maintenance; real result via API, public "maintenance", no alerts | with phase 4 | `src/monitor/alerting.rs` in Vigilant-Vole | 1 agent run, VV minor release |
| 5 VV integration, scheduling, auto mode with windows and exclusions | after the VV release | VV API | 1–2 agent runs |
| 6 API + MCP, hardening, docs, site, Unraid templates → **v0.1** | after phase 5 | Kestrel's phase 7/10 | 1 agent run |
