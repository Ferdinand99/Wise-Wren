//! The inventory (design §4, phase 3): what runs on this server, read from
//! Docker (`GET`s only), the Unraid templates and `agent.toml`. The only
//! processes the agent starts are its own script stacks' version commands
//! (`[script_stack.version]`); nothing is changed.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::config::{Config, ScriptStack as StackConfig, VersionConfig};
use crate::docker::get_json;

pub const PATH: &str = "/api/agent/v1/inventory";
/// Where Unraid keeps the user's container templates (under the root).
pub const TEMPLATES: &str = "boot/config/plugins/dockerMan/templates-user";
/// Labels Wren gets; every other label stays on the server.
pub const LABEL_PREFIXES: [&str; 3] = [
    "org.opencontainers.image.",
    "com.docker.compose.",
    "net.unraid.docker.",
];
/// Longest command output kept (characters of its first line).
pub const MAX_OUTPUT: usize = 100;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Inventory {
    pub containers: Vec<Container>,
    pub unraid_templates: Vec<String>,
    pub compose_projects: Vec<ComposeProject>,
    pub script_stacks: Vec<ScriptStack>,
    pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Container {
    pub name: String,
    /// `.Config.Image`: `docker ps` drops `@sha256:` pins (spike Q1).
    pub image: String,
    pub image_id: String,
    pub repo_digests: Vec<String>,
    pub state: String,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ComposeProject {
    pub name: String,
    pub working_dir: String,
    pub config_files: String,
    pub files_reachable: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ScriptStack {
    pub name: String,
    pub compose_project: String,
    pub working_dir: String,
    pub files_reachable: bool,
    pub version: Option<Version>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Version {
    pub source: Source,
    pub installed: String,
    pub latest: String,
    pub error: String,
}

/// The version source as Wren reads it (it looks up `github` and `url`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Source {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub github: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub regex: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub json_pointer: String,
    pub command: bool,
}

/// `path` (absolute) as seen under `root` (`/` in the agent).
fn under(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

/// Reads everything. A Docker failure ends up in `error`; the rest is
/// still reported.
pub async fn scan(config: &Config, root: &Path) -> Inventory {
    let mut inv = Inventory {
        unraid_templates: unraid_templates(root),
        ..Inventory::default()
    };
    match containers(&config.docker_socket).await {
        Ok(list) => inv.containers = list,
        Err(err) => inv.error = err,
    }
    inv.compose_projects = compose_projects(&inv.containers, root);
    for stack in &config.script_stacks {
        inv.script_stacks.push(script_stack(stack, root).await);
    }
    inv
}

fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Every container (running or not) with its configured image and the
/// repo digests of the image it runs.
pub async fn containers(socket: &str) -> Result<Vec<Container>, String> {
    let list = get_json(socket, "/containers/json?all=1").await?;
    let mut images: HashMap<String, Vec<String>> = HashMap::new();
    let mut out = Vec::new();
    for c in list.as_array().into_iter().flatten() {
        let id = text(c, "Id");
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        // Gone since the list: skip it.
        let Ok(inspect) = get_json(socket, &format!("/containers/{id}/json")).await else {
            continue;
        };
        let name = text(&inspect, "Name").trim_start_matches('/').to_string();
        let config = inspect.get("Config").cloned().unwrap_or(Value::Null);
        let image_id = text(&inspect, "Image");
        let repo_digests = match images.get(&image_id) {
            Some(d) => d.clone(),
            None => {
                let digests: Vec<String> = if image_id.starts_with("sha256:") {
                    get_json(socket, &format!("/images/{image_id}/json"))
                        .await
                        .ok()
                        .and_then(|i| i.get("RepoDigests").cloned())
                        .and_then(|d| serde_json::from_value(d).ok())
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                images.insert(image_id.clone(), digests.clone());
                digests
            }
        };
        let labels = config
            .get("Labels")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter(|(k, _)| LABEL_PREFIXES.iter().any(|p| k.starts_with(p)))
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        out.push(Container {
            name,
            image: text(&config, "Image"),
            image_id,
            repo_digests,
            state: text(c, "State"),
            labels,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The `<Name>` of every `my-*.xml` Unraid template.
pub fn unraid_templates(root: &Path) -> Vec<String> {
    let Ok(dir) = std::fs::read_dir(root.join(TEMPLATES)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = dir
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.starts_with("my-") && n.ends_with(".xml")
        })
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|xml| template_name(&xml))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The text of the first `<Name>` element.
pub fn template_name(xml: &str) -> Option<String> {
    let start = xml.find("<Name>")? + "<Name>".len();
    let end = start + xml[start..].find("</Name>")?;
    let name = xml[start..end]
        .trim()
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    (!name.is_empty()).then_some(name)
}

/// The Compose projects of these containers, with their files' place.
pub fn compose_projects(containers: &[Container], root: &Path) -> Vec<ComposeProject> {
    let mut out: BTreeMap<String, ComposeProject> = BTreeMap::new();
    for c in containers {
        let Some(project) = c.labels.get("com.docker.compose.project") else {
            continue;
        };
        out.entry(project.clone()).or_insert_with(|| {
            let label = |k: &str| c.labels.get(k).cloned().unwrap_or_default();
            let working_dir = label("com.docker.compose.project.working_dir");
            ComposeProject {
                name: project.clone(),
                // The files only count when the folder is at the same path
                // here (the daemon resolves binds on the host).
                files_reachable: working_dir.starts_with('/') && under(root, &working_dir).is_dir(),
                config_files: label("com.docker.compose.project.config_files"),
                working_dir,
            }
        });
    }
    out.into_values().collect()
}

async fn script_stack(stack: &StackConfig, root: &Path) -> ScriptStack {
    let dir = stack.working_dir.to_string_lossy().to_string();
    let reachable = under(root, &dir).is_dir();
    let version = match &stack.version {
        Some(v) => Some(version(stack, v, root, reachable).await),
        None => None,
    };
    ScriptStack {
        name: stack.name.clone(),
        compose_project: stack.compose_project.clone(),
        working_dir: dir,
        files_reachable: reachable,
        version,
    }
}

async fn version(stack: &StackConfig, v: &VersionConfig, root: &Path, reachable: bool) -> Version {
    let mut out = Version {
        source: Source {
            github: v.github.clone(),
            url: v.url.clone(),
            regex: v.regex.clone(),
            json_pointer: v.json_pointer.clone(),
            command: v.latest_command.is_some(),
        },
        ..Version::default()
    };
    let commands = [
        (v.installed_command.as_deref(), "installed_command"),
        (v.latest_command.as_deref(), "latest_command"),
    ];
    for (i, (command, field)) in commands.into_iter().enumerate() {
        let Some(command) = command else { continue };
        let result = if reachable {
            run(
                command,
                &under(root, &stack.working_dir.to_string_lossy()),
                &stack.env,
                v.timeout,
            )
            .await
        } else {
            Err(format!(
                "{} is not mounted in the agent",
                stack.working_dir.display()
            ))
        };
        match result {
            Ok(text) if i == 0 => out.installed = text,
            Ok(text) => out.latest = text,
            Err(err) => {
                if out.error.is_empty() {
                    out.error = format!("{field}: {err}");
                }
            }
        }
    }
    out
}

/// Runs one of the agent's own version commands (`sh -c`) in `dir`; the
/// first non-empty line it prints.
pub async fn run(
    command: &str,
    dir: &Path,
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<String, String> {
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .envs(env)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(timeout, child)
        .await
        .map_err(|_| format!("took longer than {} s", timeout.as_secs()))?
        .map_err(|e| format!("can't start sh: {e}"))?;
    let first = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_OUTPUT)
            .collect::<String>()
    };
    if !output.status.success() {
        let code = output
            .status
            .code()
            .map_or_else(|| "a signal".to_string(), |c| c.to_string());
        return Err(format!("exited with {code}: {}", first(&output.stderr)));
    }
    let line = first(&output.stdout);
    if line.is_empty() {
        return Err("printed nothing".into());
    }
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_names() {
        assert_eq!(
            template_name("<?xml?>\n<Container>\n  <Name>Newt</Name>\n</Container>").as_deref(),
            Some("Newt")
        );
        assert_eq!(
            template_name("<Name>A &amp; B</Name>").as_deref(),
            Some("A & B")
        );
        assert_eq!(template_name("<Name></Name>"), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scan_reads_docker_templates_and_stacks() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let root = std::env::temp_dir().join(format!("wren-agent-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let templates = root.join(TEMPLATES);
        std::fs::create_dir_all(&templates).unwrap();
        std::fs::write(
            templates.join("my-Newt.xml"),
            "<Container><Name>Newt</Name></Container>",
        )
        .unwrap();
        std::fs::write(
            templates.join("other.xml"),
            "<Container><Name>Nope</Name></Container>",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("srv/postiz")).unwrap();
        std::fs::create_dir_all(root.join("srv/stack")).unwrap();
        std::fs::write(root.join("srv/stack/VERSION"), "v1.10.3\n").unwrap();

        // A fake daemon: two containers, one image shared.
        let sock = root.join("docker.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = requests.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = vec![0; 2048];
                let n = s.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let line = req.lines().next().unwrap().to_string();
                seen.lock().unwrap().push(line.clone());
                let body = match line.split(' ').nth(1).unwrap() {
                    "/containers/json?all=1" => r#"[{"Id":"aaa1","Image":"redis","State":"running"},{"Id":"bbb2","Image":"postgres:16","State":"exited"}]"#.to_string(),
                    "/containers/aaa1/json" => r#"{"Name":"/Newt","Image":"sha256:111","Config":{"Image":"redis@sha256:abc","Labels":{"org.opencontainers.image.source":"https://github.com/x/y","traefik.enable":"true"}}}"#.to_string(),
                    "/containers/bbb2/json" => r#"{"Name":"/postiz-postgres","Image":"sha256:222","Config":{"Image":"postgres:16","Labels":{"com.docker.compose.project":"postiz","com.docker.compose.project.working_dir":"/srv/postiz","com.docker.compose.project.config_files":"/srv/postiz/compose.yaml"}}}"#.to_string(),
                    "/images/sha256:111/json" => r#"{"RepoDigests":["redis@sha256:abc"]}"#.to_string(),
                    "/images/sha256:222/json" => r#"{"RepoDigests":["postgres@sha256:def"]}"#.to_string(),
                    other => panic!("unexpected {other}"),
                };
                assert!(req.starts_with("GET "), "only GETs: {req}");
                let res =
                    format!("HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{body}");
                s.write_all(res.as_bytes()).await.unwrap();
            }
        });

        let toml = format!(
            r#"server = "http://wren"
token = "wwa_0123456789abcdefghijABCDEFGHIJ0123456789xyz"
roots = ["/srv"]
docker_socket = "{}"

[[script_stack]]
name = "stack"
working_dir = "/srv/stack"
timeout = "1m"
env = {{ WHO = "wren" }}
update = {{ command = "true" }}
[script_stack.version]
latest_command = "echo v1.12.0-$WHO"
installed_command = "cat VERSION"

[[script_stack]]
name = "away"
working_dir = "/srv/away"
timeout = "1m"
update = {{ command = "true" }}
[script_stack.version]
github = "a/b"
installed_command = "cat VERSION"
"#,
            sock.display()
        );
        let config = Config::from_parts(Some(&toml), |_| None).unwrap();
        let inv = scan(&config, &root).await;
        assert_eq!(inv.error, "");
        assert_eq!(inv.unraid_templates, ["Newt"]);
        assert_eq!(inv.containers.len(), 2);
        let newt = &inv.containers[0];
        assert_eq!(newt.name, "Newt");
        assert_eq!(
            newt.image, "redis@sha256:abc",
            "Config.Image, not docker ps"
        );
        assert_eq!(newt.repo_digests, ["redis@sha256:abc"]);
        assert!(!newt.labels.contains_key("traefik.enable"));
        assert_eq!(inv.containers[1].state, "exited");
        assert_eq!(
            inv.compose_projects,
            [ComposeProject {
                name: "postiz".into(),
                working_dir: "/srv/postiz".into(),
                config_files: "/srv/postiz/compose.yaml".into(),
                files_reachable: true,
            }]
        );
        let stack = &inv.script_stacks[0];
        assert!(stack.files_reachable);
        let v = stack.version.as_ref().unwrap();
        assert_eq!(
            (v.installed.as_str(), v.latest.as_str()),
            ("v1.10.3", "v1.12.0-wren")
        );
        assert!(v.source.command);
        let away = &inv.script_stacks[1];
        assert!(!away.files_reachable);
        let v = away.version.as_ref().unwrap();
        assert_eq!(v.source.github, "a/b");
        assert!(v.error.contains("not mounted"), "{v:?}");
        // The image is inspected once per image.
        assert_eq!(requests.lock().unwrap().len(), 5);

        let slow = run(
            "sleep 5",
            &root,
            &BTreeMap::new(),
            Duration::from_millis(200),
        )
        .await;
        assert!(slow.unwrap_err().contains("longer"));
        let failed = run(
            "echo oops >&2; exit 3",
            &root,
            &BTreeMap::new(),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(failed.unwrap_err(), "exited with 3: oops");
        let _ = std::fs::remove_dir_all(&root);
    }
}
