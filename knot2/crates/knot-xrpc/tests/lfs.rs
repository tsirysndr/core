mod common;

use axum::body::Body;
use http::{Request, StatusCode, header};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use knot_lfs::{LfsOid, LfsStore};
use knot_types::RepoDid;

use common::{OWNER, World, empty_repo, get};

const MEDIA: &[u8] = b"\xff\x00heavy media bytes that live outside the odb";

fn seeded_object(world: &World, repo: &RepoDid) -> (LfsOid, usize) {
    let oid = LfsOid::from_digest(Sha256::digest(MEDIA).into());
    world
        .state
        .lfs
        .as_ref()
        .unwrap()
        .handle
        .store
        .put(
            repo,
            &oid,
            knot_lfs::ClaimedSize::new(MEDIA.len() as u64),
            &mut &MEDIA[..],
        )
        .unwrap();
    (oid, MEDIA.len())
}

fn absent_oid() -> LfsOid {
    LfsOid::from_digest(Sha256::digest(b"never uploaded anywhere").into())
}

async fn post_batch(world: &World, path: &str, body: String) -> (StatusCode, serde_json::Value) {
    let request = Request::post(path)
        .header(header::CONTENT_TYPE, "application/vnd.git-lfs+json")
        .body(Body::from(body))
        .unwrap();
    let response = world.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

fn batch_body(operation: &str, oids: &[(String, u64)]) -> String {
    let objects: Vec<serde_json::Value> = oids
        .iter()
        .map(|(oid, size)| serde_json::json!({"oid": oid, "size": size}))
        .collect();
    serde_json::json!({
        "operation": operation,
        "transfers": ["basic", "ssh"],
        "objects": objects,
        "hash_algo": "sha256",
    })
    .to_string()
}

#[tokio::test]
async fn the_batch_download_surface_answers_every_addressing_form() {
    let world = World::new();
    let (did, _bare, _work) = empty_repo(&world, "barnacle");
    let (oid, size) = seeded_object(&world, &did);
    let missing = absent_oid();

    let body = batch_body(
        "download",
        &[
            (oid.as_str().to_string(), size as u64),
            (missing.as_str().to_string(), 9),
        ],
    );
    let paths = [
        format!("/{did}/info/lfs/objects/batch"),
        format!("/{did}.git/info/lfs/objects/batch"),
        format!("/{OWNER}/barnacle/info/lfs/objects/batch"),
        format!("/{OWNER}/barnacle.git/info/lfs/objects/batch"),
    ];
    futures::future::join_all(paths.iter().map(|path| {
        let world = &world;
        let body = body.clone();
        let oid = oid.clone();
        async move {
            let (status, json) = post_batch(world, path, body).await;
            assert_eq!(status, StatusCode::OK, "batch at {path}");
            assert_eq!(json["transfer"], "basic");
            assert_eq!(json["hash_algo"], "sha256");
            let objects = json["objects"].as_array().unwrap();
            assert_eq!(objects.len(), 2);
            assert_eq!(objects[0]["oid"], oid.as_str());
            assert_eq!(objects[0]["size"], size as u64);
            assert_eq!(objects[0]["authenticated"], true);
            let href = objects[0]["actions"]["download"]["href"].as_str().unwrap();
            assert!(
                href.ends_with(&format!("/info/lfs/objects/{oid}")),
                "href {href}"
            );
            assert!(href.starts_with("https://"), "href {href}");
            assert!(objects[0].get("error").is_none());
            assert_eq!(objects[1]["error"]["code"], 404);
            assert!(objects[1].get("actions").is_none());
        }
    }))
    .await;

    let upload = batch_body("upload", &[(oid.as_str().to_string(), size as u64)]);
    let (status, _) = post_batch(&world, &format!("/{did}/info/lfs/objects/batch"), upload).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "an unauthenticated HTTP upload batch is challenged for credentials, never answered"
    );
}

#[tokio::test]
async fn the_object_route_streams_ranges_and_stays_anonymous() {
    let world = World::new();
    let (did, _bare, _work) = empty_repo(&world, "limpet");
    let (oid, size) = seeded_object(&world, &did);

    let request = Request::get(format!("/{did}/info/lfs/objects/{oid}"))
        .body(Body::empty())
        .unwrap();
    let response = world.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/octet-stream"
    );
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&bytes[..], MEDIA);

    let request = Request::get(format!("/{OWNER}/limpet.git/info/lfs/objects/{oid}"))
        .header(header::RANGE, "bytes=3-6")
        .body(Body::empty())
        .unwrap();
    let response = world.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&bytes[..], &MEDIA[3..=6]);

    let request = Request::get(format!("/{did}/info/lfs/objects/{oid}"))
        .header(header::IF_NONE_MATCH, format!("\"{oid}\""))
        .body(Body::empty())
        .unwrap();
    let response = world.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        response.headers()[header::ETAG],
        format!("\"{oid}\"").as_str()
    );
    let _ = size;
}

#[tokio::test]
async fn missing_and_hostile_objects_get_typed_404s() {
    let world = World::new();
    let (did, _bare, _work) = empty_repo(&world, "scallop");
    seeded_object(&world, &did);

    let absent = absent_oid();
    let cases = [
        format!("/{did}/info/lfs/objects/{absent}"),
        format!("/{did}/info/lfs/objects/deadbeef"),
        format!("/{did}/info/lfs/objects/..%2f..%2fetc%2fpasswd"),
        format!("/did:plc:nowhere/info/lfs/objects/{absent}"),
    ];
    futures::future::join_all(cases.iter().map(|path| {
        let world = &world;
        async move {
            let request = Request::get(path).body(Body::empty()).unwrap();
            let response = world.router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "GET {path}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "application/vnd.git-lfs+json",
                "GET {path}"
            );
        }
    }))
    .await;

    let request = Request::get(format!("/{did}/info/lfs/objects/{absent}"))
        .header(header::IF_NONE_MATCH, "*")
        .body(Body::empty())
        .unwrap();
    let response = world.router.clone().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "an absent object must 404 even when revalidated"
    );
}

#[tokio::test]
async fn readiness_reflects_the_lfs_store() {
    let world = World::new();
    let (status, _, _) = get(&world, "/xrpc/_health").await;
    assert_eq!(status, StatusCode::OK, "a writable store reports ready");

    let incoming = world.lfs_dir.join(".incoming");
    std::fs::remove_dir(&incoming).unwrap();
    let (status, _, body) = get(&world, "/xrpc/_health").await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "an unwritable store reports unready: {}",
        String::from_utf8_lossy(&body)
    );

    std::fs::create_dir_all(&incoming).unwrap();
    let (status, _, _) = get(&world, "/xrpc/_health").await;
    assert_eq!(status, StatusCode::OK, "a recovered store reports ready");
}

#[tokio::test]
async fn hostile_batches_get_clean_typed_rejections() {
    let world = World::new();
    let (did, _bare, _work) = empty_repo(&world, "conch");
    let (oid, size) = seeded_object(&world, &did);
    let base = format!("/{did}/info/lfs/objects/batch");

    let (status, _) = post_batch(&world, &base, "not json at all".to_string()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let traversal = serde_json::json!({
        "operation": "download",
        "objects": [{"oid": "../../../../etc/passwd", "size": 1}],
    })
    .to_string();
    let (status, _) = post_batch(&world, &base, traversal).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let oversized_list: Vec<(String, u64)> = (0..1001)
        .map(|index| {
            let digest = Sha256::digest(index.to_string().as_bytes());
            (LfsOid::from_digest(digest.into()).as_str().to_string(), 1)
        })
        .collect();
    let (status, _) = post_batch(&world, &base, batch_body("download", &oversized_list)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let foreign_algo = serde_json::json!({
        "operation": "download",
        "objects": [{"oid": oid.as_str(), "size": size}],
        "hash_algo": "sha1",
    })
    .to_string();
    let (status, _) = post_batch(&world, &base, foreign_algo).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let no_common_transfer = serde_json::json!({
        "operation": "download",
        "transfers": ["tus"],
        "objects": [{"oid": oid.as_str(), "size": size}],
    })
    .to_string();
    let (status, _) = post_batch(&world, &base, no_common_transfer).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let over_limit = format!(
        r#"{{"operation":"download","objects":[],"pad":"{}"}}"#,
        "a".repeat(1024 * 1024 + 1)
    );
    let (status, _) = post_batch(&world, &base, over_limit).await;
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "a batch body over the limit is refused before buffering"
    );

    let absurd_size = serde_json::json!({
        "operation": "download",
        "objects": [{"oid": oid.as_str(), "size": u64::MAX}],
    })
    .to_string();
    let (status, json) = post_batch(&world, &base, absurd_size).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json["objects"][0]["size"], size as u64,
        "a download answer reports the stored size, ignoring the client's absurd claim"
    );
    assert!(json["objects"][0]["actions"]["download"]["href"].is_string());
}
