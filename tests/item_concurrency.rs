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
    downloader::{CollisionPolicy, ExecuteOptions, execute_plan},
};
use tempfile::tempdir;
use time::OffsetDateTime;
use tokio::{net::TcpListener, task::JoinHandle};

#[tokio::test]
async fn respects_item_concurrency_and_keeps_summary_in_plan_order() {
    let server = DelayServer::start().await;

    let serial_output = tempdir().unwrap();
    let serial = execute_plan(
        plan(&server.base),
        options(serial_output.path(), 1),
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();
    assert!(serial.success);
    assert_eq!(server.max_in_flight(), 1);

    server.reset_max();
    let concurrent_output = tempdir().unwrap();
    let concurrent = execute_plan(
        plan(&server.base),
        options(concurrent_output.path(), 2),
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();
    assert!(concurrent.success);
    assert!(server.max_in_flight() >= 2);
    assert_eq!(
        concurrent
            .artifacts
            .iter()
            .map(|artifact| artifact.task_id.as_str())
            .collect::<Vec<_>>(),
        ["one", "two"]
    );
}

fn plan(base: &str) -> DownloadPlan {
    DownloadPlan {
        schema_version: SCHEMA_VERSION,
        created_at: OffsetDateTime::now_utc(),
        expires_at: None,
        source: PlanSource {
            platform: "fixture".to_owned(),
            media_id: "concurrency".to_owned(),
        },
        tasks: vec![
            file_task("one", &format!("{base}/one.bin"), "shared/one.bin"),
            file_task("two", &format!("{base}/two.bin"), "shared/two.bin"),
        ],
    }
}

fn file_task(id: &str, url: &str, path: &str) -> DownloadTask {
    DownloadTask::File {
        id: id.to_owned(),
        sources: vec![DownloadSource {
            url: url.to_owned(),
            headers: Default::default(),
            auth_ref: None,
        }],
        output: OutputSpec {
            relative_path: path.to_owned(),
        },
    }
}

fn options(output_root: &std::path::Path, item_concurrency: usize) -> ExecuteOptions {
    ExecuteOptions {
        output_root: output_root.to_path_buf(),
        collision: CollisionPolicy::Skip,
        proxy: None,
        ffmpeg: "ffmpeg".into(),
        timeout: Duration::from_secs(5),
        item_concurrency,
        chunk_concurrency: 1,
        multipart_threshold: 32 * 1024 * 1024,
        chunk_size: 8 * 1024 * 1024,
        ..ExecuteOptions::default()
    }
}

#[derive(Clone)]
struct DelayState {
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
}

struct DelayServer {
    base: String,
    state: DelayState,
    task: JoinHandle<()>,
}

impl DelayServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = DelayState {
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_in_flight: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/one.bin", get(one))
            .route("/two.bin", get(two))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, state, task }
    }

    fn max_in_flight(&self) -> usize {
        self.state.max_in_flight.load(Ordering::SeqCst)
    }

    fn reset_max(&self) {
        self.state.max_in_flight.store(0, Ordering::SeqCst);
    }
}

impl Drop for DelayServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn one(State(state): State<DelayState>) -> &'static str {
    delayed(state, Duration::from_millis(80), "one").await
}

async fn two(State(state): State<DelayState>) -> &'static str {
    delayed(state, Duration::from_millis(20), "two").await
}

async fn delayed(state: DelayState, delay: Duration, body: &'static str) -> &'static str {
    let current = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    state.max_in_flight.fetch_max(current, Ordering::SeqCst);
    tokio::time::sleep(delay).await;
    state.in_flight.fetch_sub(1, Ordering::SeqCst);
    body
}
