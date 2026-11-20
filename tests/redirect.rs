use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect},
    routing::get,
};
use easydl::{
    domain::{
        DownloadEvent, DownloadPlan, DownloadSource, DownloadTask, OutputSpec, PlanSource,
        SCHEMA_VERSION,
    },
    downloader::{CollisionPolicy, ExecuteOptions, execute_plan},
};
use tempfile::tempdir;
use time::OffsetDateTime;
use tokio::{net::TcpListener, task::JoinHandle};

#[tokio::test]
async fn follows_http_redirects_without_leaking_executor_state() {
    let server = RedirectServer::start().await;
    let output = tempdir().unwrap();
    let summary = execute_plan(
        plan(&server.url("/redirect")),
        options(output.path()),
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();
    assert!(summary.success);
    assert_eq!(
        tokio::fs::read(output.path().join("file.bin"))
            .await
            .unwrap(),
        b"redirected"
    );
}

#[tokio::test]
async fn rejects_redirects_to_non_http_schemes() {
    let server = RedirectServer::start().await;
    let output = tempdir().unwrap();
    let summary = execute_plan(
        plan(&server.url("/unsafe")),
        options(output.path()),
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();
    assert!(!summary.success);
    assert_eq!(summary.failures[0].error.code, "download.sources_exhausted");
    assert_eq!(
        summary.failures[0].error.context["lastCode"],
        "http.unsafe_redirect"
    );
}

fn plan(url: &str) -> DownloadPlan {
    DownloadPlan {
        schema_version: SCHEMA_VERSION,
        created_at: OffsetDateTime::now_utc(),
        expires_at: None,
        source: PlanSource {
            platform: "fixture".to_owned(),
            media_id: "redirect".to_owned(),
        },
        tasks: vec![DownloadTask::File {
            id: "redirect".to_owned(),
            sources: vec![DownloadSource {
                url: url.to_owned(),
                headers: Default::default(),
                auth_ref: None,
            }],
            output: OutputSpec {
                relative_path: "file.bin".to_owned(),
            },
        }],
    }
}

fn options(output_root: &std::path::Path) -> ExecuteOptions {
    ExecuteOptions {
        output_root: output_root.to_path_buf(),
        collision: CollisionPolicy::Skip,
        timeout: Duration::from_secs(5),
        ..ExecuteOptions::default()
    }
}

struct RedirectServer {
    base: String,
    task: JoinHandle<()>,
}

impl RedirectServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/redirect", get(|| async { Redirect::temporary("/file") }))
            .route("/file", get(|| async { "redirected" }))
            .route(
                "/unsafe",
                get(|| async {
                    (
                        StatusCode::FOUND,
                        [(
                            header::LOCATION,
                            HeaderValue::from_static("file:///tmp/secret"),
                        )],
                    )
                        .into_response()
                }),
            );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, task }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

impl Drop for RedirectServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
