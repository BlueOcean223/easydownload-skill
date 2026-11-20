use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
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
    downloader::{CollisionPolicy, EventSink, ExecuteOptions, execute_plan},
};
use futures_util::{StreamExt, stream};
use tempfile::tempdir;
use time::OffsetDateTime;
use tokio::{net::TcpListener, task::JoinHandle, time::timeout};

#[tokio::test]
async fn resumes_a_validated_partial_with_range_and_if_range() {
    let server = ResumeServer::start(b"0123456789abcdef", "\"v1\"").await;
    let output = tempdir().unwrap();
    let plan = file_plan(&server.url());
    let options = execute_options(output.path());

    server.set_stall(true);
    leave_interrupted_partial(plan.clone(), options.clone(), output.path()).await;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(output.path().join("resume.bin.easydl.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
    server.set_stall(false);

    let events = Arc::new(Mutex::new(Vec::<DownloadEvent>::new()));
    let captured = Arc::clone(&events);
    let sink: Arc<dyn EventSink> = Arc::new(move |event: &DownloadEvent| {
        captured.lock().unwrap().push(event.clone());
    });
    let summary = execute_plan(plan, options, sink).await.unwrap();

    assert!(summary.success);
    assert_eq!(
        tokio::fs::read(output.path().join("resume.bin"))
            .await
            .unwrap(),
        b"0123456789abcdef"
    );
    assert!(!output.path().join("resume.bin.part").exists());
    assert!(!output.path().join("resume.bin.easydl.json").exists());
    assert!(server.ranges().iter().any(|value| value == "bytes=4-"));
    assert!(server.if_ranges().iter().any(|value| value == "\"v1\""));
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event.event, DownloadEventKind::Reset { .. }))
    );
}

#[tokio::test]
async fn restarts_an_unverified_partial_instead_of_blindly_appending() {
    let server = ResumeServer::start(b"0123456789abcdef", "\"v1\"").await;
    let output = tempdir().unwrap();
    tokio::fs::write(output.path().join("resume.bin.part"), b"stale")
        .await
        .unwrap();

    let events = Arc::new(Mutex::new(Vec::<DownloadEvent>::new()));
    let captured = Arc::clone(&events);
    let summary = execute_plan(
        file_plan(&server.url()),
        execute_options(output.path()),
        Arc::new(move |event: &DownloadEvent| {
            captured.lock().unwrap().push(event.clone());
        }),
    )
    .await
    .unwrap();

    assert!(summary.success);
    assert_eq!(
        tokio::fs::read(output.path().join("resume.bin"))
            .await
            .unwrap(),
        b"0123456789abcdef"
    );
    assert!(events.lock().unwrap().iter().any(|event| {
        matches!(
            &event.event,
            DownloadEventKind::Reset { reason, .. } if reason.code == "resume.metadata_missing"
        )
    }));
}

#[tokio::test]
async fn resets_partial_when_remote_identity_changes() {
    let server = ResumeServer::start(b"0123456789abcdef", "\"v1\"").await;
    let output = tempdir().unwrap();
    let plan = file_plan(&server.url());
    let options = execute_options(output.path());

    server.set_stall(true);
    leave_interrupted_partial(plan.clone(), options.clone(), output.path()).await;
    server.replace(b"fedcba9876543210", "\"v2\"");
    server.set_stall(false);

    let events = Arc::new(Mutex::new(Vec::<DownloadEvent>::new()));
    let captured = Arc::clone(&events);
    let sink: Arc<dyn EventSink> = Arc::new(move |event: &DownloadEvent| {
        captured.lock().unwrap().push(event.clone());
    });
    let summary = execute_plan(plan, options, sink).await.unwrap();

    assert!(summary.success);
    assert_eq!(
        tokio::fs::read(output.path().join("resume.bin"))
            .await
            .unwrap(),
        b"fedcba9876543210"
    );
    let resets: Vec<String> = {
        let events = events.lock().unwrap();
        events
            .iter()
            .filter_map(|event| match &event.event {
                DownloadEventKind::Reset { reason, .. } => Some(reason.code.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(resets, ["resume.identity_mismatch"]);
    assert!(!server.ranges().iter().any(|value| value == "bytes=4-"));
}

async fn leave_interrupted_partial(
    plan: DownloadPlan,
    options: ExecuteOptions,
    output_root: &std::path::Path,
) {
    let handle = tokio::spawn(execute_plan(
        plan,
        options,
        Arc::new(|_: &DownloadEvent| {}),
    ));
    let part = output_root.join("resume.bin.part");
    let sidecar = output_root.join("resume.bin.easydl.json");
    timeout(Duration::from_secs(5), async {
        loop {
            let length = tokio::fs::metadata(&part)
                .await
                .map(|metadata| metadata.len())
                .unwrap_or_default();
            if length == 4 && sidecar.is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("partial and sidecar should be created");
    handle.abort();
    let _ = handle.await;
}

fn file_plan(url: &str) -> DownloadPlan {
    DownloadPlan {
        schema_version: SCHEMA_VERSION,
        created_at: OffsetDateTime::now_utc(),
        expires_at: None,
        source: PlanSource {
            platform: "fixture".to_owned(),
            media_id: "resume".to_owned(),
        },
        tasks: vec![DownloadTask::File {
            id: "resume".to_owned(),
            sources: vec![DownloadSource {
                url: url.to_owned(),
                headers: Default::default(),
                auth_ref: None,
            }],
            output: OutputSpec {
                relative_path: "resume.bin".to_owned(),
            },
        }],
    }
}

fn execute_options(output_root: &std::path::Path) -> ExecuteOptions {
    ExecuteOptions {
        output_root: output_root.to_path_buf(),
        collision: CollisionPolicy::Skip,
        proxy: None,
        timeout: Duration::from_secs(10),
        ..ExecuteOptions::default()
    }
}

#[derive(Clone)]
struct ResumeState {
    inner: Arc<Mutex<Resource>>,
}

struct Resource {
    body: Vec<u8>,
    etag: String,
    stall: bool,
    ranges: Vec<String>,
    if_ranges: Vec<String>,
}

struct ResumeServer {
    base: String,
    state: ResumeState,
    task: JoinHandle<()>,
}

impl ResumeServer {
    async fn start(body: &[u8], etag: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = ResumeState {
            inner: Arc::new(Mutex::new(Resource {
                body: body.to_vec(),
                etag: etag.to_owned(),
                stall: false,
                ranges: Vec::new(),
                if_ranges: Vec::new(),
            })),
        };
        let app = Router::new()
            .route("/resource.bin", get(resource))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, state, task }
    }

    fn url(&self) -> String {
        format!("{}/resource.bin", self.base)
    }

    fn set_stall(&self, stall: bool) {
        self.state.inner.lock().unwrap().stall = stall;
    }

    fn replace(&self, body: &[u8], etag: &str) {
        let mut resource = self.state.inner.lock().unwrap();
        resource.body = body.to_vec();
        resource.etag = etag.to_owned();
    }

    fn ranges(&self) -> Vec<String> {
        self.state.inner.lock().unwrap().ranges.clone()
    }

    fn if_ranges(&self) -> Vec<String> {
        self.state.inner.lock().unwrap().if_ranges.clone()
    }
}

impl Drop for ResumeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn resource(State(state): State<ResumeState>, headers: HeaderMap) -> Response<Body> {
    let range = headers
        .get("range")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let if_range = headers
        .get("if-range")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let (body, etag, stall) = {
        let mut resource = state.inner.lock().unwrap();
        if let Some(range) = range.as_ref() {
            resource.ranges.push(range.clone());
        }
        if let Some(if_range) = if_range {
            resource.if_ranges.push(if_range);
        }
        (resource.body.clone(), resource.etag.clone(), resource.stall)
    };

    if let Some(range) = range {
        let (start, end) = parse_range(&range, body.len());
        let chunk = body[start..=end].to_vec();
        return Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header("accept-ranges", "bytes")
            .header("content-length", chunk.len())
            .header(
                "content-range",
                format!("bytes {start}-{end}/{}", body.len()),
            )
            .header("etag", etag)
            .header("last-modified", "Wed, 30 Jul 2026 00:00:00 GMT")
            .body(Body::from(chunk))
            .unwrap();
    }

    if stall {
        let first = body[..4].to_vec();
        let chunks = stream::once(async move { Ok::<Bytes, Infallible>(Bytes::from(first)) })
            .chain(stream::pending::<Result<Bytes, Infallible>>());
        return Response::builder()
            .status(StatusCode::OK)
            .header("accept-ranges", "bytes")
            .header("content-length", body.len())
            .header("etag", etag)
            .header("last-modified", "Wed, 30 Jul 2026 00:00:00 GMT")
            .body(Body::from_stream(chunks))
            .unwrap();
    }

    Response::builder()
        .status(StatusCode::OK)
        .header("accept-ranges", "bytes")
        .header("content-length", body.len())
        .header("etag", etag)
        .header("last-modified", "Wed, 30 Jul 2026 00:00:00 GMT")
        .body(Body::from(body))
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
