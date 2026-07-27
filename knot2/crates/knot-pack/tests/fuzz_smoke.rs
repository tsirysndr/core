use knot_git::Layout;
use knot_types::RepoDid;
use proptest::prelude::*;
use proptest::test_runner::TestRunner;

fn empty_repo() -> (knot_git::Repo, tempfile::TempDir) {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    let repo = layout.create(&did).unwrap();
    (repo, scan)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn parsers_never_panic(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
        knot_pack::fuzz::pkt(&data);
        knot_pack::fuzz::pack(&data);
        knot_pack::fuzz::receive_commands(&data);
        knot_pack::fuzz::upload_args(&data);
    }
}

#[test]
fn a_version3_pack_header_is_a_typed_rejection_not_a_panic() {
    let header = b"PACK\x00\x00\x00\x03\x00\x00\x00\x00";
    assert!(
        knot_pack::meter_pack(
            header,
            &knot_pack::PackLimits::default(),
            gix_hash::Kind::Sha1
        )
        .is_err(),
        "a v3 pack header must be declined with a typed error"
    );
    knot_pack::fuzz::pack(header);
}

#[test]
fn a_lying_decompressed_size_never_preallocates_the_declared_amount() {
    let bomb = [
        0x50, 0x41, 0x43, 0x4b, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x20, 0xff, 0xa0, 0xa8,
        0xa8, 0xed, 0xff, 0xff, 0x54, 0x41, 0x43, 0xff, 0xf4, 0x38, 0x06, 0x3e, 0xff, 0xff, 0xff,
        0xc7, 0x00, 0xc7, 0xff, 0xff, 0xff, 0xff, 0x3e, 0xff, 0x3e,
    ];
    assert!(
        knot_pack::meter_pack(
            &bomb,
            &knot_pack::PackLimits::default(),
            gix_hash::Kind::Sha1
        )
        .is_err(),
        "an entry declaring a petabyte object must be a typed error, never an allocation"
    );
    knot_pack::fuzz::pack(&bomb);
}

#[test]
fn repo_entry_points_never_panic() {
    let (repo, _scan) = empty_repo();
    let mut runner = TestRunner::default();
    runner
        .run(&proptest::collection::vec(any::<u8>(), 0..8192), |data| {
            let _ = knot_pack::upload_pack(&repo, &data);
            let _ = knot_pack::receive_pack(&repo, &data);
            let _ = knot_pack::meter_pack(
                &data,
                &knot_pack::PackLimits::default(),
                repo.object_format().kind(),
            );
            Ok(())
        })
        .unwrap();
}
