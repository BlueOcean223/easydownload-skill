use std::{
    collections::BTreeMap,
    io::Read,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    extract::{Path as AxumPath, State},
    http::{Response, StatusCode},
    routing::get,
};
use easydl::{
    domain::{
        ArchiveEntry, DownloadEvent, DownloadEventKind, DownloadPlan, DownloadSource, DownloadTask,
        OutputSpec, PlanSource, SCHEMA_VERSION,
    },
    downloader::{CollisionPolicy, EventSink, ExecuteOptions, execute_plan},
};
use tempfile::tempdir;
use time::OffsetDateTime;
use tokio::{net::TcpListener, task::JoinHandle};
use zip::ZipArchive;

#[tokio::test]
async fn creates_partial_archive_and_resumes_missing_entries() {
    let server = ArchiveServer::start().await;
    let output = tempdir().unwrap();
    let plan = archive_plan(&server.base);
    let events = Arc::new(Mutex::new(Vec::<DownloadEvent>::new()));
    let captured = Arc::clone(&events);
    let sink: Arc<dyn EventSink> = Arc::new(move |event: &DownloadEvent| {
        captured.lock().unwrap().push(event.clone());
    });

    let partial = execute_plan(plan.clone(), options(output.path()), sink)
        .await
        .unwrap();

    assert!(!partial.success);
    assert_eq!(partial.artifacts.len(), 1);
    assert!(partial.artifacts[0].partial);
    assert_eq!(partial.failures.len(), 1, "{:#?}", partial.failures);
    assert_eq!(partial.failures[0].task_id, "album:missing");
    assert_eq!(
        partial.failures[0].error.context["entryPath"],
        "missing.txt"
    );
    assert_eq!(
        read_archive(&output.path().join("album.zip")),
        BTreeMap::from([
            ("a.txt".to_owned(), b"aaa".to_vec()),
            ("folder/b.txt".to_owned(), b"bbb".to_vec()),
        ])
    );
    assert!(output.path().join("album.zip.parts").is_dir());
    assert!(output.path().join("album.zip.easydl.json").is_file());
    {
        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            &event.event,
            DownloadEventKind::Error { task_id, .. } if task_id == "album:missing"
        )));
        assert!(events.iter().any(|event| matches!(
            event.event,
            DownloadEventKind::Artifact { partial: true, .. }
        )));
        assert!(matches!(
            events.last().unwrap().event,
            DownloadEventKind::Done { success: false, .. }
        ));
    }
    assert_eq!(server.hits("a.txt"), 1);
    assert_eq!(server.hits("b.txt"), 1);
    assert_eq!(server.hits("missing.txt"), 1);

    server.enable_missing();
    let completed = execute_plan(
        plan.clone(),
        options(output.path()),
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();

    assert!(completed.success);
    assert_eq!(completed.artifacts.len(), 1);
    assert!(!completed.artifacts[0].partial);
    assert!(completed.failures.is_empty());
    assert_eq!(
        read_archive(&output.path().join("album.zip")),
        BTreeMap::from([
            ("a.txt".to_owned(), b"aaa".to_vec()),
            ("folder/b.txt".to_owned(), b"bbb".to_vec()),
            ("missing.txt".to_owned(), b"now-present".to_vec()),
        ])
    );
    assert!(!output.path().join("album.zip.parts").exists());
    assert!(!output.path().join("album.zip.easydl.json").exists());
    assert_eq!(server.hits("a.txt"), 1);
    assert_eq!(server.hits("b.txt"), 1);
    assert_eq!(server.hits("missing.txt"), 2);

    let skipped = execute_plan(
        plan,
        options(output.path()),
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();
    assert!(skipped.success);
    assert_eq!(skipped.skipped.len(), 1);
    assert_eq!(server.hits("a.txt"), 1);
    assert_eq!(server.hits("b.txt"), 1);
    assert_eq!(server.hits("missing.txt"), 2);
}

#[tokio::test]
async fn does_not_create_an_empty_archive_when_every_entry_fails() {
    let server = ArchiveServer::start().await;
    let output = tempdir().unwrap();
    let plan = DownloadPlan {
        schema_version: SCHEMA_VERSION,
        created_at: OffsetDateTime::now_utc(),
        expires_at: None,
        source: PlanSource {
            platform: "fixture".to_owned(),
            media_id: "empty-archive".to_owned(),
        },
        tasks: vec![DownloadTask::Archive {
            id: "empty".to_owned(),
            entries: vec![archive_entry(
                "missing",
                "missing.txt",
                &format!("{}/missing.txt", server.base),
            )],
            output: OutputSpec {
                relative_path: "empty.zip".to_owned(),
            },
        }],
    };

    let summary = execute_plan(
        plan,
        options(output.path()),
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();

    assert!(!summary.success);
    assert!(summary.artifacts.is_empty());
    assert_eq!(summary.failures.len(), 1);
    assert!(!output.path().join("empty.zip").exists());
}

fn archive_plan(base: &str) -> DownloadPlan {
    DownloadPlan {
        schema_version: SCHEMA_VERSION,
        created_at: OffsetDateTime::now_utc(),
        expires_at: None,
        source: PlanSource {
            platform: "fixture".to_owned(),
            media_id: "archive".to_owned(),
        },
        tasks: vec![DownloadTask::Archive {
            id: "album".to_owned(),
            entries: vec![
                archive_entry("a", "a.txt", &format!("{base}/a.txt")),
                archive_entry("b", "folder/b.txt", &format!("{base}/b.txt")),
                archive_entry("missing", "missing.txt", &format!("{base}/missing.txt")),
            ],
            output: OutputSpec {
                relative_path: "album.zip".to_owned(),
            },
        }],
    }
}

fn archive_entry(id: &str, path: &str, url: &str) -> ArchiveEntry {
    ArchiveEntry {
        id: id.to_owned(),
        path: path.to_owned(),
        sources: vec![DownloadSource {
            url: url.to_owned(),
            headers: Default::default(),
            auth_ref: None,
        }],
    }
}

fn options(output_root: &std::path::Path) -> ExecuteOptions {
    ExecuteOptions {
        output_root: output_root.to_path_buf(),
        collision: CollisionPolicy::Skip,
        proxy: None,
        ffmpeg: "ffmpeg".into(),
        timeout: Duration::from_secs(5),
        item_concurrency: 3,
        chunk_concurrency: 1,
        multipart_threshold: 32 * 1024 * 1024,
        chunk_size: 8 * 1024 * 1024,
        ..ExecuteOptions::default()
    }
}

fn read_archive(path: &std::path::Path) -> BTreeMap<String, Vec<u8>> {
    let file = std::fs::File::open(path).unwrap();
    let mut archive = ZipArchive::new(file).unwrap();
    let mut entries = BTreeMap::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        entries.insert(entry.name().to_owned(), bytes);
    }
    entries
}

#[derive(Clone)]
struct ArchiveState {
    missing_available: Arc<AtomicBool>,
    hits: Arc<Mutex<BTreeMap<String, usize>>>,
}

struct ArchiveServer {
    base: String,
    state: ArchiveState,
    task: JoinHandle<()>,
}

impl ArchiveServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = ArchiveState {
            missing_available: Arc::new(AtomicBool::new(false)),
            hits: Arc::new(Mutex::new(BTreeMap::new())),
        };
        let app = Router::new()
            .route("/{name}", get(resource))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, state, task }
    }

    fn enable_missing(&self) {
        self.state.missing_available.store(true, Ordering::SeqCst);
    }

    fn hits(&self, name: &str) -> usize {
        *self.state.hits.lock().unwrap().get(name).unwrap_or(&0)
    }
}

impl Drop for ArchiveServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn resource(
    State(state): State<ArchiveState>,
    AxumPath(name): AxumPath<String>,
) -> Response<Body> {
    *state.hits.lock().unwrap().entry(name.clone()).or_default() += 1;
    let body: Option<&'static [u8]> = match name.as_str() {
        "a.txt" => Some(b"aaa"),
        "b.txt" => Some(b"bbb"),
        "missing.txt" if state.missing_available.load(Ordering::SeqCst) => Some(b"now-present"),
        _ => None,
    };
    match body {
        Some(body) => Response::builder()
            .status(StatusCode::OK)
            .header("content-length", body.len())
            .body(Body::from(body))
            .unwrap(),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header("content-length", 0)
            .body(Body::empty())
            .unwrap(),
    }
}
