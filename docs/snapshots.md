# Snapshots and rollback

Before a target is updated, the agent takes a snapshot of its data, so a failed update can be undone with the data
as it was. Snapshots are short-lived **rollback points**.

> **Snapshots are not backups.** They live on the same disk or pool as the data, Wren keeps only the last two
> per target, and they vanish when the disk does. Keep your own backups, offsite (for example appdata to cloud
> storage with rclone or Duplicati), whatever Wren does.

## What is snapshotted

A target's own bind mounts under the agent's `roots`:

- never a root itself (mounting all of `/mnt/user/appdata` into one container doesn't make Wren copy all of it);
- never a folder another container also mounts (the run log lists both kinds of skipped folders);
- never named Docker volumes (listed as "not snapshotted").

Script stacks with `handles_own_backup = true` (like Fluxer) are not stopped or snapshotted by Wren: their own
script does that.

## How

Per folder, in this order:

1. **btrfs subvolume snapshot**, when the folder is a subvolume and the agent has `CAP_SYS_ADMIN`;
2. **reflink copy** (`cp --reflink=always`): instant on btrfs and xfs with `reflink=1`; an Unraid share path
   (`/mnt/user/…`) is resolved to its pool first (`/mnt/cache/…`), which is why the agent mounts both;
3. **plain copy** (`cp -a`) after checking that the folder's size plus 10 % fits.

A run is refused before anything stops when the snapshot wouldn't leave 1 GB (or 2 %) free. Snapshots are kept in
`<root>/.wren-snapshots/<target>/<run>/`; the last two per target are kept.

**Not tested yet:** btrfs subvolume snapshots on real Unraid pools (the default agent has no `CAP_SYS_ADMIN`, so
Unraid uses reflink copies), and ZFS (treated as a plain copy, which can be slow and large). On ext4 and other
Linux filesystems the plain copy is used.

## Rollback

- **Automatic:** when the health check fails, the agent stops the target, moves the live folder aside, puts the
  snapshot back (the moved folder is deleted only once the copy is back), restores the old container or images and
  Compose files (a script stack runs its rollback script) and checks health again.
- **By hand:** on the run page of the target's newest successful update, **Roll back** (editors; also
  `POST /api/v1/runs/{id}/rollback`), while the agent still keeps the old image and snapshot. **Everything the app
  wrote since the update is lost**; the page says so.
- A rollback only touches the target it undoes.
- If a rollback fails, the agent stops and leaves everything as it is: the run is **rollback failed** ("needs
  you"), and an urgent notification goes out.
