use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use knot_git::{
    EntryKind, Identity, Layout, NewCommit, RefUpdate, Repo, StagedAction, StagedChange,
};
use knot_pack::{PackError, PackLimits, RepoLookup, RepoResolver, RepoTarget, ingest_pack};
use knot_types::{AuthorName, BranchName, Email, ObjectFormat, Oid, RefName, RepoDid, UnixSeconds};
use tempfile::TempDir;
use tower::ServiceExt;

mod common;
use common::pkt;

fn serve_dids() -> Arc<dyn RepoResolver> {
    Arc::new(|target: &RepoTarget| match target {
        RepoTarget::Did(did) => RepoLookup::Hosted(did.clone()),
        RepoTarget::OwnerPath(_, _) => RepoLookup::Unhosted,
    })
}

fn commit_blob(repo: &Repo, path: &str, content: &[u8], parents: Vec<Oid>) -> Oid {
    let base = Oid::from(gix::ObjectId::empty_tree(repo.object_format().kind()));
    let id = Identity {
        name: AuthorName::new("nel"),
        email: Email::new("nel@oyster.cafe"),
        time: UnixSeconds::new(1_700_000_000),
        offset_seconds: 0,
    };
    let tree = repo
        .write_staged_tree(
            base,
            &[StagedChange {
                path: knot_types::RepoPath::new(path).unwrap(),
                action: StagedAction::Put {
                    content: content.to_vec(),
                    kind: EntryKind::Blob,
                },
            }],
        )
        .unwrap();
    repo.write_commit(&NewCommit {
        tree,
        parents,
        author: id.clone(),
        committer: id,
        message: "c".to_string(),
        extra_headers: Vec::new(),
    })
    .unwrap()
}

fn create_ref(repo: &Repo, name: &str, new: Oid) {
    repo.update_ref(&RefUpdate::Create {
        name: RefName::new(name).unwrap(),
        new,
    })
    .unwrap();
}

fn seed(format: ObjectFormat) -> (TempDir, Layout, RepoDid, Oid) {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::new(dir.path().join("scan"))
        .with_object_format(format)
        .with_default_branch(BranchName::new("main").unwrap());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let repo = layout.create(&did).unwrap();
    let tip = commit_blob(&repo, "reef.txt", b"kelp forest\n", Vec::new());
    create_ref(&repo, "refs/heads/main", tip);
    (dir, layout, did, tip)
}

fn v2_fetch_body(tip: Oid, server_option: bool) -> Vec<u8> {
    let mut body = pkt(b"command=fetch\n");
    body.extend_from_slice(b"0001");
    body.extend(pkt(format!("want {tip}\n").as_bytes()));
    if server_option {
        body.extend(pkt(b"server-option=ci-skip\n"));
    }
    body.extend(pkt(b"done\n"));
    body.extend_from_slice(b"0000");
    body
}

async fn post(router: &Router, did: &str, body: Vec<u8>) -> axum::http::Response<Body> {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/{did}/git-upload-pack"))
        .header("git-protocol", "version=2")
        .header(
            header::CONTENT_TYPE,
            "application/x-git-upload-pack-request",
        )
        .body(Body::from(body))
        .unwrap();
    router.clone().oneshot(request).await.unwrap()
}

async fn post_upload(router: &Router, did: &str, body: Vec<u8>) -> Vec<u8> {
    let response = post(router, did, body).await;
    assert_eq!(response.status(), StatusCode::OK);
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

fn hide_secret_ref(repo: &Repo) {
    let path = repo.git().git_dir().join("config");
    let mut config = std::fs::read_to_string(&path).unwrap();
    config.push_str("\n[uploadpack]\n\thideRefs = refs/heads/secret\n");
    std::fs::write(&path, config).unwrap();
}

fn refused(result: Result<Vec<u8>, PackError>) -> bool {
    matches!(result, Err(PackError::Protocol(_)))
}

#[test]
fn the_v2_advertisement_offers_server_option_and_a_fetch_using_it_is_served() {
    [ObjectFormat::SHA1, ObjectFormat::SHA256]
        .into_iter()
        .for_each(|format| {
            let (_dir, layout, did, tip) = seed(format);
            let repo = layout.open(&did).unwrap();
            let advert = knot_pack::advertise_upload(&repo).unwrap();
            assert!(
                String::from_utf8_lossy(&advert).contains("server-option"),
                "{format:?} advert"
            );
            let served = knot_pack::upload_pack(&repo, &v2_fetch_body(tip, true)).unwrap();
            assert!(
                String::from_utf8_lossy(&served).contains("packfile"),
                "{format:?} server-option fetch"
            );
        });
}

#[test]
fn upload_pack_refuses_malformed_unreachable_and_hidden_wants() {
    let (_dir, layout, did, tip) = seed(ObjectFormat::SHA1);
    let repo = layout.open(&did).unwrap();

    let dangling = commit_blob(&repo, "dangle.txt", b"dangling\n", Vec::new());
    assert!(
        refused(knot_pack::upload_pack(
            &repo,
            &v2_fetch_body(dangling, false)
        )),
        "unreachable want"
    );

    let mut malformed = pkt(b"command=fetch\n");
    malformed.extend_from_slice(b"0001");
    malformed.extend(pkt(b"want not-a-valid-object-id\n"));
    malformed.extend(pkt(b"done\n"));
    malformed.extend_from_slice(b"0000");
    assert!(
        refused(knot_pack::upload_pack(&repo, &malformed)),
        "malformed want line"
    );

    let writer = layout.open(&did).unwrap();
    let secret = commit_blob(&writer, "secret.txt", b"hidden\n", vec![tip]);
    create_ref(&writer, "refs/heads/secret", secret);
    hide_secret_ref(&writer);

    let repo = layout.open(&did).unwrap();
    let named = |scope| {
        repo.advertised_refs_for(scope)
            .unwrap()
            .iter()
            .any(|record| record.name.as_str() == "refs/heads/secret")
    };
    assert!(
        named(knot_git::AdvertScope::Receive),
        "hidden ref still public on receive advert"
    );
    assert!(
        !named(knot_git::AdvertScope::Upload),
        "hideRefs strips it from upload advert"
    );
    assert!(
        refused(knot_pack::upload_pack(&repo, &v2_fetch_body(secret, false))),
        "upload-hidden ref by oid"
    );
}

#[tokio::test]
async fn the_pack_cache_replays_then_invalidates_when_a_ref_is_hidden() {
    let (dir, layout, did, tip) = seed(ObjectFormat::SHA1);
    let writer = layout.open(&did).unwrap();
    let secret = commit_blob(&writer, "secret.txt", b"hidden\n", vec![tip]);
    create_ref(&writer, "refs/heads/secret", secret);

    let router = knot_pack::router(
        layout,
        serve_dids(),
        std::sync::Arc::new(knot_runtime::SystemClock),
    );

    let body = v2_fetch_body(tip, false);
    let first = post_upload(&router, did.as_str(), body.clone()).await;
    let second = post_upload(&router, did.as_str(), body).await;
    assert_eq!(
        first, second,
        "a cache hit replays the leader's bytes exactly"
    );
    let fork = Repo::create(dir.path().join("fork.git")).unwrap();
    ingest_pack(
        &fork.objects_dir(),
        &common::unsideband(&first),
        &PackLimits::default(),
        fork.object_format().kind(),
    )
    .unwrap();
    assert!(
        fork.contains(tip),
        "the cached pack contains the wanted tip"
    );

    let secret_body = v2_fetch_body(secret, false);
    let warm = post_upload(&router, did.as_str(), secret_body.clone()).await;
    assert!(
        !common::unsideband(&warm).is_empty(),
        "the visible secret want is served and cached"
    );

    hide_secret_ref(&writer);
    let after = post(&router, did.as_str(), secret_body).await.status();
    assert_eq!(
        after,
        StatusCode::BAD_REQUEST,
        "once hidden the cached pack isn't replayed"
    );
}

fn maint_opts() -> knot_maintenance::Options {
    knot_maintenance::Options {
        repack_max_objects: knot_maintenance::ObjectCount::new(1_000_000),
        geometric_factor: knot_maintenance::GeometricFactor::full_repack(),
        prune_grace: knot_maintenance::PruneGrace::from_secs(0),
        reflog_floor: knot_maintenance::ReflogRetention::from_secs(i64::MAX as u64 / 4),
        commit_graph: false,
        multi_pack_index: false,
        bitmap: true,
    }
}

fn pack_object_count(pack: &[u8]) -> u32 {
    assert_eq!(
        &pack[..4],
        b"PACK",
        "a served body begins with the pack signature"
    );
    u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]])
}

#[test]
fn the_bitmap_fast_path_serves_the_same_pack_as_the_object_walk() {
    [ObjectFormat::SHA1, ObjectFormat::SHA256]
        .into_iter()
        .for_each(|format| {
            let (dir, layout, did, tip) = seed(format);
            let repo = layout.open(&did).unwrap();
            let body = v2_fetch_body(tip, false);
            let walk = common::unsideband(&knot_pack::upload_pack(&repo, &body).unwrap());

            let now = UnixSeconds::new(1_700_000_500);
            assert!(
                knot_maintenance::run_repo(&repo, now, &maint_opts())
                    .unwrap()
                    .bitmap,
                "{format:?} seed packs a bitmap"
            );

            let fast = common::unsideband(&knot_pack::upload_pack(&repo, &body).unwrap());
            assert_eq!(
                pack_object_count(&fast),
                pack_object_count(&walk),
                "{format:?} reuse vs walk count"
            );

            let did = RepoDid::new("did:plc:clam").unwrap();
            let fork = Layout::new(dir.path().join("fork"))
                .with_object_format(format)
                .create(&did)
                .unwrap();
            ingest_pack(
                &fork.objects_dir(),
                &fast,
                &PackLimits::default(),
                fork.object_format().kind(),
            )
            .unwrap();
            let closure: std::collections::HashSet<Oid> = fork
                .select_pack_objects(knot_git::Wants::new(&[tip]), knot_git::Haves::new(&[]))
                .unwrap()
                .into_iter()
                .collect();
            assert!(
                fork.contains(tip) && closure.len() as u32 == pack_object_count(&fast),
                "{format:?} fast-path ingests as tip closure"
            );
        });
}
