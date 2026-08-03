use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::future::OptionFuture;
use futures::{FutureExt, StreamExt};
use knot_atproto::{Atproto, AtprotoError};
use knot_index::{
    Coverage, HostedCoverage, Index, IndexGeneration, KeyLease, KeyRecord, KeyReprieve,
    KeyReprieved, KeyTtl, MemberWork, Pushers, Resolved, StalePushers, SuspectPushers, SweepFloor,
};
use knot_resource::{Burst, HostKey, HostPacer, RateLimit, RefillMicros, SlotPermit, Slots};
use knot_runtime::{Clock, HttpTransport};
use knot_types::{AccountDid, OfferedKey, UnixMicros, UnixSeconds};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use url::Url;

const FILL_FANOUT: usize = 8;

const PASS_HEADROOM: u32 = 2;

macro_rules! span {
    ($($name:ident from $unit:ident),+ $(,)?) => {$(
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $name(Duration);

        impl $name {
            pub const fn $unit(value: u64) -> Self {
                Self(Duration::$unit(value))
            }

            pub const fn get(self) -> Duration {
                self.0
            }
        }
    )+};
}

span!(
    BusyRetry from from_millis,
    SettleFloor from from_millis,
    StalledBackoff from from_secs,
    SettledPause from from_secs,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountBudget(usize);

impl AccountBudget {
    pub const fn new(accounts: usize) -> Self {
        Self(if accounts == 0 { 1 } else { accounts })
    }

    pub const fn get(self) -> usize {
        self.0
    }
}

#[derive(Debug, Default)]
pub struct Cursor(AtomicUsize);

impl Cursor {
    fn advance(&self, by: usize, len: usize) -> usize {
        match len {
            0 => 0,
            len => self
                .0
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |seen| {
                    Some((seen % len).wrapping_add(by) % len)
                })
                .map_or(0, |seen| seen % len),
        }
    }
}

#[derive(Debug, Default)]
pub struct Cursors {
    members: Cursor,
    suspected: Cursor,
}

fn portion(accounts: &[AccountDid], budget: AccountBudget, cursor: &Cursor) -> Vec<AccountDid> {
    let taken = budget.get().min(accounts.len());
    let start = cursor.advance(taken, accounts.len());
    accounts
        .iter()
        .cycle()
        .skip(start)
        .take(taken)
        .cloned()
        .collect()
}

#[derive(Debug, Clone, Copy)]
pub struct Pace {
    pub busy: BusyRetry,
    pub floor: SettleFloor,
    pub ttl: KeyTtl,
    pub reprieve: KeyReprieve,
    pub sweep: SweepFloor,
    pub stalled: StalledBackoff,
    pub settled: SettledPause,
    pub members: AccountBudget,
    pub suspected: AccountBudget,
    pub host: RateLimit,
}

impl Default for Pace {
    fn default() -> Self {
        Self {
            busy: BusyRetry::from_millis(50),
            floor: SettleFloor::from_millis(1_000),
            ttl: KeyTtl::DEFAULT,
            reprieve: KeyReprieve::DEFAULT,
            sweep: SweepFloor::DEFAULT,
            stalled: StalledBackoff::from_secs(30),
            settled: SettledPause::from_secs(60),
            members: AccountBudget::new(64),
            suspected: AccountBudget::new(256),
            host: RateLimit {
                burst: Burst::new(10),
                refill: RefillMicros::new(200_000),
            },
        }
    }
}

impl Pace {
    fn ttl_covering(self, accounts: usize) -> KeyTtl {
        let micros = (accounts as u64).saturating_mul(self.host.interval().get());
        let secs = (micros / 1_000_000).saturating_mul(u64::from(PASS_HEADROOM));
        self.ttl.longest(KeyTtl::from_secs(secs))
    }
}

pub fn spawn<H: HttpTransport, C: Clock>(
    index: Arc<Index>,
    atproto: Arc<Atproto<H, C>>,
    slots: Slots,
    pace: Pace,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let stop = shutdown.clone();
    let driver = Driver {
        generations: index.generations(),
        pacer: HostPacer::new(pace.host),
        cursors: Cursors::default(),
        index,
        atproto,
        slots,
        pace,
        shutdown,
    };
    tokio::spawn(async move {
        futures::stream::unfold(driver, |mut driver| async move {
            driver.pass().await;
            Some(((), driver))
        })
        .take_until(stop.cancelled_owned())
        .for_each(|()| std::future::ready(()))
        .await;
        tracing::info!("key fill stopped");
    })
}

struct Driver<H, C> {
    index: Arc<Index>,
    atproto: Arc<Atproto<H, C>>,
    slots: Slots,
    pacer: HostPacer,
    cursors: Cursors,
    pace: Pace,
    shutdown: CancellationToken,
    generations: watch::Receiver<IndexGeneration>,
}

impl<H: HttpTransport, C: Clock> Driver<H, C> {
    async fn pass(&mut self) {
        self.generations.mark_unchanged();
        let filling = fill_once(
            &self.index,
            &self.atproto,
            &self.slots,
            &self.pacer,
            self.pace,
            &self.cursors,
        );
        let pause = tokio::select! {
            pause = guarded(filling, self.pace) => pause,
            () = self.shutdown.cancelled() => self.pace.stalled.get(),
        };
        tokio::select! {
            () = self.shutdown.cancelled() => {}
            () = settle(&mut self.generations, pause, self.pace.floor) => {}
        }
    }
}

async fn guarded(pass: impl Future<Output = Duration>, pace: Pace) -> Duration {
    std::panic::AssertUnwindSafe(pass)
        .catch_unwind()
        .await
        .unwrap_or_else(|_| {
            tracing::error!("key fill pass panicked, backing off before the next pass");
            pace.stalled.get()
        })
}

async fn settle(
    generations: &mut watch::Receiver<IndexGeneration>,
    pause: Duration,
    floor: SettleFloor,
) {
    let held = floor.get().min(pause);
    tokio::time::sleep(held).await;
    let _ = tokio::time::timeout(pause.saturating_sub(held), generations.changed()).await;
}

struct Pass<'a, H, C> {
    index: &'a Arc<Index>,
    atproto: &'a Arc<Atproto<H, C>>,
    slots: &'a Slots,
    pacer: &'a HostPacer,
    pace: Pace,
    now: UnixSeconds,
    lease: KeyLease,
    reprieve: KeyReprieve,
}

pub async fn fill_once<H: HttpTransport, C: Clock>(
    index: &Arc<Index>,
    atproto: &Arc<Atproto<H, C>>,
    slots: &Slots,
    pacer: &HostPacer,
    pace: Pace,
    cursors: &Cursors,
) -> Duration {
    fold_hosted(index).await;
    let now = atproto.now().seconds();
    let Resolved::Ready(work) = index.keys().work(now, pace.sweep) else {
        index.keys().mark_warming();
        return pace.stalled.get();
    };
    if let HostedCoverage::Partial { unread } = work.hosted {
        tracing::debug!(
            repos = unread,
            "partial grant set, a repo was registered while this pass was working out who may push"
        );
    }
    let members = match (work.hosted, work.members) {
        (HostedCoverage::Whole, Resolved::Ready(members)) => {
            index.keys().retain(&members.kept);
            Some(members)
        }
        (HostedCoverage::Partial { .. }, Resolved::Ready(members)) => Some(members),
        (_, Resolved::Warming) => None,
    };
    let ttl = pace.ttl_covering(work.tracked);
    if ttl != pace.ttl {
        tracing::debug!(
            tracked = work.tracked,
            ttl_secs = ttl.get().as_secs(),
            "one paced pass over the grant set outruns the key ttl, so the fill stretches it"
        );
    }
    let pass = Pass {
        index,
        atproto,
        slots,
        pacer,
        pace,
        now,
        lease: ttl.lease_from(now),
        reprieve: pace.reprieve.budgeted_for(ttl),
    };
    let pause = fill_pushers(
        &pass,
        PushWork {
            generation: work.generation,
            hosted: work.hosted,
            pushers: work.pushers,
            due: work.due,
            suspected: work.suspected,
            complete: work.complete,
        },
        &cursors.suspected,
    )
    .await;
    OptionFuture::from(members.map(|work| fill_members(&pass, work, &cursors.members))).await;
    pause
}

struct PushWork {
    generation: IndexGeneration,
    hosted: HostedCoverage,
    pushers: Pushers,
    due: StalePushers,
    suspected: SuspectPushers,
    complete: bool,
}

async fn fold_hosted(index: &Arc<Index>) {
    let index = Arc::clone(index);
    match tokio::task::spawn_blocking(move || index.warm_collaborators()).await {
        Ok(0) | Err(_) => {}
        Ok(unreadable) => tracing::warn!(
            repos = unreadable,
            "the knot hosts registered repos it can't open, so it won't grant anybody through \
             them until an operator restores or deregisters each repo"
        ),
    }
}

async fn fill_pushers<H: HttpTransport, C: Clock>(
    pass: &Pass<'_, H, C>,
    work: PushWork,
    cursor: &Cursor,
) -> Duration {
    let PushWork {
        generation,
        hosted,
        pushers,
        due,
        suspected,
        complete,
    } = work;
    if !complete {
        pass.index.keys().mark_warming();
    }
    let wanted = due.len();
    let recorded = match wanted {
        0 => 0,
        _ => {
            let recorded = record_each(pass, due.into_vec()).await;
            tracing::debug!(wanted, recorded, "pusher key fill pass");
            recorded
        }
    };
    recheck_suspected(pass, &suspected, cursor).await;
    match recorded < wanted {
        true => pass.pace.stalled.get(),
        false => claim_ready(pass, generation, hosted, &pushers),
    }
}

async fn recheck_suspected<H: HttpTransport, C: Clock>(
    pass: &Pass<'_, H, C>,
    suspected: &SuspectPushers,
    cursor: &Cursor,
) {
    if suspected.is_empty() {
        return;
    }
    let batch = portion(suspected.as_slice(), pass.pace.suspected, cursor);
    let wanted = batch.len();
    let recorded = record_each(pass, batch).await;
    tracing::debug!(
        wanted,
        recorded,
        deferred = suspected.len().saturating_sub(wanted),
        "pusher key recheck pass, a client offered a key the accounts on file don't publish"
    );
}

fn claim_ready<H: HttpTransport, C: Clock>(
    pass: &Pass<'_, H, C>,
    generation: IndexGeneration,
    hosted: HostedCoverage,
    pushers: &Pushers,
) -> Duration {
    let keys = pass.index.keys();
    let settled =
        hosted == HostedCoverage::Whole && keys.all_live(pushers, pass.atproto.now().seconds());
    if !settled {
        return pass.pace.stalled.get();
    }
    keys.mark_ready(generation);
    match keys.coverage() {
        Coverage::Ready => pass.pace.settled.get(),
        Coverage::Warming => pass.pace.stalled.get(),
    }
}

async fn fill_members<H: HttpTransport, C: Clock>(
    pass: &Pass<'_, H, C>,
    work: MemberWork,
    cursor: &Cursor,
) {
    if !work.unread.is_empty() {
        let wanted = work.unread.len();
        let recorded = record_each(pass, work.unread.into_vec()).await;
        tracing::debug!(wanted, recorded, "member key first-read pass");
    }
    if work.due.is_empty() {
        return;
    }
    let batch = portion(work.due.as_slice(), pass.pace.members, cursor);
    let wanted = batch.len();
    let recorded = record_each(pass, batch).await;
    tracing::debug!(
        wanted,
        recorded,
        deferred = work.due.len().saturating_sub(wanted),
        "member key renewal pass"
    );
}

async fn record_each<H: HttpTransport, C: Clock>(
    pass: &Pass<'_, H, C>,
    stale: Vec<AccountDid>,
) -> usize {
    futures::stream::iter(stale)
        .map(|did| async move {
            match published_keys(pass, &did).await {
                Ok(keys) => usize::from(record(pass, &did, keys)),
                Err(error) if error.is_gone() => {
                    tracing::debug!(
                        did = did.as_str(),
                        %error,
                        "key fill records an empty key set for an account whose DID document is gone"
                    );
                    usize::from(record(pass, &did, Vec::new()))
                }
                Err(error) => reprieve(pass, &did, error),
            }
        })
        .buffer_unordered(FILL_FANOUT)
        .fold(0, |total, recorded| async move { total + recorded })
        .await
}

fn reprieve<H, C>(pass: &Pass<'_, H, C>, did: &AccountDid, error: AtprotoError) -> usize {
    let outcome = pass
        .index
        .keys()
        .reprieve(did, pass.now, pass.reprieve, pass.lease);
    match outcome {
        KeyReprieved::Exhausted => tracing::warn!(
            did = did.as_str(),
            %error,
            "the knot spent the whole reprieve failing to read an account, so it records an \
             empty key set for the account until a later read succeeds"
        ),
        KeyReprieved::Extended | KeyReprieved::Pending => tracing::debug!(
            did = did.as_str(),
            %error,
            ?outcome,
            "key fill couldn't read an account's records"
        ),
    }
    match outcome {
        KeyReprieved::Extended | KeyReprieved::Exhausted => 1,
        KeyReprieved::Pending => 0,
    }
}

fn record<H, C>(pass: &Pass<'_, H, C>, did: &AccountDid, keys: Vec<OfferedKey>) -> bool {
    match pass.index.keys().record(did, keys, pass.lease) {
        KeyRecord::Stored => true,
        KeyRecord::Unheld => {
            tracing::warn!(
                did = did.as_str(),
                "the key set is full, so the knot will check this account's pushes against its \
                 PDS every time instead of against the set"
            );
            true
        }
        KeyRecord::Saturated => {
            tracing::warn!(
                did = did.as_str(),
                "the key budget can't record even that it read this account, so the knot keeps \
                 reporting the set incomplete and defers every offered key to the push check"
            );
            false
        }
    }
}

async fn published_keys<H: HttpTransport, C: Clock>(
    pass: &Pass<'_, H, C>,
    did: &AccountDid,
) -> Result<Vec<OfferedKey>, AtprotoError> {
    let atproto = pass.atproto;
    if let Ok(document) = atproto.document_url(did) {
        wait_for_turn(pass.pacer, &document, atproto.now()).await;
    }
    let identity = {
        let _permit = idle_permit(pass.slots, pass.pace).await;
        atproto.resolve_identity(did).await?
    };
    wait_for_turn(pass.pacer, identity.pds.url(), atproto.now()).await;
    let _permit = idle_permit(pass.slots, pass.pace).await;
    atproto.pubkeys_at(&identity, did).await
}

async fn wait_for_turn(pacer: &HostPacer, url: &Url, now: UnixMicros) {
    if let Some(host) = url.host_str().map(HostKey::new) {
        tokio::time::sleep(pacer.reserve(&host, now)).await;
    }
}

async fn idle_permit(slots: &Slots, pace: Pace) -> SlotPermit {
    let attempts = futures::stream::repeat(()).filter_map(|()| async {
        match slots.resolve.try_acquire() {
            Some(permit) => Some(permit),
            None => {
                tokio::time::sleep(pace.busy.get()).await;
                None
            }
        }
    });
    futures::pin_mut!(attempts);
    attempts
        .next()
        .await
        .expect("an endless stream of attempts yields a permit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grant_set_the_pacer_cant_reread_within_the_ttl_stretches_it() {
        let pace = Pace::default();
        assert_eq!(
            pace.ttl_covering(1_000),
            KeyTtl::DEFAULT,
            "a set one paced pass covers well inside the ttl keeps the ttl it was configured with"
        );
        assert_eq!(
            pace.ttl_covering(100_000),
            KeyTtl::from_secs(40_000),
            "at one directory turn per 200ms a hundred thousand accounts take 20_000s to reread, \
             so the entries the pass wrote first must outlive the pass that writes the last"
        );
    }

    #[test]
    fn the_reprieve_budget_never_undercuts_the_ttl_the_fill_is_working_to() {
        let stretched = KeyTtl::from_secs(40_000);
        assert_eq!(
            KeyReprieve::DEFAULT.budgeted_for(stretched),
            KeyReprieve::from_secs(300, 40_000),
            "an account would be released while the pass that would reread it is still running, \
             if the budget stayed under the ttl"
        );
        assert_eq!(
            KeyReprieve::DEFAULT.budgeted_for(KeyTtl::DEFAULT),
            KeyReprieve::DEFAULT,
            "a ttl the budget already covers leaves the budget alone"
        );
    }

    #[test]
    fn the_member_cursor_turns_over_without_running_past_its_type() {
        let cursor = Cursor(AtomicUsize::new(usize::MAX));
        assert_eq!(
            cursor.advance(3, 4),
            usize::MAX % 4,
            "a cursor at the end of its range wraps instead of overflowing"
        );
        assert_eq!(cursor.advance(3, 4), (usize::MAX % 4 + 3) % 4);
        assert_eq!(
            cursor.advance(3, 4),
            (usize::MAX % 4 + 6) % 4,
            "every pass after the wrap steps by its budget, so one member can't keep the front \
             of the queue"
        );
    }

    #[test]
    fn a_set_larger_than_its_budget_comes_round_in_turns_that_cover_everybody() {
        let accounts: Vec<AccountDid> = ["nel", "olaren", "teq", "bailey", "cuttle"]
            .iter()
            .map(|name| AccountDid::new(format!("did:plc:{name}")).unwrap())
            .collect();
        let cursor = Cursor::default();
        let budget = AccountBudget::new(2);
        let turns: Vec<Vec<String>> = (0..3)
            .map(|_| {
                portion(&accounts, budget, &cursor)
                    .iter()
                    .map(|did| did.as_str().to_string())
                    .collect()
            })
            .collect();
        assert_eq!(
            turns,
            vec![
                vec!["did:plc:nel", "did:plc:olaren"],
                vec!["did:plc:teq", "did:plc:bailey"],
                vec!["did:plc:cuttle", "did:plc:nel"],
            ],
            "a stranger offering an unrecognized key mustn't cost the knot a read of every \
             account it grants. The accounts it defers must come round on later passes"
        );
    }

    #[test]
    fn a_set_the_budget_covers_is_read_whole_without_repeats() {
        let accounts: Vec<AccountDid> = ["nel", "olaren"]
            .iter()
            .map(|name| AccountDid::new(format!("did:plc:{name}")).unwrap())
            .collect();
        let cursor = Cursor::default();
        assert_eq!(
            portion(&accounts, AccountBudget::new(256), &cursor),
            accounts,
            "a set that fits inside one budget must behave as though there were no budget"
        );
        assert!(
            portion(&[], AccountBudget::new(256), &cursor).is_empty(),
            "an empty set must yield an empty portion, or the cycle turns forever"
        );
    }
}
