use http::Response;
use http::header::CONTENT_TYPE;
use http_body::Body;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{And, Predicate, SizeAbove};

const MIN_COMPRESS_BYTES: u64 = 256;

#[derive(Clone, Copy)]
pub(crate) struct CompressibleResponse;

impl Predicate for CompressibleResponse {
    fn should_compress<B>(&self, response: &Response<B>) -> bool
    where
        B: Body,
    {
        response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or(value)
                    .trim()
                    .to_ascii_lowercase()
            })
            .is_some_and(|base| {
                matches!(
                    base.as_str(),
                    "application/json" | "application/x-git-upload-pack-advertisement"
                )
            })
    }
}

pub(crate) fn layer() -> CompressionLayer<And<SizeAbove, CompressibleResponse>> {
    CompressionLayer::new()
        .compress_when(SizeAbove::new(MIN_COMPRESS_BYTES).and(CompressibleResponse))
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use http::StatusCode;
    use http::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_RANGE, CONTENT_TYPE, RANGE};
    use tower::ServiceExt;

    use super::layer;

    async fn json_body() -> impl IntoResponse {
        ([(CONTENT_TYPE, "application/json")], "x".repeat(4096))
    }

    async fn advertisement() -> impl IntoResponse {
        (
            [(CONTENT_TYPE, "application/x-git-upload-pack-advertisement")],
            "x".repeat(4096),
        )
    }

    async fn pack_result() -> impl IntoResponse {
        (
            [(CONTENT_TYPE, "application/x-git-upload-pack-result")],
            "x".repeat(4096),
        )
    }

    async fn targz_archive() -> impl IntoResponse {
        ([(CONTENT_TYPE, "application/gzip")], "x".repeat(4096))
    }

    async fn zip_archive() -> impl IntoResponse {
        ([(CONTENT_TYPE, "application/zip")], "x".repeat(4096))
    }

    async fn small_json() -> impl IntoResponse {
        ([(CONTENT_TYPE, "application/json")], "{}")
    }

    async fn cased_json() -> impl IntoResponse {
        (
            [(CONTENT_TYPE, "Application/JSON; charset=utf-8")],
            "x".repeat(4096),
        )
    }

    async fn partial_archive() -> impl IntoResponse {
        (
            StatusCode::PARTIAL_CONTENT,
            [
                (CONTENT_TYPE, "application/gzip"),
                (CONTENT_RANGE, "bytes 0-3/4096"),
            ],
            "xxxx",
        )
    }

    fn app() -> Router {
        Router::new()
            .route("/json", get(json_body))
            .route("/adv", get(advertisement))
            .route("/pack", get(pack_result))
            .route("/targz", get(targz_archive))
            .route("/zip", get(zip_archive))
            .route("/small", get(small_json))
            .route("/cased", get(cased_json))
            .route("/partial", get(partial_archive))
            .layer(layer())
    }

    async fn content_encoding(path: &str, accept: Option<&str>) -> Option<String> {
        let mut builder = http::Request::builder().method("GET").uri(path);
        if let Some(value) = accept {
            builder = builder.header(ACCEPT_ENCODING, value);
        }
        let request = builder.body(axum::body::Body::empty()).unwrap();
        app()
            .oneshot(request)
            .await
            .unwrap()
            .headers()
            .get(CONTENT_ENCODING)
            .map(|value| value.to_str().unwrap().to_string())
    }

    #[tokio::test]
    async fn json_and_advertisement_compress_but_pack_bytes_pass_through() {
        let negotiated = ["zstd", "br", "gzip"];
        let json = content_encoding("/json", Some("zstd, br, gzip")).await;
        assert!(
            json.as_deref().is_some_and(|enc| negotiated.contains(&enc)),
            "json negotiates an encoding, got {json:?}"
        );
        let adv = content_encoding("/adv", Some("zstd, br, gzip")).await;
        assert!(
            adv.as_deref().is_some_and(|enc| negotiated.contains(&enc)),
            "the ref advertisement negotiates an encoding, got {adv:?}"
        );
        assert_eq!(
            content_encoding("/pack", Some("zstd, br, gzip")).await,
            None,
            "an already-compressed pack stream is never re-encoded"
        );
    }

    #[tokio::test]
    async fn already_compressed_archives_pass_through() {
        assert_eq!(
            content_encoding("/targz", Some("zstd, br, gzip")).await,
            None,
            "a gzip archive is never re-encoded"
        );
        assert_eq!(
            content_encoding("/zip", Some("zstd, br, gzip")).await,
            None,
            "a zip archive is never re-encoded"
        );
    }

    #[tokio::test]
    async fn zstd_outranks_brotli_outranks_gzip_on_equal_quality() {
        assert_eq!(
            content_encoding("/json", Some("gzip, br, zstd"))
                .await
                .as_deref(),
            Some("zstd"),
            "zstd wins over brotli and gzip at equal q"
        );
        assert_eq!(
            content_encoding("/json", Some("gzip, br")).await.as_deref(),
            Some("br"),
            "brotli wins over gzip at equal q"
        );
        assert_eq!(
            content_encoding("/json", Some("gzip")).await.as_deref(),
            Some("gzip"),
            "gzip serves the client that offers only gzip"
        );
    }

    #[tokio::test]
    async fn nothing_compresses_without_accept_encoding_or_below_the_floor() {
        assert_eq!(content_encoding("/json", None).await, None);
        assert_eq!(
            content_encoding("/small", Some("zstd, br, gzip")).await,
            None,
            "a body below the size floor is left alone"
        );
    }

    #[tokio::test]
    async fn a_mixed_case_content_type_still_compresses() {
        let negotiated = ["zstd", "br", "gzip"];
        let cased = content_encoding("/cased", Some("zstd, br, gzip")).await;
        assert!(
            cased
                .as_deref()
                .is_some_and(|enc| negotiated.contains(&enc)),
            "content-type matching is case insensitive, got {cased:?}"
        );
    }

    #[tokio::test]
    async fn a_partial_archive_passes_through_with_its_range_intact() {
        let mut builder = http::Request::builder().method("GET").uri("/partial");
        builder = builder.header(ACCEPT_ENCODING, "zstd, br, gzip");
        builder = builder.header(RANGE, "bytes=0-3");
        let response = app()
            .oneshot(builder.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.headers().get(CONTENT_RANGE).unwrap(),
            "bytes 0-3/4096",
            "the range survives the compression layer untouched"
        );
        assert_eq!(
            response.headers().get(CONTENT_ENCODING),
            None,
            "a partial archive is never re-encoded"
        );
    }
}
