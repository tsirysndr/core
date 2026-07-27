use axum::Router;
use axum::body::Body;
use http::header::ALT_SVC;
use http::{HeaderValue, Request, Response, StatusCode};

const ALT_SVC_MAX_AGE_SECS: u32 = 86_400;

knot_types::scalar_newtype! {
    pub struct Port(u16);
}

pub fn alt_svc_header(port: Port) -> HeaderValue {
    let port = port.get();
    HeaderValue::from_str(&format!("h3=\":{port}\"; ma={ALT_SVC_MAX_AGE_SECS}"))
        .expect("alt-svc header value is valid ascii")
}

pub fn with_alt_svc(app: Router, port: Port) -> Router {
    let value = alt_svc_header(port);
    app.layer(axum::middleware::map_response(
        move |mut response: Response<Body>| {
            let value = value.clone();
            async move {
                if response.status() != StatusCode::SWITCHING_PROTOCOLS {
                    response.headers_mut().insert(ALT_SVC, value);
                }
                response
            }
        },
    ))
}

pub fn with_host_from_authority(app: Router) -> Router {
    app.layer(axum::middleware::map_request(
        |mut request: Request<Body>| async move {
            let authority = request
                .uri()
                .authority()
                .map(|authority| HeaderValue::from_str(authority.as_str()));
            match (
                request.headers().contains_key(http::header::HOST),
                authority,
            ) {
                (false, Some(Ok(value))) => {
                    request.headers_mut().insert(http::header::HOST, value);
                    request
                }
                _ => request,
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::routing::get;
    use tower::ServiceExt;

    #[test]
    fn alt_svc_header_advertises_h3() {
        assert_eq!(
            alt_svc_header(Port::new(443)).to_str().unwrap(),
            "h3=\":443\"; ma=86400"
        );
    }

    #[tokio::test]
    async fn alt_svc_added_to_responses_except_switching_protocols() {
        let app = with_alt_svc(
            Router::new().route("/ok", get(|| async { "ok" })).route(
                "/upgrade",
                get(|| async {
                    Response::builder()
                        .status(StatusCode::SWITCHING_PROTOCOLS)
                        .body(Body::empty())
                        .unwrap()
                }),
            ),
            Port::new(443),
        );

        let normal = app
            .clone()
            .oneshot(Request::get("/ok").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            normal.headers().get(ALT_SVC).and_then(|v| v.to_str().ok()),
            Some("h3=\":443\"; ma=86400")
        );

        let upgrade = app
            .oneshot(Request::get("/upgrade").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(
            upgrade.headers().get(ALT_SVC).is_none(),
            "101 responses mustn't include Alt-Svc"
        );
    }
}
