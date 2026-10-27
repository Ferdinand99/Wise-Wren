//! Snapshots of a target's data paths (design §7, spike Q4): short-lived
//! local rollback points, not backups.
//!
//! Method, in the spike's fallback order, chosen per path:
//! 1. the path is the root of a btrfs subvolume and the agent has
//!    `CAP_SYS_ADMIN` (needed to delete it again): `btrfs subvolume snapshot -r`;
//! 2. a reflink copy (`cp -a --reflink=always`, btrfs and xfs with reflink):
//!    near-instant, shares every block until written;
//! 3. a plain copy (`cp -a`: owners, modes, hard links, xattrs), only after a
//!    free-space check.
//!
//! An Unraid user share path (`/mnt/user/<share>/…`, FUSE, can't reflink) is
//! resolved to its disk or pool (`getfattr -n system.LOCATIONS`) first, when
//! that path is mounted in the agent too.
//!
//! A snapshot of `<root>/<rest>` lives in `<root>/.wren-snapshots/<target>/<run>/<rest>`
//! on the same filesystem (`<root>` resolved like the path), so a later rollback
//! finds it from the path alone, and retention only ever deletes inside
//! `.wren-snapshots/<target>/`.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The folder (under each root) that holds snapshots.
pub const DIR: &str = ".wren-snapshots";
/// How long one copy may take.
pub const COPY_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// Free space that must remain after a snapshot (bytes): at least this, or
/// 2 % of the filesystem, whichever is more.
pub const MIN_FREE: u64 = 1 << 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Btrfs,
    Reflink,
    Copy,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Btrfs => "btrfs snapshot",
            Method::Reflink => "reflink copy",
            Method::Copy => "plain copy",
        }
    }
}

/// One data path as the agent snapshots it.
#[derive(Debug, Clone, PartialEq)]
pub struct DataPath {
    /// The path the container mounts (`/mnt/user/appdata/x/data`).
    pub path: PathBuf,
    /// Where it really is (`/mnt/disk1/appdata/x/data`), or `path`.
    pub real: PathBuf,
    /// The root it is under, resolved like `real`.
    pub root: PathBuf,
}

impl DataPath {
    /// Where the snapshot of this path for `target`'s run `run` goes.
    pub fn snapshot_at(&self, target: &str, run: i64) -> PathBuf {
        let rest = self.real.strip_prefix(&self.root).unwrap_or(&self.real);
        self.root
            .join(DIR)
            .join(safe_name(target))
            .join(run.to_string())
            .join(rest)
    }
}

/// A target name as one path component (no `/`, no dots-only names).
pub fn safe_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.chars().all(|c| c == '.') {
        format!("_{s}")
    } else {
        s
    }
}

/// Runs a program (argv, no shell) with a timeout; stdout on success, else
/// the exit status and the end of stderr.
pub async fn exec(program: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    crate::process::scrub(&mut cmd);
    let out = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| format!("{program} took longer than {} s", timeout.as_secs()))?
        .map_err(|e| format!("can't start {program}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!(
            "{program} failed ({}): {}",
            out.status,
            tail(err.trim(), 400)
        ))
    }
}

/// The last `n` characters of `s`.
pub fn tail(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

/// Resolves `/mnt/user/<share>/…` to the one disk or pool holding it, when
/// that path exists here; any other path stays as it is.
pub async fn resolve(path: &Path, root: &Path) -> DataPath {
    let plain = DataPath {
        path: path.to_path_buf(),
        real: path.to_path_buf(),
        root: root.to_path_buf(),
    };
    let (Ok(rest), Ok(root_rest)) = (
        path.strip_prefix("/mnt/user"),
        root.strip_prefix("/mnt/user"),
    ) else {
        return plain;
    };
    let p = path.to_string_lossy().to_string();
    let Ok(loc) = exec(
        "getfattr",
        &[
            "-n",
            "system.LOCATIONS",
            "--absolute-names",
            "--only-values",
            &p,
        ],
        Duration::from_secs(10),
    )
    .await
    else {
        return plain;
    };
    let loc = loc.trim();
    // Spread over several disks, or an odd answer: stay on the share.
    if loc.is_empty() || loc.contains(',') || loc.contains('/') || loc.contains("..") {
        return plain;
    }
    let base = Path::new("/mnt").join(loc);
    let real = base.join(rest);
    let real_root = base.join(root_rest);
    if real.exists() && real_root.is_dir() {
        DataPath {
            path: path.to_path_buf(),
            real,
            root: real_root,
        }
    } else {
        plain
    }
}

/// Whether this process has `CAP_SYS_ADMIN` (bit 21 of `CapEff`).
fn has_sys_admin() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("CapEff:"))
                .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
        })
        .is_some_and(|caps| caps & (1 << 21) != 0)
}

/// Whether `path` is the root of a btrfs subvolume (inode 256 on btrfs).
async fn is_subvolume(path: &Path) -> bool {
    let p = path.to_string_lossy().to_string();
    let fs = exec("stat", &["-f", "-c", "%T", &p], Duration::from_secs(10)).await;
    let inode = exec("stat", &["-c", "%i", &p], Duration::from_secs(10)).await;
    matches!((fs.as_deref(), inode.as_deref()), (Ok("btrfs"), Ok("256")))
}

/// Whether a reflink copy works between `from`'s filesystem and `dir`
/// (tries one on a small file in `dir`). `dir` may be writable by others
/// (the appdata tree), so the probe never writes through a symlink: both
/// names are new and unguessable, the first is created with `O_EXCL` and the
/// second must not exist before `cp`.
pub async fn reflink_works(dir: &Path) -> bool {
    use tokio::io::AsyncWriteExt as _;
    if tokio::fs::create_dir_all(dir).await.is_err() {
        return false;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    // Randomly keyed per call: what the clock doesn't add.
    let salt = RandomState::new().hash_one(nanos);
    let stamp = format!("{}-{nanos:x}-{salt:x}", std::process::id());
    let a = dir.join(format!(".wren-reflink-probe-{stamp}"));
    let b = dir.join(format!(".wren-reflink-probe-{stamp}-copy"));
    let made = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&a)
        .await;
    let Ok(mut file) = made else {
        return false;
    };
    let written = file.write_all(b"wren").await.is_ok() && file.flush().await.is_ok();
    drop(file);
    let ok = written
        && tokio::fs::symlink_metadata(&b).await.is_err()
        && exec(
            "cp",
            &[
                "--reflink=always",
                &a.to_string_lossy(),
                &b.to_string_lossy(),
            ],
            Duration::from_secs(10),
        )
        .await
        .is_ok();
    let _ = tokio::fs::remove_file(&a).await;
    let _ = tokio::fs::remove_file(&b).await;
    ok
}

/// The method for one path.
pub async fn method(p: &DataPath, snapshot_dir: &Path) -> Method {
    if is_subvolume(&p.real).await && has_sys_admin() {
        Method::Btrfs
    } else if reflink_works(snapshot_dir).await {
        Method::Reflink
    } else {
        Method::Copy
    }
}

/// Size of `path` in bytes (`du -sb`).
pub async fn size(path: &Path) -> Result<u64, String> {
    let out = exec(
        "du",
        &["-sb", &path.to_string_lossy()],
        Duration::from_secs(10 * 60),
    )
    .await?;
    out.split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("du printed {out}"))
}

/// `(available, total)` bytes of the filesystem holding `path`.
pub async fn space(path: &Path) -> Result<(u64, u64), String> {
    let out = exec(
        "df",
        &["-B1", "--output=avail,size", &path.to_string_lossy()],
        Duration::from_secs(30),
    )
    .await?;
    let nums: Vec<u64> = out
        .lines()
        .last()
        .unwrap_or("")
        .split_whitespace()
        .filter_map(|n| n.parse().ok())
        .collect();
    match nums.as_slice() {
        [avail, total] => Ok((*avail, *total)),
        _ => Err(format!("df printed {out}")),
    }
}

/// Bytes a snapshot of `size` bytes needs with `method`: a reflink copy or a
/// btrfs snapshot only metadata, a plain copy the size plus 10 %.
pub fn need(method: Method, size: u64) -> u64 {
    match method {
        Method::Copy => size + size / 10,
        Method::Reflink | Method::Btrfs => size / 100 + (64 << 20),
    }
}

/// Whether `need` bytes fit while leaving [`MIN_FREE`] or 2 % free;
/// the refusal in words otherwise.
pub fn fits(need: u64, avail: u64, total: u64) -> Result<(), String> {
    let floor = MIN_FREE.max(total / 50);
    if avail >= need.saturating_add(floor) {
        Ok(())
    } else {
        Err(format!(
            "not enough free space for the snapshot: {} MB needed plus {} MB kept free, {} MB available",
            need >> 20,
            floor >> 20,
            avail >> 20
        ))
    }
}

/// Takes the snapshot of `p` at `dest` with `method`; a failed reflink copy
/// falls back to a plain copy. The method used. `dest` must not exist: `cp -a`
/// into an existing folder nests the copy in it (or follows a symlink put
/// there), so a taken snapshot is never added to.
pub async fn take(p: &DataPath, dest: &Path, method: Method) -> Result<Method, String> {
    if tokio::fs::symlink_metadata(dest).await.is_ok() {
        return Err(format!(
            "{} already exists; a snapshot is never copied into an existing folder",
            dest.display()
        ));
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("can't create {}: {e}", parent.display()))?;
    }
    let (src, dst) = (
        p.real.to_string_lossy().to_string(),
        dest.to_string_lossy().to_string(),
    );
    match method {
        Method::Btrfs => {
            exec(
                "btrfs",
                &["subvolume", "snapshot", "-r", &src, &dst],
                COPY_TIMEOUT,
            )
            .await?;
            Ok(Method::Btrfs)
        }
        Method::Reflink => {
            match exec("cp", &["-a", "--reflink=always", &src, &dst], COPY_TIMEOUT).await {
                Ok(_) => Ok(Method::Reflink),
                Err(_) => {
                    remove(dest).await;
                    exec("cp", &["-a", &src, &dst], COPY_TIMEOUT).await?;
                    Ok(Method::Copy)
                }
            }
        }
        Method::Copy => {
            exec("cp", &["-a", &src, &dst], COPY_TIMEOUT).await?;
            Ok(Method::Copy)
        }
    }
}

/// Removes a snapshot or a set-aside copy (a subvolume with `btrfs`).
pub async fn remove(path: &Path) {
    if !path.exists() && !path.is_symlink() {
        return;
    }
    let p = path.to_string_lossy().to_string();
    if is_subvolume(path).await
        && exec(
            "btrfs",
            &["subvolume", "delete", &p],
            Duration::from_secs(120),
        )
        .await
        .is_ok()
    {
        return;
    }
    if path.is_dir() && !path.is_symlink() {
        let _ = tokio::fs::remove_dir_all(path).await;
    } else {
        let _ = tokio::fs::remove_file(path).await;
    }
}

/// Puts the snapshot at `snap` back at `p.real`: the live copy is moved
/// aside first; on failure it is moved back. After a good restore the aside
/// copy stays (as `<name>.wren-aside-<run>`) until the target's next run
/// ([`clear_asides`]), so a rollback that turns out to be wrong can still be
/// undone by hand.
pub async fn restore(p: &DataPath, snap: &Path, run: i64) -> Result<String, String> {
    if !snap.exists() {
        return Err(format!("no snapshot at {}", snap.display()));
    }
    let name = p
        .real
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let aside = p.real.with_file_name(format!("{name}.wren-aside-{run}"));
    remove(&aside).await;
    let had_live = p.real.exists() || p.real.is_symlink();
    if had_live {
        tokio::fs::rename(&p.real, &aside)
            .await
            .map_err(|e| format!("can't move {} aside: {e}", p.real.display()))?;
    }
    let (src, dst) = (
        snap.to_string_lossy().to_string(),
        p.real.to_string_lossy().to_string(),
    );
    let copied = if is_subvolume(snap).await {
        exec(
            "btrfs",
            &["subvolume", "snapshot", &src, &dst],
            COPY_TIMEOUT,
        )
        .await
    } else {
        exec("cp", &["-a", "--reflink=auto", &src, &dst], COPY_TIMEOUT).await
    };
    match copied {
        Ok(_) => Ok(if had_live {
            format!(
                "restored {} (the data it replaced is kept as {} until the next run of this target)",
                p.path.display(),
                aside.display()
            )
        } else {
            format!("restored {}", p.path.display())
        }),
        Err(err) => {
            remove(&p.real).await;
            if had_live {
                let _ = tokio::fs::rename(&aside, &p.real).await;
            }
            Err(format!("can't restore {}: {err}", p.path.display()))
        }
    }
}

/// Removes the copies earlier restores set aside next to the data at `real`
/// (`<name>.wren-aside-<run>`); the paths removed. Called when the target
/// runs again.
pub async fn clear_asides(real: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(name)) = (real.parent(), real.file_name()) else {
        return Vec::new();
    };
    let prefix = format!("{}.wren-aside-", name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut gone = Vec::new();
    for e in entries.flatten() {
        let file = e.file_name().to_string_lossy().to_string();
        // Only what a restore makes: the prefix and a run number.
        if file
            .strip_prefix(&prefix)
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        {
            remove(&e.path()).await;
            gone.push(e.path());
        }
    }
    gone
}

/// Keeps the newest `keep` run snapshots of `target` under `root`, deletes
/// the rest; the run ids deleted.
pub async fn prune(root: &Path, target: &str, keep: usize) -> Vec<i64> {
    let dir = root.join(DIR).join(safe_name(target));
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut runs: Vec<i64> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<i64>().ok())
        .collect();
    runs.sort_unstable_by(|a, b| b.cmp(a));
    let old: Vec<i64> = runs.into_iter().skip(keep).collect();
    for run in &old {
        let run_dir = dir.join(run.to_string());
        // A btrfs snapshot inside needs its own delete.
        if let Ok(walk) = std::fs::read_dir(&run_dir) {
            for e in walk.flatten() {
                remove(&e.path()).await;
            }
        }
        remove(&run_dir).await;
    }
    old
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wren-snap-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn snapshot_and_restore_round_trip() {
        use std::os::unix::fs::MetadataExt as _;
        let root = tmp("round");
        let data = root.join("app/data");
        std::fs::create_dir_all(data.join("sub")).unwrap();
        std::fs::write(data.join("a.txt"), "one").unwrap();
        std::fs::write(data.join("sub/b.txt"), "two").unwrap();
        std::fs::hard_link(data.join("a.txt"), data.join("a-link.txt")).unwrap();
        let p = resolve(&data, &root).await;
        assert_eq!(p.real, data);
        let dest = p.snapshot_at("ww test/x", 7);
        assert_eq!(dest, root.join(".wren-snapshots/ww_test_x/7/app/data"));
        let m = method(&p, dest.parent().unwrap()).await;
        let used = take(&p, &dest, m).await.unwrap();
        assert_ne!(used, Method::Btrfs);

        // The update damages the data.
        std::fs::write(data.join("a.txt"), "broken").unwrap();
        std::fs::remove_file(data.join("sub/b.txt")).unwrap();
        std::fs::write(data.join("new.txt"), "x").unwrap();

        restore(&p, &dest, 7).await.unwrap();
        assert_eq!(std::fs::read_to_string(data.join("a.txt")).unwrap(), "one");
        assert_eq!(
            std::fs::read_to_string(data.join("sub/b.txt")).unwrap(),
            "two"
        );
        assert!(!data.join("new.txt").exists());
        let (a, l) = (
            std::fs::metadata(data.join("a.txt")).unwrap(),
            std::fs::metadata(data.join("a-link.txt")).unwrap(),
        );
        assert_eq!(a.ino(), l.ino(), "hard link kept");
        // The data the restore replaced stays beside it, so a wrong rollback
        // can be undone, until the target runs again.
        let aside = root.join("app/data.wren-aside-7");
        assert_eq!(
            std::fs::read_to_string(aside.join("a.txt")).unwrap(),
            "broken"
        );
        assert!(aside.join("new.txt").exists());
        // The snapshot stays for a later rollback.
        assert!(dest.join("a.txt").exists());
        // Only what a restore set aside is cleared; the next run does it.
        std::fs::create_dir_all(root.join("app/data.wren-aside-x")).unwrap();
        std::fs::create_dir_all(root.join("app/data2.wren-aside-3")).unwrap();
        let gone = clear_asides(&data).await;
        assert_eq!(gone, std::slice::from_ref(&aside));
        assert!(!aside.exists());
        assert!(root.join("app/data.wren-aside-x").exists());
        assert!(root.join("app/data2.wren-aside-3").exists());
        assert!(data.join("a.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A symlink planted at the old fixed probe names (by something else
    /// that writes to the appdata tree) is not written through.
    #[cfg(unix)]
    #[tokio::test]
    async fn reflink_probe_does_not_write_through_symlinks() {
        let root = tmp("probe");
        let dir = root.join(DIR).join("t/1");
        std::fs::create_dir_all(&dir).unwrap();
        let victim = root.join("victim.conf");
        std::fs::write(&victim, "precious").unwrap();
        for name in [".wren-reflink-probe", ".wren-reflink-probe-copy"] {
            std::os::unix::fs::symlink(&victim, dir.join(name)).unwrap();
        }
        let _ = reflink_works(&dir).await;
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
        for name in [".wren-reflink-probe", ".wren-reflink-probe-copy"] {
            assert!(dir.join(name).is_symlink(), "{name} left alone");
        }
        // The probe cleans up after itself.
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(left.len(), 2, "{left:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn take_refuses_an_existing_destination() {
        let root = tmp("exists");
        let data = root.join("app/data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("a.txt"), "one").unwrap();
        let p = resolve(&data, &root).await;
        let dest = p.snapshot_at("t", 3);
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("old.txt"), "earlier").unwrap();
        let err = take(&p, &dest, Method::Copy).await.unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        // Nothing was nested into it, and its content is untouched.
        assert!(!dest.join("data").exists());
        assert_eq!(
            std::fs::read_to_string(dest.join("old.txt")).unwrap(),
            "earlier"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn space_check_refuses() {
        let gb = 1u64 << 30;
        assert!(fits(need(Method::Copy, 10 * gb), 20 * gb, 100 * gb).is_ok());
        let err = fits(need(Method::Copy, 10 * gb), 11 * gb, 100 * gb).unwrap_err();
        assert!(err.contains("not enough free space"), "{err}");
        // A reflink copy needs little, but the floor stays.
        assert!(fits(need(Method::Reflink, 100 * gb), 4 * gb, 100 * gb).is_ok());
        assert!(fits(need(Method::Reflink, gb), gb, 100 * gb).is_err());
    }

    #[tokio::test]
    async fn prune_keeps_the_newest() {
        let root = tmp("prune");
        for run in [3, 12, 7, 20] {
            std::fs::create_dir_all(root.join(DIR).join("t").join(run.to_string()).join("d"))
                .unwrap();
        }
        std::fs::create_dir_all(root.join(DIR).join("other/1")).unwrap();
        let mut gone = prune(&root, "t", 2).await;
        gone.sort_unstable();
        assert_eq!(gone, vec![3, 7]);
        assert!(root.join(DIR).join("t/20").exists() && root.join(DIR).join("t/12").exists());
        assert!(
            root.join(DIR).join("other/1").exists(),
            "other targets untouched"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
