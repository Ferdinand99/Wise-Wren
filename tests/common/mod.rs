//! Shared by the integration tests: a Wren in a temporary directory, driven
//! in-process, people, tokens and a signed-in browser.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt as _;
use wise_wren::Wren;

/// The password of every user the tests add.
pub const PASSWORD: &str = "Wren-Owner-Pass-2026!";
pub const HOST: &str = "wren.test:3003";

pub struct Res {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl Res {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }

    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }
}

static N: AtomicUsize = AtomicUsize::new(0);

/// A Wren in a temporary directory.
pub struct App {
    pub wren: Wren,
    pub router: Router,
    pub dir: PathBuf,
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl App {
    pub async fn new() -> App {
        let dir = std::env::temp_dir().join(format!(
            "wise-wren-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wren = wise_wren::start(&dir, None).await.unwrap();
        App {
            router: wise_wren::router(wren.clone()),
            wren,
            dir,
        }
    }

    /// Sends `req` as a program would (no cookies).
    pub async fn send(&self, mut req: Request<Body>) -> Res {
        let h = req.headers_mut();
        if !h.contains_key(header::HOST) {
            h.insert(header::HOST, HeaderValue::from_static(HOST));
        }
        let peer: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(peer));
        let res = self.router.clone().oneshot(req).await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        Res {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    pub async fn count(&self, sql: &'static str) -> i64 {
        self.wren
            .core
            .with_conn(move |c| c.query_row(sql, [], |r| r.get(0)))
            .await
            .unwrap()
    }

    /// Runs SQL on the database.
    pub async fn exec(&self, sql: String) {
        self.wren
            .core
            .with_conn(move |c| c.execute_batch(&sql))
            .await
            .unwrap();
    }

    /// Adds a server straight in the database; its id and agent token.
    pub async fn add_server(&self, name: &'static str) -> (i64, String) {
        let token = wise_wren::db::new_agent_token();
        let t = token.clone();
        let id = self
            .wren
            .core
            .with_conn(move |c| wise_wren::db::insert_server(c, name, &t))
            .await
            .unwrap();
        (id, token.token)
    }

    /// `POST /api/agent/v1/heartbeat` with `token` and `body`.
    pub async fn heartbeat(&self, token: Option<&str>, body: &Value) -> Res {
        let mut req = Request::post("/api/agent/v1/heartbeat")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        self.send(req.body(Body::from(body.to_string())).unwrap())
            .await
    }
}

/// Adds a user with `role` (`viewer`, `editor`, `admin`); its id.
pub async fn add_user(app: &App, name: &'static str, role: &'static str) -> i64 {
    let hash = vole_kit::auth::hash_password(PASSWORD);
    app.wren
        .core
        .with_conn(move |c| {
            c.execute(
                "INSERT INTO users (username, password_hash, role) VALUES (?1, ?2, ?3)",
                rusqlite::params![name, hash, role],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await
        .unwrap()
}

/// An API token of `user_id` with `scope`.
pub async fn api_token(app: &App, user_id: i64, scope: vole_kit::auth::TokenScope) -> String {
    use vole_kit::auth::tokens;
    let token = tokens::generate();
    let (hash, prefix) = (tokens::hash(&token), tokens::display_prefix(&token));
    app.wren
        .core
        .with_conn(move |c| {
            vole_kit::db::api_tokens::insert(c, user_id, "t", &prefix, &hash, scope, None)?;
            Ok(())
        })
        .await
        .unwrap();
    token
}

/// A request with `Authorization: Bearer <token>`.
pub async fn bearer(app: &App, method: &str, path: &str, token: &str) -> Res {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    app.send(req).await
}

/// A browser with its cookies.
pub struct Web<'a> {
    pub app: &'a App,
    pub cookies: BTreeMap<String, String>,
}

impl<'a> Web<'a> {
    /// A browser that hasn't signed in.
    pub fn anonymous(app: &'a App) -> Web<'a> {
        Web {
            app,
            cookies: BTreeMap::new(),
        }
    }

    /// Adds a user with `role` and signs in.
    pub async fn new(app: &'a App, name: &'static str, role: &'static str) -> Web<'a> {
        add_user(app, name, role).await;
        let mut b = Web::anonymous(app);
        let res = b
            .post("/login", &[("username", name), ("password", PASSWORD)])
            .await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);
        b
    }

    pub fn cookie_header(&self) -> String {
        self.cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub async fn send(&mut self, mut req: Request<Body>) -> Res {
        let h = req.headers_mut();
        h.insert(header::HOST, HeaderValue::from_static(HOST));
        if !self.cookies.is_empty() {
            h.insert(
                header::COOKIE,
                HeaderValue::from_str(&self.cookie_header()).unwrap(),
            );
        }
        let peer: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(peer));
        let res = self.app.router.clone().oneshot(req).await.unwrap();
        for v in res.headers().get_all(header::SET_COOKIE) {
            let v = v.to_str().unwrap();
            if let Some((k, val)) = v.split(';').next().unwrap().split_once('=') {
                if val.is_empty() || v.contains("Max-Age=0") {
                    self.cookies.remove(k);
                } else {
                    self.cookies.insert(k.into(), val.into());
                }
            }
        }
        let (status, headers) = (res.status(), res.headers().clone());
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        Res {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    pub async fn get(&mut self, path: &str) -> Res {
        self.send(Request::get(path).body(Body::empty()).unwrap())
            .await
    }

    /// A form post from the app's own pages (with `HX-Request` when `htmx`).
    pub async fn post_with(&mut self, path: &str, form: &[(&str, &str)], htmx: bool) -> Res {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        let mut req = Request::post(path)
            .header(header::ORIGIN, format!("http://{HOST}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if htmx {
            req = req.header("hx-request", "true");
        }
        self.send(req.body(Body::from(body)).unwrap()).await
    }

    pub async fn post(&mut self, path: &str, form: &[(&str, &str)]) -> Res {
        self.post_with(path, form, false).await
    }

    pub async fn delete(&mut self, path: &str) -> Res {
        let req = Request::delete(path)
            .header(header::ORIGIN, format!("http://{HOST}"))
            .header("hx-request", "true")
            .body(Body::empty())
            .unwrap();
        self.send(req).await
    }
}

/// The next server-sent event from `body`.
pub async fn next_event(body: &mut axum::body::BodyDataStream) -> String {
    use futures_util::StreamExt as _;
    let mut text = String::new();
    while !text.contains("\n\n") {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
            .await
            .expect("an event within 5 s")
            .expect("the stream is open")
            .unwrap();
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    text
}
