use std::collections::{BTreeSet, HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use knot_runtime::{Clock, UnixMicros};
use knot_types::{
    AccountDid, ChangedFiles, Email, LanguageBytes, LanguageName, ObjectFormat, Oid, OwnerDid,
    PushOptions, RefName, RefTransition, RepoDid, RepoPath, Tid,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct EventCursor(i64);

impl EventCursor {
    pub const START: Self = Self(0);

    pub fn new(nanos: i64) -> Self {
        Self(nanos)
    }

    pub fn get(self) -> i64 {
        self.0
    }

    fn from_unix_micros(micros: UnixMicros) -> Self {
        Self((micros.get() as i64).saturating_mul(1_000))
    }
}

pub trait Publish: Serialize {
    const NSID: &'static str;
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub rkey: Tid,
    pub nsid: &'static str,
    #[serde(rename = "event")]
    pub payload: Value,
    pub created: EventCursor,
}

// `sh.tangled.git.refUpdate` requires ref, oldSha and newSha,
// so a record about no ref at all sends "" for the three rather than `null`.
#[derive(Debug, Clone)]
enum RefChange {
    Absent,
    Applied {
        ref_name: RefName,
        old_sha: Oid,
        new_sha: Oid,
    },
}

impl Serialize for RefChange {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(3))?;
        match self {
            Self::Absent => {
                map.serialize_entry("ref", "")?;
                map.serialize_entry("oldSha", "")?;
                map.serialize_entry("newSha", "")?;
            }
            Self::Applied {
                ref_name,
                old_sha,
                new_sha,
            } => {
                map.serialize_entry("ref", ref_name.as_str())?;
                map.serialize_entry("oldSha", &old_sha.to_hex())?;
                map.serialize_entry("newSha", &new_sha.to_hex())?;
            }
        }
        map.end()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GitRefUpdate {
    #[serde(rename = "$type")]
    record_type: &'static str,
    #[serde(rename = "changedFiles", skip_serializing_if = "Vec::is_empty")]
    changed_files: Vec<RepoPath>,
    #[serde(rename = "committerDid")]
    committer_did: AccountDid,
    meta: Option<RefUpdateMeta>,
    #[serde(rename = "ownerDid", skip_serializing_if = "Option::is_none")]
    owner_did: Option<OwnerDid>,
    #[serde(rename = "pushOptions", skip_serializing_if = "PushOptions::is_empty")]
    push_options: PushOptions,
    #[serde(flatten)]
    change: RefChange,
    repo: RepoDid,
}

impl GitRefUpdate {
    pub fn new(repo: RepoDid, owner: Option<OwnerDid>, committer: AccountDid) -> Self {
        Self {
            record_type: Self::NSID,
            changed_files: Vec::new(),
            committer_did: committer,
            meta: None,
            owner_did: owner,
            push_options: PushOptions::default(),
            change: RefChange::Absent,
            repo,
        }
    }

    pub fn on_ref(
        mut self,
        ref_name: RefName,
        transition: RefTransition,
        format: ObjectFormat,
    ) -> Self {
        self.change = RefChange::Applied {
            ref_name,
            old_sha: transition.old_oid().unwrap_or_else(|| format.null_oid()),
            new_sha: transition.new_oid().unwrap_or_else(|| format.null_oid()),
        };
        self
    }

    pub fn with_changed_files(mut self, changed: ChangedFiles) -> Self {
        self.changed_files = changed.into_paths();
        self
    }

    pub fn with_push_options(mut self, options: &PushOptions) -> Self {
        self.push_options = options.clone();
        self
    }

    pub fn with_meta(mut self, meta: RefUpdateMeta) -> Self {
        self.meta = Some(meta);
        self
    }
}

impl Publish for GitRefUpdate {
    const NSID: &'static str = "sh.tangled.git.refUpdate";
}

#[derive(Debug, Clone, Serialize)]
pub struct RefUpdateMeta {
    #[serde(rename = "isDefaultRef")]
    is_default_ref: bool,
    #[serde(rename = "commitCount")]
    commit_count: CommitCountBreakdown,
    #[serde(rename = "langBreakdown", skip_serializing_if = "Option::is_none")]
    lang_breakdown: Option<LangBreakdown>,
}

impl RefUpdateMeta {
    pub fn new(
        is_default_ref: bool,
        by_email: Vec<EmailCommitCount>,
        languages: Vec<LanguageSize>,
    ) -> Self {
        Self {
            is_default_ref,
            commit_count: CommitCountBreakdown {
                by_email: (!by_email.is_empty()).then_some(by_email),
            },
            lang_breakdown: (!languages.is_empty()).then_some(LangBreakdown {
                inputs: Some(languages),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct CommitCountBreakdown {
    #[serde(rename = "byEmail", skip_serializing_if = "Option::is_none")]
    by_email: Option<Vec<EmailCommitCount>>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct CommitCount(u64);

impl CommitCount {
    pub const fn new(count: u64) -> Self {
        Self(count)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn succ(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EmailCommitCount {
    email: Email,
    count: CommitCount,
}

impl EmailCommitCount {
    pub fn new(email: Email, count: CommitCount) -> Self {
        Self { email, count }
    }
}

#[derive(Debug, Clone, Serialize)]
struct LangBreakdown {
    #[serde(skip_serializing_if = "Option::is_none")]
    inputs: Option<Vec<LanguageSize>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LanguageSize {
    lang: LanguageName,
    size: LanguageBytes,
}

impl LanguageSize {
    pub fn new(lang: LanguageName, size: LanguageBytes) -> Self {
        Self { lang, size }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum AclOp {
    Add,
    Remove,
}

#[derive(Debug, Clone, Serialize)]
pub struct KnotMemberUpdate {
    op: AclOp,
    subject: AccountDid,
}

impl KnotMemberUpdate {
    pub fn added(subject: AccountDid) -> Self {
        Self {
            op: AclOp::Add,
            subject,
        }
    }

    pub fn removed(subject: AccountDid) -> Self {
        Self {
            op: AclOp::Remove,
            subject,
        }
    }
}

impl Publish for KnotMemberUpdate {
    const NSID: &'static str = "sh.tangled.knot.memberUpdate";
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoCollaboratorUpdate {
    op: AclOp,
    subject: AccountDid,
    repo: RepoDid,
}

impl RepoCollaboratorUpdate {
    pub fn added(subject: AccountDid, repo: RepoDid) -> Self {
        Self {
            op: AclOp::Add,
            subject,
            repo,
        }
    }

    pub fn removed(subject: AccountDid, repo: RepoDid) -> Self {
        Self {
            op: AclOp::Remove,
            subject,
            repo,
        }
    }
}

impl Publish for RepoCollaboratorUpdate {
    const NSID: &'static str = "sh.tangled.repo.collaboratorUpdate";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayEvents(std::num::NonZeroUsize);

impl ReplayEvents {
    pub fn new(value: usize) -> Option<Self> {
        std::num::NonZeroUsize::new(value).map(Self)
    }

    pub fn get(self) -> usize {
        self.0.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayBytes(std::num::NonZeroUsize);

impl ReplayBytes {
    pub fn new(value: usize) -> Option<Self> {
        std::num::NonZeroUsize::new(value).map(Self)
    }

    pub fn get(self) -> usize {
        self.0.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayBounds {
    events: ReplayEvents,
    bytes: ReplayBytes,
}

impl ReplayBounds {
    pub fn new(events: ReplayEvents, bytes: ReplayBytes) -> Self {
        Self { events, bytes }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchEnd {
    CaughtUp,
    Bounded,
}

pub struct Replayed {
    pub events: Vec<Arc<Event>>,
    pub end: BatchEnd,
}

struct Entry {
    event: Arc<Event>,
    bytes: usize,
}

struct Ring {
    entries: VecDeque<Entry>,
    bytes: usize,
    last_micros: UnixMicros,
    pending: BTreeSet<EventCursor>,
}

struct Inner {
    bounds: ReplayBounds,
    ring: Mutex<Ring>,
    head: watch::Sender<EventCursor>,
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, Ring> {
        self.ring
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // leave that guiness be, boy! it needs to
    fn settle(&self, ring: std::sync::MutexGuard<'_, Ring>) {
        let head = stable_head(&ring);
        drop(ring);
        self.head.send_if_modified(|current| {
            let changed = *current != head;
            *current = head;
            changed
        });
    }
}

fn stable_head(ring: &Ring) -> EventCursor {
    let stable = match ring.pending.iter().next().copied() {
        Some(horizon) => ring
            .entries
            .partition_point(|entry| entry.event.created < horizon),
        None => ring.entries.len(),
    };
    stable
        .checked_sub(1)
        .and_then(|index| ring.entries.get(index))
        .map(|entry| entry.event.created)
        .unwrap_or(EventCursor::START)
}

fn value_bytes(value: &Value) -> usize {
    let node = std::mem::size_of::<Value>();
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => node,
        Value::String(text) => node + text.len(),
        Value::Array(items) => node + items.iter().map(value_bytes).sum::<usize>(),
        Value::Object(fields) => {
            node + fields
                .iter()
                .map(|(key, field)| key.len() + value_bytes(field))
                .sum::<usize>()
        }
    }
}

fn insert_sorted(ring: &mut Ring, event: Event, bounds: ReplayBounds) {
    let bytes = std::mem::size_of::<Event>() + value_bytes(&event.payload);
    let position = ring
        .entries
        .partition_point(|existing| existing.event.created < event.created);
    ring.entries.insert(
        position,
        Entry {
            event: Arc::new(event),
            bytes,
        },
    );
    ring.bytes += bytes;
    evict_oldest(ring, bounds);
}

fn evict_oldest(ring: &mut Ring, bounds: ReplayBounds) {
    let over = ring.entries.len() > bounds.events.get()
        || (ring.bytes > bounds.bytes.get() && ring.entries.len() > 1);
    if let Some(evicted) = over.then(|| ring.entries.pop_front()).flatten() {
        ring.bytes -= evicted.bytes;
        evict_oldest(ring, bounds);
    }
}

pub struct EventLog<C> {
    clock: C,
    inner: Arc<Inner>,
}

impl<C: Clock> EventLog<C> {
    pub fn new(clock: C, bounds: ReplayBounds) -> Self {
        Self {
            clock,
            inner: Arc::new(Inner {
                bounds,
                ring: Mutex::new(Ring {
                    entries: VecDeque::new(),
                    bytes: 0,
                    last_micros: UnixMicros::new(0),
                    pending: BTreeSet::new(),
                }),
                head: watch::Sender::new(EventCursor::START),
            }),
        }
    }

    fn next_cursor(&self, ring: &mut Ring) -> (UnixMicros, EventCursor) {
        let micros = self.clock.now_unix_micros().max(ring.last_micros.next());
        ring.last_micros = micros;
        (micros, EventCursor::from_unix_micros(micros))
    }

    pub fn publish<P: Publish>(&self, payload: &P) -> EventCursor {
        let payload = serde_json::to_value(payload).expect("event payload serializes to JSON");
        let mut ring = self.inner.lock();
        let (micros, created) = self.next_cursor(&mut ring);
        insert_sorted(
            &mut ring,
            Event {
                rkey: Tid::from_time(micros.get(), 0),
                nsid: P::NSID,
                payload,
                created,
            },
            self.inner.bounds,
        );
        self.inner.settle(ring);
        created
    }

    pub fn reserve(&self) -> Reservation {
        let mut ring = self.inner.lock();
        let (micros, cursor) = self.next_cursor(&mut ring);
        ring.pending.insert(cursor);
        drop(ring);
        Reservation {
            inner: Arc::clone(&self.inner),
            cursor,
            micros,
            fulfilled: false,
        }
    }

    pub fn replay(&self, after: EventCursor, bounds: ReplayBounds) -> Replayed {
        let ring = self.inner.lock();
        // The corresponding read side guarantee of `reserve`.
        let horizon = ring.pending.iter().next().copied();
        let visible = |entry: &&Entry| {
            entry.event.created > after
                && horizon.is_none_or(|horizon| entry.event.created < horizon)
        };
        let events: Vec<Arc<Event>> = ring
            .entries
            .iter()
            .filter(visible)
            .take(bounds.events.get())
            // Why the first event gets to ignore the byte bound?
            // Imagine a consumer whose next event is by itself wider
            // than the entire bound, right -
            // every batch it requests would come back empty,
            // its cursor would never advance,
            // it would ask again, repeat.
            // Sending that one event alone over the bound
            // is the only way.
            .scan(0usize, |spent, entry| {
                let first = *spent == 0;
                *spent += entry.bytes;
                (first || *spent <= bounds.bytes.get()).then(|| Arc::clone(&entry.event))
            })
            .collect();
        let end = match ring.entries.iter().filter(visible).nth(events.len()) {
            Some(_) => BatchEnd::Bounded,
            None => BatchEnd::CaughtUp,
        };
        Replayed { events, end }
    }

    pub fn subscribe(&self) -> watch::Receiver<EventCursor> {
        self.inner.head.subscribe()
    }
}

pub struct Reservation {
    inner: Arc<Inner>,
    cursor: EventCursor,
    micros: UnixMicros,
    fulfilled: bool,
}

impl Reservation {
    pub fn cursor(&self) -> EventCursor {
        self.cursor
    }

    pub fn fulfill<P: Publish>(mut self, payload: &P) {
        let payload = serde_json::to_value(payload).expect("event payload serializes to JSON");
        let mut ring = self.inner.lock();
        insert_sorted(
            &mut ring,
            Event {
                rkey: Tid::from_time(self.micros.get(), 0),
                nsid: P::NSID,
                payload,
                created: self.cursor,
            },
            self.inner.bounds,
        );
        ring.pending.remove(&self.cursor);
        self.inner.settle(ring);
        self.fulfilled = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.fulfilled {
            return;
        }
        let mut ring = self.inner.lock();
        ring.pending.remove(&self.cursor);
        self.inner.settle(ring);
    }
}

knot_types::scalar_newtype! {
    pub struct GlobalSubscriberLimit(usize);
    pub struct PerPeerSubscriberLimit(usize);
}

pub struct SubscriberGate {
    global: Arc<Semaphore>,
    per_peer_max: usize,
    peers: Mutex<HashMap<IpAddr, usize>>,
}

impl SubscriberGate {
    pub fn new(global_max: GlobalSubscriberLimit, per_peer_max: PerPeerSubscriberLimit) -> Self {
        Self {
            global: Arc::new(Semaphore::new(global_max.get().max(1))),
            per_peer_max: per_peer_max.get().max(1),
            peers: Mutex::new(HashMap::new()),
        }
    }

    pub fn try_admit(self: &Arc<Self>, peer: IpAddr) -> Option<SubscriberPermit> {
        let global = Arc::clone(&self.global).try_acquire_owned().ok()?;
        let mut peers = self
            .peers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = peers.get(&peer).copied().unwrap_or(0);
        if current >= self.per_peer_max {
            return None;
        }
        peers.insert(peer, current + 1);
        Some(SubscriberPermit {
            _global: global,
            gate: Arc::clone(self),
            peer,
        })
    }
}

pub struct SubscriberPermit {
    _global: OwnedSemaphorePermit,
    gate: Arc<SubscriberGate>,
    peer: IpAddr,
}

impl Drop for SubscriberPermit {
    fn drop(&mut self) {
        let mut peers = self
            .gate
            .peers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = peers.get_mut(&self.peer) {
            *count -= 1;
            if *count == 0 {
                peers.remove(&self.peer);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use knot_runtime::{ManualClock, UnixMicros};

    fn bounds(events: usize, bytes: usize) -> ReplayBounds {
        ReplayBounds::new(
            ReplayEvents::new(events).unwrap(),
            ReplayBytes::new(bytes).unwrap(),
        )
    }

    fn log(capacity: usize) -> EventLog<ManualClock> {
        EventLog::new(
            ManualClock::new(UnixMicros::new(1_700_000_000_000_000)),
            bounds(capacity, 1 << 20),
        )
    }

    fn replay(log: &EventLog<ManualClock>, after: EventCursor, limit: usize) -> Vec<Arc<Event>> {
        log.replay(after, bounds(limit, 1 << 30)).events
    }

    fn update() -> GitRefUpdate {
        GitRefUpdate::new(
            RepoDid::new("did:plc:limpet").unwrap(),
            Some(OwnerDid::new("did:web:olaren.dev").unwrap()),
            AccountDid::new("did:plc:nel").unwrap(),
        )
    }

    fn wire<P: Publish>(log: &EventLog<ManualClock>, payload: &P) -> serde_json::Value {
        log.publish(payload);
        serde_json::to_value(&*replay(log, EventCursor::START, 1).remove(0)).unwrap()
    }

    #[test]
    fn publish_nsids_are_valid_type_names() {
        [
            GitRefUpdate::NSID,
            KnotMemberUpdate::NSID,
            RepoCollaboratorUpdate::NSID,
        ]
        .iter()
        .for_each(|nsid| {
            assert!(knot_types::TypeName::new(*nsid).is_ok(), "{nsid}");
        });
    }

    #[test]
    fn a_frozen_clock_still_yields_strictly_increasing_cursors_and_distinct_rkeys() {
        let log = log(8);
        let cursors: Vec<_> = (0..3).map(|_| log.publish(&update())).collect();
        assert!(cursors.windows(2).all(|pair| pair[0] < pair[1]));
        let events = replay(&log, EventCursor::START, 8);
        let rkeys: std::collections::BTreeSet<_> = events
            .iter()
            .map(|event| event.rkey.as_str().to_string())
            .collect();
        assert_eq!(rkeys.len(), 3);
    }

    #[test]
    fn the_ring_evicts_the_oldest_event_past_capacity() {
        let log = log(2);
        let first = log.publish(&update());
        log.publish(&update());
        log.publish(&update());
        let replayed = replay(&log, EventCursor::START, 8);
        assert_eq!(replayed.len(), 2);
        assert!(replayed.iter().all(|event| event.created > first));
    }

    #[test]
    fn a_wide_event_evicts_by_bytes_long_before_the_ring_fills() {
        let wide = |count: usize| {
            update().with_changed_files(fill_changed(
                (0..count).map(|index| format!("crates/knot-events/src/f{index}.rs")),
            ))
        };
        let log = EventLog::new(
            ManualClock::new(UnixMicros::new(1_700_000_000_000_000)),
            bounds(1_024, 64 * 1_024),
        );
        (0..16).for_each(|_| {
            log.publish(&wide(512));
        });
        let replayed = replay(&log, EventCursor::START, 1_024);
        assert!(
            (1..16).contains(&replayed.len()),
            "the byte maximum evicts before the event maximum does: {}",
            replayed.len()
        );

        let one = EventLog::new(
            ManualClock::new(UnixMicros::new(1_700_000_000_000_000)),
            bounds(1_024, 1),
        );
        let only = one.publish(&wide(512));
        assert_eq!(
            replay(&one, EventCursor::START, 8)
                .iter()
                .map(|event| event.created)
                .collect::<Vec<EventCursor>>(),
            vec![only],
            "the ring keeps the one event wider than the whole byte maximum"
        );
    }

    fn fill_changed(paths: impl Iterator<Item = String>) -> ChangedFiles {
        let mut budget = knot_types::ChangedFilesBudget::new();
        let _ = paths.into_iter().try_for_each(|path| {
            budget.admit(knot_types::RepoPath::new(path).expect("test path is well-formed"))
        });
        budget.finish()
    }

    #[test]
    fn replay_honors_the_cursor_and_the_limit() {
        let log = log(8);
        let cursors: Vec<_> = (0..4).map(|_| log.publish(&update())).collect();
        let after_second = replay(&log, cursors[1], 8);
        assert_eq!(
            after_second
                .iter()
                .map(|event| event.created)
                .collect::<Vec<_>>(),
            cursors[2..].to_vec()
        );
        assert_eq!(replay(&log, EventCursor::START, 2).len(), 2);
        assert!(replay(&log, cursors[3], 8).is_empty());
    }

    #[test]
    fn a_replay_batch_stops_at_the_byte_maximum_and_reports_whether_more_remains() {
        let log = EventLog::new(
            ManualClock::new(UnixMicros::new(1_700_000_000_000_000)),
            bounds(1_024, 1 << 20),
        );
        assert_eq!(
            log.replay(EventCursor::START, bounds(8, 1 << 20)).end,
            BatchEnd::CaughtUp,
            "an empty ring has nothing left to send"
        );
        let wide = update().with_changed_files(fill_changed(
            (0..512).map(|index| format!("crates/knot-events/src/f{index}.rs")),
        ));
        let cursors: Vec<EventCursor> = (0..8).map(|_| log.publish(&wide)).collect();

        let batch = log.replay(EventCursor::START, bounds(1_024, 16 * 1_024));
        assert!(
            (1..8).contains(&batch.events.len()),
            "the byte maximum stops the batch before the event maximum does: {}",
            batch.events.len()
        );
        assert_eq!(batch.end, BatchEnd::Bounded);

        let rest = log.replay(
            batch.events.last().expect("the batch is nonempty").created,
            bounds(1_024, 1 << 30),
        );
        assert_eq!(rest.end, BatchEnd::CaughtUp);
        assert_eq!(
            batch.events.len() + rest.events.len(),
            cursors.len(),
            "the two batches together are every event, with none repeated or skipped"
        );
        let head = log.replay(cursors[7], bounds(8, 1 << 20));
        assert!(head.events.is_empty() && head.end == BatchEnd::CaughtUp);

        let single = log.replay(EventCursor::START, bounds(1_024, 1));
        assert_eq!(
            single.events.len(),
            1,
            "an event wider than the whole batch maximum is sent alone"
        );
        assert_eq!(single.end, BatchEnd::Bounded);
    }

    #[test]
    fn a_subscriber_observes_the_head_advance() {
        let log = log(8);
        let mut head = log.subscribe();
        assert_eq!(*head.borrow_and_update(), EventCursor::START);
        let created = log.publish(&update());
        assert!(head.has_changed().unwrap());
        assert_eq!(*head.borrow_and_update(), created);
    }

    #[test]
    fn the_wire_event_matches_the_eventstream_shape() {
        let wire = wire(&log(8), &update());
        assert_eq!(wire["nsid"], "sh.tangled.git.refUpdate");
        assert_eq!(wire["created"].as_i64().unwrap() % 1_000, 0);
        assert_eq!(wire["rkey"].as_str().unwrap().len(), 13);
        let payload = &wire["event"];
        assert_eq!(payload["$type"], "sh.tangled.git.refUpdate");
        assert_eq!(payload["committerDid"], "did:plc:nel");
        assert_eq!(payload["ownerDid"], "did:web:olaren.dev");
        assert_eq!(payload["repo"], "did:plc:limpet");
        assert_eq!(payload["meta"], serde_json::Value::Null);
        assert_eq!(payload["ref"], "");
        assert_eq!(
            payload["oldSha"], "",
            "a record about no ref sends the empty sha"
        );
        assert_eq!(payload["newSha"], "");
    }

    #[test]
    fn an_absent_sha_of_a_transition_is_the_null_oid_of_the_repo_object_format() {
        let new = Oid::from_hex(&"cd".repeat(32)).unwrap();
        let created = &wire(
            &log(8),
            &GitRefUpdate::new(
                RepoDid::new("did:plc:limpet").unwrap(),
                None,
                AccountDid::new("did:plc:nel").unwrap(),
            )
            .on_ref(
                RefName::new("refs/heads/fresh").unwrap(),
                RefTransition::Create { new },
                ObjectFormat::SHA256,
            ),
        )["event"];
        assert_eq!(created["oldSha"], "0".repeat(64));
        assert_eq!(created["newSha"], new.to_hex());

        let old = Oid::from_hex(&"ab".repeat(20)).unwrap();
        let rebuilt = update()
            .on_ref(
                RefName::new("refs/heads/fresh").unwrap(),
                RefTransition::Create {
                    new: Oid::from_hex(&"cd".repeat(20)).unwrap(),
                },
                ObjectFormat::SHA1,
            )
            .on_ref(
                RefName::new("refs/heads/gone").unwrap(),
                RefTransition::Delete { old },
                ObjectFormat::SHA1,
            );
        let payload = &wire(&log(8), &rebuilt)["event"];
        assert_eq!(
            payload["ref"], "refs/heads/gone",
            "a later transition replaces the earlier one whole"
        );
        assert_eq!(payload["oldSha"], old.to_hex());
        assert_eq!(payload["newSha"], "0".repeat(40));
    }

    fn peer(last: u8) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, last))
    }

    #[test]
    fn gate_enforces_limits_and_prunes() {
        let global = Arc::new(SubscriberGate::new(
            GlobalSubscriberLimit::new(2),
            PerPeerSubscriberLimit::new(8),
        ));
        let first = global
            .try_admit(peer(1))
            .expect("first subscriber is admitted");
        let _second = global
            .try_admit(peer(2))
            .expect("second subscriber is admitted");
        assert!(
            global.try_admit(peer(3)).is_none(),
            "third subscriber is refused once the global limit is reached"
        );
        drop(first);
        assert!(
            global.try_admit(peer(3)).is_some(),
            "freeing global slot admits waiting subscriber"
        );

        let per_peer = Arc::new(SubscriberGate::new(
            GlobalSubscriberLimit::new(16),
            PerPeerSubscriberLimit::new(2),
        ));
        let _socket = per_peer
            .try_admit(peer(1))
            .expect("first socket is admitted");
        let second = per_peer
            .try_admit(peer(1))
            .expect("second socket is admitted");
        assert!(
            per_peer.try_admit(peer(1)).is_none(),
            "third socket from same peer is refused at the per-peer limit"
        );
        assert!(
            per_peer.try_admit(peer(2)).is_some(),
            "different peer keeps its own budget"
        );
        drop(second);
        assert!(
            per_peer.try_admit(peer(1)).is_some(),
            "freed per-peer slot is reusable"
        );

        let prune = Arc::new(SubscriberGate::new(
            GlobalSubscriberLimit::new(16),
            PerPeerSubscriberLimit::new(2),
        ));
        let permit = prune.try_admit(peer(1)).expect("admitted");
        drop(permit);
        assert!(
            prune
                .peers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty(),
            "peer map prunes peer once its last socket closes"
        );
    }

    #[test]
    fn a_ref_update_includes_its_computed_meta_on_the_wire() {
        let wire = wire(
            &log(8),
            &update().with_meta(RefUpdateMeta::new(
                true,
                vec![EmailCommitCount::new(
                    Email::new("nel@oyster.cafe"),
                    CommitCount::new(3),
                )],
                vec![LanguageSize::new(
                    LanguageName::new("Rust"),
                    LanguageBytes::new(1234),
                )],
            )),
        );
        let meta = &wire["event"]["meta"];
        assert_eq!(meta["isDefaultRef"], true);
        assert_eq!(
            meta["commitCount"]["byEmail"][0]["email"],
            "nel@oyster.cafe"
        );
        assert_eq!(meta["commitCount"]["byEmail"][0]["count"], 3);
        assert_eq!(meta["langBreakdown"]["inputs"][0]["lang"], "Rust");
        assert_eq!(meta["langBreakdown"]["inputs"][0]["size"], 1234);
    }

    #[test]
    fn an_empty_breakdown_omits_the_optional_meta_arrays() {
        let wire = wire(
            &log(8),
            &update().with_meta(RefUpdateMeta::new(false, Vec::new(), Vec::new())),
        );
        let meta = &wire["event"]["meta"];
        assert_eq!(meta["isDefaultRef"], false);
        assert!(meta["commitCount"].get("byEmail").is_none());
        assert!(meta.get("langBreakdown").is_none());
    }

    #[test]
    fn acl_updates_match_eventstream_shape() {
        let log = log(8);
        log.publish(&KnotMemberUpdate::added(
            AccountDid::new("did:plc:nel").unwrap(),
        ));
        log.publish(&KnotMemberUpdate::removed(
            AccountDid::new("did:plc:olaren").unwrap(),
        ));
        log.publish(&RepoCollaboratorUpdate::added(
            AccountDid::new("did:plc:nel").unwrap(),
            RepoDid::new("did:plc:limpet").unwrap(),
        ));
        log.publish(&RepoCollaboratorUpdate::removed(
            AccountDid::new("did:plc:nel").unwrap(),
            RepoDid::new("did:plc:limpet").unwrap(),
        ));
        let events = replay(&log, EventCursor::START, 8);

        let member_added = serde_json::to_value(&*events[0]).unwrap();
        assert_eq!(member_added["nsid"], "sh.tangled.knot.memberUpdate");
        assert_eq!(member_added["event"]["op"], "add");
        assert_eq!(member_added["event"]["subject"], "did:plc:nel");
        assert!(member_added["event"].get("$type").is_none());
        let member_removed = serde_json::to_value(&*events[1]).unwrap();
        assert_eq!(member_removed["event"]["op"], "remove");
        assert_eq!(member_removed["event"]["subject"], "did:plc:olaren");

        let collab_added = serde_json::to_value(&*events[2]).unwrap();
        assert_eq!(collab_added["nsid"], "sh.tangled.repo.collaboratorUpdate");
        assert_eq!(collab_added["event"]["op"], "add");
        assert_eq!(collab_added["event"]["subject"], "did:plc:nel");
        assert_eq!(collab_added["event"]["repo"], "did:plc:limpet");
        assert!(collab_added["event"].get("$type").is_none());
        let collab_removed = serde_json::to_value(&*events[3]).unwrap();
        assert_eq!(collab_removed["event"]["op"], "remove");
        assert_eq!(collab_removed["event"]["repo"], "did:plc:limpet");
    }

    fn cursors(log: &EventLog<ManualClock>) -> Vec<EventCursor> {
        replay(log, EventCursor::START, 64)
            .iter()
            .map(|event| event.created)
            .collect()
    }

    #[test]
    fn a_reservation_holds_back_later_events_until_it_is_fulfilled() {
        let log = log(8);
        let early = log.publish(&update());
        let reservation = log.reserve();
        let later = log.publish(&update());
        assert!(reservation.cursor() > early && reservation.cursor() < later);
        assert_eq!(
            cursors(&log),
            vec![early],
            "event published after reservation waits behind it"
        );
        let mid = reservation.cursor();
        reservation.fulfill(&update());
        assert_eq!(
            cursors(&log),
            vec![early, mid, later],
            "fulfilling reservation releases it and event queued behind it, in cursor order"
        );
    }

    #[test]
    fn out_of_order_fulfillment_still_replays_in_cursor_order() {
        let log = log(8);
        let first = log.reserve();
        let second = log.reserve();
        let (c1, c2) = (first.cursor(), second.cursor());
        assert!(c1 < c2);
        second.fulfill(&update());
        assert!(
            cursors(&log).is_empty(),
            "later reservation stays hidden while earlier one is outstanding"
        );
        first.fulfill(&update());
        assert_eq!(
            cursors(&log),
            vec![c1, c2],
            "both surface in cursor order regardless of fulfillment order"
        );
    }

    #[test]
    fn a_dropped_reservation_unblocks_the_horizon_without_an_event() {
        let log = log(8);
        let reservation = log.reserve();
        let later = log.publish(&update());
        assert!(
            cursors(&log).is_empty(),
            "later event waits behind unfulfilled reservation"
        );
        drop(reservation);
        assert_eq!(
            cursors(&log),
            vec![later],
            "dropping reservation surfaces queued event and leaves no gap"
        );
    }

    #[test]
    fn the_head_holds_at_the_last_stable_event_until_a_reservation_is_fulfilled() {
        let log = log(8);
        let mut head = log.subscribe();
        let early = log.publish(&update());
        assert_eq!(*head.borrow_and_update(), early);
        let reservation = log.reserve();
        let later = log.publish(&update());
        assert_eq!(
            *head.borrow_and_update(),
            early,
            "head holds while lower-cursor reservation is pending"
        );
        reservation.fulfill(&update());
        assert_eq!(
            *head.borrow_and_update(),
            later,
            "fulfilling reservation advances head past released events"
        );
    }

    #[test]
    fn an_anonymous_owner_is_omitted_from_the_wire() {
        let wire = wire(
            &log(8),
            &GitRefUpdate::new(
                RepoDid::new("did:plc:limpet").unwrap(),
                None,
                AccountDid::new("did:plc:nel").unwrap(),
            ),
        );
        assert!(wire["event"].get("ownerDid").is_none());
    }
}
