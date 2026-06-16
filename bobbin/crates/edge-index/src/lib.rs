use std::collections::{BTreeSet, HashMap};
use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::ops::{Bound, ControlFlow};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

const FILTER_SCAN_MULTIPLIER: usize = 64;
const FILTER_SCAN_FLOOR: usize = 512;

struct ScanState {
    matched: Vec<(BucketKey, AtUri<DefaultStr>)>,
    last_scanned: Option<BucketKey>,
    scanned: usize,
}

impl ScanState {
    fn with_capacity(cap: usize) -> Self {
        Self {
            matched: Vec::with_capacity(cap),
            last_scanned: None,
            scanned: 0,
        }
    }
}

use bobbin_runtime::RuntimeHasher;
use bobbin_types::edges::Edge;
use bobbin_types::ids::EdgeKey;
use either::Either;
use jacquard_common::DefaultStr;
use jacquard_common::types::string::AtUri;
use lasso::{Key, Spur, ThreadedRodeo};
use scc::HashMap as SccMap;
use scc::hash_map::Entry;
use smallvec::SmallVec;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd)]
struct SortMicros(u64);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd)]
struct BucketKey {
    micros: SortMicros,
    source: SourceId,
}

impl BucketKey {
    fn new(micros: u64, source: SourceId) -> Self {
        Self {
            micros: SortMicros(micros),
            source,
        }
    }

    fn token(self) -> PageToken {
        PageToken::new(self.micros.0, self.source.index())
    }

    fn from_token(tok: PageToken) -> Self {
        Self {
            micros: SortMicros(tok.micros),
            source: SourceId::from_raw(tok.source),
        }
    }
}

pub mod coverage;
pub mod state_index;
pub use coverage::{Coverage, CoverageWatch, HydrantCursor, PromotionSignal};
pub use state_index::{
    ApplyOutcome, IssueStateKind, PullStatusKind, StateIndex, StateKind, apply_record_state,
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceTag;
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct AuthorTag;
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct CollectionTag;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Interned<T>(u32, PhantomData<T>);

impl<T: Copy> Interned<T> {
    fn from_spur(spur: Spur) -> Self {
        Self(spur.into_usize() as u32, PhantomData)
    }

    fn from_raw(raw: u32) -> Self {
        Self(raw, PhantomData)
    }

    fn index(self) -> u32 {
        self.0
    }

    fn to_spur(self) -> Option<Spur> {
        Spur::try_from_usize(self.0 as usize)
    }
}

pub type SourceId = Interned<SourceTag>;
type AuthorId = Interned<AuthorTag>;
type CollectionId = Interned<CollectionTag>;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PageToken {
    micros: u64,
    source: u32,
}

impl PageToken {
    pub fn new(micros: u64, source: u32) -> Self {
        Self { micros, source }
    }

    pub fn micros(self) -> u64 {
        self.micros
    }

    pub fn source(self) -> u32 {
        self.source
    }

    pub fn encode_token(self) -> String {
        let mut bytes = [0u8; 12];
        bytes[..8].copy_from_slice(&self.micros.to_be_bytes());
        bytes[8..].copy_from_slice(&self.source.to_be_bytes());
        encode_hex(&bytes)
    }

    pub fn decode_token(token: &str) -> Result<Self, CursorParseError> {
        let bytes: [u8; 12] = decode_hex_array(token).ok_or(CursorParseError::Malformed)?;
        let micros = u64::from_be_bytes(bytes[..8].try_into().unwrap());
        let source = u32::from_be_bytes(bytes[8..].try_into().unwrap());
        Ok(Self { micros, source })
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
            acc.push(char::from_digit((b >> 4) as u32, 16).unwrap());
            acc.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
            acc
        })
}

fn decode_hex_array<const N: usize>(token: &str) -> Option<[u8; N]> {
    if token.len() != N * 2 {
        return None;
    }
    let parsed: Vec<u8> = token
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            Some(((hi << 4) | lo) as u8)
        })
        .collect::<Option<Vec<u8>>>()?;
    parsed.try_into().ok()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageCursor {
    Start,
    After(PageToken),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SortDir {
    Asc,
    #[default]
    Desc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum CursorParseError {
    #[error("cursor token must be a valid TID")]
    Malformed,
}

impl PageCursor {
    pub fn from_token(raw: Option<&str>) -> Result<Self, CursorParseError> {
        raw.map_or(Ok(Self::Start), |t| {
            PageToken::decode_token(t).map(Self::After)
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PageLimit(u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum PageLimitError {
    #[error("page limit {value} below minimum {min}")]
    TooSmall { value: u32, min: u32 },
    #[error("page limit {value} above maximum {max}")]
    TooLarge { value: u32, max: u32 },
}

impl PageLimit {
    pub const MIN: u32 = 1;
    pub const MAX: u32 = 1000;

    pub fn new(value: u32) -> Result<Self, PageLimitError> {
        match value {
            v if v < Self::MIN => Err(PageLimitError::TooSmall {
                value: v,
                min: Self::MIN,
            }),
            v if v > Self::MAX => Err(PageLimitError::TooLarge {
                value: v,
                max: Self::MAX,
            }),
            v => Ok(Self(v)),
        }
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Debug)]
pub struct EdgePage {
    pub items: Vec<EdgeItem>,
    pub next: Option<PageToken>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EdgeItem {
    pub uri: AtUri<DefaultStr>,
    pub sort_micros: u64,
}

impl AsRef<str> for EdgeItem {
    fn as_ref(&self) -> &str {
        self.uri.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EdgeMemReport {
    pub key_count: u64,
    pub source_count: u64,
    pub source_interner_bytes: u64,
    pub did_interner_bytes: u64,
    pub collection_interner_bytes: u64,
    pub key_interner_bytes: u64,
    pub edges_total: u64,
    pub author_refs_total: u64,
    pub reverse_entries: u64,
    pub reverse_cap: u64,
    pub forward_struct_bytes: u64,
    pub reverse_struct_bytes: u64,
    pub bucket_struct_bytes: u64,
    pub max_bucket: u64,
    pub bucket_size_classes: [u64; BUCKET_CLASS_COUNT],
}

const BUCKET_CLASS_BOUNDS: [u64; 16] = [
    1,
    2,
    4,
    8,
    16,
    32,
    64,
    128,
    256,
    512,
    1024,
    2048,
    8192,
    32768,
    131072,
    u64::MAX,
];
const BUCKET_CLASS_COUNT: usize = BUCKET_CLASS_BOUNDS.len();

fn bucket_class(n: u64) -> usize {
    BUCKET_CLASS_BOUNDS
        .iter()
        .position(|&bound| n <= bound)
        .unwrap_or(BUCKET_CLASS_COUNT - 1)
}

impl EdgeMemReport {
    pub fn bucket_histogram(&self) -> impl Iterator<Item = (u64, u64)> {
        BUCKET_CLASS_BOUNDS
            .into_iter()
            .zip(self.bucket_size_classes)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct EdgeKeyId(u32);

fn bump_author(authors: &mut HashMap<AuthorId, NonZeroU32, RuntimeHasher>, author: AuthorId) {
    authors
        .entry(author)
        .and_modify(|c| *c = c.saturating_add(1))
        .or_insert(NonZeroU32::MIN);
}

fn drop_author(authors: &mut HashMap<AuthorId, NonZeroU32, RuntimeHasher>, author: AuthorId) {
    match authors.get(&author).map(|c| c.get() - 1) {
        Some(0) | None => {
            authors.remove(&author);
        }
        Some(next) => {
            authors.insert(author, NonZeroU32::new(next).unwrap());
        }
    }
}

struct LargeBucket {
    keys: BTreeSet<BucketKey>,
    authors: HashMap<AuthorId, NonZeroU32, RuntimeHasher>,
}

const BUCKET_PROMOTE_AT: usize = 256;
const SOURCE_BYTES: u64 = 16;

enum Sources {
    Small(SmallVec<[BucketKey; 2]>),
    Large(Box<LargeBucket>),
}

impl Default for Sources {
    fn default() -> Self {
        Self::Small(SmallVec::new())
    }
}

impl Sources {
    fn len(&self) -> usize {
        match self {
            Self::Small(v) => v.len(),
            Self::Large(big) => big.keys.len(),
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn insert(&mut self, key: BucketKey, author: Option<AuthorId>) -> bool {
        match self {
            Self::Small(v) => match v.binary_search(&key) {
                Ok(_) => false,
                Err(pos) => {
                    v.insert(pos, key);
                    true
                }
            },
            Self::Large(big) => {
                let inserted = big.keys.insert(key);
                if inserted && let Some(a) = author {
                    bump_author(&mut big.authors, a);
                }
                inserted
            }
        }
    }

    fn remove(&mut self, key: &BucketKey, author: Option<AuthorId>) {
        match self {
            Self::Small(v) => {
                if let Ok(pos) = v.binary_search(key) {
                    v.remove(pos);
                }
            }
            Self::Large(big) => {
                if big.keys.remove(key)
                    && let Some(a) = author
                {
                    drop_author(&mut big.authors, a);
                }
            }
        }
    }

    fn directed(
        &self,
        cursor: PageCursor,
        dir: SortDir,
    ) -> Box<dyn Iterator<Item = BucketKey> + '_> {
        match self {
            Self::Small(v) => Box::new(directed_slice(v, cursor, dir)),
            Self::Large(big) => Box::new(directed_tree(&big.keys, cursor, dir)),
        }
    }

    fn heap_bytes(&self) -> u64 {
        const BTREE_BYTES_PER_KEY: u64 = 32;
        const HASHMAP_FIXED: u64 = 48;
        const HASHMAP_PER_CAP: u64 = 9;
        match self {
            Self::Small(v) if v.spilled() => v.capacity() as u64 * SOURCE_BYTES,
            Self::Small(_) => 0,
            Self::Large(big) => {
                std::mem::size_of::<LargeBucket>() as u64
                    + big.keys.len() as u64 * BTREE_BYTES_PER_KEY
                    + HASHMAP_FIXED
                    + big.authors.capacity() as u64 * HASHMAP_PER_CAP
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReverseEntry {
    key_id: EdgeKeyId,
    sort_micros: u64,
}

pub struct EdgeStore {
    source_interner: Arc<ThreadedRodeo<Spur, RuntimeHasher>>,
    did_interner: Arc<ThreadedRodeo<Spur, RuntimeHasher>>,
    collection_interner: Arc<ThreadedRodeo<Spur, RuntimeHasher>>,
    key_ids: SccMap<EdgeKey, EdgeKeyId, RuntimeHasher>,
    next_key_id: AtomicU32,
    forward: SccMap<EdgeKeyId, Sources, RuntimeHasher>,
    reverse: SccMap<SourceId, SmallVec<[ReverseEntry; 1]>, RuntimeHasher>,
    hasher: RuntimeHasher,
    writer: Mutex<()>,
}

impl EdgeStore {
    pub fn new(hasher: RuntimeHasher) -> Self {
        Self {
            source_interner: Arc::new(ThreadedRodeo::with_hasher(hasher.clone())),
            did_interner: Arc::new(ThreadedRodeo::with_hasher(hasher.clone())),
            collection_interner: Arc::new(ThreadedRodeo::with_hasher(hasher.clone())),
            key_ids: SccMap::with_hasher(hasher.clone()),
            next_key_id: AtomicU32::new(0),
            forward: SccMap::with_hasher(hasher.clone()),
            reverse: SccMap::with_hasher(hasher.clone()),
            hasher,
            writer: Mutex::new(()),
        }
    }

    fn intern_key(&self, key: EdgeKey) -> EdgeKeyId {
        match self.key_ids.entry_sync(key) {
            Entry::Occupied(e) => *e.get(),
            Entry::Vacant(e) => {
                let id = EdgeKeyId(self.next_key_id.fetch_add(1, Ordering::Relaxed));
                e.insert_entry(id);
                id
            }
        }
    }

    fn lookup_key(&self, key: &EdgeKey) -> Option<EdgeKeyId> {
        self.key_ids.read_sync(key, |_, id| *id)
    }

    pub fn add(&self, edge: Edge) {
        let _w = self
            .writer
            .lock()
            .expect("edge-store writer mutex poisoned");
        self.add_locked(edge);
    }

    pub fn intern_source(&self, source: &AtUri<DefaultStr>) -> SourceId {
        let author = self.intern_author(source);
        SourceId::from_spur(
            self.source_interner
                .get_or_intern(self.source_key(source, author)),
        )
    }

    pub fn upsert_source(&self, source: &AtUri<DefaultStr>, edges: Vec<Edge>) {
        let _w = self
            .writer
            .lock()
            .expect("edge-store writer mutex poisoned");
        self.clear_source_locked(source);
        edges.into_iter().for_each(|e| self.add_locked(e));
    }

    pub fn remove_source(&self, source: &AtUri<DefaultStr>) {
        let _w = self
            .writer
            .lock()
            .expect("edge-store writer mutex poisoned");
        self.clear_source_locked(source);
    }

    fn add_locked(&self, edge: Edge) {
        let author = self.intern_author(&edge.source);
        let source_key = self.source_key(&edge.source, author);
        let id = SourceId::from_spur(self.source_interner.get_or_intern(source_key));
        let sort_micros = edge.sort_micros;
        let key_id = self.intern_key(EdgeKey::new(edge.kind, edge.subject));

        let key = BucketKey::new(sort_micros, id);
        let mut entry = self.forward.entry_sync(key_id).or_default();
        let inserted = entry.get_mut().insert(key, author);
        let promote_keys = match entry.get() {
            Sources::Small(v) if v.len() > BUCKET_PROMOTE_AT => {
                Some(v.iter().copied().collect::<Vec<_>>())
            }
            _ => None,
        };
        drop(entry);

        if let Some(keys) = promote_keys {
            let authors = self.build_author_map(&keys);
            let large = LargeBucket {
                keys: keys.into_iter().collect(),
                authors,
            };
            if let Entry::Occupied(mut e) = self.forward.entry_sync(key_id) {
                *e.get_mut() = Sources::Large(Box::new(large));
            }
        }

        if inserted {
            let mut rev = self.reverse.entry_sync(id).or_default();
            rev.get_mut().push(ReverseEntry {
                key_id,
                sort_micros,
            });
        }
    }

    fn clear_source_locked(&self, source: &AtUri<DefaultStr>) {
        let author = source_authority_did(source)
            .and_then(|s| self.did_interner.get(s))
            .map(AuthorId::from_spur);
        let Some(source_spur) = self.source_interner.get(self.source_key(source, author)) else {
            return;
        };
        let id = SourceId::from_spur(source_spur);
        let Some((_, entries)) = self.reverse.remove_sync(&id) else {
            return;
        };
        entries.into_iter().for_each(
            |ReverseEntry {
                 key_id,
                 sort_micros,
             }| {
                self.forward.update_sync(&key_id, |_, sources| {
                    sources.remove(&BucketKey::new(sort_micros, id), author);
                });
                self.forward
                    .remove_if_sync(&key_id, |sources| sources.is_empty());
            },
        );
    }

    fn intern_author(&self, source: &AtUri<DefaultStr>) -> Option<AuthorId> {
        let did = source_authority_did(source)?;
        Some(AuthorId::from_spur(self.did_interner.get_or_intern(did)))
    }

    fn source_key(&self, source: &AtUri<DefaultStr>, author: Option<AuthorId>) -> String {
        match (split_record_uri(source.as_ref()), author) {
            (Some((_, collection, rkey)), Some(author)) => {
                let collection =
                    CollectionId::from_spur(self.collection_interner.get_or_intern(collection));
                format!("{}/{}/{}", author.index(), collection.index(), rkey)
            }
            _ => source.as_ref().to_owned(),
        }
    }

    fn decode_source(&self, stored: &str) -> Option<String> {
        if stored.starts_with("at://") {
            return Some(stored.to_owned());
        }
        let mut parts = stored.splitn(3, '/');
        let author = AuthorId::from_raw(parts.next()?.parse().ok()?);
        let collection = CollectionId::from_raw(parts.next()?.parse().ok()?);
        let rkey = parts.next()?;
        let did = self.did_interner.try_resolve(&author.to_spur()?)?;
        let collection = self
            .collection_interner
            .try_resolve(&collection.to_spur()?)?;
        Some(format!("at://{did}/{collection}/{rkey}"))
    }

    fn author_of_stored(&self, stored: &str) -> Option<AuthorId> {
        match stored.strip_prefix("at://") {
            Some(rest) => {
                let authority = rest.split('/').next().unwrap_or(rest);
                authority
                    .starts_with("did:")
                    .then(|| self.did_interner.get(authority).map(AuthorId::from_spur))
                    .flatten()
            }
            None => stored
                .split('/')
                .next()?
                .parse::<u32>()
                .ok()
                .map(AuthorId::from_raw),
        }
    }

    fn author_of(&self, source: SourceId) -> Option<AuthorId> {
        let spur = source.to_spur()?;
        let stored = self.source_interner.try_resolve(&spur)?;
        self.author_of_stored(stored)
    }

    fn distinct_authors_small(&self, keys: &[BucketKey]) -> u64 {
        keys.iter()
            .filter_map(|key| self.author_of(key.source))
            .collect::<std::collections::HashSet<AuthorId>>()
            .len() as u64
    }

    fn build_author_map(&self, keys: &[BucketKey]) -> HashMap<AuthorId, NonZeroU32, RuntimeHasher> {
        keys.iter().fold(
            HashMap::with_hasher(self.hasher.clone()),
            |mut authors, key| {
                if let Some(a) = self.author_of(key.source) {
                    bump_author(&mut authors, a);
                }
                authors
            },
        )
    }

    pub fn count(&self, key: &EdgeKey) -> u64 {
        self.lookup_key(key)
            .and_then(|id| {
                self.forward
                    .read_sync(&id, |_, sources| sources.len() as u64)
            })
            .unwrap_or(0)
    }

    pub fn count_distinct_authors(&self, key: &EdgeKey) -> u64 {
        self.lookup_key(key)
            .and_then(|id| {
                self.forward.read_sync(&id, |_, sources| match sources {
                    Sources::Large(big) => big.authors.len() as u64,
                    Sources::Small(v) => self.distinct_authors_small(v),
                })
            })
            .unwrap_or(0)
    }

    pub fn sources_for(&self, key: &EdgeKey) -> Vec<AtUri<DefaultStr>> {
        self.lookup_key(key)
            .and_then(|id| {
                self.forward.read_sync(&id, |_, sources| {
                    sources
                        .directed(PageCursor::Start, SortDir::Desc)
                        .filter_map(|bucket| {
                            let spur = bucket.source.to_spur()?;
                            let stored = self.source_interner.try_resolve(&spur)?;
                            AtUri::new_owned(self.decode_source(stored)?).ok()
                        })
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default()
    }

    pub fn list(
        &self,
        key: &EdgeKey,
        cursor: PageCursor,
        limit: PageLimit,
        dir: SortDir,
    ) -> EdgePage {
        let limit_usize = limit.get() as usize;
        self.lookup_key(key)
            .and_then(|id| {
                self.forward.read_sync(&id, |_, sources| {
                    let iter = sources.directed(cursor, dir);
                    let entries: Vec<BucketKey> = iter.take(limit_usize + 1).collect();
                    let has_more = entries.len() > limit_usize;
                    let page = &entries[..entries.len().min(limit_usize)];
                    let items = page
                        .iter()
                        .filter_map(|&key| {
                            let spur = key.source.to_spur()?;
                            let stored = self.source_interner.try_resolve(&spur)?;
                            let uri = AtUri::new_owned(self.decode_source(stored)?).ok()?;
                            Some(EdgeItem {
                                uri,
                                sort_micros: key.micros.0,
                            })
                        })
                        .collect();
                    let next = has_more
                        .then(|| page.last().copied())
                        .flatten()
                        .map(BucketKey::token);
                    EdgePage { items, next }
                })
            })
            .unwrap_or(EdgePage {
                items: Vec::new(),
                next: None,
            })
    }

    pub fn list_filtered<F>(
        &self,
        key: &EdgeKey,
        cursor: PageCursor,
        limit: PageLimit,
        dir: SortDir,
        predicate: F,
    ) -> EdgePage
    where
        F: Fn(&AtUri<DefaultStr>) -> bool,
    {
        let limit_usize = limit.get() as usize;
        let scan_cap = limit_usize
            .saturating_mul(FILTER_SCAN_MULTIPLIER)
            .max(FILTER_SCAN_FLOOR);
        self.lookup_key(key)
            .and_then(|id| {
                self.forward.read_sync(&id, |_, sources| {
                    let init = ScanState::with_capacity(limit_usize + 1);
                    let outcome = sources
                        .directed(cursor, dir)
                        .try_fold(init, |mut state, key| {
                            if state.scanned >= scan_cap && state.matched.len() <= limit_usize {
                                return ControlFlow::Break(state);
                            }
                            state.scanned += 1;
                            state.last_scanned = Some(key);
                            if let Some(spur) = key.source.to_spur()
                                && let Some(stored) = self.source_interner.try_resolve(&spur)
                                && let Some(decoded) = self.decode_source(stored)
                                && let Ok(uri) = AtUri::new_owned(decoded)
                                && predicate(&uri)
                            {
                                state.matched.push((key, uri));
                                if state.matched.len() > limit_usize {
                                    return ControlFlow::Break(state);
                                }
                            }
                            ControlFlow::Continue(state)
                        });
                    let (state, bucket_exhausted) = match outcome {
                        ControlFlow::Continue(s) => (s, true),
                        ControlFlow::Break(s) => (s, false),
                    };
                    let has_more_matches = state.matched.len() > limit_usize;
                    let visible_len = state.matched.len().min(limit_usize);
                    let next = if has_more_matches && visible_len > 0 {
                        Some(state.matched[visible_len - 1].0.token())
                    } else if !bucket_exhausted {
                        state.last_scanned.map(BucketKey::token)
                    } else {
                        None
                    };
                    let items = state
                        .matched
                        .into_iter()
                        .take(visible_len)
                        .map(|(key, uri)| EdgeItem {
                            uri,
                            sort_micros: key.micros.0,
                        })
                        .collect();
                    EdgePage { items, next }
                })
            })
            .unwrap_or(EdgePage {
                items: Vec::new(),
                next: None,
            })
    }

    pub fn key_count(&self) -> usize {
        self.forward.len()
    }

    pub fn source_count(&self) -> usize {
        self.reverse.len()
    }

    pub fn mem_report(&self) -> EdgeMemReport {
        const SCC_SLOT: u64 = 32;
        let edge_key = std::mem::size_of::<EdgeKey>() as u64;
        let rev_entry = std::mem::size_of::<ReverseEntry>() as u64;
        let rev_smallvec = std::mem::size_of::<SmallVec<[ReverseEntry; 1]>>() as u64;
        let bucket_struct = std::mem::size_of::<Sources>() as u64;

        let mut edges_total = 0u64;
        let mut author_refs_total = 0u64;
        let mut forward_struct_bytes = 0u64;
        let mut max_bucket = 0u64;
        let mut bucket_size_classes = [0u64; BUCKET_CLASS_COUNT];
        self.forward.iter_sync(|_, sources| {
            let bucket_len = sources.len() as u64;
            edges_total += bucket_len;
            max_bucket = max_bucket.max(bucket_len);
            bucket_size_classes[bucket_class(bucket_len)] += 1;
            if let Sources::Large(big) = sources {
                author_refs_total += big.authors.len() as u64;
            }
            forward_struct_bytes += SCC_SLOT + bucket_struct + sources.heap_bytes();
            true
        });
        let key_interner_bytes = self.key_ids.len() as u64
            * (edge_key + std::mem::size_of::<EdgeKeyId>() as u64 + SCC_SLOT);

        let mut reverse_entries = 0u64;
        let mut reverse_cap = 0u64;
        let mut reverse_struct_bytes = 0u64;
        self.reverse.iter_sync(|_, refs| {
            let cap = refs.capacity() as u64;
            reverse_entries += refs.len() as u64;
            reverse_cap += cap;
            let heap = if refs.spilled() { cap * rev_entry } else { 0 };
            reverse_struct_bytes += SCC_SLOT + rev_smallvec + heap;
            true
        });

        EdgeMemReport {
            key_count: self.forward.len() as u64,
            source_count: self.reverse.len() as u64,
            source_interner_bytes: self.source_interner.current_memory_usage() as u64,
            did_interner_bytes: self.did_interner.current_memory_usage() as u64,
            collection_interner_bytes: self.collection_interner.current_memory_usage() as u64,
            key_interner_bytes,
            edges_total,
            author_refs_total,
            reverse_entries,
            reverse_cap,
            forward_struct_bytes,
            reverse_struct_bytes,
            bucket_struct_bytes: bucket_struct,
            max_bucket,
            bucket_size_classes,
        }
    }
}

fn directed_slice(
    sources: &[BucketKey],
    cursor: PageCursor,
    dir: SortDir,
) -> impl Iterator<Item = BucketKey> + '_ {
    match dir {
        SortDir::Asc => {
            let start = match cursor {
                PageCursor::Start => 0,
                PageCursor::After(tok) => {
                    sources.partition_point(|k| *k <= BucketKey::from_token(tok))
                }
            };
            Either::Left(sources[start..].iter().copied())
        }
        SortDir::Desc => {
            let end = match cursor {
                PageCursor::Start => sources.len(),
                PageCursor::After(tok) => {
                    sources.partition_point(|k| *k < BucketKey::from_token(tok))
                }
            };
            Either::Right(sources[..end].iter().rev().copied())
        }
    }
}

fn directed_tree(
    keys: &BTreeSet<BucketKey>,
    cursor: PageCursor,
    dir: SortDir,
) -> impl Iterator<Item = BucketKey> + '_ {
    match dir {
        SortDir::Asc => {
            let lower = match cursor {
                PageCursor::Start => Bound::Unbounded,
                PageCursor::After(tok) => Bound::Excluded(BucketKey::from_token(tok)),
            };
            Either::Left(keys.range((lower, Bound::Unbounded)).copied())
        }
        SortDir::Desc => {
            let upper = match cursor {
                PageCursor::Start => Bound::Unbounded,
                PageCursor::After(tok) => Bound::Excluded(BucketKey::from_token(tok)),
            };
            Either::Right(keys.range((Bound::Unbounded, upper)).rev().copied())
        }
    }
}

fn split_record_uri(source: &str) -> Option<(&str, &str, &str)> {
    let rest = source.strip_prefix("at://")?;
    let mut parts = rest.split('/');
    let authority = parts.next()?;
    let collection = parts.next()?;
    let rkey = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    (authority.starts_with("did:") && !collection.is_empty() && !rkey.is_empty())
        .then_some((authority, collection, rkey))
}

fn source_authority_did(source: &AtUri<DefaultStr>) -> Option<&str> {
    let rest = source.as_ref().strip_prefix("at://")?;
    let end = rest.find('/').unwrap_or(rest.len());
    let candidate = &rest[..end];
    candidate.starts_with("did:").then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobbin_types::ids::SubjectRef;
    use jacquard_common::types::did::Did;
    use jacquard_common::types::nsid::Nsid;
    use jacquard_common::types::string::AtUri;

    fn store() -> EdgeStore {
        EdgeStore::new(RuntimeHasher::default())
    }

    fn nsid(s: &'static str) -> Nsid<DefaultStr> {
        Nsid::new_static(s).unwrap()
    }

    fn at(s: &str) -> AtUri<DefaultStr> {
        AtUri::new_owned(s).unwrap()
    }

    fn did(s: &str) -> Did<DefaultStr> {
        Did::new_owned(s).unwrap()
    }

    fn did_subj(s: &str) -> SubjectRef {
        SubjectRef::Did(did(s))
    }

    fn limit(n: u32) -> PageLimit {
        PageLimit::new(n).unwrap()
    }

    const NAMES: [&str; 5] = ["nel", "olaren", "teq", "lyna", "bailey"];

    fn star_edge(source: AtUri<DefaultStr>, subject: Did<DefaultStr>) -> Edge {
        star_edge_at(source, subject, 0)
    }

    fn star_edge_at(source: AtUri<DefaultStr>, subject: Did<DefaultStr>, sort_micros: u64) -> Edge {
        Edge {
            kind: nsid("sh.tangled.feed.star"),
            subject: SubjectRef::Did(subject),
            source,
            sort_micros,
        }
    }

    fn shuffled_micros(i: usize) -> u64 {
        (i as u64).wrapping_mul(2_654_435_761) % 64
    }

    fn source_uri(i: usize) -> String {
        format!(
            "at://did:plc:{}/sh.tangled.feed.star/r{i}",
            NAMES[i % NAMES.len()]
        )
    }

    fn fill_subject(store: &EdgeStore, n: usize) -> EdgeKey {
        let subject = did("did:plc:squid");
        (0..n).for_each(|i| {
            store.add(star_edge_at(
                at(&source_uri(i)),
                subject.clone(),
                shuffled_micros(i),
            ));
        });
        EdgeKey::new(nsid("sh.tangled.feed.star"), did_subj("did:plc:squid"))
    }

    fn reference(kept: impl Iterator<Item = usize>) -> Vec<String> {
        let mut rows: Vec<(u64, usize)> = kept.map(|i| (shuffled_micros(i), i)).collect();
        rows.sort_unstable();
        rows.into_iter().map(|(_, i)| source_uri(i)).collect()
    }

    fn paginate_all(store: &EdgeStore, key: &EdgeKey, dir: SortDir, page: u32) -> Vec<String> {
        std::iter::successors(
            Some(store.list(key, PageCursor::Start, limit(page), dir)),
            |prev| {
                prev.next
                    .map(|tok| store.list(key, PageCursor::After(tok), limit(page), dir))
            },
        )
        .flat_map(|p| {
            p.items
                .into_iter()
                .map(|u| u.as_ref().to_owned())
                .collect::<Vec<_>>()
        })
        .collect()
    }

    #[test]
    fn pagination_matches_reference_across_small_and_large() {
        [64usize, 1000].into_iter().for_each(|n| {
            let store = store();
            let key = fill_subject(&store, n);
            assert_eq!(store.count(&key), n as u64, "count mismatch at n={n}");
            assert_eq!(
                store.count_distinct_authors(&key),
                NAMES.len() as u64,
                "distinct authors at n={n}"
            );

            let asc = reference(0..n);
            let desc: Vec<String> = asc.iter().rev().cloned().collect();

            [3u32, 7, 50].into_iter().for_each(|page| {
                assert_eq!(
                    paginate_all(&store, &key, SortDir::Asc, page),
                    asc,
                    "asc mismatch n={n} page={page}"
                );
                assert_eq!(
                    paginate_all(&store, &key, SortDir::Desc, page),
                    desc,
                    "desc mismatch n={n} page={page}"
                );
            });
        });
    }

    #[test]
    fn remove_from_large_bucket_keeps_pagination_exact() {
        let store = store();
        let n = 1000usize;
        let key = fill_subject(&store, n);
        (0..n).step_by(3).for_each(|i| {
            store.remove_source(&at(&source_uri(i)));
        });
        let expected = reference((0..n).filter(|i| i % 3 != 0));
        assert_eq!(store.count(&key), expected.len() as u64);
        assert_eq!(
            store.count_distinct_authors(&key),
            NAMES.len() as u64,
            "every author keeps sources after partial removal"
        );
        assert_eq!(paginate_all(&store, &key, SortDir::Asc, 7), expected);
        assert_eq!(
            paginate_all(&store, &key, SortDir::Desc, 11),
            expected.iter().rev().cloned().collect::<Vec<_>>()
        );
    }

    #[test]
    fn add_then_count() {
        let store = store();
        let key = EdgeKey::new(nsid("sh.tangled.feed.star"), did_subj("did:plc:abalone"));

        store.add(star_edge(
            at("at://did:plc:nel/sh.tangled.feed.star/r1"),
            did("did:plc:abalone"),
        ));
        store.add(star_edge(
            at("at://did:plc:olaren/sh.tangled.feed.star/r2"),
            did("did:plc:abalone"),
        ));
        store.add(star_edge(
            at("at://did:plc:nel/sh.tangled.feed.star/r3"),
            did("did:plc:abalone"),
        ));

        assert_eq!(store.count(&key), 3);
        assert_eq!(store.count_distinct_authors(&key), 2);
    }

    #[test]
    fn duplicate_add_is_idempotent() {
        let store = store();
        let key = EdgeKey::new(nsid("sh.tangled.feed.star"), did_subj("did:plc:abalone"));
        let edge = star_edge(
            at("at://did:plc:nel/sh.tangled.feed.star/r1"),
            did("did:plc:abalone"),
        );
        store.add(edge.clone());
        store.add(edge);
        assert_eq!(store.count(&key), 1);
        assert_eq!(store.count_distinct_authors(&key), 1);
    }

    #[test]
    fn remove_source_clears_all_keys_for_that_source() {
        let store = store();
        let star_key = EdgeKey::new(nsid("sh.tangled.feed.star"), did_subj("did:plc:abalone"));
        let follow_key = EdgeKey::new(nsid("sh.tangled.graph.follow"), did_subj("did:plc:lyna"));
        let source = "at://did:plc:nel/sh.tangled.feed.star/r1";

        store.add(Edge {
            kind: nsid("sh.tangled.feed.star"),
            subject: did_subj("did:plc:abalone"),
            source: at(source),
            sort_micros: 0,
        });
        store.add(Edge {
            kind: nsid("sh.tangled.graph.follow"),
            subject: did_subj("did:plc:lyna"),
            source: at(source),
            sort_micros: 0,
        });

        assert_eq!(store.count(&star_key), 1);
        assert_eq!(store.count(&follow_key), 1);

        store.remove_source(&at(source));
        assert_eq!(store.count(&star_key), 0);
        assert_eq!(store.count(&follow_key), 0);
    }

    #[test]
    fn upsert_source_replaces_old_edges() {
        let store = store();
        let source = at("at://did:plc:teq/sh.tangled.feed.star/r1");
        let old_subject = did_subj("did:plc:abalone");
        let new_subject = did_subj("did:plc:uni");
        let kind = nsid("sh.tangled.feed.star");

        store.upsert_source(
            &source,
            vec![Edge {
                kind: kind.clone(),
                subject: old_subject.clone(),
                source: source.clone(),
                sort_micros: 0,
            }],
        );
        assert_eq!(
            store.count(&EdgeKey::new(kind.clone(), old_subject.clone())),
            1
        );

        store.upsert_source(
            &source,
            vec![Edge {
                kind: kind.clone(),
                subject: new_subject.clone(),
                source: source.clone(),
                sort_micros: 0,
            }],
        );
        assert_eq!(store.count(&EdgeKey::new(kind.clone(), old_subject)), 0);
        assert_eq!(store.count(&EdgeKey::new(kind, new_subject)), 1);
    }

    #[test]
    fn list_pages_in_sort_order() {
        let store = store();
        let key = EdgeKey::new(nsid("sh.tangled.feed.star"), did_subj("did:plc:abalone"));
        (0..5).for_each(|i| {
            store.add(star_edge_at(
                at(&format!(
                    "at://did:plc:{}/sh.tangled.feed.star/r{i}",
                    NAMES[i]
                )),
                did("did:plc:abalone"),
                1_000_000 + i as u64 * 1_000_000,
            ));
        });

        let page1 = store.list(&key, PageCursor::Start, limit(2), SortDir::Asc);
        assert_eq!(page1.items.len(), 2);
        let cursor = PageCursor::After(page1.next.expect("has next"));

        let page2 = store.list(&key, cursor, limit(2), SortDir::Asc);
        assert_eq!(page2.items.len(), 2);

        let cursor2 = PageCursor::After(page2.next.expect("has next"));
        let page3 = store.list(&key, cursor2, limit(2), SortDir::Asc);
        assert_eq!(page3.items.len(), 1);
        assert!(
            page3.next.is_none(),
            "final partial page should signal exhaustion"
        );
    }

    #[test]
    fn list_exact_fill_signals_exhaustion() {
        let store = store();
        let key = EdgeKey::new(nsid("sh.tangled.feed.star"), did_subj("did:plc:abalone"));
        (0..2).for_each(|i| {
            store.add(star_edge(
                at(&format!(
                    "at://did:plc:{}/sh.tangled.feed.star/r{i}",
                    NAMES[i]
                )),
                did("did:plc:abalone"),
            ));
        });

        let page = store.list(&key, PageCursor::Start, limit(2), SortDir::Asc);
        assert_eq!(page.items.len(), 2);
        assert!(page.next.is_none(), "exact-fill page must not promise more");
    }

    #[test]
    fn list_on_unknown_key_is_empty() {
        let store = store();
        let page = store.list(
            &EdgeKey::new(nsid("sh.tangled.feed.star"), did_subj("did:plc:periwinkle")),
            PageCursor::Start,
            limit(10),
            SortDir::Asc,
        );
        assert!(page.items.is_empty());
        assert!(page.next.is_none());
    }

    #[test]
    fn distinct_authors_decreases_when_last_source_from_author_removed() {
        let store = store();
        let key = EdgeKey::new(nsid("sh.tangled.feed.star"), did_subj("did:plc:abalone"));
        let s1 = at("at://did:plc:nel/sh.tangled.feed.star/r1");
        let s2 = at("at://did:plc:nel/sh.tangled.feed.star/r2");
        let s3 = at("at://did:plc:olaren/sh.tangled.feed.star/r3");

        store.add(star_edge(s1.clone(), did("did:plc:abalone")));
        store.add(star_edge(s2.clone(), did("did:plc:abalone")));
        store.add(star_edge(s3, did("did:plc:abalone")));
        assert_eq!(store.count_distinct_authors(&key), 2);

        store.remove_source(&s1);
        assert_eq!(store.count_distinct_authors(&key), 2, "user1 still has s2");

        store.remove_source(&s2);
        assert_eq!(store.count_distinct_authors(&key), 1, "user1 fully gone");
    }

    #[test]
    fn page_limit_rejects_zero_and_oversize() {
        assert!(matches!(
            PageLimit::new(0),
            Err(PageLimitError::TooSmall { value: 0, min: 1 })
        ));
        assert!(matches!(
            PageLimit::new(PageLimit::MAX + 1),
            Err(PageLimitError::TooLarge { .. })
        ));
        assert_eq!(PageLimit::new(50).unwrap().get(), 50);
    }

    #[test]
    fn cursor_token_round_trip() {
        let original = PageToken::new(1_730_000_000_000_000, 0x1234_abcd);
        let token = original.encode_token();
        assert_eq!(token.len(), 24, "12-byte cursor encodes to 24 hex chars");
        assert_eq!(PageToken::decode_token(&token).unwrap(), original);
    }

    #[test]
    fn from_token_none_yields_start() {
        assert_eq!(PageCursor::from_token(None).unwrap(), PageCursor::Start);
    }

    #[test]
    fn from_token_some_yields_after() {
        let original = PageToken::new(1_730_000_000_000_000, 42);
        let token = original.encode_token();
        assert_eq!(
            PageCursor::from_token(Some(&token)).unwrap(),
            PageCursor::After(original),
        );
    }

    #[test]
    fn cursor_decode_rejects_malformed() {
        let bad = [
            "",
            "deadbeef",
            "no-hex!!aaaaaaaaaaaaaaaa",
            "12345",
            "this-string-is-way-too-long-to-be-a-valid-cursor",
        ];
        bad.into_iter().for_each(|s| {
            assert!(
                matches!(PageToken::decode_token(s), Err(CursorParseError::Malformed)),
                "expected malformed for {s:?}",
            );
        });
    }

    #[test]
    fn cursor_token_is_hex_shape() {
        let token = PageToken::new(1_730_000_000_000_000, 7).encode_token();
        assert_eq!(token.len(), 24);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn list_does_not_drop_entries_at_same_sort_micros_across_pages() {
        let store = store();
        let subject_did = did("did:plc:limpet");
        let key = EdgeKey::new(
            nsid("sh.tangled.feed.star"),
            SubjectRef::Did(subject_did.clone()),
        );
        (0..4).for_each(|i| {
            store.add(star_edge_at(
                at(&format!(
                    "at://did:plc:{}/sh.tangled.feed.star/r{i}",
                    NAMES[i]
                )),
                subject_did.clone(),
                42,
            ));
        });

        let page1 = store.list(&key, PageCursor::Start, limit(2), SortDir::Asc);
        assert_eq!(page1.items.len(), 2);
        let token = page1.next.expect("cursor must continue across ties");

        let page2 = store.list(&key, PageCursor::After(token), limit(2), SortDir::Asc);
        assert_eq!(
            page2.items.len(),
            2,
            "remaining ties must surface on next page"
        );
        assert!(page2.next.is_none());
        let combined: std::collections::HashSet<String> = page1
            .items
            .iter()
            .chain(page2.items.iter())
            .map(|u| u.as_ref().to_owned())
            .collect();
        assert_eq!(combined.len(), 4, "every tied entry visible exactly once");
    }

    #[test]
    fn list_filtered_narrows_by_predicate_and_paginates() {
        let store = store();
        let subject_did = did("did:plc:limpet");
        let key = EdgeKey::new(
            nsid("sh.tangled.repo.issue"),
            SubjectRef::Did(subject_did.clone()),
        );
        let by_nel = (0..3).map(|i| {
            star_edge_at(
                at(&format!("at://did:plc:nel/sh.tangled.repo.issue/r{i}")),
                subject_did.clone(),
                100 + i as u64,
            )
        });
        let by_olaren = (0..2).map(|i| {
            star_edge_at(
                at(&format!("at://did:plc:olaren/sh.tangled.repo.issue/o{i}")),
                subject_did.clone(),
                500 + i as u64,
            )
        });
        by_nel
            .chain(by_olaren)
            .map(|mut e| {
                e.kind = nsid("sh.tangled.repo.issue");
                e
            })
            .for_each(|e| store.add(e));

        let only_nel = |u: &AtUri<DefaultStr>| u.as_ref().starts_with("at://did:plc:nel/");
        let page1 = store.list_filtered(&key, PageCursor::Start, limit(2), SortDir::Asc, only_nel);
        assert_eq!(page1.items.len(), 2);
        assert!(page1.next.is_some(), "cursor must allow more nel matches");

        let page2 = store.list_filtered(
            &key,
            PageCursor::After(page1.next.unwrap()),
            limit(2),
            SortDir::Asc,
            only_nel,
        );
        assert_eq!(page2.items.len(), 1, "only one nel issue left");
        assert!(page2.next.is_none(), "tail page must not promise more");
    }

    #[test]
    fn list_descending_returns_newest_first() {
        let store = store();
        let subject_did = did("did:plc:limpet");
        let key = EdgeKey::new(
            nsid("sh.tangled.feed.star"),
            SubjectRef::Did(subject_did.clone()),
        );
        (0..5).for_each(|i| {
            store.add(star_edge_at(
                at(&format!(
                    "at://did:plc:{}/sh.tangled.feed.star/r{i}",
                    NAMES[i]
                )),
                subject_did.clone(),
                1_000_000 + i as u64 * 1_000_000,
            ));
        });

        let asc = store.list(&key, PageCursor::Start, limit(5), SortDir::Asc);
        let desc = store.list(&key, PageCursor::Start, limit(5), SortDir::Desc);
        assert_eq!(asc.items.len(), 5);
        assert_eq!(desc.items.len(), 5);
        let asc_uris: Vec<_> = asc.items.iter().map(|u| u.as_ref().to_owned()).collect();
        let mut reversed = asc_uris.clone();
        reversed.reverse();
        let desc_uris: Vec<_> = desc.items.iter().map(|u| u.as_ref().to_owned()).collect();
        assert_eq!(desc_uris, reversed, "desc must be exact reverse of asc");
    }

    #[test]
    fn list_descending_paginates_with_cursor() {
        let store = store();
        let subject_did = did("did:plc:whelk");
        let key = EdgeKey::new(
            nsid("sh.tangled.feed.star"),
            SubjectRef::Did(subject_did.clone()),
        );
        (0..5).for_each(|i| {
            store.add(star_edge_at(
                at(&format!(
                    "at://did:plc:{}/sh.tangled.feed.star/r{i}",
                    NAMES[i]
                )),
                subject_did.clone(),
                1_000_000 + i as u64 * 1_000_000,
            ));
        });

        let page1 = store.list(&key, PageCursor::Start, limit(2), SortDir::Desc);
        assert_eq!(page1.items.len(), 2);
        let cursor = PageCursor::After(page1.next.expect("desc page1 must continue"));
        let page2 = store.list(&key, cursor, limit(2), SortDir::Desc);
        assert_eq!(page2.items.len(), 2);
        let cursor2 = PageCursor::After(page2.next.expect("desc page2 must continue"));
        let page3 = store.list(&key, cursor2, limit(2), SortDir::Desc);
        assert_eq!(page3.items.len(), 1);
        assert!(page3.next.is_none());

        let combined: std::collections::HashSet<String> = page1
            .items
            .iter()
            .chain(page2.items.iter())
            .chain(page3.items.iter())
            .map(|u| u.as_ref().to_owned())
            .collect();
        assert_eq!(
            combined.len(),
            5,
            "desc pagination visits each item exactly once"
        );
    }

    #[test]
    fn list_filtered_descending_respects_predicate() {
        let store = store();
        let subject_did = did("did:plc:scallop");
        let key = EdgeKey::new(
            nsid("sh.tangled.repo.issue"),
            SubjectRef::Did(subject_did.clone()),
        );
        let by_nel = (0..3).map(|i| {
            star_edge_at(
                at(&format!("at://did:plc:nel/sh.tangled.repo.issue/r{i}")),
                subject_did.clone(),
                100 + i as u64,
            )
        });
        let by_olaren = (0..2).map(|i| {
            star_edge_at(
                at(&format!("at://did:plc:olaren/sh.tangled.repo.issue/o{i}")),
                subject_did.clone(),
                500 + i as u64,
            )
        });
        by_nel
            .chain(by_olaren)
            .map(|mut e| {
                e.kind = nsid("sh.tangled.repo.issue");
                e
            })
            .for_each(|e| store.add(e));

        let only_nel = |u: &AtUri<DefaultStr>| u.as_ref().starts_with("at://did:plc:nel/");
        let page = store.list_filtered(&key, PageCursor::Start, limit(5), SortDir::Desc, only_nel);
        assert_eq!(page.items.len(), 3, "all three nel issues visible");
        let last = page.items.last().unwrap().as_ref();
        let first = page.items.first().unwrap().as_ref();
        assert!(
            first > last,
            "desc order: first item rkey must be greater than last (got first={first}, last={last})",
        );
    }

    #[test]
    fn non_did_source_round_trips_via_raw_fallback() {
        let store = store();
        let subject = did("did:plc:limpet");
        let key = EdgeKey::new(
            nsid("sh.tangled.feed.star"),
            SubjectRef::Did(subject.clone()),
        );
        let did_source = at("at://did:plc:nel/sh.tangled.feed.star/r1");
        let handle_source = at("at://witchcraft.systems/sh.tangled.feed.star/r2");
        store.add(star_edge_at(did_source.clone(), subject.clone(), 1));
        store.add(star_edge_at(handle_source.clone(), subject.clone(), 2));

        let page = store.list(&key, PageCursor::Start, limit(10), SortDir::Asc);
        let got: std::collections::HashSet<String> =
            page.items.iter().map(|u| u.as_ref().to_owned()).collect();
        assert!(
            got.contains(did_source.as_ref()),
            "did source must decode exactly"
        );
        assert!(
            got.contains(handle_source.as_ref()),
            "non-did authority must round-trip through the raw fallback"
        );

        store.remove_source(&handle_source);
        assert_eq!(
            store.count(&key),
            1,
            "raw-keyed source removable by its uri"
        );
    }

    #[test]
    fn distinct_collections_decode_with_their_own_collection() {
        let store = store();
        let subject = did("did:plc:limpet");
        let key = EdgeKey::new(
            nsid("sh.tangled.feed.star"),
            SubjectRef::Did(subject.clone()),
        );
        let star_src = at("at://did:plc:nel/sh.tangled.feed.star/aaa");
        let issue_src = at("at://did:plc:nel/sh.tangled.repo.issue/bbb");
        store.add(Edge {
            kind: nsid("sh.tangled.feed.star"),
            subject: SubjectRef::Did(subject.clone()),
            source: star_src.clone(),
            sort_micros: 1,
        });
        store.add(Edge {
            kind: nsid("sh.tangled.feed.star"),
            subject: SubjectRef::Did(subject.clone()),
            source: issue_src.clone(),
            sort_micros: 2,
        });

        let page = store.list(&key, PageCursor::Start, limit(10), SortDir::Asc);
        let got: std::collections::HashSet<String> =
            page.items.iter().map(|u| u.as_ref().to_owned()).collect();
        assert!(
            got.contains(star_src.as_ref()),
            "star-collection source decodes exactly"
        );
        assert!(
            got.contains(issue_src.as_ref()),
            "issue-collection source must keep its own collection, not borrow the star one"
        );
    }

    #[test]
    fn author_refs_spill_preserves_distinct_count() {
        let store = store();
        let subject = did("did:plc:scallop");
        let key = EdgeKey::new(
            nsid("sh.tangled.feed.star"),
            SubjectRef::Did(subject.clone()),
        );
        (0..5).for_each(|i| {
            store.add(star_edge_at(
                at(&format!(
                    "at://did:plc:{}/sh.tangled.feed.star/r{i}",
                    NAMES[i]
                )),
                subject.clone(),
                i as u64,
            ));
        });
        assert_eq!(
            store.count_distinct_authors(&key),
            5,
            "five authors exceed the inline cap and spill to the map"
        );

        store.add(star_edge_at(
            at("at://did:plc:nel/sh.tangled.feed.star/r99"),
            subject.clone(),
            99,
        ));
        assert_eq!(
            store.count_distinct_authors(&key),
            5,
            "second nel source adds no author"
        );

        store.remove_source(&at("at://did:plc:nel/sh.tangled.feed.star/r0"));
        assert_eq!(
            store.count_distinct_authors(&key),
            5,
            "nel still present via r99"
        );
        store.remove_source(&at("at://did:plc:nel/sh.tangled.feed.star/r99"));
        assert_eq!(store.count_distinct_authors(&key), 4, "nel fully removed");
    }
}
