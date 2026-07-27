use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path as UrlPath, Query, State};
use axum::http::header;
use axum::response::Response;
use axum::routing::{get, post};
use knot_git::Layout;
use knot_pack::PackError;
use knot_types::{ObjectFormat, RepoDid, RepoRkey};

mod common;
use common::{
    advance_via_receive, git, must, object_set, seed_branches_and_tag, serve_dids, spawn,
};

#[derive(Clone)]
struct Receive {
    layout: Layout,
}

fn open(layout: &Layout, did: &str, name: &str) -> Result<knot_git::Repo, PackError> {
    let did = RepoDid::new(did).map_err(|e| PackError::BadPath(e.to_string()))?;
    RepoRkey::new(name).map_err(|e| PackError::BadPath(e.to_string()))?;
    Ok(layout.open(&did)?)
}

fn raw_response(content_type: &'static str, body: Vec<u8>) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap()
}

async fn receive_info_refs(
    State(state): State<Receive>,
    UrlPath((did, name)): UrlPath<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, PackError> {
    let repo = open(&state.layout, &did, &name)?;
    match query.get("service").map(String::as_str) {
        Some("git-upload-pack") => Ok(raw_response(
            "application/x-git-upload-pack-advertisement",
            knot_pack::advertise_upload(&repo)?,
        )),
        Some("git-receive-pack") => Ok(raw_response(
            "application/x-git-receive-pack-advertisement",
            knot_pack::advertise_receive(&repo)?,
        )),
        _ => Err(PackError::UnsupportedService),
    }
}

async fn receive_upload(
    State(state): State<Receive>,
    UrlPath((did, name)): UrlPath<(String, String)>,
    body: Bytes,
) -> Result<Response, PackError> {
    let repo = open(&state.layout, &did, &name)?;
    let result = knot_pack::upload_pack(&repo, &body)?;
    Ok(raw_response("application/x-git-upload-pack-result", result))
}

async fn receive_receive(
    State(state): State<Receive>,
    UrlPath((did, name)): UrlPath<(String, String)>,
    body: Bytes,
) -> Result<Response, PackError> {
    let repo = open(&state.layout, &did, &name)?;
    let result = knot_pack::receive_pack(&repo, &body)?;
    Ok(raw_response(
        "application/x-git-receive-pack-result",
        result,
    ))
}

fn knot_receive_router(layout: Layout) -> Router {
    Router::new()
        .route("/{did}/{name}/info/refs", get(receive_info_refs))
        .route("/{did}/{name}/git-upload-pack", post(receive_upload))
        .route("/{did}/{name}/git-receive-pack", post(receive_receive))
        .layer(DefaultBodyLimit::disable())
        .with_state(Receive { layout })
}

fn clone_to(scratch: &Path, url: &str, dest: &Path) {
    must(scratch, &["clone", "-q", url, dest.to_str().unwrap()]);
}

fn publish(work: &Path, bares: [&Path; 2], refs: &[&str], head: &str) {
    bares.into_iter().for_each(|bare| {
        let push = [&["push", "-q", bare.to_str().unwrap()], refs].concat();
        must(work, &push);
        must(bare, &["symbolic-ref", "HEAD", head]);
    });
}

fn seed_cross_branch_deltas(work: &Path, knot_bare: &Path, canon_bare: &Path) {
    std::fs::create_dir_all(work).unwrap();
    must(work, &["init", "-q", "-b", "base"]);
    std::fs::write(work.join("README.md"), "thin\n").unwrap();
    must(work, &["add", "-A"]);
    must(work, &["commit", "-q", "-m", "base"]);
    let base = must(work, &["rev-parse", "HEAD"]);

    let bulk = |stem: usize, extra: &str| {
        let body: String = (0..200)
            .map(|line| format!("file {stem} line {line} with some shared payload\n"))
            .collect();
        format!("{body}{extra}")
    };
    let branch = |name: &str, extra: &str| {
        must(work, &["checkout", "-q", "-b", name, &base]);
        (0..8).for_each(|stem| {
            std::fs::write(work.join(format!("bulk-{stem}.txt")), bulk(stem, extra)).unwrap();
        });
        must(work, &["add", "-A"]);
        must(work, &["commit", "-q", "-m", name]);
    };
    branch("side", "");
    branch("other", "one divergent trailing line\n");
    must(work, &["checkout", "-q", "base"]);

    publish(
        work,
        [knot_bare, canon_bare],
        &["base", "side", "other"],
        "refs/heads/base",
    );
    [knot_bare, canon_bare].into_iter().for_each(|bare| {
        must(bare, &["repack", "-adf", "--window=50", "--depth=50"]);
    });
}

fn seed_nested_bares(work: &Path, knot_bare: &Path, canon_bare: &Path, format: ObjectFormat) {
    let fmt = format!("--object-format={}", format.capability());
    std::fs::create_dir_all(work).unwrap();
    must(work, &["init", &fmt, "-q", "-b", "main"]);
    std::fs::create_dir_all(work.join("a/c")).unwrap();
    std::fs::create_dir_all(work.join("b")).unwrap();
    std::fs::create_dir_all(work.join("deep/mid/bottom")).unwrap();
    std::fs::write(work.join("root.txt"), b"01234567").unwrap();
    std::fs::write(work.join("a/c/leaf"), b"hi\n").unwrap();
    std::fs::write(work.join("b/leaf"), b"hi\n").unwrap();
    std::fs::write(work.join("deep/mid/bottom/far.txt"), "x".repeat(20)).unwrap();
    must(work, &["add", "-A"]);
    must(work, &["commit", "-q", "-m", "c1"]);
    std::fs::write(work.join("deep/mid/bottom/far.txt"), "y".repeat(40)).unwrap();
    must(work, &["add", "-A"]);
    must(work, &["commit", "-q", "-m", "c2"]);
    publish(work, [knot_bare, canon_bare], &["main"], "refs/heads/main");
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Serve,
    Receive,
}

struct Servers {
    canon_bare: std::path::PathBuf,
    knot_bare: std::path::PathBuf,
    canon_url: String,
    knot_url: String,
}

async fn stand_up(
    scan: &Path,
    canon_root: &Path,
    did: &RepoDid,
    name: &RepoRkey,
    format: ObjectFormat,
    mode: Mode,
) -> Servers {
    let layout = Layout::new(scan).with_object_format(format);
    layout.create(did).unwrap();
    let knot_bare = layout.repo_path(did).unwrap();

    let canon_bare = canon_root.join(format!("{}.git", name.as_str()));
    let fmt = format!("--object-format={}", format.capability());
    must(
        Path::new("/tmp"),
        &["init", "--bare", &fmt, "-q", canon_bare.to_str().unwrap()],
    );

    let knot = match mode {
        Mode::Serve => {
            spawn(
                knot_pack::router(
                    layout,
                    serve_dids(),
                    std::sync::Arc::new(knot_runtime::SystemClock),
                ),
                "[::1]:0",
            )
            .await
        }
        Mode::Receive => spawn(knot_receive_router(layout), "[::1]:0").await,
    };
    let knot_url = match mode {
        Mode::Serve => format!("http://{knot}/{}", did.as_str()),
        Mode::Receive => format!("http://{knot}/{}/{}", did.as_str(), name.as_str()),
    };
    Servers {
        canon_url: format!("file://{}", canon_bare.to_str().unwrap()),
        canon_bare,
        knot_bare,
        knot_url,
    }
}

fn single_pack_idx(bare: &Path) -> std::path::PathBuf {
    std::fs::read_dir(bare.join("objects/pack"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "idx"))
        .expect("a single pack index after repack")
}

async fn clone_lifecycle(format: ObjectFormat, did: &str, name: &str) {
    let scan = tempfile::tempdir().unwrap();
    let canon_root = tempfile::tempdir().unwrap();
    let did = RepoDid::new(did).unwrap();
    let name = RepoRkey::new(name).unwrap();
    let s = stand_up(
        scan.path(),
        canon_root.path(),
        &did,
        &name,
        format,
        Mode::Serve,
    )
    .await;

    let scratch = tempfile::tempdir().unwrap();
    let work = scratch.path().join("work");
    seed_branches_and_tag(&work, [&s.knot_bare, &s.canon_bare], format);

    let canon = scratch.path().join("clone-canon");
    let knot = scratch.path().join("clone-knot");
    clone_to(scratch.path(), &s.canon_url, &canon);
    clone_to(scratch.path(), &s.knot_url, &knot);
    let parse = |dir: &Path, rev: &str| must(dir, &["rev-parse", rev]);
    assert_eq!(
        parse(&knot, "--show-object-format"),
        format.capability(),
        "{format:?} clone inherits object format"
    );
    assert_eq!(
        parse(&canon, "HEAD^{tree}"),
        parse(&knot, "HEAD^{tree}"),
        "{format:?} same checked-out tree"
    );
    assert_eq!(
        must(&canon, &["ls-files", "-s"]),
        must(&knot, &["ls-files", "-s"]),
        "{format:?} identical working tree"
    );
    assert_eq!(
        object_set(&canon),
        object_set(&knot),
        "{format:?} clone transfers canonical object set"
    );
    assert_eq!(
        must(&canon, &["branch", "-r"]),
        must(&knot, &["branch", "-r"]),
        "{format:?} same remote branches"
    );

    must(
        &s.knot_bare,
        &["-c", "repack.writeBitmaps=false", "repack", "-adq"],
    );
    let repo = knot_git::Repo::open(&s.knot_bare).unwrap();
    assert!(
        knot_git::write_bitmap(&repo, &single_pack_idx(&s.knot_bare)).unwrap(),
        "{format:?} single-pack repo gets a bitmap"
    );
    must(&s.knot_bare, &["rev-list", "--test-bitmap", "HEAD"]);

    let bm_canon = scratch.path().join("reclone-canon");
    let bm_knot = scratch.path().join("reclone-knot");
    clone_to(scratch.path(), &s.canon_url, &bm_canon);
    clone_to(scratch.path(), &s.knot_url, &bm_knot);
    assert_eq!(
        object_set(&bm_canon),
        object_set(&bm_knot),
        "{format:?} bitmap fast path serves canonical object set"
    );
    must(&bm_knot, &["fsck", "--strict"]);

    std::fs::write(work.join("incremental.txt"), "fetch me\n").unwrap();
    must(&work, &["add", "-A"]);
    must(&work, &["commit", "-q", "-m", "c4"]);
    let tip = must(&work, &["rev-parse", "HEAD"]);
    let old = must(&work, &["rev-parse", "HEAD~1"]);
    advance_via_receive(&s.knot_bare, &work, &old, &tip);
    must(
        &work,
        &["push", "-q", s.canon_bare.to_str().unwrap(), "main"],
    );
    must(&canon, &["fetch", "-q", "origin"]);
    must(&knot, &["fetch", "-q", "origin"]);
    assert_eq!(
        must(&knot, &["rev-parse", "origin/main"]),
        tip,
        "{format:?} fetch advances origin/main"
    );
    assert_eq!(
        object_set(&canon),
        object_set(&knot),
        "{format:?} incremental fetch transfers canonical object set"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn differential_clone_fetch_and_bitmap_serving_match_canonical() {
    clone_lifecycle(ObjectFormat::SHA1, "did:plc:squid", "scallop").await;
    clone_lifecycle(ObjectFormat::SHA256, "did:plc:nautilus", "whelk").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shallow_server_repo_is_served_and_the_clone_becomes_shallow() {
    let scan = tempfile::tempdir().unwrap();
    let canon_root = tempfile::tempdir().unwrap();
    let did = RepoDid::new("did:plc:periwinkle").unwrap();
    let name = RepoRkey::new("conch").unwrap();
    let s = stand_up(
        scan.path(),
        canon_root.path(),
        &did,
        &name,
        ObjectFormat::SHA1,
        Mode::Serve,
    )
    .await;

    let scratch = tempfile::tempdir().unwrap();
    let work = scratch.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    must(&work, &["init", "-q", "-b", "main"]);
    (1..=5).for_each(|n| {
        std::fs::write(work.join("history.txt"), format!("revision {n}\n")).unwrap();
        must(&work, &["add", "-A"]);
        must(&work, &["commit", "-q", "-m", &format!("c{n}")]);
    });
    let work_url = format!("file://{}", work.to_str().unwrap());
    [&s.knot_bare, &s.canon_bare].into_iter().for_each(|bare| {
        must(
            bare,
            &[
                "fetch",
                "-q",
                "--depth=2",
                &work_url,
                "main:refs/heads/main",
            ],
        );
        must(bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        assert!(bare.join("shallow").exists(), "server repo is shallow");
    });

    let canon = scratch.path().join("clone-canon");
    let knot = scratch.path().join("clone-knot");
    clone_to(scratch.path(), &s.canon_url, &canon);
    clone_to(scratch.path(), &s.knot_url, &knot);
    assert!(
        knot.join(".git/shallow").exists(),
        "clone of a shallow server is itself shallow"
    );
    must(&knot, &["fsck"]);
    assert_eq!(
        must(&canon, &["rev-list", "--count", "HEAD"]),
        must(&knot, &["rev-list", "--count", "HEAD"]),
        "both clones see the same clamped depth"
    );
    assert_eq!(
        object_set(&canon),
        object_set(&knot),
        "shallow clone transfers canonical object set"
    );
}

fn fetch_branch(scratch: &Path, url: &str, label: &str, branch: &str) -> BTreeSet<String> {
    let clone = scratch.join(format!("clone-{label}-{branch}"));
    must(
        scratch,
        &[
            "clone",
            "-q",
            "--single-branch",
            "--branch",
            "base",
            url,
            clone.to_str().unwrap(),
        ],
    );
    must(&clone, &["fetch", "-q", "origin", branch]);
    must(&clone, &["fsck", "--strict"]);
    object_set(&clone)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thin_fetch_never_deltas_against_objects_the_client_lacks() {
    let scan = tempfile::tempdir().unwrap();
    let canon_root = tempfile::tempdir().unwrap();
    let did = RepoDid::new("did:plc:limpet").unwrap();
    let name = RepoRkey::new("whelk").unwrap();
    let s = stand_up(
        scan.path(),
        canon_root.path(),
        &did,
        &name,
        ObjectFormat::SHA1,
        Mode::Serve,
    )
    .await;

    let scratch = tempfile::tempdir().unwrap();
    seed_cross_branch_deltas(&scratch.path().join("work"), &s.knot_bare, &s.canon_bare);

    ["side", "other"].into_iter().for_each(|branch| {
        assert_eq!(
            fetch_branch(scratch.path(), &s.canon_url, "canon", branch),
            fetch_branch(scratch.path(), &s.knot_url, "knot", branch),
            "thin fetch of {branch} onto a base-only clone transfers canonical object set"
        );
    });
}

fn push_sequence(remote: &str, scratch: &Path, label: &str) -> Vec<(String, bool)> {
    let clone = scratch.join(format!("push-{label}"));
    must(scratch, &["clone", "-q", remote, clone.to_str().unwrap()]);
    let commit = |file: &str, body: &str, msg: &str| {
        std::fs::write(clone.join(file), body).unwrap();
        must(&clone, &["add", "-A"]);
        must(&clone, &["commit", "-q", "-m", msg]);
    };
    let push = |args: &[&str]| git(&clone, &[&["push", "-q", "origin"], args].concat()).0;

    commit("ff.txt", "fast forward\n", "fast forward");
    let ff = push(&["main"]);
    must(&clone, &["checkout", "-q", "-b", "feature"]);
    commit("feature.txt", "new branch\n", "feature");
    let new_branch = push(&["feature"]);
    let delete = push(&["--delete", "feature"]);
    must(&clone, &["checkout", "-q", "main"]);
    must(&clone, &["reset", "-q", "--hard", "HEAD~1"]);
    commit("diverge.txt", "non fast forward\n", "diverge");
    let non_ff = push(&["main"]);

    vec![
        ("fast-forward".to_string(), ff),
        ("new-branch".to_string(), new_branch),
        ("delete-branch".to_string(), delete),
        ("non-fast-forward".to_string(), non_ff),
    ]
}

async fn push_diff(format: ObjectFormat, did: &str, name: &str) {
    let scan = tempfile::tempdir().unwrap();
    let canon_root = tempfile::tempdir().unwrap();
    let did = RepoDid::new(did).unwrap();
    let name = RepoRkey::new(name).unwrap();
    let s = stand_up(
        scan.path(),
        canon_root.path(),
        &did,
        &name,
        format,
        Mode::Receive,
    )
    .await;

    let scratch = tempfile::tempdir().unwrap();
    seed_branches_and_tag(
        &scratch.path().join("work"),
        [&s.knot_bare, &s.canon_bare],
        format,
    );

    let advertised = |bare: &Path| {
        must(
            bare,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads/",
                "refs/tags/",
            ],
        )
    };
    assert_eq!(
        push_sequence(&s.canon_url, scratch.path(), "canon"),
        push_sequence(&s.knot_url, scratch.path(), "knot"),
        "{format:?} knot accepts and rejects the same pushes as canonical git"
    );
    assert_eq!(
        advertised(&s.canon_bare),
        advertised(&s.knot_bare),
        "{format:?} both servers hold the same refs after an identical push sequence"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn differential_push_verdicts_match_canonical() {
    push_diff(ObjectFormat::SHA1, "did:plc:squid", "barnacle").await;
    push_diff(ObjectFormat::SHA256, "did:plc:cuttle", "scallop").await;
}

fn enable_filter(bare: &Path) {
    must(bare, &["config", "uploadpack.allowFilter", "true"]);
    must(bare, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
}

fn filtered_clone(scratch: &Path, url: &str, label: &str, filter: &str) -> BTreeSet<String> {
    let clone = scratch.join(format!("clone-{label}"));
    must(
        scratch,
        &[
            "clone",
            "-q",
            "--no-checkout",
            &format!("--filter={filter}"),
            url,
            clone.to_str().unwrap(),
        ],
    );
    object_set(&clone)
}

fn fetched_set(
    scratch: &Path,
    url: &str,
    label: &str,
    oid: &str,
    filter: &str,
    format: ObjectFormat,
) -> BTreeSet<String> {
    let dest = scratch.join(format!("fetch-{label}"));
    let fmt = format!("--object-format={}", format.capability());
    must(scratch, &["init", &fmt, "-q", dest.to_str().unwrap()]);
    must(
        &dest,
        &["fetch", "-q", &format!("--filter={filter}"), url, oid],
    );
    object_set(&dest)
}

fn blobless_checkout(scratch: &Path, url: &str, label: &str) -> BTreeSet<String> {
    let clone = scratch.join(format!("clone-{label}"));
    must(
        scratch,
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            url,
            clone.to_str().unwrap(),
        ],
    );
    must(&clone, &["fsck", "--strict"]);
    object_set(&clone)
}

async fn partial_clone(format: ObjectFormat, did: &str, name: &str) {
    let scan = tempfile::tempdir().unwrap();
    let canon_root = tempfile::tempdir().unwrap();
    let did = RepoDid::new(did).unwrap();
    let name = RepoRkey::new(name).unwrap();
    let s = stand_up(
        scan.path(),
        canon_root.path(),
        &did,
        &name,
        format,
        Mode::Serve,
    )
    .await;
    enable_filter(&s.canon_bare);

    let scratch = tempfile::tempdir().unwrap();
    let work = scratch.path().join("work");
    seed_nested_bares(&work, &s.knot_bare, &s.canon_bare, format);

    [
        "tree:0",
        "tree:1",
        "tree:2",
        "tree:3",
        "tree:4",
        "tree:5",
        "blob:none",
        "blob:limit=3",
        "blob:limit=4",
        "blob:limit=8",
        "blob:limit=20",
    ]
    .into_iter()
    .for_each(|filter| {
        assert_eq!(
            filtered_clone(
                scratch.path(),
                &s.canon_url,
                &format!("canon-{filter}"),
                filter
            ),
            filtered_clone(
                scratch.path(),
                &s.knot_url,
                &format!("knot-{filter}"),
                filter
            ),
            "{format:?} a --filter={filter} clone transfers canonical object set"
        );
    });

    let deep = must(&work, &["rev-parse", "HEAD:deep"]);
    let far = must(&work, &["rev-parse", "HEAD:deep/mid/bottom/far.txt"]);
    [
        (&deep, "tree:0"),
        (&deep, "tree:1"),
        (&deep, "tree:2"),
        (&deep, "tree:3"),
        (&deep, "blob:none"),
        (&deep, "blob:limit=10"),
        (&far, "blob:none"),
        (&far, "blob:limit=10"),
    ]
    .into_iter()
    .enumerate()
    .for_each(|(case, (oid, filter))| {
        assert_eq!(
            fetched_set(
                scratch.path(),
                &s.canon_url,
                &format!("canon-{case}"),
                oid,
                filter,
                format
            ),
            fetched_set(
                scratch.path(),
                &s.knot_url,
                &format!("knot-{case}"),
                oid,
                filter,
                format
            ),
            "{format:?} explicit want {oid} --filter={filter} transfers canonical object set"
        );
    });

    assert_eq!(
        blobless_checkout(scratch.path(), &s.canon_url, "canon"),
        blobless_checkout(scratch.path(), &s.knot_url, "knot"),
        "{format:?} a blobless clone faults its checkout blobs in like canonical git"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn differential_partial_clone_matches_canonical() {
    partial_clone(ObjectFormat::SHA1, "did:plc:anemone", "barnacle").await;
    partial_clone(ObjectFormat::SHA256, "did:plc:cuttle", "uni").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_sha256_repo_clones_to_sha256_over_v2_and_v0() {
    let scan = tempfile::tempdir().unwrap();
    let did = RepoDid::new("did:plc:limpet").unwrap();
    let layout = Layout::new(scan.path()).with_object_format(ObjectFormat::SHA256);
    layout.create(&did).unwrap();
    let knot = spawn(
        knot_pack::router(
            layout,
            serve_dids(),
            std::sync::Arc::new(knot_runtime::SystemClock),
        ),
        "[::1]:0",
    )
    .await;
    let url = format!("http://{knot}/{}", did.as_str());

    let scratch = tempfile::tempdir().unwrap();
    [("v2", "2"), ("v0", "0")].into_iter().for_each(|(label, version)| {
        let dst = scratch.path().join(format!("clone-{label}"));
        must(scratch.path(), &["-c", &format!("protocol.version={version}"), "clone", "-q", &url, dst.to_str().unwrap()]);
        assert_eq!(
            must(&dst, &["rev-parse", "--show-object-format"]),
            "sha256",
            "{label}: an empty knot sha256 repo clones to sha256 from its advertised capabilities"
        );
    });
}
