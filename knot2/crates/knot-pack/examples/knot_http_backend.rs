use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use knot_git::Repo;

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const SUFFIXES: [&str; 3] = ["/info/refs", "/git-upload-pack", "/git-receive-pack"];

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn resolve() -> Option<(PathBuf, &'static str)> {
    let path_info = env("PATH_INFO")?;
    let (repo_sub, suffix) = SUFFIXES
        .iter()
        .find_map(|suffix| path_info.strip_suffix(suffix).map(|rest| (rest, *suffix)))?;
    let root = env("GIT_PROJECT_ROOT")
        .filter(|root| !root.is_empty())
        .or_else(|| {
            let translated = env("PATH_TRANSLATED")?;
            translated
                .strip_suffix(path_info.as_str())
                .map(str::to_string)
        })?;
    let repo_sub = repo_sub.trim_start_matches('/');
    Some((Path::new(&root).join(repo_sub), suffix))
}

fn wants_v2() -> bool {
    env("HTTP_GIT_PROTOCOL")
        .or_else(|| env("GIT_PROTOCOL"))
        .is_some_and(|value| value.split(':').any(|token| token.trim() == "version=2"))
}

fn dev_push_allowed() -> bool {
    env("KNOT_DEV_ALLOW_HTTP_PUSH").is_some_and(|value| {
        let value = value.trim();
        value == "1" || value.eq_ignore_ascii_case("true")
    })
}

fn dev_pack_limits() -> knot_pack::PackLimits {
    let mut limits = knot_pack::PackLimits::default();
    if let Some(value) = env("KNOT_DEV_MAX_OBJECTS").and_then(|value| value.trim().parse().ok()) {
        limits.max_objects = knot_types::ObjectCount::new(value);
    }
    if let Some(value) = env("KNOT_DEV_MAX_TOTAL_BYTES").and_then(|value| value.trim().parse().ok())
    {
        limits.max_total_bytes = knot_pack::MaxTotalBytes::new(value);
    }
    limits
}

fn apply_dev_selection_limits() {
    let max_objects = env("KNOT_DEV_SELECTION_MAX_OBJECTS")
        .and_then(|value| value.trim().parse().ok())
        .map(knot_types::ObjectCount::new);
    let secs =
        env("KNOT_DEV_SELECTION_TIME_BUDGET_SECS").and_then(|value| value.trim().parse().ok());
    if max_objects.is_none() && secs.is_none() {
        return;
    }
    let base = knot_pack::SelectionLimits::default();
    knot_pack::init_selection_limits(knot_pack::SelectionLimits {
        max_objects: max_objects.unwrap_or(base.max_objects),
        time_budget: secs
            .map(std::time::Duration::from_secs)
            .unwrap_or(base.time_budget),
    });
}

fn apply_dev_resources() {
    let max_threads = env("KNOT_DEV_MAX_THREADS")
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|threads| *threads != 0)
        .map(knot_resource::ThreadCount::new);
    knot_resource::init(knot_resource::Ceilings {
        max_threads,
        ..knot_resource::Ceilings::default()
    });
}

fn read_body() -> Vec<u8> {
    let mut raw = Vec::new();
    std::io::stdin().read_to_end(&mut raw).ok();
    let gzipped = env("HTTP_CONTENT_ENCODING").is_some_and(|value| {
        value
            .split(',')
            .any(|token| token.trim().eq_ignore_ascii_case("gzip"))
    });
    if !gzipped {
        return raw;
    }
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(raw.as_slice())
        .read_to_end(&mut decoded)
        .ok();
    decoded
}

fn emit(out: &mut dyn Write, content_type: &str, body: &[u8]) {
    let _ = write!(
        out,
        "Expires: Fri, 01 Jan 1980 00:00:00 GMT\r\nPragma: no-cache\r\nCache-Control: no-cache, max-age=0, must-revalidate\r\nContent-Type: {content_type}\r\n\r\n"
    );
    let _ = out.write_all(body);
}

fn fail(out: &mut dyn Write, status: &str, message: &str) {
    let _ = write!(
        out,
        "Status: {status}\r\nContent-Type: text/plain\r\n\r\n{message}\n"
    );
}

const PUSH_REFUSED: &str = "knot accepts pushes over SSH, not HTTP";

fn serve_receive_advert(out: &mut dyn Write, repo: &Repo) {
    if !dev_push_allowed() {
        return fail(out, "403 Forbidden", PUSH_REFUSED);
    }
    match knot_pack::advertise_receive(repo) {
        Ok(body) => emit(out, "application/x-git-receive-pack-advertisement", &body),
        Err(error) => fail(out, "500 Internal Server Error", &error.to_string()),
    }
}

fn serve_receive_pack(out: &mut dyn Write, repo: &Repo) {
    if !dev_push_allowed() {
        return fail(out, "403 Forbidden", PUSH_REFUSED);
    }
    match knot_pack::receive_pack_with_limits(repo, &read_body(), &dev_pack_limits()) {
        Ok(result) => emit(out, "application/x-git-receive-pack-result", &result),
        Err(error) => fail(out, "500 Internal Server Error", &error.to_string()),
    }
}

fn main() {
    apply_dev_selection_limits();
    apply_dev_resources();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let (repo_dir, suffix) = match resolve() {
        Some(parts) => parts,
        None => {
            return fail(
                &mut out,
                "400 Bad Request",
                "unrecognized git smart-http path",
            );
        }
    };
    let repo = match Repo::open(&repo_dir) {
        Ok(repo) => repo,
        Err(_) => return fail(&mut out, "404 Not Found", "no such repository"),
    };
    let method = env("REQUEST_METHOD").unwrap_or_default();

    match (method.as_str(), suffix) {
        ("GET", "/info/refs") => {
            let service = env("QUERY_STRING")
                .and_then(|query| {
                    query
                        .split('&')
                        .find_map(|pair| pair.strip_prefix("service=").map(str::to_string))
                })
                .unwrap_or_default();
            match service.as_str() {
                "git-upload-pack" => {
                    let body = if wants_v2() {
                        knot_pack::advertise_upload(&repo)
                    } else {
                        knot_pack::advertise_upload_v0(&repo)
                    };
                    match body {
                        Ok(body) => emit(
                            &mut out,
                            "application/x-git-upload-pack-advertisement",
                            &body,
                        ),
                        Err(error) => {
                            fail(&mut out, "500 Internal Server Error", &error.to_string())
                        }
                    }
                }
                "git-receive-pack" => serve_receive_advert(&mut out, &repo),
                _ => fail(&mut out, "403 Forbidden", "unsupported service"),
            }
        }
        ("POST", "/git-upload-pack") => match knot_pack::upload_pack(&repo, &read_body()) {
            Ok(result) => emit(&mut out, "application/x-git-upload-pack-result", &result),
            Err(error) => fail(&mut out, "500 Internal Server Error", &error.to_string()),
        },
        ("POST", "/git-receive-pack") => serve_receive_pack(&mut out, &repo),
        _ => fail(
            &mut out,
            "400 Bad Request",
            "unsupported git smart-http request",
        ),
    }
}
