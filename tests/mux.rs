use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
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
        DownloadEvent, DownloadPlan, DownloadSource, DownloadTask, MuxFormat, MuxTrack,
        MuxTrackKind, OutputSpec, PlanSource, SCHEMA_VERSION,
    },
    downloader::{CollisionPolicy, ExecuteOptions, execute_plan},
};
use tempfile::tempdir;
use time::OffsetDateTime;
use tokio::{net::TcpListener, task::JoinHandle};

#[cfg(unix)]
#[tokio::test]
async fn downloads_tracks_concurrently_and_invokes_controlled_ffmpeg() {
    let server = TrackServer::start().await;
    let output = tempdir().unwrap();
    let ffmpeg = output.path().join("fake-ffmpeg.sh");
    write_executable(
        &ffmpeg,
        r#"#!/bin/sh
base=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
printf '%s\n' "$@" > "$base/ffmpeg-args.txt"
for last do :; done
printf muxed > "$last"
"#,
    )
    .await;

    let plan = mux_plan(&server.base);
    let summary = execute_plan(
        plan.clone(),
        options(output.path(), ffmpeg.clone()),
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();

    assert!(summary.success);
    assert_eq!(summary.artifacts.len(), 1);
    assert_eq!(
        tokio::fs::read(output.path().join("merged.mp4"))
            .await
            .unwrap(),
        b"muxed"
    );
    assert!(server.max_in_flight() >= 2);
    assert_eq!(server.hits("video"), 1);
    assert_eq!(server.hits("audio"), 1);
    assert!(!output.path().join("merged.mp4.parts").exists());

    let args = tokio::fs::read_to_string(output.path().join("ffmpeg-args.txt"))
        .await
        .unwrap();
    let args = args.lines().collect::<Vec<_>>();
    assert!(args.contains(&"-nostdin"));
    assert!(args.contains(&"0:v:0"));
    assert!(args.contains(&"1:a:0"));
    assert!(args.windows(2).any(|pair| pair == ["-c", "copy"]));
    assert!(
        args.windows(2)
            .any(|pair| pair == ["-movflags", "+faststart"])
    );
    assert!(args.windows(2).any(|pair| pair == ["-f", "mp4"]));
    assert!(args.iter().all(|arg| !arg.starts_with("http://")));

    let skipped = execute_plan(
        plan,
        options(output.path(), ffmpeg),
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();
    assert!(skipped.success);
    assert_eq!(skipped.skipped.len(), 1);
    assert_eq!(server.hits("video"), 1);
    assert_eq!(server.hits("audio"), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn preserves_downloaded_tracks_after_ffmpeg_failure() {
    let server = TrackServer::start().await;
    let output = tempdir().unwrap();
    let ffmpeg = output.path().join("fake-ffmpeg.sh");
    write_executable(&ffmpeg, "#!/bin/sh\necho intentional failure >&2\nexit 7\n").await;
    let plan = mux_plan(&server.base);

    let failed = execute_plan(
        plan.clone(),
        options(output.path(), ffmpeg.clone()),
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();

    assert!(!failed.success);
    assert_eq!(failed.failures.len(), 1);
    assert_eq!(failed.failures[0].task_id, "mux");
    assert_eq!(failed.failures[0].error.code, "ffmpeg.failed");
    assert_eq!(failed.failures[0].error.context["exitCode"], 7);
    assert!(
        failed.failures[0]
            .error
            .message
            .contains("intentional failure")
    );
    assert!(!output.path().join("merged.mp4").exists());
    assert!(output.path().join("merged.mp4.parts").is_dir());
    assert_eq!(server.hits("video"), 1);
    assert_eq!(server.hits("audio"), 1);

    write_executable(
        &ffmpeg,
        "#!/bin/sh\nfor last do :; done\nprintf recovered > \"$last\"\n",
    )
    .await;
    let recovered = execute_plan(
        plan,
        options(output.path(), ffmpeg),
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();

    assert!(recovered.success);
    assert_eq!(
        tokio::fs::read(output.path().join("merged.mp4"))
            .await
            .unwrap(),
        b"recovered"
    );
    assert_eq!(server.hits("video"), 1);
    assert_eq!(server.hits("audio"), 1);
}

#[tokio::test]
async fn reports_missing_ffmpeg_with_a_stable_error_code() {
    let server = TrackServer::start().await;
    let output = tempdir().unwrap();
    let missing = output.path().join("definitely-missing-ffmpeg");

    let summary = execute_plan(
        mux_plan(&server.base),
        options(output.path(), missing),
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();

    assert!(!summary.success);
    assert_eq!(summary.failures.len(), 1);
    assert_eq!(summary.failures[0].task_id, "mux");
    assert_eq!(summary.failures[0].error.code, "ffmpeg.not_found");
    assert_eq!(server.hits("video"), 0);
    assert_eq!(server.hits("audio"), 0);
    assert!(!output.path().join("merged.mp4").exists());
}

fn mux_plan(base: &str) -> DownloadPlan {
    DownloadPlan {
        schema_version: SCHEMA_VERSION,
        created_at: OffsetDateTime::now_utc(),
        expires_at: None,
        source: PlanSource {
            platform: "fixture".to_owned(),
            media_id: "mux".to_owned(),
        },
        tasks: vec![DownloadTask::Mux {
            id: "mux".to_owned(),
            tracks: vec![
                mux_track("video", MuxTrackKind::Video, &format!("{base}/video")),
                mux_track("audio", MuxTrackKind::Audio, &format!("{base}/audio")),
            ],
            format: MuxFormat::Mp4,
            output: OutputSpec {
                relative_path: "merged.mp4".to_owned(),
            },
        }],
    }
}

fn mux_track(id: &str, kind: MuxTrackKind, url: &str) -> MuxTrack {
    MuxTrack {
        id: id.to_owned(),
        kind,
        sources: vec![DownloadSource {
            url: url.to_owned(),
            headers: Default::default(),
            auth_ref: None,
        }],
    }
}

fn options(output_root: &std::path::Path, ffmpeg: std::path::PathBuf) -> ExecuteOptions {
    ExecuteOptions {
        output_root: output_root.to_path_buf(),
        collision: CollisionPolicy::Skip,
        proxy: None,
        ffmpeg,
        timeout: Duration::from_secs(5),
        item_concurrency: 2,
        chunk_concurrency: 1,
        multipart_threshold: 32 * 1024 * 1024,
        chunk_size: 8 * 1024 * 1024,
        ..ExecuteOptions::default()
    }
}

#[cfg(unix)]
async fn write_executable(path: &std::path::Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    tokio::fs::write(path, contents).await.unwrap();
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .await
        .unwrap();
}

#[derive(Clone)]
struct TrackState {
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
    hits: Arc<Mutex<BTreeMap<String, usize>>>,
}

struct TrackServer {
    base: String,
    state: TrackState,
    task: JoinHandle<()>,
}

impl TrackServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = TrackState {
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_in_flight: Arc::new(AtomicUsize::new(0)),
            hits: Arc::new(Mutex::new(BTreeMap::new())),
        };
        let app = Router::new()
            .route("/{track}", get(track))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, state, task }
    }

    fn max_in_flight(&self) -> usize {
        self.state.max_in_flight.load(Ordering::SeqCst)
    }

    fn hits(&self, track: &str) -> usize {
        *self.state.hits.lock().unwrap().get(track).unwrap_or(&0)
    }
}

impl Drop for TrackServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn track(
    State(state): State<TrackState>,
    AxumPath(track): AxumPath<String>,
) -> Response<Body> {
    *state.hits.lock().unwrap().entry(track.clone()).or_default() += 1;
    let current = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    state.max_in_flight.fetch_max(current, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(50)).await;
    state.in_flight.fetch_sub(1, Ordering::SeqCst);

    let body: Option<&'static [u8]> = match track.as_str() {
        "video" => Some(b"video-track"),
        "audio" => Some(b"audio-track"),
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
