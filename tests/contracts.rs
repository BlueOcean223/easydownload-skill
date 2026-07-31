use std::{collections::BTreeSet, fs, path::PathBuf};

use easydl::{
    bilibili_auth::{
        BilibiliLoginStatus, BilibiliLogoutResult, BilibiliQrPollResult, BilibiliQrSession,
    },
    domain::{DownloadEvent, DownloadPlan, InspectDocument},
    errors::ErrorDocument,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

#[test]
fn frozen_v2_fixture_manifest_matches_every_json_contract() {
    let directory = fixture_path("");
    let manifest = fs::read_to_string(directory.join("SHA256SUMS")).unwrap();
    let mut manifested = BTreeSet::new();
    for line in manifest.lines().filter(|line| !line.trim().is_empty()) {
        let (expected, name) = line
            .split_once("  ")
            .expect("contract checksum line should use sha256sum format");
        manifested.insert(name.to_owned());
        let actual = format!(
            "{:x}",
            Sha256::digest(fs::read(directory.join(name)).unwrap())
        );
        assert_eq!(actual, expected, "contract fixture changed: {name}");
    }
    let actual = fs::read_dir(&directory)
        .unwrap()
        .filter_map(|entry| {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            name.ends_with(".json").then_some(name)
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        manifested, actual,
        "SHA256SUMS must list every v2 JSON fixture"
    );
}

#[test]
fn inspect_fixture_round_trips() {
    assert_round_trip::<InspectDocument>("inspect-kuaishou-video.json");
}

#[test]
fn album_inspect_fixture_round_trips() {
    assert_round_trip::<InspectDocument>("inspect-kuaishou-album.json");
}

#[test]
fn bilibili_inspect_fixtures_round_trip() {
    assert_round_trip::<InspectDocument>("inspect-bilibili-playlist.json");
    assert_round_trip::<InspectDocument>("inspect-bilibili-gongfang.json");
}

#[test]
fn douyin_inspect_fixtures_round_trip() {
    assert_round_trip::<InspectDocument>("inspect-douyin-video.json");
    assert_round_trip::<InspectDocument>("inspect-douyin-album.json");
}

#[test]
fn xhs_inspect_fixtures_round_trip() {
    assert_round_trip::<InspectDocument>("inspect-xhs-video.json");
    assert_round_trip::<InspectDocument>("inspect-xhs-album.json");
}

#[test]
fn plan_fixture_round_trips_and_validates_at_fixture_time() {
    let (plan, expected) = read_fixture::<DownloadPlan>("plan-file.json");
    let now = OffsetDateTime::parse(
        "2026-07-30T00:05:00Z",
        &time::format_description::well_known::Rfc3339,
    )
    .expect("valid fixture time");
    plan.validate_at(now).expect("fixture plan should be valid");
    assert_eq!(serde_json::to_value(plan).unwrap(), expected);
}

#[test]
fn archive_plan_fixture_round_trips_and_validates_at_fixture_time() {
    let (plan, expected) = read_fixture::<DownloadPlan>("plan-archive.json");
    let now = OffsetDateTime::parse(
        "2026-07-30T00:05:00Z",
        &time::format_description::well_known::Rfc3339,
    )
    .expect("valid fixture time");
    plan.validate_at(now)
        .expect("archive fixture plan should be valid");
    assert_eq!(serde_json::to_value(plan).unwrap(), expected);
}

#[test]
fn mux_plan_fixture_round_trips_and_validates_at_fixture_time() {
    let (plan, expected) = read_fixture::<DownloadPlan>("plan-mux.json");
    let now = OffsetDateTime::parse(
        "2026-07-30T00:05:00Z",
        &time::format_description::well_known::Rfc3339,
    )
    .expect("valid fixture time");
    plan.validate_at(now)
        .expect("mux fixture plan should be valid");
    assert_eq!(serde_json::to_value(plan).unwrap(), expected);
}

#[test]
fn event_fixture_round_trips() {
    assert_round_trip::<Vec<DownloadEvent>>("events-file-success.json");
}

#[test]
fn bilibili_auth_control_fixtures_round_trip_without_secret_values() {
    assert_round_trip::<BilibiliQrSession>("auth-bilibili-session.json");
    let (poll_result, poll) = read_fixture::<BilibiliQrPollResult>("auth-bilibili-poll.json");
    assert_eq!(serde_json::to_value(poll_result).unwrap(), poll);
    assert!(poll.get("sessData").is_none());
    assert!(poll.get("cookieHeader").is_none());
    assert_round_trip::<BilibiliLoginStatus>("auth-bilibili-status.json");
    assert_round_trip::<BilibiliLogoutResult>("auth-bilibili-logout.json");
}

#[test]
fn structured_error_fixture_round_trips() {
    assert_round_trip::<ErrorDocument>("error-http-forbidden.json");
}

#[test]
fn plan_rejects_expired_documents_secrets_and_traversal() {
    let (mut plan, _) = read_fixture::<DownloadPlan>("plan-file.json");
    let after_expiry = plan.expires_at.unwrap() + Duration::seconds(1);
    let error = plan.validate_at(after_expiry).unwrap_err();
    assert_eq!(error.info.code, "plan.expired");

    plan.expires_at = None;
    {
        let easydl::domain::DownloadTask::File { sources, .. } = &mut plan.tasks[0] else {
            panic!("fixture should contain a file task");
        };
        sources[0]
            .headers
            .insert("Cookie".to_owned(), "SESSDATA=secret".to_owned());
    }
    let error = plan.validate().unwrap_err();
    assert_eq!(error.info.code, "plan.secret_header");

    {
        let easydl::domain::DownloadTask::File { sources, .. } = &mut plan.tasks[0] else {
            panic!("fixture should contain a file task");
        };
        sources[0].headers.remove("Cookie");
        sources[0]
            .headers
            .insert("Range".to_owned(), "bytes=0-1".to_owned());
    }
    let error = plan.validate().unwrap_err();
    assert_eq!(error.info.code, "plan.reserved_header");

    {
        let easydl::domain::DownloadTask::File { sources, .. } = &mut plan.tasks[0] else {
            panic!("fixture should contain a file task");
        };
        sources[0].headers.remove("Range");
    }
    let mut duplicate = plan.tasks[0].clone();
    let easydl::domain::DownloadTask::File { id, .. } = &mut duplicate else {
        panic!("fixture should contain a file task");
    };
    *id = "duplicate-output".to_owned();
    plan.tasks.push(duplicate);
    let error = plan.validate().unwrap_err();
    assert_eq!(error.info.code, "plan.invalid");
    plan.tasks.pop();

    let easydl::domain::DownloadTask::File { output, .. } = &mut plan.tasks[0] else {
        panic!("fixture should contain a file task");
    };
    output.relative_path = "../escape.mp4".to_owned();
    let error = plan.validate().unwrap_err();
    assert_eq!(error.info.code, "output.unsafe_path");
}

#[test]
fn archive_plan_rejects_unsafe_duplicate_and_overlapping_entries() {
    let (mut plan, _) = read_fixture::<DownloadPlan>("plan-archive.json");
    plan.expires_at = None;

    let mut unsafe_path = plan.clone();
    let easydl::domain::DownloadTask::Archive { entries, .. } = &mut unsafe_path.tasks[0] else {
        panic!("fixture should contain an archive task");
    };
    entries[0].path = "../escape.jpg".to_owned();
    assert_eq!(
        unsafe_path.validate().unwrap_err().info.code,
        "output.unsafe_path"
    );

    let mut duplicate_id = plan.clone();
    let easydl::domain::DownloadTask::Archive { entries, .. } = &mut duplicate_id.tasks[0] else {
        panic!("fixture should contain an archive task");
    };
    entries[1].id = entries[0].id.clone();
    assert_eq!(
        duplicate_id.validate().unwrap_err().info.code,
        "plan.invalid"
    );

    let mut overlapping_path = plan;
    let easydl::domain::DownloadTask::Archive { entries, .. } = &mut overlapping_path.tasks[0]
    else {
        panic!("fixture should contain an archive task");
    };
    entries[1].path = "01.jpg.part".to_owned();
    assert_eq!(
        overlapping_path.validate().unwrap_err().info.code,
        "plan.invalid"
    );
}

#[test]
fn mux_plan_rejects_duplicate_tracks_and_missing_roles() {
    let (mut plan, _) = read_fixture::<DownloadPlan>("plan-mux.json");
    plan.expires_at = None;

    let mut duplicate = plan.clone();
    let easydl::domain::DownloadTask::Mux { tracks, .. } = &mut duplicate.tasks[0] else {
        panic!("fixture should contain a mux task");
    };
    tracks[1].id = tracks[0].id.clone();
    assert_eq!(duplicate.validate().unwrap_err().info.code, "plan.invalid");

    let easydl::domain::DownloadTask::Mux { tracks, .. } = &mut plan.tasks[0] else {
        panic!("fixture should contain a mux task");
    };
    tracks[1].kind = easydl::domain::MuxTrackKind::Video;
    assert_eq!(plan.validate().unwrap_err().info.code, "plan.invalid");
}

#[test]
fn auth_ref_rejects_unknown_profiles_and_out_of_scope_origins() {
    let (mut plan, _) = read_fixture::<DownloadPlan>("plan-mux.json");
    plan.expires_at = None;
    plan.validate()
        .expect("fixture authRef origins should be valid");

    let mut wrong_host = plan.clone();
    let easydl::domain::DownloadTask::Mux { tracks, .. } = &mut wrong_host.tasks[0] else {
        panic!("fixture should contain a mux task");
    };
    tracks[0].sources[0].url = "https://evilbilivideo.com/video.m4s".to_owned();
    assert_eq!(
        wrong_host.validate().unwrap_err().info.code,
        "auth.scope_violation"
    );

    let mut insecure = plan.clone();
    let easydl::domain::DownloadTask::Mux { tracks, .. } = &mut insecure.tasks[0] else {
        panic!("fixture should contain a mux task");
    };
    tracks[0].sources[0].url = "http://www.bilibili.com/video.m4s".to_owned();
    assert_eq!(
        insecure.validate().unwrap_err().info.code,
        "auth.scope_violation"
    );

    let mut unknown = plan;
    let easydl::domain::DownloadTask::Mux { tracks, .. } = &mut unknown.tasks[0] else {
        panic!("fixture should contain a mux task");
    };
    tracks[0].sources[0].auth_ref = Some("unknown:default".to_owned());
    assert_eq!(
        unknown.validate().unwrap_err().info.code,
        "auth.unknown_ref"
    );
}

fn assert_round_trip<T>(name: &str)
where
    T: DeserializeOwned + Serialize,
{
    let (value, expected) = read_fixture::<T>(name);
    assert_eq!(serde_json::to_value(value).unwrap(), expected);
}

fn read_fixture<T: DeserializeOwned>(name: &str) -> (T, Value) {
    let path = fixture_path(name);
    let text = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!("failed to read {}: {error}", path.display());
    });
    let expected: Value = serde_json::from_str(&text).expect("fixture must be valid JSON");
    let value: T = serde_json::from_value(expected.clone()).expect("fixture must match schema");
    (value, expected)
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/v2")
        .join(name)
}
