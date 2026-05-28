use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use bobbin_edge_index::{CoverageWatch, EdgeStore};
use bobbin_record_lru::NoopRecordStore;
use bobbin_runtime::{Clock, Entropy, MemHttpResponder, MemWsResponder};
use tokio_util::sync::CancellationToken;

use crate::report::SimReport;

pub struct WorkloadCtx {
    pub seed: u64,
    pub clock: Arc<dyn Clock>,
    pub entropy: Arc<dyn Entropy>,
    pub store: Arc<EdgeStore>,
    pub coverage: Arc<CoverageWatch>,
    pub records: Arc<NoopRecordStore>,
    pub cancel: CancellationToken,
    pub consumer_too_slow_count: Arc<AtomicU64>,
}

pub type WorkloadScript = Pin<Box<dyn Future<Output = SimReport> + Send + 'static>>;

pub struct WorkloadHooks {
    pub slingshot: Arc<dyn MemHttpResponder>,
    pub hydrant: Arc<dyn MemWsResponder>,
    pub script: WorkloadScript,
}

pub trait Workload: Send + 'static {
    fn name(&self) -> &'static str;
    fn build(self: Box<Self>, ctx: WorkloadCtx) -> WorkloadHooks;
}
