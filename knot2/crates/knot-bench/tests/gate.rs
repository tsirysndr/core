#![cfg(feature = "instrument")]

use knot_bench::{ChurnCount, CommitCount, HistorySpec, PathCount, build_history};
use knot_git::instrument::measure;
use knot_git::{Filter, Haves, PackBudget, Wants};
use knot_pack::upload_pack;
use knot_types::Oid;

fn gate_spec() -> HistorySpec {
    HistorySpec {
        commits: CommitCount::new(32),
        paths: PathCount::new(64),
        churn: ChurnCount::new(4),
    }
}

const SELECTION_ODB_READS: u64 = 129;
const SERVER_FETCH_ODB_READS: u64 = 2;

fn pkt(payload: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
    out.extend_from_slice(payload);
    out
}

fn fetch_request(want: Oid) -> Vec<u8> {
    let mut request = pkt(b"command=fetch\n");
    request.extend_from_slice(b"0001");
    request.extend(pkt(format!("want {}\n", want.to_hex()).as_bytes()));
    request.extend(pkt(b"done\n"));
    request.extend_from_slice(b"0000");
    request
}

#[test]
fn a_single_selection_walk_has_an_exact_odb_read_count() {
    let history = build_history(gate_spec());
    let tips = history.tips();
    let (_selection, reads) = measure(|| {
        history
            .repo()
            .select_pack_objects_filtered(
                Wants::new(&tips),
                Haves::new(&[]),
                Filter::None,
                PackBudget::unbounded(),
            )
            .unwrap()
    });
    assert_eq!(
        reads.get(),
        SELECTION_ODB_READS,
        "the selection walk made {} explicit object loads through Repo::load_object. \
         The gate pins this at {SELECTION_ODB_READS}. A lower count means the single-pass \
         commit-walk fix landed. A higher count is a regression. The counter records loads \
         on the calling thread only. gix's internal rev-walk decodes never reach load_object \
         and stay uncounted. Update the constant only when the change is deliberate",
        reads.get()
    );
}

#[test]
fn the_upload_pack_server_path_has_an_exact_odb_read_count() {
    let history = build_history(gate_spec());
    let tips = history.tips();
    let walk = history
        .repo()
        .rev_walk(Wants::new(&tips), Haves::new(&[]))
        .unwrap();
    let hidden = walk
        .iter()
        .copied()
        .find(|commit| *commit != history.tip())
        .expect("multi-commit history has a non-tip commit to want");
    let request = fetch_request(hidden);
    let (_response, reads) = measure(|| upload_pack(history.repo(), &request).unwrap());
    assert_eq!(
        reads.get(),
        SERVER_FETCH_ODB_READS,
        "server fetch made {} Repo::load_object calls, gate pins {SERVER_FETCH_ODB_READS}. \
         No-haves fetch enumerates inside gix, so only the want check and root peel reach \
         load_object. A count near the old manual-walk figure means the full-clone fast path \
         stopped firing",
        reads.get()
    );
}
