use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bobbin_runtime::Clock;
use bobbin_types::knot_acl::KnotHostKey;
use tokio::time::Instant;

use crate::client::{KnotClient, knot_endpoint};

const KNOT_ACL_CAPABILITY: &str = "knot-acl";
const LEGACY_REPROBE_INTERVAL: Duration = Duration::from_secs(300);
const ERROR_REPROBE_INTERVAL: Duration = Duration::from_secs(60);

struct ProbeRecord {
    at: Instant,
    retry_after: Duration,
}

pub struct CapabilityGate {
    client: KnotClient,
    clock: Arc<dyn Clock>,
    dev: bool,
    allow_private: bool,
    native: Mutex<HashSet<KnotHostKey>>,
    last_probe: Mutex<HashMap<KnotHostKey, ProbeRecord>>,
}

impl CapabilityGate {
    pub fn new(client: KnotClient, clock: Arc<dyn Clock>, dev: bool, allow_private: bool) -> Self {
        Self {
            client,
            clock,
            dev,
            allow_private,
            native: Mutex::new(HashSet::new()),
            last_probe: Mutex::new(HashMap::new()),
        }
    }

    pub fn is_native(&self, host: &KnotHostKey) -> bool {
        self.native.lock().unwrap().contains(host)
    }

    pub async fn has_knot_acl(&self, host: &KnotHostKey) -> bool {
        if self.is_native(host) {
            return true;
        }
        let now = self.clock.now_instant();
        if self.throttled(host, now) {
            return false;
        }
        match self.probe(host).await {
            Ok(true) => {
                self.native.lock().unwrap().insert(host.clone());
                true
            }
            Ok(false) => {
                self.mark(host, now, LEGACY_REPROBE_INTERVAL);
                false
            }
            Err(err) => {
                tracing::warn!(host = %host, error = %err, "knot capability probe failed");
                self.mark(host, now, ERROR_REPROBE_INTERVAL);
                false
            }
        }
    }

    async fn probe(&self, host: &KnotHostKey) -> Result<bool, crate::client::KnotClientError> {
        let endpoint = knot_endpoint(host.as_str(), self.dev, self.allow_private)?;
        let caps = self.client.capabilities(&endpoint).await?;
        Ok(caps.iter().any(|cap| cap == KNOT_ACL_CAPABILITY))
    }

    fn throttled(&self, host: &KnotHostKey, now: Instant) -> bool {
        self.last_probe
            .lock()
            .unwrap()
            .get(host)
            .is_some_and(|rec| now.saturating_duration_since(rec.at) < rec.retry_after)
    }

    fn mark(&self, host: &KnotHostKey, now: Instant, retry_after: Duration) {
        self.last_probe.lock().unwrap().insert(
            host.clone(),
            ProbeRecord {
                at: now,
                retry_after,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use bobbin_runtime::{ReqwestHttp, SleepFuture, UnixMicros};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct ManualClock {
        base: Instant,
        offset_micros: AtomicU64,
    }

    impl ManualClock {
        fn new() -> Self {
            Self {
                base: Instant::now(),
                offset_micros: AtomicU64::new(0),
            }
        }

        fn advance(&self, by: Duration) {
            self.offset_micros
                .fetch_add(by.as_micros() as u64, Ordering::SeqCst);
        }
    }

    impl Clock for ManualClock {
        fn now_unix_micros(&self) -> UnixMicros {
            UnixMicros::new(self.offset_micros.load(Ordering::SeqCst))
        }
        fn now_instant(&self) -> Instant {
            self.base + Duration::from_micros(self.offset_micros.load(Ordering::SeqCst))
        }
        fn sleep(&self, _: Duration) -> SleepFuture {
            Box::pin(async {})
        }
        fn sleep_until(&self, _: Instant) -> SleepFuture {
            Box::pin(async {})
        }
    }

    fn gate(server: &MockServer, clock: Arc<dyn Clock>) -> (CapabilityGate, KnotHostKey) {
        let client = KnotClient::new(ReqwestHttp::shared(reqwest::Client::new()));
        let url = url::Url::parse(&server.uri()).unwrap();
        let host = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
        (
            CapabilityGate::new(client, clock, true, true),
            KnotHostKey::new(&host),
        )
    }

    async fn mount_version(server: &MockServer, caps: serde_json::Value, expect: u64) {
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.knot.version"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "version": "1.0.0 (cafe)", "capabilities": caps })),
            )
            .expect(expect)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn declares_knot_acl() {
        let server = MockServer::start().await;
        mount_version(&server, json!(["knot-acl"]), 1).await;
        let (gate, host) = gate(&server, Arc::new(ManualClock::new()));
        assert!(gate.has_knot_acl(&host).await);
        assert!(gate.is_native(&host));
    }

    #[tokio::test]
    async fn legacy_knot_without_capability() {
        let server = MockServer::start().await;
        mount_version(&server, json!([]), 1).await;
        let (gate, host) = gate(&server, Arc::new(ManualClock::new()));
        assert!(!gate.has_knot_acl(&host).await);
        assert!(!gate.is_native(&host));
    }

    #[tokio::test]
    async fn native_is_latched_and_survives_probe_error() {
        let server = MockServer::start().await;
        mount_version(&server, json!(["knot-acl"]), 1).await;
        let clock = Arc::new(ManualClock::new());
        let (gate, host) = gate(&server, clock.clone());
        assert!(gate.has_knot_acl(&host).await);

        server.reset().await;
        clock.advance(LEGACY_REPROBE_INTERVAL + Duration::from_secs(1));
        assert!(
            gate.has_knot_acl(&host).await,
            "latched native never re-probes"
        );
        assert!(gate.is_native(&host));
    }

    #[tokio::test]
    async fn legacy_throttled_then_reprobed_after_interval() {
        let server = MockServer::start().await;
        mount_version(&server, json!([]), 2).await;
        let clock = Arc::new(ManualClock::new());
        let (gate, host) = gate(&server, clock.clone());
        assert!(!gate.has_knot_acl(&host).await);
        clock.advance(Duration::from_secs(60));
        assert!(!gate.has_knot_acl(&host).await);
        clock.advance(LEGACY_REPROBE_INTERVAL);
        assert!(!gate.has_knot_acl(&host).await);
    }

    #[tokio::test]
    async fn legacy_upgrade_is_detected_on_reprobe() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.knot.version"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "version": "1.0.0" })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.knot.version"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "version": "1.1.0", "capabilities": ["knot-acl"] })),
            )
            .mount(&server)
            .await;
        let clock = Arc::new(ManualClock::new());
        let (gate, host) = gate(&server, clock.clone());
        assert!(!gate.has_knot_acl(&host).await);
        clock.advance(LEGACY_REPROBE_INTERVAL + Duration::from_secs(1));
        assert!(gate.has_knot_acl(&host).await);
        assert!(gate.is_native(&host));
    }

    #[tokio::test]
    async fn probe_error_throttled_briefly_then_reprobed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.knot.version"))
            .respond_with(ResponseTemplate::new(503))
            .expect(2)
            .mount(&server)
            .await;
        let clock = Arc::new(ManualClock::new());
        let (gate, host) = gate(&server, clock.clone());
        assert!(!gate.has_knot_acl(&host).await);
        clock.advance(Duration::from_secs(1));
        assert!(
            !gate.has_knot_acl(&host).await,
            "error reprobe is throttled"
        );
        clock.advance(ERROR_REPROBE_INTERVAL);
        assert!(!gate.has_knot_acl(&host).await);
    }
}
