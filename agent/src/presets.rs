//! Script-stack presets compiled into the agent. A preset is a stack whose
//! commands are constants in this file: Wren (managed settings,
//! [`crate::managed`]) can only pick a preset by name and give it a working
//! folder, a Compose project and a name, all of which the agent validates.
//! It can never send a command. Stacks with commands of their own stay in
//! `agent.toml`.
//!
//! One preset so far: `fluxer`, the vendor's `install.sh` (docs/agent.md,
//! "Script stacks"). Its version detection asks Fluxer's release document
//! for the newest version and reads the installed one from the state file
//! that Fluxer's `rexctl` keeps in the stack's folder.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::config::{ScriptStack, valid_name};

/// The Fluxer preset.
pub const FLUXER: &str = "fluxer";
/// Every preset this agent knows.
pub const NAMES: [&str; 1] = [FLUXER];

/// Where Fluxer publishes its newest release (it redirects, see
/// [`redirect_ok`]).
pub const FLUXER_RELEASE_URL: &str = "https://fluxer.dev/rexctl/release.json";
/// What `rexctl` keeps in the instance's folder.
pub const FLUXER_STATE: &str = ".rexctl/state.json";
/// Longest time the newest-version lookup may take.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(20);
/// The most the agent reads of the release document.
const MAX_RELEASE_BYTES: usize = 256 << 10;
/// The most the agent reads of the state file.
const MAX_STATE_BYTES: u64 = 1 << 20;
/// Longest version kept.
const MAX_VERSION: usize = 64;

/// Asked before anything stops: rexctl, which the installer downloads, is
/// released.
const FLUXER_CHECK: &str = "curl -fsSL -o /dev/null https://fluxer.dev/rexctl/release.json";
/// Downloads and verifies the installer into a temporary folder inside the
/// stack's folder (removed on exit, however the script ends) and leaves
/// `$w` as the stack's folder.
const FLUXER_FETCH: &str = r#"w=$PWD; d=$(mktemp -d "$w/.wren-install.XXXXXX") || exit 1; trap 'rm -rf "$d"' EXIT; cd "$d" && curl -fsSLO https://fluxer.dev/install.sh && curl -fsSLO https://fluxer.dev/install.sh.sha256 && sha256sum -c install.sh.sha256"#;
const FLUXER_RUN: &str = "--allow-root --non-interactive --dir \"$w\"";

/// What the docs showed before presets: the installer downloaded into the
/// stack's folder itself. Stacks with these commands in `agent.toml` count
/// as the preset ([`identify`]).
const LEGACY_FETCH: &str = "curl -fsSLO https://fluxer.dev/install.sh && curl -fsSLO https://fluxer.dev/install.sh.sha256 && sha256sum -c install.sh.sha256";
const LEGACY_RUN: &str = "--allow-root --non-interactive --dir \"$PWD\"";

/// A preset's stack in `dir`.
pub fn render(
    preset: &str,
    name: &str,
    project: &str,
    dir: &Path,
    min_interval: Duration,
) -> Option<ScriptStack> {
    match preset {
        FLUXER => Some(ScriptStack {
            name: name.to_string(),
            compose_project: project.to_string(),
            working_dir: dir.to_path_buf(),
            // The installer stops, backs up and verifies itself.
            handles_own_backup: true,
            env: BTreeMap::from([("COMPOSE_PROJECT_NAME".to_string(), project.to_string())]),
            timeout: Duration::from_secs(20 * 60),
            check: Some(FLUXER_CHECK.to_string()),
            update: format!("{FLUXER_FETCH} && sh install.sh --update {FLUXER_RUN}"),
            rollback: Some(format!(
                "{FLUXER_FETCH} && sh install.sh --rollback {FLUXER_RUN}"
            )),
            // Version detection is the preset's own ([`version`]).
            version: None,
            min_interval,
            preset: Some(FLUXER),
        }),
        _ => None,
    }
}

/// The text of a command with its whitespace (and line continuations)
/// folded, to compare commands written differently.
fn flat(command: &str) -> String {
    command
        .replace("\\\r\n", " ")
        .replace("\\\n", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Which preset a stack from `agent.toml` is exactly (its commands, env,
/// timeout and own-backup flag; the name, folder and interval are free).
/// Such a stack is imported into Wren's settings as the preset; any other
/// stack stays "custom" and only `agent.toml` can change it.
pub fn identify(stack: &ScriptStack) -> Option<&'static str> {
    // A stack the managed settings couldn't take (a Compose project name
    // they'd refuse) stays custom.
    if stack.version.is_some() || stack.preset.is_some() || !valid_name(&stack.compose_project) {
        return None;
    }
    let env = BTreeMap::from([(
        "COMPOSE_PROJECT_NAME".to_string(),
        stack.compose_project.clone(),
    )]);
    let same = |got: &str, want: &[String]| want.iter().any(|w| flat(w) == flat(got));
    let fluxer = render(
        FLUXER,
        &stack.name,
        &stack.compose_project,
        &stack.working_dir,
        stack.min_interval,
    )?;
    let check_ok = stack
        .check
        .as_deref()
        .is_some_and(|c| same(c, &[FLUXER_CHECK.to_string()]));
    let update_ok = same(
        &stack.update,
        &[
            fluxer.update.clone(),
            format!("{LEGACY_FETCH} && sh install.sh --update {LEGACY_RUN}"),
        ],
    );
    let rollback_ok = stack.rollback.as_deref().is_some_and(|r| {
        same(
            r,
            &[
                fluxer.rollback.clone().unwrap_or_default(),
                format!("{LEGACY_FETCH} && sh install.sh --rollback {LEGACY_RUN}"),
                format!("sh install.sh --rollback {LEGACY_RUN}"),
            ],
        )
    });
    (stack.handles_own_backup
        && stack.timeout == fluxer.timeout
        && stack.env == env
        && check_ok
        && update_ok
        && rollback_ok)
        .then_some(FLUXER)
}

// ---------------------------------------------------------------------------
// Version detection
// ---------------------------------------------------------------------------

/// What a preset knows about its versions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Found {
    pub installed: String,
    pub latest: String,
    pub error: String,
}

/// Keeps a version string that looks like one.
fn clean(raw: &str) -> Option<String> {
    let v = raw.trim().rsplit('@').next().unwrap_or_default().trim();
    (!v.is_empty()
        && v.len() <= MAX_VERSION
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b)))
    .then(|| v.to_string())
}

/// The newest version in Fluxer's release document (`{"version": "…"}`).
pub fn fluxer_latest(body: &[u8]) -> Result<String, String> {
    let doc: Value = serde_json::from_slice(body)
        .map_err(|e| format!("Fluxer's release document isn't JSON ({e})"))?;
    doc.get("version")
        .and_then(Value::as_str)
        .and_then(clean)
        .ok_or_else(|| "Fluxer's release document has no version".to_string())
}

/// The installed version in `rexctl`'s state file. `rexctl` records the
/// release it installed; the field is read as a string or as an object with
/// a `version`.
pub fn fluxer_installed(state: &str) -> Option<String> {
    let doc: Value = serde_json::from_str(state).ok()?;
    [
        "/release",
        "/release/version",
        "/version",
        "/current/version",
    ]
    .iter()
    .filter_map(|p| doc.pointer(p))
    .filter_map(Value::as_str)
    .find_map(clean)
}

/// Whether a redirect of the release lookup may be followed: HTTPS to
/// Fluxer's own hosts only (the document moved to `docs.fluxer.app`).
pub fn redirect_ok(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    url.scheme() == "https"
        && (host == "fluxer.dev"
            || host.ends_with(".fluxer.dev")
            || host == "fluxer.app"
            || host.ends_with(".fluxer.app"))
}

async fn fetch_latest(url: &str) -> Result<String, String> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("WiseWrenAgent/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(LOOKUP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 3 || !redirect_ok(attempt.url()) {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|e| e.to_string())?;
    let res = http
        .get(url)
        .send()
        .await
        .map_err(|e| format!("can't reach Fluxer's release document: {e}"))?;
    if !res.status().is_success() {
        return Err(format!(
            "Fluxer's release document answered {}",
            res.status()
        ));
    }
    let (body, more) = crate::heartbeat::read_head(res, MAX_RELEASE_BYTES)
        .await
        .map_err(|e| format!("can't read Fluxer's release document: {e}"))?;
    if more {
        return Err("Fluxer's release document is larger than 256 KiB".into());
    }
    fluxer_latest(&body)
}

/// The versions of a preset stack whose folder is `dir` (as the agent sees
/// it; `reachable`: the folder is mounted). `release_url` is where the
/// newest one is asked for ([`FLUXER_RELEASE_URL`]; tests use their own).
pub async fn version(preset: &str, dir: &Path, reachable: bool, release_url: &str) -> Found {
    let mut out = Found::default();
    if preset != FLUXER {
        return out;
    }
    if reachable {
        let path = dir.join(FLUXER_STATE);
        let state = std::fs::metadata(&path)
            .ok()
            .filter(|m| m.len() <= MAX_STATE_BYTES)
            .and_then(|_| std::fs::read_to_string(&path).ok());
        match state {
            Some(text) => match fluxer_installed(&text) {
                Some(v) => out.installed = v,
                None => out.error = format!("no version found in {FLUXER_STATE}"),
            },
            None => out.error = format!("{FLUXER_STATE} not found in the stack's folder"),
        }
    }
    match fetch_latest(release_url).await {
        Ok(v) => out.latest = v,
        // The installed version's problem stays the first one told.
        Err(e) if out.error.is_empty() => out.error = e,
        Err(_) => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// What `https://fluxer.dev/rexctl/release.json` answered on 2026-10-27.
    const RELEASE: &str = include_str!("../tests/fixtures/fluxer-release.json");

    fn fluxer() -> ScriptStack {
        render(
            FLUXER,
            "fluxer",
            "fluxer",
            &PathBuf::from("/mnt/cache/appdata/fluxer"),
            Duration::from_secs(3600),
        )
        .unwrap()
    }

    #[test]
    fn the_fluxer_preset_renders_constant_commands() {
        let s = fluxer();
        assert_eq!(s.preset, Some(FLUXER));
        assert!(s.handles_own_backup);
        assert_eq!(s.timeout, Duration::from_secs(1200));
        assert_eq!(s.env["COMPOSE_PROJECT_NAME"], "fluxer");
        assert_eq!(s.check.as_deref(), Some(FLUXER_CHECK));
        // Download, verify, run: in a temporary folder inside the stack's
        // folder that goes away however the script ends.
        for command in [&s.update, s.rollback.as_ref().unwrap()] {
            assert!(command.starts_with("w=$PWD; d=$(mktemp -d \"$w/.wren-install.XXXXXX\")"));
            assert!(command.contains("trap 'rm -rf \"$d\"' EXIT"));
            assert!(command.contains("sha256sum -c install.sh.sha256 && sh install.sh --"));
            assert!(command.ends_with("--allow-root --non-interactive --dir \"$w\""));
        }
        assert!(s.update.contains("install.sh --update"));
        assert!(
            s.rollback
                .as_ref()
                .unwrap()
                .contains("install.sh --rollback")
        );
        // The parameters never reach the commands: the project is only in env.
        let other = render(
            FLUXER,
            "chat",
            "chat",
            Path::new("/srv/chat"),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(other.update, s.update);
        assert_eq!(other.env["COMPOSE_PROJECT_NAME"], "chat");
        assert!(render("nope", "x", "x", Path::new("/srv/x"), Duration::ZERO).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn the_commands_are_valid_shell() {
        let s = fluxer();
        for command in [&s.update, s.rollback.as_ref().unwrap()] {
            assert!(!command.contains('\n'));
            let ok = std::process::Command::new("sh")
                .args(["-n", "-c", command])
                .status()
                .unwrap();
            assert!(ok.success(), "{command}");
        }
    }

    /// Runs the preset's update and rollback in a folder with a fake `curl`
    /// and `sha256sum` (`verify`: the checksum's exit code) and a fake
    /// installer that records its arguments.
    #[cfg(unix)]
    fn run_with_fakes(name: &str, verify: i32, rollback: bool) -> (bool, String, Vec<String>) {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("wren-preset-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (bin, stack_dir) = (dir.join("bin"), dir.join("stack"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&stack_dir).unwrap();
        let script = |file: &str, body: &str| {
            let p = bin.join(file);
            std::fs::write(
                &p,
                format!(
                    "#!/bin/sh
{body}
"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        // curl -fsSLO <url>: saves the file under the URL's last part.
        script(
            "curl",
            r#"for a; do u=$a; done; case ${u##*/} in install.sh) printf 'echo "$@" > "$OUT"
' > install.sh ;; *) echo "abc  install.sh" > install.sh.sha256 ;; esac"#,
        );
        script("sha256sum", &format!("exit {verify}"));
        let stack = fluxer();
        let command = if rollback {
            stack.rollback.clone().unwrap()
        } else {
            stack.update.clone()
        };
        let out = dir.join("out");
        let status = std::process::Command::new("sh")
            .args(["-c", &command])
            .current_dir(&stack_dir)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("OUT", &out)
            .status()
            .unwrap();
        let args = std::fs::read_to_string(&out).unwrap_or_default();
        let left: Vec<String> = std::fs::read_dir(&stack_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        let shown = args
            .trim()
            .replace(&stack_dir.to_string_lossy().to_string(), "<dir>");
        let _ = std::fs::remove_dir_all(&dir);
        (status.success(), shown, left)
    }

    #[cfg(unix)]
    #[test]
    fn the_installer_is_downloaded_into_a_temporary_folder_and_removed() {
        let (ok, args, left) = run_with_fakes("update", 0, false);
        assert!(ok);
        assert_eq!(args, "--update --allow-root --non-interactive --dir <dir>");
        assert!(
            left.is_empty(),
            "nothing is left in the stack's folder: {left:?}"
        );
        let (ok, args, left) = run_with_fakes("rollback", 0, true);
        assert!(ok);
        assert_eq!(
            args,
            "--rollback --allow-root --non-interactive --dir <dir>"
        );
        assert!(left.is_empty(), "{left:?}");
        // A checksum that doesn't match: the installer never runs, nothing stays.
        let (ok, args, left) = run_with_fakes("badsum", 1, false);
        assert!(!ok);
        assert_eq!(args, "");
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn stacks_from_the_docs_count_as_the_preset() {
        // The agent.toml of the docs before presets, as TOML would give it.
        let mut s = fluxer();
        s.update = "curl -fsSLO https://fluxer.dev/install.sh && curl -fsSLO https://fluxer.dev/install.sh.sha256   && sha256sum -c install.sh.sha256   && sh install.sh --update --allow-root --non-interactive --dir \"$PWD\"\n".into();
        s.rollback =
            Some("sh install.sh --rollback --allow-root --non-interactive --dir \"$PWD\"".into());
        s.preset = None;
        assert_eq!(identify(&s), Some(FLUXER));
        // The preset's own commands too.
        let mut own = fluxer();
        own.preset = None;
        assert_eq!(identify(&own), Some(FLUXER));
        // Anything else is custom.
        let mut edited = own.clone();
        edited.update.push_str(" && echo hi");
        assert_eq!(identify(&edited), None);
        let mut slow = own.clone();
        slow.timeout = Duration::from_secs(60);
        assert_eq!(identify(&slow), None);
        let mut noenv = own.clone();
        noenv.env.clear();
        assert_eq!(identify(&noenv), None);
        let mut nocheck = own.clone();
        nocheck.check = None;
        assert_eq!(identify(&nocheck), None);
        // A Compose project the managed settings would refuse keeps the stack custom.
        let mut odd = own.clone();
        odd.compose_project = "My Project".into();
        odd.env = BTreeMap::from([("COMPOSE_PROJECT_NAME".to_string(), "My Project".to_string())]);
        assert_eq!(identify(&odd), None);
        // A stack that is already a preset isn't "identified" again.
        assert_eq!(identify(&fluxer()), None);
    }

    #[test]
    fn the_newest_version_is_read_from_the_recorded_release_document() {
        assert_eq!(
            fluxer_latest(RELEASE.as_bytes()).unwrap(),
            "2026.1026.223737"
        );
        assert!(
            fluxer_latest(b"not json")
                .unwrap_err()
                .contains("isn't JSON")
        );
        assert!(
            fluxer_latest(br#"{"min_from":""}"#)
                .unwrap_err()
                .contains("no version")
        );
        // Nothing but a version gets through.
        assert!(fluxer_latest(br#"{"version":"1 && reboot"}"#).is_err());
        assert!(fluxer_latest(br#"{"version":""}"#).is_err());
    }

    #[test]
    fn the_installed_version_is_read_from_rexctls_state() {
        for (state, want) in [
            (
                r#"{"release":"2026.1026.223737"}"#,
                Some("2026.1026.223737"),
            ),
            (
                r#"{"release":"fluxer-release@2026.1020.1"}"#,
                Some("2026.1020.1"),
            ),
            (
                r#"{"release":{"version":"2026.1019.5"}}"#,
                Some("2026.1019.5"),
            ),
            (r#"{"version":"v1.2.3"}"#, Some("v1.2.3")),
            (r#"{"state_schema":2}"#, None),
            ("{", None),
        ] {
            assert_eq!(fluxer_installed(state).as_deref(), want, "{state}");
        }
    }

    #[test]
    fn redirects_stay_with_fluxer() {
        for ok in [
            "https://docs.fluxer.app/rexctl/release.json",
            "https://fluxer.dev/x",
        ] {
            assert!(redirect_ok(&reqwest::Url::parse(ok).unwrap()), "{ok}");
        }
        for bad in [
            "http://fluxer.dev/x",
            "https://fluxer.dev.evil.example/x",
            "https://evilfluxer.app/x",
            "https://10.0.0.1/x",
        ] {
            assert!(!redirect_ok(&reqwest::Url::parse(bad).unwrap()), "{bad}");
        }
    }

    /// Answers every connection with `body` as JSON.
    async fn serve(status: &'static str, body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        format!("http://{addr}/rexctl/release.json")
    }

    #[tokio::test]
    async fn versions_come_from_the_state_file_and_the_release_document() {
        let dir = std::env::temp_dir().join(format!("wren-preset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".rexctl")).unwrap();
        std::fs::write(dir.join(FLUXER_STATE), r#"{"release":"2026.1020.9"}"#).unwrap();
        let url = serve("200 OK", RELEASE).await;
        let found = version(FLUXER, &dir, true, &url).await;
        assert_eq!(
            found,
            Found {
                installed: "2026.1020.9".into(),
                latest: "2026.1026.223737".into(),
                error: String::new()
            }
        );
        // The folder isn't mounted: the newest one is still known.
        let found = version(FLUXER, &dir, false, &url).await;
        assert_eq!(found.latest, "2026.1026.223737");
        assert_eq!(found.installed, "");
        // No state file: the reason is told, the newest version still shown.
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let found = version(FLUXER, &empty, true, &url).await;
        assert!(found.error.contains("not found"), "{found:?}");
        assert_eq!(found.latest, "2026.1026.223737");
        // Fluxer down: the error says so.
        let down = serve("503 Service Unavailable", "{}").await;
        let found = version(FLUXER, &dir, true, &down).await;
        assert!(found.error.contains("answered 503"), "{found:?}");
        assert_eq!(found.installed, "2026.1020.9");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
