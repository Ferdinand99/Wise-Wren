//! The agent.toml helper (admins, a server's page): builds an `agent.toml`
//! snippet to copy, from the server's targets and inventory: an `[updates]`
//! allow list (or allow `*` with a deny list), `roots` from the folders the
//! inventory showed, and a `[[script_stack]]` (with a Fluxer preset).
//!
//! Wren stores none of it and sends nothing to the agent (design §8): the
//! page is a form that only renders text. The person puts it in place,
//! checks it with `wren-agent --check` and restarts the agent.

use std::collections::BTreeMap;

use askama::Template;
use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum_extra::extract::cookie::SignedCookieJar;
use vole_kit::auth::CurrentUser;
use vole_kit::http::{ApiError, ApiResult, HtmlTemplate};
use vole_kit::i18n::{self, Tr as _};
use vole_kit::page::PageCtx;
use vole_kit::state::Core;

use crate::policy::{AgentPolicy, COMPOSE_MANAGER};
use crate::updates_notify::kind_label;

/// Where the Unraid template keeps the agent's config on the host.
pub const UNRAID_PATH: &str = "/mnt/user/appdata/wren-agent/agent.toml";
/// The Fluxer preset (docs/agent.md).
pub const FLUXER_DIR: &str = "/mnt/cache/appdata/fluxer";
/// Asked before anything stops: rexctl, which the installer downloads, is
/// released.
pub const FLUXER_CHECK: &str = "curl -fsSL -o /dev/null https://fluxer.dev/rexctl/release.json";
pub const FLUXER_UPDATE: &str = "curl -fsSLO https://fluxer.dev/install.sh && curl -fsSLO https://fluxer.dev/install.sh.sha256 \\\n  && sha256sum -c install.sh.sha256 \\\n  && sh install.sh --update --allow-root --non-interactive --dir \"$PWD\"";
pub const FLUXER_ROLLBACK: &str =
    "sh install.sh --rollback --allow-root --non-interactive --dir \"$PWD\"";

pub fn router() -> Router<Core> {
    Router::new().route("/servers/{id}/agent-toml", get(page))
}

/// A name the agent's `[updates]` globs can hold.
fn glob_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.*?".contains(&b))
}

/// A script stack name the agent takes (a-z 0-9 - _).
fn stack_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// An absolute folder without `.` or `..` parts, not `/`.
fn folder_ok(p: &str) -> bool {
    p.starts_with('/')
        && p != "/"
        && !p.split('/').any(|c| c == "." || c == "..")
        && !p.chars().any(char::is_control)
}

/// A TOML basic string.
pub fn q(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A TOML multi-line basic string (for commands).
fn q_multi(s: &str) -> String {
    let body: String = s
        .trim()
        .replace('\r', "")
        .chars()
        .map(|c| match c {
            '\\' => "\\\\".to_string(),
            '"' => "\\\"".to_string(),
            c if c.is_control() && c != '\n' && c != '\t' => String::new(),
            c => c.to_string(),
        })
        .collect();
    format!("\"\"\"\n{body}\n\"\"\"")
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        return "[]".into();
    }
    let one = format!(
        "[{}]",
        items.iter().map(|i| q(i)).collect::<Vec<_>>().join(", ")
    );
    if one.len() <= 72 {
        one
    } else {
        format!(
            "[\n{}\n]",
            items
                .iter()
                .map(|i| format!("  {},", q(i)))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
}

/// A script stack as the form gives it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stack {
    pub name: String,
    pub project: String,
    pub dir: String,
    pub own_backup: bool,
    pub timeout: String,
    pub check: String,
    pub update: String,
    pub rollback: String,
}

impl Stack {
    pub fn fluxer(dir: &str) -> Stack {
        Stack {
            name: "fluxer".into(),
            project: "fluxer".into(),
            dir: dir.into(),
            own_backup: true,
            timeout: "20m".into(),
            check: FLUXER_CHECK.into(),
            update: FLUXER_UPDATE.into(),
            rollback: FLUXER_ROLLBACK.into(),
        }
    }
}

/// What the snippet says.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Choice {
    /// `true`: allow `*` and deny `deny`; `false`: allow exactly `allow`.
    pub all: bool,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub roots: Vec<String>,
    pub stack: Option<Stack>,
}

/// The `agent.toml` text for `choice`.
pub fn render(server: &str, choice: &Choice) -> String {
    // The name sits in a comment: a newline in it would start a line of its
    // own (names are checked on creation, but older ones may not have been).
    let server: String = server.chars().filter(|c| !c.is_control()).collect();
    let mut out = vec![
        format!("# agent.toml for {server}, made with Wise Wren's agent.toml helper."),
        "# Keep your own server and token lines (or WW_SERVER_URL and WW_AGENT_TOKEN).".into(),
        String::new(),
        "# Folders Wren may touch, each mounted at the same path in the agent.".into(),
    ];
    let roots: Vec<String> = choice
        .roots
        .iter()
        .filter(|r| folder_ok(r))
        .cloned()
        .collect();
    out.push(format!("roots = {}", list(&roots)));
    out.push(String::new());
    out.push("# Which targets Wren may update (deny wins over allow).".into());
    out.push("[updates]".into());
    let names =
        |v: &[String]| -> Vec<String> { v.iter().filter(|n| glob_ok(n)).cloned().collect() };
    if choice.all {
        out.push("allow = [\"*\"]".into());
        out.push(format!("deny = {}", list(&names(&choice.deny))));
    } else {
        out.push(format!("allow = {}", list(&names(&choice.allow))));
    }
    if let Some(s) = &choice.stack {
        out.push(String::new());
        out.push("# A Compose stack that updates with its own script.".into());
        out.push("[[script_stack]]".into());
        out.push(format!("name = {}", q(&s.name)));
        if !s.project.is_empty() && s.project != s.name {
            out.push(format!("compose_project = {}", q(&s.project)));
        }
        out.push(format!("working_dir = {}", q(&s.dir)));
        if s.own_backup {
            out.push("handles_own_backup = true".into());
        }
        if !s.project.is_empty() {
            out.push(format!(
                "env = {{ COMPOSE_PROJECT_NAME = {} }}",
                q(&s.project)
            ));
        }
        out.push(format!(
            "timeout = {}",
            q(if s.timeout.is_empty() {
                "20m"
            } else {
                &s.timeout
            })
        ));
        if !s.check.trim().is_empty() {
            out.push(String::new());
            out.push("[script_stack.check]".into());
            out.push(format!("command = {}", q_multi(&s.check)));
        }
        out.push(String::new());
        out.push("[script_stack.update]".into());
        out.push(format!("command = {}", q_multi(&s.update)));
        if !s.rollback.trim().is_empty() {
            out.push(String::new());
            out.push("[script_stack.rollback]".into());
            out.push(format!("command = {}", q_multi(&s.rollback)));
        }
    }
    out.join("\n") + "\n"
}

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

/// A target to tick.
pub struct Pick {
    pub name: String,
    pub kind_label: &'static str,
    pub allow: bool,
    pub deny: bool,
}

/// A folder to tick.
pub struct Folder {
    pub path: String,
    pub checked: bool,
}

#[derive(Template)]
#[template(path = "agent_toml.html")]
struct HelperPage {
    ctx: PageCtx,
    server_id: i64,
    server_name: String,
    back_href: String,
    all: bool,
    picks: Vec<Pick>,
    folders: Vec<Folder>,
    extra_roots: String,
    /// '' (none), `custom` or `fluxer`.
    stack_kind: String,
    stack: Stack,
    toml: String,
    path: String,
}

impl HelperPage {
    pub fn stack_is(&self, k: &str) -> bool {
        self.stack_kind == k
    }
}

fn form(raw: Option<&str>) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, v) in url::form_urlencoded::parse(raw.unwrap_or("").as_bytes()) {
        out.entry(k.into_owned()).or_default().push(v.into_owned());
    }
    out
}

/// Whether a suggested root is ticked before the person chose: what the
/// agent already has, appdata folders and Compose Manager's projects.
fn default_root(path: &str, policy: Option<&AgentPolicy>) -> bool {
    policy.is_some_and(|p| p.roots.iter().any(|r| r == path))
        || path.ends_with("/appdata")
        || path == COMPOSE_MANAGER
}

async fn page(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    RawQuery(raw): RawQuery,
) -> ApiResult<impl IntoResponse> {
    user.require_admin()?;
    let (server, policy, folders, targets) = core
        .with_conn(move |c| {
            let row: Option<(String, Option<String>, String)> = c
                .query_row(
                    "SELECT name, policy, folders FROM servers WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    e => Err(e),
                })?;
            let targets: Vec<(String, String, String)> = c
                .prepare(
                    "SELECT name, kind, working_dir FROM targets WHERE server_id = ?1
                     ORDER BY name COLLATE NOCASE",
                )?
                .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(row.map(|(n, p, f)| (n, p, f, targets)))
        })
        .await?
        .ok_or(ApiError::NotFound)?;
    let policy = AgentPolicy::parse(policy.as_deref());
    let f = form(raw.as_deref());
    let submitted = f.contains_key("go");
    let field = |k: &str| {
        f.get(k)
            .and_then(|v| v.first())
            .map_or(String::new(), |s| s.trim().to_string())
    };
    let ticked = |k: &str| -> Vec<String> { f.get(k).cloned().unwrap_or_default() };

    // [updates]
    let all = if submitted {
        field("updates") == "all"
    } else {
        policy.as_ref().is_some_and(|p| {
            p.allow
                .as_deref()
                .is_none_or(|a| a.iter().any(|g| g == "*"))
        })
    };
    let picks: Vec<Pick> = targets
        .iter()
        .filter(|(name, _, _)| glob_ok(name))
        .map(|(name, kind, _)| {
            let (allow, deny) = if submitted {
                (
                    ticked("allow").contains(name),
                    ticked("deny").contains(name),
                )
            } else {
                match &policy {
                    Some(p) => (
                        p.allow.is_some() && p.allows(name),
                        p.deny.iter().any(|g| crate::policy::glob(g, name)),
                    ),
                    None => (false, false),
                }
            };
            Pick {
                name: name.clone(),
                kind_label: kind_label(kind),
                allow,
                deny,
            }
        })
        .collect();

    // roots
    let mut paths: Vec<String> = serde_json::from_str(&folders).unwrap_or_default();
    if let Some(p) = &policy {
        paths.extend(p.roots.iter().cloned());
    }
    paths.sort();
    paths.dedup();
    let folders: Vec<Folder> = paths
        .into_iter()
        .filter(|p| folder_ok(p))
        .map(|path| Folder {
            checked: if submitted {
                ticked("root").contains(&path)
            } else {
                default_root(&path, policy.as_ref())
            },
            path,
        })
        .collect();
    let extra_roots = field("roots_extra");

    // [[script_stack]]
    let stack_kind = match field("stack").as_str() {
        k @ ("custom" | "fluxer") => k.to_string(),
        _ => String::new(),
    };
    let fluxer_dir = targets
        .iter()
        .find(|(n, k, _)| n == "fluxer" && (k == "compose" || k == "script"))
        .map_or(FLUXER_DIR.to_string(), |(_, _, d)| d.clone());
    let preset = if stack_kind == "fluxer" {
        Stack::fluxer(&fluxer_dir)
    } else {
        Stack::default()
    };
    let pick = |k: &str, d: &str| {
        let v = field(k);
        if v.is_empty() { d.to_string() } else { v }
    };
    // Switching to the preset fills its values in; edits after that stay.
    let fresh = field("stack_was") != stack_kind;
    let stack = if fresh {
        preset
    } else {
        Stack {
            name: pick("stack_name", &preset.name),
            project: pick("stack_project", &preset.project),
            dir: pick("stack_dir", &preset.dir),
            own_backup: field("stack_own") == "true",
            timeout: pick("stack_timeout", &preset.timeout),
            check: field("stack_check"),
            update: pick("stack_update", &preset.update),
            rollback: field("stack_rollback"),
        }
    };

    let mut roots: Vec<String> = folders
        .iter()
        .filter(|f| f.checked)
        .map(|f| f.path.clone())
        .collect();
    roots.extend(
        extra_roots
            .split(['\n', ','])
            .map(str::trim)
            .filter(|r| folder_ok(r))
            .map(str::to_string),
    );
    let stack_ok = !stack_kind.is_empty()
        && stack_name_ok(&stack.name)
        && folder_ok(&stack.dir)
        && !stack.update.trim().is_empty();
    if stack_ok
        && !roots
            .iter()
            .any(|r| std::path::Path::new(&stack.dir).starts_with(r))
    {
        roots.push(stack.dir.clone());
    }
    roots.sort();
    roots.dedup();
    let choice = Choice {
        all,
        allow: picks
            .iter()
            .filter(|p| p.allow)
            .map(|p| p.name.clone())
            .chain(stack_ok.then(|| stack.name.clone()))
            .collect(),
        deny: picks
            .iter()
            .filter(|p| p.deny)
            .map(|p| p.name.clone())
            .collect(),
        roots,
        stack: stack_ok.then(|| stack.clone()),
    };
    let toml = render(&server, &choice);
    let (ctx, jar) = PageCtx::build(
        &core,
        Some(&user),
        &jar,
        i18n::t("toml.title"),
        "servers",
        "",
    )
    .await;
    Ok((
        jar,
        HtmlTemplate(HelperPage {
            ctx,
            server_id: id,
            back_href: format!("/servers/{id}"),
            server_name: server,
            all,
            picks,
            folders,
            extra_roots,
            stack_kind,
            stack,
            toml,
            path: UNRAID_PATH.to_string(),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_and_names() {
        assert_eq!(q("a\"b\\c"), r#""a\"b\\c""#);
        assert!(glob_ok("ww-test-*") && !glob_ok("bad name"));
        assert!(folder_ok("/mnt/user/appdata") && !folder_ok("/") && !folder_ok("/x/../y"));
        let text = render(
            "Tower",
            &Choice {
                allow: vec!["sonarr".into(), "bad name".into()],
                ..Choice::default()
            },
        );
        assert!(text.contains("allow = [\"sonarr\"]"), "{text}");
    }

    #[test]
    fn a_server_name_cannot_add_lines_to_the_text() {
        let name = "Tower\n[[script_stack]]\r\nname = \"x\"\u{1b}[0m\u{0085}";
        let text = render(name, &Choice::default());
        let first = text.lines().next().unwrap();
        assert!(
            first.starts_with("# agent.toml for Tower[[script_stack]]name = \"x\"[0m, made with"),
            "{first}"
        );
        assert!(
            !text.lines().any(|l| l.starts_with("[[script_stack]]")),
            "{text}"
        );
        assert!(!text.contains('\u{1b}'));
    }
}
