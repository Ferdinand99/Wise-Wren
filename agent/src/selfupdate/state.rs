//! The run's state file, `selfupdate.json` in the agent's config folder, and
//! the confirm marker beside it.
//!
//! The file crosses versions: the helper of one version reads what the old
//! agent wrote, and the next version reads what the helper left. So it only
//! grows: a field is added with a default, never renamed or removed, and a
//! reader ignores fields it does not know. [`FORMAT`] is 1; the fixture
//! `tests/fixtures/selfupdate-v1.json` freezes it.
//!
//! One writer at a time. While the helper lives it alone writes
//! `selfupdate.json`. The new agent never does; it confirms by writing the
//! marker file (see [`write_confirm`]), and the helper then moves the phase
//! to `confirmed`. After a terminal phase the agent that reports may save the
//! file once more (`reported`).

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{CONFIRM_FILE, old_name};
use crate::managed::write_private;

/// The state file's format. Raise it only for a change an older reader
/// cannot survive; adding a field never needs it.
pub const FORMAT: u32 = 1;

/// How far the update has come. The names are part of the file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Prepared by the old agent; the helper is not started yet.
    #[default]
    Prepared,
    /// The helper runs.
    HelperStarted,
    /// The old agent's container is stopped.
    OldStopped,
    /// The old container is renamed out of the way.
    OldRenamed,
    /// The new container exists.
    NewCreated,
    /// The new container is started; the confirm window runs.
    NewStarted,
    /// The new agent has confirmed.
    Confirmed,
    /// Finished: the new agent runs.
    Done,
    /// The helper puts the old agent back.
    RollingBack,
    /// Finished: the old agent runs again.
    RolledBack,
    /// Finished: the helper could not put the old agent back.
    RollbackFailed,
}

impl Phase {
    /// The run has ended; nothing moves it on.
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Phase::Done | Phase::RolledBack | Phase::RollbackFailed
        )
    }
}

/// The whole state of one run. Every field has a default, and unknown fields
/// are ignored, so an older or newer writer never makes the file unreadable.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StateFile {
    pub format: u32,
    pub run_id: i64,
    pub server_id: i64,
    pub container: String,
    pub reference: String,
    pub from_version: String,
    pub to_version: String,
    pub to_digest: String,
    pub old_container_id: String,
    pub old_image_id: String,
    pub new_container_id: String,
    pub helper_id: String,
    pub unraid: bool,
    pub phase: Phase,
    pub updated_at: u64,
    pub confirm_deadline: u64,
    pub hard_deadline: u64,
    pub reboots_seen: u32,
    pub result: String,
    pub reason: String,
    pub break_glass: String,
    pub from_digest: String,
    pub confirm_minutes: u64,
    pub stable_seconds: u64,
    pub host_boot: String,
    pub config_dir: String,
    pub reported: bool,
}

/// Reads the state file: `Ok(None)` when there is none; an error when it is
/// not JSON or has no format (`format` 0).
pub fn load(path: &Path) -> Result<Option<StateFile>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("can't read {}: {e}", path.display())),
    };
    let state: StateFile =
        serde_json::from_str(&text).map_err(|e| format!("{} is not valid: {e}", path.display()))?;
    if state.format == 0 {
        return Err(format!("{} has no format", path.display()));
    }
    Ok(Some(state))
}

/// Writes the state file as a whole (temporary file, sync, rename; mode 0600).
pub fn save(path: &Path, s: &StateFile) -> Result<(), String> {
    let text = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    write_private(path, text.as_bytes())
}

/// The command that puts the old agent back by hand, for when the helper
/// cannot.
pub fn break_glass(container: &str, run: i64) -> String {
    let old = old_name(container, run);
    format!(
        "docker rm -f {container} && docker rename {old} {container} && docker start {container}"
    )
}

/// The new agent confirms: writes the marker file [`CONFIRM_FILE`]
/// (`selfupdate.confirm`) in the config folder `dir`. The content is the run
/// id in decimal digits and nothing else. The helper reads it and sets the
/// phase to `confirmed`.
///
/// The marker crosses versions like the state file does (a helper of one
/// version reads what the next version's agent writes), so the file name and
/// the content are frozen.
pub fn write_confirm(dir: &Path, run: i64) -> Result<(), String> {
    write_private(&dir.join(CONFIRM_FILE), run.to_string().as_bytes())
}

/// The marker for run `run` is there. A marker for another run does not count.
pub fn confirmed(dir: &Path, run: i64) -> bool {
    std::fs::read_to_string(dir.join(CONFIRM_FILE))
        .is_ok_and(|text| text.trim().parse::<i64>() == Ok(run))
}

/// Seconds since the epoch.
pub fn now() -> u64 {
    crate::ledger::now()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The fixture is the v1 file of 0.10.0. It is frozen once 0.10.0 ships:
    // never edit it. A later version adds its own fixture.
    const V1: &str = include_str!("../../tests/fixtures/selfupdate-v1.json");

    #[test]
    fn the_v1_fixture_reads_and_writes_back() {
        let s: StateFile = serde_json::from_str(V1).unwrap();
        assert_eq!(
            (s.format, s.run_id, s.phase, s.unraid),
            (1, 57, Phase::NewStarted, true)
        );
        let back = serde_json::to_value(&s).unwrap();
        let orig: serde_json::Value = serde_json::from_str(V1).unwrap();
        for (k, v) in orig.as_object().unwrap() {
            assert_eq!(&back[k], v, "{k}");
        }
    }

    #[test]
    fn a_newer_writers_extra_fields_are_ignored() {
        let mut v: serde_json::Value = serde_json::from_str(V1).unwrap();
        v["from_the_future"] = serde_json::json!({"x": 1});
        let s: StateFile = serde_json::from_value(v).unwrap();
        assert_eq!(s.run_id, 57);
    }

    #[test]
    fn phases_have_the_specs_names() {
        use Phase::*;
        for (p, n) in [
            (Prepared, "prepared"),
            (HelperStarted, "helper_started"),
            (OldStopped, "old_stopped"),
            (OldRenamed, "old_renamed"),
            (NewCreated, "new_created"),
            (NewStarted, "new_started"),
            (Confirmed, "confirmed"),
            (Done, "done"),
            (RollingBack, "rolling_back"),
            (RolledBack, "rolled_back"),
            (RollbackFailed, "rollback_failed"),
        ] {
            assert_eq!(serde_json::to_value(p).unwrap(), n);
            assert_eq!(
                p.terminal(),
                matches!(p, Done | RolledBack | RollbackFailed)
            );
        }
    }

    #[test]
    fn the_file_is_private_and_replaced_atomically() {
        let dir = std::env::temp_dir().join(format!("wren-su-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(crate::selfupdate::STATE_FILE);
        assert_eq!(load(&path), Ok(None));
        let s: StateFile = serde_json::from_str(V1).unwrap();
        save(&path, &s).unwrap();
        assert_eq!(load(&path).unwrap(), Some(s));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(
            std::fs::read_dir(&dir).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
        );
        std::fs::write(&path, "nonsense").unwrap();
        assert!(load(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_without_a_format_is_refused() {
        let dir = std::env::temp_dir().join(format!("wren-su-noformat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(crate::selfupdate::STATE_FILE);
        save(&path, &StateFile::default()).unwrap();
        assert!(load(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn break_glass_and_the_confirm_marker() {
        assert_eq!(
            break_glass("Wise-Wren-Agent", 57),
            "docker rm -f Wise-Wren-Agent && docker rename Wise-Wren-Agent-wren-old-57 Wise-Wren-Agent && docker start Wise-Wren-Agent"
        );
        let dir = std::env::temp_dir().join(format!("wren-su-confirm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_confirm(&dir, 57).unwrap();
        assert!(confirmed(&dir, 57));
        assert!(!confirmed(&dir, 58));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The confirm marker is frozen with the v1 file: a helper of one version
    // reads what the agent of the next version writes. Name `selfupdate.confirm`,
    // content the run id in decimal digits, nothing else.
    #[test]
    fn the_confirm_marker_contract_is_frozen() {
        assert_eq!(crate::selfupdate::CONFIRM_FILE, "selfupdate.confirm");
        let dir = std::env::temp_dir().join(format!("wren-su-marker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_confirm(&dir, 57).unwrap();
        assert_eq!(
            std::fs::read(dir.join("selfupdate.confirm")).unwrap(),
            b"57"
        );
        // A marker a later version writes with a trailing newline still counts.
        std::fs::write(dir.join("selfupdate.confirm"), "57\n").unwrap();
        assert!(confirmed(&dir, 57));
        // Other content never confirms.
        std::fs::write(dir.join("selfupdate.confirm"), "garbage").unwrap();
        assert!(!confirmed(&dir, 57));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
