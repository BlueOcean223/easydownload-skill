use std::{
    convert::Infallible,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Response, StatusCode},
    routing::get,
};
use easydl::{
    domain::{
        DownloadEvent, DownloadEventKind, DownloadPlan, DownloadSource, DownloadTask, OutputSpec,
        PlanSource, SCHEMA_VERSION,
    },
    downloader::{CollisionPolicy, ExecuteOptions, execute_plan},
};
use futures_util::{StreamExt, stream};
use tempfile::tempdir;
use time::OffsetDateTime;
use tokio::{net::TcpListener, task::JoinHandle, time::timeout};

const CHUNK_SIZE: u64 = 8 * 1024;

#[tokio::test]
async fn downloads_chunks_concurrently_and_concatenates_in_order() {
    let body = fixture_body(64 * 1024);
    let server = MultipartServer::start(body.clone()).await;
    let output = tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::<DownloadEvent>::new()));
    let captured = Arc::clone(&events);

    let summary = execute_plan(
        file_plan(&server.url()),
        multipart_options(output.path()),
        Arc::new(move |event: &DownloadEvent| {
            captured.lock().unwrap().push(event.clone());
        }),
    )
    .await
    .unwrap();

    assert!(summary.success);
    assert_eq!(
        tokio::fs::read(output.path().join("multipart.bin"))
            .await
            .unwrap(),
        body
    );
    assert!(server.max_in_flight() > 1);
    assert!(server.ranges().len() > 2);
    assert!(!output.path().join("multipart.bin.parts").exists());
    assert!(!output.path().join("multipart.bin.easydl.json").exists());

    let progress: Vec<u64> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event.event {
            DownloadEventKind::Progress { downloaded, .. } => Some(downloaded),
            _ => None,
        })
        .collect();
    assert!(progress.windows(2).all(|pair| pair[0] <= pair[1]));
    assert_eq!(progress.last().copied(), Some(64 * 1024));
}

#[tokio::test]
async fn resumes_only_missing_multipart_chunks_after_interruption() {
    let body = fixture_body(64 * 1024);
    let server = MultipartServer::start(body.clone()).await;
    let output = tempdir().unwrap();
    server.set_mode(ServerMode::StallFrom(2 * CHUNK_SIZE));

    let handle = tokio::spawn(execute_plan(
        file_plan(&server.url()),
        multipart_options(output.path()),
        Arc::new(|_: &DownloadEvent| {}),
    ));
    let parts_dir = output.path().join("multipart.bin.parts");
    timeout(Duration::from_secs(5), async {
        loop {
            if completed_chunks(&parts_dir).await >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("at least two chunks should complete before interruption");
    handle.abort();
    let _ = handle.await;

    server.set_mode(ServerMode::Normal);
    server.clear_ranges();
    let summary = execute_plan(
        file_plan(&server.url()),
        multipart_options(output.path()),
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();

    assert!(summary.success);
    assert_eq!(
        tokio::fs::read(output.path().join("multipart.bin"))
            .await
            .unwrap(),
        body
    );
    let ranges = server.ranges();
    assert!(ranges.iter().any(|value| value == "bytes=0-0"));
    assert!(!ranges.iter().any(|value| value == "bytes=0-8191"));
    assert!(!ranges.iter().any(|value| value == "bytes=8192-16383"));
    assert!(ranges.iter().any(|value| value == "bytes=16384-24575"));
}

#[tokio::test]
async fn falls_back_to_sequential_when_server_ignores_chunk_ranges() {
    let body = fixture_body(32 * 1024);
    let server = MultipartServer::start(body.clone()).await;
    server.set_mode(ServerMode::IgnoreChunkRanges);
    let output = tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::<DownloadEvent>::new()));
    let captured = Arc::clone(&events);

    let summary = execute_plan(
        file_plan(&server.url()),
        multipart_options(output.path()),
        Arc::new(move |event: &DownloadEvent| {
            captured.lock().unwrap().push(event.clone());
        }),
    )
    .await
    .unwrap();

    assert!(summary.success);
    assert_eq!(
        tokio::fs::read(output.path().join("multipart.bin"))
            .await
            .unwrap(),
        body
    );
    assert!(events.lock().unwrap().iter().any(|event| {
        matches!(
            &event.event,
            DownloadEventKind::Reset { reason, .. }
                if reason.code == "multipart.range_unsupported"
        )
    }));
}

fn file_plan(url: &str) -> DownloadPlan {
    DownloadPlan {
        schema_version: SCHEMA_VERSION,
        created_at: OffsetDateTime::now_utc(),
        expires_at: None,
        source: PlanSource {
            platform: "fixture".to_owned(),
            media_id: "multipart".to_owned(),
        },
        tasks: vec![DownloadTask::File {
            id: "multipart".to_owned(),
            sources: vec![DownloadSource {
                url: url.to_owned(),
                headers: Default::default(),
                auth_ref: None,
            }],
            output: OutputSpec {
                relative_path: "multipart.bin".to_owned(),
            },
        }],
    }
}

fn multipart_options(output_root: &std::path::Path) -> ExecuteOptions {
    ExecuteOptions {
        output_root: output_root.to_path_buf(),
        collision: CollisionPolicy::Skip,
        proxy: None,
        ffmpeg: "ffmpeg".into(),
        timeout: Duration::from_secs(10),
        item_concurrency: 4,
        chunk_concurrency: 3,
        multipart_threshold: 1024,
        chunk_size: CHUNK_SIZE,
        ..ExecuteOptions::default()
    }
}

async fn completed_chunks(path: &std::path::Path) -> usize {
    let mut entries = match tokio::fs::read_dir(path).await {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    let mut count = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_name().to_string_lossy().ends_with(".part") {
            count += 1;
        }
    }
    count
}

fn fixture_body(size: usize) -> Vec<u8> {
    (0..size).map(|index| (index % 251) as u8).collect()
}

#[derive(Debug, Clone, Copy)]
enum ServerMode {
    Normal,
    StallFrom(u64),
    IgnoreChunkRanges,
}

#[derive(Clone)]
struct MultipartState {
    body: Arc<Vec<u8>>,
    mode: Arc<Mutex<ServerMode>>,
    ranges: Arc<Mutex<Vec<String>>>,
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
}

struct MultipartServer {
    base: String,
    state: MultipartState,
    task: JoinHandle<()>,
}

impl MultipartServer {
    async fn start(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = MultipartState {
            body: Arc::new(body),
            mode: Arc::new(Mutex::new(ServerMode::Normal)),
            ranges: Arc::new(Mutex::new(Vec::new())),
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_in_flight: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/large.bin", get(large_file))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, state, task }
    }

    fn url(&self) -> String {
        format!("{}/large.bin", self.base)
    }

    fn set_mode(&self, mode: ServerMode) {
        *self.state.mode.lock().unwrap() = mode;
    }

    fn ranges(&self) -> Vec<String> {
        self.state.ranges.lock().unwrap().clone()
    }

    fn clear_ranges(&self) {
        self.state.ranges.lock().unwrap().clear();
    }

    fn max_in_flight(&self) -> usize {
        self.state.max_in_flight.load(Ordering::SeqCst)
    }
}

impl Drop for MultipartServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn large_file(State(state): State<MultipartState>, headers: HeaderMap) -> Response<Body> {
    let range = headers
        .get("range")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if let Some(range) = range.as_ref() {
        state.ranges.lock().unwrap().push(range.clone());
    }
    let mode = *state.mode.lock().unwrap();
    let body = Arc::clone(&state.body);

    let Some(range) = range else {
        return full_response(&body);
    };
    let (start, end) = parse_range(&range, body.len());
    if range == "bytes=0-0" {
        return partial_response(&body, start, end, Body::from(body[start..=end].to_vec()));
    }
    if matches!(mode, ServerMode::IgnoreChunkRanges) {
        return full_response(&body);
    }

    let in_flight = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    state.max_in_flight.fetch_max(in_flight, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(25)).await;
    state.in_flight.fetch_sub(1, Ordering::SeqCst);

    if matches!(mode, ServerMode::StallFrom(offset) if start as u64 >= offset) {
        let first = Bytes::copy_from_slice(&body[start..=start]);
        let chunks = stream::once(async move { Ok::<Bytes, Infallible>(first) })
            .chain(stream::pending::<Result<Bytes, Infallible>>());
        return partial_response(&body, start, end, Body::from_stream(chunks));
    }
    partial_response(&body, start, end, Body::from(body[start..=end].to_vec()))
}

fn full_response(body: &[u8]) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header("accept-ranges", "bytes")
        .header("content-length", body.len())
        .header("etag", "\"multipart-v1\"")
        .header("last-modified", "Wed, 30 Jul 2026 00:00:00 GMT")
        .body(Body::from(body.to_vec()))
        .unwrap()
}

fn partial_response(body: &[u8], start: usize, end: usize, response: Body) -> Response<Body> {
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header("accept-ranges", "bytes")
        .header("content-length", end - start + 1)
        .header(
            "content-range",
            format!("bytes {start}-{end}/{}", body.len()),
        )
        .header("etag", "\"multipart-v1\"")
        .header("last-modified", "Wed, 30 Jul 2026 00:00:00 GMT")
        .body(response)
        .unwrap()
}

fn parse_range(value: &str, total: usize) -> (usize, usize) {
    let value = value.strip_prefix("bytes=").unwrap();
    let (start, end) = value.split_once('-').unwrap();
    let start: usize = start.parse().unwrap();
    let end = if end.is_empty() {
        total - 1
    } else {
        end.parse::<usize>().unwrap().min(total - 1)
    };
    (start, end)
}
