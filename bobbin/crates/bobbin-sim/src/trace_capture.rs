use std::sync::Arc;
use std::sync::Mutex;

use jacquard_common::DefaultStr;
use jacquard_common::types::nsid::Nsid;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

#[derive(Clone, Default)]
pub struct TraceCapture {
    inner: Arc<Mutex<Vec<String>>>,
}

impl TraceCapture {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lines(&self) -> Vec<String> {
        self.inner.lock().unwrap().clone()
    }

    pub fn into_lines(self) -> Vec<String> {
        std::mem::take(&mut *self.inner.lock().unwrap())
    }

    pub fn layer(&self) -> StageLayer {
        StageLayer {
            sink: self.inner.clone(),
        }
    }
}

pub struct StageLayer {
    sink: Arc<Mutex<Vec<String>>>,
}

impl<S> Layer<S> for StageLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if meta.target() != "bobbin_ingest::stage" {
            return;
        }
        let mut visitor = StageVisitor::default();
        event.record(&mut visitor);
        let line = format!(
            "cursor={} regime={} nsid={} edge_count={} parallelism={}",
            visitor.cursor.unwrap_or(u64::MAX),
            visitor.regime.as_deref().unwrap_or(""),
            visitor.nsid.as_ref().map(Nsid::as_ref).unwrap_or(""),
            visitor.edge_count.unwrap_or(0),
            visitor.parallelism.unwrap_or(0),
        );
        self.sink.lock().unwrap().push(line);
    }
}

#[derive(Default)]
struct StageVisitor {
    cursor: Option<u64>,
    regime: Option<String>,
    nsid: Option<Nsid<DefaultStr>>,
    edge_count: Option<u64>,
    parallelism: Option<usize>,
}

impl Visit for StageVisitor {
    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "cursor" => self.cursor = Some(value),
            "edge_count" => self.edge_count = Some(value),
            "parallelism" => self.parallelism = Some(value as usize),
            _ => {}
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if field.name() == "cursor" {
            self.cursor = Some(value as u64);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "regime" => self.regime = Some(value.to_owned()),
            "nsid" => self.nsid = Nsid::new_owned(value).ok(),
            _ => {}
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}
