mod common;

use axum::body::Body;
use futures::StreamExt;
use http::{Request, StatusCode, header};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use knot_lfs::{LfsOid, LfsStore};
use knot_types::RepoDid;

use common::{World, empty_repo};

const OBJECT_BYTES: usize = 8 * 1024 * 1024;
const OBJECTS: usize = 4;
const DOWNLOADERS: usize = 12;
const ROUNDS: usize = 4;
const PAGE_BYTES: u64 = 4096;
const PEAK_CEILING: u64 = 512 * 1024 * 1024;
const GROWTH_SLACK: u64 = 64 * 1024 * 1024;

fn rss_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").expect("/proc/self/statm is readable");
    statm
        .split_whitespace()
        .nth(1)
        .and_then(|pages| pages.parse::<u64>().ok())
        .map(|pages| pages * PAGE_BYTES)
        .expect("statm lists the resident page count")
}

fn incompressible(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state & 0xff) as u8
        })
        .collect()
}

fn seed_objects(world: &World, repo: &RepoDid) -> Vec<LfsOid> {
    let store = &world.state.lfs.as_ref().unwrap().handle.store;
    (0..OBJECTS)
        .map(|index| {
            let body = incompressible(OBJECT_BYTES, 0x5eed_0000 + index as u64);
            let oid = LfsOid::from_digest(Sha256::digest(&body).into());
            store
                .put(
                    repo,
                    &oid,
                    knot_lfs::ClaimedSize::new(body.len() as u64),
                    &mut &body[..],
                )
                .unwrap();
            oid
        })
        .collect()
}

async fn download(world: &World, did: &RepoDid, oid: &LfsOid, tag: &str) {
    let request = Request::get(format!("/{did}/info/lfs/objects/{oid}"))
        .body(Body::empty())
        .unwrap();
    let response = world.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{tag}");
    let (streamed, hasher) = response
        .into_body()
        .into_data_stream()
        .fold(
            (0u64, Sha256::new()),
            |(streamed, mut hasher), chunk| async move {
                let chunk = chunk.unwrap();
                hasher.update(&chunk);
                (streamed + chunk.len() as u64, hasher)
            },
        )
        .await;
    assert_eq!(streamed, OBJECT_BYTES as u64, "{tag}");
    assert_eq!(
        LfsOid::from_digest(hasher.finalize().into()),
        oid.clone(),
        "{tag}: downloaded bytes must hash to the requested oid"
    );
}

async fn batch(world: &World, did: &RepoDid, oids: &[LfsOid], tag: &str) {
    let objects: Vec<serde_json::Value> = oids
        .iter()
        .map(|oid| serde_json::json!({"oid": oid.as_str(), "size": OBJECT_BYTES}))
        .collect();
    let body = serde_json::json!({"operation": "download", "objects": objects}).to_string();
    let request = Request::post(format!("/{did}/info/lfs/objects/batch"))
        .header(header::CONTENT_TYPE, "application/vnd.git-lfs+json")
        .body(Body::from(body))
        .unwrap();
    let response = world.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{tag}: {}",
        String::from_utf8_lossy(&body)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sustained_anonymous_downloads_stay_bounded_and_leak_nothing() {
    let world = World::unshed();
    let (did, _bare, _work) = empty_repo(&world, "nautilus");
    let oids = seed_objects(&world, &did);

    let storm = |round: usize| {
        let world = &world;
        let did = &did;
        let oids = &oids;
        async move {
            futures::future::join_all((0..DOWNLOADERS).map(|task| {
                let oid = oids[task % OBJECTS].clone();
                async move {
                    let tag = format!("round {round} task {task}");
                    batch(world, did, oids, &tag).await;
                    download(world, did, &oid, &tag).await;
                }
            }))
            .await;
        }
    };

    storm(0).await;
    let settled = rss_bytes();

    let peaks: Vec<u64> = futures::stream::iter(1..ROUNDS)
        .then(|round| {
            let storm = &storm;
            async move {
                storm(round).await;
                rss_bytes()
            }
        })
        .collect()
        .await;

    let peak = peaks.iter().copied().max().unwrap_or(settled);
    assert!(
        peak < PEAK_CEILING,
        "concurrent downloads peaked at {peak} bytes, ceiling {PEAK_CEILING}"
    );
    let last = *peaks.last().unwrap_or(&settled);
    assert!(
        last <= settled + GROWTH_SLACK,
        "rss grew from {settled} to {last} across rounds, downloads are leaking"
    );
}
