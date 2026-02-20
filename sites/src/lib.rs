use std::collections::HashMap;

use serde::Deserialize;
use worker::*;

/// The JSON value stored in Workers KV, keyed by domain.
///
/// Example KV entry:
///   key:   "foo.example.com"
///   value: {"did": "did:plc:...", "repos": {"my_repo": true, "other_repo": false}}
///
/// The boolean on each repo indicates whether it is the index site for the
/// domain (true) or a sub-path site (false). At most one repo may be true.
#[derive(Deserialize)]
struct DomainMapping {
    did: String,
    /// repo name → is_index
    repos: HashMap<String, bool>,
}

impl DomainMapping {
    /// Returns the repo that is marked as the index site, if any.
    fn index_repo(&self) -> Option<&str> {
        self.repos
            .iter()
            .find_map(|(name, &is_index)| if is_index { Some(name.as_str()) } else { None })
    }
}

/// Build the R2 object key for a given did/repo and intra-site path.
/// `site_path` should start with a `/` or be empty.
fn r2_key(did: &str, repo: &str, site_path: &str) -> String {
    let base = format!("{}/{}/", did, repo);
    if site_path.is_empty() || site_path == "/" {
        format!("{}index.html", base)
    } else {
        let trimmed = site_path.trim_start_matches('/');
        if trimmed.is_empty() || trimmed.ends_with('/') {
            format!("{}{}index.html", base, trimmed)
        } else {
            format!("{}{}", base, trimmed)
        }
    }
}

/// Fetch an object from R2, falling back to appending /index.html if the
/// key looks like a directory (no file extension in the last segment).
async fn fetch_from_r2(bucket: &Bucket, key: &str) -> Result<Option<Object>> {
    if let Some(obj) = bucket.get(key).execute().await? {
        return Ok(Some(obj));
    }

    let last_segment = key.rsplit('/').next().unwrap_or(key);
    if !last_segment.contains('.') {
        let index_key = format!("{}/index.html", key.trim_end_matches('/'));
        if let Some(obj) = bucket.get(&index_key).execute().await? {
            return Ok(Some(obj));
        }
    }

    Ok(None)
}

/// Build a Response from an R2 Object, forwarding the content-type header.
fn response_from_object(obj: Object) -> Result<Response> {
    let content_type = obj
        .http_metadata()
        .content_type
        .unwrap_or_else(|| "application/octet-stream".to_string());

    let body = obj.body().ok_or_else(|| Error::RustError("empty R2 body".into()))?;
    let mut resp = Response::from_body(body.response_body()?)?;
    resp.headers_mut().set("Content-Type", &content_type)?;
    resp.headers_mut().set("Cache-Control", "public, max-age=60")?;
    Ok(resp)
}

fn is_excluded(path: &str) -> bool {
    let excluded = ["/.well-known/atproto-did"];
    excluded.iter().any(|&prefix| path.starts_with(prefix))
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let kv = env.kv("SITES")?;
    let bucket = env.bucket("SITES_BUCKET")?;

    // Extract host, stripping any port.
    let host = req.headers().get("host")?.unwrap_or_default();
    let host = host.split(':').next().unwrap_or("").to_string();

    if host.is_empty() {
        return Response::error("Bad Request: missing host", 400);
    }

    let url = req.url()?;
    let path = url.path();

    if is_excluded(path) {
        return Fetch::Request(req).send().await;
    }

    // Single KV lookup for the whole domain.
    let mapping = match kv.get(&host).text().await? {
        Some(raw) => match serde_json::from_str::<DomainMapping>(&raw) {
            Ok(m) => m,
            Err(_) => return Response::error("Internal Error: bad mapping", 500),
        },
        None => return Response::error("site not found!", 404),
    };

    let path = url.path(); // always starts with "/"

    // First path segment, e.g. "my_repo" from "/my_repo/page.html"
    let first_segment = path
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or("")
        .to_string();

    // 1. sub-path site
    // If the first path segment matches a non-index repo, serve from it.
    if !first_segment.is_empty() {
        if let Some(&is_index) = mapping.repos.get(&first_segment) {
            if !is_index {
                // Strip the leading "/{first_segment}" to get the intra-site path.
                let site_path = path
                    .trim_start_matches('/')
                    .trim_start_matches(&first_segment)
                    .to_string();

                let key = r2_key(&mapping.did, &first_segment, &site_path);
                return match fetch_from_r2(&bucket, &key).await? {
                    Some(obj) => response_from_object(obj),
                    None => Response::error("Not Found", 404),
                };
            }
        }
    }

    // 2. index site
    // Fall back to the repo marked as the index site, serving the full path.
    if let Some(index_repo) = mapping.index_repo() {
        let key = r2_key(&mapping.did, index_repo, path);
        return match fetch_from_r2(&bucket, &key).await? {
            Some(obj) => response_from_object(obj),
            None => Response::error("Not Found", 404),
        };
    }

    Response::error("Not Found", 404)
}
