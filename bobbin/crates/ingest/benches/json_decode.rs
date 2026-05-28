use std::hint::black_box;

use bobbin_types::edges::Record;
use criterion::{Criterion, criterion_group, criterion_main};
use jacquard_common::DefaultStr;
use jacquard_common::types::nsid::Nsid;
use serde::Deserialize;
use serde_json::value::RawValue;

#[derive(Deserialize)]
struct ValueFrame {
    record: ValueRecordFrame,
}

#[derive(Deserialize)]
struct ValueRecordFrame {
    collection: Nsid<DefaultStr>,
    #[serde(default)]
    record: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct BorrowedRawFrame<'a> {
    #[serde(borrow)]
    record: BorrowedRawRecordFrame<'a>,
}

#[derive(Deserialize)]
struct BorrowedRawRecordFrame<'a> {
    collection: Nsid<DefaultStr>,
    #[serde(borrow, default)]
    record: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct OwnedRawFrame {
    record: OwnedRawRecordFrame,
}

#[derive(Deserialize)]
struct OwnedRawRecordFrame {
    collection: Nsid<DefaultStr>,
    #[serde(default)]
    record: Option<Box<RawValue>>,
}

fn current_path(text: &str) -> Record {
    let frame: ValueFrame = serde_json::from_str(text).expect("frame");
    let value = frame.record.record.expect("upsert body");
    let bytes = serde_json::to_vec(&value).expect("re-serialize Value");
    Record::from_json_bytes(&frame.record.collection, &bytes).expect("decode record")
}

fn raw_value_borrowed(text: &str) -> Record {
    let frame: BorrowedRawFrame<'_> = serde_json::from_str(text).expect("frame");
    let raw = frame.record.record.expect("upsert body");
    Record::from_json_bytes(&frame.record.collection, raw.get().as_bytes()).expect("decode record")
}

fn raw_value_owned(text: &str) -> Record {
    let frame: OwnedRawFrame = serde_json::from_str(text).expect("frame");
    let raw = frame.record.record.expect("upsert body");
    Record::from_json_bytes(&frame.record.collection, raw.get().as_bytes()).expect("decode record")
}

fn floor_record_only(collection: &Nsid<DefaultStr>, record_bytes: &[u8]) -> Record {
    Record::from_json_bytes(collection, record_bytes).expect("decode record")
}

fn star_text() -> &'static str {
    r#"{
        "id": 1,
        "type": "record",
        "record": {
            "live": false,
            "did": "did:plc:olaren",
            "rev": "3lq2zk5wqsh2k",
            "collection": "sh.tangled.feed.star",
            "rkey": "abcabcabcabcz",
            "action": "create",
            "record": {
                "$type": "sh.tangled.feed.star",
                "createdAt": "2026-05-01T00:00:00Z",
                "subject": {
                    "$type": "sh.tangled.feed.star#repo",
                    "did": "did:plc:limpet"
                }
            }
        }
    }"#
}

fn repo_text() -> &'static str {
    r#"{
        "id": 2,
        "type": "record",
        "record": {
            "live": false,
            "did": "did:plc:nel",
            "rev": "3lq2zk5wqsh2l",
            "collection": "sh.tangled.repo",
            "rkey": "3lq2zk5wq0001",
            "action": "create",
            "record": {
                "$type": "sh.tangled.repo",
                "createdAt": "2026-05-01T00:00:00Z",
                "knot": "knot.witchcraft.systems",
                "name": "limpet",
                "description": "demonstration repository for the bench corpus",
                "owner": "did:plc:nel",
                "repoDid": "did:plc:limpet",
                "labels": [
                    "at://did:plc:periwinkle/sh.tangled.label.definition/3lq2zk5wq0010",
                    "at://did:plc:periwinkle/sh.tangled.label.definition/3lq2zk5wq0011"
                ]
            }
        }
    }"#
}

fn issue_text() -> &'static str {
    r#"{
        "id": 3,
        "type": "record",
        "record": {
            "live": false,
            "did": "did:plc:teq",
            "rev": "3lq2zk5wqsh2m",
            "collection": "sh.tangled.repo.issue",
            "rkey": "3lq2zk5wq0100",
            "action": "create",
            "record": {
                "$type": "sh.tangled.repo.issue",
                "createdAt": "2026-05-01T00:00:00Z",
                "title": "ingest: single-pass JSON decode follow-up corpus entry",
                "body": "Long body to give the bench a realistic decode cost. Repeats: blahhhhh meow meow aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "repo": "did:plc:limpet",
                "mentions": [
                    "did:plc:nel",
                    "did:plc:olaren",
                    "did:plc:bailey"
                ],
                "references": [
                    "at://did:plc:limpet/sh.tangled.repo.issue/3lq2zk5wq0099",
                    "at://did:plc:limpet/sh.tangled.repo.pull/3lq2zk5wq0098"
                ]
            }
        }
    }"#
}

fn pull_comment_text() -> &'static str {
    r#"{
        "id": 4,
        "type": "record",
        "record": {
            "live": false,
            "did": "did:plc:lyna",
            "rev": "3lq2zk5wqsh2n",
            "collection": "sh.tangled.feed.comment",
            "rkey": "3lq2zk5wq0200",
            "action": "create",
            "record": {
                "$type": "sh.tangled.feed.comment",
                "createdAt": "2026-05-01T00:00:00Z",
                "body": {
                    "$type": "sh.tangled.markup.markdown",
                    "text": "lgtm i thinks!!!! but please verify the cursor invariant under buffered(N) before landing. :3"
                },
                "subject": {
                    "uri": "at://did:plc:limpet/sh.tangled.repo.pull/3lq2zk5wq0098",
                    "cid": "bafkqaaa"
                },
                "pullRoundIdx": 0
            }
        }
    }"#
}

fn follow_text() -> &'static str {
    r#"{
        "id": 5,
        "type": "record",
        "record": {
            "live": false,
            "did": "did:plc:bailey",
            "rev": "3lq2zk5wqsh2o",
            "collection": "sh.tangled.graph.follow",
            "rkey": "3lq2zk5wq0300",
            "action": "create",
            "record": {
                "$type": "sh.tangled.graph.follow",
                "createdAt": "2026-05-01T00:00:00Z",
                "subject": "did:plc:nel"
            }
        }
    }"#
}

fn extract_record_slice(text: &str) -> (Nsid<DefaultStr>, Vec<u8>) {
    let frame: ValueFrame = serde_json::from_str(text).expect("frame");
    let value = frame.record.record.expect("upsert body");
    let bytes = serde_json::to_vec(&value).expect("re-serialize Value");
    (frame.record.collection, bytes)
}

fn bench_decode(c: &mut Criterion) {
    let corpus: &[(&str, &str)] = &[
        ("star", star_text()),
        ("repo", repo_text()),
        ("issue", issue_text()),
        ("pull_comment", pull_comment_text()),
        ("follow", follow_text()),
    ];
    for (name, text) in corpus {
        let mut group = c.benchmark_group(format!("decode/{name}"));
        let (collection, record_bytes) = extract_record_slice(text);

        group.bench_function("current_path", |b| {
            b.iter(|| {
                let r = current_path(black_box(text));
                black_box(r);
            });
        });

        group.bench_function("raw_value_borrowed", |b| {
            b.iter(|| {
                let r = raw_value_borrowed(black_box(text));
                black_box(r);
            });
        });

        group.bench_function("raw_value_owned", |b| {
            b.iter(|| {
                let r = raw_value_owned(black_box(text));
                black_box(r);
            });
        });

        group.bench_function("floor_record_only", |b| {
            let bytes = record_bytes.as_slice();
            b.iter(|| {
                let r = floor_record_only(black_box(&collection), black_box(bytes));
                black_box(r);
            });
        });

        group.finish();
    }
}

criterion_group!(benches, bench_decode);
criterion_main!(benches);
