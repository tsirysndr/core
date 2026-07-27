use std::path::Path;
use std::time::Instant;

use knot_cob::{CobHome, CobStore};
use knot_cobs::{Grant, MembersChange};
use knot_git::{Layout, Repo};
use knot_pack::{
    MaxWireBytes, PackLimits, PackReceiver, ReceiveCommand, ReceiveFramer, ReceiveGuard,
    RefDecision, receive_pack_guarded, receive_request_complete, sweep_incoming,
};
use knot_runtime::{K256Signer, SeededEntropy, Signer};
use knot_types::{AccountDid, ActorId, ObjectFormat, Oid, RepoDid, UnixSeconds};

const SHA1: ObjectFormat = ObjectFormat::SHA1;

mod common;
use common::{commit, must, pack_objects, receive_request};

fn limits() -> PackLimits {
    PackLimits::default()
}

fn seeded_pack(layout: &Layout, did: &RepoDid) -> (Repo, String, Vec<u8>) {
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
    (bare, c1, pack)
}

fn report_text(report: &[u8]) -> String {
    String::from_utf8_lossy(report).replace('\0', "")
}

fn dir_count(path: &Path) -> usize {
    std::fs::read_dir(path)
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0)
}

fn live_object_count(repo: &Repo) -> usize {
    let objects = repo.objects_dir();
    let loose: usize = std::fs::read_dir(&objects)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.len() == 2)
                        && entry.path().is_dir()
                })
                .map(|shard| dir_count(&shard.path()))
                .sum()
        })
        .unwrap_or(0);
    let packs = std::fs::read_dir(objects.join("pack"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "pack"))
                .count()
        })
        .unwrap_or(0);
    loose + packs
}

struct AllowPublic;

impl ReceiveGuard for AllowPublic {
    fn authorize(&self, _staged: &Repo, commands: &[ReceiveCommand]) -> Vec<RefDecision> {
        commands
            .iter()
            .map(|command| {
                if command.name().is_some_and(knot_git::is_public_ref) {
                    RefDecision::Allow
                } else {
                    RefDecision::Reject("not public ref".to_string())
                }
            })
            .collect()
    }
}

struct DenyAll;

impl ReceiveGuard for DenyAll {
    fn authorize(&self, _staged: &Repo, commands: &[ReceiveCommand]) -> Vec<RefDecision> {
        commands
            .iter()
            .map(|_| RefDecision::Reject("unauthorized".to_string()))
            .collect()
    }
}

struct AllowAll;

impl ReceiveGuard for AllowAll {
    fn authorize(&self, _staged: &Repo, commands: &[ReceiveCommand]) -> Vec<RefDecision> {
        commands.iter().map(|_| RefDecision::Allow).collect()
    }
}

struct CobVerify {
    owner: ActorId,
    home: CobHome,
}

impl ReceiveGuard for CobVerify {
    fn authorize(&self, staged: &Repo, commands: &[ReceiveCommand]) -> Vec<RefDecision> {
        commands
            .iter()
            .map(|command| {
                let store = CobStore::new(staged);
                let Ok(name) = knot_types::RefName::new(command.refname()) else {
                    return RefDecision::Reject("invalid ref name".to_string());
                };
                match knot_cobs::verify_cob_ref(&store, &self.home, &name, &self.owner) {
                    Ok(_) => RefDecision::Allow,
                    Err(error) => RefDecision::Reject(error.to_string()),
                }
            })
            .collect()
    }
}

#[test]
fn a_denied_push_leaves_no_objects_in_the_live_odb() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let (bare, c1, pack) = seeded_pack(&layout, &did);

    assert_eq!(
        live_object_count(&bare),
        0,
        "fresh bare repo has no objects"
    );
    let request = receive_request("refs/heads/main", &Oid::null().to_hex(), &c1, &pack);
    let report = receive_pack_guarded(
        &bare,
        &request,
        &limits(),
        &DenyAll,
        &|_| {},
        &knot_pack::default_catalog().reject,
    )
    .unwrap()
    .report;
    let report = report_text(&report);
    assert!(
        report.contains("ng refs/heads/main unauthorized"),
        "{report}"
    );

    assert!(
        bare.references().unwrap().is_empty(),
        "denied push mustn't create the ref"
    );
    assert_eq!(
        live_object_count(&bare),
        0,
        "denied push must leave no objects in the live odb"
    );
    assert!(!bare.contains(Oid::from_hex(&c1).unwrap()));
}

#[test]
fn an_authorized_push_migrates_objects_then_a_stale_push_is_rejected() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let (bare, c1, pack) = seeded_pack(&layout, &did);

    let report = receive_pack_guarded(
        &bare,
        &receive_request("refs/heads/main", &Oid::null().to_hex(), &c1, &pack),
        &limits(),
        &AllowPublic,
        &|_| {},
        &knot_pack::default_catalog().reject,
    )
    .unwrap()
    .report;
    let report = report_text(&report);
    assert!(report.contains("unpack ok"), "{report}");
    assert!(report.contains("ok refs/heads/main"), "{report}");
    let refs = bare.references().unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].name.as_str(), "refs/heads/main");
    assert_eq!(refs[0].target, Oid::from_hex(&c1).unwrap());
    assert!(bare.contains(Oid::from_hex(&c1).unwrap()));
    let after_first = live_object_count(&bare);

    let wrong_old = "1".repeat(40);
    let fresh = "2".repeat(40);
    let report = receive_pack_guarded(
        &bare,
        &receive_request("refs/heads/main", &wrong_old, &fresh, b""),
        &limits(),
        &AllowPublic,
        &|_| {},
        &knot_pack::default_catalog().reject,
    )
    .unwrap()
    .report;
    let report = report_text(&report);
    assert!(report.contains("ng refs/heads/main"), "{report}");
    assert_eq!(
        bare.find_ref(&knot_types::RefName::new("refs/heads/main").unwrap())
            .unwrap(),
        Some(Oid::from_hex(&c1).unwrap()),
        "stale push mustn't move the ref"
    );
    assert_eq!(
        live_object_count(&bare),
        after_first,
        "stale push mustn't add objects to the live odb"
    );
}

#[test]
fn a_cob_ref_verifies_against_the_owner_key_and_is_refused_for_a_stranger() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let signer = K256Signer::generate(&SeededEntropy::new(7));

    let source_did = RepoDid::new("did:plc:source").unwrap();
    let home = CobHome::from(&source_did);
    let source = layout.create(&source_did).unwrap();
    let store = CobStore::new(&source);
    let grant = Grant {
        subject: AccountDid::new("did:plc:nel").unwrap(),
        added_by: AccountDid::new("did:plc:nel").unwrap(),
        created_at: UnixSeconds::new(1),
    };
    let created = store
        .create(
            &home,
            &MembersChange::Add(grant),
            &signer,
            UnixSeconds::new(1),
        )
        .unwrap();
    let tip = created.tip.oid().to_hex();
    let refname = format!(
        "refs/cobs/sh.tangled.knot.member/{}",
        created.object.oid().to_hex()
    );

    let reachable: Vec<String> = must(source.path(), &["rev-list", "--objects", &tip])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect();
    let pack = pack_objects(source.path(), &reachable);
    let owner = ActorId::from_secp256k1(signer.public_key().as_bytes());

    let dest = layout
        .create(&RepoDid::new("did:plc:dest").unwrap())
        .unwrap();
    let report = receive_pack_guarded(
        &dest,
        &receive_request(&refname, &Oid::null().to_hex(), &tip, &pack),
        &limits(),
        &CobVerify {
            owner: owner.clone(),
            home: home.clone(),
        },
        &|_| {},
        &knot_pack::default_catalog().reject,
    )
    .unwrap()
    .report;
    assert!(
        report_text(&report).contains(&format!("ok {refname}")),
        "owner-signed COB ref must be accepted at the receive boundary: {}",
        report_text(&report)
    );

    let stranger_key = K256Signer::generate(&SeededEntropy::new(9));
    let stranger = ActorId::from_secp256k1(stranger_key.public_key().as_bytes());
    let dest2 = layout
        .create(&RepoDid::new("did:plc:dest2").unwrap())
        .unwrap();
    let report = receive_pack_guarded(
        &dest2,
        &receive_request(&refname, &Oid::null().to_hex(), &tip, &pack),
        &limits(),
        &CobVerify {
            owner: stranger,
            home: home.clone(),
        },
        &|_| {},
        &knot_pack::default_catalog().reject,
    )
    .unwrap()
    .report;
    assert!(
        report_text(&report).contains(&format!("ng {refname}")),
        "COB ref not signed by the resolved owner key must be refused: {}",
        report_text(&report)
    );
    assert_eq!(
        live_object_count(&dest2),
        0,
        "refused COB ref push leaves no objects behind"
    );
}

#[test]
fn a_reserved_ref_update_is_refused_even_when_the_guard_allows_it() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let bare = layout.create(&did).unwrap();

    let cob_ref = format!("refs/cobs/sh.tangled.knot.member/{}", "a".repeat(40));
    let old = "1".repeat(40);
    let new = "2".repeat(40);
    let report = receive_pack_guarded(
        &bare,
        &receive_request(&cob_ref, &old, &new, b""),
        &limits(),
        &AllowAll,
        &|_| {},
        &knot_pack::default_catalog().reject,
    )
    .unwrap()
    .report;
    let report = report_text(&report);

    assert!(
        report.contains(&format!("ng {cob_ref}")),
        "non-create update to a reserved ref must be refused even under an allow-all guard: {report}"
    );
    assert!(
        report.contains("cannot be modified"),
        "refusal must name the create-only rule, not connectivity or compare-and-swap: {report}"
    );
    assert!(
        bare.references().unwrap().is_empty(),
        "refused reserved-ref update must land nothing"
    );
    assert_eq!(
        live_object_count(&bare),
        0,
        "refused reserved-ref update must migrate no objects"
    );
}

fn pseudo_random(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

fn incompressible_pack(megabytes: usize) -> Vec<u8> {
    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path();
    must(work, &["init", "-q", "-b", "main"]);
    (0..megabytes).for_each(|i| {
        let blob = pseudo_random(i as u64 + 1, 1024 * 1024);
        std::fs::write(work.join(format!("blob-{i:04}.bin")), blob).unwrap();
    });
    must(work, &["add", "-A"]);
    must(work, &["commit", "-q", "-m", "bulk"]);
    let head = must(work, &["rev-parse", "HEAD"]);
    let oids: Vec<String> = must(work, &["rev-list", "--objects", &head])
        .lines()
        .map(|line| line.split_whitespace().next().unwrap().to_string())
        .collect();
    pack_objects(work, &oids)
}

#[test]
fn the_receive_framer_scans_incrementally_in_linear_time() {
    const READ_CHUNK: usize = 64 * 1024;
    let pack = incompressible_pack(24);
    let body = receive_request(
        "refs/heads/main",
        &Oid::null().to_hex(),
        &"1".repeat(40),
        &pack,
    );
    assert!(
        pack.len() > 8 * 1024 * 1024,
        "pack must be large enough to expose any quadratic scaling: {} bytes",
        pack.len()
    );

    let single_start = Instant::now();
    let complete = ReceiveFramer::new(limits(), SHA1.kind())
        .advance_bytes(&body)
        .unwrap();
    let single = single_start.elapsed();
    assert_eq!(
        complete,
        Some(body.len()),
        "one advance over the full body frames whole request"
    );

    let chunks = body.len().div_ceil(READ_CHUNK);
    let chunked_start = Instant::now();
    let mut framer = ReceiveFramer::new(limits(), SHA1.kind());
    let mut framed_at = None;
    (1..=chunks).for_each(|n| {
        let end = (n * READ_CHUNK).min(body.len());
        if framed_at.is_none()
            && let Some(total) = framer.advance_bytes(&body[..end]).unwrap()
        {
            framed_at = Some(total);
        }
    });
    let chunked = chunked_start.elapsed();
    assert_eq!(
        framed_at,
        Some(body.len()),
        "feeding the same body in 64KB chunks frames it at identical length"
    );

    let ratio = chunked.as_secs_f64() / single.as_secs_f64().max(1e-6);
    assert!(
        ratio < 5.0,
        "resumable framer never re-inflates settled objects, so the {chunks}-chunk feed \
         must stay within a small constant of one pass; got {ratio:.2}x"
    );
}

#[test]
fn receive_request_completion_framing_and_the_per_object_limit() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let (_bare, c1, pack) = seeded_pack(&layout, &did);
    let request = receive_request("refs/heads/main", &Oid::null().to_hex(), &c1, &pack);

    assert_eq!(
        receive_request_complete(&request[..request.len() / 2], &limits(), SHA1.kind()).unwrap(),
        None,
        "half-delivered request isn't yet complete"
    );
    assert_eq!(
        receive_request_complete(&request, &limits(), SHA1.kind()).unwrap(),
        Some(request.len()),
        "whole request reports its exact length"
    );
    let mut trailing = request.clone();
    trailing.extend_from_slice(b"junk-after-the-pack");
    assert_eq!(
        receive_request_complete(&trailing, &limits(), SHA1.kind()).unwrap(),
        Some(request.len()),
        "framer stops at the pack trailer, ignoring trailing bytes"
    );

    let tight = PackLimits {
        max_object_bytes: knot_pack::MaxObjectBytes::new(1),
        ..PackLimits::default()
    };
    assert!(
        receive_request_complete(&request, &tight, SHA1.kind()).is_err(),
        "object beyond the per-object limit is refused by the framer"
    );
}

#[test]
fn sweep_incoming_removes_abandoned_staging_directories() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let bare = layout.create(&did).unwrap();

    let staging = bare.path().join(".knot-incoming.4242.0");
    std::fs::create_dir_all(staging.join("objects")).unwrap();
    std::fs::write(staging.join("objects").join("leftover"), b"x").unwrap();
    assert!(staging.exists());

    let swept = sweep_incoming(scan.path());
    assert_eq!(swept, 1, "exactly one staging directory is swept");
    assert!(!staging.exists(), "abandoned staging directory is gone");
    assert!(
        bare.path().join("objects").exists(),
        "live repository is untouched"
    );
}

#[test]
fn streamed_receipt_reassembles_a_request_byte_for_byte() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let (_bare, c1, pack) = seeded_pack(&layout, &did);
    let request = receive_request("refs/heads/main", &Oid::null().to_hex(), &c1, &pack);

    let dir = tempfile::tempdir().unwrap();
    let mut receiver = PackReceiver::new(
        dir.path(),
        MaxWireBytes::new(1 << 30),
        limits(),
        SHA1.kind(),
    )
    .unwrap();
    let completed = request
        .chunks(7)
        .try_fold(false, |_, chunk| receiver.write(chunk))
        .unwrap();
    assert!(
        completed,
        "the framer must detect completion within the streamed request"
    );

    let body = receiver.finish().unwrap();
    let mut reconstructed = body.preamble().to_vec();
    if let Some(pack) = body.open_pack().unwrap() {
        reconstructed.extend_from_slice(&std::fs::read(pack.path()).unwrap());
    }
    assert_eq!(
        &reconstructed[..],
        &request[..],
        "the streamed body must be byte-identical to the buffered request"
    );
    assert_eq!(
        receive_request_complete(&request, &limits(), SHA1.kind()).unwrap(),
        Some(request.len()),
        "streamed total must agree with the buffered framer"
    );
}

#[test]
fn streamed_receipt_handles_empty_and_delete_only_requests() {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let (_bare, c1, _pack) = seeded_pack(&layout, &did);
    let dir = tempfile::tempdir().unwrap();

    let empty = PackReceiver::new(
        dir.path(),
        MaxWireBytes::new(1 << 30),
        limits(),
        SHA1.kind(),
    )
    .unwrap()
    .finish()
    .unwrap();
    assert!(
        empty.is_empty(),
        "a stream with no bytes yields an empty body"
    );

    let delete = receive_request("refs/heads/main", &c1, &Oid::null().to_hex(), &[]);
    let mut receiver = PackReceiver::new(
        dir.path(),
        MaxWireBytes::new(1 << 30),
        limits(),
        SHA1.kind(),
    )
    .unwrap();
    let completed = delete
        .chunks(5)
        .try_fold(false, |_, chunk| receiver.write(chunk))
        .unwrap();
    assert!(completed, "a delete-only push completes without a pack");
    let body = receiver.finish().unwrap();
    assert_eq!(body.preamble(), &delete[..]);
    assert!(
        body.open_pack().unwrap().is_none(),
        "a delete-only push has no pack"
    );
}
