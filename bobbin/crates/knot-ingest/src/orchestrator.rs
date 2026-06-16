use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bobbin_edge_index::EdgeStore;
use bobbin_knot_proxy::KnotHost;
use bobbin_runtime::{Clock, WsTransport};
use bobbin_types::knot_acl::{KnotHostKey, host_to_knot_did};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use tokio_util::sync::CancellationToken;

use crate::client::{AclListing, Completeness, KnotClient, knot_endpoint};
use crate::gate::CapabilityGate;
use crate::registry::KnotRegistry;
use crate::roster::{AclOp, Cursor, Roster};
use crate::stream::{StreamConfig, run_stream};

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const RECONCILE_INTERVAL: Duration = Duration::from_secs(300);
const PROBE_CONCURRENCY: usize = 16;

pub struct Orchestrator {
    pub client: Arc<KnotClient>,
    pub gate: Arc<CapabilityGate>,
    pub registry: Arc<KnotRegistry>,
    pub store: Arc<EdgeStore>,
    pub ws: Arc<dyn WsTransport>,
    pub clock: Arc<dyn Clock>,
    pub dev: bool,
    pub allow_private: bool,
    pub cancel: CancellationToken,
}

impl Orchestrator {
    pub async fn run(self) {
        let mut subscribed: HashMap<KnotHostKey, CancellationToken> = HashMap::new();
        let mut unspawnable: HashSet<KnotHostKey> = HashSet::new();
        loop {
            if self.cancel.is_cancelled() {
                break;
            }
            self.discover(&mut subscribed, &mut unspawnable).await;
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                _ = self.clock.sleep(POLL_INTERVAL) => {}
            }
        }
    }

    async fn discover(
        &self,
        subscribed: &mut HashMap<KnotHostKey, CancellationToken>,
        unspawnable: &mut HashSet<KnotHostKey>,
    ) {
        let candidates: Vec<KnotHostKey> = self
            .registry
            .hosts()
            .into_iter()
            .filter(|host| !subscribed.contains_key(host) && !unspawnable.contains(host))
            .collect();
        let approved: Vec<KnotHostKey> = futures::stream::iter(candidates)
            .map(|host| async move { self.gate.has_knot_acl(&host).await.then_some(host) })
            .buffer_unordered(PROBE_CONCURRENCY)
            .filter_map(|approved| async move { approved })
            .collect()
            .await;
        approved
            .into_iter()
            .for_each(|host| match self.spawn(&host) {
                Some(token) => {
                    subscribed.insert(host, token);
                }
                None => {
                    tracing::warn!(host = %host, "skipping unspawnable knot endpoint");
                    unspawnable.insert(host);
                }
            });
    }

    fn spawn(&self, host: &KnotHostKey) -> Option<CancellationToken> {
        let endpoint = knot_endpoint(host.as_str(), self.dev, self.allow_private).ok()?;
        let knot_did = host_to_knot_did(host.as_str())?;
        let roster = Arc::new(Mutex::new(Roster::new(
            self.store.clone(),
            knot_did,
            self.registry.clone(),
            host.clone(),
        )));
        let token = self.cancel.child_token();

        let stream_cfg = StreamConfig {
            ws: self.ws.clone(),
            clock: self.clock.clone(),
            cancel: token.clone(),
        };
        let stream_roster = roster.clone();
        let stream_endpoint = endpoint.clone();
        tokio::spawn(async move {
            run_stream(&stream_cfg, &stream_endpoint, &stream_roster, 0).await;
        });

        let client = self.client.clone();
        let registry = self.registry.clone();
        let clock = self.clock.clone();
        let host_owned = host.clone();
        let reconcile_token = token.clone();
        tokio::spawn(async move {
            reconcile_loop(
                &client,
                &endpoint,
                &host_owned,
                &registry,
                &roster,
                &*clock,
                &reconcile_token,
            )
            .await;
        });

        Some(token)
    }
}

async fn reconcile_loop(
    client: &KnotClient,
    endpoint: &KnotHost,
    host: &KnotHostKey,
    registry: &KnotRegistry,
    roster: &Mutex<Roster>,
    clock: &dyn Clock,
    cancel: &CancellationToken,
) {
    loop {
        if cancel.is_cancelled() {
            return;
        }
        reconcile_once(client, endpoint, host, registry, roster).await;
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = clock.sleep(RECONCILE_INTERVAL) => {}
        }
    }
}

async fn reconcile_once(
    client: &KnotClient,
    endpoint: &KnotHost,
    host: &KnotHostKey,
    registry: &KnotRegistry,
    roster: &Mutex<Roster>,
) {
    let horizon = roster.lock().unwrap().max_cursor();

    match client.list_members(endpoint).await {
        Ok(AclListing {
            entries,
            completeness,
        }) => {
            let present: HashSet<Did<DefaultStr>> =
                entries.iter().map(|entry| entry.subject.clone()).collect();
            let mut guard = roster.lock().unwrap();
            entries.into_iter().for_each(|entry| {
                guard.apply_member(AclOp::Add, entry.subject, Cursor(nanos(entry.created_at)));
            });
            if completeness == Completeness::Complete {
                guard.reap_members(&present, horizon);
            }
        }
        Err(err) => tracing::warn!(host = %host, error = %err, "knot member reconcile failed"),
    }

    futures::stream::iter(registry.repos(host))
        .for_each(|repo| async move {
            match client.list_collaborators(endpoint, &repo).await {
                Ok(AclListing {
                    entries,
                    completeness,
                }) => {
                    let present: HashSet<Did<DefaultStr>> =
                        entries.iter().map(|entry| entry.subject.clone()).collect();
                    let mut guard = roster.lock().unwrap();
                    entries.into_iter().for_each(|entry| {
                        guard.apply_collaborator(
                            AclOp::Add,
                            repo.clone(),
                            entry.subject,
                            Cursor(nanos(entry.created_at)),
                        );
                    });
                    if completeness == Completeness::Complete {
                        guard.reap_collaborators(&repo, &present, horizon);
                    }
                }
                Err(err) => {
                    tracing::warn!(host = %host, repo = %repo.as_ref(), error = %err, "knot collaborator reconcile failed")
                }
            }
        })
        .await;

    roster.lock().unwrap().purge_legacy();
}

fn nanos(timestamp: DateTime<Utc>) -> i64 {
    timestamp.timestamp_nanos_opt().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::authority;
    use bobbin_runtime::{ReqwestHttp, RuntimeHasher};
    use bobbin_types::edges::Edge;
    use bobbin_types::ids::{EdgeKey, SubjectRef, nsid_static};
    use jacquard_common::types::string::AtUri;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn did(s: &str) -> Did<DefaultStr> {
        Did::new_owned(s).unwrap()
    }

    fn member_count(store: &EdgeStore, subject: &str) -> u64 {
        store.count(&EdgeKey::new(
            nsid_static("sh.tangled.knot.member"),
            SubjectRef::Did(did(subject)),
        ))
    }

    fn collaborator_count(store: &EdgeStore, repo: &str) -> u64 {
        store.count(&EdgeKey::new(
            nsid_static("sh.tangled.repo.collaborator"),
            SubjectRef::Did(did(repo)),
        ))
    }

    #[tokio::test]
    async fn reconcile_backfills_members_and_collaborators() {
        let server = MockServer::start().await;
        let endpoint = KnotHost::parse(&server.uri()).unwrap();
        let host = KnotHostKey::new(&authority(&endpoint));

        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.knot.listMembers"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [
                    {"subject": "did:plc:boltless", "addedBy": "did:plc:akshay", "createdAt": "2026-06-01T00:00:00Z"},
                    {"subject": "did:plc:akshay", "addedBy": "did:plc:akshay", "createdAt": "2026-06-02T00:00:00Z"}
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.repo.listCollaborators"))
            .and(query_param("subject", "did:plc:scallop"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [
                    {"subject": "did:plc:olaren", "addedBy": "did:plc:boltless", "createdAt": "2026-06-03T00:00:00Z"}
                ]
            })))
            .mount(&server)
            .await;

        let registry = Arc::new(KnotRegistry::new());
        registry.observe_repo(&host, did("did:plc:scallop"));

        let store = Arc::new(EdgeStore::new(RuntimeHasher::default()));
        let knot_did = host_to_knot_did(host.as_str()).unwrap();
        let roster = Mutex::new(Roster::new(
            store.clone(),
            knot_did,
            registry.clone(),
            host.clone(),
        ));
        let client = KnotClient::new(ReqwestHttp::shared(reqwest::Client::new()));

        reconcile_once(&client, &endpoint, &host, &registry, &roster).await;

        assert_eq!(member_count(&store, "did:plc:boltless"), 1);
        assert_eq!(member_count(&store, "did:plc:akshay"), 1);
        assert_eq!(collaborator_count(&store, "did:plc:scallop"), 1);
    }

    #[tokio::test]
    async fn reconcile_reaps_departed_member() {
        let server = MockServer::start().await;
        let endpoint = KnotHost::parse(&server.uri()).unwrap();
        let host = KnotHostKey::new(&authority(&endpoint));

        let registry = Arc::new(KnotRegistry::new());
        let store = Arc::new(EdgeStore::new(RuntimeHasher::default()));
        let knot_did = host_to_knot_did(host.as_str()).unwrap();
        let roster = Mutex::new(Roster::new(
            store.clone(),
            knot_did,
            registry.clone(),
            host.clone(),
        ));
        let client = KnotClient::new(ReqwestHttp::shared(reqwest::Client::new()));

        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.knot.listMembers"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [
                    {"subject": "did:plc:boltless", "addedBy": "did:plc:akshay", "createdAt": "2026-06-01T00:00:00Z"},
                    {"subject": "did:plc:akshay", "addedBy": "did:plc:akshay", "createdAt": "2026-06-02T00:00:00Z"}
                ]
            })))
            .mount(&server)
            .await;
        reconcile_once(&client, &endpoint, &host, &registry, &roster).await;
        assert_eq!(member_count(&store, "did:plc:boltless"), 1);
        assert_eq!(member_count(&store, "did:plc:akshay"), 1);

        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.knot.listMembers"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [
                    {"subject": "did:plc:akshay", "addedBy": "did:plc:akshay", "createdAt": "2026-06-02T00:00:00Z"}
                ]
            })))
            .mount(&server)
            .await;
        reconcile_once(&client, &endpoint, &host, &registry, &roster).await;

        assert_eq!(
            member_count(&store, "did:plc:boltless"),
            0,
            "a member dropped from the authoritative snapshot is reaped on reconcile"
        );
        assert_eq!(member_count(&store, "did:plc:akshay"), 1);
    }

    #[tokio::test]
    async fn reconcile_skips_reap_when_member_list_truncated() {
        let server = MockServer::start().await;
        let endpoint = KnotHost::parse(&server.uri()).unwrap();
        let host = KnotHostKey::new(&authority(&endpoint));

        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.knot.listMembers"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [{"subject": "did:plc:boltless", "addedBy": "did:plc:akshay", "createdAt": "2026-06-01T00:00:00Z"}],
                "cursor": "more"
            })))
            .mount(&server)
            .await;

        let registry = Arc::new(KnotRegistry::new());
        let store = Arc::new(EdgeStore::new(RuntimeHasher::default()));
        let knot_did = host_to_knot_did(host.as_str()).unwrap();
        let roster = Mutex::new(Roster::new(
            store.clone(),
            knot_did,
            registry.clone(),
            host.clone(),
        ));
        let client = KnotClient::new(ReqwestHttp::shared(reqwest::Client::new()));

        let stayed = did("did:plc:akshay");
        roster
            .lock()
            .unwrap()
            .apply_member(AclOp::Add, stayed, Cursor(1_000_000));
        assert_eq!(member_count(&store, "did:plc:akshay"), 1);

        reconcile_once(&client, &endpoint, &host, &registry, &roster).await;

        assert_eq!(
            member_count(&store, "did:plc:akshay"),
            1,
            "a truncated member snapshot must not reap members it could not enumerate"
        );
        assert_eq!(member_count(&store, "did:plc:boltless"), 1);
    }

    fn seed_legacy_edge(store: &EdgeStore, kind: &'static str, subject: &str, source: &str) {
        let source = AtUri::new_owned(source).unwrap();
        store.upsert_source(
            &source,
            vec![Edge {
                kind: nsid_static(kind),
                subject: SubjectRef::Did(did(subject)),
                source: source.clone(),
                sort_micros: 0,
            }],
        );
    }

    #[tokio::test]
    async fn reconcile_purges_seeded_legacy_acl() {
        let server = MockServer::start().await;
        let endpoint = KnotHost::parse(&server.uri()).unwrap();
        let host = KnotHostKey::new(&authority(&endpoint));

        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.knot.listMembers"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [
                    {"subject": "did:plc:boltless", "addedBy": "did:plc:akshay", "createdAt": "2026-06-01T00:00:00Z"}
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.repo.listCollaborators"))
            .and(query_param("subject", "did:plc:scallop"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [
                    {"subject": "did:plc:olaren", "addedBy": "did:plc:boltless", "createdAt": "2026-06-03T00:00:00Z"}
                ]
            })))
            .mount(&server)
            .await;

        let registry = Arc::new(KnotRegistry::new());
        let repo = did("did:plc:scallop");
        registry.observe_repo(&host, repo.clone());

        let store = Arc::new(EdgeStore::new(RuntimeHasher::default()));
        let member_source = "at://did:plc:akshay/sh.tangled.knot.member/r1";
        seed_legacy_edge(
            &store,
            "sh.tangled.knot.member",
            "did:plc:boltless",
            member_source,
        );
        registry.note_legacy_member(AtUri::new_owned(member_source).unwrap(), &host);
        seed_legacy_edge(
            &store,
            "sh.tangled.repo.collaborator",
            "did:plc:scallop",
            "at://did:plc:akshay/sh.tangled.repo.collaborator/r2",
        );

        let knot_did = host_to_knot_did(host.as_str()).unwrap();
        let roster = Mutex::new(Roster::new(
            store.clone(),
            knot_did,
            registry.clone(),
            host.clone(),
        ));
        let client = KnotClient::new(ReqwestHttp::shared(reqwest::Client::new()));

        reconcile_once(&client, &endpoint, &host, &registry, &roster).await;

        assert_eq!(
            member_count(&store, "did:plc:boltless"),
            1,
            "stale legacy member purged, knot-owned member kept"
        );
        assert_eq!(
            collaborator_count(&store, "did:plc:scallop"),
            1,
            "stale legacy collaborator purged, knot-owned collaborator kept"
        );
    }
}
