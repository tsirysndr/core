#![allow(dead_code, unused_imports)]

use std::collections::BTreeSet;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use axum::Router;
use knot_git::{Layout, Repo};
use knot_pack::{RepoLookup, RepoResolver, RepoTarget};
use knot_types::{ObjectFormat, RepoDid};

pub use knot_fixtures::{commit, contains, must, run as git};

pub fn pkt(payload: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
    out.extend_from_slice(payload);
    out
}

pub fn pack_objects(cwd: &Path, oids: &[String]) -> Vec<u8> {
    let mut child = knot_fixtures::command(cwd)
        .args(["pack-objects", "--stdout", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pack-objects");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(oids.join("\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "pack-objects failed");
    out.stdout
}

pub fn pack_objects_tuned(cwd: &Path, oids: &[String], ofs: bool) -> Vec<u8> {
    let args: &[&str] = if ofs {
        &[
            "pack-objects",
            "--stdout",
            "-q",
            "--delta-base-offset",
            "--depth=50",
            "--window=250",
        ]
    } else {
        &[
            "-c",
            "pack.useDeltaBaseOffset=false",
            "pack-objects",
            "--stdout",
            "-q",
            "--depth=50",
            "--window=250",
        ]
    };
    let mut child = knot_fixtures::command(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pack-objects");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(oids.join("\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "pack-objects failed");
    out.stdout
}

pub fn index_into_bare(extra: &[&str], pack: &[u8]) -> bool {
    let bare = tempfile::tempdir().unwrap();
    knot_fixtures::must(
        bare.path(),
        &["init", "--bare", "-q", bare.path().to_str().unwrap()],
    );
    let args: Vec<&str> = std::iter::once("index-pack")
        .chain(extra.iter().copied())
        .chain(std::iter::once("--stdin"))
        .collect();
    knot_fixtures::feed(bare.path(), &args, pack).0
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn base128(value: u64) -> Vec<u8> {
    let low = (value & 0x7f) as u8;
    let rest = value >> 7;
    if rest == 0 {
        vec![low]
    } else {
        std::iter::once(low | 0x80).chain(base128(rest)).collect()
    }
}

fn obj_header(obj_type: u8, size: usize) -> Vec<u8> {
    fn tail(size: usize) -> Vec<u8> {
        if size == 0 {
            Vec::new()
        } else {
            let byte = (size & 0x7f) as u8;
            let rest = size >> 7;
            let cont = if rest > 0 { 0x80 } else { 0 };
            std::iter::once(byte | cont).chain(tail(rest)).collect()
        }
    }
    let rest = size >> 4;
    let cont = if rest > 0 { 0x80 } else { 0 };
    std::iter::once((obj_type << 4) | (size & 0x0f) as u8 | cont)
        .chain(tail(rest))
        .collect()
}

fn ofs_distance(distance: u64) -> Vec<u8> {
    fn prefix(value: u64) -> Vec<u8> {
        if value == 0 {
            Vec::new()
        } else {
            let reduced = value - 1;
            prefix(reduced >> 7)
                .into_iter()
                .chain(std::iter::once(0x80 | (reduced & 0x7f) as u8))
                .collect()
        }
    }
    prefix(distance >> 7)
        .into_iter()
        .chain(std::iter::once((distance & 0x7f) as u8))
        .collect()
}

pub fn delta_bomb_pack(declared_result_bytes: u64) -> Vec<u8> {
    let base = b"hi";
    let mut entry0 = obj_header(3, base.len());
    entry0.extend(zlib(base));

    let delta_stream: Vec<u8> = base128(base.len() as u64)
        .into_iter()
        .chain(base128(declared_result_bytes))
        .chain([0x90, 0x02])
        .collect();
    let mut entry1 = obj_header(6, delta_stream.len());
    entry1.extend(ofs_distance(entry0.len() as u64));
    entry1.extend(zlib(&delta_stream));

    let mut pack = b"PACK".to_vec();
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&entry0);
    pack.extend_from_slice(&entry1);

    let mut hasher = gix_hash::hasher(gix_hash::Kind::Sha1);
    hasher.update(&pack);
    let checksum = hasher.try_finalize().unwrap();
    pack.extend_from_slice(checksum.as_bytes());
    pack
}

pub fn receive_request(refname: &str, old: &str, new: &str, pack: &[u8]) -> Vec<u8> {
    let mut first = format!("{old} {new} {refname}").into_bytes();
    first.push(0);
    first.extend_from_slice(b"report-status\n");
    let mut req = pkt(&first);
    req.extend_from_slice(b"0000");
    req.extend_from_slice(pack);
    req
}

pub fn unsideband(resp: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut in_pack = false;
    while pos + 4 <= resp.len() {
        let len = std::str::from_utf8(&resp[pos..pos + 4])
            .ok()
            .and_then(|hex| usize::from_str_radix(hex, 16).ok())
            .unwrap_or(0);
        pos += 4;
        if len < 4 {
            continue;
        }
        let end = (pos + len - 4).min(resp.len());
        let payload = &resp[pos..end];
        pos = end;
        if payload == b"packfile\n" {
            in_pack = true;
        } else if in_pack && payload.first() == Some(&1) {
            out.extend_from_slice(&payload[1..]);
        }
    }
    out
}

pub fn object_set(dir: &Path) -> BTreeSet<String> {
    must(
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

pub fn incompressible(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state & 0xff) as u8
        })
        .collect()
}

pub fn serve_dids() -> Arc<dyn RepoResolver> {
    Arc::new(|target: &RepoTarget| match target {
        RepoTarget::Did(did) => RepoLookup::Hosted(did.clone()),
        RepoTarget::OwnerPath(_, _) => RepoLookup::Unhosted,
    })
}

pub async fn spawn(router: Router, bind: &str) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind(bind).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

pub fn advance_via_receive(bare: &Path, work: &Path, old: &str, new: &str) {
    let oids: Vec<String> = must(work, &["rev-list", "--objects", new, "--not", old])
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect();
    let request = receive_request("refs/heads/main", old, new, &pack_objects(work, &oids));
    let repo = Repo::open(bare).expect("open knot bare");
    let report = knot_pack::receive_pack(&repo, &request).expect("knot receive");
    assert!(
        String::from_utf8_lossy(&report).contains("ok refs/heads/main"),
        "knot must accept a receive that advances main"
    );
}

pub fn seed_branches_and_tag(work: &Path, bares: [&Path; 2], format: ObjectFormat) {
    let fmt = format!("--object-format={}", format.capability());
    std::fs::create_dir_all(work).unwrap();
    must(work, &["init", &fmt, "-q", "-b", "main"]);
    std::fs::write(work.join("README.md"), "seed\n").unwrap();
    must(work, &["add", "-A"]);
    must(work, &["commit", "-q", "-m", "c1"]);
    let c1 = must(work, &["rev-parse", "HEAD"]);
    std::fs::write(work.join("src.txt"), "more\n").unwrap();
    must(work, &["add", "-A"]);
    must(work, &["commit", "-q", "-m", "c2"]);
    must(work, &["checkout", "-q", "-b", "dev", &c1]);
    std::fs::write(work.join("dev.txt"), "branch\n").unwrap();
    must(work, &["add", "-A"]);
    must(work, &["commit", "-q", "-m", "c3"]);
    must(work, &["checkout", "-q", "main"]);
    must(work, &["tag", "-a", "v1", "-m", "release"]);
    bares.into_iter().for_each(|bare| {
        must(
            work,
            &["push", "-q", bare.to_str().unwrap(), "main", "dev", "v1"],
        );
        must(bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    });
}

pub fn seeded(layout: &Layout, did: &RepoDid) -> (Repo, tempfile::TempDir, String, Vec<u8>) {
    let bare = layout.create(did).unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    must(work, &["init", "-q", "-b", "main"]);
    commit(work, "a.txt", "x\n", "c1");
    let c1 = must(work, &["rev-parse", "HEAD"]);
    let oids: Vec<String> = must(work, &["rev-list", "--objects", &c1])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect();
    let pack = pack_objects(work, &oids);
    (bare, work_dir, c1, pack)
}

pub struct Stand {
    pub scan: tempfile::TempDir,
    pub scratch: tempfile::TempDir,
    pub layout: Layout,
    pub addr: SocketAddr,
    pub bare: PathBuf,
}

pub async fn stand(did: &RepoDid) -> Stand {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    layout.create(did).unwrap();
    let bare = layout.repo_path(did).unwrap();
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
    Stand {
        scan,
        scratch,
        layout,
        addr,
        bare,
    }
}
