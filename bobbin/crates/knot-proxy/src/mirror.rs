use std::sync::Arc;
use std::time::Duration;

use bobbin_runtime::{Clock, RuntimeHasher};
use http::HeaderMap;
use jacquard_common::types::did::Did;
use jacquard_common::types::nsid::Nsid;
use jacquard_common::{BosStr, DefaultStr};
use thiserror::Error;
use url::Url;

use crate::breaker::FailureThreshold;
use crate::host::{KnotHost, KnotHostError};
use crate::{KnotHttpConfig, KnotProxy, KnotProxyConfig, KnotProxyError, ProxyResponse};

const REPO_PARAM: &str = "repo";
const PATH_PARAM: &str = "path";

const MIRROR_HTTP: KnotHttpConfig = KnotHttpConfig {
    connect_timeout: Duration::from_secs(3),
    read_timeout: Duration::from_secs(5),
};

#[derive(Debug, Error)]
pub enum MirrorProxyError {
    #[error("mirror url: {0}")]
    Host(#[from] KnotHostError),
    #[error("mirror http client: {0}")]
    Http(#[from] reqwest::Error),
    #[error("mirror url must be a bare origin, got {0}")]
    NotAnOrigin(String),
}

struct MirrorRoute {
    knot: &'static str,
    mirror: &'static str,
    eligible: fn(&[(&str, &str)]) -> bool,
}

const ROUTES: &[MirrorRoute] = &[
    MirrorRoute {
        knot: "sh.tangled.repo.branches",
        mirror: "sh.tangled.git.temp.listBranches",
        eligible: any_query,
    },
    MirrorRoute {
        knot: "sh.tangled.repo.log",
        mirror: "sh.tangled.git.temp.listCommits",
        eligible: no_path_filter,
    },
    MirrorRoute {
        knot: "sh.tangled.repo.tag",
        mirror: "sh.tangled.git.temp.getTag",
        eligible: any_query,
    },
    MirrorRoute {
        knot: "sh.tangled.repo.tags",
        mirror: "sh.tangled.git.temp.listTags",
        eligible: any_query,
    },
    MirrorRoute {
        knot: "sh.tangled.repo.tree",
        mirror: "sh.tangled.git.temp.getTree",
        eligible: any_query,
    },
];

fn any_query(_: &[(&str, &str)]) -> bool {
    true
}

fn no_path_filter(query: &[(&str, &str)]) -> bool {
    !query.iter().any(|(k, v)| *k == PATH_PARAM && !v.is_empty())
}

#[derive(Debug)]
pub struct MirrorNsid(Nsid<DefaultStr>);

impl MirrorNsid {
    pub fn route(knot_nsid: &str, query: &[(&str, &str)]) -> Option<Self> {
        let route = ROUTES.iter().find(|r| r.knot == knot_nsid)?;
        (route.eligible)(query).then(|| {
            Self(Nsid::new_static(route.mirror).expect("every nsid in ROUTES is a valid literal"))
        })
    }

    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

pub struct MirrorProxy {
    proxy: KnotProxy,
    host: KnotHost,
}

impl MirrorProxy {
    pub fn new(
        url: &Url,
        clock: Arc<dyn Clock>,
        hasher: RuntimeHasher,
    ) -> Result<Self, MirrorProxyError> {
        let host = KnotHost::parse(url.as_str())?;
        match beyond_origin(url) {
            Some(extra) => Err(MirrorProxyError::NotAnOrigin(extra)),
            None => Ok(Self {
                proxy: KnotProxy::new(
                    KnotProxyConfig {
                        allow_private_hosts: true,
                        require_https: false,
                        failure_threshold: FailureThreshold::new(3).expect("nonzero literal"),
                        ..KnotProxyConfig::default()
                    },
                    MIRROR_HTTP,
                    clock,
                    hasher,
                )?,
                host,
            }),
        }
    }

    pub fn host(&self) -> &KnotHost {
        &self.host
    }

    pub async fn forward<S: BosStr + AsRef<str>>(
        &self,
        nsid: &MirrorNsid,
        repo: &Did<S>,
        query: &[(&str, &str)],
        headers: HeaderMap,
    ) -> Result<ProxyResponse, KnotProxyError> {
        let keyed: Vec<(&str, &str)> = query
            .iter()
            .copied()
            .filter(|(k, _)| *k != REPO_PARAM)
            .chain(std::iter::once((REPO_PARAM, repo.as_ref())))
            .collect();
        self.proxy
            .forward(&self.host, &nsid.0, &keyed, headers)
            .await
    }
}

fn beyond_origin(url: &Url) -> Option<String> {
    [
        (!matches!(url.path(), "" | "/")).then(|| format!("the path {}", url.path())),
        url.query().map(|query| format!("the query {query}")),
        url.fragment()
            .map(|fragment| format!("the fragment {fragment}")),
        (!url.username().is_empty()).then(|| format!("the username {}", url.username())),
        url.password().map(|_| "a password".to_owned()),
    ]
    .into_iter()
    .flatten()
    .next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobbin_runtime::SystemClock;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn mirror_at(raw: &str) -> Result<MirrorProxy, MirrorProxyError> {
        MirrorProxy::new(
            &Url::parse(raw).unwrap(),
            Arc::new(SystemClock::new()),
            RuntimeHasher::default(),
        )
    }

    #[test]
    fn a_mirror_url_with_more_than_an_origin_is_refused_without_echoing_a_password() {
        [
            "https://nel.pet/api",
            "https://nel.pet/?a=1",
            "https://nel.pet/#x",
            "https://nel@nel.pet/",
            "https://nel:hunter2@nel.pet/",
            "https://:hunter2@nel.pet/",
        ]
        .iter()
        .for_each(|raw| {
            let Err(err) = mirror_at(raw) else {
                panic!("{raw} must error out, since KnotHost::parse would truncate it");
            };
            assert!(
                matches!(err, MirrorProxyError::NotAnOrigin(_)),
                "{raw} gave {err}",
            );
            assert!(
                !err.to_string().contains("hunter2"),
                "an operator's password must stay out of the log, got {err}",
            );
        });
        assert!(
            mirror_at("https://nel.pet/").is_ok(),
            "a bare origin is what an operator will configure",
        );
    }

    #[tokio::test]
    async fn forward_replaces_a_slug_the_caller_left_in_the_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/sh.tangled.git.temp.listTags"))
            .and(query_param("repo", "did:plc:conch"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .mount(&server)
            .await;

        let mirror = mirror_at(&server.uri()).unwrap();
        let nsid = MirrorNsid::route("sh.tangled.repo.tags", &[]).unwrap();
        let resp = mirror
            .forward(
                &nsid,
                &Did::<DefaultStr>::new_static("did:plc:conch").unwrap(),
                &[("repo", "did:plc:nel/scallop")],
                HeaderMap::new(),
            )
            .await
            .expect("the mounted mirror answers");
        assert_eq!(resp.status(), 200);
    }

    #[test]
    fn a_mirror_dials_where_the_knot_policy_refuses() {
        let mirror = mirror_at("http://127.0.0.1:9/").expect("an operator picks the mirror");
        assert!(
            mirror.proxy.allows_private_hosts() && !mirror.proxy.requires_https(),
            "the knot policy doesn't bind an operator-configured mirror",
        );
    }
}
