use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    extract::{Query, State},
    http::{Response, StatusCode, header},
    routing::get,
};
use clap::Parser;
use easydl::{
    bilibili_auth::BilibiliAuthClient,
    cli::{AppContext, Cli, run},
};
use serde_json::json;
use tempfile::tempdir;
use tokio::{net::TcpListener, task::JoinHandle};

#[tokio::test]
async fn creates_polls_persists_and_clears_qr_login_state() {
    let server = AuthServer::start().await;
    let config = tempdir().unwrap();
    let client = BilibiliAuthClient::with_base_url(
        Some(config.path().to_path_buf()),
        None,
        Duration::from_secs(5),
        &server.base,
    )
    .unwrap();

    let session = client.create_session().await.unwrap();
    assert_eq!(session.session_id, "key123");
    let png = tokio::fs::read(&session.qr_path).await.unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            tokio::fs::metadata(&session.qr_path)
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    let result = client.poll_session(&session.session_id).await.unwrap();
    assert!(result.logged_in);
    let public_result = serde_json::to_string(&result).unwrap();
    for secret in ["sess/data", "sess%2Fdata", "csrf", "abc123"] {
        assert!(!public_result.contains(secret));
    }
    assert_eq!(
        result.cookie_names.unwrap(),
        ["SESSDATA", "DedeUserID", "bili_jct", "sid"]
    );
    let auth: serde_json::Value = serde_json::from_slice(
        &tokio::fs::read(config.path().join("auth.json"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(auth["bilibili"]["sessData"], "sess/data");
    assert_eq!(
        auth["bilibili"]["cookieHeader"],
        "SESSDATA=sess%2Fdata; DedeUserID=42; bili_jct=csrf; sid=abc123"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            tokio::fs::metadata(config.path())
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            tokio::fs::metadata(config.path().join("auth.json"))
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert!(client.status().await.unwrap().logged_in);
    assert!(client.logout().await.unwrap().cleared);
    assert!(!client.status().await.unwrap().logged_in);
}

#[tokio::test]
async fn exposes_qr_login_without_adding_a_fifth_top_level_verb() {
    let server = AuthServer::start().await;
    let config = tempdir().unwrap();
    let context = AppContext {
        bilibili_auth_base_url: Some(server.base.clone()),
        config_dir: Some(config.path().to_path_buf()),
        ..AppContext::default()
    };
    let create =
        Cli::try_parse_from(["easydl", "execute", "--bilibili-auth", "create", "--json"]).unwrap();
    assert!(run(create, context.clone()).await.unwrap());
    let poll = Cli::try_parse_from([
        "easydl",
        "execute",
        "--bilibili-auth",
        "poll",
        "--auth-session",
        "key123",
        "--json",
    ])
    .unwrap();
    assert!(run(poll, context.clone()).await.unwrap());
    let auth = tokio::fs::read_to_string(config.path().join("auth.json"))
        .await
        .unwrap();
    assert!(auth.contains("sess/data"));
    let logout =
        Cli::try_parse_from(["easydl", "execute", "--bilibili-auth", "logout", "--json"]).unwrap();
    assert!(run(logout, context).await.unwrap());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            &tokio::fs::read_to_string(config.path().join("auth.json"))
                .await
                .unwrap()
        )
        .unwrap(),
        json!({})
    );
}

#[tokio::test]
async fn waits_through_pending_qr_status_and_validates_session_ids() {
    let server = AuthServer::start().await;
    let config = tempdir().unwrap();
    let client = BilibiliAuthClient::with_base_url(
        Some(config.path().to_path_buf()),
        None,
        Duration::from_secs(5),
        &server.base,
    )
    .unwrap();
    let result = client
        .wait_for_login("wait_key", Duration::from_millis(1), Duration::from_secs(1))
        .await
        .unwrap();
    assert!(result.logged_in);
    assert_eq!(server.polls.load(Ordering::Relaxed), 2);
    assert_eq!(
        client
            .poll_session("../unsafe")
            .await
            .unwrap_err()
            .info
            .code,
        "auth.invalid_session"
    );
}

#[derive(Clone)]
struct AuthState {
    polls: Arc<AtomicUsize>,
}

struct AuthServer {
    base: String,
    polls: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl AuthServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let polls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/x/passport-login/web/qrcode/generate",
                get(|| async {
                    json_response(json!({
                        "code": 0,
                        "data": {
                            "url": "https://passport.bilibili.com/qrcode/test",
                            "qrcode_key": "key123"
                        }
                    }))
                }),
            )
            .route("/x/passport-login/web/qrcode/poll", get(poll))
            .with_state(AuthState {
                polls: Arc::clone(&polls),
            });
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, polls, task }
    }
}

impl Drop for AuthServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn poll(
    State(state): State<AuthState>,
    Query(query): Query<std::collections::BTreeMap<String, String>>,
) -> Response<Body> {
    let key = query
        .get("qrcode_key")
        .map(String::as_str)
        .unwrap_or_default();
    let count = state.polls.fetch_add(1, Ordering::Relaxed);
    if key == "wait_key" && count == 0 {
        return json_response(json!({
            "code": 0,
            "data": { "code": 86101, "message": "not scanned" }
        }));
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::SET_COOKIE,
            "sid=abc123; Path=/; Domain=.bilibili.com",
        )
        .body(Body::from(
            json!({
                "code": 0,
                "data": {
                    "code": 0,
                    "message": "ok",
                    "url": "https://www.bilibili.com/?SESSDATA=sess%2Fdata&DedeUserID=42&bili_jct=csrf"
                }
            })
            .to_string(),
        ))
        .unwrap()
}

fn json_response(value: serde_json::Value) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}
