use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use axum::http;
use knot_git::{Layout, Repo, Staging};
use knot_pack::{FetchError, HaveOids, PackLimits, WantOids, ingest_pack, local_pack, local_refs};
use knot_runtime::{FakeHttp, HttpRequest, HttpResponse, HttpTransport};
use knot_types::{Oid, RefName, RepoDid};
use url::Url;

mod common;
use common::{commit, must, pack_objects};

fn seed_source(dir: &Path) -> PathBuf {
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    must(&work, &["init", "-q", "-b", "main"]);
    commit(&work, "reef.txt", "kelp forest\n", "first");
    commit(&work, "tide.txt", "rock pool\n", "second");
    must(&work, &["tag", "v1"]);
    must(&work, &["branch", "anemone"]);
    let bare = dir.join("source.git");
    must(
        dir,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    bare
}

fn stock_git_server(repo: PathBuf) -> Arc<dyn HttpTransport> {
    Arc::new(FakeHttp::new(move |request: &HttpRequest| {
        let response = |body: Vec<u8>| {
            Ok(HttpResponse {
                status: http::StatusCode::OK,
                headers: http::HeaderMap::new(),
                body: body.into(),
            })
        };
        if request.url.path().ends_with("/info/refs") {
            let out = knot_fixtures::command(&repo)
                .args([
                    "upload-pack",
                    "--stateless-rpc",
                    "--http-backend-info-refs",
                    ".",
                ])
                .env("GIT_PROTOCOL", "version=2")
                .output()
                .expect("git upload-pack advertises");
            assert!(out.status.success());
            let mut body = b"001e# service=git-upload-pack\n0000".to_vec();
            body.extend_from_slice(&out.stdout);
            return response(body);
        }
        let mut child = knot_fixtures::command(&repo)
            .args(["upload-pack", "--stateless-rpc", "."])
            .env("GIT_PROTOCOL", "version=2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("git upload-pack serves");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.body.as_deref().unwrap_or_default())
            .unwrap();
        let out = child.wait_with_output().expect("git upload-pack finishes");
        assert!(
            out.status.success(),
            "upload-pack: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        response(out.stdout)
    }))
}

fn knot_server(repo_path: PathBuf) -> Arc<dyn HttpTransport> {
    Arc::new(FakeHttp::new(move |request: &HttpRequest| {
        let repo = Repo::open(&repo_path).unwrap();
        let body = if request.url.path().ends_with("/info/refs") {
            knot_pack::advertise_upload(&repo).unwrap()
        } else {
            knot_pack::upload_pack(&repo, request.body.as_deref().unwrap_or_default()).unwrap()
        };
        Ok(HttpResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: body.into(),
        })
    }))
}

fn base_url() -> Url {
    Url::parse("https://kelp.oyster.cafe/did:plc:squid/uni").unwrap()
}

const CAP: u64 = 64 * 1024 * 1024;

fn refnames(records: &[knot_git::RefRecord]) -> Vec<&str> {
    records.iter().map(|record| record.name.as_str()).collect()
}

async fn clone_through(http: &dyn HttpTransport, source: &Repo, target: &Repo) {
    let refs = knot_pack::remote_refs(http, &base_url(), &["HEAD", "refs/heads/", "refs/tags/"])
        .await
        .unwrap();
    assert_eq!(
        refs.head_symref.as_ref().map(RefName::as_str),
        Some("refs/heads/main")
    );
    let mut expected = source.advertised_refs().unwrap().to_vec();
    expected.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
    let mut got = refs.refs.clone();
    got.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
    assert_eq!(refnames(&got), refnames(&expected));
    assert_eq!(got, expected);

    let pack = knot_pack::remote_pack(
        http,
        &base_url(),
        &WantOids::new(refs.tips()),
        &HaveOids::default(),
        CAP,
    )
    .await
    .unwrap();
    ingest_pack(
        &target.objects_dir(),
        &pack,
        &PackLimits::default(),
        target.object_format().kind(),
    )
    .unwrap();
    let closure = target
        .select_pack_objects(
            knot_git::Wants::new(&refs.tips()),
            knot_git::Haves::new(&[]),
        )
        .unwrap();
    assert!(closure.iter().all(|oid| target.contains(*oid)));
}

#[tokio::test]
async fn the_client_clones_from_both_stock_git_and_native_knot_servers() {
    let dir = tempfile::tempdir().unwrap();
    let source_path = seed_source(dir.path());
    let source = Repo::open(&source_path).unwrap();

    let stock = Repo::create(dir.path().join("fork-stock.git")).unwrap();
    clone_through(
        stock_git_server(source_path.clone()).as_ref(),
        &source,
        &stock,
    )
    .await;

    let knot = Repo::create(dir.path().join("fork-knot.git")).unwrap();
    clone_through(knot_server(source_path.clone()).as_ref(), &source, &knot).await;
}

#[tokio::test]
async fn an_incremental_pull_completes_through_staging() {
    let dir = tempfile::tempdir().unwrap();
    let source_path = seed_source(dir.path());
    let source = Repo::open(&source_path).unwrap();
    let clone = Repo::create(dir.path().join("fork.git")).unwrap();
    let http = knot_server(source_path.clone());
    clone_through(http.as_ref(), &source, &clone).await;
    let main = RefName::new("refs/heads/main").unwrap();
    let old_tip = source.find_ref(&main).unwrap().unwrap();
    clone
        .update_ref(&knot_git::RefUpdate::Create {
            name: main.clone(),
            new: old_tip,
        })
        .unwrap();

    let work = dir.path().join("work");
    commit(&work, "spray.txt", "salt\n", "third");
    let new_hex = must(&work, &["rev-parse", "HEAD"]);
    let new_tip = Oid::from_hex(&new_hex).unwrap();
    let oids: Vec<String> = must(
        &work,
        &[
            "rev-list",
            "--objects",
            &new_hex,
            "--not",
            &old_tip.to_hex(),
        ],
    )
    .lines()
    .filter_map(|line| line.split_whitespace().next())
    .map(str::to_string)
    .collect();
    let pack = pack_objects(&work, &oids);
    ingest_pack(
        &source.objects_dir(),
        &pack,
        &PackLimits::default(),
        source.object_format().kind(),
    )
    .unwrap();
    source
        .update_ref(&knot_git::RefUpdate::Update {
            name: main.clone(),
            old: old_tip,
            new: new_tip,
        })
        .unwrap();
    assert_ne!(old_tip, new_tip);

    let pack = knot_pack::remote_pack(
        http.as_ref(),
        &base_url(),
        &WantOids::new(vec![new_tip]),
        &HaveOids::new(vec![old_tip]),
        CAP,
    )
    .await
    .unwrap();
    let staging = Staging::new(&clone).unwrap();
    ingest_pack(
        &staging.repo().objects_dir(),
        &pack,
        &PackLimits::default(),
        clone.object_format().kind(),
    )
    .unwrap();
    let closure = staging
        .repo()
        .select_pack_objects(
            knot_git::Wants::new(&[new_tip]),
            knot_git::Haves::new(&[old_tip]),
        )
        .unwrap();
    assert!(closure.iter().all(|oid| staging.repo().contains(*oid)));
    staging.migrate_into(&clone).unwrap();
    assert!(clone.contains(new_tip));
}

#[tokio::test]
async fn a_tiny_pack_limit_refuses_both_remote_and_local_transfers() {
    let dir = tempfile::tempdir().unwrap();
    let source_path = seed_source(dir.path());
    let source = Repo::open(&source_path).unwrap();
    let tips: Vec<Oid> = source
        .advertised_refs()
        .unwrap()
        .iter()
        .map(|record| record.target)
        .collect();

    let http = knot_server(source_path);
    let remote = knot_pack::remote_pack(
        http.as_ref(),
        &base_url(),
        &WantOids::new(tips.clone()),
        &HaveOids::default(),
        16,
    )
    .await;
    assert!(matches!(
        remote,
        Err(FetchError::PackTooLarge { limit: 16 })
    ));

    let local = local_pack(&source, &WantOids::new(tips), &HaveOids::default(), 16);
    assert!(matches!(local, Err(FetchError::PackTooLarge { limit: 16 })));
}

#[test]
fn local_refs_and_pack_mirror_a_same_knot_source() {
    let dir = tempfile::tempdir().unwrap();
    let source_path = seed_source(dir.path());
    let source = Repo::open(&source_path).unwrap();
    let hidden = RefName::new("refs/hidden/feature/main").unwrap();
    let main = RefName::new("refs/heads/main").unwrap();
    let tip = source.find_ref(&main).unwrap().unwrap();
    source
        .update_ref(&knot_git::RefUpdate::Create {
            name: hidden,
            new: tip,
        })
        .unwrap();

    let refs = local_refs(&source, &["HEAD", "refs/heads/", "refs/tags/"]).unwrap();
    assert_eq!(
        refs.head_symref.as_ref().map(RefName::as_str),
        Some("refs/heads/main")
    );
    assert!(
        refs.refs
            .iter()
            .all(|record| !record.name.as_str().starts_with("refs/hidden/")),
        "hidden ref must never leave source repo through a fork"
    );

    let layout = Layout::new(dir.path().join("scan"));
    let fork = layout
        .create(&RepoDid::new("did:plc:limpet").unwrap())
        .unwrap();
    let pack = local_pack(
        &source,
        &WantOids::new(refs.tips()),
        &HaveOids::default(),
        CAP,
    )
    .unwrap();
    ingest_pack(
        &fork.objects_dir(),
        &pack,
        &PackLimits::default(),
        fork.object_format().kind(),
    )
    .unwrap();
    let closure = fork
        .select_pack_objects(
            knot_git::Wants::new(&refs.tips()),
            knot_git::Haves::new(&[]),
        )
        .unwrap();
    assert!(closure.iter().all(|oid| fork.contains(*oid)));
}
