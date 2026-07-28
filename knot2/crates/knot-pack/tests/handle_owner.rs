use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_git::Layout;
use knot_pack::{CacheConfig, HandleResolver, RepoLookup, RepoResolver, RepoTarget};
use knot_types::{AccountDid, Handle, OwnerDid, RepoDid};
use tower::ServiceExt;

struct FakeHandles;

impl HandleResolver for FakeHandles {
    fn resolve(
        &self,
        handle: Handle,
    ) -> Pin<Box<dyn Future<Output = Option<AccountDid>> + Send + '_>> {
        Box::pin(async move {
            (handle.as_str() == "nel.pet").then(|| AccountDid::new("did:plc:nel").unwrap())
        })
    }
}

fn repo_resolver() -> Arc<dyn RepoResolver> {
    let owner = OwnerDid::new("did:plc:nel").unwrap();
    let repo = RepoDid::new("did:plc:whelk").unwrap();
    Arc::new(move |target: &RepoTarget| match target {
        RepoTarget::OwnerPath(o, p)
            if *o == owner && p.rkeys().any(|rkey| rkey.as_str() == "squid") =>
        {
            RepoLookup::Hosted(repo.clone())
        }
        RepoTarget::Did(d) if *d == repo => RepoLookup::Hosted(d.clone()),
        _ => RepoLookup::Unhosted,
    })
}

fn build(layout: &Layout, handle_resolver: Option<Arc<dyn HandleResolver>>) -> axum::Router {
    let (_write, advertisement) = knot_pack::edge_routes(
        layout.clone(),
        repo_resolver(),
        None,
        handle_resolver,
        knot_resource::PackSlots::new(4),
        CacheConfig::default(),
        Arc::new(knot_messages::Catalog::defaults()),
        knot_pack::default_hostname().clone(),
        Arc::new(knot_runtime::SystemClock),
    );
    advertisement.into_router()
}

async fn get(router: axum::Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let response = router
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, body)
}

fn hosted_layout() -> (tempfile::TempDir, Layout) {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    layout
        .create(&RepoDid::new("did:plc:whelk").unwrap())
        .unwrap();
    (scan, layout)
}

#[tokio::test]
async fn a_handle_owner_serves_the_same_repo_as_its_did() {
    let (_scan, layout) = hosted_layout();
    let (handle_status, handle_body) = get(
        build(&layout, Some(Arc::new(FakeHandles))),
        "/nel.pet/squid/info/refs?service=git-upload-pack",
    )
    .await;
    let (did_status, did_body) = get(
        build(&layout, Some(Arc::new(FakeHandles))),
        "/did:plc:nel/squid/info/refs?service=git-upload-pack",
    )
    .await;
    assert_eq!(handle_status, StatusCode::OK);
    assert_eq!(did_status, StatusCode::OK);
    assert_eq!(
        handle_body, did_body,
        "handle owner and DID owner must serve the same repository"
    );
}

#[tokio::test]
async fn a_handle_owner_is_not_found_when_unknown_or_unresolvable() {
    let (_scan, layout) = hosted_layout();
    let (unknown, _) = get(
        build(&layout, Some(Arc::new(FakeHandles))),
        "/olaren.dev/squid/info/refs?service=git-upload-pack",
    )
    .await;
    assert_eq!(
        unknown,
        StatusCode::NOT_FOUND,
        "a handle the resolver rejects isn't found"
    );
    let (no_resolver, _) = get(
        build(&layout, None),
        "/nel.pet/squid/info/refs?service=git-upload-pack",
    )
    .await;
    assert_eq!(
        no_resolver,
        StatusCode::NOT_FOUND,
        "a handle owner with no resolver configured isn't found"
    );
}
