//! Versions that don't come from image digests (design §4): a script
//! stack's version source (GitHub releases, or a URL with a regex or JSON
//! pointer; the agent's own `latest_command` reports in the inventory) and
//! release notes from GitHub for images whose
//! `org.opencontainers.image.source` is a GitHub repository.
//!
//! GitHub is asked anonymously (60 requests an hour): one answer per
//! repository is kept for [`CACHE_FOR`], and after a 403/429 Wren leaves
//! GitHub alone until the limit resets.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How long a GitHub answer is reused.
pub const CACHE_FOR: Duration = Duration::from_secs(6 * 60 * 60);
/// How long one request may take.
pub const TIMEOUT: Duration = Duration::from_secs(20);
/// The most Wren reads of a version URL.
pub const MAX_BODY: usize = 256 * 1024;
/// Longest version string kept.
pub const MAX_VERSION: usize = 100;

/// Where a script stack's newest version comes from (from `agent.toml`).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Source {
    /// `owner/repo`: the newest GitHub release.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub github: String,
    /// A page or JSON document with the version, read with `regex` or
    /// `json_pointer`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub regex: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub json_pointer: String,
    /// The agent runs its `latest_command` and reports the result.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub command: bool,
}

/// A newest version, with a page about it when there is one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Latest {
    pub version: String,
    pub url: String,
    pub title: String,
}

/// An answer from GitHub and when it came.
type Cached = (Instant, Result<Latest, String>);

/// GitHub's API with its cache. `api` is `https://api.github.com` (tests
/// point it elsewhere).
pub struct GitHub {
    api: std::sync::RwLock<String>,
    cache: Mutex<HashMap<String, Cached>>,
    paused_until: Mutex<Option<Instant>>,
    /// Requests sent (logs and tests).
    pub requests: std::sync::atomic::AtomicUsize,
}

impl GitHub {
    pub fn new(api: impl Into<String>) -> GitHub {
        GitHub {
            api: std::sync::RwLock::new(api.into().trim_end_matches('/').to_string()),
            cache: Mutex::default(),
            paused_until: Mutex::default(),
            requests: Default::default(),
        }
    }

    /// The API's base URL.
    pub fn api(&self) -> String {
        self.api.read().map(|a| a.clone()).unwrap_or_default()
    }

    /// Points Wren at another GitHub API (tests) and forgets the cache.
    pub fn set_api(&self, api: &str) {
        if let Ok(mut a) = self.api.write() {
            *a = api.trim_end_matches('/').to_string();
        }
        if let Ok(mut c) = self.cache.lock() {
            c.clear();
        }
    }

    /// The newest release of `owner/repo` (cached).
    pub async fn latest_release(
        &self,
        http: &reqwest::Client,
        repo: &str,
    ) -> Result<Latest, String> {
        if !valid_repo(repo) {
            return Err(format!("“{repo}” is not owner/repo"));
        }
        if let Some((at, hit)) = self.cache.lock().ok().and_then(|c| c.get(repo).cloned())
            && at.elapsed() < CACHE_FOR
        {
            return hit;
        }
        if self
            .paused_until
            .lock()
            .ok()
            .and_then(|p| *p)
            .is_some_and(|until| until > Instant::now())
        {
            return Err("GitHub's hourly limit is used up; Wren tries again later".into());
        }
        let result = self.fetch(http, repo).await;
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(repo.to_string(), (Instant::now(), result.clone()));
        }
        result
    }

    async fn fetch(&self, http: &reqwest::Client, repo: &str) -> Result<Latest, String> {
        self.requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let res = http
            .get(format!("{}/repos/{repo}/releases/latest", self.api()))
            .header("accept", "application/vnd.github+json")
            .timeout(TIMEOUT)
            .send()
            .await
            .map_err(|_| "GitHub can't be reached".to_string())?;
        let status = res.status();
        if status == StatusCode::FORBIDDEN || status == StatusCode::TOO_MANY_REQUESTS {
            let reset = res
                .headers()
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .and_then(|epoch| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()?
                        .as_secs();
                    Some(Duration::from_secs(
                        epoch.saturating_sub(now).clamp(60, 3600),
                    ))
                })
                .unwrap_or(Duration::from_secs(3600));
            if let Ok(mut p) = self.paused_until.lock() {
                *p = Some(Instant::now() + reset);
            }
            return Err("GitHub's hourly limit is used up; Wren tries again later".into());
        }
        if status == StatusCode::NOT_FOUND {
            return Err(format!("{repo} has no releases on GitHub"));
        }
        if !status.is_success() {
            return Err(format!("GitHub answered {status}"));
        }
        let body: Value = res
            .json()
            .await
            .map_err(|_| "GitHub's answer wasn't understood".to_string())?;
        let text = |k: &str| {
            body.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let version = clip_version(&text("tag_name"));
        if version.is_empty() {
            return Err("GitHub's answer wasn't understood".into());
        }
        let title = text("name");
        // Only GitHub's own pages become links on Wren's pages.
        let url = text("html_url");
        let url = if url.starts_with("https://github.com/") {
            url
        } else {
            String::new()
        };
        Ok(Latest {
            title: if title.trim().is_empty() {
                version.clone()
            } else {
                clip_version(&title)
            },
            version,
            url,
        })
    }
}

fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let ok = |p: Option<&str>| {
        p.is_some_and(|p| {
            !p.is_empty()
                && p.len() <= 100
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                && p != "."
                && p != ".."
        })
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

/// `owner/repo` of a GitHub source URL (`https://github.com/o/r`,
/// `…/o/r.git`, `…/o/r/tree/main`).
pub fn github_repo(source_url: &str) -> Option<String> {
    let rest = source_url
        .trim()
        .strip_prefix("https://github.com/")
        .or_else(|| source_url.trim().strip_prefix("http://github.com/"))?;
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?.trim_end_matches(".git");
    let repo = format!("{owner}/{repo}");
    valid_repo(&repo).then_some(repo)
}

fn clip_version(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_VERSION)
        .collect()
}

/// The newest version from a URL: the JSON value at `json_pointer`, or
/// `regex`'s first group (or whole match). The URL comes from an agent's
/// inventory, so it goes through [`crate::fetch`] (public addresses only,
/// every redirect checked, at most [`MAX_BODY`] read in [`TIMEOUT`]); only
/// the version is kept, never the body.
pub async fn from_url(source: &Source, allow: crate::fetch::Allow) -> Result<Latest, String> {
    let pattern = if source.json_pointer.is_empty() {
        Some(
            regex::RegexBuilder::new(&source.regex)
                .size_limit(1 << 20)
                .build()
                .map_err(|_| "the version regex is not valid".to_string())?,
        )
    } else {
        None
    };
    let bytes = crate::fetch::get_limited(&source.url, allow, MAX_BODY, TIMEOUT)
        .await
        .map_err(|e| format!("version URL: {e}"))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let version = match pattern {
        None => {
            let json: Value = serde_json::from_str(&text)
                .map_err(|_| "the version URL sent no JSON".to_string())?;
            match json.pointer(&source.json_pointer) {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n.to_string(),
                _ => return Err(format!("no version at {}", source.json_pointer)),
            }
        }
        Some(re) => {
            let caps = re
                .captures(&text)
                .ok_or_else(|| "the version regex found nothing".to_string())?;
            caps.get(1)
                .or_else(|| caps.get(0))
                .map(|m| m.as_str().to_string())
                .unwrap_or_default()
        }
    };
    let version = clip_version(&version);
    if version.is_empty() {
        return Err("the version URL gave an empty version".into());
    }
    Ok(Latest {
        title: version.clone(),
        version,
        url: String::new(),
    })
}

/// `1.2`, `v1.2.3`, `1.2.3-rc.1` as semver (missing parts are 0).
fn semver_of(v: &str) -> Option<semver::Version> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    if let Ok(parsed) = semver::Version::parse(v) {
        return Some(parsed);
    }
    let (core, rest) = match v.find(['-', '+']) {
        Some(i) => v.split_at(i),
        None => (v, ""),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.is_empty() || parts.len() > 3 || parts.iter().any(|p| p.parse::<u64>().is_err()) {
        return None;
    }
    let mut padded = parts.join(".");
    for _ in parts.len()..3 {
        padded.push_str(".0");
    }
    semver::Version::parse(&format!("{padded}{rest}")).ok()
}

/// Whether `latest` is newer than `installed`: by semver when both are
/// versions, else whenever they differ.
pub fn is_newer(latest: &str, installed: &str) -> bool {
    match (semver_of(latest), semver_of(installed)) {
        (Some(l), Some(i)) => l > i,
        _ => {
            latest.trim().trim_start_matches(['v', 'V'])
                != installed.trim().trim_start_matches(['v', 'V'])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        assert!(is_newer("v1.12.0", "v1.10.3"));
        assert!(!is_newer("v1.10.3", "1.10.3"));
        assert!(!is_newer("1.9", "1.10.0"));
        assert!(is_newer("2.0.0", "2.0.0-rc.1"));
        assert!(is_newer("2026.1023", "2026.1020"));
        assert!(is_newer("build-b", "build-a"));
        assert!(!is_newer("build-a", "build-a"));
    }

    #[test]
    fn github_repos_from_sources() {
        assert_eq!(
            github_repo("https://github.com/fosrl/newt").as_deref(),
            Some("fosrl/newt")
        );
        assert_eq!(
            github_repo("https://github.com/a/b.git").as_deref(),
            Some("a/b")
        );
        assert_eq!(
            github_repo("https://github.com/a/b/tree/main").as_deref(),
            Some("a/b")
        );
        assert_eq!(github_repo("https://gitlab.com/a/b"), None);
        assert_eq!(github_repo("https://github.com/a"), None);
        assert!(!valid_repo("a/../b"));
    }
}
