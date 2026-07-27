use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use knot_git::Layout;
use knot_lfs::DiskStore;
use knot_runtime::Clock;
use knot_types::{RepoDid, UnixSeconds};
use tokio::sync::{mpsc, watch};

use crate::{LfsGrace, Options, RepackStatus, Report, SweepInterval, run_repo};

pub trait RepoSource: Send + Sync + 'static {
    fn repos(&self) -> Vec<RepoDid>;
    fn ready_repos(&self) -> Option<Vec<RepoDid>>;
}

// "Grace" means how old an unreferenced object can get before we reap it,
// and the "interval" is how often we look.
struct LfsGc {
    store: Arc<DiskStore>,
    grace: LfsGrace,
    interval: SweepInterval,
}

const TRIGGER_CAPACITY: usize = 1024;

knot_types::scalar_newtype! {
    pub struct PushBytes(u64) => ordered;
}

#[derive(Clone)]
pub struct MaintenanceHandle {
    trigger: Option<mpsc::Sender<RepoDid>>,
    large_push: PushBytes,
}

impl MaintenanceHandle {
    pub fn disabled() -> Self {
        Self {
            trigger: None,
            large_push: PushBytes::new(u64::MAX),
        }
    }

    pub fn note_push(&self, repo: &RepoDid, pack_bytes: PushBytes) {
        if pack_bytes >= self.large_push
            && let Some(trigger) = &self.trigger
        {
            let _ = trigger.try_send(repo.clone());
        }
    }
}

pub struct Scheduler<C> {
    layout: Layout,
    source: Arc<dyn RepoSource>,
    clock: C,
    options: Options,
    interval: Duration,
    triggers: mpsc::Receiver<RepoDid>,
    lfs: Option<LfsGc>,
}

impl<C: Clock> Scheduler<C> {
    pub fn new(
        layout: Layout,
        source: Arc<dyn RepoSource>,
        clock: C,
        options: Options,
        interval: Duration,
        large_push: PushBytes,
    ) -> (Self, MaintenanceHandle) {
        let (trigger, triggers) = mpsc::channel(TRIGGER_CAPACITY);
        let handle = MaintenanceHandle {
            trigger: Some(trigger),
            large_push,
        };
        let scheduler = Self {
            layout,
            source,
            clock,
            options,
            interval,
            triggers,
            lfs: None,
        };
        (scheduler, handle)
    }

    pub fn with_lfs_gc(
        mut self,
        store: Arc<DiskStore>,
        grace: LfsGrace,
        interval: SweepInterval,
    ) -> Self {
        self.lfs = Some(LfsGc {
            store,
            grace,
            interval,
        });
        self
    }

    fn now_seconds(&self) -> UnixSeconds {
        UnixSeconds::new((self.clock.now_unix_micros().get() / 1_000_000) as i64)
    }

    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_micros(self.clock.now_unix_micros().get())
    }

    async fn gc_repo(&self, repo: &RepoDid) -> knot_lfs::GcReport {
        let Some(lfs) = &self.lfs else {
            return knot_lfs::GcReport::default();
        };
        let layout = self.layout.clone();
        let store = Arc::clone(&lfs.store);
        let grace = lfs.grace.get();
        let now = self.now();
        let target = repo.clone();
        let started = std::time::Instant::now();
        let outcome = tokio::task::spawn_blocking(move || {
            layout
                .open(&target)
                .map_err(knot_lfs::GcError::from)
                .and_then(|opened| knot_lfs::collect_repo(&store, &opened, &target, grace, now))
        })
        .await;
        match outcome {
            Ok(Ok(report)) => {
                if report.swept > 0 {
                    tracing::info!(
                        repo = %repo,
                        scanned = report.scanned,
                        marked = report.marked,
                        swept = report.swept,
                        bytes = report.bytes.get(),
                        duration_ms = started.elapsed().as_millis() as u64,
                        "lfs gc reclaimed objects"
                    );
                }
                report
            }
            Ok(Err(error)) => {
                tracing::warn!(repo = %repo, %error, "lfs gc skipped, projection uncertain");
                knot_lfs::GcReport::default()
            }
            Err(join) => {
                tracing::error!(repo = %repo, %join, "lfs gc task panicked");
                knot_lfs::GcReport::default()
            }
        }
    }

    async fn sweep_orphans(&self) {
        let Some(lfs) = &self.lfs else {
            return;
        };
        let Some(hosted) = self.source.ready_repos() else {
            return;
        };
        let store = Arc::clone(&lfs.store);
        let grace = lfs.grace.get();
        let now = self.now();
        let hosted: HashSet<RepoDid> = hosted.into_iter().collect();
        let outcome =
            tokio::task::spawn_blocking(move || store.sweep_orphans(&hosted, grace, now)).await;
        match outcome {
            Ok(Ok(sweep)) if sweep.prefixes > 0 => tracing::info!(
                prefixes = sweep.prefixes,
                objects = sweep.objects,
                bytes = sweep.bytes.get(),
                "lfs gc reclaimed orphan prefixes"
            ),
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!(%error, "lfs orphan sweep failed"),
            Err(join) => tracing::error!(%join, "lfs orphan sweep task panicked"),
        }
    }

    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        let mut tick = tokio::time::interval(self.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.tick().await;
        let mut lfs_tick = self.lfs.as_ref().map(|lfs| {
            let mut lfs_tick = tokio::time::interval(lfs.interval.get());
            lfs_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            lfs_tick
        });
        if let Some(lfs_tick) = &mut lfs_tick {
            lfs_tick.tick().await;
        }
        let probe = shutdown.clone();
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                _ = tick.tick() => self.sweep_all(&probe).await,
                _ = tick_lfs(&mut lfs_tick) => self.lfs_sweep_all(&probe).await,
                Some(repo) = self.triggers.recv() => self.maintain_batch(repo).await,
            }
        }
    }

    async fn sweep_all(&self, shutdown: &watch::Receiver<bool>) {
        for repo in self.source.repos() {
            if *shutdown.borrow() {
                return;
            }
            self.maintain_one(repo).await;
        }
    }

    async fn lfs_sweep_all(&self, shutdown: &watch::Receiver<bool>) {
        if self.lfs.is_none() {
            return;
        }
        let started = std::time::Instant::now();
        let mut totals = knot_lfs::GcReport::default();
        let mut repos = 0usize;
        for repo in self.source.repos() {
            if *shutdown.borrow() {
                return;
            }
            let report = self.gc_repo(&repo).await;
            repos += 1;
            totals.scanned += report.scanned;
            totals.marked += report.marked;
            totals.swept += report.swept;
            totals.bytes = totals.bytes.saturating_add(report.bytes);
        }
        if !*shutdown.borrow() {
            self.sweep_orphans().await;
        }
        tracing::info!(
            repos,
            scanned = totals.scanned,
            marked = totals.marked,
            swept = totals.swept,
            bytes = totals.bytes.get(),
            duration_ms = started.elapsed().as_millis() as u64,
            "lfs gc pass finished"
        );
    }

    async fn maintain_batch(&mut self, first: RepoDid) {
        let pending: HashSet<RepoDid> = std::iter::once(first)
            .chain(std::iter::from_fn(|| self.triggers.try_recv().ok()))
            .collect();
        for repo in pending {
            self.maintain_one(repo.clone()).await;
            self.gc_repo(&repo).await;
        }
    }

    async fn maintain_one(&self, repo: RepoDid) {
        let layout = self.layout.clone();
        let options = self.options;
        let now_seconds = self.now_seconds();
        let target = repo.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            layout
                .open(&target)
                .map_err(crate::MaintError::from)
                .and_then(|opened| run_repo(&opened, now_seconds, &options))
        })
        .await;
        match outcome {
            Ok(Ok(report)) => report_skips(&repo, &report),
            Ok(Err(error)) => tracing::error!(repo = %repo, %error, "maintenance run failed"),
            Err(join) => tracing::error!(repo = %repo, %join, "maintenance task panicked"),
        }
    }
}

async fn tick_lfs(tick: &mut Option<tokio::time::Interval>) {
    match tick {
        Some(tick) => {
            tick.tick().await;
        }
        None => std::future::pending::<()>().await,
    }
}

fn report_skips(repo: &RepoDid, report: &Report) {
    match report.repack.status {
        RepackStatus::SkippedTooLarge => {
            tracing::warn!(
                repo = %repo,
                reason = "reachable set exceeds repack_max_objects",
                "skipped repack"
            )
        }
        RepackStatus::ClosureFailed => {
            tracing::warn!(
                repo = %repo,
                reason = "couldn't compute reachable set",
                "skipped repack and prune"
            )
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use knot_git::{Layout, RefUpdate};
    use knot_runtime::SystemClock;
    use knot_types::{BranchName, RefName, RepoDid};

    use super::{MaintenanceHandle, PushBytes, RepoSource, Scheduler};
    use crate::test_support::{commit_on, empty_tree, options};

    struct Fixed(Vec<RepoDid>);
    impl RepoSource for Fixed {
        fn repos(&self) -> Vec<RepoDid> {
            self.0.clone()
        }

        fn ready_repos(&self) -> Option<Vec<RepoDid>> {
            Some(self.0.clone())
        }
    }

    fn seed_repo(layout: &Layout, did: &RepoDid) {
        let repo = layout.create(did).unwrap();
        let tip = commit_on(&repo, empty_tree(repo.object_format()), Vec::new(), "a");
        repo.update_ref(&RefUpdate::Create {
            name: RefName::new("refs/heads/main").unwrap(),
            new: tip,
        })
        .unwrap();
    }

    #[test]
    fn note_push_fires_only_past_the_threshold() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let handle = MaintenanceHandle {
            trigger: Some(sender),
            large_push: PushBytes::new(1_000),
        };
        let did = RepoDid::new("did:plc:squid").unwrap();
        handle.note_push(&did, PushBytes::new(999));
        assert!(receiver.try_recv().is_err(), "small push is ignored");
        handle.note_push(&did, PushBytes::new(1_000));
        assert_eq!(receiver.try_recv().unwrap(), did, "large push triggers");
    }

    #[test]
    fn disabled_handle_never_triggers() {
        let did = RepoDid::new("did:plc:squid").unwrap();
        MaintenanceHandle::disabled().note_push(&did, PushBytes::new(u64::MAX));
    }

    #[tokio::test]
    async fn the_lfs_interval_collects_without_a_push_trigger() {
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        use knot_lfs::{DiskStore, LfsOid, LfsStore, LfsStorePath};
        use knot_runtime::{ManualClock, UnixMicros};
        use sha2::{Digest, Sha256};

        let scan = tempfile::tempdir().unwrap();
        let lfs_dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(scan.path()).with_default_branch(BranchName::new("main").unwrap());
        let did = RepoDid::new("did:plc:limpet").unwrap();
        seed_repo(&layout, &did);

        let store = Arc::new(DiskStore::open(LfsStorePath::new(lfs_dir.path())).unwrap());
        let body: &[u8] = b"unreferenced media reclaimed on the interval alone";
        let oid = LfsOid::from_digest(Sha256::digest(body).into());
        let size = knot_lfs::ClaimedSize::new(body.len() as u64);
        store.put(&did, &oid, size, &mut &body[..]).unwrap();

        let real_micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;
        let future = ManualClock::new(UnixMicros::new(real_micros + 5 * 86_400 * 1_000_000));

        let (scheduler, _handle) = Scheduler::new(
            layout.clone(),
            Arc::new(Fixed(vec![did.clone()])),
            future,
            options(),
            std::time::Duration::from_secs(3_600),
            PushBytes::new(1_000),
        );
        let scheduler = scheduler.with_lfs_gc(
            Arc::clone(&store),
            crate::lfs_grace(
                crate::GcGrace::from_secs(86_400),
                crate::ReflogRetention::from_secs(90 * 86_400),
            ),
            crate::SweepInterval::new(Duration::from_millis(40)),
        );
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(scheduler.run(shutdown_rx));

        let mut waited = 0;
        while store.probe(&did, &oid).unwrap().is_some() && waited < 200 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            waited += 1;
        }
        assert_eq!(
            store.probe(&did, &oid).unwrap(),
            None,
            "the lfs interval alone reclaimed the unreferenced, past-grace object"
        );

        shutdown_tx.send(true).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn a_triggered_push_collects_an_unreferenced_lfs_object() {
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        use knot_lfs::{DiskStore, LfsOid, LfsStore, LfsStorePath};
        use knot_runtime::{ManualClock, UnixMicros};
        use sha2::{Digest, Sha256};

        let scan = tempfile::tempdir().unwrap();
        let lfs_dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(scan.path()).with_default_branch(BranchName::new("main").unwrap());
        let did = RepoDid::new("did:plc:limpet").unwrap();
        seed_repo(&layout, &did);

        let store = Arc::new(DiskStore::open(LfsStorePath::new(lfs_dir.path())).unwrap());
        let body: &[u8] = b"unreferenced media the sweep should reclaim";
        let oid = LfsOid::from_digest(Sha256::digest(body).into());
        let size = knot_lfs::ClaimedSize::new(body.len() as u64);
        store.put(&did, &oid, size, &mut &body[..]).unwrap();

        let real_micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;
        let future = ManualClock::new(UnixMicros::new(real_micros + 5 * 86_400 * 1_000_000));

        let (scheduler, handle) = Scheduler::new(
            layout.clone(),
            Arc::new(Fixed(vec![did.clone()])),
            future,
            options(),
            std::time::Duration::from_secs(3_600),
            PushBytes::new(1_000),
        );
        let scheduler = scheduler.with_lfs_gc(
            Arc::clone(&store),
            crate::lfs_grace(
                crate::GcGrace::from_secs(86_400),
                crate::ReflogRetention::from_secs(90 * 86_400),
            ),
            crate::SweepInterval::new(Duration::from_secs(3_600)),
        );
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(scheduler.run(shutdown_rx));

        handle.note_push(&did, PushBytes::new(10_000));

        let mut waited = 0;
        while store.probe(&did, &oid).unwrap().is_some() && waited < 200 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            waited += 1;
        }
        assert_eq!(
            store.probe(&did, &oid).unwrap(),
            None,
            "the triggered gc reclaimed the unreferenced, past-grace object"
        );

        shutdown_tx.send(true).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn a_triggered_repo_is_maintained() {
        let scan = tempfile::tempdir().unwrap();
        let layout = Layout::new(scan.path()).with_default_branch(BranchName::new("main").unwrap());
        let did = RepoDid::new("did:plc:limpet").unwrap();
        seed_repo(&layout, &did);

        let (scheduler, handle) = Scheduler::new(
            layout.clone(),
            Arc::new(Fixed(vec![did.clone()])),
            SystemClock,
            options(),
            std::time::Duration::from_secs(3_600),
            PushBytes::new(1_000),
        );
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(scheduler.run(shutdown_rx));

        handle.note_push(&did, PushBytes::new(10_000));

        let graph = layout
            .open(&did)
            .unwrap()
            .objects_dir()
            .join("info/commit-graph");
        let mut waited = 0;
        while !graph.exists() && waited < 200 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            waited += 1;
        }
        assert!(graph.exists(), "triggered repo got commit-graph");

        shutdown_tx.send(true).unwrap();
        task.await.unwrap();
    }
}
