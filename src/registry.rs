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
//! References pinned with `@sha256:`, image IDs and images without a repo
//! digest (built locally) are `unknown` and cause no request at all.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, RETRY_AFTER, WWW_AUTHENTICATE};
use serde_json::Value;

/// Index types first: multi-arch images answer with their index digest,
/// which is what `RepoDigests` holds.
pub const ACCEPT_MANIFESTS: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";
/// How long one registry request may take.
pub const TIMEOUT: Duration = Duration::from_secs(20);
/// A `Retry-After` up to this long is waited out once; a longer one skips
/// the registry for the rest of the run.
pub const MAX_WAIT: Duration = Duration::from_secs(10);

/// Docker Hub's name in references.
pub const DOCKER_HUB: &str = "docker.io";

/// Why an image's status is `unknown` without asking a registry, as stored
/// in `status_detail`.
pub const PINNED: &str = "pinned";
pub const IMAGE_ID: &str = "image_id";
pub const LOCAL: &str = "local";

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
            if first.contains('.') || first.contains(':') || first == "localhost" =>
        {
            let registry = normal_registry(first);
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

impl ImageRef {
    /// Parses a container's `.Config.Image`. `Err` is why it can't be
    /// checked ([`PINNED`], [`IMAGE_ID`]).
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

    /// Plain HTTP only for the local machine (a test or a local registry).
    fn scheme(&self) -> &'static str {
        let host = self.registry.split(':').next().unwrap_or("");
        if host == "localhost" || host.starts_with("127.") {
            "http"
        } else {
            "https"
        }
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
    http: reqwest::Client,
    creds: Creds,
    cache: HashMap<String, Result<Remote, String>>,
    tokens: HashMap<(String, String), String>,
    blocked: HashMap<String, Instant>,
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
    pub fn new(http: reqwest::Client, creds: Creds) -> Checker {
        Checker {
            http,
            creds,
            cache: HashMap::new(),
            tokens: HashMap::new(),
            blocked: HashMap::new(),
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
        let result = self.lookup(image).await;
        self.cache.insert(key, result.clone());
        result
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
            let mut req = self
                .http
                .head(&url)
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
                    if !digest.starts_with("sha256:") {
                        return Err(format!("{host} sent no digest"));
                    }
                    return Ok(Remote {
                        digest,
                        media_type: header("content-type"),
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
        let scope = if c.scope.is_empty() {
            format!("repository:{}:pull", image.repo)
        } else {
            c.scope.clone()
        };
        let mut req = self
            .http
            .get(&c.realm)
            .query(&[("service", c.service.as_str()), ("scope", scope.as_str())])
            .timeout(TIMEOUT);
        if let Some(auth) = self.basic(image)
            && creds_may_go_to(&c.realm, image.host())
        {
            req = req.header(AUTHORIZATION, auth);
        }
        self.requests += 1;
        let res = req
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
        let body: Value = res
            .json()
            .await
            .map_err(|_| "the token service sent no token".to_string())?;
        body.get("token")
            .or_else(|| body.get("access_token"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "the token service sent no token".to_string())
    }
}

/// Whether saved credentials may go to a token `realm` the registry at
/// `host` named: over https, or to the registry's own host (a plain-http
/// registry on the LAN). Never to some other plain-http address, where
/// anyone on the path could read them.
fn creds_may_go_to(realm: &str, host: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(realm) else {
        return false;
    };
    let authority = match (url.host_str(), url.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        _ => return false,
    };
    url.scheme() == "https" || (url.scheme() == "http" && authority.eq_ignore_ascii_case(host))
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
                let digest = if path.contains("/old/") {
                    "sha256:2222"
                } else {
                    "sha256:1111"
                };
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
        let mut checker = Checker::new(reqwest::Client::new(), Creds::new());
        let image = ImageRef::parse(&format!("{addr}/team/app:1")).unwrap();
        let remote = checker.digest(&image).await.unwrap();
        assert_eq!(remote.digest, "sha256:1111");
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
        let mut checker = Checker::new(reqwest::Client::new(), creds);
        let image = ImageRef::parse(&format!("{addr}/team/old/app")).unwrap();
        assert_eq!(checker.digest(&image).await.unwrap().digest, "sha256:2222");
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
        let mut checker = Checker::new(reqwest::Client::new(), Creds::new());
        let image = ImageRef::parse(&format!("{addr}/slow/app")).unwrap();
        let started = Instant::now();
        assert_eq!(checker.digest(&image).await.unwrap().digest, "sha256:1111");
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
}
