use std::cell::RefCell;
use std::sync::Arc;
use std::sync::OnceLock;

use bobbin_types::record::RecordBody;
use bytes::Bytes;
use jacquard_common::DefaultStr;
use jacquard_common::types::string::{AtUri, Cid};
use quick_cache::Weighter;
use quick_cache::sync::Cache;
use zstd::bulk::{Compressor, Decompressor};
use zstd::dict::{DecoderDictionary, EncoderDictionary};

static RECORD_DICT: &[u8] = include_bytes!("record.dict");
const COMPRESS_LEVEL: i32 = 6;
const ENTRY_OVERHEAD: u64 = 64;

static ENCODER_DICT: OnceLock<EncoderDictionary<'static>> = OnceLock::new();
static DECODER_DICT: OnceLock<DecoderDictionary<'static>> = OnceLock::new();

fn encoder_dict() -> &'static EncoderDictionary<'static> {
    ENCODER_DICT.get_or_init(|| EncoderDictionary::copy(RECORD_DICT, COMPRESS_LEVEL))
}

fn decoder_dict() -> &'static DecoderDictionary<'static> {
    DECODER_DICT.get_or_init(|| DecoderDictionary::copy(RECORD_DICT))
}

thread_local! {
    static COMPRESSOR: RefCell<Compressor<'static>> = RefCell::new(
        Compressor::with_prepared_dictionary(encoder_dict()).expect("zstd compressor init"),
    );
    static DECOMPRESSOR: RefCell<Decompressor<'static>> = RefCell::new(
        Decompressor::with_prepared_dictionary(decoder_dict()).expect("zstd decompressor init"),
    );
}

pub trait RecordStore: Send + Sync {
    fn get(&self, uri: &AtUri<DefaultStr>) -> Option<Arc<RecordBody>>;
    fn put(&self, uri: AtUri<DefaultStr>, body: Arc<RecordBody>);
    fn remove(&self, uri: &AtUri<DefaultStr>);
    fn cache_stats(&self) -> Option<CacheStats> {
        None
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CacheStats {
    pub weight: u64,
    pub len: u64,
    pub capacity: u64,
}

pub struct NoopRecordStore;

impl RecordStore for NoopRecordStore {
    fn get(&self, _uri: &AtUri<DefaultStr>) -> Option<Arc<RecordBody>> {
        None
    }
    fn put(&self, _uri: AtUri<DefaultStr>, _body: Arc<RecordBody>) {}
    fn remove(&self, _uri: &AtUri<DefaultStr>) {}
}

#[derive(Clone, Debug)]
pub struct CacheCapacity {
    pub bytes: u64,
    pub estimated_items: usize,
}

impl CacheCapacity {
    pub fn from_bytes(bytes: u64) -> Self {
        const TYPICAL_RECORD_BYTES: u64 = 512;
        let estimated = bytes.div_ceil(TYPICAL_RECORD_BYTES).max(64) as usize;
        Self {
            bytes,
            estimated_items: estimated,
        }
    }
}

#[derive(Clone)]
enum Payload {
    Raw(Bytes),
    Zstd { compressed: Bytes, plain_len: usize },
}

#[derive(Clone)]
struct Stored {
    cid: Cid<DefaultStr>,
    payload: Payload,
}

#[derive(Clone)]
struct ByteWeighter;

impl Weighter<AtUri<DefaultStr>, Stored> for ByteWeighter {
    fn weight(&self, key: &AtUri<DefaultStr>, val: &Stored) -> u64 {
        let payload = match &val.payload {
            Payload::Raw(bytes) => bytes.len(),
            Payload::Zstd { compressed, .. } => compressed.len(),
        };
        ENTRY_OVERHEAD
            + payload as u64
            + key.as_ref().len() as u64
            + val.cid.as_ref().len() as u64
    }
}

pub struct LruRecordStore {
    cache: Cache<AtUri<DefaultStr>, Stored, ByteWeighter>,
}

impl LruRecordStore {
    pub fn new(capacity: CacheCapacity) -> Self {
        Self {
            cache: Cache::with_weighter(capacity.estimated_items, capacity.bytes, ByteWeighter),
        }
    }
}

impl RecordStore for LruRecordStore {
    fn get(&self, uri: &AtUri<DefaultStr>) -> Option<Arc<RecordBody>> {
        let stored = self.cache.get(uri)?;
        let value = match stored.payload {
            Payload::Raw(bytes) => bytes,
            Payload::Zstd {
                compressed,
                plain_len,
            } => {
                let plain = DECOMPRESSOR
                    .with_borrow_mut(|d| d.decompress(&compressed, plain_len))
                    .ok()?;
                Bytes::from(plain)
            }
        };
        Some(Arc::new(RecordBody {
            uri: uri.clone(),
            cid: stored.cid,
            value,
        }))
    }

    fn put(&self, uri: AtUri<DefaultStr>, body: Arc<RecordBody>) {
        let plain_len = body.value.len();
        let payload = match COMPRESSOR.with_borrow_mut(|c| c.compress(&body.value)) {
            Ok(mut compressed) if compressed.len() < plain_len => {
                compressed.shrink_to_fit();
                Payload::Zstd {
                    compressed: Bytes::from(compressed),
                    plain_len,
                }
            }
            _ => Payload::Raw(body.value.clone()),
        };
        self.cache.insert(
            uri,
            Stored {
                cid: body.cid.clone(),
                payload,
            },
        );
    }

    fn remove(&self, uri: &AtUri<DefaultStr>) {
        self.cache.remove(uri);
    }

    fn cache_stats(&self) -> Option<CacheStats> {
        Some(CacheStats {
            weight: self.cache.weight(),
            len: self.cache.len() as u64,
            capacity: self.cache.capacity(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jacquard_common::types::string::Cid;

    fn at(s: &str) -> AtUri<DefaultStr> {
        AtUri::new_owned(s).unwrap()
    }

    fn body(uri: AtUri<DefaultStr>, payload: &[u8]) -> Arc<RecordBody> {
        let cid: Cid<DefaultStr> = "bafyreieqygohnz2zqyvtvktbjpvhutphobcmbsnt4q5lc36ri7vpcmoz4i"
            .parse()
            .unwrap();
        Arc::new(RecordBody {
            uri,
            cid,
            value: Bytes::copy_from_slice(payload),
        })
    }

    fn high_entropy(seed: u64, len: usize) -> Vec<u8> {
        use std::hash::{Hash, Hasher};
        let mut out = Vec::with_capacity(len);
        let mut counter = seed;
        while out.len() < len {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            counter.hash(&mut hasher);
            out.extend_from_slice(&hasher.finish().to_le_bytes());
            counter = counter.wrapping_add(1);
        }
        out.truncate(len);
        out
    }

    #[test]
    fn put_then_get_round_trips_through_compression() {
        let store = LruRecordStore::new(CacheCapacity::from_bytes(64 * 1024));
        let uri = at("at://did:plc:nel/sh.tangled.repo/r1");
        let payload = br#"{"$type":"sh.tangled.repo","name":"oyster","knot":"oyster.cafe"}"#;
        let b = body(uri.clone(), payload);
        store.put(uri.clone(), b.clone());
        let got = store.get(&uri).expect("hit");
        assert_eq!(got.value, b.value, "decompressed body must equal original");
        assert_eq!(got.cid, b.cid);
    }

    #[test]
    fn remove_evicts_entry() {
        let store = LruRecordStore::new(CacheCapacity::from_bytes(64 * 1024));
        let uri = at("at://did:plc:nel/sh.tangled.repo/r1");
        store.put(uri.clone(), body(uri.clone(), br#"{"v":1}"#));
        assert!(store.get(&uri).is_some());
        store.remove(&uri);
        assert!(store.get(&uri).is_none());
    }

    #[test]
    fn miss_returns_none() {
        let store = LruRecordStore::new(CacheCapacity::from_bytes(64 * 1024));
        assert!(
            store
                .get(&at("at://did:plc:abalone/sh.tangled.repo/r1"))
                .is_none()
        );
    }

    #[test]
    fn byte_capacity_evicts_incompressible_entries() {
        let store = LruRecordStore::new(CacheCapacity::from_bytes(2_048));
        let resident = (0..16)
            .map(|i| {
                let uri = at(&format!("at://did:plc:olaren/sh.tangled.string/r{i}"));
                store.put(uri.clone(), body(uri.clone(), &high_entropy(i, 900)));
                uri
            })
            .collect::<Vec<_>>()
            .iter()
            .filter(|uri| store.get(uri).is_some())
            .count();
        assert!(
            resident < 16,
            "byte cap must evict incompressible bodies, observed {resident} of 16 resident"
        );
    }

    #[test]
    fn capacity_estimates_items_floor() {
        let cap = CacheCapacity::from_bytes(0);
        assert_eq!(cap.estimated_items, 64, "tiny caches still need a floor");
    }

    #[test]
    fn incompressible_body_round_trips_via_raw() {
        let store = LruRecordStore::new(CacheCapacity::from_bytes(64 * 1024));
        let uri = at("at://did:plc:olaren/sh.tangled.string/r1");
        let payload = high_entropy(7, 300);
        let stored = body(uri.clone(), &payload);
        store.put(uri.clone(), stored.clone());
        let got = store.get(&uri).expect("hit");
        assert_eq!(
            got.value, stored.value,
            "incompressible body must round-trip byte-exact through the raw path"
        );
    }
}
