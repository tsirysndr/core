use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use knot_types::OfferedKey;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct Cursor(String);

impl Cursor {
    #[cfg(test)]
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KeyParseError {
    #[error("public key line is missing algorithm and blob")]
    Incomplete,
    #[error("public key blob isn't valid base64: {0}")]
    Base64(String),
    #[error("public key blob is truncated")]
    Truncated,
    #[error("declared algorithm {declared:?} doesn't match blob's {embedded:?}")]
    AlgorithmMismatch { declared: String, embedded: String },
}

pub fn parse_authorized_key(line: &str) -> Result<OfferedKey, KeyParseError> {
    let parts: Vec<&str> = line.split_whitespace().take(2).collect();
    let [algo, blob_b64] = parts.as_slice() else {
        return Err(KeyParseError::Incomplete);
    };
    let blob = STANDARD
        .decode(blob_b64)
        .map_err(|error| KeyParseError::Base64(error.to_string()))?;
    let embedded = embedded_algorithm(&blob)?;
    if embedded != algo.as_bytes() {
        return Err(KeyParseError::AlgorithmMismatch {
            declared: (*algo).to_string(),
            embedded: String::from_utf8_lossy(embedded).into_owned(),
        });
    }
    Ok(OfferedKey::from_bytes(blob))
}

fn embedded_algorithm(blob: &[u8]) -> Result<&[u8], KeyParseError> {
    let length = blob
        .get(..4)
        .map(|head| u32::from_be_bytes(head.try_into().expect("four bytes")) as usize)
        .ok_or(KeyParseError::Truncated)?;
    let end = length.checked_add(4).ok_or(KeyParseError::Truncated)?;
    blob.get(4..end).ok_or(KeyParseError::Truncated)
}

#[derive(Deserialize)]
struct ListRecords {
    records: Vec<Envelope>,
    #[serde(default)]
    cursor: Option<Cursor>,
}

#[derive(Deserialize)]
struct Envelope {
    value: KeyRecord,
}

#[derive(Deserialize)]
struct KeyRecord {
    key: String,
}

pub(crate) struct PubkeyPage {
    pub keys: Vec<OfferedKey>,
    pub cursor: Option<Cursor>,
}

pub(crate) fn offered_page(body: &[u8], max_keys: usize) -> Result<PubkeyPage, serde_json::Error> {
    let listing: ListRecords = serde_json::from_slice(body)?;
    let keys = listing
        .records
        .iter()
        .take(max_keys)
        .filter_map(|envelope| parse_authorized_key(&envelope.value.key).ok())
        .collect();
    Ok(PubkeyPage {
        keys,
        cursor: listing.cursor.filter(|cursor| !cursor.as_str().is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    fn ssh_string(bytes: &[u8]) -> Vec<u8> {
        [&(bytes.len() as u32).to_be_bytes()[..], bytes].concat()
    }

    fn wire_blob(material: &[u8]) -> OfferedKey {
        OfferedKey::from_bytes([ssh_string(b"ssh-ed25519"), ssh_string(material)].concat())
    }

    struct KeyCase {
        name: &'static str,
        line: fn() -> String,
        expect: fn(&Result<OfferedKey, KeyParseError>) -> bool,
    }

    const KEY_CASES: &[KeyCase] = &[
        KeyCase {
            name: "genuine ed25519 key with a trailing comment",
            line: || ssh_line("ssh-ed25519", &[7u8; 32], "nel@oyster.cafe"),
            expect: |r| matches!(r, Ok(key) if *key == wire_blob(&[7u8; 32])),
        },
        KeyCase {
            name: "the same key material with no comment",
            line: || ssh_line("ssh-ed25519", &[7u8; 32], ""),
            expect: |r| matches!(r, Ok(key) if *key == wire_blob(&[7u8; 32])),
        },
        KeyCase {
            name: "bare algorithm with no blob",
            line: || "ssh-ed25519".to_string(),
            expect: |r| matches!(r, Err(KeyParseError::Incomplete)),
        },
        KeyCase {
            name: "whitespace-only line",
            line: || "   ".to_string(),
            expect: |r| matches!(r, Err(KeyParseError::Incomplete)),
        },
        KeyCase {
            name: "blob that isn't base64",
            line: || "ssh-ed25519 not-base64!!!".to_string(),
            expect: |r| matches!(r, Err(KeyParseError::Base64(_))),
        },
        KeyCase {
            name: "declared algorithm lying about the blob",
            line: || {
                let blob = [ssh_string(b"ssh-ed25519"), ssh_string(&[1u8; 32])].concat();
                format!("ssh-rsa {}", STANDARD.encode(blob))
            },
            expect: |r| matches!(r, Err(KeyParseError::AlgorithmMismatch { .. })),
        },
        KeyCase {
            name: "authorized_keys options prefix",
            line: || {
                let blob = [ssh_string(b"ssh-ed25519"), ssh_string(&[7u8; 32])].concat();
                format!(
                    "command=\"true\",no-pty ssh-ed25519 {} nel@oyster.cafe",
                    STANDARD.encode(blob)
                )
            },
            expect: |r| r.is_err(),
        },
        KeyCase {
            name: "overlong length prefix",
            line: || {
                let lying = [&u32::MAX.to_be_bytes()[..], b"short"].concat();
                format!("ssh-ed25519 {}", STANDARD.encode(lying))
            },
            expect: |r| matches!(r, Err(KeyParseError::Truncated)),
        },
    ];

    #[test]
    fn parse_authorized_key_accepts_genuine_lines_and_rejects_malformed_ones() {
        KEY_CASES.iter().for_each(|case| {
            let result = parse_authorized_key(&(case.line)());
            assert!(
                (case.expect)(&result),
                "case {:?} got {result:?}",
                case.name
            );
        });
    }

    #[test]
    fn list_records_yields_every_well_formed_key_and_skips_the_rest() {
        let good_one = ssh_line("ssh-ed25519", &[1u8; 32], "one");
        let good_two = ssh_line("ssh-ed25519", &[2u8; 32], "two");
        let body = serde_json::json!({
            "records": [
                { "uri": "at://did:plc:squid/sh.tangled.publicKey/a", "value": { "$type": "sh.tangled.publicKey", "key": good_one, "name": "laptop", "createdAt": "2026-06-08T00:00:00Z" } },
                { "uri": "at://did:plc:squid/sh.tangled.publicKey/b", "value": { "$type": "sh.tangled.publicKey", "key": "garbage line", "name": "broken", "createdAt": "2026-06-08T00:00:00Z" } },
                { "uri": "at://did:plc:squid/sh.tangled.publicKey/c", "value": { "$type": "sh.tangled.publicKey", "key": good_two, "name": "desktop", "createdAt": "2026-06-08T00:00:00Z" } }
            ],
            "cursor": "c"
        });
        let page = offered_page(serde_json::to_vec(&body).unwrap().as_slice(), 100).unwrap();
        assert_eq!(page.keys.len(), 2);
        assert_eq!(page.keys[0], parse_authorized_key(&good_one).unwrap());
        assert_eq!(page.keys[1], parse_authorized_key(&good_two).unwrap());
        assert_eq!(page.cursor.as_ref().map(Cursor::as_str), Some("c"));
    }

    #[test]
    fn a_cursor_serializes_as_a_plain_json_string() {
        let cursor = Cursor::new("page-token");
        assert_eq!(cursor.as_str(), "page-token");
        assert_eq!(serde_json::to_string(&cursor).unwrap(), "\"page-token\"");
        let parsed: Cursor = serde_json::from_str("\"page-token\"").unwrap();
        assert_eq!(parsed, cursor);
    }

    #[test]
    fn a_page_is_bounded_by_the_requested_record_limit() {
        let records: Vec<_> = (0u32..50)
            .map(|seed| {
                let mut material = [0u8; 32];
                material[..4].copy_from_slice(&seed.to_be_bytes());
                let line = ssh_line("ssh-ed25519", &material, "k");
                serde_json::json!({
                    "uri": "at://did:plc:squid/sh.tangled.publicKey/x",
                    "value": { "$type": "sh.tangled.publicKey", "key": line, "name": "k", "createdAt": "2026-06-08T00:00:00Z" }
                })
            })
            .collect();
        let body = serde_json::json!({ "records": records });
        let page = offered_page(serde_json::to_vec(&body).unwrap().as_slice(), 10).unwrap();
        assert_eq!(
            page.keys.len(),
            10,
            "page yields at most requested record limit, no matter how many the PDS returns"
        );
    }
}
