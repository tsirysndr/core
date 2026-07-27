use std::io::Write;
use std::process::Stdio;

use knot_git::{Layout, RefUpdate};
use knot_pack::{DeltaDepth, PackLimits};
use knot_types::{ObjectCount, ObjectFormat, Oid, RefName, RepoDid};

mod common;
use common::{
    commit, delta_bomb_pack, index_into_bare, must, pack_objects, pack_objects_tuned, pkt,
    receive_request, seeded, unsideband,
};

fn generous() -> PackLimits {
    PackLimits {
        max_objects: ObjectCount::new(1_000_000),
        max_object_bytes: knot_pack::MaxObjectBytes::new(1 << 30),
        max_total_bytes: knot_pack::MaxTotalBytes::new(1 << 31),
        max_delta_depth: DeltaDepth::new(50),
    }
}

fn v2_fetch(wants: &[&str], haves: &[&str], done: bool) -> Vec<u8> {
    let mut req = pkt(b"command=fetch\n");
    req.extend_from_slice(b"0001");
    wants
        .iter()
        .for_each(|want| req.extend(pkt(format!("want {want}\n").as_bytes())));
    haves
        .iter()
        .for_each(|have| req.extend(pkt(format!("have {have}\n").as_bytes())));
    if done {
        req.extend(pkt(b"done\n"));
    }
    req.extend_from_slice(b"0000");
    req
}

fn thin_resolves_against_base(base_pack: &[u8], thin: &[u8]) -> bool {
    let bare = tempfile::tempdir().unwrap();
    knot_fixtures::must(
        bare.path(),
        &["init", "--bare", "-q", bare.path().to_str().unwrap()],
    );
    let feed = |args: &[&str], pack: &[u8]| knot_fixtures::feed(bare.path(), args, pack).0;
    feed(&["index-pack", "--stdin"], base_pack)
        && feed(&["index-pack", "--stdin", "--fix-thin"], thin)
}

fn v2_fetch_thin(want: &str, have: &str) -> Vec<u8> {
    let mut req = pkt(b"command=fetch\n");
    req.extend_from_slice(b"0001");
    req.extend(pkt(b"thin-pack\n"));
    req.extend(pkt(format!("want {want}\n").as_bytes()));
    req.extend(pkt(format!("have {have}\n").as_bytes()));
    req.extend(pkt(b"done\n"));
    req.extend_from_slice(b"0000");
    req
}

fn has_band(resp: &[u8], band: u8) -> bool {
    let mut pos = 0usize;
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
        if resp[pos..end].first() == Some(&band) {
            return true;
        }
        pos = end;
    }
    false
}

fn created_refs(bare: &knot_git::Repo) -> Vec<String> {
    bare.references()
        .unwrap()
        .into_iter()
        .map(|record| record.name.as_str().to_string())
        .collect()
}

#[test]
fn pack_with_missing_parent_is_now_rejected() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let bare = layout.create(&did).unwrap();

    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    must(work, &["init", "-q", "-b", "main"]);
    commit(work, "old.txt", "old\n", "c1");
    let c1 = must(work, &["rev-parse", "HEAD"]);
    must(work, &["rm", "-q", "old.txt"]);
    commit(work, "new.txt", "new\n", "c2");
    let c2 = must(work, &["rev-parse", "HEAD"]);

    let oids: Vec<String> = must(work, &["rev-list", "--objects", &c2, "--not", &c1])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect();
    let pack = pack_objects(work, &oids);

    let mut first = format!("{} {} refs/heads/dangle", Oid::null().to_hex(), c2).into_bytes();
    first.push(0);
    first.extend_from_slice(b"report-status\n");
    let mut req = pkt(&first);
    req.extend_from_slice(b"0000");
    req.extend_from_slice(&pack);

    let report = knot_pack::receive_pack(&bare, &req).unwrap();
    let text = String::from_utf8_lossy(&report).replace('\0', "");

    let created = layout
        .open(&did)
        .unwrap()
        .references()
        .unwrap()
        .into_iter()
        .any(|r| r.name.as_str() == "refs/heads/dangle");

    assert!(
        text.contains("ng refs/heads/dangle missing necessary objects"),
        "pack whose tip has a missing parent must be rejected:\n{text}"
    );
    assert!(!created, "dangling ref mustn't have been created");
}

#[test]
fn ls_refs_prefix_and_cob_hiding() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let (bare, _work, c1, pack) = seeded(&layout, &did);
    knot_pack::receive_pack(
        &bare,
        &receive_request("refs/heads/main", &Oid::null().to_hex(), &c1, &pack),
    )
    .unwrap();
    bare.update_ref(&RefUpdate::Create {
        name: RefName::new("refs/cobs/sh.tangled.repo.collaborator/secret").unwrap(),
        new: Oid::from_hex(&c1).unwrap(),
    })
    .unwrap();

    let ls = |prefix: Option<&str>| {
        let mut req = pkt(b"command=ls-refs\n");
        if let Some(prefix) = prefix {
            req.extend_from_slice(b"0001");
            req.extend(pkt(format!("ref-prefix {prefix}\n").as_bytes()));
        }
        req.extend_from_slice(b"0000");
        String::from_utf8_lossy(&knot_pack::upload_pack(&bare, &req).unwrap()).into_owned()
    };

    let tags = ls(Some("refs/tags/"));
    assert!(
        !tags.contains("refs/heads/main"),
        "tags-only ref-prefix must exclude heads:\n{tags}"
    );

    let heads = ls(Some("refs/heads/"));
    assert!(
        heads.contains("refs/heads/main"),
        "heads ref-prefix must keep heads:\n{heads}"
    );

    let cobs = ls(Some("refs/cobs/"));
    assert!(
        !cobs.contains("refs/cobs"),
        "explicit ref-prefix refs/cobs/ must still return nothing:\n{cobs}"
    );

    let all = ls(None);
    assert!(
        all.contains("refs/heads/main"),
        "unfiltered ls-refs must still advertise heads:\n{all}"
    );
    assert!(
        !all.contains("refs/cobs"),
        "unfiltered ls-refs must never advertise cob refs:\n{all}"
    );
}

#[test]
fn push_namespace_gating_accepts_only_unreserved_refs() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let (bare, _work, c1, pack) = seeded(&layout, &did);

    let cases: [(&str, &str, bool); 3] = [
        (
            "refs/cobs/sh.tangled.repo.collaborator/evil",
            "ng refs/cobs/sh.tangled.repo.collaborator/evil",
            false,
        ),
        (
            "refs/hidden/feature/main",
            "ng refs/hidden/feature/main",
            false,
        ),
        ("refs/notes/commits", "ok refs/notes/commits", true),
    ];
    cases.iter().for_each(|(refname, verdict, lands)| {
        let req = receive_request(refname, &Oid::null().to_hex(), &c1, &pack);
        let report = String::from_utf8_lossy(&knot_pack::receive_pack(&bare, &req).unwrap())
            .replace('\0', "");
        assert!(report.contains(*verdict), "{verdict}:\n{report}");
        assert_eq!(
            created_refs(&bare)
                .iter()
                .any(|name| name.as_str() == *refname),
            *lands,
            "ref landing mismatch for {refname}",
        );
    });
}

#[test]
fn pack_missing_a_blob_is_rejected() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let bare = layout.create(&did).unwrap();

    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    must(work, &["init", "-q", "-b", "main"]);
    commit(work, "a.txt", "secret\n", "c1");
    let c1 = must(work, &["rev-parse", "HEAD"]);
    let tree = must(work, &["rev-parse", "HEAD^{tree}"]);
    let pack = pack_objects(work, &[c1.clone(), tree]);

    let req = receive_request("refs/heads/main", &Oid::null().to_hex(), &c1, &pack);
    let report =
        String::from_utf8_lossy(&knot_pack::receive_pack(&bare, &req).unwrap()).replace('\0', "");
    assert!(
        report.contains("ng refs/heads/main missing necessary objects"),
        "pack whose tree references a missing blob must be rejected:\n{report}"
    );
    assert!(
        created_refs(&bare).is_empty(),
        "ref to an object-incomplete commit mustn't be created"
    );
}

#[test]
fn pack_with_a_submodule_gitlink_is_accepted() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let bare = layout.create(&did).unwrap();

    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    must(work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("a.txt"), "alpha\n").unwrap();
    must(work, &["add", "a.txt"]);
    let absent = "0123456789abcdef0123456789abcdef01234567";
    let cacheinfo = format!("160000,{absent},vendor");
    must(
        work,
        &["update-index", "--add", "--cacheinfo", cacheinfo.as_str()],
    );
    let tree = must(work, &["write-tree"]);
    let head = must(work, &["commit-tree", &tree, "-m", "adds a submodule"]);
    let oids: Vec<String> = must(work, &["rev-list", "--objects", &head])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect();
    let pack = pack_objects(work, &oids);

    let req = receive_request("refs/heads/main", &Oid::null().to_hex(), &head, &pack);
    let report =
        String::from_utf8_lossy(&knot_pack::receive_pack(&bare, &req).unwrap()).replace('\0', "");
    assert!(
        report.contains("ok refs/heads/main"),
        "a gitlink names a submodule commit the host needn't hold, so the push must land:\n{report}"
    );
    assert!(
        created_refs(&bare)
            .iter()
            .any(|name| name.as_str() == "refs/heads/main"),
        "the submodule-bearing ref must be created"
    );
}

#[test]
fn empty_root_commit_over_the_virtual_tree_is_accepted() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let bare = layout.create(&did).unwrap();

    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    must(work, &["init", "-q", "-b", "main"]);
    must(work, &["commit", "-q", "--allow-empty", "-m", "empty root"]);
    let head = must(work, &["rev-parse", "HEAD"]);
    let pack = pack_objects(work, std::slice::from_ref(&head));

    let req = receive_request("refs/heads/main", &Oid::null().to_hex(), &head, &pack);
    let report =
        String::from_utf8_lossy(&knot_pack::receive_pack(&bare, &req).unwrap()).replace('\0', "");
    assert!(
        report.contains("ok refs/heads/main"),
        "an empty root commit points at git's virtual empty tree, so the push must land:\n{report}"
    );
    assert!(
        created_refs(&bare)
            .iter()
            .any(|name| name.as_str() == "refs/heads/main"),
        "the empty root ref must be created"
    );
}

fn sole_idx(pack_dir: std::path::PathBuf) -> Vec<u8> {
    let idx = std::fs::read_dir(&pack_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "idx"))
        .expect("exactly one idx file");
    std::fs::read(idx).unwrap()
}

fn folded_index_matches_canonical_git(format: ObjectFormat) {
    let fmt = format!("--object-format={}", format.capability());
    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    must(work, &["init", &fmt, "-q", "-b", "main"]);
    std::fs::create_dir_all(work.join("dir/nested")).unwrap();
    let grow = |lines: usize| {
        (0..lines)
            .map(|n| format!("line {n}\n"))
            .collect::<String>()
    };
    (0..40).for_each(|revision| {
        std::fs::write(work.join("dir/nested/a.txt"), grow(revision * 50 + 10)).unwrap();
        std::fs::write(work.join("b.txt"), grow(revision * 30 + 5)).unwrap();
        must(work, &["add", "-A"]);
        must(work, &["commit", "-q", "-m", &format!("rev {revision}")]);
    });
    must(work, &["tag", "-a", "v1", "-m", "release one"]);
    let head = must(work, &["rev-parse", "HEAD"]);
    let oids: Vec<String> = must(work, &["rev-list", "--objects", "--all"])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect();
    let pack = pack_objects_tuned(work, &oids, true);

    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path()).with_object_format(format);
    let did = RepoDid::new("did:plc:squid").unwrap();
    let bare = layout.create(&did).unwrap();
    let report = String::from_utf8_lossy(
        &knot_pack::receive_pack(
            &bare,
            &receive_request("refs/heads/main", &format.null_oid().to_hex(), &head, &pack),
        )
        .unwrap(),
    )
    .replace('\0', "");
    assert!(
        report.contains("ok refs/heads/main"),
        "fold push must land:\n{report}"
    );
    let mine = sole_idx(bare.objects_dir().join("pack"));

    let scratch = tempfile::tempdir().unwrap();
    must(scratch.path(), &["init", "--bare", &fmt, "-q"]);
    let mut child = knot_fixtures::command(scratch.path())
        .args(["index-pack", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&pack).unwrap();
    assert!(
        child.wait().unwrap().success(),
        "canonical index-pack failed"
    );
    let canonical = sole_idx(scratch.path().join("objects/pack"));

    assert_eq!(
        mine,
        canonical,
        "folded ingest index must be byte-identical to canonical git index-pack for {}",
        format.capability()
    );
}

fn bushy_delta_repo(work: &std::path::Path, fmt: &str) -> String {
    must(work, &["init", fmt, "-q", "-b", "main"]);
    let body: String = (0..400).map(|n| format!("shared line {n}\n")).collect();
    (0..10).for_each(|revision| {
        (0..150).for_each(|file| {
            let contents = format!(
                "{body}unique {file} rev {revision}\ntail {}\n",
                file * 7 + revision
            );
            std::fs::write(work.join(format!("f{file:03}.txt")), contents).unwrap();
        });
        must(work, &["add", "-A"]);
        must(work, &["commit", "-q", "-m", &format!("rev {revision}")]);
    });
    must(work, &["rev-parse", "HEAD"])
}

#[test]
fn folded_bushy_push_matches_git_through_the_parallel_delta_path() {
    let format = ObjectFormat::SHA1;
    let fmt = format!("--object-format={}", format.capability());
    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    let head = bushy_delta_repo(work, &fmt);
    let oids: Vec<String> = must(work, &["rev-list", "--objects", "--all"])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect();
    let pack = pack_objects_tuned(work, &oids, true);

    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path()).with_object_format(format);
    let did = RepoDid::new("did:plc:squid").unwrap();
    let bare = layout.create(&did).unwrap();
    let report = String::from_utf8_lossy(
        &knot_pack::receive_pack(
            &bare,
            &receive_request("refs/heads/main", &format.null_oid().to_hex(), &head, &pack),
        )
        .unwrap(),
    )
    .replace('\0', "");
    assert!(
        report.contains("ok refs/heads/main"),
        "bushy push must land:\n{report}"
    );
    let mine = sole_idx(bare.objects_dir().join("pack"));

    let scratch = tempfile::tempdir().unwrap();
    must(scratch.path(), &["init", "--bare", &fmt, "-q"]);
    let mut child = knot_fixtures::command(scratch.path())
        .args(["index-pack", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&pack).unwrap();
    assert!(
        child.wait().unwrap().success(),
        "canonical index-pack failed"
    );
    let canonical = sole_idx(scratch.path().join("objects/pack"));
    assert_eq!(
        mine, canonical,
        "a bushy delta pack drives the work-stealing traversal; its folded index must match canonical git"
    );
}

#[test]
fn a_forced_base_spill_folds_the_bushy_pack_to_the_canonical_index() {
    let format = ObjectFormat::SHA1;
    let fmt = format!("--object-format={}", format.capability());
    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    let _head = bushy_delta_repo(work, &fmt);
    let oids: Vec<String> = must(work, &["rev-list", "--objects", "--all"])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect();
    let pack = pack_objects_tuned(work, &oids, true);

    let staged = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(staged.path(), &pack).unwrap();
    let objects = tempfile::tempdir().unwrap();
    let folded = knot_pack::bench_ingest_with_base_budget(
        objects.path(),
        staged.path(),
        format.kind(),
        Some(4096),
    )
    .unwrap();
    assert!(
        folded,
        "the bushy pack is self-contained, so a forced spill must still fold it"
    );
    let mine = sole_idx(objects.path().join("pack"));

    let scratch = tempfile::tempdir().unwrap();
    must(scratch.path(), &["init", "--bare", &fmt, "-q"]);
    let mut child = knot_fixtures::command(scratch.path())
        .args(["index-pack", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&pack).unwrap();
    assert!(
        child.wait().unwrap().success(),
        "canonical index-pack failed"
    );
    let canonical = sole_idx(scratch.path().join("objects/pack"));
    assert_eq!(
        mine, canonical,
        "paging the delta-base working set to disk mustn't change the folded index"
    );
}

#[test]
fn the_streaming_connectivity_verify_agrees_with_the_in_ram_map() {
    let kind = ObjectFormat::SHA1.kind();
    let stage = |pack: &[u8]| -> Option<bool> {
        let staged = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(staged.path(), pack).unwrap();
        let objects = tempfile::tempdir().unwrap();
        knot_pack::bench_ingest_external(objects.path(), staged.path(), kind).unwrap()
    };

    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    must(work, &["init", "-q", "-b", "main"]);
    commit(work, "a.txt", "hello\n", "c1");
    let all: Vec<String> = must(work, &["rev-list", "--objects", "--all"])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect();
    assert_eq!(
        stage(&pack_objects(work, &all)),
        Some(true),
        "the streaming probe must accept a self-contained pack"
    );

    let c1 = must(work, &["rev-parse", "HEAD"]);
    let tree = must(work, &["rev-parse", "HEAD^{tree}"]);
    assert_eq!(
        stage(&pack_objects(work, &[c1, tree])),
        Some(false),
        "the streaming probe must reject a pack whose tree references a missing blob"
    );

    let sub_dir = tempfile::tempdir().unwrap();
    let sub = sub_dir.path();
    must(sub, &["init", "-q", "-b", "main"]);
    std::fs::write(sub.join("x.txt"), "sub\n").unwrap();
    must(sub, &["add", "x.txt"]);
    let cacheinfo = "160000,0123456789abcdef0123456789abcdef01234567,vendor";
    must(sub, &["update-index", "--add", "--cacheinfo", cacheinfo]);
    let subtree = must(sub, &["write-tree"]);
    let subhead = must(sub, &["commit-tree", &subtree, "-m", "with gitlink"]);
    let sub_oids: Vec<String> = must(sub, &["rev-list", "--objects", &subhead])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect();
    assert_eq!(
        stage(&pack_objects(sub, &sub_oids)),
        Some(true),
        "a gitlink names a submodule commit the host needn't hold, so absence is fine"
    );
}

#[test]
fn folded_first_push_index_is_byte_identical_to_canonical_git() {
    folded_index_matches_canonical_git(ObjectFormat::SHA1);
}

#[test]
fn folded_first_push_index_is_byte_identical_under_sha256() {
    folded_index_matches_canonical_git(ObjectFormat::SHA256);
}

#[test]
fn garbage_pack_reports_unpack_error() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let bare = layout.create(&did).unwrap();

    let fake = "1111111111111111111111111111111111111111";
    let req = receive_request(
        "refs/heads/main",
        &Oid::null().to_hex(),
        fake,
        b"not a packfile",
    );
    let report =
        String::from_utf8_lossy(&knot_pack::receive_pack(&bare, &req).unwrap()).replace('\0', "");
    assert!(
        !report.contains("unpack ok"),
        "non-PACK body mustn't report unpack ok:\n{report}"
    );
    assert!(
        report.contains("unpack pack:") && report.contains("PACK signature"),
        "malformed pack must surface an unpack error:\n{report}"
    );
    assert!(
        created_refs(&bare).is_empty(),
        "no ref may be created when pack is malformed"
    );
}

#[test]
fn delete_only_push_has_no_pack() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let (bare, _work, c1, pack) = seeded(&layout, &did);

    let create = receive_request("refs/heads/main", &Oid::null().to_hex(), &c1, &pack);
    let created = String::from_utf8_lossy(&knot_pack::receive_pack(&bare, &create).unwrap())
        .replace('\0', "");
    assert!(
        created.contains("unpack ok") && created.contains("ok refs/heads/main"),
        "setup push failed:\n{created}"
    );

    let delete = receive_request("refs/heads/main", &c1, &Oid::null().to_hex(), b"");
    let report = String::from_utf8_lossy(&knot_pack::receive_pack(&bare, &delete).unwrap())
        .replace('\0', "");
    assert!(
        report.contains("unpack ok"),
        "delete-only push has no pack and must still report unpack ok:\n{report}"
    );
    assert!(
        report.contains("ok refs/heads/main"),
        "deleting head must succeed:\n{report}"
    );
    assert!(
        created_refs(&bare).is_empty(),
        "ref must be gone after a delete"
    );
}

#[test]
fn v2_fetch_negotiation_acks_readies_waits_and_ignores_unknowns() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let (bare, _path, old, tip, _blob) = pushed_history(&layout, &did);

    let fake_have = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let nak = String::from_utf8_lossy(
        &knot_pack::upload_pack(&bare, &v2_fetch(&[&tip], &[fake_have], false)).unwrap(),
    )
    .into_owned();
    assert!(
        nak.contains("acknowledgments"),
        "must open acknowledgments:\n{nak}"
    );
    assert!(nak.contains("NAK"), "no common commit -> NAK:\n{nak}");
    assert!(
        !nak.contains("packfile"),
        "with no common commit server mustn't send a pack this round:\n{nak}"
    );

    let bytes = knot_pack::upload_pack(&bare, &v2_fetch(&[&tip], &[&old], false)).unwrap();
    let ready = String::from_utf8_lossy(&bytes).into_owned();
    assert!(
        ready.contains(&format!("ACK {old}")),
        "must ACK common commit:\n{ready}"
    );
    assert!(
        ready.contains("ready"),
        "must declare ready once a common commit is found"
    );
    assert!(
        ready.contains("packfile"),
        "must open packfile section after ready"
    );
    assert!(
        bytes.windows(4).any(|window| window == b"PACK"),
        "side-band payload must contain a real PACK"
    );

    let waiting = String::from_utf8_lossy(
        &knot_pack::upload_pack(
            &bare,
            &v2_fetch_with(&[&tip], &[&old], false, &["wait-for-done"]),
        )
        .unwrap(),
    )
    .into_owned();
    assert!(
        waiting.contains(&format!("ACK {old}")),
        "wait-for-done still acknowledges common commit:\n{waiting}"
    );
    assert!(
        !waiting.contains("ready"),
        "wait-for-done mustn't declare ready; it waits for the client's done:\n{waiting}"
    );
    assert!(
        !waiting.contains("packfile"),
        "wait-for-done mustn't open pack before done arrives:\n{waiting}"
    );
    let finished = knot_pack::upload_pack(
        &bare,
        &v2_fetch_with(&[&tip], &[&old], true, &["wait-for-done"]),
    )
    .unwrap();
    let finished_text = String::from_utf8_lossy(&finished).into_owned();
    assert!(
        finished_text.contains("packfile"),
        "once done arrives server opens the pack:\n{finished_text}"
    );
    assert!(
        finished.windows(4).any(|window| window == b"PACK"),
        "follow-up round must contain a real PACK"
    );

    let mut req = pkt(b"command=fetch\n");
    req.extend_from_slice(b"0001");
    [
        "thin-pack\n",
        "ofs-delta\n",
        "include-tag\n",
        "no-progress\n",
        "some-future-capability-knot-does-not-know\n",
    ]
    .iter()
    .for_each(|arg| req.extend(pkt(arg.as_bytes())));
    req.extend(pkt(format!("want {tip}\n").as_bytes()));
    req.extend(pkt(b"done\n"));
    req.extend_from_slice(b"0000");
    let resp = knot_pack::upload_pack(&bare, &req).unwrap();
    let text = String::from_utf8_lossy(&resp);
    assert!(
        text.contains("packfile"),
        "unknown fetch arguments must be ignored and pack still produced:\n{text}"
    );
    assert!(
        resp.windows(4).any(|window| window == b"PACK"),
        "response must still contain a real PACK despite unknown arguments"
    );

    let progress = knot_pack::upload_pack(&bare, &v2_fetch(&[&tip], &[], true)).unwrap();
    assert!(
        has_band(&progress, 2),
        "fetch must emit sideband band-2 progress"
    );
    let suppressed =
        knot_pack::upload_pack(&bare, &v2_fetch_with(&[&tip], &[], true, &["no-progress"]))
            .unwrap();
    assert!(
        !has_band(&suppressed, 2),
        "no-progress must suppress band-2 output"
    );
    assert!(
        suppressed.windows(4).any(|window| window == b"PACK"),
        "pack itself must still be sent when progress is suppressed"
    );
}

fn v2_fetch_with(wants: &[&str], haves: &[&str], done: bool, extra: &[&str]) -> Vec<u8> {
    let mut req = pkt(b"command=fetch\n");
    req.extend_from_slice(b"0001");
    extra
        .iter()
        .for_each(|line| req.extend(pkt(format!("{line}\n").as_bytes())));
    wants
        .iter()
        .for_each(|want| req.extend(pkt(format!("want {want}\n").as_bytes())));
    haves
        .iter()
        .for_each(|have| req.extend(pkt(format!("have {have}\n").as_bytes())));
    if done {
        req.extend(pkt(b"done\n"));
    }
    req.extend_from_slice(b"0000");
    req
}

fn pkt_payloads(resp: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
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
        out.push(resp[pos..end].to_vec());
        pos = end;
    }
    out
}

fn banded(payloads: &[Vec<u8>], section: &[u8]) -> bool {
    payloads
        .iter()
        .any(|payload| payload.first() == Some(&1) && payload[1..].starts_with(section))
}

fn plain(payloads: &[Vec<u8>], section: &[u8]) -> bool {
    payloads.iter().any(|payload| payload == section)
}

fn pushed_history(
    layout: &Layout,
    did: &RepoDid,
) -> (knot_git::Repo, std::path::PathBuf, String, String, String) {
    let bare = layout.create(did).unwrap();
    let bare_path = layout.repo_path(did).unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    must(work, &["init", "-q", "-b", "main"]);
    commit(work, "a.txt", "1\n", "c1");
    let old = must(work, &["rev-parse", "HEAD"]);
    let blob = must(work, &["rev-parse", "HEAD:a.txt"]);
    commit(work, "b.txt", "2\n", "c2");
    let tip = must(work, &["rev-parse", "HEAD"]);
    must(work, &["push", "-q", bare_path.to_str().unwrap(), "main"]);
    (bare, bare_path, old, tip, blob)
}

#[test]
fn v2_sideband_all_band_frames_negotiation_and_packfile_uris() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let (bare, bare_path, old, tip, blob) = pushed_history(&layout, &did);

    let plain_resp =
        knot_pack::upload_pack(&bare, &v2_fetch_with(&[&tip], &[&old], false, &[])).unwrap();
    let plain_payloads = pkt_payloads(&plain_resp);
    assert!(
        plain(&plain_payloads, b"acknowledgments\n"),
        "without sideband-all acknowledgments header is a plain pkt-line"
    );
    assert!(
        !banded(&plain_payloads, b"acknowledgments\n"),
        "without sideband-all negotiation mustn't be band-framed"
    );

    let banded_resp = knot_pack::upload_pack(
        &bare,
        &v2_fetch_with(&[&tip], &[&old], false, &["sideband-all"]),
    )
    .unwrap();
    let banded_payloads = pkt_payloads(&banded_resp);
    assert!(
        banded(&banded_payloads, b"acknowledgments\n"),
        "sideband-all moves acknowledgments header into band 1"
    );
    assert!(
        banded(&banded_payloads, &format!("ACK {old}\n").into_bytes()),
        "sideband-all wraps ACK lines too"
    );
    assert!(
        !plain(&banded_payloads, b"acknowledgments\n"),
        "under sideband-all nothing in negotiation is sent as a plain pkt-line"
    );

    let packhash = "0123456789abcdef0123456789abcdef01234567";
    let uri = format!("https://cdn.nel.pet/{packhash}.pack");
    must(
        &bare_path,
        &[
            "config",
            "uploadpack.blobPackfileUri",
            &format!("{blob} {packhash} {uri}"),
        ],
    );
    let bare = layout.open(&did).unwrap();

    let plain_uris = knot_pack::upload_pack(
        &bare,
        &v2_fetch_with(&[&tip], &[], false, &["packfile-uris https"]),
    )
    .unwrap();
    let plain_uri_payloads = pkt_payloads(&plain_uris);
    assert!(
        plain(&plain_uri_payloads, b"packfile-uris\n"),
        "without sideband-all packfile-uris header is a plain pkt-line, not band 1"
    );
    assert!(
        plain(
            &plain_uri_payloads,
            format!("{packhash} {uri}\n").as_bytes()
        ),
        "configured blob uri must be advertised verbatim"
    );

    let banded_uris = knot_pack::upload_pack(
        &bare,
        &v2_fetch_with(
            &[&tip],
            &[],
            false,
            &["packfile-uris https", "sideband-all"],
        ),
    )
    .unwrap();
    let banded_uri_payloads = pkt_payloads(&banded_uris);
    assert!(
        banded(&banded_uri_payloads, b"packfile-uris\n"),
        "sideband-all moves packfile-uris header into band 1"
    );
    assert!(
        !plain(&banded_uri_payloads, b"packfile-uris\n"),
        "under sideband-all packfile-uris header is never a plain pkt-line"
    );
}

fn big_blob_repo(
    layout: &Layout,
    did: &RepoDid,
    size: usize,
) -> (knot_git::Repo, String, Vec<u8>, usize) {
    let bare = layout.create(did).unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    must(work, &["init", "-q", "-b", "main"]);
    let filler: String = std::iter::repeat_n('a', size).collect();
    commit(work, "big.txt", &filler, "c1");
    let c1 = must(work, &["rev-parse", "HEAD"]);
    let oids: Vec<String> = must(work, &["rev-list", "--objects", &c1])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect();
    let count = oids.len();
    let pack = pack_objects(work, &oids);
    (bare, c1, pack, count)
}

#[test]
fn pack_limits_reject_oversized_and_overdeep_packs() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());

    let (size_bare, size_tip, size_pack, count) =
        big_blob_repo(&layout, &RepoDid::new("did:plc:squid").unwrap(), 256 * 1024);
    assert!(count >= 2, "commit packs at least a commit and a tree");

    let ofs_bare = layout
        .create(&RepoDid::new("did:plc:whelk").unwrap())
        .unwrap();
    let ofs_work = tempfile::tempdir().unwrap();
    let ow = ofs_work.path();
    must(ow, &["init", "-q", "-b", "main"]);
    let base: String = std::iter::repeat_n('a', 128 * 1024).collect();
    commit(ow, "big.txt", &base, "c1");
    let oc1 = must(ow, &["rev-parse", "HEAD"]);
    let mut tweaked = base.clone();
    tweaked.push('b');
    commit(ow, "big.txt", &tweaked, "c2");
    let oc2 = must(ow, &["rev-parse", "HEAD"]);
    let ofs_oids: Vec<String> = must(ow, &["rev-list", "--objects", &oc1, &oc2])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect();
    let ofs_pack = pack_objects_tuned(ow, &ofs_oids, true);

    let (ref_bare, ref_tip, ref_pack) =
        ref_delta_chain_repo(&layout, &RepoDid::new("did:plc:limpet").unwrap(), 4);

    let cases: [(&knot_git::Repo, &[u8], &str, PackLimits, &str); 5] = [
        (
            &size_bare,
            size_pack.as_slice(),
            size_tip.as_str(),
            PackLimits {
                max_object_bytes: knot_pack::MaxObjectBytes::new(4096),
                ..generous()
            },
            "unpack pack exceeds per-object size limit",
        ),
        (
            &size_bare,
            size_pack.as_slice(),
            size_tip.as_str(),
            PackLimits {
                max_total_bytes: knot_pack::MaxTotalBytes::new(4096),
                ..generous()
            },
            "unpack pack exceeds total decompressed size limit",
        ),
        (
            &size_bare,
            size_pack.as_slice(),
            size_tip.as_str(),
            PackLimits {
                max_objects: ObjectCount::new(count - 1),
                ..generous()
            },
            "unpack pack exceeds object count limit",
        ),
        (
            &ofs_bare,
            ofs_pack.as_slice(),
            oc2.as_str(),
            PackLimits {
                max_delta_depth: DeltaDepth::new(0),
                ..generous()
            },
            "unpack pack exceeds delta chain depth limit",
        ),
        (
            &ref_bare,
            ref_pack.as_slice(),
            ref_tip.as_str(),
            PackLimits {
                max_delta_depth: DeltaDepth::new(0),
                ..generous()
            },
            "unpack pack exceeds delta chain depth limit",
        ),
    ];
    cases
        .into_iter()
        .for_each(|(bare, pack, tip, limits, message)| {
            let req = receive_request("refs/heads/main", &Oid::null().to_hex(), tip, pack);
            let report = String::from_utf8_lossy(
                &knot_pack::receive_pack_with_limits(bare, &req, &limits).unwrap(),
            )
            .replace('\0', "");
            assert!(report.contains(message), "{message}:\n{report}");
            assert!(
                created_refs(bare).is_empty(),
                "refused pack must create no ref"
            );
        });
}

#[test]
fn a_delta_declaring_an_oversized_result_is_rejected_on_both_ingest_paths() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let limits = generous();
    let bomb = delta_bomb_pack(1 << 40);
    let absent_tip = "f".repeat(40);

    let empty = layout
        .create(&RepoDid::new("did:plc:cuttle").unwrap())
        .unwrap();
    let fresh_req = receive_request("refs/heads/main", &Oid::null().to_hex(), &absent_tip, &bomb);
    let fresh_report = String::from_utf8_lossy(
        &knot_pack::receive_pack_with_limits(&empty, &fresh_req, &limits).unwrap(),
    )
    .replace('\0', "");
    assert!(
        fresh_report.contains("reconstructed size"),
        "the fold traversal must refuse a delta declaring an oversized result:\n{fresh_report}"
    );
    assert!(
        created_refs(&empty).is_empty(),
        "refused bomb must create no ref"
    );

    let (seeded_bare, tip, seed_pack, _) =
        big_blob_repo(&layout, &RepoDid::new("did:plc:scallop").unwrap(), 1024);
    let seed_req = receive_request("refs/heads/main", &Oid::null().to_hex(), &tip, &seed_pack);
    let seed_report = String::from_utf8_lossy(
        &knot_pack::receive_pack_with_limits(&seeded_bare, &seed_req, &limits).unwrap(),
    )
    .replace('\0', "");
    assert!(
        seed_report.contains("unpack ok"),
        "seeding the repo must succeed:\n{seed_report}"
    );

    let bomb_req = receive_request("refs/heads/bomb", &Oid::null().to_hex(), &absent_tip, &bomb);
    let meter_report = String::from_utf8_lossy(
        &knot_pack::receive_pack_with_limits(&seeded_bare, &bomb_req, &limits).unwrap(),
    )
    .replace('\0', "");
    assert!(
        meter_report.contains("per-object size"),
        "the meter gate must refuse the bomb on the buffered path:\n{meter_report}"
    );
}

fn ref_delta_chain_repo(
    layout: &Layout,
    did: &RepoDid,
    revisions: usize,
) -> (knot_git::Repo, String, Vec<u8>) {
    let bare = layout.create(did).unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    must(work, &["init", "-q", "-b", "main"]);
    let tips: Vec<String> = (0..revisions)
        .map(|step| {
            let body: String = std::iter::repeat_n('a', 64 * 1024).collect();
            commit(
                work,
                "big.txt",
                &format!("{body}{step}\n"),
                &format!("c{step}"),
            );
            must(work, &["rev-parse", "HEAD"])
        })
        .collect();
    let tip = tips.last().unwrap().clone();
    let oids: Vec<String> = must(work, &["rev-list", "--objects", "HEAD"])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect();
    let pack = pack_objects_tuned(work, &oids, false);
    (bare, tip, pack)
}

#[test]
fn ref_delta_with_in_pack_base_is_resolved() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let (bare, tip, pack) = ref_delta_chain_repo(&layout, &did, 4);

    let req = receive_request("refs/heads/main", &Oid::null().to_hex(), &tip, &pack);
    let report =
        String::from_utf8_lossy(&knot_pack::receive_pack(&bare, &req).unwrap()).replace('\0', "");
    assert!(
        report.contains("unpack ok") && report.contains("ok refs/heads/main"),
        "self-contained ref-delta pack, which gix alone cannot index, must be resolved natively:\n{report}"
    );
    assert_eq!(
        bare.find_ref(&RefName::new("refs/heads/main").unwrap())
            .unwrap(),
        Some(Oid::from_hex(&tip).unwrap()),
        "ref must point at the pushed tip"
    );
    let reopened = layout.open(&did).unwrap();
    assert!(
        reopened.find_commit(Oid::from_hex(&tip).unwrap()).is_ok(),
        "every resolved object must be readable from the odb after push"
    );
}

#[test]
fn crafted_stale_old_oid_is_rejected_by_server_cas() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let bare = layout.create(&did).unwrap();

    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    must(work, &["init", "-q", "-b", "main"]);
    commit(work, "a.txt", "one\n", "c1");
    let c1 = must(work, &["rev-parse", "HEAD"]);
    commit(work, "a.txt", "two\n", "c2");
    let c2 = must(work, &["rev-parse", "HEAD"]);

    let oids1: Vec<String> = must(work, &["rev-list", "--objects", &c1])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect();
    let create = receive_request(
        "refs/heads/main",
        &Oid::null().to_hex(),
        &c1,
        &pack_objects(work, &oids1),
    );
    let created = String::from_utf8_lossy(&knot_pack::receive_pack(&bare, &create).unwrap())
        .replace('\0', "");
    assert!(
        created.contains("ok refs/heads/main"),
        "setup push failed:\n{created}"
    );

    let wrong = "1234567812345678123456781234567812345678";
    let oids2: Vec<String> = must(work, &["rev-list", "--objects", &c2, "--not", &c1])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect();
    let stale = receive_request("refs/heads/main", wrong, &c2, &pack_objects(work, &oids2));
    let report =
        String::from_utf8_lossy(&knot_pack::receive_pack(&bare, &stale).unwrap()).replace('\0', "");
    assert!(
        report.contains("unpack ok"),
        "pack itself is valid and must unpack:\n{report}"
    );
    assert!(
        report.contains("ng refs/heads/main"),
        "crafted stale old-oid must be refused by the server-side compare-and-swap:\n{report}"
    );
    assert_eq!(
        bare.find_ref(&RefName::new("refs/heads/main").unwrap())
            .unwrap(),
        Some(Oid::from_hex(&c1).unwrap()),
        "ref must stay at its original tip after a rejected stale update"
    );
}

#[test]
fn fetch_emits_a_thin_pack_against_client_haves() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let bare = layout.create(&did).unwrap();

    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    must(work, &["init", "-q", "-b", "main"]);
    let big: String = (0..20_000).map(|line| format!("line {line}\n")).collect();
    let small: String = (0..5_000).map(|line| format!("line {line}\n")).collect();
    commit(work, "f.txt", &big, "c1");
    let c1 = must(work, &["rev-parse", "HEAD"]);
    commit(work, "f.txt", &small, "c2");
    let c2 = must(work, &["rev-parse", "HEAD"]);

    let oids: Vec<String> = must(work, &["rev-list", "--objects", &c2])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect();
    let pack = pack_objects_tuned(work, &oids, true);
    let report = String::from_utf8_lossy(
        &knot_pack::receive_pack(
            &bare,
            &receive_request("refs/heads/main", &Oid::null().to_hex(), &c2, &pack),
        )
        .unwrap(),
    )
    .replace('\0', "");
    assert!(
        report.contains("ok refs/heads/main"),
        "setup push failed:\n{report}"
    );

    let thin = unsideband(&knot_pack::upload_pack(&bare, &v2_fetch_thin(&c2, &c1)).unwrap());
    assert!(
        !index_into_bare(&[], &thin),
        "thin-pack fetch must delta against the client's have and omit it, so it cannot resolve standalone"
    );

    let base_oids: Vec<String> = must(work, &["rev-list", "--objects", &c1])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect();
    assert!(
        thin_resolves_against_base(&pack_objects(work, &base_oids), &thin),
        "client that already has the base must resolve the thin pack w/ --fix-thin"
    );

    let fat = unsideband(&knot_pack::upload_pack(&bare, &v2_fetch(&[&c2], &[&c1], true)).unwrap());
    assert!(
        index_into_bare(&[], &fat),
        "without thin-pack same fetch must be self-contained"
    );
}

#[test]
fn valid_pack_passes_default_limits() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let (bare, c1, pack, _) = big_blob_repo(&layout, &did, 64 * 1024);

    let req = receive_request("refs/heads/main", &Oid::null().to_hex(), &c1, &pack);
    let report =
        String::from_utf8_lossy(&knot_pack::receive_pack(&bare, &req).unwrap()).replace('\0', "");
    assert!(
        report.contains("unpack ok") && report.contains("ok refs/heads/main"),
        "valid pack within the default limits must be accepted:\n{report}"
    );
}

#[test]
fn selection_and_full_clone_expansion_abort_past_object_limit_and_deadline() {
    use std::time::Duration;

    use knot_bench::{ChurnCount, CommitCount, HistorySpec, PathCount, build_history};
    use knot_git::{Filter, PackBudget, SelectionLimit};

    let history = build_history(HistorySpec {
        commits: CommitCount::new(8),
        paths: PathCount::new(16),
        churn: ChurnCount::new(2),
    });
    let repo = history.repo();
    let tips = history.tips();
    let stall = Duration::from_secs(60);

    let selected = repo
        .select_pack_objects_filtered(
            knot_git::Wants::new(&tips),
            knot_git::Haves::new(&[]),
            Filter::None,
            PackBudget::unbounded(),
        )
        .unwrap()
        .send
        .len();
    assert!(
        selected > 4,
        "fixture must contain more objects than the limit under test, has {selected}"
    );
    assert!(
        matches!(
            repo.select_pack_objects_filtered(
                knot_git::Wants::new(&tips),
                knot_git::Haves::new(&[]),
                Filter::None,
                PackBudget::new(ObjectCount::new(4), stall)
            ),
            Err(knot_git::GitError::Selection(SelectionLimit::Objects))
        ),
        "selection past the object-set limit must abort, freeing its core within the budget"
    );
    assert!(
        matches!(
            repo.select_pack_objects_filtered(
                knot_git::Wants::new(&tips),
                knot_git::Haves::new(&[]),
                Filter::None,
                PackBudget::new(ObjectCount::new(usize::MAX), Duration::ZERO)
            ),
            Err(knot_git::GitError::Selection(SelectionLimit::Time))
        ),
        "selection that makes no progress within its stall window must abort with a time limit, not a partial pack"
    );

    let dir = repo.objects_dir();
    let fmt = repo.object_format().kind();
    let roots = repo.clone_roots(&tips, PackBudget::unbounded()).unwrap();
    let expanded = knot_pack::count_expanded(
        &dir,
        roots.clone(),
        ObjectCount::new(usize::MAX),
        stall,
        fmt,
    )
    .unwrap()
    .len();
    assert!(
        expanded > 4,
        "fixture must contain more objects than the limit under test, has {expanded}"
    );
    assert!(
        matches!(
            knot_pack::count_expanded(&dir, roots.clone(), ObjectCount::new(4), stall, fmt),
            Err(knot_pack::PackError::SelectionTooLarge)
        ),
        "full-clone expansion past the object limit must abort before the entry stream opens"
    );
    assert!(
        matches!(
            knot_pack::count_expanded(
                &dir,
                roots,
                ObjectCount::new(usize::MAX),
                Duration::ZERO,
                fmt
            ),
            Err(knot_pack::PackError::SelectionTimeout)
        ),
        "full-clone expansion that makes no progress within its stall window must abort with a time limit, \
         not a partial pack"
    );
}

#[test]
fn parallel_selection_matches_the_oracle_and_honors_its_budget() {
    use std::time::Duration;

    use knot_bench::{ChurnCount, CommitCount, HistorySpec, PathCount, build_history};
    use knot_git::{Filter, PackBudget, SelectionLimit};

    let history = build_history(HistorySpec {
        commits: CommitCount::new(5000),
        paths: PathCount::new(2),
        churn: ChurnCount::new(1),
    });
    let repo = history.repo();
    let tips = history.tips();
    let stall = Duration::from_secs(60);

    let selected = repo
        .select_pack_objects_filtered(
            knot_git::Wants::new(&tips),
            knot_git::Haves::new(&[]),
            Filter::None,
            PackBudget::unbounded(),
        )
        .unwrap()
        .send;

    let dir = repo.objects_dir();
    let fmt = repo.object_format().kind();
    let roots = repo.clone_roots(&tips, PackBudget::unbounded()).unwrap();
    let expanded = knot_pack::count_expanded(&dir, roots, ObjectCount::new(usize::MAX), stall, fmt)
        .unwrap()
        .len();
    assert_eq!(
        selected.len(),
        expanded,
        "the parallel selection walk must reach the same object set as the expansion oracle"
    );

    assert!(
        matches!(
            repo.select_pack_objects_filtered(
                knot_git::Wants::new(&tips),
                knot_git::Haves::new(&[]),
                Filter::None,
                PackBudget::new(ObjectCount::new(4), stall)
            ),
            Err(knot_git::GitError::Selection(SelectionLimit::Objects))
        ),
        "the parallel walk must honor its object limit"
    );
    assert!(
        matches!(
            repo.select_pack_objects_filtered(
                knot_git::Wants::new(&tips),
                knot_git::Haves::new(&[]),
                Filter::None,
                PackBudget::new(ObjectCount::new(usize::MAX), Duration::ZERO)
            ),
            Err(knot_git::GitError::Selection(SelectionLimit::Time))
        ),
        "the parallel walk with no progress budget must abort on its stall window"
    );
}

#[test]
fn full_clone_roots_expand_a_directly_wanted_tree() {
    use std::time::Duration;

    use knot_bench::{ChurnCount, CommitCount, HistorySpec, PathCount, build_history};
    use knot_git::{Filter, PackBudget};
    use knot_types::Oid;

    let history = build_history(HistorySpec {
        commits: CommitCount::new(4),
        paths: PathCount::new(16),
        churn: ChurnCount::new(2),
    });
    let repo = history.repo();
    let tip = history.tip();
    let tree = Oid::from(
        repo.git()
            .rev_parse_single(format!("{}^{{tree}}", tip.to_hex()).as_bytes())
            .unwrap()
            .detach(),
    );

    let slow = repo
        .select_pack_objects_filtered(
            knot_git::Wants::new(&[tree]),
            knot_git::Haves::new(&[]),
            Filter::None,
            PackBudget::unbounded(),
        )
        .unwrap()
        .send
        .len();
    assert!(
        slow > 1,
        "directly-wanted tree must include its blobs and subtrees, the selection walk found {slow}"
    );

    let stall = Duration::from_secs(60);
    let roots = repo.clone_roots(&[tree], PackBudget::unbounded()).unwrap();
    let fast = knot_pack::count_expanded(
        &repo.objects_dir(),
        roots,
        ObjectCount::new(usize::MAX),
        stall,
        repo.object_format().kind(),
    )
    .unwrap()
    .len();
    assert_eq!(
        fast, slow,
        "full-clone fast path must enumerate the same object set as the selection walk \
         for a directly-wanted tree"
    );
}
