use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use bobbin_edge_index::{EdgeStore, IssueStateKind, PullStatusKind, StateIndex};
use bobbin_record_lru::RecordStore;
use bobbin_search::SearchIndex;
use serde::Serialize;
use tikv_jemalloc_ctl::{epoch, stats};

#[derive(Clone)]
pub struct MemProbe {
    pub edges: Arc<EdgeStore>,
    pub search: Arc<SearchIndex>,
    pub records: Arc<dyn RecordStore>,
    pub issue_states: Arc<StateIndex<IssueStateKind>>,
    pub pull_statuses: Arc<StateIndex<PullStatusKind>>,
}

pub fn debug_router(probe: MemProbe) -> Router {
    Router::new()
        .route("/debug/mem", get(mem_report))
        .route("/debug/heap", get(heap_dump))
        .with_state(probe)
}

#[derive(Serialize)]
struct Jemalloc {
    allocated: u64,
    active: u64,
    resident: u64,
    retained: u64,
    mapped: u64,
}

#[derive(Serialize)]
struct Tantivy {
    num_docs: u64,
    num_segments: u64,
    total_bytes: u64,
}

#[derive(Serialize)]
struct HistBin {
    le: u64,
    count: u64,
}

#[derive(Serialize)]
struct Edges {
    key_count: u64,
    source_count: u64,
    source_interner_bytes: u64,
    did_interner_bytes: u64,
    collection_interner_bytes: u64,
    key_interner_bytes: u64,
    edges_total: u64,
    author_refs_total: u64,
    reverse_entries: u64,
    reverse_cap: u64,
    forward_struct_bytes: u64,
    reverse_struct_bytes: u64,
    bucket_struct_bytes: u64,
    max_bucket: u64,
    bucket_histogram: Vec<HistBin>,
}

#[derive(Serialize)]
struct StateIdx {
    issue_entities: u64,
    issue_sources: u64,
    pull_entities: u64,
    pull_sources: u64,
}

#[derive(Serialize)]
struct Lru {
    weight: u64,
    len: u64,
    capacity: u64,
}

#[derive(Serialize)]
struct Derived {
    known_bytes: u64,
    allocated_minus_known: u64,
}

#[derive(Serialize)]
struct MemSnapshot {
    jemalloc: Jemalloc,
    tantivy: Tantivy,
    edges: Edges,
    state: StateIdx,
    lru: Lru,
    derived: Derived,
}

async fn mem_report(State(p): State<MemProbe>) -> Json<MemSnapshot> {
    let _ = epoch::advance();
    let allocated = stats::allocated::read().unwrap_or(0) as u64;
    let active = stats::active::read().unwrap_or(0) as u64;
    let resident = stats::resident::read().unwrap_or(0) as u64;
    let retained = stats::retained::read().unwrap_or(0) as u64;
    let mapped = stats::mapped::read().unwrap_or(0) as u64;

    let su = p.search.space_usage();
    let er = p.edges.mem_report();
    let lru = p.records.cache_stats().unwrap_or_default();

    let known_bytes = su.total_bytes
        + er.source_interner_bytes
        + er.did_interner_bytes
        + er.collection_interner_bytes
        + er.key_interner_bytes
        + er.forward_struct_bytes
        + er.reverse_struct_bytes
        + lru.weight;

    Json(MemSnapshot {
        jemalloc: Jemalloc {
            allocated,
            active,
            resident,
            retained,
            mapped,
        },
        tantivy: Tantivy {
            num_docs: su.num_docs,
            num_segments: su.num_segments,
            total_bytes: su.total_bytes,
        },
        edges: Edges {
            key_count: er.key_count,
            source_count: er.source_count,
            source_interner_bytes: er.source_interner_bytes,
            did_interner_bytes: er.did_interner_bytes,
            collection_interner_bytes: er.collection_interner_bytes,
            key_interner_bytes: er.key_interner_bytes,
            edges_total: er.edges_total,
            author_refs_total: er.author_refs_total,
            reverse_entries: er.reverse_entries,
            reverse_cap: er.reverse_cap,
            forward_struct_bytes: er.forward_struct_bytes,
            reverse_struct_bytes: er.reverse_struct_bytes,
            bucket_struct_bytes: er.bucket_struct_bytes,
            max_bucket: er.max_bucket,
            bucket_histogram: er
                .bucket_histogram()
                .map(|(le, count)| HistBin { le, count })
                .collect(),
        },
        state: StateIdx {
            issue_entities: p.issue_states.entity_count() as u64,
            issue_sources: p.issue_states.source_count() as u64,
            pull_entities: p.pull_statuses.entity_count() as u64,
            pull_sources: p.pull_statuses.source_count() as u64,
        },
        lru: Lru {
            weight: lru.weight,
            len: lru.len,
            capacity: lru.capacity,
        },
        derived: Derived {
            known_bytes,
            allocated_minus_known: allocated.saturating_sub(known_bytes),
        },
    })
}

async fn heap_dump() -> (StatusCode, String) {
    let dump = unsafe {
        tikv_jemalloc_ctl::raw::write(b"prof.dump\0", core::ptr::null::<core::ffi::c_char>())
    };
    match dump {
        Ok(()) => (StatusCode::OK, "dumped to prof_prefix".to_owned()),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "prof.dump failed, is the binary built with profiling and run with prof:true: {e}"
            ),
        ),
    }
}
