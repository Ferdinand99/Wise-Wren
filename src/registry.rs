//! Registry checks (design §4, spike Q1): is there a new image for a
//! container's image reference?
//!
//! The method the spike proved on Docker Hub, GHCR, Quay and lscr.io: an
//! anonymous manifest `HEAD` with an index-first `Accept` list. On `401` the
//! `WWW-Authenticate: Bearer realm,service,scope` challenge gives an
//! anonymous token (or one bought with the registry's saved credentials) and
//! the `HEAD` is repeated. `docker-content-digest` is the index digest,
//! compared with the `RepoDigests` entry of the image the container runs.
//! Wren never pulls and never `GET`s a manifest, so checks cost no pull quota.
//!
//! References pinned with `@sha256:`, image IDs, images without a repo
//! digest (built locally) and references that aren't well-formed are
//! `unknown` and cause no request at all.
//!
//! Image references come from agents, so what Wren connects to is
//! constrained (design §8, [`crate::fetch`]):
//!
//! - A reference must be well-formed (host, repository and tag characters).
//! - Plain http only for `localhost` and `127.0.0.1`; everything else is https.
//! - Requests go through [`fetch::pinned_client`]: checked and pinned address,
//!   no redirects, no proxy. A registry on a private or loopback address is
//!   asked only when it is `localhost`/`127.0.0.1` or has saved credentials
//!   (Settings → Registries): that is how an admin says "this one is mine".
//! - The token service named by a `WWW-Authenticate` challenge must be https
//!   and belong to the registry: same host, same registrable domain (last two
//!   labels), Docker Hub's own names, or a pair in [`REALM_ALLOWLIST`].
//! - Token answers are capped and a token must look like one before it is
//!   sent anywhere; digests must be `sha256:` and 64 hex digits.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, RETRY_AFTER, WWW_AUTHENTICATE};
use reqwest::{StatusCode, Url};
use serde_json::Value;

use crate::fetch;

/// Index types first: multi-arch images answer with their index digest,
/// which is what `RepoDigests` holds.
pub const ACCEPT_MANIFESTS: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";
/// How long one registry request may take.
pub const TIMEOUT: Duration = Duration::from_secs(20);
/// A `Retry-After` up to this long is waited out once; a longer one skips
/// the registry for the rest of the run.
pub const MAX_WAIT: Duration = Duration::from_secs(10);
/// How long one reference may take in all: its probe, token and retries.
pub const LOOKUP_DEADLINE: Duration = Duration::from_secs(20);
/// The most a token service's answer is read.
const MAX_TOKEN_BODY: usize = 64 << 10;
/// The longest token Wren replays.
const MAX_TOKEN_LEN: usize = 8 << 10;
/// The longest media type kept.
const MAX_MEDIA_TYPE: usize = 255;

/// Docker Hub's name in references.
pub const DOCKER_HUB: &str = "docker.io";
/// Docker Hub's other hosts: registry and token service count as one family.
const DOCKER_FAMILY: [&str; 5] = [
    "docker.io",
    "registry-1.docker.io",
    "auth.docker.io",
    "index.docker.io",
    "registry.hub.docker.com",
];
/// Registries whose token service lives on another domain, by
/// `(registry host, token host)`. lscr.io is a front for ghcr.io.
pub const REALM_ALLOWLIST: [(&str, &str); 1] = [("lscr.io", "ghcr.io")];

/// Why an image's status is `unknown` without asking a registry, as stored
/// in `status_detail`.
pub const PINNED: &str = "pinned";
pub const IMAGE_ID: &str = "image_id";
pub const LOCAL: &str = "local";
/// Not a well-formed reference.
pub const INVALID: &str = "invalid";

/// An image reference split the way registries see it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageRef {
    /// `docker.io` for Docker Hub, else the host (with port) as written.
    pub registry: String,
    /// The repository, with `library/` for Docker Hub's official images.
    pub repo: String,
    pub tag: String,
}

fn is_hex(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Docker Hub's other names.
fn normal_registry(host: &str) -> String {
    match host {
        "index.docker.io" | "registry-1.docker.io" | "registry.hub.docker.com" => {
            DOCKER_HUB.to_string()
        }
        other => other.to_string(),
    }
}

/// `(registry, repo)` of a reference without its tag or digest.
fn split_name(name: &str) -> (String, String) {
    match name.split_once('/') {
        Some((first, rest))
            if first.contains('.')
                || first.contains(':')
                || first.eq_ignore_ascii_case("localhost") =>
        {
            let registry = normal_registry(&first.to_ascii_lowercase());
            let repo = if registry == DOCKER_HUB && !rest.contains('/') {
                format!("library/{rest}")
            } else {
                rest.to_string()
            };
            (registry, repo)
        }
        Some(_) => (DOCKER_HUB.to_string(), name.to_string()),
        None => (DOCKER_HUB.to_string(), format!("library/{name}")),
    }
}

/// `host` or `host:port`: lowercase letters, digits, `.` and `-`, no empty
/// label, at most 253 characters, a port of 1-65535.
fn valid_registry(registry: &str) -> bool {
    let (host, port) = match registry.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (registry, None),
    };
    let port_ok = port.is_none_or(|p| {
        !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) && {
            matches!(p.parse::<u32>(), Ok(1..=65535))
        }
    });
    port_ok
        && !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// `[a-z0-9._/-]+` without empty, `.` or `..` segments.
fn valid_repo(repo: &str) -> bool {
    !repo.is_empty()
        && repo.len() <= 255
        && repo.split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        })
}

/// `[A-Za-z0-9_.-]{1,128}`.
fn valid_tag(tag: &str) -> bool {
    (1..=128).contains(&tag.len())
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// `sha256:` and 64 lowercase hex digits.
pub fn valid_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

impl ImageRef {
    /// Parses a container's `.Config.Image`. `Err` is why it can't be
    /// checked ([`PINNED`], [`IMAGE_ID`], [`INVALID`]).
    pub fn parse(image: &str) -> Result<ImageRef, &'static str> {
        let image = image.trim();
        if image.contains('@') {
            return Err(PINNED);
        }
        if image.starts_with("sha256:") || (matches!(image.len(), 12 | 64) && is_hex(image)) {
            return Err(IMAGE_ID);
        }
        if image.is_empty() {
            return Err(IMAGE_ID);
        }
        // The tag is after the last ':' of the last path part (a ':' before
        // a '/' is a registry port).
        let last = image.rsplit('/').next().unwrap_or(image);
        let (name, tag) = match last.rsplit_once(':') {
            Some((_, tag)) => (&image[..image.len() - tag.len() - 1], tag),
            None => (image, "latest"),
        };
        let (registry, repo) = split_name(name);
        if !valid_registry(&registry) || !valid_repo(&repo) || !valid_tag(tag) {
            return Err(INVALID);
        }
        Ok(ImageRef {
            registry,
            repo,
            tag: tag.to_string(),
        })
    }

    /// The host Wren talks to.
    pub fn host(&self) -> &str {
        if self.registry == DOCKER_HUB {
            "registry-1.docker.io"
        } else {
            &self.registry
        }
    }

    /// The registry's host without its port.
    fn hostname(&self) -> &str {
        self.registry.split(':').next().unwrap_or("")
    }

    /// Literally the local machine (`localhost`, `127.0.0.1`): the only
    /// registry spoken to over plain HTTP (a test or a local registry), and
    /// one that needs no entry on the Registries page to be asked.
    fn is_local(&self) -> bool {
        matches!(self.hostname(), "localhost" | "127.0.0.1")
    }

    fn scheme(&self) -> &'static str {
        if self.is_local() { "http" } else { "https" }
    }

    /// One spelling per image (the cache key and `remote_digests.image`).
    pub fn canonical(&self) -> String {
        format!("{}/{}:{}", self.registry, self.repo, self.tag)
    }
}

/// The repo digest of the image a container runs, for `image`'s repository
/// (`docker.io/` and `library/` normalised). `None` when there is none for
/// that repository: built locally, or tagged under another name. Never a
/// guess from another repository (it would show a false "update").
pub fn local_digest(image: &ImageRef, repo_digests: &[String]) -> Option<String> {
    local_digests(image, repo_digests).into_iter().next()
}

/// Every repo digest of the image for `image`'s repository. An image pulled
/// again by another digest of the same content (a new index over the same
/// platform manifest) keeps both: each of them is the image.
pub fn local_digests(image: &ImageRef, repo_digests: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in repo_digests {
        let Some((name, digest)) = entry.split_once('@') else {
            continue;
        };
        let (registry, repo) = split_name(name);
        if registry == image.registry && repo == image.repo && !out.iter().any(|d| d == digest) {
            out.push(digest.to_string());
        }
    }
    out
}

/// What a registry answered for a reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub digest: String,
    pub media_type: String,
}

/// Saved credentials: registry (as in [`ImageRef::registry`]) → user and
/// password.
pub type Creds = HashMap<String, (String, String)>;

/// One check run's registry client: one answer per reference, one token per
/// repository, and registries that asked to wait are left alone.
pub struct Checker {
    creds: Creds,
    /// Which addresses may be asked besides the exceptions of
    /// [`Checker::open`]: public ones, or any (`fetch::any`).
    allow: fetch::Allow,
    /// One pinned client per origin, for the run.
    clients: HashMap<String, reqwest::Client>,
    cache: HashMap<String, Result<Remote, String>>,
    tokens: HashMap<(String, String), String>,
    blocked: HashMap<String, Instant>,
    /// Hosts that ran into the deadline: not asked again this run.
    slow: HashSet<String>,
    /// How long one reference may take ([`LOOKUP_DEADLINE`]).
    pub deadline: Duration,
    /// HTTP requests sent (logs and tests).
    pub requests: usize,
}

/// A `Bearer` challenge's parameters.
#[derive(Debug, Default, PartialEq)]
struct Challenge {
    realm: String,
    service: String,
    scope: String,
}

fn parse_challenge(headers: &HeaderMap) -> Option<Result<Challenge, ()>> {
    let value = headers.get(WWW_AUTHENTICATE)?.to_str().ok()?;
    let (scheme, params) = value.trim().split_once(' ').unwrap_or((value, ""));
    if !scheme.eq_ignore_ascii_case("bearer") {
        // Basic (or something else): no token to fetch.
        return Some(Err(()));
    }
    let mut c = Challenge::default();
    for part in split_params(params) {
        if let Some((k, v)) = part.split_once('=') {
            let v = v.trim().trim_matches('"').to_string();
            match k.trim().to_ascii_lowercase().as_str() {
                "realm" => c.realm = v,
                "service" => c.service = v,
                "scope" => c.scope = v,
                _ => {}
            }
        }
    }
    Some(if c.realm.is_empty() { Err(()) } else { Ok(c) })
}

/// Splits `a="x,y",b="z"` at the commas outside quotes.
fn split_params(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut quoted) = (0, false);
    for (i, ch) in s.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// Seconds of a `Retry-After` header (an HTTP date counts as a minute).
fn retry_after(headers: &HeaderMap) -> Duration {
    headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(Duration::from_secs(60), Duration::from_secs)
}

impl Checker {
    pub fn new(creds: Creds, allow: fetch::Allow) -> Checker {
        Checker {
            creds,
            allow,
            clients: HashMap::new(),
            cache: HashMap::new(),
            tokens: HashMap::new(),
            blocked: HashMap::new(),
            slow: HashSet::new(),
            deadline: LOOKUP_DEADLINE,
            requests: 0,
        }
    }

    /// The registry's current digest for `image` (once per run and
    /// reference). The error is safe to show.
    pub async fn digest(&mut self, image: &ImageRef) -> Result<Remote, String> {
        let key = image.canonical();
        if let Some(hit) = self.cache.get(&key) {
            return hit.clone();
        }
        let host = image.host().to_string();
        let result = if self.slow.contains(&host) {
            Err(format!("{host} took too long to answer"))
        } else {
            match tokio::time::timeout(self.deadline, self.lookup(image)).await {
                Ok(result) => result,
                Err(_) => {
                    self.slow.insert(host.clone());
                    Err(format!("{host} took too long to answer"))
                }
            }
        };
        self.cache.insert(key, result.clone());
        result
    }

    /// Whether `image`'s registry may be on a private or loopback address:
    /// it is the local machine, or an admin saved credentials for it.
    fn open(&self, image: &ImageRef) -> bool {
        image.is_local() || self.creds.contains_key(&image.registry)
    }

    /// The pinned client for `url`'s origin (resolved and checked once per
    /// run). `open`: private addresses are fine for this origin.
    async fn client(&mut self, url: &Url, open: bool) -> Result<reqwest::Client, String> {
        let origin = url.origin().ascii_serialization();
        if let Some(client) = self.clients.get(&origin) {
            return Ok(client.clone());
        }
        let allow = if open { fetch::any } else { self.allow };
        let client = fetch::pinned_client(url, allow).await.map_err(|err| {
            let host = url.host_str().unwrap_or("the host");
            if err == fetch::PRIVATE {
                format!(
                    "{host} is a private or local address; add it on the Registries page to check it"
                )
            } else {
                format!("can't reach {host}: {err}")
            }
        })?;
        self.clients.insert(origin, client.clone());
        Ok(client)
    }

    async fn lookup(&mut self, image: &ImageRef) -> Result<Remote, String> {
        let host = image.host().to_string();
        if self
            .blocked
            .get(&host)
            .is_some_and(|until| *until > Instant::now())
        {
            return Err(format!("{host} asked Wren to wait (rate limit)"));
        }
        let url = format!(
            "{}://{host}/v2/{}/manifests/{}",
            image.scheme(),
            image.repo,
            image.tag
        );
        let url = Url::parse(&url).map_err(|_| format!("{host} isn't a usable address"))?;
        let client = self.client(&url, self.open(image)).await?;
        let token_key = (host.clone(), image.repo.clone());
        // Docker Hub's challenge is known: skip the probe.
        if image.registry == DOCKER_HUB && !self.tokens.contains_key(&token_key) {
            let challenge = Challenge {
                realm: "https://auth.docker.io/token".into(),
                service: "registry.docker.io".into(),
                scope: format!("repository:{}:pull", image.repo),
            };
            let token = self.fetch_token(image, &challenge).await?;
            self.tokens.insert(token_key.clone(), token);
        }
        let mut waited = false;
        let mut challenged = false;
        loop {
            let auth = match self.tokens.get(&token_key) {
                Some(token) => Some(format!("Bearer {token}")),
                None if challenged => self.basic(image),
                None => None,
            };
            let mut req = client
                .head(url.clone())
                .header(ACCEPT, ACCEPT_MANIFESTS)
                .timeout(TIMEOUT);
            if let Some(auth) = auth {
                req = req.header(AUTHORIZATION, auth);
            }
            self.requests += 1;
            let res = req
                .send()
                .await
                .map_err(|e| format!("can't reach {host}: {}", without_url(&e)))?;
            let status = res.status();
            match status {
                StatusCode::OK => {
                    let header = |name: &str| {
                        res.headers()
                            .get(name)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string()
                    };
                    let digest = header("docker-content-digest");
                    if !valid_digest(&digest) {
                        return Err(format!("{host} sent no valid digest"));
                    }
                    return Ok(Remote {
                        digest,
                        media_type: header("content-type")
                            .chars()
                            .filter(|c| !c.is_control())
                            .take(MAX_MEDIA_TYPE)
                            .collect(),
                    });
                }
                StatusCode::UNAUTHORIZED if !challenged => {
                    challenged = true;
                    self.tokens.remove(&token_key);
                    match parse_challenge(res.headers()) {
                        Some(Ok(challenge)) => {
                            let token = self.fetch_token(image, &challenge).await?;
                            self.tokens.insert(token_key.clone(), token);
                        }
                        // Basic: retry with the saved credentials, if any.
                        _ if self.basic(image).is_some() => {}
                        _ => return Err(format!("{host} wants credentials")),
                    }
                }
                StatusCode::TOO_MANY_REQUESTS => {
                    let wait = retry_after(res.headers());
                    if wait <= MAX_WAIT && !waited {
                        waited = true;
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    self.blocked.insert(host.clone(), Instant::now() + wait);
                    return Err(format!("{host} asked Wren to wait (rate limit)"));
                }
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                    return Err(format!("{host} refused access ({status})"));
                }
                StatusCode::NOT_FOUND => {
                    return Err(format!("{host} doesn't know {}:{}", image.repo, image.tag));
                }
                other => return Err(format!("{host} answered {other}")),
            }
        }
    }

    fn basic(&self, image: &ImageRef) -> Option<String> {
        use base64::Engine as _;
        let (user, pass) = self.creds.get(&image.registry)?;
        let raw = format!("{user}:{pass}");
        Some(format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw)
        ))
    }

    async fn fetch_token(&mut self, image: &ImageRef, c: &Challenge) -> Result<String, String> {
        let Some(realm) = check_realm(image, &c.realm) else {
            return Err(format!(
                "{} named a token service that Wren doesn't trust",
                image.host()
            ));
        };
        let scope = if c.scope.is_empty() {
            format!("repository:{}:pull", image.repo)
        } else {
            c.scope.clone()
        };
        // The token service may be on a private address only when it is the
        // registry's own host and that one may be.
        let open = self.open(image) && realm.host_str() == Some(image.hostname());
        let client = self.client(&realm, open).await?;
        let mut req = client
            .get(realm.clone())
            .query(&[("service", c.service.as_str()), ("scope", scope.as_str())])
            .timeout(TIMEOUT);
        if let Some(auth) = self.basic(image)
            && creds_may_go_to(realm.as_str(), image.host())
        {
            req = req.header(AUTHORIZATION, auth);
        }
        self.requests += 1;
        let mut res = req
            .send()
            .await
            .map_err(|e| format!("can't get a token: {}", without_url(&e)))?;
        if res.status() == StatusCode::TOO_MANY_REQUESTS {
            let wait = retry_after(res.headers());
            self.blocked
                .insert(image.host().to_string(), Instant::now() + wait);
            return Err(format!("{} asked Wren to wait (rate limit)", image.host()));
        }
        if !res.status().is_success() {
            return Err(format!("the token service answered {}", res.status()));
        }
        // At most MAX_TOKEN_BODY bytes are read, whatever it sends.
        let mut raw = Vec::new();
        while let Ok(Some(chunk)) = res.chunk().await {
            raw.extend_from_slice(&chunk);
            if raw.len() > MAX_TOKEN_BODY {
                return Err("the token service sent too much".into());
            }
        }
        let body: Value = serde_json::from_slice(&raw)
            .map_err(|_| "the token service sent no token".to_string())?;
        let token = body
            .get("token")
            .or_else(|| body.get("access_token"))
            .and_then(Value::as_str)
            .ok_or_else(|| "the token service sent no token".to_string())?;
        if !valid_token(token) {
            return Err("the token service sent something that isn't a token".into());
        }
        Ok(token.to_string())
    }
}

/// A token is URL-safe base64 or a JWT: letters, digits and `-._~+/=`.
fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_TOKEN_LEN
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
}

/// The last two labels of a host (three under `co.uk`-style suffixes); an
/// IP address is its own domain.
fn registrable(host: &str) -> String {
    if host.parse::<IpAddr>().is_ok() {
        return host.to_string();
    }
    let labels: Vec<&str> = host.split('.').collect();
    let n = labels.len();
    let second_level = n >= 3
        && labels[n - 1].len() == 2
        && matches!(
            labels[n - 2],
            "co" | "com" | "org" | "net" | "gov" | "edu" | "ac"
        );
    labels[n.saturating_sub(if second_level { 3 } else { 2 })..].join(".")
}

/// Whether the token service at `realm_host` may serve the registry at
/// `registry_host` (both lowercase, no port).
fn same_family(registry_host: &str, realm_host: &str) -> bool {
    registry_host == realm_host
        || (DOCKER_FAMILY.contains(&registry_host) && DOCKER_FAMILY.contains(&realm_host))
        || REALM_ALLOWLIST.contains(&(registry_host, realm_host))
        || (registry_host.parse::<IpAddr>().is_err()
            && realm_host.parse::<IpAddr>().is_err()
            && registrable(registry_host) == registrable(realm_host))
}

/// The token service a registry named, if Wren may ask it: https (http only
/// when the registry is the local machine and it is the same address), no user
/// name, and a host that belongs to the registry ([`same_family`]).
fn check_realm(image: &ImageRef, realm: &str) -> Option<Url> {
    let url = Url::parse(realm).ok()?;
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let registry_host = image.host().split(':').next().unwrap_or("");
    let scheme_ok = match url.scheme() {
        "https" => true,
        "http" => image.is_local() && authority(&url).eq_ignore_ascii_case(image.host()),
        _ => false,
    };
    (scheme_ok && same_family(registry_host, &host)).then_some(url)
}

/// `host` or `host:port` of a URL (the port only when it isn't the scheme's
/// default).
fn authority(url: &Url) -> String {
    match (url.host_str(), url.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        _ => String::new(),
    }
}

/// Whether saved credentials may go to a token `realm` the registry at
/// `host` named: over https, or to the registry's own host (a plain-http
/// registry on the LAN). Never to some other plain-http address, where
/// anyone on the path could read them.
fn creds_may_go_to(realm: &str, host: &str) -> bool {
    let Ok(url) = Url::parse(realm) else {
        return false;
    };
    if url.host_str().is_none() {
        return false;
    }
    url.scheme() == "https"
        || (url.scheme() == "http" && authority(&url).eq_ignore_ascii_case(host))
}

/// A request error without the URL (it may carry a token in its query).
fn without_url(err: &reqwest::Error) -> String {
    let mut msg = err.to_string();
    if let Some(url) = err.url() {
        msg = msg.replace(url.as_str(), "");
    }
    if err.is_timeout() {
        "timed out".into()
    } else {
        msg.trim().trim_end_matches(['(', ')', ' ']).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 64 hex digits, as registries send.
    const D1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const D2: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

    #[test]
    fn credentials_only_to_https_realm() {
        assert!(creds_may_go_to(
            "https://auth.docker.io/token",
            "registry-1.docker.io"
        ));
        assert!(creds_may_go_to("https://ghcr.io/token", "ghcr.io"));
        assert!(creds_may_go_to(
            "http://10.0.3.26:5056/token",
            "10.0.3.26:5056"
        ));
        assert!(!creds_may_go_to(
            "http://evil.example/token",
            "10.0.3.26:5056"
        ));
        assert!(!creds_may_go_to(
            "http://10.0.3.26:9999/token",
            "10.0.3.26:5056"
        ));
        assert!(!creds_may_go_to("ftp://ghcr.io/token", "ghcr.io"));
        assert!(!creds_may_go_to("not a url", "ghcr.io"));
    }
    use std::sync::{Arc, Mutex};

    fn r(registry: &str, repo: &str, tag: &str) -> ImageRef {
        ImageRef {
            registry: registry.into(),
            repo: repo.into(),
            tag: tag.into(),
        }
    }

    #[test]
    fn refs_parse_like_the_spike() {
        let cases = [
            ("postgres:16", r("docker.io", "library/postgres", "16")),
            ("redis", r("docker.io", "library/redis", "latest")),
            ("fosrl/newt", r("docker.io", "fosrl/newt", "latest")),
            (
                "docker.io/library/postgres:17-alpine",
                r("docker.io", "library/postgres", "17-alpine"),
            ),
            (
                "docker.io/postgres:17",
                r("docker.io", "library/postgres", "17"),
            ),
            (
                "ghcr.io/tecnativa/docker-socket-proxy:latest",
                r("ghcr.io", "tecnativa/docker-socket-proxy", "latest"),
            ),
            (
                "localhost:5055/wren-spike/whoami",
                r("localhost:5055", "wren-spike/whoami", "latest"),
            ),
            (
                "lscr.io/linuxserver/syncthing:1.2",
                r("lscr.io", "linuxserver/syncthing", "1.2"),
            ),
        ];
        for (raw, want) in cases {
            assert_eq!(ImageRef::parse(raw).unwrap(), want, "{raw}");
        }
        assert_eq!(
            ImageRef::parse("postgres:16").unwrap().canonical(),
            "docker.io/library/postgres:16"
        );
        assert_eq!(
            ImageRef::parse("redis").unwrap().host(),
            "registry-1.docker.io"
        );
    }

    #[test]
    fn pinned_ids_and_local_images_are_unknown() {
        assert_eq!(
            ImageRef::parse("redis@sha256:1009f15c").unwrap_err(),
            PINNED
        );
        assert_eq!(
            ImageRef::parse("redis:7@sha256:1009f15c").unwrap_err(),
            PINNED
        );
        assert_eq!(
            ImageRef::parse("sha256:3e4882ccf187").unwrap_err(),
            IMAGE_ID
        );
        assert_eq!(ImageRef::parse("3e4882ccf187").unwrap_err(), IMAGE_ID);
        let local = ImageRef::parse("wren-spike-local:1").unwrap();
        assert_eq!(local_digest(&local, &[]), None);
    }

    #[test]
    fn local_digest_matches_repo() {
        let pg = ImageRef::parse("postgres:16").unwrap();
        let digests = vec![
            "other/thing@sha256:111".to_string(),
            "postgres@sha256:a902".to_string(),
        ];
        assert_eq!(local_digest(&pg, &digests).as_deref(), Some("sha256:a902"));
        let full = vec!["docker.io/library/postgres@sha256:b0f9".to_string()];
        assert_eq!(local_digest(&pg, &full).as_deref(), Some("sha256:b0f9"));
        // Another repository's digest is never used.
        assert_eq!(local_digest(&pg, &digests[..1]), None);
        // Pulled again by a new index digest: both are the image.
        let both = [
            "postgres@sha256:1267".to_string(),
            "docker.io/library/postgres@sha256:edb0".to_string(),
            "other/postgres@sha256:ffff".to_string(),
        ];
        assert_eq!(local_digests(&pg, &both), ["sha256:1267", "sha256:edb0"]);
        let ghcr = ImageRef::parse("ghcr.io/a/b:1").unwrap();
        assert_eq!(
            local_digest(&ghcr, &["ghcr.io/a/b@sha256:c".into()]).as_deref(),
            Some("sha256:c")
        );
    }

    #[test]
    fn challenges_parse() {
        let mut h = HeaderMap::new();
        h.insert(
            WWW_AUTHENTICATE,
            r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:a/b:pull,push""#
                .parse()
                .unwrap(),
        );
        let c = parse_challenge(&h).unwrap().unwrap();
        assert_eq!(c.realm, "https://ghcr.io/token");
        assert_eq!(c.scope, "repository:a/b:pull,push");
        h.insert(WWW_AUTHENTICATE, "Basic realm=\"x\"".parse().unwrap());
        assert!(parse_challenge(&h).unwrap().is_err());
    }

    /// What the fake registry saw: (method, path, authorization).
    type Seen = Arc<Mutex<Vec<(String, String, String)>>>;

    /// A registry on 127.0.0.1 that wants a Bearer token ("good") and
    /// answers `HEAD` with a digest per path; `429` once for `/v2/slow/`.
    async fn fake_registry() -> (String, Seen) {
        use axum::extract::{Request, State};
        use axum::response::{IntoResponse, Response};

        let seen: Seen = Arc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let realm = format!("http://{addr}/token");
        let slow_once = Arc::new(Mutex::new(true));
        let handler = move |State(seen): State<Seen>, req: Request| {
            let realm = realm.clone();
            let slow_once = slow_once.clone();
            async move {
                let path = req.uri().path().to_string();
                let auth = req
                    .headers()
                    .get(AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                seen.lock().unwrap().push((
                    req.method().to_string(),
                    req.uri().to_string(),
                    auth.clone(),
                ));
                if path == "/token" {
                    let token = if auth.is_empty() { "good" } else { "good-user" };
                    return axum::Json(serde_json::json!({ "token": token })).into_response();
                }
                if !auth.starts_with("Bearer good") {
                    return Response::builder()
                        .status(401)
                        .header(
                            WWW_AUTHENTICATE,
                            format!(r#"Bearer realm="{realm}",service="fake""#),
                        )
                        .body(axum::body::Body::empty())
                        .unwrap();
                }
                if path.starts_with("/v2/slow/") && std::mem::take(&mut *slow_once.lock().unwrap())
                {
                    return Response::builder()
                        .status(429)
                        .header(RETRY_AFTER, "1")
                        .body(axum::body::Body::empty())
                        .unwrap();
                }
                let digest = if path.contains("/old/") { D2 } else { D1 };
                Response::builder()
                    .status(200)
                    .header("docker-content-digest", digest)
                    .header("content-type", "application/vnd.oci.image.index.v1+json")
                    .body(axum::body::Body::empty())
                    .unwrap()
            }
        };
        let app = axum::Router::new()
            .fallback(handler)
            .with_state(seen.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (addr, seen)
    }

    #[tokio::test]
    async fn head_follows_the_challenge_and_caches() {
        let (addr, seen) = fake_registry().await;
        let mut checker = Checker::new(Creds::new(), fetch::is_public);
        let image = ImageRef::parse(&format!("{addr}/team/app:1")).unwrap();
        let remote = checker.digest(&image).await.unwrap();
        assert_eq!(remote.digest, D1);
        assert!(remote.media_type.contains("index"));
        // Probe (401), token, HEAD with the token.
        assert_eq!(checker.requests, 3);
        // Cached for the run: no more requests.
        checker.digest(&image).await.unwrap();
        assert_eq!(checker.requests, 3);
        let seen = seen.lock().unwrap();
        assert!(seen.iter().all(|(m, ..)| m == "HEAD" || m == "GET"));
        assert!(
            seen.iter()
                .filter(|(_, p, _)| p.starts_with("/v2/"))
                .all(|(m, ..)| m == "HEAD"),
            "manifests are never fetched: {seen:?}"
        );
        assert!(
            seen[1]
                .1
                .starts_with("/token?service=fake&scope=repository%3Ateam%2Fapp%3Apull")
        );
    }

    #[tokio::test]
    async fn credentials_reach_the_token_request() {
        let (addr, seen) = fake_registry().await;
        let mut creds = Creds::new();
        creds.insert(addr.clone(), ("me".into(), "secret".into()));
        let mut checker = Checker::new(creds, fetch::is_public);
        let image = ImageRef::parse(&format!("{addr}/team/old/app")).unwrap();
        assert_eq!(checker.digest(&image).await.unwrap().digest, D2);
        let seen = seen.lock().unwrap();
        let token = seen
            .iter()
            .find(|(_, p, _)| p.starts_with("/token"))
            .unwrap();
        // "me:secret"
        assert_eq!(token.2, "Basic bWU6c2VjcmV0");
        assert_eq!(seen.last().unwrap().2, "Bearer good-user");
    }

    #[tokio::test]
    async fn retry_after_is_honoured() {
        let (addr, _seen) = fake_registry().await;
        let mut checker = Checker::new(Creds::new(), fetch::is_public);
        let image = ImageRef::parse(&format!("{addr}/slow/app")).unwrap();
        let started = Instant::now();
        assert_eq!(checker.digest(&image).await.unwrap().digest, D1);
        assert!(started.elapsed() >= Duration::from_secs(1));
        // A long wait blocks the registry for the run without a request.
        checker.blocked.insert(
            image.host().to_string(),
            Instant::now() + Duration::from_secs(60),
        );
        let other = ImageRef::parse(&format!("{addr}/team/other")).unwrap();
        let before = checker.requests;
        assert!(checker.digest(&other).await.unwrap_err().contains("wait"));
        assert_eq!(checker.requests, before);
    }

    #[test]
    fn malformed_references_are_not_checked() {
        for bad in [
            "evil.example/../x:1",
            "evil.example/a//b:1",
            "evil.example/a/./b:1",
            "evil.example/x?y=z:1",
            "evil.example/x#frag",
            "evil.example/X:1",
            "evil.example/x:a b",
            "evil.example/x:",
            "evil_host.example/x:1",
            "evil.example:99999/x:1",
            "evil.example:/x:1",
            "-evil.example/x:1",
            "evil..example/x:1",
            "evil.example\\x/y:1",
            "ev\u{ef}l.example/x:1",
            "ghcr.io/Foo/bar:1",
        ] {
            assert_eq!(ImageRef::parse(bad), Err(INVALID), "{bad}");
        }
        let long_tag = format!("a/b:{}", "t".repeat(129));
        assert_eq!(ImageRef::parse(&long_tag), Err(INVALID));
        // The host is lowercased; ports are kept.
        assert_eq!(
            ImageRef::parse("GHCR.IO/foo/bar:v1.2_3-X").unwrap(),
            r("ghcr.io", "foo/bar", "v1.2_3-X")
        );
        assert_eq!(
            ImageRef::parse("registry.lan:5000/a/b").unwrap(),
            r("registry.lan:5000", "a/b", "latest")
        );
    }

    #[test]
    fn plain_http_only_for_the_local_machine() {
        for (raw, scheme) in [
            ("localhost:5055/a/b", "http"),
            ("127.0.0.1:5055/a/b", "http"),
            ("127.0.0.2:5055/a/b", "https"),
            ("127.evil.example/a/b", "https"),
            ("localhost.evil.example/a/b", "https"),
            ("10.0.0.1/a/b", "https"),
            ("ghcr.io/a/b", "https"),
            ("redis", "https"),
        ] {
            assert_eq!(ImageRef::parse(raw).unwrap().scheme(), scheme, "{raw}");
        }
    }

    #[test]
    fn digests_and_tokens_must_have_the_right_shape() {
        assert!(valid_digest(D1));
        for bad in [
            "",
            "sha256:",
            "sha256:aa,version=1.2",
            "sha256:1111",
            &D1.to_uppercase(),
            &format!("{D1}0"),
            &D1.replace("sha256:", "sha512:"),
            &format!("{D1} "),
        ] {
            assert!(!valid_digest(bad), "{bad}");
        }
        assert!(valid_token("eyJhbGciOi.eyJzdWIi_-.sig+/=="));
        assert!(!valid_token(""));
        assert!(!valid_token("a b"));
        assert!(!valid_token("a\r\nX-Evil: 1"));
        assert!(!valid_token("a\"b"));
        assert!(!valid_token(&"a".repeat(MAX_TOKEN_LEN + 1)));
        assert!(valid_token(&"a".repeat(MAX_TOKEN_LEN)));
    }

    #[test]
    fn token_services_must_belong_to_the_registry() {
        let allowed = |image: &str, realm: &str| {
            check_realm(&ImageRef::parse(image).unwrap(), realm).is_some()
        };
        // Same host, same registrable domain, Docker Hub's names, the list.
        assert!(allowed("ghcr.io/a/b", "https://ghcr.io/token"));
        assert!(allowed("quay.io/a/b", "https://quay.io/v2/auth"));
        assert!(allowed(
            "docker.elastic.co/a/b",
            "https://docker-auth.elastic.co/t"
        ));
        assert!(allowed("a/b", "https://auth.docker.io/token"));
        assert!(allowed("lscr.io/a/b", "https://ghcr.io/token"));
        assert!(allowed(
            "registry.example.co.uk/a",
            "https://auth.example.co.uk/t"
        ));
        assert!(allowed(
            "registry.lan:5000/a",
            "https://registry.lan:5001/token"
        ));
        // Another domain, another registry's name, IP addresses, other
        // schemes, user names, plain http for remote registries.
        assert!(!allowed("evil.example/a", "https://169.254.169.254/token"));
        assert!(!allowed("evil.example/a", "https://tower.lan/token"));
        assert!(!allowed("evil.example/a", "http://evil.example/token"));
        assert!(!allowed("evil.example/a", "ftp://evil.example/token"));
        assert!(!allowed("evil.example/a", "https://u:p@evil.example/token"));
        assert!(!allowed("evil.example/a", "https://notevil.example/token"));
        assert!(!allowed("evil.example/a", "file:///etc/passwd"));
        assert!(!allowed("ghcr.io/a/b", "https://lscr.io/token"));
        assert!(!allowed("example.co.uk/a", "https://victim.co.uk/t"));
        assert!(!allowed("10.0.0.1/a", "https://10.0.0.2/token"));
        assert!(!allowed("10.0.0.1/a", "http://10.0.0.1/token"));
        // Plain http only for the local machine, and only to itself.
        assert!(allowed("localhost:5055/a", "http://localhost:5055/token"));
        assert!(allowed("127.0.0.1:5055/a", "http://127.0.0.1:5055/token"));
        assert!(!allowed("127.0.0.1:5055/a", "http://127.0.0.1:9/token"));
        assert!(!allowed("127.0.0.1:5055/a", "http://10.0.0.1/token"));
        assert!(!allowed("127.0.0.1:5055/a", "http://localhost/token"));
    }

    /// A raw HTTP server on 127.0.0.1 that answers every request with
    /// `reply(path)` (nothing, ever, when that is empty) and records the
    /// request lines.
    async fn serve(reply: impl Fn(&str) -> String + Send + Sync + 'static) -> (String, Seen) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let seen: Seen = Arc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let reply = Arc::new(reply);
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let (reply, log) = (reply.clone(), log.clone());
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let text = String::from_utf8_lossy(&buf[..n]).to_string();
                    let mut head = text.lines().next().unwrap_or("").split(' ');
                    let (method, path) = (head.next().unwrap_or(""), head.next().unwrap_or(""));
                    log.lock()
                        .unwrap()
                        .push((method.into(), path.into(), String::new()));
                    let out = reply(path);
                    if out.is_empty() {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    } else {
                        let _ = s.write_all(out.as_bytes()).await;
                        let _ = s.shutdown().await;
                    }
                });
            }
        });
        (addr, seen)
    }

    fn challenge(realm: &str) -> String {
        format!(
            "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"{realm}\",service=\"x\"\r\nContent-Length: 0\r\n\r\n"
        )
    }

    const OK_EMPTY: &str = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";

    #[tokio::test]
    async fn a_token_service_elsewhere_is_never_asked() {
        // A registry that names a token service on another domain, on a
        // private or metadata address, or on a local port (all would be
        // requests from Wren's network position).
        let (victim, victim_seen) = serve(|_| OK_EMPTY.into()).await;
        for realm in [
            "http://evil.example/token".to_string(),
            "https://10.255.255.1/token".to_string(),
            "http://169.254.169.254/latest/meta-data/".to_string(),
            format!("http://{victim}/token"),
        ] {
            let named = realm.clone();
            let (addr, _) = serve(move |_| challenge(&named)).await;
            let mut checker = Checker::new(Creds::new(), fetch::is_public);
            let image = ImageRef::parse(&format!("{addr}/team/app:1")).unwrap();
            let err = checker.digest(&image).await.unwrap_err();
            assert!(err.contains("doesn't trust"), "{realm}: {err}");
            // Only the probe was sent.
            assert_eq!(checker.requests, 1, "{realm}");
        }
        assert!(victim_seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let (target, target_seen) = serve(|_| OK_EMPTY.into()).await;
        let location = format!("http://{target}/v2/x/manifests/1");
        let (addr, _) = serve(move |_| {
            format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n")
        })
        .await;
        let mut checker = Checker::new(Creds::new(), fetch::is_public);
        let image = ImageRef::parse(&format!("{addr}/team/app:1")).unwrap();
        let err = checker.digest(&image).await.unwrap_err();
        assert!(err.contains("302"), "{err}");
        assert!(target_seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn private_registries_need_an_entry_on_the_registries_page() {
        // Nothing is sent to private, link-local or metadata addresses.
        let mut checker = Checker::new(Creds::new(), fetch::is_public);
        for raw in [
            "10.255.255.1/a/b:1",
            "169.254.169.254/a/b:1",
            "192.168.1.1:5000/a/b:1",
            "127.0.0.2:5000/a/b:1",
        ] {
            let image = ImageRef::parse(raw).unwrap();
            let err = checker.digest(&image).await.unwrap_err();
            assert!(err.contains("private or local"), "{raw}: {err}");
        }
        assert_eq!(checker.requests, 0);
        // With saved credentials the registry is asked (a closed port here,
        // so it fails at the connection).
        let mut creds = Creds::new();
        creds.insert("127.0.0.2:9".into(), ("u".into(), "p".into()));
        let mut checker = Checker::new(creds, fetch::is_public);
        let image = ImageRef::parse("127.0.0.2:9/a/b:1").unwrap();
        let err = checker.digest(&image).await.unwrap_err();
        assert!(!err.contains("private or local"), "{err}");
        assert_eq!(checker.requests, 1);
    }

    #[tokio::test]
    async fn bad_tokens_and_digests_are_refused() {
        use axum::http::StatusCode;
        use axum::response::IntoResponse as _;
        // One server is registry and token service: `/token` answers a
        // token that is not one or one that is far too big; `/v2/digest/...`
        // answers a digest that is not one.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let base = format!("http://{addr}");
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let base = base.clone();
            async move {
                let path = req.uri().path().to_string();
                if path == "/token" {
                    let scope = req.uri().query().unwrap_or("");
                    let token = if scope.contains("huge") {
                        "a".repeat(100_000)
                    } else {
                        "a b\r\nX: 1".to_string()
                    };
                    return axum::Json(serde_json::json!({ "token": token })).into_response();
                }
                if path.starts_with("/v2/digest/") {
                    return ([("docker-content-digest", "sha256:aa,version=1.2")], "")
                        .into_response();
                }
                (
                    StatusCode::UNAUTHORIZED,
                    [(
                        WWW_AUTHENTICATE.as_str(),
                        format!("Bearer realm=\"{base}/token\",service=\"x\""),
                    )],
                )
                    .into_response()
            }
        });
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut checker = Checker::new(Creds::new(), fetch::is_public);
        for (repo, want) in [
            ("bad/x", "isn't a token"),
            ("huge/x", "too much"),
            ("digest/x", "no valid digest"),
        ] {
            let image = ImageRef::parse(&format!("{addr}/{repo}:1")).unwrap();
            let err = checker.digest(&image).await.unwrap_err();
            assert!(err.contains(want), "{repo}: {err}");
        }
    }

    #[tokio::test]
    async fn a_slow_registry_takes_one_deadline_then_is_skipped() {
        let (addr, seen) = serve(|_| String::new()).await;
        let mut checker = Checker::new(Creds::new(), fetch::is_public);
        checker.deadline = Duration::from_millis(300);
        let started = Instant::now();
        let first = ImageRef::parse(&format!("{addr}/a/one:1")).unwrap();
        let err = checker.digest(&first).await.unwrap_err();
        assert!(err.contains("took too long"), "{err}");
        // The same host again is refused without a request.
        let second = ImageRef::parse(&format!("{addr}/a/two:1")).unwrap();
        assert!(checker.digest(&second).await.is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
