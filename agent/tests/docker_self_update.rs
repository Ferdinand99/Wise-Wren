//! The agent updates itself against a real Docker (design: agent
//! self-update, section 14). A local registry (`ww-test-su-registry`, port
//! 5057) holds test builds of the agent (`--features selfupdate-test`, version
//! and label chosen per image); a fake Wren in this process answers the agent;
//! the agent runs in a container on the same Docker and updates itself from
//! one test version to the next.
//!
//! Needs the Docker CLI with a socket, a Rust toolchain (the test compiles the
//! agent) and `WW_DOCKER_TESTS=1`. Everything the tests make is named
//! `ww-test-su-…` and labelled `owner=wise-wren`, except the helper containers
//! the agent itself makes (`wren-agent-selfupdate-<run>`, found by the test's
//! config volume, which only the test's own containers mount). Each test
//! removes only its own, and the scenarios run one at a time.
//! `ww-test-su-registry` and the test images stay for the next run.
//!
//! ```sh
//! WW_DOCKER_TESTS=1 cargo test -p wren-agent --test docker_self_update -- --ignored --test-threads=1
//! ```

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wren_agent::selfupdate::state::{Phase, StateFile};

// The registry address; the scenarios of Task 10 use it.
#[allow(dead_code)]
const REG: &str = "localhost:5057";
const REPO: &str = "localhost:5057/ww-test-su/agent";
const REGISTRY: &str = "ww-test-su-registry";
const ALPINE: &str = "alpine:3.22";

// --- Docker -----------------------------------------------------------------

/// Whether the Docker tests are switched on.
fn on() -> bool {
    std::env::var("WW_DOCKER_TESTS").as_deref() == Ok("1")
}

/// `docker <args>`: its trimmed stdout, or its stderr as the error.
fn try_docker(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker")
        .args(args)
        .output()
        .map_err(|e| format!("docker: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "docker {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// `docker <args>`; panics with Docker's stderr.
fn docker(args: &[&str]) -> String {
    try_docker(args).unwrap_or_else(|e| panic!("{e}"))
}

/// The local registry the test builds are pushed to: started when it isn't
/// there, and refused when something else holds its port.
fn ensure_registry() {
    match try_docker(&["inspect", "-f", "{{.State.Running}}", REGISTRY]).as_deref() {
        Ok("true") => return,
        Ok(_) => {
            docker(&["start", REGISTRY]);
        }
        Err(_) => {
            assert!(
                TcpListener::bind("0.0.0.0:5057").is_ok(),
                "port 5057 is in use, and not by {REGISTRY}"
            );
            docker(&[
                "run",
                "-d",
                "--name",
                REGISTRY,
                "--label",
                "owner=wise-wren",
                "-p",
                "5057:5000",
                "--tmpfs",
                "/var/lib/registry",
                "registry:2",
            ]);
        }
    }
    let end = Instant::now() + Duration::from_secs(30);
    while TcpStream::connect("127.0.0.1:5057").is_err() {
        assert!(Instant::now() < end, "{REGISTRY} didn't come up");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Version to (label, digest) of the images this test process has built.
fn built() -> &'static Mutex<HashMap<String, (String, String)>> {
    static BUILT: OnceLock<Mutex<HashMap<String, (String, String)>>> = OnceLock::new();
    BUILT.get_or_init(Mutex::default)
}

/// Builds the agent as version `version` with the image label `label`,
/// pushes it as `REPO:<version>` and returns its digest. Once per test
/// process and version: the tag is the version, so a version has one label
/// (a variant with another label needs a tag of its own).
fn build(version: &str, label: &str) -> String {
    if let Some((built_label, digest)) = built().lock().unwrap().get(version) {
        assert_eq!(
            built_label, label,
            "{version} was built with another label; give a variant its own tag"
        );
        return digest.clone();
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the agent crate is in the workspace");
    // Its own target folder (in the same cached volume): no lock clash with
    // the cargo that runs this test, and only `wren-agent` compiles again when
    // the version changes.
    let target = root.join("target/su-images");
    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--locked",
            "-p",
            "wren-agent",
            "--features",
            "selfupdate-test",
        ])
        .current_dir(root)
        .env("WW_SU_TEST_VERSION", version)
        .env("WW_SU_TEST_REPO", REPO)
        .env("CARGO_TARGET_DIR", &target)
        .status()
        .expect("can't run cargo");
    assert!(status.success(), "cargo build of {version} failed");
    let dir = std::env::temp_dir().join(format!("ww-test-su-build-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(target.join("release/wren-agent"), dir.join("wren-agent")).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/selfupdate.Dockerfile"),
        dir.join("Dockerfile"),
    )
    .unwrap();
    let tag = format!("{REPO}:{version}");
    docker(&[
        "build",
        "--build-arg",
        &format!("WW_SU_LABEL_VERSION={label}"),
        "-t",
        &tag,
        dir.to_str().unwrap(),
    ]);
    let _ = std::fs::remove_dir_all(&dir);
    docker(&["push", &tag]);
    let digests = docker(&["image", "inspect", "-f", "{{json .RepoDigests}}", &tag]);
    let digests: Vec<String> = serde_json::from_str(&digests).unwrap();
    let prefix = format!("{REPO}@");
    let digest = digests
        .iter()
        .find_map(|d| d.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("{tag} has no digest in {REPO}: {digests:?}"))
        .to_string();
    built()
        .lock()
        .unwrap()
        .insert(version.to_string(), (label.to_string(), digest.clone()));
    digest
}

/// Makes `REPO:<version>` the release: it becomes `:latest` and `:0` too.
fn publish(version: &str) {
    for tag in ["latest", "0"] {
        let to = format!("{REPO}:{tag}");
        docker(&["tag", &format!("{REPO}:{version}"), &to]);
        docker(&["push", &to]);
    }
}

fn inspect(name: &str) -> Value {
    let all: Value = serde_json::from_str(&docker(&["inspect", name])).unwrap();
    all[0].clone()
}

/// The ids of the containers whose name contains `part` (empty: none).
fn containers_named(part: &str) -> String {
    docker(&["ps", "-aq", "--filter", &format!("name={part}")])
}

/// Waits until no container has `part` in its name. The new agent removes the
/// helper just after it has told Wren, so a test that has the finish must wait
/// a little for that.
async fn wait_gone(part: &str, within: Duration) {
    let end = Instant::now() + within;
    while !containers_named(part).is_empty() {
        assert!(
            Instant::now() < end,
            "a container named {part} is still there after {within:?}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The container runs and is not just between two restarts (Docker says
/// "running" during a restart backoff too).
fn running(name: &str) -> bool {
    try_docker(&[
        "inspect",
        "-f",
        "{{.State.Running}} {{.State.Restarting}}",
        name,
    ])
    .as_deref()
        == Ok("true false")
}

// --- The fake Wren -----------------------------------------------------------

/// How the fake answers.
#[derive(Clone, Debug)]
enum Mode {
    Ok,
    /// 401 to the heartbeats of this agent version.
    Refuse {
        agent_version: String,
    },
    /// Closes every connection without an answer.
    // Used by the Wren-down scenario of Task 10.
    #[allow(dead_code)]
    Down,
}

struct Step {
    run: i64,
    step: String,
    status: String,
    at: Instant,
}

struct Finish {
    run: i64,
    outcome: String,
    code: String,
    detail: String,
    at: Instant,
}

struct Shared {
    server_id: i64,
    jobs: Mutex<VecDeque<Value>>,
    mode: Mutex<Mode>,
    steps: Mutex<Vec<Step>>,
    finishes: Mutex<Vec<Finish>>,
    /// The `agent_version` of every heartbeat.
    beats: Mutex<Vec<String>>,
    stop: AtomicBool,
}

/// Enough of Wren's agent API for the agent to run and to update itself.
/// It runs on threads of its own (std networking): the test body blocks on
/// `docker` and `cargo` for minutes, and the agent must still be answered.
struct FakeWren {
    url: String,
    // The id the fake answers with; Task 10 reads it.
    #[allow(dead_code)]
    server_id: i64,
    port: u16,
    shared: Arc<Shared>,
}

impl FakeWren {
    async fn start(server_id: i64) -> FakeWren {
        let listener = TcpListener::bind("0.0.0.0:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let shared = Arc::new(Shared {
            server_id,
            jobs: Mutex::default(),
            mode: Mutex::new(Mode::Ok),
            steps: Mutex::default(),
            finishes: Mutex::default(),
            beats: Mutex::default(),
            stop: AtomicBool::new(false),
        });
        let accept = shared.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if accept.stop.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(conn) = conn else { continue };
                let shared = accept.clone();
                std::thread::spawn(move || serve(conn, &shared));
            }
        });
        FakeWren {
            url: format!("http://host.docker.internal:{port}"),
            server_id,
            port,
            shared,
        }
    }

    fn queue_job(&self, job: Value) {
        self.shared.jobs.lock().unwrap().push_back(job);
    }

    fn set_mode(&self, m: Mode) {
        *self.shared.mode.lock().unwrap() = m;
    }

    /// The outcome and the code of the run's finish, waiting up to `within`.
    async fn finish_of(&self, run: i64, within: Duration) -> (String, String) {
        let end = Instant::now() + within;
        loop {
            if let Some(f) = self.finish(run) {
                return (f.0, f.1);
            }
            assert!(
                Instant::now() < end,
                "run {run} didn't finish within {within:?}; steps {:?}; heartbeats from {:?}",
                self.steps_of(run),
                self.shared.beats.lock().unwrap().iter().fold(
                    Vec::<(String, usize)>::new(),
                    |mut all, v| {
                        match all.iter_mut().find(|(seen, _)| seen == v) {
                            Some((_, n)) => *n += 1,
                            None => all.push((v.clone(), 1)),
                        }
                        all
                    }
                )
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// The first finish the run got: outcome, code, detail.
    fn finish(&self, run: i64) -> Option<(String, String, String)> {
        self.shared
            .finishes
            .lock()
            .unwrap()
            .iter()
            .find(|f| f.run == run)
            .map(|f| (f.outcome.clone(), f.code.clone(), f.detail.clone()))
    }

    /// The step's last status, in the order the steps first arrived.
    fn steps_of(&self, run: i64) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for s in self
            .shared
            .steps
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.run == run)
        {
            match out.iter_mut().find(|(step, _)| *step == s.step) {
                Some(seen) => seen.1.clone_from(&s.status),
                None => out.push((s.step.clone(), s.status.clone())),
            }
        }
        out
    }

    /// When the step first arrived.
    fn step_at(&self, run: i64, step: &str) -> Option<Instant> {
        self.shared
            .steps
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.run == run && s.step == step)
            .map(|s| s.at)
    }

    /// When the run's first finish arrived.
    fn finish_at(&self, run: i64) -> Option<Instant> {
        self.shared
            .finishes
            .lock()
            .unwrap()
            .iter()
            .find(|f| f.run == run)
            .map(|f| f.at)
    }

    /// How many heartbeats came from this agent version.
    fn beats_from(&self, version: &str) -> usize {
        self.shared
            .beats
            .lock()
            .unwrap()
            .iter()
            .filter(|v| *v == version)
            .count()
    }
}

impl Drop for FakeWren {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // Wakes the thread that waits in `accept`.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// `(method, path with query, body)` of one HTTP/1.1 request.
fn read_request(conn: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
    conn.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        let n = conn.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut request = lines.next()?.split_whitespace();
    let method = request.next()?.to_string();
    let path = request.next()?.to_string();
    let length = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let n = conn.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some((method, path, body))
}

fn respond(conn: &mut TcpStream, status: &str, body: &str) {
    let text = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = conn.write_all(text.as_bytes());
    let _ = conn.flush();
}

/// One request, one answer.
fn serve(mut conn: TcpStream, shared: &Shared) {
    let Some((method, path, body)) = read_request(&mut conn) else {
        return;
    };
    let mode = shared.mode.lock().unwrap().clone();
    if matches!(mode, Mode::Down) {
        return;
    }
    let (route, query) = path.split_once('?').unwrap_or((path.as_str(), ""));
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let text = |key: &str| json[key].as_str().unwrap_or("").to_string();
    match (method.as_str(), route) {
        ("POST", "/api/agent/v1/heartbeat") => {
            let version = text("agent_version");
            shared.beats.lock().unwrap().push(version.clone());
            if matches!(&mode, Mode::Refuse { agent_version } if *agent_version == version) {
                respond(&mut conn, "401 Unauthorized", r#"{"error":"unauthorized"}"#);
                return;
            }
            let reply = json!({
                "server": {"id": shared.server_id, "name": "ww-test"},
                "interval_seconds": 5,
                "scan": false
            });
            respond(&mut conn, "200 OK", &reply.to_string());
        }
        ("GET", "/api/agent/v1/jobs") => {
            let quick = query.split('&').any(|p| p == "wait=0");
            let mut job = shared.jobs.lock().unwrap().pop_front();
            if job.is_none() && !quick {
                std::thread::sleep(Duration::from_secs(1));
                job = shared.jobs.lock().unwrap().pop_front();
            }
            match job {
                Some(job) => respond(&mut conn, "200 OK", &json!({"job": job}).to_string()),
                None => respond(&mut conn, "204 No Content", ""),
            }
        }
        ("POST", "/api/agent/v1/inventory") => respond(&mut conn, "204 No Content", ""),
        ("POST", other) => {
            let parts: Vec<&str> = other.trim_matches('/').split('/').collect();
            match parts.as_slice() {
                ["api", "agent", "v1", "runs", run, "steps"] => {
                    shared.steps.lock().unwrap().push(Step {
                        run: run.parse().unwrap_or(0),
                        step: text("step"),
                        status: text("status"),
                        at: Instant::now(),
                    });
                    respond(&mut conn, "200 OK", r#"{"cancel":false}"#);
                }
                ["api", "agent", "v1", "runs", run, "finish"] => {
                    shared.finishes.lock().unwrap().push(Finish {
                        run: run.parse().unwrap_or(0),
                        outcome: text("outcome"),
                        code: text("code"),
                        detail: text("detail"),
                        at: Instant::now(),
                    });
                    respond(&mut conn, "200 OK", "{}");
                }
                _ => respond(&mut conn, "404 Not Found", "{}"),
            }
        }
        _ => respond(&mut conn, "404 Not Found", "{}"),
    }
}

// --- The agent under test ----------------------------------------------------

fn volume(name: &str) -> String {
    format!("ww-test-su-{name}-config")
}

/// Starts the agent `ww-test-su-<name>` from `image` against the fake Wren;
/// its id. `extra` goes before the image (more `-e`, `--label`, …).
fn start_agent(name: &str, wren: &FakeWren, image: &str, extra: &[&str]) -> String {
    let container = format!("ww-test-su-{name}");
    // Made here with the label (`-v` would make it without one), so that the
    // volume can be told from anyone else's by its label.
    docker(&[
        "volume",
        "create",
        "--label",
        "owner=wise-wren",
        &volume(name),
    ]);
    let config = format!("{}:/config", volume(name));
    let server = format!("WW_SERVER_URL={}", wren.url);
    let token = format!("WW_AGENT_TOKEN=wwa_{}", "t".repeat(43));
    let mut args = vec![
        "run",
        "-d",
        "--name",
        &container,
        "--label",
        "owner=wise-wren",
        "--add-host",
        "host.docker.internal:host-gateway",
        "-v",
        "/var/run/docker.sock:/var/run/docker.sock",
        "-v",
        &config,
        "-e",
        &server,
        "-e",
        &token,
        "-e",
        "WW_AGENT_SELF_UPDATE=1",
        "-e",
        "WW_SU_TEST_TIME_PCT=10",
    ];
    // Docker refuses a restart policy together with `--rm`.
    if !extra.contains(&"--rm") {
        args.extend(["--restart", "unless-stopped"]);
    }
    args.extend(extra);
    args.push(image);
    docker(&args)
}

/// An `agent_update` job as Wren sends it.
fn job(run: i64, to_version: &str, to_digest: &str) -> Value {
    json!({
        "run_id": run,
        "kind": "agent_update",
        "target": {"kind": "agent", "name": "ww-test"},
        "from_key": "",
        "to_key": to_version,
        "to_version": to_version,
        "to_digest": to_digest,
    })
}

/// The test's state file, read through a throwaway container on its volume.
fn state(name: &str) -> StateFile {
    let reader = format!("ww-test-su-{name}-read");
    let mount = format!("{}:/c:ro", volume(name));
    let text = docker(&[
        "run",
        "--rm",
        "--name",
        &reader,
        "--label",
        "owner=wise-wren",
        "-v",
        &mount,
        ALPINE,
        "cat",
        "/c/selfupdate.json",
    ]);
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("selfupdate.json: {e}: {text}"))
}

/// The ids of the containers that mount this volume.
fn users_of(volume: &str) -> Vec<String> {
    try_docker(&["ps", "-aq", "--filter", &format!("volume={volume}")])
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Removes what the test `name` made: every container on its config volume
/// (the agent, the renamed old and the new one, the helper of any run, the
/// reader: the helpers carry no owner label, but they mount this volume and
/// nobody else's container does), the containers by name, the volume and the
/// rollback tags. A volume that can't be removed is reported, not forgotten.
fn cleanup(name: &str) {
    let volume = volume(name);
    for _ in 0..2 {
        let mut doomed = users_of(&volume);
        doomed.push(format!("ww-test-su-{name}"));
        doomed.push(format!("ww-test-su-{name}-read"));
        let old = try_docker(&[
            "ps",
            "-a",
            "--filter",
            &format!("name=ww-test-su-{name}-wren-old-"),
            "--format",
            "{{.Names}}",
        ])
        .unwrap_or_default();
        doomed.extend(old.split_whitespace().map(str::to_string));
        for each in doomed {
            let _ = try_docker(&["rm", "-f", &each]);
        }
        // A container that appeared meanwhile (a restarting helper) gets one
        // more round.
        if users_of(&volume).is_empty() {
            break;
        }
    }
    if try_docker(&["volume", "inspect", &volume]).is_ok()
        && let Err(e) = try_docker(&["volume", "rm", &volume])
    {
        let msg = format!("cleanup of {name} left the volume {volume}: {e}");
        if std::thread::panicking() {
            eprintln!("{msg}");
        } else {
            panic!("{msg}");
        }
    }
    let tags = try_docker(&[
        "image",
        "ls",
        "--format",
        "{{.Repository}}:{{.Tag}}",
        "--filter",
        &format!("reference=wren-rollback/agent-ww-test-su-{name}:*"),
    ])
    .unwrap_or_default();
    for tag in tags.split_whitespace() {
        let _ = try_docker(&["rmi", tag]);
    }
}

/// Cleans up before the test (what an earlier, killed run left) and after it;
/// when the test failed, prints the logs of its containers first.
/// It also holds the lock that keeps the scenarios one at a time: they share
/// the registry, the `:latest` tag and the Docker host's names.
struct Scope(&'static str, #[allow(dead_code)] MutexGuard<'static, ()>);

impl Scope {
    fn new(name: &'static str) -> Scope {
        static SERIAL: Mutex<()> = Mutex::new(());
        let lock = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        cleanup(name);
        Scope(name, lock)
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let names = try_docker(&[
                "ps",
                "-a",
                "--filter",
                &format!("volume={}", volume(self.0)),
                "--format",
                "{{.Names}}",
            ])
            .unwrap_or_default();
            for each in names.split_whitespace() {
                let logs = try_docker(&["logs", "--tail", "60", each]);
                eprintln!("--- logs of {each}\n{}", logs.unwrap_or_else(|e| e));
            }
        }
        cleanup(self.0);
    }
}

// --- Scenarios ---------------------------------------------------------------

#[tokio::test]
#[ignore = "needs Docker: WW_DOCKER_TESTS=1"]
async fn a_good_update_keeps_name_mounts_env_and_labels() {
    if !on() {
        return;
    }
    let _scope = Scope::new("good");
    ensure_registry();
    build("0.90.0", "0.90.0");
    publish("0.90.0");
    let wren = FakeWren::start(3).await;
    let old_id = start_agent(
        "good",
        &wren,
        &format!("{REPO}:latest"),
        &["-e", "TZ=Europe/Oslo", "--label", "x.test=1"],
    );
    let before = inspect("ww-test-su-good");
    let digest = build("0.90.1", "0.90.1");
    publish("0.90.1");
    wren.queue_job(job(100, "0.90.1", &digest));
    assert_eq!(
        wren.finish_of(100, Duration::from_secs(240)).await,
        ("succeeded".into(), "selfupdate.done".into())
    );
    let after = inspect("ww-test-su-good");
    assert_ne!(after["Id"], old_id);
    for p in [
        "/Config/Env",
        "/HostConfig/Binds",
        "/HostConfig/Mounts",
        "/HostConfig/RestartPolicy",
        "/Config/Labels/x.test",
    ] {
        assert_eq!(after.pointer(p), before.pointer(p), "{p}");
    }
    // The replacement is the new release, and it is up and talking to Wren
    // (`:latest` points at 0.90.1 locally before the update, so the image
    // name proves nothing; the image's version label does).
    assert_eq!(
        after
            .pointer("/Config/Labels/org.opencontainers.image.version")
            .and_then(Value::as_str),
        Some("0.90.1")
    );
    assert!(running("ww-test-su-good"));
    assert!(wren.beats_from("0.90.1") > 0, "the new agent never beat");
    assert_eq!(containers_named("ww-test-su-good-wren-old-"), "");
    wait_gone("wren-agent-selfupdate-100", Duration::from_secs(30)).await;
    assert!(
        docker(&["image", "ls", "--format", "{{.Repository}}:{{.Tag}}"])
            .contains("wren-rollback/agent-ww-test-su-good:100-ww-test-su-good")
    );
    assert_eq!(
        wren.steps_of(100),
        [
            ("preflight".into(), "ok".into()),
            ("update".into(), "ok".into()),
            ("health".into(), "ok".into())
        ]
    );
    let st = state("good");
    assert_eq!(st.phase, Phase::Done);
    assert!(st.reported);
}

#[tokio::test]
#[ignore = "needs Docker: WW_DOCKER_TESTS=1"]
async fn a_new_agent_wren_refuses_is_rolled_back() {
    if !on() {
        return;
    }
    let _scope = Scope::new("refused");
    ensure_registry();
    build("0.90.0", "0.90.0");
    publish("0.90.0");
    let wren = FakeWren::start(3).await;
    wren.set_mode(Mode::Refuse {
        agent_version: "0.90.1".into(),
    });
    let old_id = start_agent("refused", &wren, &format!("{REPO}:latest"), &[]);
    let digest = build("0.90.1", "0.90.1");
    publish("0.90.1");
    wren.queue_job(job(110, "0.90.1", &digest));
    assert_eq!(
        wren.finish_of(110, Duration::from_secs(240)).await,
        ("rolled_back".into(), "selfupdate.rolled_back".into())
    );
    assert!(wren.beats_from("0.90.1") > 0, "the new agent never beat");
    let after = inspect("ww-test-su-refused");
    assert_eq!(after["Id"], old_id);
    assert!(running("ww-test-su-refused"));
    assert_eq!(
        wren.steps_of(110),
        [
            ("preflight".into(), "ok".into()),
            ("update".into(), "failed".into()),
            ("rollback".into(), "ok".into())
        ]
    );
    let st = state("refused");
    assert_eq!(st.phase, Phase::RolledBack);
    assert!(st.reason.contains("didn't reach Wren"), "{}", st.reason);
}

#[tokio::test]
#[ignore = "needs Docker: WW_DOCKER_TESTS=1"]
async fn a_new_agent_that_exits_at_once_is_rolled_back_fast() {
    if !on() {
        return;
    }
    let _scope = Scope::new("exits");
    ensure_registry();
    build("0.90.0", "0.90.0");
    publish("0.90.0");
    let wren = FakeWren::start(3).await;
    let old_id = start_agent(
        "exits",
        &wren,
        &format!("{REPO}:latest"),
        &["-e", "WW_SU_FAULT=exit2"],
    );
    let digest = build("0.90.1", "0.90.1");
    publish("0.90.1");
    wren.queue_job(job(120, "0.90.1", &digest));
    assert_eq!(
        wren.finish_of(120, Duration::from_secs(240)).await,
        ("rolled_back".into(), "selfupdate.rolled_back".into())
    );
    // Before the 30 s confirm deadline could have decided anything.
    let took = wren
        .finish_at(120)
        .unwrap()
        .duration_since(wren.step_at(120, "update").expect("no update step"));
    eprintln!("rolled back {took:?} after the update step");
    assert!(took < Duration::from_secs(25), "took {took:?}");
    let after = inspect("ww-test-su-exits");
    assert_eq!(after["Id"], old_id);
    assert!(running("ww-test-su-exits"));
    assert_eq!(
        wren.steps_of(120),
        [
            ("preflight".into(), "ok".into()),
            ("update".into(), "failed".into()),
            ("rollback".into(), "ok".into())
        ]
    );
    let st = state("exits");
    assert_eq!(st.phase, Phase::RolledBack);
    assert!(st.reason.contains("the new agent stopped"), "{}", st.reason);
}
