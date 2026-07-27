use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

mod common;

use common::{oid_of, repo};
use knot_lfs::{ClaimedSize, DiskStore, LfsOid, LfsSize, LfsStore, LfsStorePath, Reclaimed};

const ROUNDS: usize = 400;
const GRACE: Duration = Duration::from_secs(14 * 86_400);
const BACKDATE: Duration = Duration::from_secs(60 * 86_400);

#[derive(Clone, Copy)]
enum Bias {
    TouchFirst,
    SweepFirst,
    Simultaneous,
}

impl Bias {
    fn of_round(round: usize) -> Self {
        match round % 3 {
            0 => Self::TouchFirst,
            1 => Self::SweepFirst,
            _ => Self::Simultaneous,
        }
    }
}

fn seed_expired(store: &DiskStore, round: usize) -> LfsOid {
    let body = format!("past-grace media, round {round}").into_bytes();
    let oid = oid_of(&body);
    store
        .put(
            &repo(),
            &oid,
            ClaimedSize::new(body.len() as u64),
            &mut &body[..],
        )
        .unwrap();
    let path = store.object_file(&repo(), &oid).unwrap().unwrap().1;
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - BACKDATE)
        .unwrap();
    oid
}

#[test]
fn a_mention_concurrent_with_the_sweep_never_yields_a_dangling_pointer() {
    let dir = tempfile::tempdir().unwrap();
    let store = DiskStore::open(LfsStorePath::new(dir.path())).unwrap();

    let outcomes: Vec<(Option<LfsSize>, Reclaimed)> = (0..ROUNDS)
        .map(|round| {
            let oid = seed_expired(&store, round);
            let bias = Bias::of_round(round);
            let go = AtomicBool::new(false);
            let touch_started = AtomicBool::new(false);
            let sweep_started = AtomicBool::new(false);
            let (vouched, reclaimed) = std::thread::scope(|scope| {
                let toucher = scope.spawn(|| {
                    while !go.load(Ordering::Acquire) {
                        std::hint::spin_loop();
                    }
                    if matches!(bias, Bias::SweepFirst) {
                        while !sweep_started.load(Ordering::Acquire) {
                            std::hint::spin_loop();
                        }
                    }
                    touch_started.store(true, Ordering::Release);
                    store.touch(&repo(), &oid).unwrap()
                });
                let sweeper = scope.spawn(|| {
                    while !go.load(Ordering::Acquire) {
                        std::hint::spin_loop();
                    }
                    if matches!(bias, Bias::TouchFirst) {
                        while !touch_started.load(Ordering::Acquire) {
                            std::hint::spin_loop();
                        }
                    }
                    sweep_started.store(true, Ordering::Release);
                    store
                        .collect_expired(&repo(), &oid, GRACE, SystemTime::now())
                        .unwrap()
                });
                go.store(true, Ordering::Release);
                (toucher.join().unwrap(), sweeper.join().unwrap())
            });

            if let Some(size) = vouched {
                assert!(
                    matches!(reclaimed, Reclaimed::Spared),
                    "round {round}: the sweep deleted an object the server just reported stored"
                );
                assert_eq!(
                    store.probe(&repo(), &oid).unwrap(),
                    Some(size),
                    "round {round}: an object reported stored must remain readable"
                );
            } else {
                assert!(
                    matches!(reclaimed, Reclaimed::Swept(_)),
                    "round {round}: a touch that reports missing means the sweeper unlinked first"
                );
                assert_eq!(
                    store.probe(&repo(), &oid).unwrap(),
                    None,
                    "round {round}: a swept object reports missing"
                );
            }
            (vouched, reclaimed)
        })
        .collect();

    assert!(
        outcomes.iter().any(|(vouched, _)| vouched.is_some()),
        "some round must complete the touch first, or the interleaving never varied"
    );
    assert!(
        outcomes.iter().any(|(vouched, _)| vouched.is_none()),
        "some round must complete the sweep first, or the interleaving never varied"
    );
}
