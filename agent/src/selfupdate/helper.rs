//! The self-update helper: a short-lived container (the old agent's image,
//! `wren-agent --self-update-helper`) that swaps the agent's own container
//! for one on the new image, waits for the new agent to confirm and puts the
//! old one back if it doesn't. It never talks to Wren; the state file is its
//! only memory ([`machine`](super::machine) decides, this module does).
//!
//! Every step looks at the world first, so a helper that was restarted (a
//! crash, a daemon restart, a reboot) carries on without repeating a
//! destructive step. While it lives, the helper alone writes the state file.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use super::machine::{Action, Ctr0, Seen, apply, next};
use super::state::{FORMAT, Phase, StateFile, break_glass, confirmed, load, now, save};
use super::{BACKUP_DIR, ROLLBACK_CHECK, STOP, helper_name, old_name, scaled};
use crate::docker::{cli, get_json, post_json};
use crate::managed::write_private;
use crate::run::{UNRAID_STATUS, create_body, write_unraid_status};

/// The files a rollback puts back from the run's backup folder.
const RESTORED: [&str; 2] = ["managed.json", "runs.json"];

/// What the helper needs of the Docker host. [`DockerEngine`] is the real
/// one; the tests use a fake.
pub trait Engine {
    /// `GET /containers/<name>/json`; `None` when there is no such container.
    fn inspect(&self, name: &str) -> impl Future<Output = Option<Value>> + Send;
    /// The `Config` of image `image_id` (`Null` when it can't be read).
    fn image_config(&self, image_id: &str) -> impl Future<Output = Value> + Send;
    fn stop(&self, name: &str, seconds: u64) -> impl Future<Output = Result<(), String>> + Send;
    fn rename(&self, from: &str, to: &str) -> impl Future<Output = Result<(), String>> + Send;
    /// Creates container `name` and returns its id.
    fn create(
        &self,
        name: &str,
        body: &Value,
    ) -> impl Future<Output = Result<String, String>> + Send;
    fn start(&self, name: &str) -> impl Future<Output = Result<(), String>> + Send;
    fn remove(&self, name: &str, force: bool) -> impl Future<Output = Result<(), String>> + Send;
    fn tag(&self, source: &str, target: &str) -> impl Future<Output = Result<(), String>> + Send;
    /// The host's kernel boot id (`/proc/sys/kernel/random/boot_id`).
    fn host_boot(&self) -> String;
    /// Unraid's update-status file ([`UNRAID_STATUS`]).
    fn unraid_status_file(&self) -> PathBuf;
    /// Seconds since the epoch.
    fn now(&self) -> u64;
    fn sleep(&self, d: Duration) -> impl Future<Output = ()> + Send;
}

/// Docker over its Unix socket.
pub struct DockerEngine {
    pub socket: String,
}

impl DockerEngine {
    async fn docker(&self, args: &[&str], timeout: Duration) -> Result<(), String> {
        cli(&self.socket, args, timeout).await.map(|_| ())
    }
}

impl Engine for DockerEngine {
    async fn inspect(&self, name: &str) -> Option<Value> {
        get_json(&self.socket, &format!("/containers/{name}/json"))
            .await
            .ok()
    }

    async fn image_config(&self, image_id: &str) -> Value {
        get_json(&self.socket, &format!("/images/{image_id}/json"))
            .await
            .ok()
            .and_then(|i| i.get("Config").cloned())
            .unwrap_or(Value::Null)
    }

    async fn stop(&self, name: &str, seconds: u64) -> Result<(), String> {
        let secs = seconds.to_string();
        self.docker(
            &["stop", "-t", &secs, name],
            Duration::from_secs(seconds + 30),
        )
        .await
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        self.docker(&["rename", from, to], Duration::from_secs(30))
            .await
    }

    async fn create(&self, name: &str, body: &Value) -> Result<String, String> {
        let answer = post_json(
            &self.socket,
            &format!("/containers/create?name={name}"),
            body,
        )
        .await?;
        answer
            .get("Id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "Docker created the container but sent no id".to_string())
    }

    async fn start(&self, name: &str) -> Result<(), String> {
        self.docker(&["start", name], Duration::from_secs(120))
            .await
    }

    async fn remove(&self, name: &str, force: bool) -> Result<(), String> {
        let mut args = vec!["rm"];
        if force {
            args.push("-f");
        }
        args.push(name);
        self.docker(&args, Duration::from_secs(60)).await
    }

    async fn tag(&self, source: &str, target: &str) -> Result<(), String> {
        self.docker(&["tag", source, target], Duration::from_secs(30))
            .await
    }

    fn host_boot(&self) -> String {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    fn unraid_status_file(&self) -> PathBuf {
        PathBuf::from(UNRAID_STATUS)
    }

    fn now(&self) -> u64 {
        now()
    }

    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}

/// What the helper makes of a container's inspect.
pub fn observe(inspect: Option<&Value>) -> Option<Ctr0> {
    let v = inspect?;
    Some(Ctr0 {
        id: v
            .get("Id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        running: v
            .pointer("/State/Running")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        status: v
            .pointer("/State/Status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        restart_count: v.get("RestartCount").and_then(Value::as_i64).unwrap_or(0),
    })
}

/// Carries out the run in `state_path` to its end and returns the last phase.
/// An error means the file could not be used at all (nothing was touched).
pub async fn run<E: Engine>(engine: &E, state_path: &Path) -> Result<Phase, String> {
    let mut s =
        load(state_path)?.ok_or_else(|| format!("{} doesn't exist", state_path.display()))?;
    if s.format != FORMAT || s.run_id == 0 {
        return Err(format!(
            "{} is not a run this helper understands (format {}, run {})",
            state_path.display(),
            s.format,
            s.run_id
        ));
    }
    // The config folder is wherever the state file is (the helper only sees
    // the folder's mount, under whatever name).
    let dir = state_path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let old_n = old_name(&s.container, s.run_id);
    tracing::info!(
        "self-update helper: run {} of {}, phase {:?}",
        s.run_id,
        s.container,
        s.phase
    );
    let mut first = true;
    loop {
        let before = s.clone();
        let now = engine.now();
        // The boot id is compared and stored on the first pass only, so a
        // reboot is seen once.
        let mut boot_changed = false;
        if first && !s.phase.terminal() {
            let boot = engine.host_boot();
            boot_changed = !boot.is_empty() && !s.host_boot.is_empty() && boot != s.host_boot;
            if !boot.is_empty() {
                s.host_boot = boot;
            }
        }
        let renamed = engine.inspect(&old_n).await;
        let seen = Seen {
            now,
            named: observe(engine.inspect(&s.container).await.as_ref()),
            renamed: observe(renamed.as_ref()),
            confirmed: confirmed(&dir, s.run_id),
            boot_changed,
            fresh_start: first,
        };
        first = false;
        let mut action = next(&s, &seen);
        match &action {
            Action::Finish => return Ok(finish(engine, &s).await),
            Action::Wait | Action::RollBack(_) => {}
            Action::Advance(_) | Action::ExtendDeadline => apply(&mut s, &action, &seen),
            // A stop or rename that fails leaves the old agent down or
            // half-moved: put it back at once.
            Action::StopOld => {
                if let Err(e) = engine.stop(&s.container, STOP).await {
                    action = Action::RollBack(format!("couldn't stop the old agent: {e}"));
                }
            }
            Action::RenameOld => {
                if let Err(e) = engine.rename(&s.container, &old_n).await {
                    action = Action::RollBack(format!("couldn't rename the old agent: {e}"));
                }
            }
            // Create and start are retried on the next pass until the time
            // for creating and starting the new agent is over.
            Action::CreateNew => match create_new(engine, &s, renamed.as_ref()).await {
                Ok(id) => s.new_container_id = id,
                Err(e) => tracing::warn!("couldn't create the new agent: {e}"),
            },
            Action::StartNew => match engine.start(&s.container).await {
                Ok(()) => apply(&mut s, &action, &seen),
                Err(e) => tracing::warn!("couldn't start the new agent: {e}"),
            },
            Action::CleanUp => {
                clean_up(engine, &s, &dir, &old_n).await;
                s.phase = Phase::Done;
                s.updated_at = seen.now;
            }
        }
        if let Action::RollBack(reason) = action {
            // A rollback that was begun keeps its first reason.
            if s.phase != Phase::RollingBack {
                s.phase = Phase::RollingBack;
                s.reason = reason;
            }
            tracing::warn!("rolling back: {}", s.reason);
            s.updated_at = seen.now;
            // Written before anything is touched, so a restarted helper (and
            // the old agent once it runs again) knows what is going on.
            save(state_path, &s)?;
            match roll_back(engine, &s, &dir, &old_n).await {
                Ok(()) => s.phase = Phase::RolledBack,
                Err(e) => {
                    tracing::error!("the rollback failed: {e}");
                    s.phase = Phase::RollbackFailed;
                    s.reason = format!("{}; {e}", s.reason);
                    s.break_glass = break_glass(&s.container, s.run_id);
                }
            }
            s.updated_at = engine.now();
            save(state_path, &s)?;
            continue;
        }
        if s != before {
            save(state_path, &s)?;
        }
        engine.sleep(Duration::from_secs(scaled(2))).await;
    }
}

/// Creates the new container like the old one (renamed out of the way), on
/// the new image.
async fn create_new<E: Engine>(
    engine: &E,
    s: &StateFile,
    renamed_inspect: Option<&Value>,
) -> Result<String, String> {
    let inspect = renamed_inspect.ok_or("the old container is gone")?;
    let image = engine.image_config(&s.old_image_id).await;
    let body = create_body(inspect, &image, &s.reference);
    engine.create(&s.container, &body).await
}

/// After `confirmed` and the stable period: the status file says the new
/// image is the one that runs, the old container and the backups go.
/// Nothing here is worth failing an update that works for.
async fn clean_up<E: Engine>(engine: &E, s: &StateFile, dir: &Path, old_n: &str) {
    if s.unraid {
        unraid_status(engine, s, &s.to_digest, &s.to_digest);
    }
    if engine.inspect(old_n).await.is_some()
        && let Err(e) = engine.remove(old_n, false).await
    {
        tracing::warn!("couldn't remove the old container {old_n}: {e}");
    }
    let backup = dir.join(BACKUP_DIR).join(s.run_id.to_string());
    match std::fs::remove_dir_all(&backup) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("couldn't remove {}: {e}", backup.display()),
    }
}

fn unraid_status<E: Engine>(engine: &E, s: &StateFile, local: &str, remote: &str) {
    if let Err(e) = write_unraid_status(&engine.unraid_status_file(), &s.reference, local, remote) {
        tracing::warn!("couldn't set Unraid's update status: {e}");
    }
}

/// Puts the old agent back. Every step looks at what is there first, so it
/// can be run again after a crash. The first error stops it.
async fn roll_back<E: Engine>(
    engine: &E,
    s: &StateFile,
    dir: &Path,
    old_n: &str,
) -> Result<(), String> {
    // 1. Whatever now has the agent's name is not the old agent.
    if let Some(c) = observe(engine.inspect(&s.container).await.as_ref())
        && c.id != s.old_container_id
    {
        engine.remove(&s.container, true).await?;
    }
    // 2. The old agent gets its name back.
    if engine.inspect(old_n).await.is_some() {
        engine.rename(old_n, &s.container).await?;
    }
    // 3. managed.json and runs.json as they were (a missing backup leaves the
    //    current file).
    let backup = dir.join(BACKUP_DIR).join(s.run_id.to_string());
    for name in RESTORED {
        match std::fs::read(backup.join(name)) {
            Ok(bytes) => write_private(&dir.join(name), &bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("can't read {}: {e}", backup.join(name).display())),
        }
    }
    // 4. The reference points at the old image again.
    engine.tag(&s.old_image_id, &s.reference).await?;
    if s.unraid {
        unraid_status(engine, s, &s.from_digest, &s.to_digest);
    }
    // 5. Start it and see that it stays up.
    engine.start(&s.container).await?;
    let until = engine.now() + scaled(ROLLBACK_CHECK);
    loop {
        engine.sleep(Duration::from_secs(scaled(2))).await;
        let up = observe(engine.inspect(&s.container).await.as_ref())
            .is_some_and(|c| c.running && c.status != "restarting");
        if !up {
            return Err("the old agent doesn't stay running".into());
        }
        if engine.now() >= until {
            return Ok(());
        }
    }
}

/// The run is over: the helper removes itself (which ends this process).
async fn finish<E: Engine>(engine: &E, s: &StateFile) -> Phase {
    tracing::info!("run {} ended as {:?}", s.run_id, s.phase);
    if let Err(e) = engine.remove(&helper_name(s.run_id), true).await {
        tracing::warn!("couldn't remove the helper container: {e}");
    }
    s.phase
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::*;
    use crate::selfupdate::state::{Phase, StateFile, break_glass, load, save, write_confirm};

    const REF: &str = "ghcr.io/ferdinand99/wise-wren-agent:latest";
    const T0: u64 = 1_000_000;

    #[derive(Clone)]
    struct FakeCtr {
        id: String,
        running: bool,
        status: String,
        restarts: i64,
    }

    /// A Docker host in memory: containers by name, a log of what the helper
    /// changed, and a clock that only `sleep` moves.
    struct Fake {
        ctrs: Mutex<BTreeMap<String, FakeCtr>>,
        log: Mutex<Vec<String>>,
        /// The phase in the state file at each logged call.
        phases: Mutex<Vec<(String, Phase)>>,
        clock: AtomicU64,
        fail_on: Option<&'static str>,
        /// How many matching calls still fail.
        fail_left: Mutex<u32>,
        confirm: bool,
        unraid: bool,
        dir: Mutex<PathBuf>,
    }

    impl Fake {
        fn new(confirm: bool) -> Fake {
            Fake {
                ctrs: Mutex::new(BTreeMap::new()),
                log: Mutex::new(Vec::new()),
                phases: Mutex::new(Vec::new()),
                clock: AtomicU64::new(T0),
                fail_on: None,
                fail_left: Mutex::new(u32::MAX),
                confirm,
                unraid: false,
                dir: Mutex::new(PathBuf::new()),
            }
        }
        /// The new agent writes the confirm marker as soon as it is started.
        fn confirming() -> Fake {
            Fake::new(true)
        }
        /// The new agent never reaches Wren.
        fn silent() -> Fake {
            Fake::new(false)
        }
        fn failing(mut self, prefix: &'static str) -> Fake {
            self.fail_on = Some(prefix);
            self
        }
        fn failing_times(self, prefix: &'static str, times: u32) -> Fake {
            *self.fail_left.lock().unwrap() = times;
            self.failing(prefix)
        }
        fn unraid(mut self) -> Fake {
            self.unraid = true;
            self
        }
        fn dir(&self) -> PathBuf {
            self.dir.lock().unwrap().clone()
        }
        fn calls(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
        fn ctr(&self, name: &str) -> Option<FakeCtr> {
            self.ctrs.lock().unwrap().get(name).cloned()
        }
        fn put(&self, name: &str, id: &str, running: bool, status: &str) {
            self.ctrs.lock().unwrap().insert(
                name.into(),
                FakeCtr {
                    id: id.into(),
                    running,
                    status: status.into(),
                    restarts: 0,
                },
            );
        }
        /// Logs the call, then fails it when it is the one set to fail.
        fn record(&self, call: String) -> Result<(), String> {
            let phase = load(&self.dir().join("selfupdate.json"))
                .ok()
                .flatten()
                .map(|s| s.phase);
            if let Some(p) = phase {
                self.phases.lock().unwrap().push((call.clone(), p));
            }
            self.log.lock().unwrap().push(call.clone());
            let mut left = self.fail_left.lock().unwrap();
            match self.fail_on {
                Some(prefix) if call.starts_with(prefix) && *left > 0 => {
                    *left -= 1;
                    Err(format!("{call} failed"))
                }
                _ => Ok(()),
            }
        }
    }

    impl Engine for Fake {
        async fn inspect(&self, name: &str) -> Option<Value> {
            // The new agent confirms once the helper has recorded its start.
            if self.confirm
                && let Ok(Some(s)) = load(&self.dir().join("selfupdate.json"))
                && s.phase == Phase::NewStarted
            {
                write_confirm(&self.dir(), s.run_id).unwrap();
            }
            let c = self.ctr(name)?;
            Some(json!({
                "Id": c.id,
                "RestartCount": c.restarts,
                "State": {"Running": c.running, "Status": c.status},
                "Config": {"Image": "old-image", "Env": ["A=1"]},
                "HostConfig": {"NetworkMode": "bridge"},
            }))
        }
        async fn image_config(&self, _image_id: &str) -> Value {
            json!({})
        }
        async fn stop(&self, name: &str, seconds: u64) -> Result<(), String> {
            self.record(format!("stop {name} {seconds}"))?;
            self.put(
                name,
                &self.ctr(name).ok_or("no such container")?.id,
                false,
                "exited",
            );
            Ok(())
        }
        async fn rename(&self, from: &str, to: &str) -> Result<(), String> {
            self.record(format!("rename {from} {to}"))?;
            let mut all = self.ctrs.lock().unwrap();
            if all.contains_key(to) {
                return Err(format!("{to} exists"));
            }
            let c = all.remove(from).ok_or("no such container")?;
            all.insert(to.into(), c);
            Ok(())
        }
        async fn create(&self, name: &str, body: &Value) -> Result<String, String> {
            self.record(format!("create {name}"))?;
            assert_eq!(body["Image"], REF);
            if self.ctr(name).is_some() {
                return Err(format!("{name} exists"));
            }
            self.put(name, "new", false, "created");
            Ok("new".into())
        }
        async fn start(&self, name: &str) -> Result<(), String> {
            self.record(format!("start {name}"))?;
            let id = self.ctr(name).ok_or("no such container")?.id;
            self.put(name, &id, true, "running");
            Ok(())
        }
        async fn remove(&self, name: &str, force: bool) -> Result<(), String> {
            self.record(format!(
                "remove {name}{}",
                if force { " force" } else { "" }
            ))?;
            self.ctrs
                .lock()
                .unwrap()
                .remove(name)
                .map(|_| ())
                .ok_or_else(|| "no such container".to_string())
        }
        async fn tag(&self, source: &str, target: &str) -> Result<(), String> {
            self.record(format!("tag {source} {target}"))
        }
        fn host_boot(&self) -> String {
            "boot-1".into()
        }
        fn unraid_status_file(&self) -> PathBuf {
            self.dir().join("unraid-update-status.json")
        }
        fn now(&self) -> u64 {
            self.clock.load(Ordering::SeqCst)
        }
        async fn sleep(&self, d: Duration) {
            self.clock.fetch_add(d.as_secs().max(1), Ordering::SeqCst);
        }
    }

    fn setup(name: &str, fake: Fake) -> (Fake, PathBuf) {
        setup_at(name, fake, Phase::Prepared)
    }

    /// A run in `phase`: the config folder, the backups, the state file and
    /// the containers as they stand in that phase.
    fn setup_at(name: &str, fake: Fake, phase: Phase) -> (Fake, PathBuf) {
        use Phase::*;
        let dir =
            std::env::temp_dir().join(format!("wren-su-helper-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("selfupdate/57")).unwrap();
        *fake.dir.lock().unwrap() = dir.clone();
        for (file, before) in [
            ("managed.json", &b"{\"before\":1}"[..]),
            ("runs.json", &b"{\"ledger\":\"before\"}"[..]),
        ] {
            std::fs::write(dir.join("selfupdate/57").join(file), before).unwrap();
            std::fs::write(dir.join(file), b"{\"after\":1}").unwrap();
        }
        if fake.unraid {
            std::fs::write(
                fake.unraid_status_file(),
                include_str!("../../tests/fixtures/unraid-update-status.json"),
            )
            .unwrap();
        }
        match phase {
            Prepared | HelperStarted => fake.put("wren-agent", "old", true, "running"),
            OldStopped => fake.put("wren-agent", "old", false, "exited"),
            _ => fake.put("wren-agent-wren-old-57", "old", false, "exited"),
        }
        match phase {
            NewCreated => fake.put("wren-agent", "new", false, "created"),
            NewStarted | Confirmed => fake.put("wren-agent", "new", true, "running"),
            _ => {}
        }
        let path = dir.join("selfupdate.json");
        let s = StateFile {
            format: 1,
            run_id: 57,
            server_id: 3,
            container: "wren-agent".into(),
            reference: REF.into(),
            from_version: "0.9.2".into(),
            to_version: "0.10.0".into(),
            old_container_id: "old".into(),
            old_image_id: "sha256:old".into(),
            from_digest: "sha256:old".into(),
            to_digest: "sha256:new".into(),
            unraid: fake.unraid,
            phase,
            updated_at: T0,
            confirm_deadline: T0 + 300,
            hard_deadline: T0 + 1800,
            confirm_minutes: 5,
            stable_seconds: 60,
            host_boot: "boot-1".into(),
            ..StateFile::default()
        };
        save(&path, &s).unwrap();
        (fake, path)
    }

    #[tokio::test]
    async fn a_good_update_swaps_the_container_and_cleans_up() {
        let (fake, path) = setup("good", Fake::confirming());
        assert_eq!(run(&fake, &path).await.unwrap(), Phase::Done);
        assert_eq!(
            fake.calls(),
            [
                "stop wren-agent 60",
                "rename wren-agent wren-agent-wren-old-57",
                "create wren-agent",
                "start wren-agent",
                "remove wren-agent-wren-old-57",
                "remove wren-agent-selfupdate-57 force",
            ]
        );
        assert_eq!(fake.ctr("wren-agent").unwrap().id, "new");
        assert!(!path.parent().unwrap().join("selfupdate/57").exists());
        let s = load(&path).unwrap().unwrap();
        assert_eq!((s.phase, s.new_container_id.as_str()), (Phase::Done, "new"));
    }

    #[tokio::test]
    async fn no_confirmation_rolls_back_to_the_same_old_container() {
        let (fake, path) = setup("silent", Fake::silent());
        assert_eq!(run(&fake, &path).await.unwrap(), Phase::RolledBack);
        let c = fake.ctr("wren-agent").unwrap();
        assert_eq!((c.id.as_str(), c.running), ("old", true));
        for call in [
            "remove wren-agent force",
            "rename wren-agent-wren-old-57 wren-agent",
            "tag sha256:old ghcr.io/ferdinand99/wise-wren-agent:latest",
            "start wren-agent",
        ] {
            assert!(fake.calls().iter().any(|x| x == call), "{call}");
        }
        let dir = path.parent().unwrap();
        assert_eq!(
            std::fs::read(dir.join("managed.json")).unwrap(),
            b"{\"before\":1}"
        );
        assert_eq!(
            std::fs::read(dir.join("runs.json")).unwrap(),
            b"{\"ledger\":\"before\"}"
        );
        assert!(
            load(&path)
                .unwrap()
                .unwrap()
                .reason
                .contains("didn't reach Wren within 5 min")
        );
    }

    #[tokio::test]
    async fn a_restarted_helper_resumes_without_repeating_steps() {
        let (fake, path) = setup_at("resume", Fake::confirming(), Phase::OldRenamed);
        assert_eq!(run(&fake, &path).await.unwrap(), Phase::Done);
        assert!(
            !fake
                .calls()
                .iter()
                .any(|c| c.starts_with("stop") || c == "rename wren-agent wren-agent-wren-old-57")
        );
    }

    #[tokio::test]
    async fn a_failed_rename_back_stops_touching_anything() {
        let (fake, path) = setup(
            "rbfail",
            Fake::silent().failing("rename wren-agent-wren-old-57"),
        );
        assert_eq!(run(&fake, &path).await.unwrap(), Phase::RollbackFailed);
        let s = load(&path).unwrap().unwrap();
        assert_eq!(s.break_glass, break_glass("wren-agent", 57));
        assert!(s.reason.contains("didn't reach Wren"), "{}", s.reason);
        assert!(
            s.reason.contains("; rename wren-agent-wren-old-57"),
            "{}",
            s.reason
        );
        let calls = fake.calls();
        let i = calls
            .iter()
            .position(|c| c == "rename wren-agent-wren-old-57 wren-agent")
            .unwrap();
        assert!(
            calls[i + 1..]
                .iter()
                .all(|c| c.starts_with("remove wren-agent-selfupdate-57")),
            "{calls:?}"
        );
    }

    #[tokio::test]
    async fn on_unraid_the_status_file_follows_the_container() {
        for (fake, want_local, want_status) in [
            (Fake::confirming().unraid(), "sha256:new", "true"),
            (Fake::silent().unraid(), "sha256:old", "false"),
        ] {
            let (fake, path) = setup("unraid", fake);
            run(&fake, &path).await.unwrap();
            let v: Value =
                serde_json::from_str(&std::fs::read_to_string(fake.unraid_status_file()).unwrap())
                    .unwrap();
            let e = &v[REF];
            assert_eq!(
                (e["local"].as_str().unwrap(), e["status"].as_str().unwrap()),
                (want_local, want_status)
            );
            // The other entries stay.
            assert_eq!(v["lscr.io/linuxserver/sonarr:latest"]["status"], "true");
        }
    }

    #[tokio::test]
    async fn not_on_unraid_the_status_file_is_left_alone() {
        let (fake, path) = setup("notunraid", Fake::confirming());
        run(&fake, &path).await.unwrap();
        assert!(!fake.unraid_status_file().exists());
    }

    #[tokio::test]
    async fn the_phase_says_rolling_back_while_the_old_agent_starts() {
        let (fake, path) = setup("phase", Fake::silent());
        assert_eq!(run(&fake, &path).await.unwrap(), Phase::RolledBack);
        let at = fake.phases.lock().unwrap().clone();
        let start = at
            .iter()
            .rev()
            .find(|(c, _)| c == "start wren-agent")
            .unwrap();
        assert_eq!(start.1, Phase::RollingBack);
    }

    #[tokio::test]
    async fn a_new_agent_that_dies_at_once_rolls_back_for_that_reason() {
        let (fake, path) = setup_at("dies", Fake::silent(), Phase::NewCreated);
        // `start` worked but the agent exited at once (restart backoff).
        fake.ctrs.lock().unwrap().insert(
            "wren-agent".into(),
            FakeCtr {
                id: "new".into(),
                running: false,
                status: "restarting".into(),
                restarts: 1,
            },
        );
        assert_eq!(run(&fake, &path).await.unwrap(), Phase::RolledBack);
        let s = load(&path).unwrap().unwrap();
        assert!(
            s.reason.contains("the new agent stopped or restarted"),
            "{}",
            s.reason
        );
    }

    #[tokio::test]
    async fn a_host_that_restarted_is_counted_once_and_the_new_boot_stored() {
        let (fake, path) = setup_at("reboot", Fake::confirming(), Phase::NewStarted);
        let mut s = load(&path).unwrap().unwrap();
        s.host_boot = "an-older-boot".into();
        save(&path, &s).unwrap();
        assert_eq!(run(&fake, &path).await.unwrap(), Phase::Done);
        let s = load(&path).unwrap().unwrap();
        assert_eq!((s.reboots_seen, s.host_boot.as_str()), (1, "boot-1"));
    }

    #[tokio::test]
    async fn a_finished_run_only_removes_the_helper() {
        let (fake, path) = setup_at("finished", Fake::silent(), Phase::Done);
        assert_eq!(run(&fake, &path).await.unwrap(), Phase::Done);
        assert_eq!(fake.calls(), ["remove wren-agent-selfupdate-57 force"]);
    }

    #[tokio::test]
    async fn a_failing_start_is_tried_again_not_taken_for_done() {
        let fake = Fake::confirming().failing_times("start wren-agent", 3);
        let (fake, path) = setup_at("startfail", fake, Phase::NewCreated);
        assert_eq!(run(&fake, &path).await.unwrap(), Phase::Done);
        let starts = fake
            .calls()
            .iter()
            .filter(|c| *c == "start wren-agent")
            .count();
        assert_eq!(starts, 4, "{:?}", fake.calls());
    }

    #[tokio::test]
    async fn a_file_the_helper_does_not_understand_is_an_error() {
        let (fake, path) = setup("format2", Fake::silent());
        let mut s = load(&path).unwrap().unwrap();
        s.format = 2;
        save(&path, &s).unwrap();
        assert!(run(&fake, &path).await.is_err());
        s.format = 1;
        s.run_id = 0;
        save(&path, &s).unwrap();
        assert!(run(&fake, &path).await.is_err());
        assert!(
            run(&fake, &path.with_file_name("nothing.json"))
                .await
                .is_err()
        );
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn what_the_helper_sees_of_a_container() {
        assert_eq!(observe(None), None);
        let v = json!({"Id": "abc", "RestartCount": 3, "State": {"Running": false, "Status": "restarting"}});
        assert_eq!(
            observe(Some(&v)),
            Some(Ctr0 {
                id: "abc".into(),
                running: false,
                status: "restarting".into(),
                restart_count: 3
            })
        );
    }
}
