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
    Json, Router,
    body::Body,
    extract::{OriginalUri, Query, State},
    http::{HeaderMap, Response, StatusCode, Uri, header},
    response::Redirect,
    routing::{get, post},
};
use easydl::{
    domain::{DownloadEvent, DownloadTask, MediaKind, MuxTrackKind},
    downloader::{CollisionPolicy, ExecuteOptions, execute_plan},
    platforms::{
        PlanSelection, ResolveOptions,
        bilibili::{self, BilibiliResolver},
    },
};
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::{net::TcpListener, task::JoinHandle};
use zip::ZipArchive;

const BVID: &str = "BV1xx411c7mD";
const DRM_BVID: &str = "BV1drm411c7mD";

#[tokio::test]
async fn resolves_b23_video_and_lazily_plans_all_dash_parts() {
    let server = BiliServer::start().await;
    assert!(bilibili::matches("https://b23.tv/abc"));
    assert!(bilibili::matches(BVID));
    let media = server
        .resolver()
        .resolve(&format!("http://localhost:{}/b23", server.port()))
        .await
        .unwrap();
    let inspect = media.inspect();
    assert_eq!(inspect.platform, "bilibili");
    assert_eq!(inspect.media.kind, MediaKind::Playlist);
    assert_eq!(inspect.choices.parts.len(), 2);
    assert_eq!(inspect.choices.qualities[0].id, "80");
    assert_eq!(inspect.choices.qualities[0].label, "1080P");
    let inspect_json = serde_json::to_string(&inspect).unwrap();
    assert!(!inspect_json.contains("222-80.m4s"));

    let default_plan = media
        .create_plan(&PlanSelection {
            quality: Some("80".to_owned()),
            ..PlanSelection::default()
        })
        .await
        .unwrap();
    assert_eq!(default_plan.tasks.len(), 1);
    assert_mux_sources(
        &default_plan.tasks[0],
        &server.url("/media/222-80.m4s"),
        &server.url("/audio/222.m4s"),
    );

    let first_part = media
        .create_plan(&PlanSelection {
            part: Some(1),
            ..PlanSelection::default()
        })
        .await
        .unwrap();
    assert_mux_sources(
        &first_part.tasks[0],
        &server.url("/media/111-80.m4s"),
        &server.url("/audio/111.m4s"),
    );
    assert_eq!(
        media
            .create_plan(&PlanSelection {
                part: Some(3),
                ..PlanSelection::default()
            })
            .await
            .unwrap_err()
            .info
            .code,
        "selection.part_out_of_range"
    );

    let all_parts = media
        .create_plan(&PlanSelection {
            quality: Some("1080P".to_owned()),
            all_parts: true,
            ..PlanSelection::default()
        })
        .await
        .unwrap();
    assert_eq!(all_parts.tasks.len(), 2);
    assert_mux_sources(
        &all_parts.tasks[0],
        &server.url("/media/111-80.m4s"),
        &server.url("/audio/111.m4s"),
    );
    assert_mux_sources(
        &all_parts.tasks[1],
        &server.url("/media/222-80.m4s"),
        &server.url("/audio/222.m4s"),
    );
    assert_eq!(
        all_parts
            .tasks
            .iter()
            .map(|task| task.output().relative_path.as_str())
            .collect::<Vec<_>>(),
        ["普通视频_P01_第一P.mp4", "普通视频_P02_第二P.mp4"]
    );

    let normalized = json!({
        "platform": inspect.platform,
        "kind": inspect.media.kind,
        "title": inspect.media.title,
        "qualityIds": inspect.choices.qualities.iter().map(|quality| quality.id.as_str()).collect::<Vec<_>>(),
        "qualityLabels": inspect.choices.qualities.iter().map(|quality| quality.label.as_str()).collect::<Vec<_>>(),
        "partTitles": inspect.choices.parts.iter().map(|part| part.title.as_str()).collect::<Vec<_>>(),
        "tasks": all_parts.tasks.iter().map(normalize_mux_task).collect::<Vec<_>>(),
    });
    let expected: Value =
        serde_json::from_str(include_str!("../tests/fixtures/golden/bilibili-video.json")).unwrap();
    assert_eq!(normalized, expected);
}

#[tokio::test]
async fn adds_video_subtitles_but_skips_bangumi_player_v2() {
    let server = BiliServer::start().await;
    server.enable_subtitles();
    let media = server
        .resolver()
        .resolve(&format!("https://www.bilibili.com/video/{BVID}?subtitle=1"))
        .await
        .unwrap();
    let plan = media
        .create_plan(&PlanSelection {
            quality: Some("80".to_owned()),
            ..PlanSelection::default()
        })
        .await
        .unwrap();
    assert_eq!(plan.tasks.len(), 2);
    let DownloadTask::File {
        sources, output, ..
    } = &plan.tasks[1]
    else {
        panic!("subtitle should create a file task");
    };
    assert_eq!(sources[0].url, server.url("/subtitle/zh.json"));
    assert!(output.relative_path.ends_with(".中文.json"));

    server.clear_requests();
    let bangumi = server
        .resolver()
        .resolve("https://www.bilibili.com/bangumi/play/ep123")
        .await
        .unwrap();
    assert_eq!(bangumi.inspect().media.kind, MediaKind::Bangumi);
    assert_eq!(
        bangumi
            .inspect()
            .choices
            .parts
            .iter()
            .map(|part| part.title.as_str())
            .collect::<Vec<_>>(),
        ["1 第一集", "2 第二集"]
    );
    assert!(
        !server
            .requests()
            .iter()
            .any(|request| request.contains("/x/player/v2"))
    );
    let plan = bangumi
        .create_plan(&PlanSelection::default())
        .await
        .unwrap();
    assert_mux_sources(
        &plan.tasks[0],
        &server.url("/media/333-80.m4s"),
        &server.url("/audio/333.m4s"),
    );
}

#[tokio::test]
async fn resolves_collection_url_variants_and_all_parts() {
    let server = BiliServer::start().await;
    let inputs = [
        "https://space.bilibili.com/123/channel/collectiondetail?sid=456",
        "https://space.bilibili.com/123/lists?sid=456",
        "https://space.bilibili.com/123/lists/456",
        "https://www.bilibili.com/list/123/?sid=456",
        "https://m.bilibili.com/space/123/lists/456",
    ];
    for input in inputs {
        let media = server.resolver().resolve(input).await.unwrap();
        assert_eq!(media.inspect().media.title, "收藏夹");
        assert_eq!(
            media
                .inspect()
                .choices
                .parts
                .iter()
                .map(|part| part.title.as_str())
                .collect::<Vec<_>>(),
            ["[01] BVcol1 标题", "[02] BVcol2 标题"]
        );
    }
    let media = server.resolver().resolve(inputs[0]).await.unwrap();
    let plan = media
        .create_plan(&PlanSelection {
            quality: Some("80".to_owned()),
            all_parts: true,
            ..PlanSelection::default()
        })
        .await
        .unwrap();
    assert_eq!(plan.tasks.len(), 2);
    assert_mux_sources(
        &plan.tasks[0],
        &server.url("/media/111-80.m4s"),
        &server.url("/audio/111.m4s"),
    );
    assert_mux_sources(
        &plan.tasks[1],
        &server.url("/media/222-80.m4s"),
        &server.url("/audio/222.m4s"),
    );
}

#[tokio::test]
async fn resolves_cheese_course_and_plans_dash() {
    let server = BiliServer::start().await;
    let media = server
        .resolver()
        .resolve("https://www.bilibili.com/cheese/play/ss123")
        .await
        .unwrap();
    assert_eq!(media.inspect().media.title, "课程标题");
    assert!(media.inspect().requirements.login);
    let plan = media
        .create_plan(&PlanSelection {
            quality: Some("80".to_owned()),
            ..PlanSelection::default()
        })
        .await
        .unwrap();
    assert_mux_sources(
        &plan.tasks[0],
        &server.url("/media/777-80.m4s"),
        &server.url("/audio/777.m4s"),
    );
}

#[tokio::test]
async fn resolves_gongfang_files_to_an_executable_archive_plan() {
    let server = BiliServer::start().await;
    let media = server
        .resolver()
        .resolve("https://gf.bilibili.com/order/hyg-download/12345")
        .await
        .unwrap();
    let plan = media.create_plan(&PlanSelection::default()).await.unwrap();
    let DownloadTask::Archive {
        entries, output, ..
    } = &plan.tasks[0]
    else {
        panic!("gongfang should create an archive task");
    };
    assert_eq!(entries[0].path, "01_讲义.pdf");
    assert_eq!(entries[0].sources[0].url, server.url("/download/file.pdf"));
    assert_eq!(output.relative_path, "课程资料.zip");

    let output_root = tempdir().unwrap();
    let summary = execute_plan(
        plan,
        ExecuteOptions {
            output_root: output_root.path().to_path_buf(),
            collision: CollisionPolicy::Skip,
            chunk_concurrency: 1,
            timeout: Duration::from_secs(5),
            ..ExecuteOptions::default()
        },
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();
    assert!(summary.success);
    assert_eq!(
        read_archive(&output_root.path().join("课程资料.zip")),
        BTreeMap::from([("01_讲义.pdf".to_owned(), b"pdf-data".to_vec())])
    );
}

#[tokio::test]
async fn rejects_drm_without_attempting_a_bypass() {
    let server = BiliServer::start().await;
    let error = server
        .resolver()
        .resolve(&format!("https://www.bilibili.com/video/{DRM_BVID}"))
        .await
        .unwrap_err();
    assert_eq!(error.info.code, "platform.drm_unsupported");
}

#[tokio::test]
async fn uses_stored_or_explicit_cookie_credentials_only_during_resolution() {
    let server = BiliServer::start().await;
    let config = tempdir().unwrap();
    tokio::fs::write(
        config.path().join("auth.json"),
        r#"{"bilibili":{"sessData":"sess/data","updatedAt":"2026-01-01T00:00:00Z"}}"#,
    )
    .await
    .unwrap();
    let resolver = server.resolver_with_credentials(config.path(), None);
    let media = resolver
        .resolve(&format!("https://www.bilibili.com/video/{BVID}"))
        .await
        .unwrap();
    assert!(
        server
            .requests()
            .iter()
            .any(|request| request.contains("cookie=SESSDATA=sess%2Fdata"))
    );
    let plan_json =
        serde_json::to_string(&media.create_plan(&PlanSelection::default()).await.unwrap())
            .unwrap();
    assert!(!plan_json.contains("SESSDATA"));
    assert!(!plan_json.contains("Cookie"));

    server.clear_requests();
    let cookies = config.path().join("cookies.txt");
    tokio::fs::write(&cookies, "SESSDATA=file-cookie; bili_jct=csrf\n")
        .await
        .unwrap();
    server
        .resolver_with_credentials(config.path(), Some(cookies))
        .resolve(&format!("https://www.bilibili.com/video/{BVID}"))
        .await
        .unwrap();
    assert!(
        server
            .requests()
            .iter()
            .any(|request| { request.contains("cookie=SESSDATA=file-cookie; bili_jct=csrf") })
    );
}

fn assert_mux_sources(task: &DownloadTask, video: &str, audio: &str) {
    let DownloadTask::Mux { tracks, .. } = task else {
        panic!("expected mux task");
    };
    let video_track = tracks
        .iter()
        .find(|track| track.kind == MuxTrackKind::Video)
        .unwrap();
    let audio_track = tracks
        .iter()
        .find(|track| track.kind == MuxTrackKind::Audio)
        .unwrap();
    assert_eq!(video_track.sources[0].url, video);
    assert_eq!(audio_track.sources[0].url, audio);
}

fn normalize_mux_task(task: &DownloadTask) -> Value {
    let DownloadTask::Mux { tracks, output, .. } = task else {
        panic!("expected mux task");
    };
    let source_name = |kind| {
        let url = &tracks
            .iter()
            .find(|track| track.kind == kind)
            .expect("normalized mux track should exist")
            .sources[0]
            .url;
        url::Url::parse(url)
            .unwrap()
            .path_segments()
            .and_then(Iterator::last)
            .unwrap()
            .to_owned()
    };
    json!({
        "kind": "mux",
        "output": output.relative_path,
        "video": source_name(MuxTrackKind::Video),
        "audio": source_name(MuxTrackKind::Audio),
    })
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
    subtitles: Arc<AtomicBool>,
}

struct BiliServer {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    subtitles: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl BiliServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let subtitles = Arc::new(AtomicBool::new(false));
        let state = TestState {
            base: base.clone(),
            requests: Arc::clone(&requests),
            subtitles: Arc::clone(&subtitles),
        };
        let app = Router::new()
            .route(
                "/b23",
                get(|| async {
                    Redirect::temporary(&format!("https://www.bilibili.com/video/{BVID}?p=2"))
                }),
            )
            .route("/x/web-interface/view", get(video_view))
            .route("/x/player/v2", get(player_v2))
            .route("/x/player/playurl", get(video_playurl))
            .route("/pgc/view/web/season", get(bangumi_season))
            .route("/pgc/player/web/playurl", get(bangumi_playurl))
            .route(
                "/x/polymer/web-space/seasons_archives_list",
                get(collection),
            )
            .route("/pugv/view/web/season/v2", get(cheese_season))
            .route("/pugv/player/web/playurl", get(cheese_playurl))
            .route("/mall-c/order/detail", get(gongfang_title))
            .route("/mall-c/ship/orderdetails/query", post(gongfang_detail))
            .route(
                "/mall-c/ship/orderdetails/querydownloadurl",
                post(gongfang_download),
            )
            .route("/download/file.pdf", get(|| async { "pdf-data" }))
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base,
            requests,
            subtitles,
            task,
        }
    }

    fn resolver(&self) -> BilibiliResolver {
        self.resolver_with_credentials(std::path::Path::new("/nonexistent"), None)
    }

    fn resolver_with_credentials(
        &self,
        config_dir: &std::path::Path,
        cookies_path: Option<std::path::PathBuf>,
    ) -> BilibiliResolver {
        BilibiliResolver::with_base_url(
            ResolveOptions {
                proxy: None,
                timeout: Duration::from_secs(5),
                cookies_path,
                config_dir: config_dir.to_path_buf(),
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

    fn clear_requests(&self) {
        self.requests.lock().unwrap().clear();
    }

    fn enable_subtitles(&self) {
        self.subtitles.store(true, Ordering::Relaxed);
    }
}

impl Drop for BiliServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn video_view(
    State(state): State<TestState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response<Body> {
    record(&state, &uri, &headers);
    let bvid = query.get("bvid").map(String::as_str).unwrap_or(BVID);
    let (cid, title, pages) = match bvid {
        "BVcol1" => (
            111,
            "BVcol1 标题",
            vec![json!({ "cid": 111, "page": 1, "part": "BVcol1 标题", "duration": 60 })],
        ),
        "BVcol2" => (
            222,
            "BVcol2 标题",
            vec![json!({ "cid": 222, "page": 1, "part": "BVcol2 标题", "duration": 60 })],
        ),
        _ => (
            111,
            "普通视频",
            vec![
                json!({ "cid": 111, "page": 1, "part": "第一P", "duration": 60 }),
                json!({ "cid": 222, "page": 2, "part": "第二P", "duration": 60 }),
            ],
        ),
    };
    json_response(json!({
        "code": 0,
        "data": {
            "bvid": bvid,
            "aid": 170001,
            "title": title,
            "pic": format!("{}/cover.jpg", state.base),
            "duration": 120,
            "owner": { "name": "UP主" },
            "cid": cid,
            "pages": pages
        }
    }))
}

async fn player_v2(
    State(state): State<TestState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response<Body> {
    record(&state, &uri, &headers);
    let has_subtitle = query.get("bvid").is_some_and(|bvid| bvid == BVID)
        && state.subtitles.load(Ordering::Relaxed);
    let subtitles = if has_subtitle {
        vec![json!({
            "lan": "zh-CN",
            "lan_doc": "中文",
            "subtitle_url": format!("{}/subtitle/zh.json", state.base)
        })]
    } else {
        Vec::new()
    };
    json_response(json!({ "code": 0, "data": { "subtitle": { "subtitles": subtitles } } }))
}

async fn video_playurl(
    State(state): State<TestState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response<Body> {
    record(&state, &uri, &headers);
    if query.get("bvid").is_some_and(|bvid| bvid == DRM_BVID) {
        return json_response(json!({
            "code": 0,
            "data": { "dash": { "video": [{ "id": 80, "bilidrm_uri": "drm://protected" }], "audio": [] } }
        }));
    }
    let cid = query
        .get("cid")
        .and_then(|cid| cid.parse().ok())
        .unwrap_or(111);
    json_response(json!({ "code": 0, "data": play_data(&state.base, cid) }))
}

async fn bangumi_season(
    State(state): State<TestState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response<Body> {
    record(&state, &uri, &headers);
    json_response(json!({
        "code": 0,
        "result": {
            "season_id": 1,
            "media_id": 2,
            "season_title": "番剧标题",
            "title": "番剧",
            "cover": format!("{}/season.jpg", state.base),
            "episodes": [
                { "aid": 1, "bvid": "BVep1", "cid": 333, "ep_id": 123, "id": 123, "title": "1", "long_title": "第一集", "duration": 120, "section_type": 0 },
                { "aid": 2, "bvid": "BVep2", "cid": 444, "ep_id": 124, "id": 124, "title": "2", "long_title": "第二集", "duration": 120, "section_type": 0 }
            ]
        }
    }))
}

async fn bangumi_playurl(
    State(state): State<TestState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response<Body> {
    record(&state, &uri, &headers);
    let cid = query
        .get("cid")
        .and_then(|cid| cid.parse().ok())
        .unwrap_or(333);
    json_response(json!({ "code": 0, "result": play_data(&state.base, cid) }))
}

async fn collection(
    State(state): State<TestState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response<Body> {
    record(&state, &uri, &headers);
    let page = query
        .get("page_num")
        .and_then(|page| page.parse::<u32>().ok())
        .unwrap_or(1);
    json_response(json!({
        "code": 0,
        "data": {
            "meta": { "name": "收藏夹" },
            "archives": if page == 1 { vec![json!({ "bvid": "BVcol1" }), json!({ "bvid": "BVcol2" })] } else { Vec::<Value>::new() }
        }
    }))
}

async fn cheese_season(
    State(state): State<TestState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response<Body> {
    record(&state, &uri, &headers);
    json_response(json!({
        "code": 0,
        "data": { "title": "课程标题", "episodes": [{ "id": 777, "title": "第一课", "duration": 100 }] }
    }))
}

async fn cheese_playurl(
    State(state): State<TestState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response<Body> {
    record(&state, &uri, &headers);
    json_response(json!({ "code": 0, "data": play_data(&state.base, 777) }))
}

async fn gongfang_title(
    State(state): State<TestState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response<Body> {
    record(&state, &uri, &headers);
    Response::builder()
        .status(StatusCode::OK)
        .body(Body::from(r#"{"itemsName":"课程资料"}"#))
        .unwrap()
}

async fn gongfang_detail(
    State(state): State<TestState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Json(_body): Json<Value>,
) -> Response<Body> {
    record(&state, &uri, &headers);
    json_response(json!({
        "code": 0,
        "data": { "shipOrderDetails": [{ "fileContentType": ".pdf", "fileName": "讲义.pdf", "shipOrderDetailsId": 42 }] }
    }))
}

async fn gongfang_download(
    State(state): State<TestState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Json(_body): Json<Value>,
) -> Response<Body> {
    record(&state, &uri, &headers);
    json_response(
        json!({ "code": 0, "data": { "url": format!("{}/download/file.pdf", state.base) } }),
    )
}

fn play_data(base: &str, cid: u64) -> Value {
    json!({
        "accept_quality": [80, 64],
        "accept_description": ["1080P", "720P"],
        "support_formats": [
            { "quality": 80, "new_description": "1080P" },
            { "quality": 64, "new_description": "720P" }
        ],
        "dash": {
            "video": [
                { "id": 80, "baseUrl": format!("{base}/media/{cid}-80-h264.m4s"), "bandwidth": 1000, "width": 1920, "height": 1080, "codecid": 7 },
                { "id": 80, "baseUrl": format!("{base}/media/{cid}-80.m4s"), "bandwidth": 900, "width": 1920, "height": 1080, "codecid": 12 },
                { "id": 64, "baseUrl": format!("{base}/media/{cid}-64.m4s"), "bandwidth": 500, "width": 1280, "height": 720, "codecid": 7 }
            ],
            "audio": [{ "id": 30280, "baseUrl": format!("{base}/audio/{cid}.m4s"), "bandwidth": 128000 }]
        }
    })
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
    state.requests.lock().unwrap().push(value);
}

fn json_response(value: Value) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}
