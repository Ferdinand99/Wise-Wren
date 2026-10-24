//! A script stack against a real Docker (run 48 on Tower: Fluxer's installer
//! failed before it changed anything, and the rollback stopped the stack).
//! Needs the Docker CLI with Compose and `WW_DOCKER_TESTS=1`; makes and
//! removes the Compose project `ww-test-ss` only.
//!
//! ```sh
//! WW_DOCKER_TESTS=1 cargo test -p wren-agent --test docker_script_stack -- --ignored --test-threads=1
//! ```

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};
use wren_agent::config::Config;
use wren_agent::jobs::{self, Job, Note};
use wren_agent::run::{self, DockerHost, Report};

const PROJECT: &str = "ww-test-ss";
const SOCKET: &str = "/var/run/docker.sock";

#[derive(Default)]
struct Log {
    steps: Mutex<Vec<String>>,
    finished: Mutex<Option<(String, String)>>,
}

impl Report for Log {
    async fn step(
        &self,
        step: &str,
        status: &str,
        log: &str,
        _: u64,
        _: &[Note],
    ) -> Result<bool, String> {
        if status != "running" {
            self.steps
                .lock()
                .unwrap()
                .push(format!("{step}:{status}: {log}"));
        }
        Ok(false)
    }
    async fn finish(&self, outcome: &str, note: &Note) {
        *self.finished.lock().unwrap() = Some((outcome.into(), note.text()));
    }
    async fn monitors(&self) -> Result<(String, String), String> {
        Ok(("skipped".into(), String::new()))
    }
}

fn sh(dir: &Path, command: &str) -> String {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .env("COMPOSE_PROJECT_NAME", PROJECT)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{command}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A fresh two-container stack; its folder.
fn stack(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("{PROJECT}-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("docker-compose.yml"),
        "services:\n  db:\n    image: busybox:1.37\n    command: sleep 3600\n    labels: [owner=wise-wren]\n  web:\n    image: busybox:1.37\n    command: sleep 3600\n    labels: [owner=wise-wren]\n",
    )
    .unwrap();
    sh(
        &dir,
        "docker compose down -t 0 >/dev/null 2>&1; docker compose up -d --quiet-pull",
    );
    dir
}

fn ids(dir: &Path) -> String {
    sh(dir, "docker compose ps -q --status running | sort")
}

/// Runs an update of the stack with these scripts; the outcome and the detail.
async fn update(dir: &Path, check: &str, update: &str, rollback: &str) -> (String, String, Log) {
    let toml = format!(
        "server = \"http://wren\"\ntoken = \"wwa_{t}\"\nroots = [{root:?}]\n\n\
         [[script_stack]]\nname = {PROJECT:?}\nworking_dir = {dir:?}\nhandles_own_backup = true\n\
         env = {{ COMPOSE_PROJECT_NAME = {PROJECT:?} }}\ntimeout = \"2m\"\n\n\
         [script_stack.check]\ncommand = {check:?}\n\n\
         [script_stack.update]\ncommand = {update:?}\n\n\
         [script_stack.rollback]\ncommand = {rollback:?}\n",
        t = "a".repeat(43),
        root = std::env::temp_dir().display().to_string(),
        dir = dir.display().to_string(),
    );
    let config = Config::from_parts(Some(&toml), |_| None).unwrap_or_else(|e| panic!("{e}"));
    let job: Job = serde_json::from_value(json!({
        "run_id": 1, "kind": "update", "target": {"kind": "script", "name": PROJECT},
        "health_seconds": 2
    }))
    .unwrap();
    let all = run::read_all(SOCKET).await.unwrap();
    let plan = jobs::check(&job, &config, &all, &[], "").unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(plan.ctrs.len(), 2);
    let log = Log::default();
    let host = DockerHost { config: &config };
    let outcome = tokio::time::timeout(Duration::from_secs(300), run::execute(&host, &log, &plan))
        .await
        .unwrap();
    let detail = log.finished.lock().unwrap().clone().unwrap().1;
    (outcome.to_string(), detail, log)
}

fn down(dir: &Path) {
    sh(
        dir,
        &format!(
            "docker compose down -t 0 >/dev/null 2>&1; docker images --format '{{{{.Repository}}}}:{{{{.Tag}}}}'              | grep '^wren-rollback/{PROJECT}:' | xargs -r docker rmi >/dev/null; true"
        ),
    );
    let _ = std::fs::remove_dir_all(dir);
}

fn on() -> bool {
    std::env::var("WW_DOCKER_TESTS").is_ok_and(|v| v == "1")
}

#[tokio::test]
#[ignore = "needs Docker: WW_DOCKER_TESTS=1"]
async fn a_failed_check_touches_nothing() {
    if !on() {
        return;
    }
    let dir = stack("check");
    let before = ids(&dir);
    let (outcome, detail, log) = update(&dir, "echo not yet; false", "touch updated", "true").await;
    assert_eq!(outcome, "failed");
    assert!(
        detail.contains("the check says the update can't run now"),
        "{detail}"
    );
    assert!(detail.contains("not yet"), "{detail}");
    assert!(!dir.join("updated").exists(), "the update ran");
    assert_eq!(ids(&dir), before);
    assert!(
        log.steps
            .lock()
            .unwrap()
            .iter()
            .all(|s| !s.starts_with("update"))
    );
    down(&dir);
}

#[tokio::test]
#[ignore = "needs Docker: WW_DOCKER_TESTS=1"]
async fn a_script_that_changed_nothing_leaves_the_stack_running() {
    if !on() {
        return;
    }
    let dir = stack("nothing");
    let before = ids(&dir);
    let (outcome, detail, log) =
        update(&dir, "true", "echo 404; false", "docker compose stop").await;
    assert_eq!(outcome, "failed", "{:?}", log.steps.lock().unwrap());
    assert!(detail.contains("404"), "{detail}");
    assert!(
        log.steps
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.starts_with("rollback:skipped")),
        "{:?}",
        log.steps.lock().unwrap()
    );
    assert_eq!(ids(&dir), before, "the same containers still run");
    down(&dir);
}

#[tokio::test]
#[ignore = "needs Docker: WW_DOCKER_TESTS=1"]
async fn the_rollback_of_an_own_backup_stack_stops_nothing_itself() {
    if !on() {
        return;
    }
    let dir = stack("own");
    // The script stops web and fails; the rollback script does nothing, so
    // web stays down and the rollback's health check fails.
    let (outcome, detail, log) =
        update(&dir, "true", "docker compose stop web; false", "true").await;
    assert_eq!(
        outcome,
        "rollback_failed",
        "{:?}",
        log.steps.lock().unwrap()
    );
    assert!(
        detail.ends_with(&format!("; not running now: {PROJECT}-web-1")),
        "{detail}"
    );
    // Wren didn't stop db before the rollback script.
    let running: Value = serde_json::from_str(&sh(
        &dir,
        &format!("docker inspect -f '{{{{json .State.Running}}}}' {PROJECT}-db-1"),
    ))
    .unwrap();
    assert_eq!(running, json!(true));
    down(&dir);
}
