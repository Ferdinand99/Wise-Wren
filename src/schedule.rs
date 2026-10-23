//! Auto windows, the auto picker and what Wren never updates (design §5, §6).
//!
//! **Automatic updates are off unless an admin turns them on**, and even
//! then they only start inside the server's auto window (local `HH:MM` in
//! the site's time zone), for targets whose effective mode is `auto`: a
//! target set to `notify` or `approve` on an `auto` server is excluded. Each
//! auto run is for the exact version key seen when it was picked, and a
//! version whose auto run failed, rolled back or was refused is not tried
//! again on its own. Wren, its agents and Vigilant Vole are never updated by
//! Wren in v1, whatever the mode ([`own_image`]).

use jiff::Timestamp;
use jiff::tz::TimeZone;
use rusqlite::{Connection, OptionalExtension, params};
use vole_kit::db::settings;

/// The longest a run can be scheduled ahead.
pub const MAX_DAYS_AHEAD: i64 = 90;

/// Whether `image` is Wise Wren, its agent or Vigilant Vole (any registry,
/// tag or digest; also local builds of those names).
pub fn own_image(image: &str) -> bool {
    let lower = image.trim().to_ascii_lowercase();
    let no_digest = lower.split('@').next().unwrap_or_default();
    let slash = no_digest.rfind('/');
    let repo = match no_digest.rfind(':') {
        Some(i) if slash.is_none_or(|s| i > s) => &no_digest[..i],
        _ => no_digest,
    };
    let last = repo.rsplit('/').next().unwrap_or_default();
    last.starts_with("wise-wren") || last.starts_with("vigilant-vole")
}

/// Whether any of the target's images is one of Wren's own or VV's.
pub fn own_target(c: &Connection, target_id: i64) -> rusqlite::Result<bool> {
    let images: Vec<String> = c
        .prepare("SELECT image FROM target_images WHERE target_id = ?1")?
        .query_map([target_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(images.iter().any(|i| own_image(i)))
}

/// `HH:MM` (24 h) as minutes after midnight.
pub fn parse_hhmm(raw: &str) -> Option<u16> {
    let (h, m) = raw.trim().split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Normalises `HH:MM` (`3:00` → `03:00`).
pub fn normal_hhmm(raw: &str) -> Option<String> {
    parse_hhmm(raw).map(|m| format!("{:02}:{:02}", m / 60, m % 60))
}

/// Whether `now` (minutes after local midnight) is inside `[start, end)`; a
/// window whose end is before its start runs past midnight. An empty
/// window (start = end) is never open.
pub fn in_window(now: u16, start: u16, end: u16) -> bool {
    match start.cmp(&end) {
        std::cmp::Ordering::Less => start <= now && now < end,
        std::cmp::Ordering::Greater => now >= start || now < end,
        std::cmp::Ordering::Equal => false,
    }
}

/// The site's time zone (General settings), UTC when unset or unknown.
pub fn site_tz(c: &Connection) -> rusqlite::Result<TimeZone> {
    let name = settings::get(c, settings::TIMEZONE)?.unwrap_or_default();
    Ok(tz_named(&name))
}

pub fn tz_named(name: &str) -> TimeZone {
    if name.is_empty() {
        return TimeZone::UTC;
    }
    TimeZone::get(name).unwrap_or(TimeZone::UTC)
}

/// The site time zone's name ("UTC" when unset).
pub fn site_tz_name(c: &Connection) -> rusqlite::Result<String> {
    Ok(site_tz(c)?.iana_name().unwrap_or("UTC").to_string())
}

/// Minutes after local midnight at `now` in `tz`.
pub fn local_minutes(now: Timestamp, tz: &TimeZone) -> u16 {
    let t = now.to_zoned(tz.clone()).time();
    u16::try_from(t.hour()).unwrap_or(0) * 60 + u16::try_from(t.minute()).unwrap_or(0)
}

/// A server's auto window, when it has one.
pub fn window(c: &Connection, server_id: i64) -> rusqlite::Result<Option<(String, String)>> {
    Ok(c.query_row(
        "SELECT auto_start, auto_end FROM servers WHERE id = ?1",
        [server_id],
        |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
            ))
        },
    )
    .optional()?
    .and_then(|(s, e)| Some((s?, e?))))
}

/// Whether the server's auto window is open at `now`.
pub fn window_open(c: &Connection, server_id: i64, now: Timestamp) -> rusqlite::Result<bool> {
    let Some((s, e)) = window(c, server_id)? else {
        return Ok(false);
    };
    let (Some(s), Some(e)) = (parse_hhmm(&s), parse_hhmm(&e)) else {
        return Ok(false);
    };
    Ok(in_window(local_minutes(now, &site_tz(c)?), s, e))
}

/// A UTC time as SQLite stores it.
pub fn db_time(t: Timestamp) -> String {
    t.strftime("%Y-%m-%d %H:%M:%S").to_string()
}

/// A stored UTC time (`YYYY-MM-DD HH:MM:SS`, or RFC 3339) as a timestamp.
pub fn parse_db_time(raw: &str) -> Option<Timestamp> {
    let raw = raw.trim();
    if let Ok(t) = raw.parse::<Timestamp>() {
        return Some(t);
    }
    let civil: jiff::civil::DateTime = raw.replacen(' ', "T", 1).parse().ok()?;
    civil.to_zoned(TimeZone::UTC).ok().map(|z| z.timestamp())
}

/// When to run, as a person or a program gave it: RFC 3339 with an offset,
/// or a local date and time (`2026-10-24T03:00`, `2026-10-24 03:00:00`) in
/// `tz`. The UTC time as stored, or `None` when it isn't a time.
pub fn parse_at(raw: &str, tz: &TimeZone) -> Option<Timestamp> {
    let raw = raw.trim();
    if let Ok(t) = raw.parse::<Timestamp>() {
        return Some(t);
    }
    let mut s = raw.replacen(' ', "T", 1);
    if s.len() == 16 {
        s.push_str(":00");
    }
    let civil: jiff::civil::DateTime = s.parse().ok()?;
    civil.to_zoned(tz.clone()).ok().map(|z| z.timestamp())
}

/// A stored UTC time in the site's time zone, `YYYY-MM-DD HH:MM`.
pub fn local(raw: &str, tz: &TimeZone) -> String {
    parse_db_time(raw).map_or_else(
        || raw.to_string(),
        |t| {
            t.to_zoned(tz.clone())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        },
    )
}

/// Picks the targets the auto windows open now allow, and queues one auto
/// run for each, for the version key seen now. The new runs' ids.
pub fn pick_auto(c: &Connection, now: Timestamp) -> rusqlite::Result<Vec<i64>> {
    let candidates: Vec<(i64, i64)> = c
        .prepare(
            "SELECT t.id, t.server_id FROM targets t JOIN servers s ON s.id = t.server_id
             WHERE COALESCE(NULLIF(t.mode, ''), s.mode) = 'auto'
               AND t.status = 'update' AND t.available_key != ''
               AND s.auto_start IS NOT NULL AND s.auto_end IS NOT NULL
               AND NOT (t.skipped_key != '' AND t.skipped_key = t.available_key)
               AND (t.snoozed_until IS NULL OR t.snoozed_until <= datetime('now'))
               AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.target_id = t.id
                               AND r.status IN ('queued', 'running'))
               AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.target_id = t.id
                               AND r.to_key = t.available_key
                               AND r.status IN ('failed', 'rolled_back', 'rollback_failed', 'refused'))
             ORDER BY t.server_id, t.id",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for (target_id, server_id) in candidates {
        if !window_open(c, server_id, now)? {
            continue;
        }
        if let Ok(id) = crate::runs::queue_auto(c, target_id)? {
            out.push(id);
        }
    }
    Ok(out)
}

/// The targets of a server that update on their own (effective mode
/// `auto`, not one of Wren's or VV's own): `(id, name)`.
pub fn auto_targets(c: &Connection, server_id: i64) -> rusqlite::Result<Vec<(i64, String)>> {
    let rows: Vec<(i64, String)> = c
        .prepare(
            "SELECT t.id, t.name FROM targets t JOIN servers s ON s.id = t.server_id
             WHERE t.server_id = ?1 AND COALESCE(NULLIF(t.mode, ''), s.mode) = 'auto'
             ORDER BY t.name",
        )?
        .query_map(params![server_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for (id, name) in rows {
        if !own_target(c, id)? {
            out.push((id, name));
        }
    }
    Ok(out)
}

/// A server's automatic updates, for the Schedules page and the API.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerAuto {
    pub id: i64,
    pub name: String,
    pub mode: String,
    pub window: Option<(String, String)>,
    pub open_now: bool,
    /// The targets that update on their own inside the window.
    pub targets: Vec<(i64, String)>,
}

/// `(id, name, mode, auto_start, auto_end)`.
type ServerRow = (i64, String, String, Option<String>, Option<String>);

pub fn servers_auto(c: &Connection, now: Timestamp) -> rusqlite::Result<Vec<ServerAuto>> {
    let rows: Vec<ServerRow> = c
        .prepare("SELECT id, name, mode, auto_start, auto_end FROM servers ORDER BY name")?
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for (id, name, mode, start, end) in rows {
        let window = start.zip(end);
        let targets = if window.is_some() {
            auto_targets(c, id)?
        } else {
            Vec::new()
        };
        out.push(ServerAuto {
            open_now: window_open(c, id, now)?,
            id,
            name,
            mode,
            window,
            targets,
        });
    }
    Ok(out)
}

/// Looks for auto runs every minute.
pub fn spawn_picker(wren: crate::Wren) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            match wren
                .core
                .with_conn(|c| pick_auto(c, Timestamp::now()))
                .await
            {
                Ok(ids) if !ids.is_empty() => {
                    tracing::info!(runs = ?ids, "auto runs queued");
                    wren.jobs.notify_waiters();
                    let _ = wren.changes.send(());
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(%err, "can't look for auto updates"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_images() {
        for i in [
            "ghcr.io/ferdinand99/wise-wren:latest",
            "ghcr.io/ferdinand99/wise-wren-agent:0.1.0",
            "GHCR.IO/Ferdinand99/Vigilant-Vole@sha256:abc",
            "ghcr.io/ferdinand99/vigilant-vole",
            "wise-wren:dev",
            "wise-wren-agent:dev",
        ] {
            assert!(own_image(i), "{i}");
        }
        for i in [
            "localhost:5056/ww-test/app:container",
            "postgres:16",
            "ghcr.io/ferdinand99/wren-tools:1",
            "vv-test-wren-vv:dev",
        ] {
            assert!(!own_image(i), "{i}");
        }
    }

    #[test]
    fn in_window_wraps_midnight() {
        let (s, e) = (parse_hhmm("03:00").unwrap(), parse_hhmm("05:00").unwrap());
        assert!(!in_window(parse_hhmm("02:59").unwrap(), s, e));
        assert!(in_window(s, s, e));
        assert!(in_window(parse_hhmm("04:59").unwrap(), s, e));
        assert!(!in_window(e, s, e));
        let (s, e) = (parse_hhmm("22:00").unwrap(), parse_hhmm("2:00").unwrap());
        assert!(in_window(parse_hhmm("23:30").unwrap(), s, e));
        assert!(in_window(parse_hhmm("01:59").unwrap(), s, e));
        assert!(!in_window(parse_hhmm("12:00").unwrap(), s, e));
        assert!(!in_window(s, s, s));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("3:5"), None);
        assert_eq!(normal_hhmm("3:05").as_deref(), Some("03:05"));
    }

    #[test]
    fn times_parse_in_the_given_zone() {
        let oslo = tz_named("Europe/Oslo");
        let t = parse_at("2026-10-24T03:00", &oslo).unwrap();
        assert_eq!(db_time(t), "2026-10-24 01:00:00");
        let t = parse_at("2026-10-24T03:00:00Z", &oslo).unwrap();
        assert_eq!(db_time(t), "2026-10-24 03:00:00");
        let t = parse_at("2026-10-24 03:00:00", &TimeZone::UTC).unwrap();
        assert_eq!(db_time(t), "2026-10-24 03:00:00");
        assert_eq!(local("2026-10-24 01:00:00", &oslo), "2026-10-24 03:00");
        assert!(parse_at("tomorrow", &oslo).is_none());
        assert_eq!(
            local_minutes(parse_db_time("2026-10-24 01:30:00").unwrap(), &oslo),
            3 * 60 + 30
        );
    }
}
