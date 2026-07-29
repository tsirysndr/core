use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::header;
use knot_git::{Layout, RefUpdate};
use knot_pack::{RepoLookup, RepoResolver, RepoTarget};
use knot_types::{OwnerDid, RefName, RepoDid, RepoRkey};

mod common;
use common::{commit, contains, git, must, pkt, serve_dids, spawn, unsideband};

fn seed_repo(work: &Path, bare: &str, file: &str, contents: &str) {
    std::fs::create_dir_all(work).unwrap();
    must(work, &["init", "-q", "-b", "main"]);
    commit(work, file, contents, "initial");
    must(work, &["push", "-q", bare, "main"]);
    must(
        Path::new(bare),
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    );
}

fn url(addr: SocketAddr, did: &RepoDid, _name: &RepoRkey) -> String {
    format!("http://{addr}/{}", did.as_str())
}

fn pack_object_oids(pack: &[u8]) -> std::collections::BTreeSet<String> {
    use std::io::Write;
    use std::process::Stdio;

    let bare = tempfile::tempdir().unwrap();
    let path = bare.path().to_str().unwrap();
    assert!(
        knot_fixtures::command(bare.path())
            .args(["init", "--bare", "-q", path])
            .output()
            .unwrap()
            .status
            .success()
    );
    let mut child = knot_fixtures::command(bare.path())
        .args(["index-pack", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(pack).unwrap();
    let indexed = child.wait_with_output().unwrap();
    assert!(
        indexed.status.success(),
        "index-pack of our pack failed:\n{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let listed = knot_fixtures::command(bare.path())
        .args([
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&listed.stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

fn canonical_pack(work: &Path, revs: &[String]) -> Vec<u8> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = knot_fixtures::command(work)
        .args(["pack-objects", "--revs", "--stdout", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(revs.join("\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "canonical pack-objects failed");
    out.stdout
}

fn v2_fetch_body(wants: &[String], haves: &[String]) -> Vec<u8> {
    let mut body = pkt(b"command=fetch\n");
    body.extend_from_slice(b"0001");
    wants
        .iter()
        .for_each(|want| body.extend(pkt(format!("want {want}\n").as_bytes())));
    haves
        .iter()
        .for_each(|have| body.extend(pkt(format!("have {have}\n").as_bytes())));
    body.extend(pkt(b"done\n"));
    body.extend_from_slice(b"0000");
    body
}

#[tokio::test(flavor = "multi_thread")]
async fn http_routing_resolves_owner_rkey_and_dot_git_and_404s_the_unhosted() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let owner = OwnerDid::new("did:plc:nel").unwrap();
    let plain_did = RepoDid::new("did:plc:squid").unwrap();
    let literal_did = RepoDid::new("did:plc:whelk").unwrap();
    layout.create(&plain_did).unwrap();
    layout.create(&literal_did).unwrap();

    let resolver: Arc<dyn RepoResolver> = {
        let owner = owner.clone();
        let plain_did = plain_did.clone();
        let literal_did = literal_did.clone();
        Arc::new(move |target: &RepoTarget| match target {
            RepoTarget::OwnerPath(o, p)
                if *o == owner && p.rkeys().any(|rkey| rkey.as_str() == "barnacle.git") =>
            {
                RepoLookup::Hosted(literal_did.clone())
            }
            RepoTarget::OwnerPath(o, p)
                if *o == owner && p.rkeys().any(|rkey| rkey.as_str() == "anemone") =>
            {
                RepoLookup::Hosted(plain_did.clone())
            }
            _ => RepoLookup::Unhosted,
        })
    };
    let addr = spawn(
        knot_pack::router(
            layout.clone(),
            resolver,
            std::sync::Arc::new(knot_runtime::SystemClock),
        ),
        "[::1]:0",
    )
    .await;

    let scratch = tempfile::tempdir().unwrap();
    seed_repo(
        &scratch.path().join("work-plain"),
        layout.repo_path(&plain_did).unwrap().to_str().unwrap(),
        "README.md",
        "plain\n",
    );
    seed_repo(
        &scratch.path().join("work-literal"),
        layout.repo_path(&literal_did).unwrap().to_str().unwrap(),
        "README.md",
        "literal\n",
    );

    let clone_ok = |name: &str, label: &str, expect: &str| {
        let dest = scratch.path().join(label);
        let remote = format!("http://{addr}/{}/{name}", owner.as_str());
        let (ok, out) = git(
            scratch.path(),
            &["clone", "-q", &remote, dest.to_str().unwrap()],
        );
        assert!(
            ok,
            "clone of {name} must resolve through the registry:\n{out}"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("README.md")).unwrap(),
            expect
        );
    };
    clone_ok("anemone", "clone-plain", "plain\n");
    clone_ok("anemone.git", "clone-suffixed", "plain\n");
    clone_ok("barnacle.git", "clone-literal", "literal\n");

    let clone_404 = |remote: String, why: &str| {
        let dest = scratch.path().join("clone-404");
        let (ok, _out) = git(
            scratch.path(),
            &["clone", "-q", &remote, dest.to_str().unwrap()],
        );
        assert!(!ok, "{why}");
        let _ = std::fs::remove_dir_all(&dest);
    };
    clone_404(
        format!("http://{addr}/{}/conch", owner.as_str()),
        "rkey with no registry entry must 404, not route to the wrong repo",
    );
    clone_404(
        format!("http://{addr}/{}", plain_did.as_str()),
        "a direct-DID path the resolver doesn't host must 404, even though the repo exists on disk",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_clone_while_the_index_is_warming_is_unavailable_not_404() {
    use tower::ServiceExt;

    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let resolver: Arc<dyn RepoResolver> = Arc::new(|_target: &RepoTarget| RepoLookup::Unavailable);

    let by_name = axum::http::Request::builder()
        .uri("/did:plc:nel/anemone/info/refs?service=git-upload-pack")
        .body(Body::empty())
        .unwrap();
    let response = knot_pack::router(
        layout.clone(),
        Arc::clone(&resolver),
        std::sync::Arc::new(knot_runtime::SystemClock),
    )
    .oneshot(by_name)
    .await
    .unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "warming registry is a retryable 503 on owner/rkey route, never a 404"
    );

    let by_did = axum::http::Request::builder()
        .uri("/did:plc:squid/info/refs?service=git-upload-pack")
        .body(Body::empty())
        .unwrap();
    let response = knot_pack::router(
        layout,
        resolver,
        std::sync::Arc::new(knot_runtime::SystemClock),
    )
    .oneshot(by_did)
    .await
    .unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "warming registry is a retryable 503 on direct-DID route, never a 404"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shallow_clone_over_protocol_v0() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let name = RepoRkey::new("scallop").unwrap();
    layout.create(&did).unwrap();
    let bare = layout.repo_path(&did).unwrap();
    let addr = spawn(
        knot_pack::router(
            layout.clone(),
            serve_dids(),
            std::sync::Arc::new(knot_runtime::SystemClock),
        ),
        "[::1]:0",
    )
    .await;

    let scratch = tempfile::tempdir().unwrap();
    let work = scratch.path().join("work");
    seed_three_commits(&work, bare.to_str().unwrap());
    commit(&work, "d.txt", "c4\n", "c4");
    must(&work, &["push", "-q", bare.to_str().unwrap(), "main"]);
    let remote = url(addr, &did, &name);

    let clone = scratch.path().join("clone");
    let (ok, out) = git(
        scratch.path(),
        &[
            "-c",
            "protocol.version=0",
            "clone",
            "--depth=1",
            "-q",
            &remote,
            clone.to_str().unwrap(),
        ],
    );
    assert!(ok, "v0 shallow clone failed:\n{out}");
    assert!(
        clone.join(".git/shallow").exists(),
        "depth-limited v0 clone must be marked shallow"
    );
    assert_eq!(
        must(&clone, &["rev-list", "--count", "HEAD"]).trim(),
        "1",
        "depth=1 over v0 must yield exactly one commit"
    );

    let (ok, out) = git(
        &clone,
        &[
            "-c",
            "protocol.version=0",
            "fetch",
            "--depth=2",
            "-q",
            "origin",
        ],
    );
    assert!(ok, "v0 deepening fetch failed:\n{out}");
    assert_eq!(
        must(&clone, &["rev-list", "--count", "origin/main"]).trim(),
        "2",
        "deepen to depth=2 over v0 must reveal second commit"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shallow_fetch_of_an_annotated_tag() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let name = RepoRkey::new("whelk").unwrap();
    layout.create(&did).unwrap();
    let bare = layout.repo_path(&did).unwrap();
    let addr = spawn(
        knot_pack::router(
            layout.clone(),
            serve_dids(),
            std::sync::Arc::new(knot_runtime::SystemClock),
        ),
        "[::1]:0",
    )
    .await;

    let scratch = tempfile::tempdir().unwrap();
    let work = scratch.path().join("work");
    seed_repo(&work, bare.to_str().unwrap(), "a.txt", "c1\n");
    commit(&work, "b.txt", "c2\n", "c2");
    must(&work, &["tag", "-a", "release", "-m", "release"]);
    must(
        &work,
        &["push", "-q", bare.to_str().unwrap(), "main", "release"],
    );
    let remote = url(addr, &did, &name);

    ["2", "0"].iter().enumerate().for_each(|(index, version)| {
        let dest = scratch.path().join(format!("tagfetch-{index}"));
        std::fs::create_dir_all(&dest).unwrap();
        must(&dest, &["init", "-q"]);
        let (ok, out) = git(
            &dest,
            &[
                "-c",
                &format!("protocol.version={version}"),
                "fetch",
                "--depth=1",
                "-q",
                &remote,
                "refs/tags/release:refs/tags/release",
            ],
        );
        assert!(ok, "shallow tag fetch over v{version} failed:\n{out}");
        assert_eq!(
            must(&dest, &["cat-file", "-t", "release"]).trim(),
            "tag",
            "annotated tag object itself must be transferred over v{version}"
        );
        assert_eq!(
            must(&dest, &["rev-list", "--count", "release^{commit}"]).trim(),
            "1",
            "depth-1 tag fetch over v{version} must contain exactly the tagged commit"
        );
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn pack_slot_limit_serializes_concurrent_clones_without_breaking_them() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let name = RepoRkey::new("cuttle").unwrap();
    layout.create(&did).unwrap();
    let bare = layout.repo_path(&did).unwrap();

    let scratch = tempfile::tempdir().unwrap();
    let work = scratch.path().join("work");
    seed_repo(&work, bare.to_str().unwrap(), "README.md", "kelp\n");
    let tip = must(&work, &["rev-parse", "HEAD"]).trim().to_string();

    let addr = spawn(
        knot_pack::router_with_pack_slots(
            layout,
            serve_dids(),
            knot_resource::PackSlots::new(1),
            std::sync::Arc::new(knot_runtime::SystemClock),
        ),
        "[::1]:0",
    )
    .await;
    let remote = url(addr, &did, &name);

    let children: Vec<(usize, std::path::PathBuf, std::process::Child)> = (0..6)
        .map(|index| {
            let dest = scratch.path().join(format!("clone-{index}"));
            let child = knot_fixtures::command(scratch.path())
                .args(["clone", "-q", &remote, dest.to_str().unwrap()])
                .spawn()
                .expect("git clone spawns");
            (index, dest, child)
        })
        .collect();

    children.into_iter().for_each(|(index, dest, mut child)| {
        assert!(
            child.wait().unwrap().success(),
            "single pack slot must still let concurrent clone {index} complete"
        );
        assert_eq!(
            must(&dest, &["rev-parse", "HEAD"]).trim(),
            tip,
            "clone served under a one-slot limit must still check out the right tip"
        );
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn http_upload_archive_serves_a_framed_tar_and_guards_refuse_cob_raw_oids_and_traversal() {
    use tower::ServiceExt as _;

    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    layout.create(&did).unwrap();
    let bare = layout.repo_path(&did).unwrap();

    let scratch = tempfile::tempdir().unwrap();
    let work = scratch.path().join("work");
    seed_repo(&work, bare.to_str().unwrap(), "README.md", "archive me\n");

    let mut framed = Vec::new();
    framed.extend(pkt(b"argument --format=tar\n"));
    framed.extend(pkt(b"argument HEAD\n"));
    framed.extend_from_slice(b"0000");
    let response = knot_pack::router(
        layout.clone(),
        serve_dids(),
        std::sync::Arc::new(knot_runtime::SystemClock),
    )
    .oneshot(
        axum::http::Request::builder()
            .method("POST")
            .uri(format!("/{}/git-upload-archive", did.as_str()))
            .header(
                header::CONTENT_TYPE,
                "application/x-git-upload-archive-request",
            )
            .body(Body::from(framed))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/x-git-upload-archive-result"),
    );
    let body = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    assert!(
        body.starts_with(b"0008ACK\n"),
        "archive response opens with the ACK pkt-line"
    );
    assert!(
        contains(&body, b"README.md"),
        "framed archive contains README.md entry"
    );

    let repo = layout.open(&did).unwrap();
    let head = repo.head().expect("seeded head").target;
    let tree = repo.find_commit(head).unwrap().tree;
    let archive = |args: &[&str]| {
        let mut request = Vec::new();
        args.iter()
            .for_each(|arg| request.extend(pkt(arg.as_bytes())));
        request.extend_from_slice(b"0000");
        knot_pack::upload_archive(&repo, &request, knot_git::ArchiveLimit::default()).unwrap()
    };

    let raw_arg = format!("argument {}\n", tree.to_hex());
    let raw_oid = archive(&["argument --format=tar\n", raw_arg.as_str()]);
    assert!(
        String::from_utf8_lossy(&raw_oid).contains("NACK"),
        "raw tree oid must be declined like uploadArchive.allowUnreachable=false"
    );

    let traversal = archive(&[
        "argument --format=tar\n",
        "argument --prefix=../evil/\n",
        "argument HEAD\n",
    ]);
    assert!(
        String::from_utf8_lossy(&traversal).contains("NACK"),
        "traversal prefix must be declined"
    );

    repo.update_ref(&RefUpdate::Create {
        name: RefName::new("refs/cobs/sh.tangled.repo.collaborator/secret").unwrap(),
        new: head,
    })
    .unwrap();
    repo.update_ref(&RefUpdate::Delete {
        name: RefName::new("refs/heads/main").unwrap(),
        old: head,
    })
    .unwrap();
    let cob = archive(&[
        "argument --format=tar\n",
        "argument refs/cobs/sh.tangled.repo.collaborator/secret\n",
    ]);
    assert!(
        String::from_utf8_lossy(&cob).contains("NACK"),
        "archiving cob-only tree must be refused"
    );
    assert!(
        !contains(&cob, b"README.md"),
        "refused archive mustn't leak the hidden tree's contents"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_upload_archive_honors_the_configured_archive_limit() {
    use tower::ServiceExt as _;

    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:limpet").unwrap();
    layout.create(&did).unwrap();
    let bare = layout.repo_path(&did).unwrap();

    let scratch = tempfile::tempdir().unwrap();
    let work = scratch.path().join("work");
    seed_repo(&work, bare.to_str().unwrap(), "README.md", "archive me\n");

    let (write_routes, _advertisement) = knot_pack::edge_routes(knot_pack::EdgeConfig {
        pack_slots: knot_resource::PackSlots::new(1),
        archive_limit: knot_git::ArchiveLimit::new(512),
        ..knot_pack::EdgeConfig::serving(
            layout.clone(),
            serve_dids(),
            Arc::new(knot_runtime::SystemClock),
        )
    });

    let mut framed = Vec::new();
    framed.extend(pkt(b"argument --format=tar\n"));
    framed.extend(pkt(b"argument HEAD\n"));
    framed.extend_from_slice(b"0000");
    let response = write_routes
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/{}/git-upload-archive", did.as_str()))
                .header(
                    header::CONTENT_TYPE,
                    "application/x-git-upload-archive-request",
                )
                .body(Body::from(framed))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("NACK") && text.contains("archive exceeds the 512 byte limit"),
        "an archive past the state's limit must be declined, got {text:?}"
    );
    assert!(
        !contains(&body, b"README.md"),
        "the declined archive mustn't leak the tree it refused to serve"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn push_over_http_is_refused() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let name = RepoRkey::new("conch").unwrap();
    layout.create(&did).unwrap();
    let addr = spawn(
        knot_pack::router(
            layout.clone(),
            serve_dids(),
            std::sync::Arc::new(knot_runtime::SystemClock),
        ),
        "[::1]:0",
    )
    .await;

    let scratch = tempfile::tempdir().unwrap();
    let work = scratch.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    must(&work, &["init", "-q", "-b", "main"]);
    commit(&work, "a.txt", "one\n", "one");
    let (ok, out) = git(&work, &["push", &url(addr, &did, &name), "main"]);
    assert!(!ok, "push over HTTP must be refused, got success:\n{out}");
}

fn seed_three_commits(work: &Path, bare: &str) {
    seed_repo(work, bare, "a.txt", "c1\n");
    commit(work, "b.txt", "c2\n", "c2");
    must(work, &["push", "-q", bare, "main"]);
    commit(work, "c.txt", "c3\n", "c3");
    must(work, &["push", "-q", bare, "main"]);
}

fn commit_dated(work: &Path, file: &str, contents: &str, message: &str, iso_date: &str) {
    std::fs::write(work.join(file), contents).unwrap();
    must(work, &["add", "-A"]);
    let out = knot_fixtures::command_at(work, iso_date)
        .args(["commit", "-q", "-m", message])
        .output()
        .unwrap();
    assert!(out.status.success(), "dated commit failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn shallow_clone_depth_exclude_since() {
    let did = RepoDid::new("did:plc:squid").unwrap();
    let s = common::stand(&did).await;
    let bare = &s.bare;
    let scratch = s.scratch.path();

    let work = scratch.join("work");
    std::fs::create_dir_all(&work).unwrap();
    must(&work, &["init", "-q", "-b", "main"]);
    commit_dated(&work, "a.txt", "c1\n", "c1", "2020-01-01T00:00:00 +0000");
    must(&work, &["tag", "base"]);
    commit_dated(&work, "b.txt", "c2\n", "c2", "2021-01-01T00:00:00 +0000");
    commit_dated(&work, "c.txt", "c3\n", "c3", "2022-01-01T00:00:00 +0000");
    commit_dated(&work, "d.txt", "c4\n", "c4", "2024-01-01T00:00:00 +0000");
    must(
        &work,
        &["push", "-q", bare.to_str().unwrap(), "main", "base"],
    );
    must(bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let remote = format!("http://{}/{}", s.addr, did.as_str());

    let depth = scratch.join("clone-depth");
    must(
        scratch,
        &["clone", "--depth=1", "-q", &remote, depth.to_str().unwrap()],
    );
    assert!(
        depth.join(".git/shallow").exists(),
        "depth-limited clone must be marked shallow"
    );
    assert_eq!(
        must(&depth, &["rev-list", "--count", "HEAD"]).trim(),
        "1",
        "depth=1 must yield exactly one commit"
    );
    let (ok, out) = git(&depth, &["fetch", "--depth=2", "-q", "origin"]);
    assert!(ok, "deepening fetch failed:\n{out}");
    assert_eq!(
        must(&depth, &["rev-list", "--count", "origin/main"]).trim(),
        "2",
        "deepen to depth=2 must reveal second commit"
    );
    let (ok, out) = git(&depth, &["fetch", "--deepen=1", "-q", "origin"]);
    assert!(ok, "relative deepen fetch failed:\n{out}");
    assert_eq!(
        must(&depth, &["rev-list", "--count", "origin/main"]).trim(),
        "3",
        "--deepen=1 from depth 2 must reveal third commit"
    );
    let (ok, out) = git(&depth, &["fetch", "--unshallow", "-q", "origin"]);
    assert!(ok, "unshallow fetch failed:\n{out}");
    assert!(
        !depth.join(".git/shallow").exists(),
        "unshallow fetch must drop the shallow marker"
    );
    assert_eq!(
        must(&depth, &["rev-list", "--count", "origin/main"]).trim(),
        "4",
        "unshallow must restore full history"
    );

    let exclude = scratch.join("clone-exclude");
    let (ok, out) = git(
        scratch,
        &[
            "clone",
            "--shallow-exclude=base",
            "-q",
            &remote,
            exclude.to_str().unwrap(),
        ],
    );
    assert!(ok, "shallow-exclude clone failed:\n{out}");
    assert_eq!(
        must(&exclude, &["rev-list", "--count", "HEAD"]).trim(),
        "3",
        "shallow-exclude=base must drop excluded commit and its ancestors"
    );

    let since = scratch.join("clone-since");
    let (ok, out) = git(
        scratch,
        &[
            "clone",
            "--shallow-since=2023-01-01",
            "-q",
            &remote,
            since.to_str().unwrap(),
        ],
    );
    assert!(ok, "shallow-since clone failed:\n{out}");
    assert_eq!(
        must(&since, &["rev-list", "--count", "HEAD"]).trim(),
        "1",
        "shallow-since must keep only commits at or after cutoff"
    );
}

#[test]
fn upload_pack_object_set_matches_canonical_git() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    layout.create(&did).unwrap();
    let bare = layout.repo_path(&did).unwrap();

    let scratch = tempfile::tempdir().unwrap();
    let work = scratch.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    must(&work, &["init", "-q", "-b", "main"]);
    commit(&work, "a.txt", "one\n", "c1");
    let c1 = must(&work, &["rev-parse", "HEAD"]).trim().to_string();
    commit(&work, "a.txt", "two\n", "c2");
    let c2 = must(&work, &["rev-parse", "HEAD"]).trim().to_string();
    must(&work, &["checkout", "-q", "-b", "dev", &c1]);
    commit(&work, "b.txt", "three\n", "c3");
    must(&work, &["checkout", "-q", "main"]);
    must(&work, &["tag", "-a", "v1", "-m", "release", &c2]);
    must(
        &work,
        &["push", "-q", bare.to_str().unwrap(), "main", "dev", "v1"],
    );

    let repo = layout.open(&did).unwrap();
    let tips: Vec<String> = repo
        .advertised_refs()
        .unwrap()
        .iter()
        .map(|record| record.target.to_hex().to_string())
        .collect();

    let clone = unsideband(&knot_pack::upload_pack(&repo, &v2_fetch_body(&tips, &[])).unwrap());
    assert_eq!(
        pack_object_oids(&clone),
        pack_object_oids(&canonical_pack(&work, &tips)),
        "full clone must transfer exactly the object set canonical git packs"
    );

    let incremental = unsideband(
        &knot_pack::upload_pack(
            &repo,
            &v2_fetch_body(std::slice::from_ref(&c2), std::slice::from_ref(&c1)),
        )
        .unwrap(),
    );
    assert_eq!(
        pack_object_oids(&incremental),
        pack_object_oids(&canonical_pack(&work, &[c2, format!("^{c1}")])),
        "incremental fetch must transfer only the objects missing from the client"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn http_boundary_encoding_and_streaming() {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use http_body_util::BodyExt;
    use std::io::Write;
    use tower::ServiceExt;

    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    layout.create(&did).unwrap();
    let bare = layout.repo_path(&did).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let work = scratch.path().join("work");
    seed_repo(&work, bare.to_str().unwrap(), "README.md", "boundary\n");
    let tip = must(&work, &["rev-parse", "HEAD"]).trim().to_string();

    let router = knot_pack::router(
        layout,
        serve_dids(),
        std::sync::Arc::new(knot_runtime::SystemClock),
    );
    let upload_uri = format!("/{}/git-upload-pack", did.as_str());

    let oversized = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(upload_uri.clone())
                .body(Body::from(vec![0u8; 17 * 1024 * 1024]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        oversized.status(),
        axum::http::StatusCode::PAYLOAD_TOO_LARGE,
        "oversized request body must be refused at the HTTP boundary before buffering"
    );

    let unsupported = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(upload_uri.clone())
                .header("content-encoding", "br")
                .body(Body::from(vec![0u8; 16]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        unsupported.status(),
        axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "a body the knot cannot decode is rejected before parsing, never mis-read as identity"
    );

    let mut plain = pkt(b"command=ls-refs\n");
    plain.extend_from_slice(b"0001");
    plain.extend(pkt(b"ref-prefix refs/heads/\n"));
    plain.extend_from_slice(b"0000");
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&plain).unwrap();
    let gzipped = encoder.finish().unwrap();
    let decoded = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(upload_uri.clone())
                .header("content-encoding", "gzip")
                .body(Body::from(gzipped))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(decoded.status(), axum::http::StatusCode::OK);
    let body = decoded.into_body().collect().await.unwrap().to_bytes();
    assert!(
        String::from_utf8_lossy(&body).contains("refs/heads/main"),
        "gzip-encoded ls-refs request must be transparently decoded and answered"
    );

    let mut fetch = pkt(b"command=fetch\n");
    fetch.extend_from_slice(b"0001");
    fetch.extend(pkt(format!("want {tip}\n").as_bytes()));
    fetch.extend(pkt(b"done\n"));
    fetch.extend_from_slice(b"0000");
    let streamed = router
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(upload_uri)
                .body(Body::from(fetch))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(streamed.status(), axum::http::StatusCode::OK);
    assert!(
        streamed.headers().get(header::CONTENT_LENGTH).is_none(),
        "streamed pack response mustn't be buffered into a length-delimited body"
    );
    let collected = streamed.into_body().collect().await.unwrap().to_bytes();
    assert!(
        contains(&collected, b"PACK"),
        "streamed response must contain a real PACK"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_knot_meta_repo_is_never_served_over_http() {
    use tower::ServiceExt;

    let scan = tempfile::tempdir().unwrap();
    let knot = knot_types::KnotId::new("did:web:oyster.cafe").unwrap();
    let layout = Layout::new(scan.path()).reserving_meta(&knot).unwrap();
    layout.bootstrap_meta(&knot).unwrap();
    assert!(
        layout.meta_path(&knot).unwrap().exists(),
        "meta-repo must exist on disk so this tests the guard, not mere absence"
    );

    let visible = RepoDid::new("did:plc:squid").unwrap();
    layout.create(&visible).unwrap();

    let router = knot_pack::router(
        layout,
        serve_dids(),
        std::sync::Arc::new(knot_runtime::SystemClock),
    );

    let status = |method: &'static str, uri: &'static str| {
        let router = router.clone();
        async move {
            router
                .oneshot(
                    axum::http::Request::builder()
                        .method(method)
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
        }
    };

    let not_found = axum::http::StatusCode::NOT_FOUND;
    assert_eq!(
        status(
            "GET",
            "/did:web:oyster.cafe/info/refs?service=git-upload-pack"
        )
        .await,
        not_found,
        "knot DID is refused on GET info/refs"
    );
    assert_eq!(
        status("POST", "/did:web:oyster.cafe/git-upload-pack").await,
        not_found,
        "knot DID is refused on POST upload-pack"
    );
    assert_eq!(
        status(
            "GET",
            "/did:web:oyster.cafe/anemone/info/refs?service=git-upload-pack"
        )
        .await,
        not_found,
        "knot DID is refused on named info/refs route"
    );
    assert_eq!(
        status("POST", "/did:web:oyster.cafe/anemone/git-upload-pack").await,
        not_found,
        "knot DID is refused on named upload-pack route"
    );
    assert_eq!(
        status("POST", "/did:web:oyster.cafe/git-upload-archive").await,
        not_found,
        "knot DID is refused on upload-archive route"
    );
    assert_eq!(
        status("POST", "/did:web:oyster.cafe/anemone/git-upload-archive").await,
        not_found,
        "knot DID is refused on named upload-archive route"
    );
    assert_eq!(
        status(
            "GET",
            "/did:web:OYSTER.cafe/info/refs?service=git-upload-pack"
        )
        .await,
        not_found,
        "case-variant of the knot DID canonicalizes to the same reserved repo"
    );

    assert_eq!(
        status("GET", "/did:plc:squid/info/refs?service=git-upload-pack").await,
        axum::http::StatusCode::OK,
        "ordinary repo path still serves its advertisement"
    );
}
