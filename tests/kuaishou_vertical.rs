use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    extract::State,
    response::Redirect,
    routing::{get, post},
};
use easydl::{
    domain::{DownloadEvent, DownloadEventKind, DownloadTask},
    downloader::{CollisionPolicy, EventSink, ExecuteOptions, execute_plan},
    platforms::{PlanSelection, ResolveOptions, kuaishou::KuaishouResolver},
};
use serde_json::json;
use tempfile::tempdir;
use tokio::{net::TcpListener, task::JoinHandle};

#[tokio::test]
async fn kuaishou_video_runs_through_inspect_plan_and_execute() {
    let server = TestServer::start().await;
    let resolver = KuaishouResolver::with_api_url(
        ResolveOptions {
            proxy: None,
            timeout: Duration::from_secs(5),
            ..ResolveOptions::default()
        },
        &format!("{}/rest/wd/ugH5App/photo/simple/info", server.base),
    )
    .unwrap();

    let media = resolver
        .resolve(&format!("分享 {}/share", server.base))
        .await
        .unwrap();
    let inspect = media.inspect();
    assert_eq!(inspect.platform, "kuaishou");
    assert_eq!(inspect.media.title, "Best Goal");
    assert_eq!(inspect.choices.qualities[0].id, "1080p_hevc");
    assert!(inspect.choices.qualities[0].recommended);
    let inspect_json = serde_json::to_string(&inspect).unwrap();
    assert!(!inspect_json.contains("/video-1080.mp4"));
    assert!(!inspect_json.contains("backup-1080.mp4"));

    let plan = media
        .create_plan(&PlanSelection {
            quality: Some("best".to_owned()),
            ..PlanSelection::default()
        })
        .unwrap();
    let DownloadTask::File {
        sources, output, ..
    } = &plan.tasks[0]
    else {
        panic!("kuaishou video should create a file task");
    };
    assert_eq!(sources[0].url, format!("{}/video-1080.mp4", server.base));
    assert_eq!(output.relative_path, "Best Goal.mp4");
    let mut quality_ids = inspect
        .choices
        .qualities
        .iter()
        .map(|quality| quality.id.as_str())
        .collect::<Vec<_>>();
    quality_ids.sort_unstable();
    let normalized = json!({
        "platform": inspect.platform,
        "kind": inspect.media.kind,
        "title": inspect.media.title,
        "qualityIds": quality_ids,
        "tasks": [{ "kind": "file", "output": output.relative_path }],
    });
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/golden/kuaishou-video.json")).unwrap();
    assert_eq!(normalized, expected);

    let output = tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::<DownloadEvent>::new()));
    let captured = Arc::clone(&events);
    let sink: Arc<dyn EventSink> = Arc::new(move |event: &DownloadEvent| {
        captured.lock().unwrap().push(event.clone());
    });
    let summary = execute_plan(
        plan.clone(),
        ExecuteOptions {
            output_root: output.path().to_path_buf(),
            collision: CollisionPolicy::Skip,
            proxy: None,
            timeout: Duration::from_secs(5),
            ..ExecuteOptions::default()
        },
        sink,
    )
    .await
    .unwrap();

    assert!(summary.success);
    assert_eq!(summary.artifacts.len(), 1);
    assert_eq!(
        tokio::fs::read(output.path().join("Best Goal.mp4"))
            .await
            .unwrap(),
        b"best-video"
    );
    {
        let events = events.lock().unwrap();
        assert!(matches!(
            events.first().unwrap().event,
            DownloadEventKind::Start { .. }
        ));
        assert!(matches!(
            events.last().unwrap().event,
            DownloadEventKind::Done { success: true, .. }
        ));
    }

    let skipped = execute_plan(
        plan.clone(),
        ExecuteOptions {
            output_root: output.path().to_path_buf(),
            collision: CollisionPolicy::Skip,
            proxy: None,
            timeout: Duration::from_secs(5),
            ..ExecuteOptions::default()
        },
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();
    assert!(skipped.success);
    assert_eq!(skipped.skipped[0].reason.code, "filesystem.exists");

    let renamed = execute_plan(
        plan.clone(),
        ExecuteOptions {
            output_root: output.path().to_path_buf(),
            collision: CollisionPolicy::Rename,
            proxy: None,
            timeout: Duration::from_secs(5),
            ..ExecuteOptions::default()
        },
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();
    assert!(renamed.success);
    assert!(output.path().join("Best Goal (1).mp4").is_file());

    tokio::fs::write(output.path().join("Best Goal.mp4"), b"stale")
        .await
        .unwrap();
    let overwritten = execute_plan(
        plan,
        ExecuteOptions {
            output_root: output.path().to_path_buf(),
            collision: CollisionPolicy::Overwrite,
            proxy: None,
            timeout: Duration::from_secs(5),
            ..ExecuteOptions::default()
        },
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();
    assert!(overwritten.success);
    assert_eq!(
        tokio::fs::read(output.path().join("Best Goal.mp4"))
            .await
            .unwrap(),
        b"best-video"
    );
}

#[tokio::test]
async fn file_task_uses_backup_source_and_reports_structured_failure() {
    let server = TestServer::start().await;
    let resolver = KuaishouResolver::with_api_url(
        ResolveOptions::default(),
        &format!("{}/rest/wd/ugH5App/photo/simple/info", server.base),
    )
    .unwrap();
    let media = resolver.resolve("3xvideo123").await.unwrap();
    let mut plan = media.create_plan(&PlanSelection::default()).unwrap();
    let DownloadTask::File { sources, .. } = &mut plan.tasks[0] else {
        panic!("kuaishou video should create a file task");
    };
    sources[0].url = format!("{}/missing.mp4", server.base);
    sources.truncate(2);
    let mut failed_plan = plan.clone();
    let DownloadTask::File { sources, .. } = &mut failed_plan.tasks[0] else {
        panic!("kuaishou video should create a file task");
    };
    for (index, source) in sources.iter_mut().enumerate() {
        source.url = format!("{}/missing-{index}.mp4", server.base);
    }

    let output = tempdir().unwrap();
    let summary = execute_plan(
        plan,
        ExecuteOptions {
            output_root: output.path().to_path_buf(),
            collision: CollisionPolicy::Skip,
            proxy: None,
            timeout: Duration::from_secs(5),
            ..ExecuteOptions::default()
        },
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();
    assert!(summary.success);
    assert_eq!(
        tokio::fs::read(output.path().join("Best Goal.mp4"))
            .await
            .unwrap(),
        b"backup-video"
    );

    let failed_output = tempdir().unwrap();
    let failed = execute_plan(
        failed_plan,
        ExecuteOptions {
            output_root: failed_output.path().to_path_buf(),
            collision: CollisionPolicy::Skip,
            proxy: None,
            timeout: Duration::from_secs(5),
            ..ExecuteOptions::default()
        },
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();
    assert!(!failed.success);
    assert_eq!(failed.failures[0].error.code, "download.sources_exhausted");
    assert_eq!(
        failed.failures[0].error.context["lastCode"],
        "http.not_found"
    );
}

#[derive(Clone)]
struct TestState {
    base: String,
}

struct TestServer {
    base: String,
    task: JoinHandle<()>,
}

impl TestServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let state = TestState { base: base.clone() };
        let app = Router::new()
            .route("/rest/wd/ugH5App/photo/simple/info", post(photo_info))
            .route(
                "/share",
                get(|| async { Redirect::temporary("/fw/photo/3xvideo123?photoId=3xvideo123") }),
            )
            .route("/video-1080.mp4", get(|| async { "best-video" }))
            .route("/video-720.mp4", get(|| async { "smaller-video" }))
            .route("/backup-1080.mp4", get(|| async { "backup-video" }))
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, task }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn photo_info(State(state): State<TestState>) -> Json<serde_json::Value> {
    Json(json!({
        "result": 1,
        "photo": {
            "photoId": "3xvideo123",
            "photoType": "VIDEO",
            "caption": "Best Goal",
            "userName": "红星新闻",
            "coverUrls": [{ "url": format!("{}/cover.jpg", state.base) }],
            "manifest": {
                "adaptationSet": [{
                    "representation": [
                        {
                            "qualityType": "720p",
                            "videoCodec": "avc",
                            "width": 720,
                            "height": 1280,
                            "fileSize": 100,
                            "url": format!("{}/video-720.mp4", state.base)
                        },
                        {
                            "qualityType": "1080p",
                            "videoCodec": "hevc",
                            "width": 1080,
                            "height": 1920,
                            "fileSize": 200,
                            "url": format!("{}/video-1080.mp4", state.base),
                            "backupUrl": [format!("{}/backup-1080.mp4", state.base)]
                        }
                    ]
                }]
            }
        }
    }))
}
