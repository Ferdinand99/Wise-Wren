//! The real agent (`wren-agent`) against a Wren on a TCP port: its
//! heartbeat reaches Wren and the server shows as online; a replaced token
//! is refused.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::App;
use wren_agent::config::Config;
use wren_agent::heartbeat::{self, Client, SendError};

async fn serve(app: &App) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app.router.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn agent_heartbeat_reaches_wren() {
    let app = App::new().await;
    let (id, token) = app.add_server("LANBox").await;
    let url = serve(&app).await;

    // One heartbeat by hand: no Docker here, so it says why.
    let beat = heartbeat::collect(
        "/nonexistent/docker.sock",
        std::path::Path::new("/nonexistent"),
    )
    .await;
    assert!(beat.docker_error.contains("can't open"), "{beat:?}");
    assert!(!beat.unraid);
    let client = Client::new(&url, &token).unwrap();
    let reply = client.send(&beat).await.unwrap();
    assert_eq!(
        (reply.server.id, reply.server.name.as_str()),
        (id, "LANBox")
    );
    assert_eq!(reply.interval_seconds, 30);
    let status = app
        .wren
        .core
        .with_conn(move |c| wise_wren::db::server(c, id))
        .await
        .unwrap()
        .unwrap()
        .status();
    assert_eq!(status, wise_wren::db::Status::Online);
    let agent_version = app
        .count("SELECT COUNT(*) FROM servers WHERE agent_version != '' AND docker_error LIKE 'can''t open%'")
        .await;
    assert_eq!(agent_version, 1);

    // The agent's loop, configured from the environment's way (server and
    // token only), sends at once and stops when told.
    let config = Config::from_parts(None, |k| match k {
        "WW_SERVER_URL" => Some(url.clone()),
        "WW_AGENT_TOKEN" => Some(token.clone()),
        _ => None,
    })
    .unwrap();
    app.exec("UPDATE servers SET last_seen_at = NULL".into())
        .await;
    wren_agent::run(config, tokio::time::sleep(Duration::from_millis(500)))
        .await
        .unwrap();
    assert_eq!(
        app.count("SELECT COUNT(*) FROM servers WHERE last_seen_at IS NOT NULL")
            .await,
        1
    );

    // A replaced token is refused, and the agent says so.
    let fresh = wise_wren::db::new_agent_token();
    app.wren
        .core
        .with_conn(move |c| wise_wren::db::set_token(c, id, Some(&fresh)))
        .await
        .unwrap();
    match client.send(&beat).await {
        Err(SendError::Refused(msg)) => assert!(msg.contains("revoked or replaced"), "{msg}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn agent_inventory_reaches_wren() {
    let app = App::new().await;
    let (_id, token) = app.add_server("LANBox").await;
    let url = serve(&app).await;
    let text = format!(
        "server = \"{url}\"\ntoken = \"{token}\"\ndocker_socket = \"/nonexistent/docker.sock\"\n"
    );
    let config = Config::from_parts(Some(&text), |_| None).unwrap();
    let inv = wren_agent::discover::scan(&config, std::path::Path::new("/nonexistent")).await;
    assert!(inv.error.contains("can't open"), "{inv:?}");
    Client::new(&url, &token)
        .unwrap()
        .inventory(&inv)
        .await
        .unwrap();
    let recorded = app
        .count("SELECT COUNT(*) FROM servers WHERE inventory_at IS NOT NULL AND inventory_error LIKE 'can''t open%'")
        .await;
    assert_eq!(recorded, 1);
}
