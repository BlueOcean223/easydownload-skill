use std::{
    collections::BTreeMap,
    io::Read,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, Response, StatusCode, Uri, header},
    response::Redirect,
    routing::{get, post},
};
use easydl::{
    domain::{DownloadEvent, DownloadTask, MediaKind},
    downloader::{CollisionPolicy, ExecuteOptions, execute_plan},
    platforms::{
        AlbumOutput, PlanSelection, ResolveOptions,
        douyin::{self, DouyinResolver},
    },
};
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::{net::TcpListener, task::JoinHandle};
use zip::ZipArchive;

const VIDEO_ID: &str = "7312345678901234561";
const ALBUM_ID: &str = "7312345678901234562";
const PROBE_720_ID: &str = "7312345678901234563";
const PROBE_1080_ID: &str = "7312345678901234564";
const TTWID_ID: &str = "7312345678901234565";
const WEB_DETAIL_ID: &str = "7312345678901234566";
const FAILED_ID: &str = "7312345678901234567";

#[tokio::test]
async fn resolves_short_video_and_executes_selected_quality() {
    let server = DouyinServer::start().await;
    assert!(douyin::matches("复制 https://v.douyin.com/abc/"));
    assert!(douyin::matches(VIDEO_ID));
    let media = server
        .resolver()
        .resolve(&format!(
            "复制口令 http://localhost:{}/short",
            server.port()
        ))
        .await
        .unwrap();
    let inspect = media.inspect();
    assert_eq!(inspect.platform, "douyin");
    assert_eq!(inspect.media.kind, MediaKind::Video);
    assert_eq!(
        inspect
            .choices
            .qualities
            .iter()
            .map(|quality| quality.id.as_str())
            .collect::<Vec<_>>(),
        ["1080p", "720p"]
    );
    let inspect_json = serde_json::to_string(&inspect).unwrap();
    assert!(!inspect_json.contains("1080-play.mp4"));

    let best = media.create_plan(&PlanSelection::default()).unwrap();
    let DownloadTask::File {
        sources, output, ..
    } = &best.tasks[0]
    else {
        panic!("video should create a file task");
    };
    assert_eq!(sources[0].url, server.url("/media/1080-play.mp4"));
    assert_eq!(
        output.relative_path,
        format!("作者_视频标题_{VIDEO_ID}.mp4")
    );
    let normalized = json!({
        "platform": inspect.platform,
        "kind": inspect.media.kind,
        "title": inspect.media.title,
        "qualityIds": inspect.choices.qualities.iter().map(|quality| quality.id.as_str()).collect::<Vec<_>>(),
        "tasks": [{
            "kind": "file",
            "output": output.relative_path.replace(VIDEO_ID, "<id>"),
        }],
    });
    let expected: Value =
        serde_json::from_str(include_str!("../tests/fixtures/golden/douyin-video.json")).unwrap();
    assert_eq!(normalized, expected);

    let selected = media
        .create_plan(&PlanSelection {
            quality: Some("720p".to_owned()),
            ..PlanSelection::default()
        })
        .unwrap();
    let DownloadTask::File { sources, .. } = &selected.tasks[0] else {
        panic!("video should create a file task");
    };
    assert_eq!(sources[0].url, server.url("/media/720-play.mp4"));

    let output_root = tempdir().unwrap();
    let summary = execute_plan(
        selected,
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
                .join(format!("作者_视频标题_{VIDEO_ID}.mp4"))
        )
        .await
        .unwrap(),
        b"video-720"
    );
}

#[tokio::test]
async fn creates_selected_album_zip_and_directory_with_sidecar_video() {
    let server = DouyinServer::start().await;
    let media = server
        .resolver()
        .resolve(&server.url(&format!("/note/{ALBUM_ID}")))
        .await
        .unwrap();
    let inspect = media.inspect();
    assert_eq!(inspect.media.kind, MediaKind::Album);
    assert_eq!(inspect.choices.images.len(), 2);
    let inspect_json = serde_json::to_string(&inspect).unwrap();
    assert!(!inspect_json.contains("image-2.jpg"));
    assert!(!inspect_json.contains("clip-play.mp4"));

    let plan = media
        .create_plan(&PlanSelection {
            images: Some(vec![1]),
            ..PlanSelection::default()
        })
        .unwrap();
    let DownloadTask::Archive {
        entries, output, ..
    } = &plan.tasks[0]
    else {
        panic!("album should create an archive task");
    };
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        ["02.jpg", "02_video.mp4"]
    );
    assert_eq!(entries[0].sources[0].url, server.url("/image-2.jpg"));
    assert_eq!(entries[1].sources[0].url, server.url("/clip-play.mp4"));
    assert_eq!(
        output.relative_path,
        format!("作者_图集标题_{ALBUM_ID}.zip")
    );

    let output_root = tempdir().unwrap();
    let summary = execute_plan(
        plan,
        options(output_root.path()),
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();
    assert!(summary.success);
    assert_eq!(
        read_archive(
            &output_root
                .path()
                .join(format!("作者_图集标题_{ALBUM_ID}.zip"))
        ),
        BTreeMap::from([
            ("02.jpg".to_owned(), b"image-two".to_vec()),
            ("02_video.mp4".to_owned(), b"sidecar-video".to_vec()),
        ])
    );

    let directory = media
        .create_plan(&PlanSelection {
            images: Some(vec![1]),
            output: Some("douyin-dir".to_owned()),
            album_output: AlbumOutput::Directory,
            ..PlanSelection::default()
        })
        .unwrap();
    assert_eq!(
        directory
            .tasks
            .iter()
            .map(|task| task.output().relative_path.as_str())
            .collect::<Vec<_>>(),
        ["douyin-dir/02.jpg", "douyin-dir/02_video.mp4"]
    );
}

#[tokio::test]
async fn probes_only_supported_ratios_and_keeps_source_1080p() {
    let server = DouyinServer::start().await;
    let media = server
        .resolver()
        .resolve(&server.url(&format!("/video/{PROBE_720_ID}")))
        .await
        .unwrap();
    assert_eq!(media.inspect().choices.qualities[0].id, "720p");
    assert_eq!(media.inspect().choices.qualities[0].size, Some(12_345));
    assert!(
        !server
            .requests()
            .iter()
            .any(|request| request.contains("video-uri-720") && request.contains("ratio=1080p"))
    );

    let media = server
        .resolver()
        .resolve(&server.url(&format!("/video/{PROBE_1080_ID}")))
        .await
        .unwrap();
    assert_eq!(media.inspect().choices.qualities[0].id, "1080p");
    assert_eq!(media.inspect().choices.qualities[0].size, Some(333));
}

#[tokio::test]
async fn retries_hidden_router_data_with_ephemeral_ttwid() {
    let server = DouyinServer::start().await;
    let media = server
        .resolver()
        .resolve(&server.url(&format!("/video/{TTWID_ID}")))
        .await
        .unwrap();
    assert_eq!(media.inspect().choices.qualities[0].id, "720p");
    assert_eq!(media.inspect().choices.qualities[0].size, Some(222));
    let requests = server.requests();
    assert!(
        requests
            .iter()
            .any(|request| request == "/ttwid/union/register/")
    );
    assert!(requests.iter().any(|request| {
        request.contains(&format!("/share/video/{TTWID_ID}/?app=aweme"))
            && request.contains("cookie=ttwid=test-token")
    }));
}

#[tokio::test]
async fn falls_back_to_web_detail_direct_cdn_sources() {
    let server = DouyinServer::start().await;
    let media = server
        .resolver()
        .resolve(&server.url(&format!("/video/{WEB_DETAIL_ID}")))
        .await
        .unwrap();
    let inspect = media.inspect();
    assert_eq!(inspect.choices.qualities[0].id, "720p");
    assert_eq!(inspect.choices.qualities[0].size, Some(9_544_587));
    let plan = media.create_plan(&PlanSelection::default()).unwrap();
    let DownloadTask::File { sources, .. } = &plan.tasks[0] else {
        panic!("video should create a file task");
    };
    assert_eq!(
        sources
            .iter()
            .map(|source| source.url.as_str())
            .collect::<Vec<_>>(),
        [
            server.url("/cdn/video-720.mp4?mime_type=video_mp4"),
            server.url("/cdn/video-720-backup.mp4?mime_type=video_mp4")
        ]
    );
    assert!(server.requests().iter().any(|request| {
        request.contains(&format!("aweme_id={WEB_DETAIL_ID}"))
            && request.contains("channel=channel_pc_web")
            && request.contains(&format!(
                "referer=https://www.douyin.com/video/{WEB_DETAIL_ID}"
            ))
    }));
}

#[tokio::test]
async fn does_not_plan_unplayable_play_endpoint_urls() {
    let server = DouyinServer::start().await;
    let error = server
        .resolver()
        .resolve(&server.url(&format!("/video/{FAILED_ID}")))
        .await
        .unwrap_err();
    assert_eq!(error.info.code, "platform.no_streams");
    assert!(error.info.message.contains("no playable video URL"));
}

#[tokio::test]
async fn sends_explicit_cookies_without_leaking_stored_bilibili_auth() {
    let server = DouyinServer::start().await;
    let config_dir = tempdir().unwrap();
    tokio::fs::write(
        config_dir.path().join("auth.json"),
        r#"{"bilibili":{"sessData":"bili-only-secret"}}"#,
    )
    .await
    .unwrap();
    let cookies_path = config_dir.path().join("douyin-cookies.txt");
    tokio::fs::write(&cookies_path, "sessionid=dy-user-cookie")
        .await
        .unwrap();
    let resolver = DouyinResolver::with_base_url(
        ResolveOptions {
            proxy: None,
            timeout: Duration::from_secs(5),
            cookies_path: Some(cookies_path),
            config_dir: config_dir.path().to_path_buf(),
        },
        &server.base,
        &["localhost"],
    )
    .unwrap();
    let media = resolver
        .resolve(&server.url(&format!("/video/{WEB_DETAIL_ID}")))
        .await
        .unwrap();
    let plan = media.create_plan(&PlanSelection::default()).unwrap();
    let plan_json = serde_json::to_string(&plan).unwrap();
    assert!(!plan_json.contains("dy-user-cookie"));
    assert!(!plan_json.contains("bili-only-secret"));
    let requests = server.requests();
    assert!(requests.iter().any(|request| {
        request.contains(&format!("aweme_id={WEB_DETAIL_ID}"))
            && request.contains("cookie=sessionid=dy-user-cookie")
    }));
    assert!(
        requests
            .iter()
            .all(|request| !request.contains("bili-only-secret"))
    );
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
struct TestState {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

struct DouyinServer {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl DouyinServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = TestState {
            base: base.clone(),
            requests: Arc::clone(&requests),
        };
        let app = Router::new()
            .route(
                "/short",
                get({
                    let base = base.clone();
                    move || {
                        let base = base.clone();
                        async move { Redirect::temporary(&format!("{base}/video/{VIDEO_ID}")) }
                    }
                }),
            )
            .route("/share/{kind}/{id}/", get(share_page))
            .route("/aweme/v1/play/", get(play_probe))
            .route("/aweme/v1/web/aweme/detail/", get(web_detail))
            .route("/ttwid/union/register/", post(ttwid))
            .route("/media/1080-play.mp4", get(|| async { "video-1080" }))
            .route("/media/720-play.mp4", get(|| async { "video-720" }))
            .route("/image-1.jpg", get(|| async { "image-one" }))
            .route("/image-2.jpg", get(|| async { "image-two" }))
            .route("/clip-play.mp4", get(|| async { "sidecar-video" }))
            .route("/cdn/video-720.mp4", get(|| async { "cdn-video" }))
            .route(
                "/cdn/video-720-backup.mp4",
                get(|| async { "cdn-video-backup" }),
            )
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

    fn resolver(&self) -> DouyinResolver {
        DouyinResolver::with_base_url(
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
        self.base.rsplit(':').next().unwrap().parse().unwrap()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for DouyinServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn share_page(
    State(state): State<TestState>,
    Path((kind, id)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response<Body> {
    record(&state, &uri, &headers);
    if id == ALBUM_ID && kind == "video" {
        return html_response(router_html(json!({})));
    }
    if id == ALBUM_ID && kind == "note" {
        return html_response(router_html(album_item(&state.base)));
    }
    if id == TTWID_ID && kind == "video" {
        let cookie = headers
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if cookie != "ttwid=test-token" {
            return html_response("<html>open app</html>".to_owned());
        }
        return html_response(router_html(video_without_bitrate(
            TTWID_ID,
            "video-uri-ttwid",
            720,
            1280,
        )));
    }
    if kind == "note" {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("not found"))
            .unwrap();
    }
    let item = match id.as_str() {
        VIDEO_ID => video_item(&state.base),
        PROBE_720_ID => video_without_bitrate(PROBE_720_ID, "video-uri-720", 720, 1280),
        PROBE_1080_ID => video_without_bitrate(PROBE_1080_ID, "video-uri-1080", 1080, 1920),
        WEB_DETAIL_ID => video_without_bitrate(WEB_DETAIL_ID, "video-uri-web", 720, 1280),
        FAILED_ID => video_without_bitrate(FAILED_ID, "video-uri-failed", 720, 1280),
        _ => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }
    };
    html_response(router_html(item))
}

async fn play_probe(
    State(state): State<TestState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response<Body> {
    record(&state, &uri, &headers);
    let video_id = query
        .get("video_id")
        .map(String::as_str)
        .unwrap_or_default();
    let ratio = query.get("ratio").map(String::as_str).unwrap_or_default();
    let total = match (video_id, ratio) {
        ("video-uri-720", "720p") => Some(12_345),
        ("video-uri-1080", "1080p") => Some(333),
        ("video-uri-ttwid", _) => Some(222),
        _ => None,
    };
    if let Some(total) = total {
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header("content-range", format!("bytes 0-1/{total}"))
            .header("content-type", "video/mp4")
            .body(Body::from("xx"))
            .unwrap()
    } else {
        Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap()
    }
}

async fn web_detail(
    State(state): State<TestState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response<Body> {
    record(&state, &uri, &headers);
    match query.get("aweme_id").map(String::as_str) {
        Some(WEB_DETAIL_ID) => json_response(json!({
            "status_code": 0,
            "aweme_detail": web_detail_item(&state.base)
        })),
        Some(FAILED_ID) => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::empty())
            .unwrap(),
        _ => json_response(json!({ "status_code": 1, "status_msg": "not found" })),
    }
}

async fn ttwid(
    State(state): State<TestState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response<Body> {
    record(&state, &uri, &headers);
    Response::builder()
        .status(StatusCode::OK)
        .header(
            "set-cookie",
            "ttwid=test-token; Path=/; Domain=bytedance.com",
        )
        .body(Body::from("{}"))
        .unwrap()
}

fn record(state: &TestState, uri: &Uri, headers: &HeaderMap) {
    let mut value = uri.to_string();
    if let Some(cookie) = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
    {
        value.push_str(" cookie=");
        value.push_str(cookie);
    }
    if let Some(referer) = headers
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())
    {
        value.push_str(" referer=");
        value.push_str(referer);
    }
    state.requests.lock().unwrap().push(value);
}

fn video_item(base: &str) -> Value {
    json!({
        "aweme_id": VIDEO_ID,
        "desc": "视频标题",
        "aweme_type": 0,
        "author": { "nickname": "作者" },
        "video": {
            "cover": { "url_list": [format!("{base}/cover.jpg")] },
            "bit_rate": [
                {
                    "gear_name": "normal_720p",
                    "bit_rate": 800,
                    "play_addr": {
                        "width": 720,
                        "height": 1280,
                        "url_list": [format!("{base}/media/720-playwm.mp4")]
                    }
                },
                {
                    "gear_name": "normal_1080p",
                    "bit_rate": 1200,
                    "play_addr": {
                        "width": 1080,
                        "height": 1920,
                        "url_list": [format!("{base}/media/1080-playwm.mp4")]
                    }
                }
            ]
        }
    })
}

fn album_item(base: &str) -> Value {
    json!({
        "aweme_id": ALBUM_ID,
        "desc": "图集标题",
        "aweme_type": 68,
        "author": { "nickname": "作者" },
        "images": [
            { "url_list": [format!("{base}/image-1.jpg")], "width": 1080, "height": 1440 },
            {
                "url_list": [format!("{base}/image-2.jpg")],
                "width": 1080,
                "height": 1440,
                "video": { "play_addr": { "url_list": [format!("{base}/clip-playwm.mp4")] } }
            }
        ]
    })
}

fn video_without_bitrate(id: &str, uri: &str, width: u32, height: u32) -> Value {
    json!({
        "aweme_id": id,
        "desc": "待探测视频",
        "aweme_type": 0,
        "author": { "nickname": "作者" },
        "video": {
            "play_addr": { "uri": uri, "width": width, "height": height, "url_list": [] },
            "width": width,
            "height": height,
            "bit_rate": []
        }
    })
}

fn web_detail_item(base: &str) -> Value {
    json!({
        "aweme_id": WEB_DETAIL_ID,
        "desc": "Web detail 视频",
        "aweme_type": 0,
        "author": { "nickname": "作者" },
        "video": {
            "width": 1280,
            "height": 720,
            "bit_rate": [{
                "gear_name": "normal_720_0",
                "bit_rate": 2701266,
                "play_addr": {
                    "width": 1280,
                    "height": 720,
                    "data_size": 9544587,
                    "url_list": [
                        format!("{base}/cdn/video-720.mp4?mime_type=video_mp4"),
                        format!("{base}/cdn/video-720-backup.mp4?mime_type=video_mp4"),
                        format!("{base}/aweme/v1/play/?video_id=video-uri-web&ratio=720p")
                    ]
                }
            }]
        }
    })
}

fn router_html(item: Value) -> String {
    let payload = if item.as_object().is_some_and(|item| item.is_empty()) {
        json!({ "loaderData": {} })
    } else {
        json!({ "loaderData": { "video": { "videoInfoRes": { "item_list": [item] } } } })
    };
    format!("<html><script>window._ROUTER_DATA={payload};</script></html>")
}

fn html_response(value: String) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html; charset=utf-8")
        .body(Body::from(value))
        .unwrap()
}

fn json_response(value: Value) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(value.to_string()))
        .unwrap()
}
