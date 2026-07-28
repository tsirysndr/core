mod common;

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use bytes::Bytes;
use common::Edge;
use http::Method;
use knot_edge::RequiresFullHandshake;
use knot_git::Layout;
use knot_pack::{CacheConfig, RepoLookup, RepoResolver, RepoTarget};
use knot_types::{ObjectFormat, RepoDid};

const PINNED_DATE: &str = "2026-06-20T12:00:00+00:00";

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = knot_fixtures::command(cwd)
        .args(args)
        .env("GIT_AUTHOR_DATE", PINNED_DATE)
        .env("GIT_COMMITTER_DATE", PINNED_DATE)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("git stdout is utf-8")
        .trim()
        .to_string()
}

fn object_set(dir: &Path) -> BTreeSet<String> {
    git(
        dir,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    )
    .lines()
    .map(|line| line.trim().to_string())
    .filter(|line| !line.is_empty())
    .collect()
}

fn seed_bare(work: &Path, bare: &Path, format: ObjectFormat) -> String {
    let fmt = format!("--object-format={}", format.capability());
    std::fs::create_dir_all(work).unwrap();
    git(work, &["init", &fmt, "-q", "-b", "main"]);
    std::fs::write(work.join("README.md"), "h3 over the simulated quic edge\n").unwrap();
    git(work, &["add", "-A"]);
    git(work, &["commit", "-q", "-m", "c1"]);
    std::fs::write(work.join("extra.txt"), "a second object\n").unwrap();
    git(work, &["add", "-A"]);
    git(work, &["commit", "-q", "-m", "c2"]);
    git(work, &["push", "-q", bare.to_str().unwrap(), "main"]);
    git(bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(work, &["rev-parse", "HEAD"])
}

fn init(dir: &Path, format: ObjectFormat) {
    std::fs::create_dir_all(dir).unwrap();
    let fmt = format!("--object-format={}", format.capability());
    git(dir, &["init", &fmt, "-q", dir.to_str().unwrap()]);
}

fn index_pack(repo: &Path, pack: &[u8]) {
    let mut child = knot_fixtures::command(repo)
        .args(["index-pack", "--stdin", "--fix-thin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn index-pack");
    child.stdin.take().unwrap().write_all(pack).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "index-pack failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fetch_body(tip: &str) -> Bytes {
    let mut body = Vec::new();
    body.extend_from_slice(&common::pkt(b"command=fetch\n"));
    body.extend_from_slice(&common::pkt(b"agent=knot/0\n"));
    body.extend_from_slice(b"0001");
    body.extend_from_slice(&common::pkt(b"no-progress\n"));
    body.extend_from_slice(&common::pkt(b"ofs-delta\n"));
    body.extend_from_slice(&common::pkt(format!("want {tip}\n").as_bytes()));
    body.extend_from_slice(&common::pkt(b"done\n"));
    body.extend_from_slice(b"0000");
    Bytes::from(body)
}

fn extract_pack(response: &[u8]) -> Vec<u8> {
    let mut channel = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= response.len() {
        let len = std::str::from_utf8(&response[pos..pos + 4])
            .ok()
            .and_then(|hex| usize::from_str_radix(hex, 16).ok())
            .unwrap_or(0);
        pos += 4;
        if len < 4 {
            continue;
        }
        let end = (pos + len - 4).min(response.len());
        let payload = &response[pos..end];
        pos = end;
        if payload.first() == Some(&1) {
            channel.extend_from_slice(&payload[1..]);
        }
    }
    match channel.windows(4).position(|window| window == b"PACK") {
        Some(start) => channel.split_off(start),
        None => channel,
    }
}

fn serve_dids() -> Arc<dyn RepoResolver> {
    Arc::new(|target: &RepoTarget| match target {
        RepoTarget::Did(did) => RepoLookup::Hosted(did.clone()),
        RepoTarget::OwnerPath(_, _) => RepoLookup::Unhosted,
    })
}

async fn clone_over_h3(edge: &Edge, did: &str, tip: &str) -> Vec<u8> {
    let connection = edge
        .client
        .connect(edge.addr, "localhost")
        .unwrap()
        .await
        .unwrap();
    let quic = connection.clone();
    let (mut driver, mut sender) = h3::client::new(h3_quinn::Connection::new(connection))
        .await
        .unwrap();
    let drive = tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
    });

    let warmup = http::Request::get(format!(
        "https://localhost/{did}/info/refs?service=git-upload-pack"
    ))
    .header("git-protocol", "version=2")
    .body(())
    .unwrap();
    let mut warm = sender.send_request(warmup).await.unwrap();
    common::finish_request(&mut warm).await;
    assert!(
        warm.recv_response().await.unwrap().status().is_success(),
        "h3 info/refs advertisement must serve over QUIC"
    );
    common::drain(&mut warm).await;

    let request = http::Request::builder()
        .method(Method::POST)
        .uri(format!("https://localhost/{did}/git-upload-pack"))
        .header("content-type", "application/x-git-upload-pack-request")
        .header("git-protocol", "version=2")
        .body(())
        .unwrap();
    let mut stream = sender.send_request(request).await.unwrap();
    stream.send_data(fetch_body(tip)).await.unwrap();
    common::finish_request(&mut stream).await;
    let response = stream.recv_response().await.unwrap();
    assert!(
        response.status().is_success(),
        "h3 upload-pack returned {}",
        response.status()
    );
    let out = common::drain(&mut stream).await;
    quic.close(0u32.into(), b"done");
    drive.abort();
    out
}

async fn cloned_set(
    layout: &Layout,
    did: &str,
    tip: &str,
    format: ObjectFormat,
) -> BTreeSet<String> {
    let certdir = tempfile::tempdir().unwrap();
    let clonedir = tempfile::tempdir().unwrap();
    let edge = common::serve_edge(certdir.path(), || {
        let (write_routes, advertisement) = knot_pack::edge_routes(
            layout.clone(),
            serve_dids(),
            None,
            None,
            knot_resource::PackSlots::new(4),
            CacheConfig::default(),
            Arc::new(knot_messages::Catalog::defaults()),
            knot_pack::default_hostname().clone(),
            Arc::new(knot_runtime::SystemClock),
        );
        (RequiresFullHandshake::new(write_routes), advertisement)
    })
    .await;
    let pack = extract_pack(&clone_over_h3(&edge, did, tip).await);
    edge.shutdown.cancel();
    edge.task.abort();
    init(clonedir.path(), format);
    index_pack(clonedir.path(), &pack);
    object_set(clonedir.path())
}

async fn the_live_h3_transport_is_logically_reproducible(format: ObjectFormat, did_str: &str) {
    let scan = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();

    let did = RepoDid::new(did_str).unwrap();
    let layout = Layout::new(scan.path()).with_object_format(format);
    layout.create(&did).unwrap();
    let bare = layout.repo_path(&did).unwrap();
    let tip = seed_bare(&scratch.path().join("work"), &bare, format);
    let truth = object_set(&bare);

    let first = cloned_set(&layout, did_str, &tip, format).await;
    let second = cloned_set(&layout, did_str, &tip, format).await;

    assert_eq!(
        first, second,
        "{format:?}: two independent live h3 connections must deliver the same object set"
    );
    assert_eq!(
        first, truth,
        "{format:?}: the h3 clone's object set must equal the bare's full object set"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_new_h3_transport_replays_to_the_same_object_set_off_the_recorded_trace() {
    the_live_h3_transport_is_logically_reproducible(ObjectFormat::SHA1, "did:plc:squid").await;
    the_live_h3_transport_is_logically_reproducible(ObjectFormat::SHA256, "did:plc:cuttle").await;
}
