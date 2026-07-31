use std::{
    collections::BTreeMap,
    io::Read,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    extract::{OriginalUri, Query, State},
    http::{Response, StatusCode},
    response::Redirect,
    routing::get,
};
use easydl::{
    domain::{DownloadEvent, DownloadTask, MediaKind},
    downloader::{CollisionPolicy, ExecuteOptions, execute_plan},
    platforms::{
        AlbumOutput, PlanSelection, ResolveOptions,
        xiaohongshu::{self, XiaohongshuResolver},
    },
};
use serde_json::json;
use tempfile::tempdir;
use tokio::{net::TcpListener, task::JoinHandle};
use zip::ZipArchive;

const NOTE_ID: &str = "64f123456789abcdef123456";

#[tokio::test]
async fn resolves_redirected_video_and_selects_ranked_streams() {
    let server = XhsServer::start().await;
    let resolver = server.resolver();
    assert!(xiaohongshu::matches("https://xhslink.com/example"));
    assert!(xiaohongshu::matches(NOTE_ID));

    let media = resolver
        .resolve(&format!("看看 http://localhost:{}/video", server.port()))
        .await
        .unwrap();
    let inspect = media.inspect();
    assert_eq!(inspect.platform, "xhs");
    assert_eq!(inspect.media.kind, MediaKind::Video);
    assert_eq!(
        inspect
            .choices
            .qualities
            .iter()
            .map(|quality| quality.id.as_str())
            .collect::<Vec<_>>(),
        ["hd_115", "hd_259"]
    );
    let inspect_json = serde_json::to_string(&inspect).unwrap();
    assert!(!inspect_json.contains("video-h265.mp4"));
    assert!(!inspect_json.contains("video-h264.mp4"));
    assert!(
        server
            .requests()
            .iter()
            .any(|request| request == &format!("/explore/{NOTE_ID}?xsec_token=tok123"))
    );

    let best = media.create_plan(&PlanSelection::default()).unwrap();
    let DownloadTask::File {
        sources, output, ..
    } = &best.tasks[0]
    else {
        panic!("video should create a file task");
    };
    assert_eq!(sources[0].url, server.url("/video-h265.mp4"));
    assert_eq!(output.relative_path, format!("作者_视频标题_{NOTE_ID}.mp4"));
    let normalized = json!({
        "platform": inspect.platform,
        "kind": inspect.media.kind,
        "title": inspect.media.title,
        "qualityIds": inspect.choices.qualities.iter().map(|quality| quality.id.as_str()).collect::<Vec<_>>(),
        "tasks": [{ "kind": "file", "output": output.relative_path }],
    });
    let expected: serde_json::Value = serde_json::from_str(include_str!(
        "../tests/fixtures/golden/xiaohongshu-video.json"
    ))
    .unwrap();
    assert_eq!(normalized, expected);

    let h264 = media
        .create_plan(&PlanSelection {
            quality: Some("hd_259".to_owned()),
            ..PlanSelection::default()
        })
        .unwrap();
    let DownloadTask::File { sources, .. } = &h264.tasks[0] else {
        panic!("video should create a file task");
    };
    assert_eq!(sources[0].url, server.url("/video-h264.mp4"));
    assert_eq!(sources[1].url, server.url("/video-h264-backup.mp4"));

    let output_root = tempdir().unwrap();
    let summary = execute_plan(
        best,
        options(output_root.path()),
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();
    assert!(summary.success);
    assert_eq!(
        tokio::fs::read(
            output_root
                .path()
                .join(format!("作者_视频标题_{NOTE_ID}.mp4"))
        )
        .await
        .unwrap(),
        b"h265-video"
    );
}

#[tokio::test]
async fn resolves_html_fallback_album_with_live_photo_outputs() {
    let server = XhsServer::start().await;
    let media = server
        .resolver()
        .resolve(&format!("http://localhost:{}/album", server.port()))
        .await
        .unwrap();
    let inspect = media.inspect();
    assert_eq!(inspect.media.kind, MediaKind::Album);
    assert_eq!(inspect.choices.images.len(), 2);
    let inspect_json = serde_json::to_string(&inspect).unwrap();
    assert!(!inspect_json.contains("image-first.jpg"));
    assert!(!inspect_json.contains("live.mp4"));

    let selected = media
        .create_plan(&PlanSelection {
            images: Some(vec![1]),
            ..PlanSelection::default()
        })
        .unwrap();
    let DownloadTask::Archive { entries, .. } = &selected.tasks[0] else {
        panic!("album should create an archive task");
    };
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "02.jpg");
    assert_eq!(entries[0].sources[0].url, server.url("/image-second.jpg"));
    assert_eq!(
        entries[0].sources[1].url,
        server.url("/image-second-pre.jpg")
    );

    let live = media
        .create_plan(&PlanSelection {
            images: Some(vec![0]),
            ..PlanSelection::default()
        })
        .unwrap();
    let DownloadTask::Archive {
        entries, output, ..
    } = &live.tasks[0]
    else {
        panic!("album should create an archive task");
    };
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        ["01.jpg", "01_live.mp4"]
    );
    assert_eq!(output.relative_path, format!("作者_图文标题_{NOTE_ID}.zip"));

    let zip_output = tempdir().unwrap();
    let summary = execute_plan(
        live,
        options(zip_output.path()),
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();
    assert!(summary.success);
    assert_eq!(
        read_archive(
            &zip_output
                .path()
                .join(format!("作者_图文标题_{NOTE_ID}.zip"))
        ),
        BTreeMap::from([
            ("01.jpg".to_owned(), b"first-image".to_vec()),
            ("01_live.mp4".to_owned(), b"live-video".to_vec()),
        ])
    );

    let directory = media
        .create_plan(&PlanSelection {
            images: Some(vec![0]),
            output: Some("xhs-dir".to_owned()),
            album_output: AlbumOutput::Directory,
            ..PlanSelection::default()
        })
        .unwrap();
    assert_eq!(directory.tasks.len(), 2);
    assert_eq!(
        directory
            .tasks
            .iter()
            .map(|task| task.output().relative_path.as_str())
            .collect::<Vec<_>>(),
        ["xhs-dir/01.jpg", "xhs-dir/01_live.mp4"]
    );
}

#[tokio::test]
async fn resolves_bare_note_id_and_reports_blocked_pages() {
    let server = XhsServer::start().await;
    let media = server.resolver().resolve(NOTE_ID).await.unwrap();
    assert_eq!(media.inspect().media.kind, MediaKind::Album);

    let error = server
        .resolver()
        .resolve("aaaaaaaaaaaaaaaaaaaaaaaa")
        .await
        .unwrap_err();
    assert_eq!(error.info.code, "auth.required");
}

fn options(output_root: &std::path::Path) -> ExecuteOptions {
    ExecuteOptions {
        output_root: output_root.to_path_buf(),
        collision: CollisionPolicy::Skip,
        chunk_concurrency: 1,
        timeout: Duration::from_secs(5),
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
struct XhsState {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

struct XhsServer {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl XhsServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = XhsState {
            base: base.clone(),
            requests: Arc::clone(&requests),
        };
        let app = Router::new()
            .route(
                "/video",
                get({
                    let base = base.clone();
                    move || {
                        let base = base.clone();
                        async move {
                            Redirect::temporary(&format!(
                                "{base}/explore/{NOTE_ID}?xsec_token=tok123"
                            ))
                        }
                    }
                }),
            )
            .route("/album", get(album_short_page))
            .route("/explore/{id}", get(note_page))
            .route("/video-h265.mp4", get(|| async { "h265-video" }))
            .route("/video-h264.mp4", get(|| async { "h264-video" }))
            .route(
                "/video-h264-backup.mp4",
                get(|| async { "h264-video-backup" }),
            )
            .route("/cover.jpg", get(|| async { "cover" }))
            .route("/image-first.jpg", get(|| async { "first-image" }))
            .route("/image-second.jpg", get(|| async { "second-image" }))
            .route(
                "/image-second-pre.jpg",
                get(|| async { "second-image-pre" }),
            )
            .route("/live.mp4", get(|| async { "live-video" }))
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base,
            requests,
            task,
        }
    }

    fn resolver(&self) -> XiaohongshuResolver {
        XiaohongshuResolver::with_endpoints(
            ResolveOptions {
                proxy: None,
                timeout: Duration::from_secs(5),
                ..ResolveOptions::default()
            },
            &self.base,
            &["localhost"],
        )
        .unwrap()
    }

    fn port(&self) -> u16 {
        UrlParts::port(&self.base)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for XhsServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn album_short_page(State(state): State<XhsState>) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html")
        .body(Body::from(format!(
            "<a href=\"{}/explore/{NOTE_ID}?xsec_token=tok_album&amp;xsec_source=pc\">Found</a>",
            state.base
        )))
        .unwrap()
}

async fn note_page(
    State(state): State<XhsState>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response<Body> {
    state.requests.lock().unwrap().push(uri.to_string());
    let id = uri.path().rsplit('/').next().unwrap_or_default();
    if id == "aaaaaaaaaaaaaaaaaaaaaaaa" {
        return Response::builder()
            .status(StatusCode::OK)
            .body(Body::from("当前笔记暂时无法浏览"))
            .unwrap();
    }
    let note = if query
        .get("xsec_token")
        .is_some_and(|token| token == "tok123")
    {
        video_note(&state.base)
    } else {
        image_note(&state.base)
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html; charset=utf-8")
        .body(Body::from(initial_state_html(id, note)))
        .unwrap()
}

fn video_note(base: &str) -> serde_json::Value {
    json!({
        "noteId": NOTE_ID,
        "type": "video",
        "title": "视频标题",
        "desc": "视频描述",
        "user": { "nickname": "作者" },
        "imageList": [{ "urlDefault": format!("{base}/cover.jpg") }],
        "video": {
            "media": {
                "stream": {
                    "h264": [{
                        "qualityType": "HD",
                        "streamType": 259,
                        "width": 1280,
                        "height": 720,
                        "size": 300,
                        "masterUrl": format!("{base}/video-h264.mp4"),
                        "backupUrls": [format!("{base}/video-h264-backup.mp4")]
                    }],
                    "h265": [{
                        "qualityType": "HD",
                        "streamType": 115,
                        "width": 1280,
                        "height": 720,
                        "size": 200,
                        "masterUrl": format!("{base}/video-h265.mp4")
                    }]
                }
            }
        }
    })
}

fn image_note(base: &str) -> serde_json::Value {
    json!({
        "noteId": NOTE_ID,
        "type": "normal",
        "title": "图文标题",
        "user": { "nickname": "作者" },
        "imageList": [
            {
                "urlDefault": format!("{base}/image-first.jpg"),
                "width": 1080,
                "height": 1440,
                "livePhoto": true,
                "stream": { "h264": [{ "masterUrl": format!("{base}/live.mp4") }] }
            },
            {
                "urlDefault": format!("{base}/image-second.jpg"),
                "urlPre": format!("{base}/image-second-pre.jpg"),
                "width": 1080,
                "height": 1440
            }
        ]
    })
}

fn initial_state_html(id: &str, note: serde_json::Value) -> String {
    let mut state = json!({
        "note": { "noteDetailMap": { id: { "note": note } } }
    })
    .to_string();
    state.pop();
    state.push_str(",\"unused\":undefined,}");
    format!("<html><script>window.__INITIAL_STATE__={state};</script></html>")
}

struct UrlParts;

impl UrlParts {
    fn port(url: &str) -> u16 {
        url.rsplit(':').next().unwrap().parse().unwrap()
    }
}
