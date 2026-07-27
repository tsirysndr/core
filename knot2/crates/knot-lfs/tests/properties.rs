mod common;

use common::{oid_of, repo};
use knot_lfs::{
    ClaimedSize, DiskStore, LfsError, LfsOid, LfsSize, LfsStore, LfsStorePath, MemoryStore,
};
use proptest::prelude::*;

fn round_trip(store: &dyn LfsStore, body: &[u8]) -> Result<(), TestCaseError> {
    let repo = repo();
    let oid = oid_of(body);
    let bytes = body.len() as u64;
    store
        .put(&repo, &oid, ClaimedSize::new(bytes), &mut &body[..])
        .expect("put with matching size and oid succeeds");
    prop_assert_eq!(
        store.probe(&repo, &oid).expect("probe"),
        Some(LfsSize::new(bytes))
    );
    let mut out = Vec::new();
    store
        .read(&repo, &oid)
        .expect("stored object opens")
        .read_to_end(&mut out)
        .expect("stored object reads");
    prop_assert_eq!(out, body);
    Ok(())
}

#[derive(Debug, Clone)]
enum Tamper {
    Flip { at: usize, xor: u8 },
    Truncate { keep: usize },
    Extend { extra: Vec<u8> },
}

fn tampered(body: &[u8], tamper: &Tamper) -> Vec<u8> {
    match tamper {
        Tamper::Flip { at, xor } => {
            let mut bytes = body.to_vec();
            bytes[at % body.len()] ^= xor;
            bytes
        }
        Tamper::Truncate { keep } => body[..keep % body.len()].to_vec(),
        Tamper::Extend { extra } => [body, extra].concat(),
    }
}

fn tamper_strategy() -> impl Strategy<Value = Tamper> {
    prop_oneof![
        (any::<usize>(), 1u8..).prop_map(|(at, xor)| Tamper::Flip { at, xor }),
        any::<usize>().prop_map(|keep| Tamper::Truncate { keep }),
        proptest::collection::vec(any::<u8>(), 1..64).prop_map(|extra| Tamper::Extend { extra }),
    ]
}

fn rejects_tampering(
    store: &dyn LfsStore,
    body: &[u8],
    tamper: &Tamper,
) -> Result<(), TestCaseError> {
    let repo = repo();
    let oid = oid_of(body);
    let size = ClaimedSize::new(body.len() as u64);
    let forged = tampered(body, tamper);
    let verdict = store.put(&repo, &oid, size, &mut &forged[..]);
    prop_assert!(
        matches!(
            verdict,
            Err(LfsError::HashMismatch { .. } | LfsError::SizeMismatch { .. })
        ),
        "a tampered body must fail the verifier, got {verdict:?}"
    );
    prop_assert_eq!(store.probe(&repo, &oid).expect("probe"), None);
    Ok(())
}

fn disk_store() -> (DiskStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = DiskStore::open(LfsStorePath::new(dir.path())).expect("store opens");
    (store, dir)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn any_body_round_trips_through_both_stores(
        body in proptest::collection::vec(any::<u8>(), 0..4096)
    ) {
        round_trip(&MemoryStore::new(), &body)?;
        let (store, _dir) = disk_store();
        round_trip(&store, &body)?;
    }

    #[test]
    fn the_verifier_rejects_any_tampered_or_truncated_body(
        body in proptest::collection::vec(any::<u8>(), 1..2048),
        tamper in tamper_strategy(),
    ) {
        prop_assume!(!matches!(&tamper, Tamper::Flip { xor: 0, .. }));
        rejects_tampering(&MemoryStore::new(), &body, &tamper)?;
        let (store, _dir) = disk_store();
        rejects_tampering(&store, &body, &tamper)?;
    }

    #[test]
    fn a_pointer_file_round_trips(digest in any::<[u8; 32]>(), size in any::<u64>()) {
        let oid = LfsOid::from_digest(digest);
        let text = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {size}\n"
        );
        let parsed = knot_lfs::parse_pointer(text.as_bytes()).expect("a spec pointer parses");
        prop_assert_eq!(parsed.oid, oid);
        prop_assert_eq!(parsed.size, ClaimedSize::new(size));
    }
}
