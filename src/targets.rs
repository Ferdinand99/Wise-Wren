//! Targets and their images (the `targets`, `target_images` and
//! `remote_digests` tables): what an inventory describes, what the checks
//! learned, and the status derived from both.
//!
//! An image is `current` when its local repo digest equals the registry's
//! digest for its reference, `update` when they differ, and `unknown` when
//! it is pinned or an image ID, has no repo digest (built locally), hasn't
//! been checked yet or the registry failed. A target is `update` when any
//! image is (or a script stack's version source reports a newer version),
//! else `current` when any image or the version is, else `unknown`.

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::inventory::{Planned, SCRIPT};
use crate::registry::{self, ImageRef};
use crate::versions;

pub const CURRENT: &str = "current";
pub const UPDATE: &str = "update";
pub const UNKNOWN: &str = "unknown";
/// An image whose reference has no answer from a registry yet.
pub const NOT_CHECKED: &str = "not_checked";
/// The registry failed (the error follows after ": ").
pub const REGISTRY: &str = "registry";

/// Replaces a server's targets with an inventory's, keeping what Wren
/// learned about the ones that stay, and refreshes their status.
pub fn apply(
    c: &mut Connection,
    server_id: i64,
    planned: &[Planned],
    error: &str,
) -> rusqlite::Result<()> {
    let tx = c.transaction()?;
    tx.execute(
        "UPDATE servers SET inventory_at = datetime('now'), inventory_error = ?2,
             scan_requested_at = NULL WHERE id = ?1",
        params![server_id, error],
    )?;
    // A failed Docker read keeps the last known targets (a partial
    // inventory would drop every container).
    if !error.is_empty() {
        return tx.commit();
    }
    let mut keep = Vec::new();
    for p in planned {
        tx.execute(
            "INSERT INTO targets (server_id, kind, name, working_dir, files_reachable, source_url,
                 version_source, installed_version, version_error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (server_id, kind, name) DO UPDATE SET working_dir = excluded.working_dir,
                 files_reachable = excluded.files_reachable, source_url = excluded.source_url,
                 version_source = excluded.version_source,
                 installed_version = excluded.installed_version,
                 version_error = excluded.version_error",
            params![
                server_id,
                p.kind,
                p.name,
                p.working_dir,
                p.files_reachable,
                p.source_url,
                p.version_source,
                p.installed_version,
                p.version_error
            ],
        )?;
        let id: i64 = tx.query_row(
            "SELECT id FROM targets WHERE server_id = ?1 AND kind = ?2 AND name = ?3",
            params![server_id, p.kind, p.name],
            |r| r.get(0),
        )?;
        // The agent's latest_command result; GitHub and URL sources are
        // looked up by the checks.
        if !p.reported_latest.is_empty() {
            tx.execute(
                "UPDATE targets SET latest_version = ?2, latest_url = '' WHERE id = ?1",
                params![id, p.reported_latest],
            )?;
        } else if p.version_source.is_empty() {
            tx.execute(
                "UPDATE targets SET latest_version = '', latest_url = '' WHERE id = ?1",
                [id],
            )?;
        }
        tx.execute("DELETE FROM target_images WHERE target_id = ?1", [id])?;
        for i in &p.images {
            tx.execute(
                "INSERT OR REPLACE INTO target_images (target_id, container, service, image, state,
                     local_digest, local_digests)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id,
                    i.container,
                    i.service,
                    i.image,
                    i.state,
                    i.local_digest,
                    i.local_digests
                ],
            )?;
        }
        refresh(&tx, id)?;
        keep.push(id);
    }
    let keep = keep
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    // A target being updated stays, even when an inventory taken mid-run
    // missed it (its container is being recreated).
    tx.execute(
        &format!(
            "DELETE FROM targets WHERE server_id = ?1 AND id NOT IN ({keep})
               AND id NOT IN (SELECT target_id FROM runs
                              WHERE status IN ('queued', 'running') AND target_id IS NOT NULL)"
        ),
        [server_id],
    )?;
    tx.commit()
}

/// One image's status: `(status, detail, remote digest)`.
pub fn image_status(
    image: &str,
    local: &str,
    remote: Option<(&str, &str)>,
) -> (&'static str, String, String) {
    match ImageRef::parse(image) {
        Err(why) => (UNKNOWN, why.to_string(), String::new()),
        Ok(_) if local.is_empty() => (UNKNOWN, registry::LOCAL.to_string(), String::new()),
        Ok(_) => match remote {
            None => (UNKNOWN, NOT_CHECKED.to_string(), String::new()),
            Some((_, error)) if !error.is_empty() => {
                (UNKNOWN, format!("{REGISTRY}: {error}"), String::new())
            }
            Some((digest, _)) if digest == local => (CURRENT, String::new(), digest.to_string()),
            Some((digest, _)) => (UPDATE, String::new(), digest.to_string()),
        },
    }
}

/// The first 12 hex characters of a digest.
pub fn short(digest: &str) -> String {
    digest
        .trim_start_matches("sha256:")
        .chars()
        .take(12)
        .collect()
}

/// Recomputes one target's images and status from what is stored.
pub fn refresh(c: &Connection, target_id: i64) -> rusqlite::Result<()> {
    let images: Vec<(i64, String, String, String, String)> = c
        .prepare("SELECT id, container, image, local_digest, local_digests FROM target_images WHERE target_id = ?1 ORDER BY container")?
        .query_map([target_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut statuses = Vec::new();
    let mut updated: Vec<(String, String)> = Vec::new();
    let mut first_detail = String::new();
    for (id, container, image, local, all) in &images {
        let remote: Option<(String, String)> = match ImageRef::parse(image) {
            Ok(r) => c
                .query_row(
                    "SELECT digest, error FROM remote_digests WHERE image = ?1",
                    [r.canonical()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?,
            Err(_) => None,
        };
        // The image is current when any of its repo digests is the
        // registry's: that one is its local digest from now on.
        let local = match &remote {
            Some((d, _)) if !d.is_empty() && all.split_whitespace().any(|x| x == d) => d,
            _ => local,
        };
        let (status, detail, digest) = image_status(
            image,
            local,
            remote.as_ref().map(|(d, e)| (d.as_str(), e.as_str())),
        );
        c.execute(
            "UPDATE target_images SET status = ?2, detail = ?3, remote_digest = ?4, local_digest = ?5
             WHERE id = ?1",
            params![id, status, detail, digest, local],
        )?;
        if status == UPDATE {
            updated.push((container.clone(), digest));
        } else if status == UNKNOWN && first_detail.is_empty() {
            first_detail = detail;
        }
        statuses.push(status);
    }
    let (kind, installed, latest, version_error): (String, String, String, String) = c.query_row(
        "SELECT kind, installed_version, latest_version, version_error FROM targets WHERE id = ?1",
        [target_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let mut version_update = false;
    if kind == SCRIPT && !latest.is_empty() {
        if installed.is_empty() {
            statuses.push(UNKNOWN);
        } else if versions::is_newer(&latest, &installed) {
            version_update = true;
            statuses.push(UPDATE);
        } else {
            statuses.push(CURRENT);
        }
    }
    if first_detail.is_empty() && !version_error.is_empty() {
        first_detail = version_error;
    }
    let status = if statuses.contains(&UPDATE) {
        UPDATE
    } else if statuses.contains(&CURRENT) {
        CURRENT
    } else {
        UNKNOWN
    };
    // What is available, as shown and as the notification key.
    let mut shown = Vec::new();
    let mut key = Vec::new();
    if version_update {
        shown.push(latest.clone());
        key.push(format!("version={latest}"));
    }
    if images.len() == 1 && !version_update {
        if let Some((_, digest)) = updated.first() {
            shown.push(short(digest));
        }
    } else if !updated.is_empty() {
        shown.push(format!("{}/{}", updated.len(), images.len()));
    }
    for (container, digest) in &updated {
        key.push(format!("{container}={digest}"));
    }
    let (available, available_key) = if status == UPDATE {
        (shown.join(" · "), key.join(","))
    } else {
        (String::new(), String::new())
    };
    c.execute(
        "UPDATE targets SET status = ?2, status_detail = ?3, available = ?4, available_key = ?5 WHERE id = ?1",
        params![
            target_id,
            status,
            if status == UNKNOWN { first_detail } else { String::new() },
            available,
            available_key
        ],
    )?;
    Ok(())
}

/// Recomputes every target (after a check run: digests are shared).
pub fn refresh_all(c: &Connection) -> rusqlite::Result<()> {
    let ids: Vec<i64> = c
        .prepare("SELECT id FROM targets")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in ids {
        refresh(c, id)?;
    }
    Ok(())
}

/// Stores a registry's answer (or error) for an image reference.
pub fn set_remote(
    c: &Connection,
    image: &str,
    result: &Result<registry::Remote, String>,
) -> rusqlite::Result<()> {
    let (digest, media, error) = match result {
        Ok(r) => (r.digest.as_str(), r.media_type.as_str(), ""),
        Err(e) => ("", "", e.as_str()),
    };
    c.execute(
        "INSERT INTO remote_digests (image, digest, media_type, error, checked_at)
         VALUES (?1, ?2, ?3, ?4, datetime('now'))
         ON CONFLICT (image) DO UPDATE SET digest = excluded.digest, media_type = excluded.media_type,
             error = excluded.error, checked_at = excluded.checked_at",
        params![image, digest, media, error],
    )?;
    Ok(())
}

/// Forgets the registry answers no target's image refers to (the table is
/// keyed by [`ImageRef::canonical`], so strings an agent once reported must
/// not pile up). Returns how many rows went.
pub fn prune_remote(c: &Connection) -> rusqlite::Result<usize> {
    let used: std::collections::HashSet<String> = c
        .prepare("SELECT DISTINCT image FROM target_images")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?
        .iter()
        .filter_map(|image| ImageRef::parse(image).ok())
        .map(|r| r.canonical())
        .collect();
    let stored: Vec<String> = c
        .prepare("SELECT image FROM remote_digests")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut delete = c.prepare("DELETE FROM remote_digests WHERE image = ?1")?;
    let mut removed = 0;
    for image in stored.iter().filter(|i| !used.contains(*i)) {
        removed += delete.execute([image])?;
    }
    Ok(removed)
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// A target as pages, the API and notifications show it.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub id: i64,
    pub server_id: i64,
    pub server_name: String,
    pub kind: String,
    pub name: String,
    pub working_dir: String,
    pub files_reachable: bool,
    pub source_url: String,
    pub version_source: String,
    pub installed_version: String,
    pub latest_version: String,
    pub latest_url: String,
    pub version_error: String,
    pub status: String,
    pub status_detail: String,
    pub available: String,
    pub available_key: String,
    pub check_interval_hours: Option<i64>,
    pub server_interval_hours: i64,
    pub check_requested: bool,
    pub last_checked_at: Option<String>,
    pub notes_url: String,
    pub notes_title: String,
    pub notified_key: String,
    /// The first image (the target's image for single containers).
    pub image: String,
    pub image_count: i64,
    /// Why the server's agent would refuse to update it ([`crate::policy`]).
    pub blocked: Option<crate::policy::Blocked>,
}

const COLUMNS: &str = "t.id, t.server_id, s.name, t.kind, t.name, t.working_dir, t.files_reachable,
    t.source_url, t.version_source, t.installed_version, t.latest_version, t.latest_url, t.version_error,
    t.status, t.status_detail, t.available, t.available_key, t.check_interval_hours,
    s.check_interval_hours, t.check_requested, t.last_checked_at, t.notes_url, t.notes_title,
    t.notified_key,
    COALESCE((SELECT image FROM target_images i WHERE i.target_id = t.id ORDER BY container LIMIT 1), ''),
    (SELECT COUNT(*) FROM target_images i WHERE i.target_id = t.id), s.policy";

fn row(r: &Row<'_>) -> rusqlite::Result<Target> {
    let policy = crate::policy::AgentPolicy::parse(r.get::<_, Option<String>>(26)?.as_deref());
    let (kind, name, working_dir, files_reachable): (String, String, String, bool) =
        (r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?);
    Ok(Target {
        blocked: crate::policy::blocked(
            policy.as_ref(),
            &kind,
            &name,
            &working_dir,
            files_reachable,
        ),
        id: r.get(0)?,
        server_id: r.get(1)?,
        server_name: r.get(2)?,
        kind: r.get(3)?,
        name: r.get(4)?,
        working_dir: r.get(5)?,
        files_reachable: r.get(6)?,
        source_url: r.get(7)?,
        version_source: r.get(8)?,
        installed_version: r.get(9)?,
        latest_version: r.get(10)?,
        latest_url: r.get(11)?,
        version_error: r.get(12)?,
        status: r.get(13)?,
        status_detail: r.get(14)?,
        available: r.get(15)?,
        available_key: r.get(16)?,
        check_interval_hours: r.get(17)?,
        server_interval_hours: r.get(18)?,
        check_requested: r.get(19)?,
        last_checked_at: r.get(20)?,
        notes_url: r.get(21)?,
        notes_title: r.get(22)?,
        notified_key: r.get(23)?,
        image: r.get(24)?,
        image_count: r.get(25)?,
    })
}

/// Which targets to list.
#[derive(Debug, Clone, Copy, Default)]
pub struct Filter {
    pub server_id: Option<i64>,
    pub updates_only: bool,
}

pub fn list(c: &Connection, f: Filter) -> rusqlite::Result<Vec<Target>> {
    c.prepare(&format!(
        "SELECT {COLUMNS} FROM targets t JOIN servers s ON s.id = t.server_id
         WHERE (?1 IS NULL OR t.server_id = ?1) AND (?2 = 0 OR t.status = 'update')
         ORDER BY s.name COLLATE NOCASE, t.status != 'update', t.name COLLATE NOCASE, t.kind"
    ))?
    .query_map(params![f.server_id, f.updates_only], row)?
    .collect()
}

pub fn get(c: &Connection, id: i64) -> rusqlite::Result<Option<Target>> {
    c.query_row(
        &format!(
            "SELECT {COLUMNS} FROM targets t JOIN servers s ON s.id = t.server_id WHERE t.id = ?1"
        ),
        [id],
        row,
    )
    .optional()
}

/// An image of a target.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub container: String,
    pub service: String,
    pub image: String,
    pub state: String,
    pub local_digest: String,
    pub remote_digest: String,
    pub status: String,
    pub detail: String,
}

pub fn images(c: &Connection, target_id: i64) -> rusqlite::Result<Vec<Image>> {
    c.prepare(
        "SELECT container, service, image, state, local_digest, remote_digest, status, detail
         FROM target_images WHERE target_id = ?1 ORDER BY container",
    )?
    .query_map([target_id], |r| {
        Ok(Image {
            container: r.get(0)?,
            service: r.get(1)?,
            image: r.get(2)?,
            state: r.get(3)?,
            local_digest: r.get(4)?,
            remote_digest: r.get(5)?,
            status: r.get(6)?,
            detail: r.get(7)?,
        })
    })?
    .collect()
}

/// How many targets have an update.
pub fn update_count(c: &Connection) -> rusqlite::Result<i64> {
    c.query_row(
        "SELECT COUNT(*) FROM targets WHERE status = 'update'",
        [],
        |r| r.get(0),
    )
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

/// "Check now" for a server (all its targets) or one target: the checks
/// run at once, after the agent's fresh scan when it is online.
pub fn request_check(
    c: &Connection,
    server_id: i64,
    target_id: Option<i64>,
) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE servers SET scan_requested_at = datetime('now') WHERE id = ?1",
        [server_id],
    )?;
    c.execute(
        "UPDATE targets SET check_requested = 1 WHERE server_id = ?1 AND (?2 IS NULL OR id = ?2)",
        params![server_id, target_id],
    )?;
    Ok(())
}

/// How long a requested scan is waited for.
pub const SCAN_WAIT_SECONDS: i64 = 60;

/// The targets due for a check: asked for, never checked, or checked longer
/// ago than their interval (the target's, else the server's). A server whose
/// online agent was just asked for a scan is skipped until the scan arrives
/// (at most [`SCAN_WAIT_SECONDS`]).
pub fn due(c: &Connection) -> rusqlite::Result<Vec<i64>> {
    c.prepare(&format!(
        "SELECT t.id FROM targets t JOIN servers s ON s.id = t.server_id
         WHERE (t.check_requested = 1 OR t.last_checked_at IS NULL
                OR t.last_checked_at <= datetime('now',
                       '-' || COALESCE(t.check_interval_hours, s.check_interval_hours) || ' hours'))
           AND NOT (s.scan_requested_at IS NOT NULL
                    AND s.scan_requested_at > datetime('now', '-{SCAN_WAIT_SECONDS} seconds')
                    AND s.last_seen_at >= datetime('now', '-{} seconds'))
         ORDER BY t.id",
        crate::db::ONLINE_WINDOW_SECONDS
    ))?
    .query_map([], |r| r.get(0))?
    .collect()
}

/// The image references of these targets (deduplicated).
pub fn image_refs(c: &Connection, ids: &[i64]) -> rusqlite::Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stmt =
        c.prepare("SELECT image, local_digest FROM target_images WHERE target_id = ?1")?;
    for id in ids {
        for pair in stmt.query_map([id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (image, local) = pair?;
            // Pinned, ID-only and local images make no request.
            if local.is_empty() {
                continue;
            }
            if ImageRef::parse(&image).is_ok() && !out.contains(&image) {
                out.push(image);
            }
        }
    }
    Ok(out)
}

pub fn mark_checked(c: &Connection, ids: &[i64]) -> rusqlite::Result<()> {
    let mut stmt = c.prepare(
        "UPDATE targets SET last_checked_at = datetime('now'), check_requested = 0 WHERE id = ?1",
    )?;
    for id in ids {
        stmt.execute([id])?;
    }
    Ok(())
}

pub fn set_latest(
    c: &Connection,
    id: i64,
    result: &Result<versions::Latest, String>,
) -> rusqlite::Result<()> {
    match result {
        Ok(l) => c.execute(
            "UPDATE targets SET latest_version = ?2, latest_url = ?3, version_error = '',
                 notes_url = ?3, notes_title = ?4 WHERE id = ?1",
            params![id, l.version, l.url, l.title],
        )?,
        Err(e) => c.execute(
            "UPDATE targets SET version_error = ?2 WHERE id = ?1",
            params![id, e],
        )?,
    };
    Ok(())
}

pub fn set_notes(c: &Connection, id: i64, url: &str, title: &str) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE targets SET notes_url = ?2, notes_title = ?3 WHERE id = ?1",
        params![id, url, title],
    )?;
    Ok(())
}

pub fn set_interval(c: &Connection, id: i64, hours: Option<i64>) -> rusqlite::Result<bool> {
    Ok(c.execute(
        "UPDATE targets SET check_interval_hours = ?2 WHERE id = ?1",
        params![id, hours],
    )? > 0)
}

pub fn set_server_interval(c: &Connection, server_id: i64, hours: i64) -> rusqlite::Result<bool> {
    Ok(c.execute(
        "UPDATE servers SET check_interval_hours = ?2 WHERE id = ?1",
        params![server_id, hours],
    )? > 0)
}

/// Targets with an update not notified yet; marks them notified (before
/// anything is sent, so a version is never announced twice).
pub fn take_new_updates(c: &mut Connection) -> rusqlite::Result<Vec<Target>> {
    let tx = c.transaction()?;
    let rows: Vec<Target> = tx
        .prepare(&format!(
            "SELECT {COLUMNS} FROM targets t JOIN servers s ON s.id = t.server_id
             WHERE t.status = 'update' AND t.available_key != '' AND t.available_key != t.notified_key
             ORDER BY s.name COLLATE NOCASE, t.name COLLATE NOCASE"
        ))?
        .query_map([], row)?
        .collect::<rusqlite::Result<_>>()?;
    for t in &rows {
        tx.execute(
            "UPDATE targets SET notified_key = available_key WHERE id = ?1",
            [t.id],
        )?;
    }
    tx.commit()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreferenced_registry_answers_are_pruned() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE target_images (image TEXT NOT NULL);
             CREATE TABLE remote_digests (image TEXT NOT NULL PRIMARY KEY);
             INSERT INTO target_images VALUES ('postgres:16'), ('ghcr.io/a/b:1'), ('sha256:abc');
             INSERT INTO remote_digests VALUES ('docker.io/library/postgres:16'),
                 ('ghcr.io/a/b:1'), ('evil.example/x:1'), ('docker.io/library/postgres:15');",
        )
        .unwrap();
        assert_eq!(prune_remote(&c).unwrap(), 2);
        let left: Vec<String> = c
            .prepare("SELECT image FROM remote_digests ORDER BY image")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(left, ["docker.io/library/postgres:16", "ghcr.io/a/b:1"]);
        assert_eq!(prune_remote(&c).unwrap(), 0);
    }

    #[test]
    fn image_statuses() {
        assert_eq!(
            image_status("redis@sha256:1", "sha256:1", None).1,
            registry::PINNED
        );
        assert_eq!(image_status("sha256:abc", "", None).1, registry::IMAGE_ID);
        assert_eq!(image_status("local:1", "", None).1, registry::LOCAL);
        assert_eq!(image_status("redis", "sha256:1", None).1, NOT_CHECKED);
        assert_eq!(
            image_status("redis", "sha256:1", Some(("", "boom"))).1,
            "registry: boom"
        );
        assert_eq!(
            image_status("redis", "sha256:1", Some(("sha256:1", ""))).0,
            CURRENT
        );
        assert_eq!(
            image_status("redis", "sha256:1", Some(("sha256:2", ""))).0,
            UPDATE
        );
    }
}
