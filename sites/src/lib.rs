use std::collections::HashMap;

use serde::Deserialize;
use worker::*;

/// The JSON value stored in Workers KV, keyed by domain.
///
/// Example KV entry:
///   key:   "foo.example.com"
///   value: {"did": "did:plc:...",
///           "repos": {"my_repo":    {"rkey": "3lk...", "is_index": true},
///                     "other_repo": {"rkey": "3ll...", "is_index": false}}}
///
/// The is_index flag on each entry indicates whether it is the index site
/// for the domain (true) or a sub-path site (false). At most one repo may
/// be true. The rkey identifies the {did}/{rkey}/ prefix in R2 where the
/// site's objects live.
#[derive(Deserialize)]
struct DomainMapping {
    #[serde(default)]
    did: String,
    /// repo name → entry
    #[serde(default)]
    repos: HashMap<String, RepoEntry>,
}

/// Deserialises from either {"rkey": "...", "is_index": bool} (new shape)
/// or a bare bool (old shape, where the map key itself was the rkey).
#[derive(Deserialize)]
#[serde(untagged)]
enum RepoEntry {
    New {
        rkey: String,
        #[serde(default)]
        is_index: bool,
    },
    Legacy(bool),
}

impl RepoEntry {
    fn is_index(&self) -> bool {
        match self {
            RepoEntry::New { is_index, .. } => *is_index,
            RepoEntry::Legacy(b) => *b,
        }
    }

    /// Returns the rkey, falling back to the map key (name) for the legacy
    /// shape where the key itself was the rkey.
    fn rkey<'a>(&'a self, name: &'a str) -> &'a str {
        match self {
            RepoEntry::New { rkey, .. } => rkey.as_str(),
            RepoEntry::Legacy(_) => name,
        }
    }
}

impl DomainMapping {
    /// Returns the (name, entry) pair for the index site, if any.
    fn index_repo(&self) -> Option<(&str, &RepoEntry)> {
        self.repos.iter().find_map(|(name, entry)| {
            if entry.is_index() {
                Some((name.as_str(), entry))
            } else {
                None
            }
        })
    }
}

/// Build the R2 object key for a given did/rkey and intra-site path.
/// `site_path` should start with a `/` or be empty.
fn r2_key(did: &str, rkey: &str, site_path: &str) -> String {
    let base = format!("{}/{}/", did, rkey);
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

/// Returns true when a directory-like path is missing a trailing slash.
///
/// Examples:
/// - "/docs" => true
/// - "/docs/" => false
/// - "/file.txt" => false
/// - "/" => false
fn needs_trailing_slash(path: &str) -> bool {
    if path == "/" || path.ends_with('/') {
        return false;
    }
    let last_segment = path.rsplit('/').next().unwrap_or(path);
    !last_segment.contains('.')
}

/// Return the canonical URL with a trailing slash appended to the path.
fn with_trailing_slash(url: &Url) -> String {
    let mut url = url.clone();
    url.set_path(&format!("{}/", url.path()));
    url.to_string()
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

    let body = obj
        .body()
        .ok_or_else(|| Error::RustError("empty R2 body".into()))?;
    let mut resp = Response::from_body(body.response_body()?)?;
    resp.headers_mut().set("Content-Type", &content_type)?;
    resp.headers_mut()
        .set("Cache-Control", "public, max-age=60")?;
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

    // Canonical redirect for directory-like paths.
    if needs_trailing_slash(path) {
        let redirect_url = with_trailing_slash(&url);
        return Response::redirect(redirect_url.parse()?, 308);
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
        if let Some(entry) = mapping.repos.get(&first_segment) {
            if !entry.is_index() {
                // Strip the leading "/{first_segment}" to get the intra-site path.
                let site_path = path
                    .trim_start_matches('/')
                    .trim_start_matches(&first_segment)
                    .to_string();

                let key = r2_key(&mapping.did, entry.rkey(&first_segment), &site_path);
                return match fetch_from_r2(&bucket, &key).await? {
                    Some(obj) => response_from_object(obj),
                    None => Response::error("Not Found", 404),
                };
            }
        }
    }

    // 2. index site
    // Fall back to the repo marked as the index site, serving the full path.
    if let Some((name, entry)) = mapping.index_repo() {
        let key = r2_key(&mapping.did, entry.rkey(name), path);
        return match fetch_from_r2(&bucket, &key).await? {
            Some(obj) => response_from_object(obj),
            None => Response::error("Not Found", 404),
        };
    }

    Response::error("Not Found", 404)
}
