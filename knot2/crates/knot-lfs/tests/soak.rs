mod common;

use std::io::Write;

use common::{
    GROWTH_SLACK, PEAK_CEILING, download_script, incompressible, oid_of, repo, rss_bytes,
    upload_script,
};
use knot_lfs::{
    ClaimedSize, DiskStore, FreeSpaceFloor, LfsOid, LfsSize, LfsStore, LfsStorePath,
    StoreAdmission, TransferOp, serve_transfer,
};

const OBJECT_BYTES: usize = 8 * 1024 * 1024;
const SEEDS: usize = 4;
const WRITERS: u64 = 4;
const READERS: usize = 4;
const ROUNDS: u64 = 3;

struct CountingSink(u64);

impl Write for CountingSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn sustained_concurrent_transfers_stay_bounded_and_leak_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = DiskStore::open(LfsStorePath::new(dir.path())).unwrap();
    let admission = StoreAdmission::new(
        LfsStorePath::new(dir.path()),
        LfsSize::new(u64::MAX),
        FreeSpaceFloor::new(0),
    );

    let seeded: Vec<LfsOid> = (0..SEEDS)
        .map(|seed| {
            let body = incompressible(OBJECT_BYTES, 0x5eed_0000 + seed as u64);
            let oid = oid_of(&body);
            store
                .put(
                    &repo(),
                    &oid,
                    ClaimedSize::new(body.len() as u64),
                    &mut &body[..],
                )
                .unwrap();
            oid
        })
        .collect();

    let storm = |round: u64| {
        std::thread::scope(|scope| {
            let writers: Vec<_> = (0..WRITERS)
                .map(|writer| {
                    let store = &store;
                    let admission = &admission;
                    scope.spawn(move || {
                        let body =
                            incompressible(OBJECT_BYTES, 0xfeed_0000 + round * WRITERS + writer);
                        let (oid, script) = upload_script(&body);
                        let mut out = Vec::new();
                        serve_transfer(
                            store,
                            admission,
                            &repo(),
                            TransferOp::Upload,
                            &knot_messages::default_catalog().lfs,
                            &script[..],
                            &mut out,
                        )
                        .unwrap();
                        let replies = String::from_utf8_lossy(&out);
                        assert!(
                            !replies.contains("status 4") && !replies.contains("status 5"),
                            "round {round} writer {writer}: upload session failed:\n{replies}"
                        );
                        oid
                    })
                })
                .collect();
            let readers: Vec<_> = (0..READERS)
                .map(|reader| {
                    let store = &store;
                    let admission = &admission;
                    let oid = seeded[reader % SEEDS].clone();
                    scope.spawn(move || {
                        let script = download_script(&oid);
                        let mut sink = CountingSink(0);
                        serve_transfer(
                            store,
                            admission,
                            &repo(),
                            TransferOp::Download,
                            &knot_messages::default_catalog().lfs,
                            &script[..],
                            &mut sink,
                        )
                        .unwrap();
                        assert!(
                            sink.0 >= OBJECT_BYTES as u64,
                            "round {round} reader {reader}: streamed {} bytes",
                            sink.0
                        );
                    })
                })
                .collect();
            let uploaded: Vec<LfsOid> = writers
                .into_iter()
                .map(|writer| writer.join().unwrap())
                .collect();
            readers.into_iter().for_each(|reader| {
                reader.join().unwrap();
            });
            uploaded
        })
    };

    let mut uploaded = storm(0);
    let settled = rss_bytes();

    let peaks: Vec<u64> = (1..ROUNDS)
        .map(|round| {
            uploaded.extend(storm(round));
            rss_bytes()
        })
        .collect();

    let peak = peaks.iter().copied().max().unwrap_or(settled);
    assert!(
        peak < PEAK_CEILING,
        "concurrent transfers peaked at {peak} bytes, ceiling {PEAK_CEILING}"
    );
    let last = *peaks.last().unwrap_or(&settled);
    assert!(
        last <= settled + GROWTH_SLACK,
        "rss grew from {settled} to {last} across rounds, transfers are leaking"
    );

    uploaded.iter().chain(seeded.iter()).for_each(|oid| {
        assert!(
            store.probe(&repo(), oid).unwrap().is_some(),
            "object {oid} must be readable after the concurrent rounds"
        );
    });
    let leftover: Vec<_> = std::fs::read_dir(dir.path().join(".incoming"))
        .unwrap()
        .collect();
    assert!(
        leftover.is_empty(),
        "temporary upload files leaked: {leftover:?}"
    );
}
