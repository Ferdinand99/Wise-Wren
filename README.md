<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/wren-logo-dark.svg">
    <img src="assets/brand/wren-logo-light.svg" alt="Wise Wren" width="420">
  </picture>
</p>

<p align="center"><strong>Keeps your Docker containers up to date, without surprises.</strong></p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License: MIT"></a>
  <a href="https://hedgerow.tools/Wise-Wren/"><img src="https://img.shields.io/badge/website-hedgerow.tools%2FWise--Wren-black.svg" alt="Website"></a>
</p>

> **Status: early development, nothing to install yet.** Wren lists what runs on each server, checks for new
> versions and runs the updates you approve, with a snapshot, a health check and an automatic rollback.
> Scheduling, automatic mode and the Vigilant Vole integration come next; there is no release or published image
> yet. The design is in [`docs/design.md`](docs/design.md), the spike's results in
> [`docs/spike-findings.md`](docs/spike-findings.md). Watch or star the repository to hear about the first
> release.
>
> To try the skeleton from a checkout, see [`docs/development.md`](docs/development.md): `cargo run` serves Wren on
> port 3003, and each server's page shows the `docker run` line for its agent.

Wise Wren ("Wren") will keep the Docker containers on your servers up to date. It checks for new versions on a
schedule you choose, tells you about them, and updates only what you approve, or what you have explicitly allowed
it to update on its own.

Today, keeping a home server current often means a manual check every day. Unraid's update button covers
containers made from its templates, but not Compose stacks or apps that update with a vendor's own script. Other
tools either only notify (Diun, What's Up Docker) or update blindly (Watchtower). None of them pause your
monitoring, take a snapshot first, check health afterwards and roll back.

## What it will do

- **Notify, approve or automatic.** Each server has a default mode and each target can override it. Approve is
  the default: one click updates now, or at a time you choose.
- **Scheduled updates.** Approve during the day, update tonight. A scheduled update shows as planned maintenance
  in [Vigilant Vole](https://github.com/Ferdinand99/Vigilant-Vole) ahead of time.
- **Safe updates, every time.** Each run goes through the same steps:
  1. preflight: the new image is pulled before anything stops;
  2. a maintenance window opens in Vigilant Vole;
  3. the target stops gracefully;
  4. a snapshot of its data is taken;
  5. the update runs;
  6. a health check: the containers are healthy and the linked monitors stay up;
  7. on failure, an automatic rollback of the image or Compose files and the data;
  8. the window closes and you get the result.
- **Notifications** through the same ~40 channels as Vigilant Vole, from ntfy on your phone to email and
  Discord. One per new version, with a digest option.
- **One Wren, several servers**, through a small agent on each.

### Three kinds of targets

| Kind | Example | New version means |
|---|---|---|
| Containers, including Unraid template containers | `nextcloud` | The image in the registry has changed |
| Docker Compose stacks | `immich` | Any of the stack's images has changed |
| Script-updated stacks | Fluxer, updated with the vendor's installer | A source such as GitHub releases reports a newer version |

### Wren never changes anything you haven't allowed

With automatic updates off, which is the default for every server and target, Wren updates nothing on its own.
Automatic updates only run inside a window you set, and any container or stack can be excluded. An approval is
for one version: if a newer one appears before it runs, Wren asks again. Scripts are defined on each server,
and Wren can only ask for them by name, so it can never become a remote shell.

## Planned for v1

| Updates | Around them |
|---|---|
| Containers, Unraid templates, Compose stacks, script stacks | Docker Hub, GHCR, Quay and any OCI registry; GitHub releases |
| Notify, approve or automatic, per server and per target | Release notes with each update, when Wren can find them |
| Scheduled updates and automatic windows with exclusions | Vigilant Vole maintenance windows and monitor-based health checks |
| Snapshot before, health check after, automatic rollback | Notifications to ~40 channels |
| A history of every run, step by step | REST API and MCP tools for everything the web UI can do |

Not in v1: the iOS app (planned later), updating Unraid OS, plugins, VMs, LXC or Kubernetes, building images
from source, and offsite backups (Wren's snapshots are short-lived rollback points, not backups). See
[`docs/design.md`](docs/design.md) for the full scope and the phases.

Wren is part of [Hedgerow](https://hedgerow.tools/), a family of small self-hosted apps, with
[Vigilant Vole](https://hedgerow.tools/Vigilant-Vole/), [Nimble Newt](https://hedgerow.tools/Nimble-Newt/) and
[Keen Kestrel](https://hedgerow.tools/Keen-Kestrel/). Like its siblings, it is one Rust binary with SQLite in
the family's design, plus a small agent for each server.

## License

[MIT](LICENSE)
