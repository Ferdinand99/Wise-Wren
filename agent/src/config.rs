//! `agent.toml`: where Wren is, the agent's token, the folders Wren may
//! touch and the scripts it may run by name (docs/design.md §8,
//! docs/spike-findings.md). Everything Wren will ever be allowed to do on
//! this server is listed here, on the server, never sent by Wren.
//!
//! ```toml
//! server = "https://wren.example.com"   # or WW_SERVER_URL
//! token = "wwa_…"                        # or WW_AGENT_TOKEN
//! roots = ["/mnt/user/appdata", "/mnt/cache/appdata"]
//!
//! [[script_stack]]
//! name = "fluxer"
//! compose_project = "fluxer"
//! working_dir = "/mnt/cache/appdata/fluxer"
//! handles_own_backup = true
//! env = { COMPOSE_PROJECT_NAME = "fluxer" }
//! timeout = "20m"
//!
//! [script_stack.update]
//! command = "sh install.sh --update --allow-root --non-interactive --dir \"$PWD\""
//!
//! [script_stack.rollback]
//! command = "sh install.sh --rollback --allow-root --non-interactive --dir \"$PWD\""
//!
//! # Optional: where the stack's versions come from. One of github, url
//! # (+ regex or json_pointer) or latest_command; the agent runs the
//! # commands itself, in working_dir, and only prints what they print.
//! [script_stack.version]
//! github = "owner/repo"
//! installed_command = "cat VERSION"
//! ```
//!
//! `scan_interval` (default `"30m"`) is how often the agent sends its
//! inventory on its own.
//!
//! Every problem names the field it is about; the agent refuses to start
//! with any of them.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// Where the agent looks for its config without `--config`/`WW_AGENT_CONFIG`.
pub const DEFAULT_PATH: &str = "/config/agent.toml";
/// Prefix of agent tokens.
pub const TOKEN_PREFIX: &str = "wwa_";
/// Characters after the prefix (the server's tokens: 43 base62).
pub const TOKEN_BODY_LEN: usize = 43;
/// Longest script timeout.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
/// Where Docker listens unless `docker_socket` says otherwise.
pub const DEFAULT_SOCKET: &str = "/var/run/docker.sock";

/// What's wrong with the configuration: one line per problem.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigError(pub Vec<String>);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("\n"))
    }
}

impl std::error::Error for ConfigError {}

/// A checked configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Wren's address, without a trailing slash.
    pub server: String,
    pub token: String,
    /// Folders Wren may touch, mounted at the same path in the agent.
    pub roots: Vec<PathBuf>,
    pub script_stacks: Vec<ScriptStack>,
    pub docker_socket: String,
    /// How often the agent sends its inventory on its own.
    pub scan_interval: Duration,
}

/// Where a script stack's versions come from: one source of the newest
/// version (Wren looks up `github` and `url` itself; the agent runs
/// `latest_command`) and, optionally, a command printing the installed one.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VersionConfig {
    pub github: String,
    pub url: String,
    pub regex: String,
    pub json_pointer: String,
    pub latest_command: Option<String>,
    pub installed_command: Option<String>,
    pub timeout: Duration,
}

/// A Compose project updated by scripts of its own (like Fluxer's installer).
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptStack {
    pub name: String,
    pub compose_project: String,
    pub working_dir: PathBuf,
    /// The script stops, backs up and verifies itself: Wren skips its own
    /// stop and snapshot.
    pub handles_own_backup: bool,
    pub env: BTreeMap<String, String>,
    pub timeout: Duration,
    pub update: String,
    pub rollback: Option<String>,
    pub version: Option<VersionConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    server: Option<String>,
    token: Option<String>,
    #[serde(default)]
    roots: Vec<String>,
    #[serde(default)]
    script_stack: Vec<RawStack>,
    docker_socket: Option<String>,
    scan_interval: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVersion {
    github: Option<String>,
    url: Option<String>,
    regex: Option<String>,
    json_pointer: Option<String>,
    latest_command: Option<String>,
    installed_command: Option<String>,
    timeout: Option<String>,
}

/// Default and longest time a version command may take.
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_VERSION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Default time between inventories; shortest one.
pub const SCAN_INTERVAL: Duration = Duration::from_secs(30 * 60);
pub const MIN_SCAN_INTERVAL: Duration = Duration::from_secs(60);

fn valid_github(repo: &str) -> bool {
    let parts: Vec<&str> = repo.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && *p != "."
                && *p != ".."
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
}

/// Checks `[script_stack.version]`; problems are prefixed with `at`.
fn check_version(raw: RawVersion, at: &str, problems: &mut Vec<String>) -> Option<VersionConfig> {
    let set = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let before = problems.len();
    let v = VersionConfig {
        github: set(raw.github).unwrap_or_default(),
        url: set(raw.url).unwrap_or_default(),
        regex: set(raw.regex).unwrap_or_default(),
        json_pointer: set(raw.json_pointer).unwrap_or_default(),
        latest_command: set(raw.latest_command),
        installed_command: set(raw.installed_command),
        timeout: match raw.timeout.as_deref() {
            None => VERSION_TIMEOUT,
            Some(t) => match parse_duration(t) {
                Some(d) if d <= MAX_VERSION_TIMEOUT => d,
                _ => {
                    problems.push(format!(
                        "{at}: version.timeout \"{t}\" must be like \"30s\" or \"2m\" (at most 10m)"
                    ));
                    Duration::ZERO
                }
            },
        },
    };
    let sources = [
        !v.github.is_empty(),
        !v.url.is_empty(),
        v.latest_command.is_some(),
    ]
    .iter()
    .filter(|s| **s)
    .count();
    if sources != 1 {
        problems.push(format!(
            "{at}: version needs exactly one of github, url or latest_command"
        ));
    }
    if !v.github.is_empty() && !valid_github(&v.github) {
        problems.push(format!("{at}: version.github must be owner/repo"));
    }
    if !v.url.is_empty() {
        if !(v.url.starts_with("https://") || v.url.starts_with("http://")) {
            problems.push(format!(
                "{at}: version.url must start with https:// or http://"
            ));
        }
        if v.regex.is_empty() == v.json_pointer.is_empty() {
            problems.push(format!(
                "{at}: version.url needs exactly one of regex or json_pointer"
            ));
        }
        if !v.json_pointer.is_empty() && !v.json_pointer.starts_with('/') {
            problems.push(format!(
                "{at}: version.json_pointer must start with / (like \"/version\")"
            ));
        }
    } else if !v.regex.is_empty() || !v.json_pointer.is_empty() {
        problems.push(format!(
            "{at}: version.regex and json_pointer go with version.url"
        ));
    }
    (problems.len() == before).then_some(v)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStack {
    name: Option<String>,
    compose_project: Option<String>,
    working_dir: Option<String>,
    #[serde(default)]
    handles_own_backup: bool,
    #[serde(default)]
    env: BTreeMap<String, String>,
    timeout: Option<String>,
    update: Option<RawScript>,
    rollback: Option<RawScript>,
    version: Option<RawVersion>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScript {
    command: Option<String>,
}

/// `"90"`, `"90s"`, `"20m"` or `"1h"`: more than zero and at most a day.
pub fn parse_duration(raw: &str) -> Option<Duration> {
    let raw = raw.trim();
    let (number, unit) = match raw.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => raw.split_at(i),
        None => (raw, "s"),
    };
    let n: u64 = number.parse().ok()?;
    let secs = match unit {
        "s" => n,
        "m" => n.checked_mul(60)?,
        "h" => n.checked_mul(3600)?,
        _ => return None,
    };
    let d = Duration::from_secs(secs);
    (secs > 0 && d <= MAX_TIMEOUT).then_some(d)
}

/// An absolute path with no `.` or `..` parts, as a `PathBuf`.
fn absolute(raw: &str) -> Result<PathBuf, &'static str> {
    if !raw.starts_with('/') {
        return Err("must be an absolute path (starting with /)");
    }
    let path = PathBuf::from(raw);
    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err("must not contain . or .. parts");
    }
    Ok(path)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn valid_env_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with(|c: char| c.is_ascii_digit())
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Whether `token` looks like an agent token.
pub fn is_agent_token(token: &str) -> bool {
    token.strip_prefix(TOKEN_PREFIX).is_some_and(|body| {
        body.len() == TOKEN_BODY_LEN && body.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

fn check_server(raw: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(raw.trim())
        .map_err(|e| format!("server: not a URL like https://wren.example.com ({e})"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("server: use an http:// or https:// address".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("server: leave out queries and fragments".into());
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

impl Config {
    /// Checks the file's text (none: no file) with `WW_SERVER_URL` and
    /// `WW_AGENT_TOKEN` from `env` taking precedence.
    pub fn from_parts(
        text: Option<&str>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Config, ConfigError> {
        let raw: RawConfig = match text {
            Some(text) => toml::from_str(text)
                .map_err(|e| ConfigError(vec![e.to_string().trim_end().to_string()]))?,
            None => RawConfig::default(),
        };
        let mut problems = Vec::new();
        let set = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

        let server = match set(env("WW_SERVER_URL")).or(set(raw.server)) {
            Some(s) => check_server(&s).unwrap_or_else(|e| {
                problems.push(e);
                String::new()
            }),
            None => {
                problems.push("server: missing (set it in agent.toml or WW_SERVER_URL)".into());
                String::new()
            }
        };
        let token = match set(env("WW_AGENT_TOKEN")).or(set(raw.token)) {
            Some(t) if is_agent_token(&t) => t,
            Some(_) => {
                problems.push(
                    "token: not an agent token (it starts with wwa_; copy it from the server's page in Wren)"
                        .into(),
                );
                String::new()
            }
            None => {
                problems.push("token: missing (set it in agent.toml or WW_AGENT_TOKEN)".into());
                String::new()
            }
        };

        let mut roots = Vec::new();
        for (i, r) in raw.roots.iter().enumerate() {
            match absolute(r) {
                Ok(p) if p == Path::new("/") => {
                    problems.push(format!(
                        "roots[{i}] \"{r}\": / is too broad; list the folders Wren may touch"
                    ));
                }
                Ok(p) => roots.push(p),
                Err(e) => problems.push(format!("roots[{i}] \"{r}\": {e}")),
            }
        }

        let docker_socket = match set(raw.docker_socket) {
            Some(s) => match absolute(&s) {
                Ok(_) => s,
                Err(e) => {
                    problems.push(format!("docker_socket: {e}"));
                    String::new()
                }
            },
            None => DEFAULT_SOCKET.to_string(),
        };

        let mut names = HashSet::new();
        let mut script_stacks = Vec::new();
        for (i, s) in raw.script_stack.into_iter().enumerate() {
            let at = match &s.name {
                Some(n) if !n.trim().is_empty() => format!("script_stack[{i}] ({})", n.trim()),
                _ => format!("script_stack[{i}]"),
            };
            let before = problems.len();
            let name = s
                .name
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .to_string();
            if name.is_empty() {
                problems.push(format!("{at}: name is missing"));
            } else if !valid_name(&name) {
                problems.push(format!(
                    "{at}: name may only use a-z, 0-9, - and _ (at most 64)"
                ));
            } else if !names.insert(name.clone()) {
                problems.push(format!("{at}: name is used twice"));
            }
            let working_dir = match s.working_dir.as_deref() {
                None => {
                    problems.push(format!("{at}: working_dir is missing"));
                    PathBuf::new()
                }
                Some(w) => match absolute(w) {
                    Err(e) => {
                        problems.push(format!("{at}: working_dir {e}"));
                        PathBuf::new()
                    }
                    Ok(p) => {
                        if !roots.iter().any(|r| p.starts_with(r)) {
                            problems
                                .push(format!("{at}: working_dir {w} is not under any of roots"));
                        }
                        p
                    }
                },
            };
            let timeout = match s.timeout.as_deref() {
                None => {
                    problems.push(format!("{at}: timeout is missing (like \"20m\")"));
                    Duration::ZERO
                }
                Some(t) => parse_duration(t).unwrap_or_else(|| {
                    problems.push(format!(
                        "{at}: timeout \"{t}\" must be like \"90s\", \"20m\" or \"1h\" (at most 24h)"
                    ));
                    Duration::ZERO
                }),
            };
            let command = |script: Option<RawScript>, which: &str, problems: &mut Vec<String>| {
                let script = script?;
                match set(script.command) {
                    Some(c) => Some(c),
                    None => {
                        problems.push(format!("{at}: {which}.command is missing"));
                        None
                    }
                }
            };
            let had_update = s.update.is_some();
            let update = command(s.update, "update", &mut problems);
            if !had_update {
                problems.push(format!("{at}: update.command is missing"));
            }
            let rollback = command(s.rollback, "rollback", &mut problems);
            for key in s.env.keys() {
                if !valid_env_key(key) {
                    problems.push(format!("{at}: env name \"{key}\" is not a variable name"));
                }
            }
            let version = s.version.and_then(|v| check_version(v, &at, &mut problems));
            if problems.len() == before {
                script_stacks.push(ScriptStack {
                    compose_project: set(s.compose_project).unwrap_or_else(|| name.clone()),
                    name,
                    working_dir,
                    handles_own_backup: s.handles_own_backup,
                    env: s.env,
                    timeout,
                    update: update.unwrap_or_default(),
                    rollback,
                    version,
                });
            }
        }

        let scan_interval = match raw.scan_interval.as_deref() {
            None => SCAN_INTERVAL,
            Some(t) => match parse_duration(t) {
                Some(d) if d >= MIN_SCAN_INTERVAL => d,
                _ => {
                    problems.push(format!(
                        "scan_interval \"{t}\" must be like \"30m\" or \"6h\" (1m to 24h)"
                    ));
                    SCAN_INTERVAL
                }
            },
        };

        if problems.is_empty() {
            Ok(Config {
                server,
                token,
                roots,
                script_stacks,
                docker_socket,
                scan_interval,
            })
        } else {
            Err(ConfigError(problems))
        }
    }

    /// Reads `path` (when it doesn't exist and wasn't asked for by name,
    /// the configuration comes from the environment alone) and checks it.
    pub fn load(path: &Path, explicit: bool) -> Result<Config, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => None,
            Err(e) => {
                return Err(ConfigError(vec![format!(
                    "can't read {}: {e}",
                    path.display()
                )]));
            }
        };
        Config::from_parts(text.as_deref(), |k| std::env::var(k).ok()).map_err(|e| {
            let file = if text.is_some() {
                path.display().to_string()
            } else {
                "the environment".to_string()
            };
            ConfigError(e.0.into_iter().map(|p| format!("{file}: {p}")).collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "wwa_0123456789abcdefghijABCDEFGHIJ0123456789xyz";

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn base() -> String {
        format!(
            "server = \"http://10.0.3.26:3003/\"\ntoken = \"{TOKEN}\"\nroots = [\"/mnt/user/appdata\", \"/mnt/cache/appdata\"]\n"
        )
    }

    fn problems(extra: &str) -> Vec<String> {
        Config::from_parts(Some(&format!("{}{extra}", base())), no_env)
            .unwrap_err()
            .0
    }

    #[test]
    fn fluxer_entry_from_the_spike_loads() {
        let text = format!(
            "{}{}",
            base(),
            r#"
[[script_stack]]
name = "fluxer"
compose_project = "fluxer"
working_dir = "/mnt/cache/appdata/fluxer"
handles_own_backup = true
env = { COMPOSE_PROJECT_NAME = "fluxer" }
timeout = "20m"

[script_stack.update]
command = """
curl -fsSLO https://fluxer.dev/install.sh && curl -fsSLO https://fluxer.dev/install.sh.sha256 \
  && sha256sum -c install.sh.sha256 \
  && sh install.sh --update --allow-root --non-interactive --dir "$PWD"
"""

[script_stack.rollback]
command = "sh install.sh --rollback --allow-root --non-interactive --dir \"$PWD\""
"#
        );
        let c = Config::from_parts(Some(&text), no_env).unwrap();
        assert_eq!(c.server, "http://10.0.3.26:3003");
        assert_eq!(c.docker_socket, DEFAULT_SOCKET);
        let s = &c.script_stacks[0];
        assert_eq!(s.name, "fluxer");
        assert_eq!(s.timeout, Duration::from_secs(20 * 60));
        assert!(s.handles_own_backup);
        assert!(s.update.contains("sha256sum -c"));
        assert!(s.rollback.as_deref().unwrap().contains("--rollback"));
        assert_eq!(s.env["COMPOSE_PROJECT_NAME"], "fluxer");
    }

    #[test]
    fn config_errors_name_the_field() {
        let cases = [
            (
                "roots = [\"relative/dir\"]",
                "roots[0] \"relative/dir\": must be an absolute path",
            ),
            (
                "roots = [\"/mnt/../etc\"]",
                "roots[0] \"/mnt/../etc\": must not contain",
            ),
            ("roots = [\"/\"]", "roots[0] \"/\": / is too broad"),
            (
                "[[script_stack]]\nworking_dir = \"/mnt/user/appdata/x\"\ntimeout = \"1m\"\nupdate = { command = \"x\" }",
                "script_stack[0]: name is missing",
            ),
            (
                "[[script_stack]]\nname = \"x\"\nworking_dir = \"/mnt/user/appdata/x\"\ntimeout = \"1m\"",
                "script_stack[0] (x): update.command is missing",
            ),
            (
                "[[script_stack]]\nname = \"x\"\nworking_dir = \"/mnt/user/appdata/x\"\ntimeout = \"1m\"\nupdate = {}",
                "script_stack[0] (x): update.command is missing",
            ),
            (
                "[[script_stack]]\nname = \"x\"\nworking_dir = \"/mnt/user/appdata/x\"\nupdate = { command = \"x\" }",
                "script_stack[0] (x): timeout is missing",
            ),
            (
                "[[script_stack]]\nname = \"x\"\nworking_dir = \"/mnt/user/appdata/x\"\ntimeout = \"soon\"\nupdate = { command = \"x\" }",
                "script_stack[0] (x): timeout \"soon\"",
            ),
            (
                "[[script_stack]]\nname = \"x\"\nworking_dir = \"x\"\ntimeout = \"1m\"\nupdate = { command = \"x\" }",
                "script_stack[0] (x): working_dir must be an absolute path",
            ),
            (
                "[[script_stack]]\nname = \"x\"\nworking_dir = \"/opt/x\"\ntimeout = \"1m\"\nupdate = { command = \"x\" }",
                "script_stack[0] (x): working_dir /opt/x is not under any of roots",
            ),
            (
                "[[script_stack]]\nname = \"X Y\"\nworking_dir = \"/mnt/user/appdata/x\"\ntimeout = \"1m\"\nupdate = { command = \"x\" }",
                "name may only use",
            ),
        ];
        for (extra, want) in cases {
            // Roots replace the base's; script stacks come after it.
            let got = if extra.starts_with("roots") {
                Config::from_parts(
                    Some(&format!(
                        "server = \"http://x\"\ntoken = \"{TOKEN}\"\n{extra}\n"
                    )),
                    no_env,
                )
                .unwrap_err()
                .0
            } else {
                problems(&format!("\n{extra}\n"))
            };
            assert!(
                got.iter().any(|p| p.contains(want)),
                "{extra:?}: want {want:?}, got {got:?}"
            );
        }
    }

    #[test]
    fn unknown_keys_and_names_used_twice_are_refused() {
        let err = Config::from_parts(Some(&format!("{}rots = []\n", base())), no_env).unwrap_err();
        assert!(err.to_string().contains("rots"), "{err}");
        let stack = "[[script_stack]]\nname = \"a\"\nworking_dir = \"/mnt/user/appdata/a\"\ntimeout = \"1m\"\nupdate = { command = \"x\" }\n";
        let twice = problems(&format!("{stack}{stack}"));
        assert!(
            twice.iter().any(|p| p.contains("name is used twice")),
            "{twice:?}"
        );
    }

    #[test]
    fn server_and_token_come_from_the_file_or_the_environment() {
        let err = Config::from_parts(None, no_env).unwrap_err();
        assert!(err.0.iter().any(|p| p.starts_with("server: missing")));
        assert!(err.0.iter().any(|p| p.starts_with("token: missing")));
        let env = |k: &str| match k {
            "WW_SERVER_URL" => Some("https://wren.example.com".to_string()),
            "WW_AGENT_TOKEN" => Some(TOKEN.to_string()),
            _ => None,
        };
        let c = Config::from_parts(None, env).unwrap();
        assert_eq!(c.server, "https://wren.example.com");
        assert!(c.roots.is_empty());
        let err = Config::from_parts(Some("token = \"ww_api\"\nserver = \"ftp://x\""), no_env)
            .unwrap_err();
        assert!(
            err.0
                .iter()
                .any(|p| p.starts_with("token: not an agent token"))
        );
        assert!(err.0.iter().any(|p| p.starts_with("server: use an http")));
    }

    #[test]
    fn version_config_errors_name_the_field() {
        let stack = "[[script_stack]]\nname = \"s\"\nworking_dir = \"/mnt/user/appdata/s\"\ntimeout = \"1m\"\nupdate = { command = \"x\" }\n";
        let ok = format!(
            "{}{stack}[script_stack.version]\ngithub = \"traefik/whoami\"\ninstalled_command = \"cat VERSION\"\n",
            base()
        );
        let c = Config::from_parts(Some(&ok), no_env).unwrap();
        let v = c.script_stacks[0].version.as_ref().unwrap();
        assert_eq!(v.github, "traefik/whoami");
        assert_eq!(v.installed_command.as_deref(), Some("cat VERSION"));
        assert_eq!(v.timeout, VERSION_TIMEOUT);
        assert_eq!(c.scan_interval, SCAN_INTERVAL);
        let cases = [
            (
                "installed_command = \"x\"",
                "version needs exactly one of github, url or latest_command",
            ),
            (
                "github = \"a\"\nlatest_command = \"x\"",
                "version needs exactly one",
            ),
            (
                "github = \"not a repo\"",
                "version.github must be owner/repo",
            ),
            (
                "url = \"ftp://x\"\nregex = \"v(.*)\"",
                "version.url must start with",
            ),
            (
                "url = \"https://x\"",
                "version.url needs exactly one of regex or json_pointer",
            ),
            (
                "url = \"https://x\"\njson_pointer = \"version\"",
                "version.json_pointer must start with /",
            ),
            ("github = \"a/b\"\nregex = \"x\"", "go with version.url"),
            (
                "github = \"a/b\"\ntimeout = \"1h\"",
                "version.timeout \"1h\"",
            ),
            (
                "github = \"a/b\"\ncommand = \"x\"",
                "unknown field `command`",
            ),
        ];
        for (extra, want) in cases {
            let text = format!("{}{stack}[script_stack.version]\n{extra}\n", base());
            let got = Config::from_parts(Some(&text), no_env)
                .unwrap_err()
                .to_string();
            assert!(got.contains(want), "{extra:?}: want {want:?}, got {got:?}");
        }
        let err = Config::from_parts(
            Some(&format!("{}scan_interval = \"10s\"\n", base())),
            no_env,
        )
        .unwrap_err();
        assert!(err.to_string().contains("scan_interval \"10s\""), "{err}");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("20m"), Some(Duration::from_secs(1200)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        for bad in ["0s", "", "25h", "1d", "m", "-1s"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
    }
}
