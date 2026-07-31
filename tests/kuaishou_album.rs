use std::{collections::BTreeMap, io::Read, sync::Arc, time::Duration};

use axum::{Json, Router, extract::State, routing::get, routing::post};
use easydl::{
    domain::{DownloadEvent, DownloadTask, MediaKind},
    downloader::{CollisionPolicy, ExecuteOptions, execute_plan},
    platforms::{AlbumOutput, PlanSelection, ResolveOptions, kuaishou::KuaishouResolver},
};
use serde_json::json;
use tempfile::tempdir;
use tokio::{net::TcpListener, task::JoinHandle};
use zip::ZipArchive;

#[tokio::test]
async fn resolves_selected_atlas_to_partial_plan_shapes_and_artifacts() {
    let server = AlbumServer::start().await;
    let resolver = server.resolver();
    let media = resolver.resolve("3xatlas123").await.unwrap();

    let inspect = media.inspect();
    assert_eq!(inspect.media.kind, MediaKind::Album);
    assert_eq!(inspect.choices.images.len(), 2);
    assert_eq!(inspect.choices.images[0].id, "image-1");
    assert_eq!(inspect.choices.images[0].index, 1);
    let inspect_json = serde_json::to_string(&inspect).unwrap();
    assert!(!inspect_json.contains("atlas-1.jpg"));
    assert!(!inspect_json.contains("music.m4a"));

    let zip_plan = media
        .create_plan(&PlanSelection {
            images: Some(vec![1]),
            album_output: AlbumOutput::Zip,
            ..PlanSelection::default()
        })
        .unwrap();
    let DownloadTask::Archive {
        entries, output, ..
    } = &zip_plan.tasks[0]
    else {
        panic!("atlas should create an archive task");
    };
    assert_eq!(output.relative_path, "Album Title.zip");
    assert_eq!(
        entries
            .iter()
            .map(|entry| (entry.id.as_str(), entry.path.as_str()))
            .collect::<Vec<_>>(),
        [("image-2", "02.jpg"), ("music", "music.m4a")]
    );
    assert_eq!(entries[0].sources[0].url, server.url("/atlas-2.jpg"));

    let zip_output = tempdir().unwrap();
    let summary = execute_plan(
        zip_plan,
        options(zip_output.path()),
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();
    assert!(summary.success);
    assert_eq!(
        read_archive(&zip_output.path().join("Album Title.zip")),
        BTreeMap::from([
            ("02.jpg".to_owned(), b"atlas-two".to_vec()),
            ("music.m4a".to_owned(), b"album-music".to_vec()),
        ])
    );

    let directory_plan = media
        .create_plan(&PlanSelection {
            output: Some("album-dir".to_owned()),
            album_output: AlbumOutput::Directory,
            ..PlanSelection::default()
        })
        .unwrap();
    assert_eq!(directory_plan.tasks.len(), 3);
    assert_eq!(
        directory_plan
            .tasks
            .iter()
            .map(|task| task.output().relative_path.as_str())
            .collect::<Vec<_>>(),
        [
            "album-dir/01.jpg",
            "album-dir/02.jpg",
            "album-dir/music.m4a"
        ]
    );

    let directory_output = tempdir().unwrap();
    let summary = execute_plan(
        directory_plan,
        options(directory_output.path()),
        Arc::new(|_: &DownloadEvent| {}),
    )
    .await
    .unwrap();
    assert!(summary.success);
    assert_eq!(
        tokio::fs::read(directory_output.path().join("album-dir/01.jpg"))
            .await
            .unwrap(),
        b"atlas-one"
    );
    assert_eq!(
        tokio::fs::read(directory_output.path().join("album-dir/02.jpg"))
            .await
            .unwrap(),
        b"atlas-two"
    );
    assert_eq!(
        tokio::fs::read(directory_output.path().join("album-dir/music.m4a"))
            .await
            .unwrap(),
        b"album-music"
    );
}

#[tokio::test]
async fn resolves_single_picture_to_a_file_task() {
    let server = AlbumServer::start().await;
    let media = server.resolver().resolve("3xpic123").await.unwrap();
    let inspect = media.inspect();
    assert_eq!(inspect.media.kind, MediaKind::Album);
    assert_eq!(inspect.choices.images.len(), 1);

    let plan = media.create_plan(&PlanSelection::default()).unwrap();
    let DownloadTask::File {
        sources, output, ..
    } = &plan.tasks[0]
    else {
        panic!("single picture should create a file task");
    };
    assert_eq!(sources[0].url, server.url("/single.jpg"));
    assert_eq!(output.relative_path, "Only One.jpg");

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
        tokio::fs::read(output_root.path().join("Only One.jpg"))
            .await
            .unwrap(),
        b"single-picture"
    );

    let error = media
        .create_plan(&PlanSelection {
            images: Some(vec![1]),
            ..PlanSelection::default()
        })
        .unwrap_err();
    assert_eq!(error.info.code, "selection.image_out_of_range");
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
struct AlbumState {
    base: String,
}

struct AlbumServer {
    base: String,
    task: JoinHandle<()>,
}

impl AlbumServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = AlbumState { base: base.clone() };
        let app = Router::new()
            .route("/photo/simple/info", post(photo_info))
            .route("/atlas-1.jpg", get(|| async { "atlas-one" }))
            .route("/atlas-2.jpg", get(|| async { "atlas-two" }))
            .route("/music.m4a", get(|| async { "album-music" }))
            .route("/single.jpg", get(|| async { "single-picture" }))
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, task }
    }

    fn resolver(&self) -> KuaishouResolver {
        KuaishouResolver::with_api_url(
            ResolveOptions {
                proxy: None,
                timeout: Duration::from_secs(5),
                ..ResolveOptions::default()
            },
            &self.url("/photo/simple/info"),
        )
        .unwrap()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

impl Drop for AlbumServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn photo_info(
    State(state): State<AlbumState>,
    Json(request): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    match request["photoId"].as_str().unwrap_or_default() {
        "3xatlas123" => Json(json!({
            "result": 1,
            "photo": {
                "photoId": "3xatlas123",
                "photoType": "HORIZONTAL_ATLAS",
                "caption": "Album Title",
                "userName": "作者",
                "coverUrls": [{ "url": format!("{}/cover.jpg", state.base) }]
            },
            "atlas": {
                "cdnList": [{ "cdn": "p23.a.yximgs.com" }],
                "list": [
                    format!("{}/atlas-1.jpg", state.base),
                    format!("{}/atlas-2.jpg", state.base)
                ],
                "size": [
                    { "w": 1080, "h": 1184 },
                    { "w": 1080, "h": 1164 }
                ],
                "music": format!("{}/music.m4a", state.base),
                "musicCdnList": [{ "cdn": "hwmov.a.yximgs.com" }]
            }
        })),
        "3xpic123" => Json(json!({
            "result": 1,
            "photo": {
                "photoId": "3xpic123",
                "photoType": "SINGLE_PICTURE",
                "caption": "Only One",
                "coverUrls": [{ "url": format!("{}/single.jpg", state.base) }]
            }
        })),
        other => Json(json!({
            "result": 0,
            "message": format!("unknown fixture photoId: {other}")
        })),
    }
}
