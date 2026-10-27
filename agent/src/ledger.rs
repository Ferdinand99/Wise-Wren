//! The run ledger: what this agent itself ran, kept in its config folder
//! (`runs.json` beside `agent.toml`, root-owned, mode 0600).
//!
//! Wren says which update or rollback to run, but it is the component the
//! agent doesn't trust: a compromised or confused server could replay old
//! run ids, ask for a rollback of an update this agent never ran, or start a
//! script stack's installer again and again. The ledger lets the agent
//! check those claims against its own record:
//!
//! - an update's `run_id` must be greater than every run id recorded for the
//!   target (a restored Wren database starts counting again: refused, not
//!   obeyed);
//! - a rollback is accepted only for the newest *succeeded* update this
//!   agent recorded for the target, and only once;
//! - a script stack's update may start at most once per `min_interval`.
//!
//! A missing or unreadable file starts a fresh ledger with a warning; a
//! corrupt one is kept aside as `runs.json.bad` first. The ledger holds the
//! last [`KEEP_RUNS`] runs per target. It is written whole to a temporary
//! file and renamed over the old one, so a crash leaves the old or the new,
//! never half of one.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::jobs::{Job, JobKind, Note};

/// The most runs kept per target (the newest succeeded update stays too).
pub const KEEP_RUNS: usize = 20;
/// The most targets kept; the ones idle the longest go first.
pub const MAX_TARGETS: usize = 200;
/// What a run that never reported an end stays as.
pub const RUNNING: &str = "running";

/// One run the agent started.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Entry {
    pub run_id: i64,
    /// `update` or `rollback`.
    pub kind: String,
    /// [`RUNNING`] until the run ends, then its outcome (`succeeded`, ...).
    pub outcome: String,
    /// When the run started (seconds since the epoch).
    pub at: u64,
    /// A rollback of this (succeeded) update was started.
    #[serde(default)]
    pub consumed: bool,
    /// For a rollback: the update it undoes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_of: Option<i64>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct File {
    #[serde(default)]
    targets: BTreeMap<String, Vec<Entry>>,
}

/// The ledger and where it lives.
#[derive(Debug)]
pub struct Ledger {
    path: PathBuf,
    file: File,
}

/// Seconds since the epoch.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Ledger {
    /// Reads the ledger at `path`. Alongside it, what is worth a warning:
    /// no file yet (nothing is known about earlier updates, so they can't be
    /// rolled back through Wren), or one that couldn't be read (a corrupt
    /// file is moved to `<path>.bad`).
    pub fn load(path: &Path) -> (Ledger, Option<String>) {
        let fresh = |path: &Path| Ledger {
            path: path.to_path_buf(),
            file: File::default(),
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (
                    fresh(path),
                    Some(format!(
                        "no run ledger at {} yet: starting a new one; updates made before it can't be rolled back from Wren",
                        path.display()
                    )),
                );
            }
            Err(e) => {
                return (
                    fresh(path),
                    Some(format!(
                        "can't read the run ledger {}: {e}; starting a new one",
                        path.display()
                    )),
                );
            }
        };
        match serde_json::from_str::<File>(&text) {
            Ok(file) => (
                Ledger {
                    path: path.to_path_buf(),
                    file,
                },
                None,
            ),
            Err(e) => {
                let bad = bad_path(path);
                let kept = match std::fs::rename(path, &bad) {
                    Ok(()) => format!("kept as {}", bad.display()),
                    Err(re) => format!("and it can't be kept aside ({re})"),
                };
                (
                    fresh(path),
                    Some(format!(
                        "the run ledger {} is corrupt ({e}), {kept}; starting a new one",
                        path.display()
                    )),
                )
            }
        }
    }

    /// What `--check` says about the ledger at `path`, without touching it.
    pub fn status(path: &Path) -> String {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<File>(&text) {
                Ok(file) => format!(
                    "{} runs of {} targets recorded",
                    file.targets.values().map(Vec::len).sum::<usize>(),
                    file.targets.len()
                ),
                Err(_) => "corrupt; the agent keeps it aside as .bad when it starts".into(),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                "none yet; the first run starts it (updates made before it can't be rolled back from Wren)"
                    .into()
            }
            Err(e) => format!("can't be read ({e})"),
        }
    }

    /// The ledger's key for a target.
    pub fn key(kind: &str, name: &str) -> String {
        format!("{kind}/{name}")
    }

    fn runs(&self, key: &str) -> &[Entry] {
        self.file.targets.get(key).map_or(&[], Vec::as_slice)
    }

    /// The runs recorded for a target, oldest first.
    pub fn entries(&self, kind: &str, name: &str) -> &[Entry] {
        self.runs(&Self::key(kind, name))
    }

    /// The newest update that succeeded: the only one a rollback may undo.
    fn newest_succeeded(&self, key: &str) -> Option<&Entry> {
        self.runs(key)
            .iter()
            .filter(|e| e.kind == "update" && e.outcome == "succeeded")
            .max_by_key(|e| e.run_id)
    }

    /// Whether this agent may run `job` by its own record. `min_interval`
    /// is the script stack's shortest time between updates (`None` for other
    /// targets). The refusal is a note Wren shows.
    pub fn check(&self, job: &Job, min_interval: Option<Duration>, now: u64) -> Result<(), Note> {
        let target = job.target.name.as_str();
        let key = Self::key(&job.target.kind, target);
        let runs = self.runs(&key);
        if let Some(newest) = runs.iter().map(|e| e.run_id).max()
            && job.run_id <= newest
        {
            return Err(Note::new(
                "refused.run_old",
                &[
                    ("target", target),
                    ("run", &job.run_id.to_string()),
                    ("newest", &newest.to_string()),
                ],
            ));
        }
        match job.kind {
            JobKind::Update => {
                if let Some(interval) = min_interval
                    && let Some(last) = runs
                        .iter()
                        .filter(|e| e.kind == "update")
                        .map(|e| e.at)
                        .max()
                {
                    let next = last.saturating_add(interval.as_secs());
                    if now < next {
                        // A clock set back lands here too: wait it out.
                        let minutes = (next - now).div_ceil(60);
                        return Err(Note::new(
                            "refused.too_soon",
                            &[("target", target), ("minutes", &minutes.to_string())],
                        ));
                    }
                }
            }
            JobKind::Rollback => {
                let of = job.rollback_of.unwrap_or_default();
                let Some(update) = self.newest_succeeded(&key).filter(|e| e.run_id == of) else {
                    return Err(Note::new(
                        "refused.rollback_unknown",
                        &[("target", target), ("run", &of.to_string())],
                    ));
                };
                if update.consumed {
                    return Err(Note::new(
                        "refused.rollback_used",
                        &[("target", target), ("run", &of.to_string())],
                    ));
                }
            }
        }
        Ok(())
    }

    /// Records that `job` starts now (a rollback uses up the update it
    /// undoes) and saves the ledger. An error means nothing was recorded
    /// durably: the run must not start.
    pub fn begin(&mut self, job: &Job, now: u64) -> Result<(), String> {
        let key = Self::key(&job.target.kind, &job.target.name);
        let runs = self.file.targets.entry(key).or_default();
        if job.kind == JobKind::Rollback
            && let Some(of) = job.rollback_of
            && let Some(update) = runs
                .iter_mut()
                .find(|e| e.kind == "update" && e.run_id == of)
        {
            update.consumed = true;
        }
        runs.push(Entry {
            run_id: job.run_id,
            kind: match job.kind {
                JobKind::Update => "update",
                JobKind::Rollback => "rollback",
            }
            .into(),
            outcome: RUNNING.into(),
            at: now,
            consumed: false,
            rollback_of: job.rollback_of.filter(|_| job.kind == JobKind::Rollback),
        });
        self.trim();
        self.save()
    }

    /// Records how the run ended. A rollback that changed nothing (it failed
    /// before anything stopped, or Wren cancelled it) doesn't use up the
    /// update, so it can be tried again.
    pub fn finish(&mut self, job: &Job, outcome: &str) -> Result<(), String> {
        let key = Self::key(&job.target.kind, &job.target.name);
        if let Some(runs) = self.file.targets.get_mut(&key) {
            if let Some(entry) = runs.iter_mut().find(|e| e.run_id == job.run_id) {
                entry.outcome = outcome.to_string();
            }
            if job.kind == JobKind::Rollback
                && matches!(outcome, "failed" | "cancelled" | "refused")
                && let Some(of) = job.rollback_of
                && let Some(update) = runs
                    .iter_mut()
                    .find(|e| e.kind == "update" && e.run_id == of)
            {
                update.consumed = false;
            }
        }
        self.save()
    }

    /// Drops the oldest runs beyond [`KEEP_RUNS`] per target (the newest
    /// succeeded update stays: it's what a rollback needs) and the targets
    /// idle the longest beyond [`MAX_TARGETS`].
    fn trim(&mut self) {
        for runs in self.file.targets.values_mut() {
            runs.sort_by_key(|e| e.run_id);
            if runs.len() > KEEP_RUNS {
                let keep_id = runs
                    .iter()
                    .filter(|e| e.kind == "update" && e.outcome == "succeeded")
                    .map(|e| e.run_id)
                    .max();
                let cut = runs.len() - KEEP_RUNS;
                let mut index = 0;
                runs.retain(|e| {
                    index += 1;
                    index > cut || Some(e.run_id) == keep_id
                });
            }
        }
        if self.file.targets.len() > MAX_TARGETS {
            let mut idle: Vec<(u64, String)> = self
                .file
                .targets
                .iter()
                .map(|(k, v)| (v.iter().map(|e| e.at).max().unwrap_or(0), k.clone()))
                .collect();
            idle.sort();
            let extra = self.file.targets.len() - MAX_TARGETS;
            for (_, key) in idle.into_iter().take(extra) {
                self.file.targets.remove(&key);
            }
        }
    }

    /// Writes the ledger to a temporary file beside it and renames it over
    /// the old one (mode 0600).
    pub fn save(&self) -> Result<(), String> {
        let text = serde_json::to_string_pretty(&self.file).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("json.tmp");
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut f = options.open(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, &self.path)
        };
        write().map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("can't write the run ledger {}: {e}", self.path.display())
        })
    }
}

fn bad_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bad");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::TargetRef;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wren-ledger-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn job(run_id: i64, kind: JobKind, of: Option<i64>) -> Job {
        Job {
            run_id,
            kind,
            target: TargetRef {
                kind: "container".into(),
                name: "app".into(),
            },
            from_key: String::new(),
            to_key: String::new(),
            images: Vec::new(),
            health_seconds: 180,
            keep_snapshots: 2,
            rollback_of: of,
            require_snapshot: false,
        }
    }

    fn code(r: Result<(), Note>) -> &'static str {
        r.err().map_or("", |n| n.code)
    }

    /// Runs `job` to `outcome` through the ledger the way the agent does.
    fn run(l: &mut Ledger, j: &Job, at: u64, outcome: &str) {
        l.check(j, None, at).unwrap();
        l.begin(j, at).unwrap();
        l.finish(j, outcome).unwrap();
    }

    #[test]
    fn update_run_ids_must_grow() {
        let d = dir("ids");
        let (mut l, warn) = Ledger::load(&d.join("runs.json"));
        assert!(warn.is_some_and(|w| w.contains("no run ledger")));
        run(&mut l, &job(5, JobKind::Update, None), 100, "failed");
        for old in [5, 4, 1] {
            let why = l
                .check(&job(old, JobKind::Update, None), None, 200)
                .unwrap_err();
            assert_eq!(why.code, "refused.run_old");
            assert_eq!(why.params["newest"], "5");
        }
        assert!(l.check(&job(6, JobKind::Update, None), None, 200).is_ok());
        // Another target has its own count.
        let mut other = job(1, JobKind::Update, None);
        other.target.name = "other".into();
        assert!(l.check(&other, None, 200).is_ok());
    }

    #[test]
    fn rollback_needs_the_newest_succeeded_update_and_only_once() {
        let d = dir("rollback");
        let (mut l, _) = Ledger::load(&d.join("runs.json"));
        // Nothing recorded: no rollback, whatever Wren says.
        assert_eq!(
            code(l.check(&job(2, JobKind::Rollback, Some(1)), None, 1)),
            "refused.rollback_unknown"
        );
        run(&mut l, &job(1, JobKind::Update, None), 10, "succeeded");
        run(&mut l, &job(2, JobKind::Update, None), 20, "rolled_back");
        // A failed or rolled-back update is not one to undo.
        assert_eq!(
            code(l.check(&job(3, JobKind::Rollback, Some(2)), None, 30)),
            "refused.rollback_unknown"
        );
        assert!(
            l.check(&job(3, JobKind::Rollback, Some(1)), None, 30)
                .is_ok()
        );
        run(&mut l, &job(3, JobKind::Update, None), 30, "succeeded");
        // Only the newest succeeded one.
        assert_eq!(
            code(l.check(&job(4, JobKind::Rollback, Some(1)), None, 40)),
            "refused.rollback_unknown"
        );
        let back = job(4, JobKind::Rollback, Some(3));
        run(&mut l, &back, 40, "succeeded");
        assert_eq!(
            code(l.check(&job(5, JobKind::Rollback, Some(3)), None, 50)),
            "refused.rollback_used"
        );
        // The ledger on disk says the same.
        let (again, warn) = Ledger::load(&d.join("runs.json"));
        assert!(warn.is_none());
        assert_eq!(
            code(again.check(&job(5, JobKind::Rollback, Some(3)), None, 50)),
            "refused.rollback_used"
        );
    }

    #[test]
    fn a_rollback_that_changed_nothing_can_be_tried_again() {
        let d = dir("retry");
        let (mut l, _) = Ledger::load(&d.join("runs.json"));
        run(&mut l, &job(1, JobKind::Update, None), 10, "succeeded");
        let first = job(2, JobKind::Rollback, Some(1));
        l.begin(&first, 20).unwrap();
        // Used up while it runs (a crash must not allow a second one) ...
        assert_eq!(
            code(l.check(&job(3, JobKind::Rollback, Some(1)), None, 21)),
            "refused.rollback_used"
        );
        l.finish(&first, "failed").unwrap();
        // ... and given back when the agent says it stopped before changing anything.
        assert!(
            l.check(&job(3, JobKind::Rollback, Some(1)), None, 30)
                .is_ok()
        );
        let second = job(3, JobKind::Rollback, Some(1));
        l.begin(&second, 30).unwrap();
        l.finish(&second, "rollback_failed").unwrap();
        assert_eq!(
            code(l.check(&job(4, JobKind::Rollback, Some(1)), None, 40)),
            "refused.rollback_used"
        );
    }

    #[test]
    fn script_stack_updates_keep_their_distance() {
        let d = dir("interval");
        let (mut l, _) = Ledger::load(&d.join("runs.json"));
        let hour = Some(Duration::from_secs(3600));
        let mut j = job(1, JobKind::Update, None);
        j.target.kind = "script".into();
        l.check(&j, hour, 1000).unwrap();
        l.begin(&j, 1000).unwrap();
        l.finish(&j, "failed").unwrap();
        let mut next = job(2, JobKind::Update, None);
        next.target.kind = "script".into();
        let why = l.check(&next, hour, 1000 + 3000).unwrap_err();
        assert_eq!(why.code, "refused.too_soon");
        assert_eq!(why.params["minutes"], "10");
        assert!(l.check(&next, hour, 1000 + 3600).is_ok());
        // A clock set back doesn't let a run through.
        assert!(l.check(&next, hour, 10).is_err());
        // Other kinds have no interval.
        assert!(l.check(&next, None, 1001).is_ok());
    }

    #[test]
    fn corrupt_ledgers_are_kept_aside_and_replaced() {
        let d = dir("corrupt");
        let path = d.join("runs.json");
        std::fs::write(&path, "{ not json").unwrap();
        let (l, warn) = Ledger::load(&path);
        let warn = warn.unwrap();
        assert!(
            warn.contains("corrupt") && warn.contains("runs.json.bad"),
            "{warn}"
        );
        assert_eq!(
            std::fs::read_to_string(d.join("runs.json.bad")).unwrap(),
            "{ not json"
        );
        assert!(!path.exists());
        assert!(l.entries("container", "app").is_empty());
    }

    #[test]
    fn the_file_is_private_and_replaced_atomically() {
        let d = dir("save");
        let path = d.join("runs.json");
        let (mut l, _) = Ledger::load(&path);
        l.begin(&job(1, JobKind::Update, None), 5).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // No temporary file is left behind.
        let names: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, ["runs.json"]);
        // A folder that can't be written is an error the caller refuses on.
        let (mut broken, _) = Ledger::load(&d.join("runs.json").join("sub").join("x.json"));
        assert!(broken.begin(&job(1, JobKind::Update, None), 5).is_err());
    }

    #[test]
    fn the_ledger_stays_small() {
        let d = dir("trim");
        let (mut l, _) = Ledger::load(&d.join("runs.json"));
        run(&mut l, &job(1, JobKind::Update, None), 1, "succeeded");
        for id in 2..=40 {
            run(&mut l, &job(id, JobKind::Update, None), id as u64, "failed");
        }
        let runs = l.entries("container", "app");
        assert_eq!(
            runs.len(),
            KEEP_RUNS + 1,
            "the newest 20 and the succeeded update"
        );
        assert_eq!(runs.last().unwrap().run_id, 40);
        // A rollback of that old update is still possible.
        assert!(
            l.check(&job(41, JobKind::Rollback, Some(1)), None, 100)
                .is_ok()
        );
        for n in 0..(MAX_TARGETS + 10) {
            let mut j = job(1, JobKind::Update, None);
            j.target.name = format!("t{n}");
            l.begin(&j, 1000 + n as u64).unwrap();
        }
        assert_eq!(l.file.targets.len(), MAX_TARGETS);
    }
}
