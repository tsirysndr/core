use std::future::Future;
use std::ops::Bound;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bobbin_runtime::Clock;
use bobbin_types::search::{SearchDoc, SearchSink};
use jacquard_common::DefaultStr;
use jacquard_common::types::nsid::Nsid;
use jacquard_common::types::string::{AtUri, Did};
use tantivy::collector::TopDocs;
use tantivy::query::{
    BooleanQuery, Occur, Query, QueryParser, QueryParserError, RangeQuery, TermQuery,
};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, TEXT, TextFieldIndexing,
    TextOptions, Value,
};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, TantivyError, Term};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

pub const DEFAULT_WRITER_HEAP_BYTES: usize = 50_000_000;
const BATCH_SIZE: usize = 200;
const BATCH_INTERVAL: Duration = Duration::from_millis(250);
const TITLE_BOOST: f32 = 4.0;
const OFFSET_TOKEN_LEN: usize = 8;
const WRITE_QUEUE_CAPACITY: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SearchOffset(u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum SearchOffsetError {
    #[error("offset token must be {OFFSET_TOKEN_LEN} hex characters")]
    Malformed,
}

impl SearchOffset {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    pub fn encode_token(self) -> String {
        format!("{:0width$x}", self.0, width = OFFSET_TOKEN_LEN)
    }

    pub fn decode_token(token: &str) -> Result<Self, SearchOffsetError> {
        if token.len() != OFFSET_TOKEN_LEN || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(SearchOffsetError::Malformed);
        }
        u32::from_str_radix(token, 16)
            .map(Self)
            .map_err(|_| SearchOffsetError::Malformed)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchCursor {
    Start,
    At(SearchOffset),
}

impl SearchCursor {
    pub fn from_token(raw: Option<&str>) -> Result<Self, SearchOffsetError> {
        raw.map_or(Ok(Self::Start), |t| {
            SearchOffset::decode_token(t).map(Self::At)
        })
    }

    fn offset(self) -> u32 {
        match self {
            Self::Start => 0,
            Self::At(o) => o.raw(),
        }
    }
}

#[derive(Debug)]
pub struct SearchHit {
    pub uri: AtUri<DefaultStr>,
    pub nsid: Nsid<DefaultStr>,
    pub score: f32,
}

#[derive(Debug)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub next: Option<SearchOffset>,
}

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("tantivy: {0}")]
    Tantivy(#[from] TantivyError),
    #[error("query parse: {0}")]
    Query(#[from] QueryParserError),
    #[error("indexed uri is invalid: {0}")]
    InvalidUri(String),
    #[error("indexed nsid is invalid: {0}")]
    InvalidNsid(String),
    #[error("indexed document missing field: {0}")]
    MissingField(&'static str),
    #[error("search blocking task cancelled: {0}")]
    Cancelled(String),
}

#[derive(Clone, Copy)]
struct Fields {
    uri: Field,
    nsid: Field,
    title: Field,
    body: Field,
    author: Field,
    created_at: Field,
    repo: Field,
}

#[derive(Clone, Debug, Default)]
pub struct SearchFilters {
    pub nsid: Option<Nsid<DefaultStr>>,
    pub author: Option<Did<DefaultStr>>,
    pub repo: Option<Did<DefaultStr>>,
    pub since: Option<i64>,
    pub until: Option<i64>,
}

impl SearchFilters {
    pub fn is_empty(&self) -> bool {
        self.nsid.is_none()
            && self.author.is_none()
            && self.repo.is_none()
            && self.since.is_none()
            && self.until.is_none()
    }
}

enum WriteOp {
    Upsert(SearchDoc),
    Remove(AtUri<DefaultStr>),
    Flush(oneshot::Sender<()>),
}

pub struct SearchIndex {
    inner: Arc<Inner>,
}

struct Inner {
    fields: Fields,
    reader: IndexReader,
    parser: QueryParser,
    tx: mpsc::Sender<WriteOp>,
}

impl SearchIndex {
    pub fn new(heap_bytes: usize, clock: Arc<dyn Clock>) -> Result<Self, SearchError> {
        let mut sb = Schema::builder();
        let uri = sb.add_text_field("uri", STRING | STORED);
        let nsid = sb.add_text_field("nsid", STRING | STORED);
        let title_indexing = TextFieldIndexing::default()
            .set_tokenizer("default")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions);
        let title_opts = TextOptions::default()
            .set_indexing_options(title_indexing)
            .set_stored();
        let title = sb.add_text_field("title", title_opts);
        let body = sb.add_text_field("body", TEXT);
        let author = sb.add_text_field("author", STRING | STORED);
        let created_at = sb.add_i64_field("created_at", INDEXED | FAST | STORED);
        let repo = sb.add_text_field("repo", STRING | STORED);
        let schema = sb.build();
        let index = Index::create_in_ram(schema);
        let writer: IndexWriter = index.writer(heap_bytes)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let mut parser = QueryParser::for_index(&index, vec![title, body]);
        parser.set_field_boost(title, TITLE_BOOST);
        let fields = Fields {
            uri,
            nsid,
            title,
            body,
            author,
            created_at,
            repo,
        };
        let (tx, rx) = mpsc::channel(WRITE_QUEUE_CAPACITY);
        let writer_reader = reader.clone();
        tokio::spawn(writer_loop(writer, fields, writer_reader, rx, clock));
        Ok(Self {
            inner: Arc::new(Inner {
                fields,
                reader,
                parser,
                tx,
            }),
        })
    }

    pub async fn flush(&self) {
        let (done_tx, done_rx) = oneshot::channel();
        if self.inner.tx.send(WriteOp::Flush(done_tx)).await.is_err() {
            warn!("search writer channel closed before flush");
            return;
        }
        if done_rx.await.is_err() {
            warn!("search writer dropped before completing flush");
        }
    }

    pub async fn search(
        &self,
        q: &str,
        filters: SearchFilters,
        cursor: SearchCursor,
        limit: u32,
    ) -> Result<SearchPage, SearchError> {
        let trimmed = q.trim();
        if trimmed.is_empty() {
            return Ok(SearchPage {
                hits: Vec::new(),
                next: None,
            });
        }
        let inner = self.inner.clone();
        let q_owned = trimmed.to_owned();
        let limit_usize = limit as usize;
        let offset = cursor.offset() as usize;
        match tokio::task::spawn_blocking(move || {
            inner.search_blocking(&q_owned, &filters, offset, limit_usize)
        })
        .await
        {
            Ok(result) => result,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(e) => Err(SearchError::Cancelled(e.to_string())),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SearchSpaceUsage {
    pub num_docs: u64,
    pub num_segments: u64,
    pub total_bytes: u64,
}

impl SearchIndex {
    pub fn space_usage(&self) -> SearchSpaceUsage {
        let searcher = self.inner.reader.searcher();
        let segments = searcher.segment_readers();
        let total_bytes = segments
            .iter()
            .map(|s| s.space_usage().map(|u| u.total().get_bytes()).unwrap_or(0))
            .sum();
        SearchSpaceUsage {
            num_docs: searcher.num_docs(),
            num_segments: segments.len() as u64,
            total_bytes,
        }
    }
}

pub type SearchReadFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SearchPage, SearchError>> + Send + 'a>>;

pub trait SearchReader: Send + Sync + 'static {
    fn search<'a>(
        &'a self,
        query: &'a str,
        filters: SearchFilters,
        cursor: SearchCursor,
        limit: u32,
    ) -> SearchReadFuture<'a>;
}

impl SearchReader for SearchIndex {
    fn search<'a>(
        &'a self,
        query: &'a str,
        filters: SearchFilters,
        cursor: SearchCursor,
        limit: u32,
    ) -> SearchReadFuture<'a> {
        Box::pin(SearchIndex::search(self, query, filters, cursor, limit))
    }
}

impl Inner {
    fn search_blocking(
        &self,
        q: &str,
        filters: &SearchFilters,
        offset: usize,
        limit: usize,
    ) -> Result<SearchPage, SearchError> {
        let parsed = self.parser.parse_query(q)?;
        let final_query: Box<dyn Query> = if filters.is_empty() {
            parsed
        } else {
            let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(5);
            clauses.push((Occur::Must, parsed));
            if let Some(n) = &filters.nsid {
                let term = Term::from_field_text(self.fields.nsid, n.as_ref());
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
                ));
            }
            if let Some(a) = &filters.author {
                let term = Term::from_field_text(self.fields.author, a.as_ref());
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
                ));
            }
            if let Some(r) = &filters.repo {
                let term = Term::from_field_text(self.fields.repo, r.as_ref());
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
                ));
            }
            if filters.since.is_some() || filters.until.is_some() {
                let lower = filters.since.map_or(Bound::Unbounded, |s| {
                    Bound::Included(Term::from_field_i64(self.fields.created_at, s))
                });
                let upper = filters.until.map_or(Bound::Unbounded, |u| {
                    Bound::Excluded(Term::from_field_i64(self.fields.created_at, u))
                });
                clauses.push((Occur::Must, Box::new(RangeQuery::new(lower, upper))));
            }
            Box::new(BooleanQuery::new(clauses))
        };
        let collector = TopDocs::with_limit(limit + 1)
            .and_offset(offset)
            .order_by_score();
        let searcher = self.reader.searcher();
        let raw_hits: Vec<(f32, tantivy::DocAddress)> =
            searcher.search(&final_query, &collector)?;
        let has_more = raw_hits.len() > limit;
        let page_slice = &raw_hits[..raw_hits.len().min(limit)];
        let hits = page_slice
            .iter()
            .map(|(score, addr)| {
                let doc: TantivyDocument = searcher.doc(*addr)?;
                let uri_str = stored_text(&doc, self.fields.uri, "uri")?;
                let nsid_str = stored_text(&doc, self.fields.nsid, "nsid")?;
                let uri = AtUri::<DefaultStr>::new_owned(uri_str.as_str())
                    .map_err(|e| SearchError::InvalidUri(format!("{e}: {uri_str}")))?;
                let nsid = Nsid::<DefaultStr>::new_owned(nsid_str.as_str())
                    .map_err(|e| SearchError::InvalidNsid(format!("{e}: {nsid_str}")))?;
                Ok::<_, SearchError>(SearchHit {
                    uri,
                    nsid,
                    score: *score,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(SearchPage {
            hits,
            next: next_cursor(offset, limit, has_more),
        })
    }
}

fn next_cursor(offset: usize, limit: usize, has_more: bool) -> Option<SearchOffset> {
    has_more
        .then(|| u32::try_from(offset.saturating_add(limit)).ok())
        .flatten()
        .map(SearchOffset::new)
}

async fn writer_loop(
    mut writer: IndexWriter,
    fields: Fields,
    reader: IndexReader,
    mut rx: mpsc::Receiver<WriteOp>,
    clock: Arc<dyn Clock>,
) {
    let mut pending: usize = 0;
    let mut deadline = clock.now_instant() + BATCH_INTERVAL;
    loop {
        let outcome = tokio::select! {
            biased;
            msg = rx.recv() => RecvOutcome::Message(msg),
            _ = clock.sleep_until(deadline) => RecvOutcome::Idle,
        };
        match outcome {
            RecvOutcome::Message(Some(WriteOp::Upsert(doc))) => {
                apply_upsert(&mut writer, fields, doc);
                pending += 1;
            }
            RecvOutcome::Message(Some(WriteOp::Remove(uri))) => {
                apply_remove(&mut writer, fields, &uri);
                pending += 1;
            }
            RecvOutcome::Message(Some(WriteOp::Flush(done))) => {
                if pending > 0 {
                    commit_and_reload(&mut writer, &reader);
                    pending = 0;
                }
                let _ = done.send(());
                deadline = clock.now_instant() + BATCH_INTERVAL;
                continue;
            }
            RecvOutcome::Message(None) => break,
            RecvOutcome::Idle => {
                if pending > 0 {
                    commit_and_reload(&mut writer, &reader);
                    pending = 0;
                }
                deadline = clock.now_instant() + BATCH_INTERVAL;
                continue;
            }
        }
        if pending >= BATCH_SIZE {
            commit_and_reload(&mut writer, &reader);
            pending = 0;
        }
    }
    if pending > 0 {
        commit_and_reload(&mut writer, &reader);
    }
}

enum RecvOutcome {
    Message(Option<WriteOp>),
    Idle,
}

fn apply_upsert(writer: &mut IndexWriter, fields: Fields, doc: SearchDoc) {
    let uri_term = Term::from_field_text(fields.uri, doc.uri.as_ref());
    writer.delete_term(uri_term);
    let mut td = TantivyDocument::default();
    td.add_text(fields.uri, doc.uri.as_ref());
    td.add_text(fields.nsid, doc.nsid.as_ref());
    td.add_text(fields.title, &doc.title);
    td.add_text(fields.body, &doc.body);
    if let Some(author) = &doc.author {
        td.add_text(fields.author, author.as_ref());
    }
    if let Some(ts) = doc.created_at {
        td.add_i64(fields.created_at, ts);
    }
    if let Some(repo) = &doc.repo {
        td.add_text(fields.repo, repo.as_ref());
    }
    if let Err(e) = writer.add_document(td) {
        warn!(?e, "search add_document failed");
    }
}

fn apply_remove(writer: &mut IndexWriter, fields: Fields, uri: &AtUri<DefaultStr>) {
    let term = Term::from_field_text(fields.uri, uri.as_ref());
    writer.delete_term(term);
}

fn commit_and_reload(writer: &mut IndexWriter, reader: &IndexReader) {
    match writer.commit() {
        Ok(_) => {
            if let Err(e) = reader.reload() {
                warn!(?e, "search reader reload failed");
            }
        }
        Err(e) => warn!(?e, "search index commit failed"),
    }
}

fn stored_text(
    doc: &TantivyDocument,
    field: Field,
    name: &'static str,
) -> Result<String, SearchError> {
    doc.get_first(field)
        .and_then(|v| v.as_str().map(|s| s.to_owned()))
        .ok_or(SearchError::MissingField(name))
}

impl SearchSink for SearchIndex {
    async fn upsert(&self, doc: SearchDoc) {
        if self.inner.tx.send(WriteOp::Upsert(doc)).await.is_err() {
            warn!("search writer channel closed; upsert dropped");
        }
    }

    async fn remove(&self, uri: &AtUri<DefaultStr>) {
        if self
            .inner
            .tx
            .send(WriteOp::Remove(uri.clone()))
            .await
            .is_err()
        {
            warn!("search writer channel closed; remove dropped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobbin_runtime::SystemClock;
    use bobbin_types::search::SearchDoc;
    use jacquard_common::types::nsid::Nsid as NsidType;

    fn at(s: &str) -> AtUri<DefaultStr> {
        AtUri::new_owned(s).unwrap()
    }

    fn nsid(s: &'static str) -> NsidType<DefaultStr> {
        NsidType::new_static(s).unwrap()
    }

    fn doc(
        uri: AtUri<DefaultStr>,
        nsid: NsidType<DefaultStr>,
        title: &str,
        body: &str,
    ) -> SearchDoc {
        SearchDoc {
            uri,
            nsid,
            title: title.to_owned(),
            body: body.to_owned(),
            author: None,
            created_at: None,
            repo: None,
        }
    }

    fn build() -> SearchIndex {
        SearchIndex::new(DEFAULT_WRITER_HEAP_BYTES, Arc::new(SystemClock::new())).unwrap()
    }

    #[tokio::test]
    async fn upsert_then_query_returns_matching_doc() {
        let idx = build();
        idx.upsert(doc(
            at("at://did:plc:nel/sh.tangled.repo.issue/r1"),
            nsid("sh.tangled.repo.issue"),
            "barnacle pagination",
            "scroll resets",
        ))
        .await;
        idx.upsert(doc(
            at("at://did:plc:teq/sh.tangled.repo.issue/r2"),
            nsid("sh.tangled.repo.issue"),
            "kelp grew sideways",
            "kelp",
        ))
        .await;
        idx.flush().await;

        let page = idx
            .search(
                "barnacle",
                SearchFilters::default(),
                SearchCursor::Start,
                10,
            )
            .await
            .unwrap();
        assert_eq!(page.hits.len(), 1);
        assert_eq!(
            page.hits[0].uri.as_ref(),
            "at://did:plc:nel/sh.tangled.repo.issue/r1"
        );
        assert!(page.next.is_none());
    }

    #[tokio::test]
    async fn query_filters_by_nsid() {
        let idx = build();
        idx.upsert(doc(
            at("at://did:plc:nel/sh.tangled.repo.issue/r1"),
            nsid("sh.tangled.repo.issue"),
            "anemone tide",
            "",
        ))
        .await;
        idx.upsert(doc(
            at("at://did:plc:teq/sh.tangled.string/k1"),
            nsid("sh.tangled.string"),
            "anemone.md",
            "anemone again",
        ))
        .await;
        idx.flush().await;

        let only_strings = idx
            .search(
                "anemone",
                SearchFilters {
                    nsid: Some(nsid("sh.tangled.string")),
                    ..SearchFilters::default()
                },
                SearchCursor::Start,
                10,
            )
            .await
            .unwrap();
        assert_eq!(only_strings.hits.len(), 1);
        assert_eq!(only_strings.hits[0].nsid.as_ref(), "sh.tangled.string");
    }

    #[tokio::test]
    async fn upsert_replaces_prior_document_for_same_uri() {
        let idx = build();
        let uri = at("at://did:plc:nel/sh.tangled.repo.issue/r1");
        idx.upsert(doc(
            uri.clone(),
            nsid("sh.tangled.repo.issue"),
            "abalone",
            "",
        ))
        .await;
        idx.upsert(doc(uri, nsid("sh.tangled.repo.issue"), "limpet", ""))
            .await;
        idx.flush().await;

        let abalone_hits = idx
            .search("abalone", SearchFilters::default(), SearchCursor::Start, 10)
            .await
            .unwrap();
        assert!(abalone_hits.hits.is_empty(), "old title must be evicted");

        let limpet_hits = idx
            .search("limpet", SearchFilters::default(), SearchCursor::Start, 10)
            .await
            .unwrap();
        assert_eq!(limpet_hits.hits.len(), 1);
    }

    #[tokio::test]
    async fn remove_drops_document_from_index() {
        let idx = build();
        let uri = at("at://did:plc:nel/sh.tangled.repo.issue/r1");
        idx.upsert(doc(
            uri.clone(),
            nsid("sh.tangled.repo.issue"),
            "whelk",
            "shell",
        ))
        .await;
        idx.remove(&uri).await;
        idx.flush().await;
        let hits = idx
            .search("whelk", SearchFilters::default(), SearchCursor::Start, 10)
            .await
            .unwrap();
        assert!(hits.hits.is_empty());
    }

    #[tokio::test]
    async fn pagination_advances_cursor() {
        let idx = build();
        let names = ["nel", "olaren", "teq", "lyna", "bailey"];
        for (i, owner) in names.iter().enumerate() {
            idx.upsert(doc(
                at(&format!("at://did:plc:{owner}/sh.tangled.repo.issue/r{i}")),
                nsid("sh.tangled.repo.issue"),
                "anemone",
                "tides",
            ))
            .await;
        }
        idx.flush().await;

        let page1 = idx
            .search("anemone", SearchFilters::default(), SearchCursor::Start, 2)
            .await
            .unwrap();
        assert_eq!(page1.hits.len(), 2);
        let next = page1.next.expect("more pages");
        let page2 = idx
            .search(
                "anemone",
                SearchFilters::default(),
                SearchCursor::At(next),
                2,
            )
            .await
            .unwrap();
        assert_eq!(page2.hits.len(), 2);
        let next2 = page2.next.expect("more pages");
        let page3 = idx
            .search(
                "anemone",
                SearchFilters::default(),
                SearchCursor::At(next2),
                2,
            )
            .await
            .unwrap();
        assert_eq!(page3.hits.len(), 1);
        assert!(page3.next.is_none());
    }

    #[tokio::test]
    async fn empty_query_short_circuits_to_empty_page() {
        let idx = build();
        idx.upsert(doc(
            at("at://did:plc:nel/sh.tangled.repo.issue/r1"),
            nsid("sh.tangled.repo.issue"),
            "abalone",
            "",
        ))
        .await;
        idx.flush().await;
        let page = idx
            .search("   ", SearchFilters::default(), SearchCursor::Start, 10)
            .await
            .unwrap();
        assert!(page.hits.is_empty());
        assert!(page.next.is_none());
    }

    #[tokio::test]
    async fn flush_without_pending_ops_completes() {
        let idx = build();
        idx.flush().await;
    }

    #[tokio::test]
    async fn writer_auto_commits_on_idle_interval() {
        let idx = build();
        idx.upsert(doc(
            at("at://did:plc:nel/sh.tangled.repo.issue/r1"),
            nsid("sh.tangled.repo.issue"),
            "auto",
            "",
        ))
        .await;
        tokio::time::sleep(BATCH_INTERVAL * 3).await;
        let page = idx
            .search("auto", SearchFilters::default(), SearchCursor::Start, 10)
            .await
            .unwrap();
        assert_eq!(page.hits.len(), 1);
    }

    #[test]
    fn next_cursor_advances_by_limit_when_more_results() {
        assert_eq!(next_cursor(0, 10, true), Some(SearchOffset::new(10)));
        assert_eq!(next_cursor(10, 25, true), Some(SearchOffset::new(35)));
    }

    #[test]
    fn next_cursor_is_none_when_no_more_results() {
        assert_eq!(next_cursor(0, 10, false), None);
        assert_eq!(next_cursor(99, 50, false), None);
    }

    #[test]
    fn next_cursor_returns_none_on_u32_overflow_rather_than_wrapping() {
        let max = u32::MAX as usize;
        assert_eq!(next_cursor(max, 1, true), None);
        assert_eq!(next_cursor(max - 5, 10, true), None);
        assert_eq!(
            next_cursor(max - 10, 10, true),
            Some(SearchOffset::new(u32::MAX))
        );
    }

    #[test]
    fn cursor_token_round_trips() {
        let off = SearchOffset::new(0xc0fe);
        let token = off.encode_token();
        assert_eq!(token.len(), OFFSET_TOKEN_LEN);
        assert_eq!(SearchOffset::decode_token(&token).unwrap(), off);
    }

    #[test]
    fn cursor_decode_rejects_malformed() {
        let bad = ["", "deadbeef0", "no-hex!!", "zzzzzzzz", "1234567"];
        for s in bad {
            assert!(matches!(
                SearchOffset::decode_token(s),
                Err(SearchOffsetError::Malformed),
            ));
        }
    }
}
