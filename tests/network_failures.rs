use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{Router, extract::State, routing::get};
use easydl::{
    domain::{
        DownloadEvent, DownloadPlan, DownloadSource, DownloadTask, OutputSpec, PlanSource,
        SCHEMA_VERSION,
    },
    downloader::{ExecuteOptions, execute_plan},
};
use tempfile::tempdir;
use time::OffsetDateTime;
use tokio::{net::TcpListener, task::JoinHandle};

#[tokio::test]
async fn retries_timed_out_source_then_uses_fallback() {
    let server = FailureServer::start().await;
    let output = tempdir().unwrap();
    let summary = execute_plan(
        DownloadPlan {
            schema_version: SCHEMA_VERSION,
            created_at: OffsetDateTime::now_utc(),
            expires_at: None,
            source: PlanSource {
                platform: "fixture".to_owned(),
                media_id: "timeout-fallback".to_owned(),
            },
            tasks: vec![DownloadTask::File {
                id: "timeout-fallback".to_owned(),
                sources: vec![
                    DownloadSource {
                        url: server.url("/slow"),
                        headers: Default::default(),
                        auth_ref: None,
                    },
                    DownloadSource {
                        url: server.url("/fast"),
                        headers: Default::default(),
                        auth_ref: None,
                    },
                ],
                output: OutputSpec {
                    relative_path: "recovered.bin".to_owned(),
                },
            }],
        },
        ExecuteOptions {
            output_root: output.path().to_path_buf(),
            timeout: Duration::from_millis(20),
            ..ExecuteOptions::default()
        },
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();

    assert!(summary.success);
    assert!(server.slow_calls.load(Ordering::Relaxed) >= 3);
    assert_eq!(
        tokio::fs::read(output.path().join("recovered.bin"))
            .await
            .unwrap(),
        b"fallback"
    );
    assert!(!output.path().join("recovered.bin.part").exists());
    assert!(!output.path().join("recovered.bin.easydl.json").exists());
}

#[derive(Clone)]
struct FailureState {
    slow_calls: Arc<AtomicUsize>,
}

struct FailureServer {
    base: String,
    slow_calls: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl FailureServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let slow_calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/slow",
                get(|State(state): State<FailureState>| async move {
                    state.slow_calls.fetch_add(1, Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    "too late"
                }),
            )
            .route("/fast", get(|| async { "fallback" }))
            .with_state(FailureState {
                slow_calls: Arc::clone(&slow_calls),
            });
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base,
            slow_calls,
            task,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

impl Drop for FailureServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
