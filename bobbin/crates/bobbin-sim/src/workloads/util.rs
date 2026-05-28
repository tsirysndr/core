use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bobbin_runtime::{HttpRequest, MemHttpBody, MemHttpResponder, MemHttpResponse};
use http::StatusCode;
use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use jacquard_common::types::nsid::Nsid;
use jacquard_common::types::recordkey::Rkey;
use url::Url;

const ALPHABET_RKEY: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
const ALPHABET_TID: &[u8] = b"234567abcdefghijklmnopqrstuvwxyz";
const ENCODED_LEN: usize = 13;

pub fn format_rkey(i: usize) -> String {
    encode_padded(i.saturating_add(1), ALPHABET_RKEY, b'a')
}

pub fn format_tid(i: usize) -> String {
    encode_padded(i.saturating_add(1), ALPHABET_TID, b'2')
}

fn encode_padded(mut idx: usize, alphabet: &[u8], pad: u8) -> String {
    let mut buf = [pad; ENCODED_LEN];
    let mut pos = buf.len();
    while idx > 0 && pos > 0 {
        pos -= 1;
        buf[pos] = alphabet[idx % alphabet.len()];
        idx /= alphabet.len();
    }
    String::from_utf8(buf.to_vec()).unwrap()
}

pub fn parse_repo_lookup(
    url: &Url,
    expected_collection: &Nsid<DefaultStr>,
) -> Option<(Did<DefaultStr>, Rkey<DefaultStr>)> {
    let mut repo: Option<Did<DefaultStr>> = None;
    let mut collection: Option<Nsid<DefaultStr>> = None;
    let mut rkey: Option<Rkey<DefaultStr>> = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "repo" => repo = Did::new_owned(&v).ok(),
            "collection" => collection = Nsid::new_owned(&v).ok(),
            "rkey" => rkey = Rkey::new_owned(&v).ok(),
            _ => {}
        }
    }
    if collection.as_ref()? != expected_collection {
        return None;
    }
    Some((repo?, rkey?))
}

#[derive(Clone, Default)]
pub struct AssertNoSlingshot {
    calls: Arc<AtomicU64>,
}

impl AssertNoSlingshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
}

impl MemHttpResponder for AssertNoSlingshot {
    fn respond(&self, _: &HttpRequest) -> MemHttpResponse {
        self.calls.fetch_add(1, Ordering::Relaxed);
        MemHttpResponse {
            latency: Duration::ZERO,
            result: Ok(MemHttpBody::status_only(StatusCode::NOT_FOUND)),
        }
    }
}
