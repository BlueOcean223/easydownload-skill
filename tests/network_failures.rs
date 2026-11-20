use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{Router, body::Body, extract::State, http::Response, routing::get};
use easydl::{
    domain::{
        DownloadEvent, DownloadPlan, DownloadSource, DownloadTask, OutputSpec, PlanSource,
        SCHEMA_VERSION,
    },
    downloader::{ExecuteOptions, execute_plan},
};
use futures_util::stream;
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

#[tokio::test]
async fn allows_active_stream_longer_than_idle_read_timeout() {
    let server = FailureServer::start().await;
    let output = tempdir().unwrap();
    let summary = execute_plan(
        DownloadPlan {
            schema_version: SCHEMA_VERSION,
            created_at: OffsetDateTime::now_utc(),
            expires_at: None,
            source: PlanSource {
                platform: "fixture".to_owned(),
                media_id: "steady-stream".to_owned(),
            },
            tasks: vec![DownloadTask::File {
                id: "steady-stream".to_owned(),
                sources: vec![DownloadSource {
                    url: server.url("/steady"),
                    headers: Default::default(),
                    auth_ref: None,
                }],
                output: OutputSpec {
                    relative_path: "steady.bin".to_owned(),
                },
            }],
        },
        ExecuteOptions {
            output_root: output.path().to_path_buf(),
            timeout: Duration::from_millis(100),
            chunk_concurrency: 1,
            ..ExecuteOptions::default()
        },
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();

    assert!(summary.success, "failures: {:?}", summary.failures);
    assert_eq!(
        tokio::fs::read(output.path().join("steady.bin"))
            .await
            .unwrap(),
        b"01234"
    );
}

#[tokio::test]
async fn times_out_when_stream_stalls_mid_transfer() {
    let server = FailureServer::start().await;
    let output = tempdir().unwrap();
    let summary = execute_plan(
        DownloadPlan {
            schema_version: SCHEMA_VERSION,
            created_at: OffsetDateTime::now_utc(),
            expires_at: None,
            source: PlanSource {
                platform: "fixture".to_owned(),
                media_id: "stalled-stream".to_owned(),
            },
            tasks: vec![DownloadTask::File {
                id: "stalled-stream".to_owned(),
                sources: vec![DownloadSource {
                    url: server.url("/stalled"),
                    headers: Default::default(),
                    auth_ref: None,
                }],
                output: OutputSpec {
                    relative_path: "stalled.bin".to_owned(),
                },
            }],
        },
        ExecuteOptions {
            output_root: output.path().to_path_buf(),
            timeout: Duration::from_millis(50),
            chunk_concurrency: 1,
            ..ExecuteOptions::default()
        },
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();

    assert!(!summary.success);
    assert_eq!(summary.failures.len(), 1);
    assert_eq!(summary.failures[0].error.code, "download.sources_exhausted");
    assert_eq!(
        summary.failures[0].error.context["lastCode"],
        "http.timeout"
    );
    assert!(!output.path().join("stalled.bin").exists());
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
            .route(
                "/steady",
                get(|| async {
                    let body = Body::from_stream(stream::unfold(0_u8, |index| async move {
                        if index >= 5 {
                            return None;
                        }
                        if index > 0 {
                            tokio::time::sleep(Duration::from_millis(35)).await;
                        }
                        let chunk = vec![b'0' + index];
                        Some((Ok::<_, Infallible>(chunk), index + 1))
                    }));
                    Response::builder()
                        .header("content-length", "5")
                        .body(body)
                        .unwrap()
                }),
            )
            .route(
                "/stalled",
                get(|| async {
                    let body = Body::from_stream(stream::unfold(0_u8, |stage| async move {
                        match stage {
                            0 => Some((Ok::<_, Infallible>(b"ab".to_vec()), 1)),
                            1 => {
                                tokio::time::sleep(Duration::from_millis(250)).await;
                                Some((Ok::<_, Infallible>(b"cd".to_vec()), 2))
                            }
                            _ => None,
                        }
                    }));
                    Response::builder()
                        .header("content-length", "4")
                        .body(body)
                        .unwrap()
                }),
            )
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
