mod common;

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::stream;
use http::{HeaderMap, StatusCode, header};
use tokio_tungstenite::tungstenite;

use knot_events::{EventCursor, GitRefUpdate};
use knot_types::{AccountDid, Oid, OwnerDid, RepoDid};
use knot_xrpc::{ArchiveLimit, ResponseLimit};

use common::{
    OWNER, World, archive_full, assert_immutable_round_trip, assert_post_rejected, assert_warming,
    commit_file, empty_repo, get, get_error, get_json, get_with_headers, git_run, post_authed,
    post_json, ref_names, repo_dids, seeded, seeded_feature_branch, sh_git, sh_git_at,
};

#[tokio::test]
async fn the_seeded_read_surface_renders_each_wire_shape_once() {
    let world = World::new();
    let (did, work) = seeded(&world, "coral");
    let head = sh_git(work.path(), &["rev-parse", "HEAD"]);
    let parent = sh_git(work.path(), &["rev-parse", "HEAD~1"]);
    let tag_object = sh_git(work.path(), &["rev-parse", "v1.0.0"]);
    let tagged_commit = sh_git(work.path(), &["rev-parse", "v1.0.0^{commit}"]);

    let tree = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.tree?repo={did}&ref=main"),
    )
    .await;
    assert_eq!(tree["ref"], "main");
    assert!(tree.get("parent").is_none());
    assert!(tree.get("dotdot").is_none());
    let files = tree["files"].as_array().unwrap();
    let names: Vec<&str> = files
        .iter()
        .map(|file| file["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["README.md", "logo.png", "src"]);
    let readme_entry = &files[0];
    assert_eq!(readme_entry["mode"], "0100644");
    assert_eq!(
        readme_entry["size"].as_i64().unwrap(),
        b"# coral\n\nhello reef\n".len() as i64
    );
    assert_eq!(
        readme_entry["last_commit"]["hash"].as_str().unwrap(),
        head,
        "README was last touched by the head commit"
    );
    assert_eq!(readme_entry["last_commit"]["message"], "update readme");
    assert_eq!(files[2]["mode"], "0040000");
    assert_eq!(tree["readme"]["filename"], "README.md");
    assert_eq!(tree["readme"]["contents"], "# coral\n\nhello reef\n");
    assert_eq!(tree["lastCommit"]["hash"], head.as_str());
    assert_eq!(tree["lastCommit"]["author"]["name"], "nel");
    assert_eq!(tree["lastCommit"]["author"]["when"], "");

    let sub = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.tree?repo={did}&ref=main&path=src"),
    )
    .await;
    assert_eq!(sub["parent"], "src");
    assert!(sub.get("dotdot").is_none());
    assert_eq!(sub["files"][0]["name"], "main.rs");
    assert_eq!(
        get_error(
            &world,
            &format!("/xrpc/sh.tangled.repo.tree?repo={did}&ref=main&path=nope"),
        )
        .await,
        (StatusCode::NOT_FOUND, "PathNotFound".to_string())
    );

    let log = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.log?repo={did}&ref=main"),
    )
    .await;
    assert_eq!(log["total"].as_i64(), Some(4));
    assert_eq!(log["page"].as_i64(), Some(1));
    assert_eq!(log["per_page"].as_i64(), Some(50));
    assert_eq!(log["log"], true);
    assert_eq!(log["ref"], "main");
    let commits = log["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 4);
    let first = &commits[0];
    let hash_bytes: Vec<u8> = first["hash"]
        .as_array()
        .unwrap()
        .iter()
        .map(|byte| byte.as_u64().unwrap() as u8)
        .collect();
    assert_eq!(
        hash_bytes,
        (0..head.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&head[index..index + 2], 16).unwrap())
            .collect::<Vec<u8>>(),
        "commit hash rides as a byte array"
    );
    assert_eq!(first["this"], head.as_str());
    assert_eq!(first["parent"], parent.as_str());
    assert_eq!(first["author"]["Name"], "nel");
    assert_eq!(first["author"]["Email"], "nel@oyster.cafe");
    assert_eq!(first["author"]["When"], "2026-06-01T12:33:00+02:00");
    assert_eq!(first["message"], "update readme\n");
    assert!(first["tree"].as_str().unwrap().len() == 40);
    let paged = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.log?repo={did}&ref=main&limit=2&cursor=2"),
    )
    .await;
    assert_eq!(paged["commits"].as_array().unwrap().len(), 2);
    assert_eq!(paged["page"].as_i64(), Some(2));
    assert_eq!(paged["per_page"].as_i64(), Some(2));

    let branches = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.branches?repo={did}"),
    )
    .await;
    let listed = branches["branches"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["reference"]["name"], "main");
    assert_eq!(listed[0]["reference"]["hash"], head.as_str());
    assert_eq!(listed[0]["is_default"], true);
    assert_eq!(listed[0]["commit"]["Author"]["Name"], "nel");
    assert!(listed[0]["commit"]["Hash"].is_array());
    assert_eq!(listed[0]["commit"]["ExtraHeaders"], serde_json::Value::Null);
    assert_eq!(listed[0]["commit"]["Message"], "update readme");
    let branch = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.branch?repo={did}&name=main"),
    )
    .await;
    assert_eq!(branch["name"], "main");
    assert_eq!(branch["hash"], head.as_str());
    assert_eq!(branch["shortHash"], head[..7].to_string().as_str());
    assert_eq!(branch["isDefault"], true);
    assert_eq!(branch["author"]["name"], "nel");
    assert_eq!(branch["when"], "2026-06-01T12:33:00+02:00");
    assert_eq!(
        get_error(
            &world,
            &format!("/xrpc/sh.tangled.repo.branch?repo={did}&name=mangrove"),
        )
        .await,
        (StatusCode::NOT_FOUND, "BranchNotFound".to_string())
    );

    let tags = get_json(&world, &format!("/xrpc/sh.tangled.repo.tags?repo={did}")).await;
    let tag_list = tags["tags"].as_array().unwrap();
    assert_eq!(tag_list.len(), 2);
    let annotated = tag_list.iter().find(|tag| tag["name"] == "v1.0.0").unwrap();
    assert_eq!(annotated["hash"], tag_object.as_str());
    assert_eq!(annotated["message"], "release one");
    assert_eq!(annotated["tag"]["TargetType"].as_i64(), Some(4));
    assert_eq!(annotated["tag"]["Tagger"]["Name"], "nel");
    let target_bytes = annotated["tag"]["Target"].as_array().unwrap();
    assert_eq!(target_bytes.len(), 20);
    assert_eq!(
        target_bytes[0].as_u64().unwrap() as u8,
        u8::from_str_radix(&tagged_commit[..2], 16).unwrap()
    );
    let lightweight = tag_list
        .iter()
        .find(|tag| tag["name"] == "lightweight")
        .unwrap();
    assert!(lightweight.get("tag").is_none());
    assert_eq!(lightweight["hash"], tagged_commit.as_str());
    assert_eq!(lightweight["message"], "add logo");
    let single = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.tag?repo={did}&tag=v1.0.0"),
    )
    .await;
    assert_eq!(single["tag"]["name"], "v1.0.0");
    assert_eq!(
        get_error(
            &world,
            &format!("/xrpc/sh.tangled.repo.tag?repo={did}&tag=v9.9.9"),
        )
        .await,
        (StatusCode::BAD_REQUEST, "TagNotFound".to_string())
    );

    let text = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.blob?repo={did}&ref=main&path=README.md"),
    )
    .await;
    assert_eq!(text["encoding"], "utf-8");
    assert_eq!(text["isBinary"], false);
    assert_eq!(text["content"], "# coral\n\nhello reef\n");
    assert_eq!(text["mimeType"], "text/plain; charset=utf-8");
    assert_eq!(text["lastCommit"]["message"], "update readme");

    let binary = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.blob?repo={did}&ref=main&path=logo.png"),
    )
    .await;
    assert_eq!(binary["encoding"], "base64");
    assert_eq!(binary["isBinary"], true);
    assert_eq!(binary["mimeType"], "image/png");

    let (status, headers, body) = get(
        &world,
        &format!("/xrpc/sh.tangled.repo.blob?repo={did}&ref=main&path=logo.png&raw=true"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "image/png");
    assert_eq!(
        headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
        "nosniff"
    );
    assert_eq!(
        headers.get(header::CONTENT_SECURITY_POLICY).unwrap(),
        "default-src 'none'; style-src 'unsafe-inline'; sandbox"
    );
    let etag = headers
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(body.starts_with(b"\x89PNG"));

    let mut cached = HeaderMap::new();
    cached.insert(
        header::IF_NONE_MATCH,
        http::HeaderValue::from_str(&etag).unwrap(),
    );
    let (status, _, _) = get_with_headers(
        &world,
        &format!("/xrpc/sh.tangled.repo.blob?repo={did}&ref=main&path=logo.png&raw=true"),
        cached,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);

    let mut weak = HeaderMap::new();
    weak.insert(
        header::IF_NONE_MATCH,
        http::HeaderValue::from_str(&format!("W/{etag}")).unwrap(),
    );
    let (status, _, _) = get_with_headers(
        &world,
        &format!("/xrpc/sh.tangled.repo.blob?repo={did}&ref=main&path=logo.png&raw=true"),
        weak,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_MODIFIED,
        "weak validator must revalidate too"
    );
    assert_eq!(
        get_error(
            &world,
            &format!("/xrpc/sh.tangled.repo.blob?repo={did}&ref=main&path=ghost.txt"),
        )
        .await,
        (StatusCode::NOT_FOUND, "FileNotFound".to_string())
    );
}

#[tokio::test]
async fn tree_directory_last_commit_is_the_newest_touching_commit() {
    let world = World::new();
    let (did, bare, work) = empty_repo(&world, "periwinkle");
    let work = work.path();
    commit_file(
        work,
        "src/a.rs",
        b"fn a() {}\n",
        "add a",
        "2026-06-01T12:30:00+02:00",
    );
    let older = sh_git(work, &["rev-parse", "HEAD"]);
    commit_file(
        work,
        "src/b.rs",
        b"fn b() {}\n",
        "add b",
        "2026-06-01T12:31:00+02:00",
    );
    let newer = sh_git(work, &["rev-parse", "HEAD"]);
    commit_file(
        work,
        "README.md",
        b"# periwinkle\n",
        "doc",
        "2026-06-01T12:33:00+02:00",
    );
    let head = sh_git(work, &["rev-parse", "HEAD"]);
    sh_git(work, &["push", "-q", &bare, "main"]);

    let value = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.tree?repo={did}&ref=main"),
    )
    .await;
    let files = value["files"].as_array().unwrap();
    let src = files
        .iter()
        .find(|file| file["name"] == "src")
        .expect("src directory is listed");
    let reported = src["last_commit"]["hash"].as_str().unwrap();
    assert_eq!(
        reported, newer,
        "the newest commit that touches the subtree is reported, not the oldest"
    );
    assert_ne!(reported, older, "not the first commit that created subtree");
    assert_ne!(
        reported, head,
        "head commit only touched README, never the src subtree"
    );
}

#[tokio::test]
async fn languages_timeout_yields_a_partial_answer_not_an_error() {
    let world = World::new();
    let (did, work) = seeded(&world, "scallop");
    let head = Oid::from_hex(&sh_git(work.path(), &["rev-parse", "HEAD"])).unwrap();
    let repo = world.layout.open(&did).unwrap();

    let full =
        knot_langs::analyze(&repo, head, Some(Instant::now() + Duration::from_secs(60))).unwrap();
    assert!(
        full.values().any(|size| size.get() > 0),
        "generous budget detects code"
    );

    let expired = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    let partial = knot_langs::analyze(&repo, head, Some(expired)).unwrap();
    assert!(
        partial.is_empty(),
        "exhausted budget breaks the walk and returns the partial map gathered so far, never an error"
    );
}

#[tokio::test]
async fn compare_format_patch_keeps_a_non_ascii_author_raw() {
    let world = World::new();
    let (did, bare, work) = empty_repo(&world, "mussel");
    let work = work.path();
    commit_file(
        work,
        "README.md",
        b"# mussel\n",
        "first",
        "2026-06-01T12:30:00+02:00",
    );
    let base = sh_git(work, &["rev-parse", "HEAD"]);
    std::fs::write(work.join("src.rs"), b"fn main() {}\n").unwrap();
    let author = ("Lýna Þórsdóttir", "lyna@nel.pet");
    git_run(work, "2026-06-01T12:31:00+02:00", author, &["add", "-A"]);
    git_run(
        work,
        "2026-06-01T12:31:00+02:00",
        author,
        &["commit", "-q", "-m", "café changes"],
    );
    let head = sh_git(work, &["rev-parse", "HEAD"]);
    sh_git(work, &["push", "-q", &bare, "main"]);

    let value = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.compare?repo={did}&rev1={base}&rev2={head}"),
    )
    .await;
    let entry = &value["format_patch"][0];
    assert_eq!(
        entry["Author"]["Name"], "Lýna Þórsdóttir",
        "structured author the appview renders keeps the raw unicode"
    );
    assert_eq!(entry["Author"]["Email"], "lyna@nel.pet");
    assert_eq!(
        entry["Title"], "café changes",
        "structured subject the appview renders keeps the raw unicode"
    );
    assert_eq!(entry["RawHeaders"]["Subject"][0], "[PATCH] café changes");
    assert_eq!(
        entry["RawHeaders"]["From"][0],
        "Lýna Þórsdóttir <lyna@nel.pet>"
    );
    let raw = entry["Raw"].as_str().unwrap();
    assert!(
        raw.contains("From: Lýna Þórsdóttir <lyna@nel.pet>"),
        "knot emits the raw UTF-8 author instead of RFC2047 Q-encoding real format-patch uses"
    );
    assert!(
        !raw.contains("=?UTF-8?") && !raw.contains("=?utf-8?"),
        "no MIME word-encoding headers"
    );
    assert!(raw.contains("Subject: [PATCH] café changes"));
    assert!(
        raw.ends_with("-- \nknot"),
        "knot signs the patch w/ its own trailer"
    );
}

#[tokio::test]
async fn compare_reports_the_merge_base_of_diverged_branches() {
    let world = World::new();
    let (did, work) = seeded(&world, "woofie");
    let work = work.path();
    let bare = world.layout.repo_path(&did).unwrap();
    let merge_base = sh_git(work, &["rev-parse", "HEAD"]);

    sh_git(work, &["checkout", "-q", "-b", "feature"]);
    commit_file(
        work,
        "feature.txt",
        b"feature\n",
        "feature moved",
        "2026-08-11T12:40:00+02:00",
    );
    sh_git(
        work,
        &[
            "push",
            "-q",
            bare.to_str().unwrap(),
            "HEAD:refs/heads/feature",
        ],
    );

    sh_git(work, &["checkout", "-q", "main"]);
    commit_file(
        work,
        "main.txt",
        b"main\n",
        "main moved",
        "2026-08-11T12:41:00+02:00",
    );
    sh_git(work, &["push", "-q", bare.to_str().unwrap(), "main"]);

    let value = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.compare?repo={did}&rev1=feature&rev2=main"),
    )
    .await;

    assert_eq!(value["merge_base"], merge_base.as_str());
}

#[tokio::test]
async fn diff_reports_structured_fragments_and_stats() {
    let world = World::new();
    let (did, work) = seeded(&world, "whelk");
    let head = sh_git(work.path(), &["rev-parse", "HEAD"]);

    let value = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.diff?repo={did}&ref={head}"),
    )
    .await;
    assert_eq!(value["ref"], head.as_str());
    let diff = &value["diff"];
    assert_eq!(diff["stat"]["files_changed"].as_i64(), Some(1));
    assert_eq!(diff["stat"]["insertions"].as_i64(), Some(1));
    assert_eq!(diff["stat"]["deletions"].as_i64(), Some(1));
    let file = &diff["diff"][0];
    assert_eq!(file["name"]["new"], "README.md");
    assert_eq!(file["is_new"], false);
    let fragment = &file["text_fragments"][0];
    assert_eq!(fragment["OldPosition"].as_i64(), Some(1));
    assert_eq!(fragment["Comment"], "");
    let lines = fragment["Lines"].as_array().unwrap();
    assert!(
        lines
            .iter()
            .any(|line| line["Op"].as_i64() == Some(1) && line["Line"] == "hello\n")
    );
    assert!(
        lines
            .iter()
            .any(|line| line["Op"].as_i64() == Some(2) && line["Line"] == "hello reef\n")
    );
    assert_eq!(diff["commit"]["this"], head.as_str());
}

#[tokio::test]
async fn compare_produces_format_patches_and_a_combined_patch() {
    let world = World::new();
    let (did, work) = seeded(&world, "conch");
    let head = sh_git(work.path(), &["rev-parse", "HEAD"]);
    let base = sh_git(work.path(), &["rev-parse", "HEAD~3"]);

    let value = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.compare?repo={did}&rev1={base}&rev2={head}"),
    )
    .await;
    assert_eq!(value["rev1"], base.as_str());
    assert_eq!(value["rev2"], head.as_str());
    let patches = value["format_patch"].as_array().unwrap();
    assert_eq!(patches.len(), 3, "three commits separate base from head");
    let first = &patches[0];
    assert_eq!(first["Title"], "add main");
    assert_eq!(first["SubjectPrefix"], "[PATCH] ");
    assert_eq!(first["Committer"], serde_json::Value::Null);
    assert_eq!(first["CommitterDate"], "0001-01-01T00:00:00Z");
    assert_eq!(first["Author"]["Name"], "nel");
    assert_eq!(first["AuthorDate"], "2026-06-01T12:31:00+02:00");
    assert_eq!(first["RawHeaders"]["Subject"][0], "[PATCH] add main");
    let raw = first["Raw"].as_str().unwrap();
    assert!(raw.starts_with(&format!(
        "From {} Mon Sep 17 00:00:00 2001\n",
        sh_git(work.path(), &["rev-parse", "HEAD~2"])
    )));
    assert!(raw.contains("Subject: [PATCH] add main"));
    assert!(raw.contains("diff --git a/src/main.rs b/src/main.rs"));
    assert!(raw.contains("new file mode 100644"));
    assert!(first["Files"][0]["NewName"] == "src/main.rs");
    assert!(first["Files"][0]["IsNew"] == true);

    assert!(value["patch"].as_str().unwrap().contains("add logo"));
    let combined = value["combined_patch"].as_array().unwrap();
    assert!(combined.iter().any(|file| file["NewName"] == "README.md"));
    assert!(
        value["combined_patch_raw"]
            .as_str()
            .unwrap()
            .contains("diff --git a/README.md b/README.md")
    );

    assert_eq!(
        get_error(
            &world,
            &format!("/xrpc/sh.tangled.repo.compare?repo={did}&rev1=nope&rev2={head}"),
        )
        .await,
        (StatusCode::BAD_REQUEST, "RevisionNotFound".to_string())
    );
}

#[tokio::test]
async fn archive_conditional_and_range_semantics() {
    let world = World::new();
    let (did, _work) = seeded(&world, "nautilus");
    let path = format!("/xrpc/sh.tangled.repo.archive?repo={did}&ref=main");

    let (status, headers, full) = get(&world, &path).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/gzip"
    );
    assert_eq!(
        headers
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap(),
        "attachment; filename=\"nautilus-main.tar.gz\""
    );
    let link = headers.get(header::LINK).unwrap().to_str().unwrap();
    assert!(link.contains("rel=\"immutable\""));
    assert!(link.contains("/xrpc/sh.tangled.repo.archive?format=tar.gz"));
    assert_eq!(headers.get(header::ACCEPT_RANGES).unwrap(), "bytes");
    let last_modified = headers
        .get(header::LAST_MODIFIED)
        .expect("a pinned modification time backs date revalidation")
        .to_str()
        .unwrap()
        .to_string();
    let etag = headers
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        etag.starts_with('"') && etag.ends_with('"'),
        "a strong etag is quoted"
    );
    assert_eq!(&full[..2], &[0x1f, 0x8b]);

    let mut range = HeaderMap::new();
    range.insert(header::RANGE, "bytes=0-3".parse().unwrap());
    let (status, range_headers, partial) = get_with_headers(&world, &path, range).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        range_headers
            .get(header::CONTENT_RANGE)
            .unwrap()
            .to_str()
            .unwrap(),
        format!("bytes 0-3/{}", full.len())
    );
    assert_eq!(
        partial.as_ref(),
        &full[..4],
        "a resumed range regenerates byte for byte"
    );

    let mut conditional = HeaderMap::new();
    conditional.insert(header::IF_NONE_MATCH, etag.parse().unwrap());
    let (status, cond_headers, conditional_body) =
        get_with_headers(&world, &path, conditional).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(
        cond_headers.get(header::ETAG).unwrap().to_str().unwrap(),
        etag
    );
    assert!(conditional_body.is_empty(), "a 304 has no body");

    let (status, _) = get_error(
        &world,
        &format!("/xrpc/sh.tangled.repo.archive?repo={did}&ref=main&format=tar.bz2"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let mut etag_match = HeaderMap::new();
    etag_match.insert(header::RANGE, "bytes=0-3".parse().unwrap());
    etag_match.insert(header::IF_RANGE, etag.parse().unwrap());
    let (status, if_range_headers, partial) = get_with_headers(&world, &path, etag_match).await;
    assert_eq!(
        status,
        StatusCode::PARTIAL_CONTENT,
        "a matching content etag resumes the range"
    );
    assert_eq!(
        if_range_headers
            .get(header::CONTENT_RANGE)
            .unwrap()
            .to_str()
            .unwrap(),
        format!("bytes 0-3/{}", full.len())
    );
    assert_eq!(partial.as_ref(), &full[..4]);

    let mut etag_stale = HeaderMap::new();
    etag_stale.insert(header::RANGE, "bytes=0-3".parse().unwrap());
    etag_stale.insert(header::IF_RANGE, "\"0000\"".parse().unwrap());
    let (status, _, body) = get_with_headers(&world, &path, etag_stale).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a stale content etag falls back to the full body"
    );
    assert_eq!(body, full, "the full archive comes back byte for byte");

    let mut weak = HeaderMap::new();
    weak.insert(header::RANGE, "bytes=0-3".parse().unwrap());
    weak.insert(header::IF_RANGE, format!("W/{etag}").parse().unwrap());
    let (status, _, body) = get_with_headers(&world, &path, weak).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a weak validator never serves a range, per strong-comparison rules"
    );
    assert_eq!(body, full);

    let mut date_match = HeaderMap::new();
    date_match.insert(header::RANGE, "bytes=0-3".parse().unwrap());
    date_match.insert(header::IF_RANGE, last_modified.parse().unwrap());
    let (status, date_headers, partial) = get_with_headers(&world, &path, date_match).await;
    assert_eq!(
        status,
        StatusCode::PARTIAL_CONTENT,
        "a date matching the pinned last-modified resumes the range"
    );
    assert_eq!(
        date_headers
            .get(header::CONTENT_RANGE)
            .unwrap()
            .to_str()
            .unwrap(),
        format!("bytes 0-3/{}", full.len())
    );
    assert_eq!(partial.as_ref(), &full[..4]);

    let mut date_stale = HeaderMap::new();
    date_stale.insert(header::RANGE, "bytes=0-3".parse().unwrap());
    date_stale.insert(
        header::IF_RANGE,
        "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
    );
    let (status, _, body) = get_with_headers(&world, &path, date_stale).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a date that doesn't match the pinned last-modified falls back to the full body"
    );
    assert_eq!(body, full);

    assert_immutable_round_trip(&world, &headers, &full, &etag).await;
}

#[tokio::test]
async fn archive_etag_distinguishes_refs_that_share_a_commit() {
    let world = World::new();
    let (did, work) = seeded(&world, "scallop");
    let bare = world.layout.repo_path(&did).unwrap();
    sh_git(work.path(), &["branch", "release", "main"]);
    sh_git(
        work.path(),
        &["push", "-q", bare.to_str().unwrap(), "refs/heads/release"],
    );

    let (main_etag, _main_last_modified, main_body) = archive_full(&world, &did).await;
    let (status, release_headers, release_body) = get(
        &world,
        &format!("/xrpc/sh.tangled.repo.archive?repo={did}&ref=release"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let release_etag = release_headers
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_ne!(
        main_etag, release_etag,
        "two refs at one commit name different archive prefixes, so the strong etag must differ"
    );
    assert_ne!(
        main_body, release_body,
        "the archives use different top-level directories and differ byte for byte"
    );

    let mut conditional = HeaderMap::new();
    conditional.insert(header::IF_NONE_MATCH, main_etag.parse().unwrap());
    let (status, _, _) = get_with_headers(
        &world,
        &format!("/xrpc/sh.tangled.repo.archive?repo={did}&ref=release"),
        conditional,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "main's etag mustn't satisfy a conditional request for the release archive"
    );
}

#[tokio::test]
async fn archive_link_advertises_the_prefix_it_served() {
    let world = World::new();
    let (did, _work) = seeded(&world, "periwinkle");
    let query = |suffix: &str| format!("/xrpc/sh.tangled.repo.archive?repo={did}&ref=main{suffix}");

    for (suffix, prefix, filename) in [
        ("", "periwinkle-main", "periwinkle-main.tar.gz"),
        ("&prefix=kelp/uni", "kelp%2Funi", "kelp-uni.tar.gz"),
        ("&prefix=/kelp//./uni/", "kelp%2Funi", "kelp-uni.tar.gz"),
    ] {
        let (status, headers, full) = get(&world, &query(suffix)).await;
        assert_eq!(status, StatusCode::OK);
        let link = headers[header::LINK].to_str().unwrap();
        assert!(
            link.contains(&format!("prefix={prefix}")),
            "the link repeats the prefix that the knot served, percent-encoded, since a stem built from the resolved commit would differ: {suffix} gave {link}"
        );
        assert_eq!(
            headers[header::CONTENT_DISPOSITION],
            format!("attachment; filename=\"{filename}\""),
            "the filename follows the prefix with the separator flattened, and the knot spells a did-addressed repo with the rkey it registered under"
        );
        let etag = headers[header::ETAG].to_str().unwrap().to_string();
        assert_immutable_round_trip(&world, &headers, &full, &etag).await;
    }

    for prefix in ["u".repeat(256), "kelp%5Cuni".to_string()] {
        assert_eq!(
            get_error(&world, &query(&format!("&prefix={prefix}"))).await,
            (StatusCode::BAD_REQUEST, "InvalidRequest".to_string()),
            "prefix {prefix}"
        );
    }
}

#[tokio::test]
async fn archive_serves_a_sha256_repo_with_a_stable_etag() {
    let world = World::sha256();
    let (did, _work) = seeded(&world, "nautilus");

    let (status, headers, full) = get(
        &world,
        &format!("/xrpc/sh.tangled.repo.archive?repo={did}&ref=main"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/gzip"
    );
    assert_eq!(&full[..2], &[0x1f, 0x8b]);
    let etag = headers
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    assert_immutable_round_trip(&world, &headers, &full, &etag).await;

    let mut conditional = HeaderMap::new();
    conditional.insert(header::IF_NONE_MATCH, etag.parse().unwrap());
    let (status, _, body) = get_with_headers(
        &world,
        &format!("/xrpc/sh.tangled.repo.archive?repo={did}&ref=main"),
        conditional,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_MODIFIED,
        "conditional revalidation works under sha256"
    );
    assert!(body.is_empty());
}

#[tokio::test]
async fn languages_detect_rust_and_markdown_stays_out() {
    let world = World::new();
    let (did, _work) = seeded(&world, "uni");

    let value = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.languages?repo={did}&ref=main"),
    )
    .await;
    let languages = value["languages"].as_array().unwrap();
    assert_eq!(
        languages.len(),
        1,
        "only Rust counts: markdown is prose, png is binary"
    );
    assert_eq!(languages[0]["name"], "Rust");
    assert_eq!(languages[0]["percentage"].as_i64(), Some(100));
    assert!(languages[0]["size"].as_i64().unwrap() > 0);
    assert_eq!(value["totalFiles"].as_i64(), Some(1));
}

#[tokio::test]
async fn repo_metadata_resolves_and_fails_closed() {
    let world = World::new();
    let (did, _work) = seeded(&world, "cuttle");

    let value = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.getDefaultBranch?repo={did}"),
    )
    .await;
    assert_eq!(value["name"], "main");
    assert_eq!(value["hash"], "");
    assert_eq!(value["when"], "1970-01-01T00:00:00Z");

    let described = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.describeRepo?repoDid={did}"),
    )
    .await;
    assert_eq!(described["repoDid"], did.as_str());
    assert_eq!(described["ownerDid"], OWNER);
    assert_eq!(described["rkey"], "cuttle");
    assert_eq!(
        get_error(
            &world,
            "/xrpc/sh.tangled.repo.describeRepo?repoDid=did:plc:doesnotexist",
        )
        .await,
        (StatusCode::NOT_FOUND, "RepoNotFound".to_string())
    );

    let by_owner = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.getDefaultBranch?repo={OWNER}/cuttle"),
    )
    .await;
    assert_eq!(by_owner["name"], "main");
    assert_eq!(
        get_error(
            &world,
            "/xrpc/sh.tangled.repo.getDefaultBranch?repo=did:plc:unregistered",
        )
        .await,
        (StatusCode::NOT_FOUND, "RepoNotFound".to_string())
    );
    let (status, _) = get_error(&world, "/xrpc/sh.tangled.repo.getDefaultBranch?repo=oyster").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        get_error(
            &world,
            &format!("/xrpc/sh.tangled.repo.log?repo={did}&ref=mangrove"),
        )
        .await,
        (StatusCode::NOT_FOUND, "RefNotFound".to_string())
    );

    let (status, error) = get_error(&world, "/xrpc/sh.tangled.repo.getDefaultBranch").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        error, "InvalidRequest",
        "a structurally unsound query gets the lexicon error shape instead of the runtime's default plaintext"
    );
}

#[tokio::test]
async fn list_refs_reports_paginates_and_drains() {
    let world = World::new();
    let (did, work) = seeded(&world, "whelk");
    let head = sh_git(work.path(), &["rev-parse", "HEAD"]);

    let value = get_json(&world, &format!("/xrpc/sh.tangled.git.listRefs?repo={did}")).await;
    assert_eq!(
        ref_names(&value, "refs"),
        vec![
            "refs/heads/main",
            "refs/tags/lightweight",
            "refs/tags/v1.0.0"
        ]
    );
    let main = value["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["ref"] == "refs/heads/main")
        .unwrap();
    assert_eq!(main["sha"], head);
    assert_eq!(value["defaultBranch"]["ref"], "refs/heads/main");
    assert_eq!(value["defaultBranch"]["head"], head);
    assert!(value["cursor"].is_null());

    let first = get_json(
        &world,
        &format!("/xrpc/sh.tangled.git.listRefs?repo={did}&limit=2"),
    )
    .await;
    assert_eq!(ref_names(&first, "refs").len(), 2);
    let cursor = first["cursor"].as_str().unwrap().to_string();
    let second = get_json(
        &world,
        &format!("/xrpc/sh.tangled.git.listRefs?repo={did}&limit=2&cursor={cursor}"),
    )
    .await;
    assert_eq!(ref_names(&second, "refs").len(), 1);
    assert!(second["cursor"].is_null());

    let refs = get_json(
        &world,
        &format!(
            "/xrpc/sh.tangled.git.listRefs?repo={did}&cursor={}",
            usize::MAX
        ),
    )
    .await;
    assert!(ref_names(&refs, "refs").is_empty());
    assert!(refs["cursor"].is_null());
    let repos = get_json(
        &world,
        &format!("/xrpc/sh.tangled.sync.listRepos?cursor={}", usize::MAX),
    )
    .await;
    assert!(repo_dids(&repos).is_empty());
    assert!(repos["cursor"].is_null());
}

#[tokio::test]
async fn a_hidden_staging_ref_resolves_for_fork_comparison_reads() {
    let world = World::new();
    let (did, work) = seeded(&world, "limpet");
    let bare = world.layout.repo_path(&did).unwrap();

    sh_git(work.path(), &["checkout", "-q", "-b", "upstream"]);
    commit_file(
        work.path(),
        "upstream.txt",
        b"upstream\n",
        "upstream moved",
        "2026-06-01T12:50:00+02:00",
    );
    let upstream = sh_git(work.path(), &["rev-parse", "HEAD"]);
    sh_git(
        work.path(),
        &[
            "push",
            "-q",
            bare.to_str().unwrap(),
            "HEAD:refs/hidden/main/main",
        ],
    );
    sh_git(work.path(), &["checkout", "-q", "main"]);
    commit_file(
        work.path(),
        "ours.txt",
        b"ours\n",
        "fork work",
        "2026-06-01T12:55:00+02:00",
    );
    sh_git(
        work.path(),
        &["push", "-q", bare.to_str().unwrap(), "HEAD:refs/heads/main"],
    );

    let comparison = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.compare?repo={did}&rev1=hidden/main/main&rev2=main"),
    )
    .await;
    assert_eq!(comparison["rev1"].as_str().unwrap(), upstream);
    assert!(!comparison["format_patch"].as_array().unwrap().is_empty());

    let log = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.log?repo={did}&ref=refs/hidden/main/main"),
    )
    .await;
    assert!(!log["commits"].as_array().unwrap().is_empty());

    let (status, _) = get_error(
        &world,
        &format!("/xrpc/sh.tangled.repo.log?repo={did}&ref={upstream}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a raw oid reachable only through the hidden ref mustn't resolve"
    );
    let (status, error) = get_error(
        &world,
        &format!("/xrpc/sh.tangled.repo.compare?repo={did}&rev1={upstream}&rev2=main"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error, "RevisionNotFound");
}

#[tokio::test]
async fn the_cob_ref_namespace_is_invisible_across_every_read() {
    let world = World::new();
    let (did, work) = seeded(&world, "anemone");
    let head = sh_git(work.path(), &["rev-parse", "HEAD"]);
    let bare = world.layout.repo_path(&did).unwrap();
    let cob = "refs/cobs/sh.tangled.repo.collaborator/x";
    sh_git(bare.as_path(), &["update-ref", cob, &head]);

    let value = get_json(&world, &format!("/xrpc/sh.tangled.git.listRefs?repo={did}")).await;
    assert!(
        ref_names(&value, "refs")
            .iter()
            .all(|name| !name.starts_with("refs/cobs/")),
        "reserved cob ref leaked into listRefs"
    );

    let w = &world;
    let d = &did;
    let named_routes: &[(&str, &str)] = &[
        ("log", ""),
        ("tree", ""),
        ("blob", "&path=README.md"),
        ("diff", ""),
        ("archive", ""),
        ("languages", ""),
    ];
    stream::iter(named_routes)
        .for_each(|&(route, suffix)| async move {
            let (status, _) = get_error(
                w,
                &format!("/xrpc/sh.tangled.repo.{route}?repo={d}&ref={cob}{suffix}"),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "{route} mustn't resolve reserved cobs ref"
            );
        })
        .await;
    let (status, _) = get_error(
        &world,
        &format!("/xrpc/sh.tangled.repo.log?repo={did}&ref=cobs/sh.tangled.repo.collaborator/x"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "reserved namespace shorthand mustn't resolve either"
    );
    let (status, _) = get_error(
        &world,
        &format!("/xrpc/sh.tangled.repo.compare?repo={did}&rev1={cob}&rev2={head}"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    commit_file(
        work.path(),
        "secret.txt",
        b"hidden\n",
        "secret",
        "2026-06-01T12:40:00+02:00",
    );
    let hidden = sh_git(work.path(), &["rev-parse", "HEAD"]);
    sh_git(
        work.path(),
        &[
            "push",
            "-q",
            bare.to_str().unwrap(),
            "HEAD:refs/cobs/sh.tangled.repo.collaborator/secret",
        ],
    );

    let hidden_ref = &hidden;
    let hidden_routes: &[(&str, &str)] = &[
        ("log", ""),
        ("tree", "&path=secret.txt"),
        ("diff", ""),
        ("archive", ""),
        ("languages", ""),
    ];
    stream::iter(hidden_routes)
        .for_each(|&(route, suffix)| async move {
            let (status, _) = get_error(
                w,
                &format!("/xrpc/sh.tangled.repo.{route}?repo={d}&ref={hidden_ref}{suffix}"),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "{route} mustn't serve a commit reachable only through cob ref"
            );
        })
        .await;
    let (status, _) = get_error(
        &world,
        &format!("/xrpc/sh.tangled.repo.blob?repo={did}&ref={hidden}&path=secret.txt"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, error) = get_error(
        &world,
        &format!("/xrpc/sh.tangled.repo.compare?repo={did}&rev1={head}&rev2={hidden}"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error, "RevisionNotFound");

    let still_public = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.log?repo={did}&ref={head}"),
    )
    .await;
    assert_eq!(still_public["total"].as_i64(), Some(4));
}

#[tokio::test]
async fn the_list_reads_reject_malformed_paging_params() {
    let world = World::new();
    let (did, _work) = seeded(&world, "barnacle");

    let queries: Vec<String> = vec![
        format!("/xrpc/sh.tangled.git.listRefs?repo={did}&limit=abc"),
        format!("/xrpc/sh.tangled.git.listRefs?repo={did}&cursor=notanint"),
        "/xrpc/sh.tangled.sync.listRepos?limit=abc".to_string(),
        "/xrpc/sh.tangled.sync.listRepos?cursor=notanint".to_string(),
        "/xrpc/sh.tangled.sync.listRepos?order=sideways".to_string(),
    ];
    let w = &world;
    stream::iter(queries.iter())
        .for_each(|query| async move {
            let (status, error) = get_error(w, query).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "query {query}");
            assert_eq!(error, "InvalidRequest", "query {query}");
        })
        .await;

    let clamped = get_json(
        &world,
        &format!("/xrpc/sh.tangled.git.listRefs?repo={did}&limit=5000"),
    )
    .await;
    assert_eq!(
        ref_names(&clamped, "refs").len(),
        3,
        "an oversize limit clamps to the max instead of erroring"
    );
}

#[tokio::test]
async fn list_repos_lists_hosted_repos_with_order_and_pagination() {
    let world = World::new();
    let (mussel, _a) = seeded(&world, "mussel");
    let (nautilus, _b) = seeded(&world, "nautilus");
    let (scallop, _c) = seeded(&world, "scallop");

    let desc = get_json(&world, "/xrpc/sh.tangled.sync.listRepos").await;
    assert_eq!(
        repo_dids(&desc),
        vec![
            scallop.as_str().to_string(),
            nautilus.as_str().to_string(),
            mussel.as_str().to_string(),
        ]
    );
    assert_eq!(desc["repos"][0]["status"], "active");
    assert_eq!(desc["repos"][0]["defaultBranch"]["ref"], "refs/heads/main");

    let asc = get_json(&world, "/xrpc/sh.tangled.sync.listRepos?order=asc").await;
    assert_eq!(
        repo_dids(&asc),
        vec![
            mussel.as_str().to_string(),
            nautilus.as_str().to_string(),
            scallop.as_str().to_string(),
        ]
    );

    let page = get_json(&world, "/xrpc/sh.tangled.sync.listRepos?order=asc&limit=2").await;
    assert_eq!(page["repos"].as_array().unwrap().len(), 2);
    let cursor = page["cursor"].as_str().unwrap().to_string();
    let rest = get_json(
        &world,
        &format!("/xrpc/sh.tangled.sync.listRepos?order=asc&limit=2&cursor={cursor}"),
    )
    .await;
    assert_eq!(repo_dids(&rest), vec![scallop.as_str().to_string()]);
    assert!(rest["cursor"].is_null());
}

#[tokio::test]
async fn every_projection_read_fails_closed_while_warming() {
    let world = World::warming();
    let did = RepoDid::new("did:plc:limpetfixture").unwrap();
    let cases: Vec<(String, Option<&str>)> = vec![
        (
            "/xrpc/sh.tangled.sync.listRepos".to_string(),
            Some("ProjectionWarming"),
        ),
        (
            format!("/xrpc/sh.tangled.repo.getDefaultBranch?repo={did}"),
            Some("ProjectionWarming"),
        ),
        (
            format!("/xrpc/sh.tangled.repo.describeRepo?repoDid={did}"),
            Some("ProjectionWarming"),
        ),
        (
            "/xrpc/sh.tangled.knot.listMembers?subject=did:web:knot.nel.pet".to_string(),
            None,
        ),
        (
            "/xrpc/sh.tangled.repo.listCollaborators?subject=did:plc:squid".to_string(),
            None,
        ),
    ];
    let w = &world;
    stream::iter(cases.iter())
        .for_each(|(path, expected)| async move {
            assert_warming(w, path, *expected).await;
        })
        .await;
}

#[tokio::test]
async fn branch_tips_render_edge_shapes() {
    let world = World::new();
    let (did, work) = seeded(&world, "trochus");
    let bare = world.layout.repo_path(&did).unwrap();
    let bare_str = bare.to_str().unwrap().to_string();

    sh_git(work.path(), &["checkout", "-q", "-b", "side", "HEAD~1"]);
    commit_file(
        work.path(),
        "side.txt",
        b"side\n",
        "side work",
        "2026-06-01T12:34:00+02:00",
    );
    sh_git(work.path(), &["checkout", "-q", "main"]);
    sh_git_at(
        work.path(),
        "2026-06-01T12:35:00+02:00",
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );
    sh_git(work.path(), &["push", "-q", &bare_str, "main"]);
    let first_parent = sh_git(work.path(), &["rev-parse", "HEAD^1"]);
    let second_parent = sh_git(work.path(), &["rev-parse", "HEAD^2"]);

    let tag_object = sh_git(work.path(), &["rev-parse", "v1.0.0"]);
    std::fs::write(bare.join("refs/heads/tagtip"), format!("{tag_object}\n")).unwrap();
    let root_commit = sh_git(work.path(), &["rev-list", "--max-parents=0", "HEAD"]);
    std::fs::write(bare.join("refs/heads/roottip"), format!("{root_commit}\n")).unwrap();

    let value = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.branches?repo={did}"),
    )
    .await;
    let branches = value["branches"].as_array().unwrap();
    assert_eq!(branches.len(), 3);

    let branch = |name: &str| {
        branches
            .iter()
            .find(|branch| branch["reference"]["name"] == name)
            .unwrap()
    };
    let parents = |branch: &serde_json::Value| -> Vec<String> {
        branch["commit"]["ParentHashes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|parent| {
                parent
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|byte| format!("{:02x}", byte.as_u64().unwrap()))
                    .collect()
            })
            .collect()
    };

    let main = branch("main");
    assert_eq!(
        parents(main),
        vec![first_parent, second_parent],
        "merge tip must report both parents in order"
    );
    assert_eq!(main["commit"]["Author"]["Name"], "nel");
    assert!(
        parents(branch("roottip")).is_empty(),
        "root tip must report no parents"
    );

    let tagtip = branch("tagtip");
    assert!(
        parents(tagtip).is_empty(),
        "a non-commit tip must report no parents"
    );
    assert_eq!(tagtip["reference"]["hash"], tag_object.as_str());
    assert_eq!(tagtip["commit"]["Author"]["Name"], "");
    assert_eq!(tagtip["commit"]["Author"]["When"], "0001-01-01T00:00:00Z");
    assert_eq!(tagtip["commit"]["Message"], "release one");
    assert!(
        tagtip["commit"]["TreeHash"]
            .as_array()
            .unwrap()
            .iter()
            .all(|byte| byte.as_u64() == Some(0)),
        "opaque tip has the zero tree hash"
    );
}

#[tokio::test]
async fn a_submodule_path_in_the_tree_is_path_not_found() {
    let world = World::new();
    let (did, work) = seeded(&world, "razorclam");
    let bare = world
        .layout
        .repo_path(&did)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let head = sh_git(work.path(), &["rev-parse", "HEAD"]);
    sh_git(
        work.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{head},vendor/dep"),
        ],
    );
    sh_git(work.path(), &["commit", "-q", "-m", "add gitlink"]);
    sh_git(work.path(), &["push", "-q", &bare, "main"]);

    assert_eq!(
        get_error(
            &world,
            &format!("/xrpc/sh.tangled.repo.tree?repo={did}&ref=main&path=vendor/dep"),
        )
        .await,
        (StatusCode::NOT_FOUND, "PathNotFound".to_string())
    );
}

#[tokio::test]
async fn a_blob_past_the_derived_serving_limit_is_a_named_error() {
    let world = World::with_response_limit(ResponseLimit::new(1024));
    let (did, bare, work) = empty_repo(&world, "auger");
    commit_file(
        work.path(),
        "big.txt",
        "a".repeat(2_000).as_bytes(),
        "big file",
        "2026-06-01T12:30:00+02:00",
    );
    sh_git(work.path(), &["push", "-q", &bare, "main"]);

    let (status, error) = get_error(
        &world,
        &format!("/xrpc/sh.tangled.repo.blob?repo={did}&ref=main&path=big.txt"),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        error, "BlobTooLarge",
        "blob limit is reached before the generic response limit"
    );

    let (status, _, body) = get(
        &world,
        &format!("/xrpc/sh.tangled.repo.blob?repo={did}&ref=main&path=big.txt&raw=true"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "raw serving keeps full limit");
    assert_eq!(body.len(), 2_000);
}

#[tokio::test]
async fn an_oversized_readme_is_omitted_from_the_tree() {
    let world = World::with_response_limit(ResponseLimit::new(2_048));
    let (did, bare, work) = empty_repo(&world, "cowrie");
    commit_file(
        work.path(),
        "README.md",
        format!("# reef\n\n{}\n", "r".repeat(1_000)).as_bytes(),
        "huge readme",
        "2026-06-01T12:30:00+02:00",
    );
    sh_git(work.path(), &["push", "-q", &bare, "main"]);

    let value = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.tree?repo={did}&ref=main"),
    )
    .await;
    assert_eq!(value["files"][0]["name"], "README.md");
    assert_eq!(
        value["readme"]["contents"], "",
        "readme past the serving limit is omitted instead of failing the whole tree"
    );
}

#[tokio::test]
async fn a_comparison_spanning_too_many_commits_is_refused() {
    let world = World::new();
    let (did, work) = seeded(&world, "abalone");
    let bare = world
        .layout
        .repo_path(&did)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let base = sh_git(work.path(), &["rev-parse", "HEAD"]);
    (0..501).for_each(|index| {
        sh_git(
            work.path(),
            &["commit", "-q", "--allow-empty", "-m", &format!("c{index}")],
        );
    });
    sh_git(work.path(), &["push", "-q", &bare, "main"]);
    let head = sh_git(work.path(), &["rev-parse", "HEAD"]);

    let (status, error) = get_error(
        &world,
        &format!("/xrpc/sh.tangled.repo.compare?repo={did}&rev1={base}&rev2={head}"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error, "CompareError");
}

#[tokio::test]
async fn archive_rejects_traversal_prefixes_and_sanitizes_the_filename() {
    let world = World::new();
    let (did, work) = seeded(&world, "cockle");
    let bare = world
        .layout
        .repo_path(&did)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let (status, error) = get_error(
        &world,
        &format!("/xrpc/sh.tangled.repo.archive?repo={did}&ref=main&prefix=../evil"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error, "InvalidRequest");

    sh_git(work.path(), &["branch", "a\"b"]);
    sh_git(work.path(), &["push", "-q", &bare, "refs/heads/a\"b"]);
    let (status, headers, _) = get(
        &world,
        &format!("/xrpc/sh.tangled.repo.archive?repo={did}&ref=a%22b"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let disposition = headers
        .get(header::CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(
        disposition, "attachment; filename=\"cockle-a-b.tar.gz\"",
        "quote in the ref name mustn't break the header quoting"
    );
}

#[tokio::test]
async fn an_archive_larger_than_the_configured_limit_is_refused() {
    let world = World::with_archive_limit(ArchiveLimit::new(64));
    let (did, _work) = seeded(&world, "murex");

    let (status, error) = get_error(
        &world,
        &format!("/xrpc/sh.tangled.repo.archive?repo={did}&ref=main"),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error, "RequestTooLarge");
}

#[tokio::test]
async fn an_oversized_read_response_is_refused() {
    let world = World::with_response_limit(ResponseLimit::new(256));
    let (did, _work) = seeded(&world, "clam");

    let (status, error) = get_error(
        &world,
        &format!("/xrpc/sh.tangled.repo.log?repo={did}&ref=main"),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error, "RequestTooLarge");

    let small = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.getDefaultBranch?repo={did}"),
    )
    .await;
    assert_eq!(small["name"], "main");
}

#[tokio::test]
async fn languages_omit_files_with_zero_size() {
    let world = World::new();
    let (did, bare, work) = empty_repo(&world, "topshell");
    commit_file(
        work.path(),
        "lib.rs",
        b"",
        "empty rust file",
        "2026-06-01T12:30:00+02:00",
    );
    sh_git(work.path(), &["push", "-q", &bare, "main"]);

    let value = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.languages?repo={did}&ref=main"),
    )
    .await;
    assert!(
        value["languages"].is_null(),
        "zero-byte file mustn't surface as a language with a NaN percentage"
    );
    assert!(value.get("totalSize").is_none());
    assert!(value.get("totalFiles").is_none());
}

#[tokio::test]
async fn a_compare_patch_round_trips_through_merge_check() {
    compare_round_trip(World::new()).await;
}

#[tokio::test]
async fn a_sha256_compare_patch_round_trips_through_merge_check() {
    compare_round_trip(World::sha256()).await;
}

async fn compare_round_trip(world: World) {
    let (did, main_sha, feature_sha) = seeded_feature_branch(&world, "periwinkle");

    let compared = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.compare?repo={did}&rev1={main_sha}&rev2={feature_sha}"),
    )
    .await;
    let patch = compared["patch"].as_str().unwrap();
    let combined = compared["combined_patch_raw"].as_str().unwrap();

    let checks = stream::iter([("main", patch), ("main", combined), ("feature", patch)])
        .then(|(branch, candidate)| {
            post_json(
                &world,
                "/xrpc/sh.tangled.repo.mergeCheck",
                serde_json::json!({
                    "repo": did,
                    "branch": branch,
                    "patch": candidate,
                }),
            )
        })
        .collect::<Vec<_>>()
        .await;
    assert_eq!(
        checks
            .iter()
            .map(|(status, check)| (*status, check["is_conflicted"].clone()))
            .collect::<Vec<_>>(),
        [false, false, true]
            .map(|conflicted| (StatusCode::OK, serde_json::Value::Bool(conflicted)))
            .to_vec(),
        "main takes both patches and feature already has them: {checks:?}"
    );

    [
        "GIT binary patch",
        " shell.bin | Bin 4096 -> 4096 bytes\n",
        "deleted file mode 100644\n",
        " delete mode 100644 anchor.bin\n",
        "diff --git a/deep water.bin b/deep water.bin\n",
        "old mode 100644\nnew mode 100755\n",
        " mode change 100644 => 100755 hull.bin\n",
        " hull.bin | Bin\n",
    ]
    .into_iter()
    .for_each(|needle| {
        assert!(
            patch.contains(needle),
            "the format patch is missing {needle:?}: {patch}"
        )
    });
    assert!(
        combined.contains("GIT binary patch"),
        "the combined patch is missing its binary payloads: {combined}"
    );
    assert!(
        !patch.contains("new mode 100755\nindex "),
        "a mode change leaves both sides at the same oid, so git prints no index line: {patch}"
    );
    assert_eq!(
        compared.get("binary_omitted"),
        None,
        "binary_omitted is absent when every payload was embedded: {compared}"
    );

    let applied = tempfile::tempdir().unwrap();
    let bare = world.layout.repo_path(&did).unwrap();
    sh_git(
        applied.path(),
        &["clone", "-q", bare.to_str().unwrap(), "."],
    );
    sh_git(applied.path(), &["checkout", "-q", "main"]);
    std::fs::write(applied.path().join("knot.patch"), patch).unwrap();
    sh_git(applied.path(), &["am", "knot.patch"]);
    assert_eq!(
        sh_git(applied.path(), &["rev-parse", "HEAD^{tree}"]),
        sh_git(applied.path(), &["rev-parse", "origin/feature^{tree}"]),
        "git am of the knot's own patch rebuilds the tree the branch already has"
    );
    assert!(
        !applied.path().join("anchor.bin").exists()
            && applied.path().join("deep water.bin").exists()
            && sh_git(applied.path(), &["ls-files", "-s", "hull.bin"]).starts_with("100755 "),
        "git am dropped anchor.bin, wrote deep water.bin and kept the exec bit on hull.bin"
    );
}

#[tokio::test]
async fn list_members_pages_in_the_wire_shape() {
    let world = World::new();
    world.add_member("did:plc:limpet", OWNER, 1_000);
    world.add_member("did:plc:scallop", OWNER, 2_000);
    world.add_member("did:plc:whelk", OWNER, 3_000);

    let page = get_json(
        &world,
        "/xrpc/sh.tangled.knot.listMembers?subject=did:web:knot.nel.pet&limit=2",
    )
    .await;
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "default order is createdAt descending");
    assert_eq!(items[0]["subject"], "did:plc:whelk");
    assert_eq!(items[0]["addedBy"], OWNER);
    assert_eq!(items[0]["createdAt"], "1970-01-01T00:50:00Z");
    assert!(
        items[0].get("uri").is_none() && items[0].get("cid").is_none(),
        "knot-owned member has no backing record, so uri and cid are omitted"
    );
    assert_eq!(items[1]["subject"], "did:plc:scallop");

    let cursor = page["cursor"].as_str().unwrap();
    let next = get_json(
        &world,
        &format!(
            "/xrpc/sh.tangled.knot.listMembers?subject=did:web:knot.nel.pet&limit=2&cursor={cursor}"
        ),
    )
    .await;
    let items = next["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["subject"], "did:plc:limpet");
    assert!(next.get("cursor").is_none(), "drained list has no cursor");

    let ascending = get_json(
        &world,
        "/xrpc/sh.tangled.knot.listMembers?subject=did:web:knot.nel.pet&order=asc&limit=1",
    )
    .await;
    assert_eq!(ascending["items"][0]["subject"], "did:plc:limpet");
}

#[tokio::test]
async fn list_members_rejects_malformed_params_and_clamps_the_limit() {
    let world = World::new();
    world.add_member("did:plc:limpet", OWNER, 1_000);

    let queries: &[&str] = &[
        "limit=abc&subject=did:web:knot.nel.pet",
        "cursor=notanint&subject=did:web:knot.nel.pet",
        "order=ascending&subject=did:web:knot.nel.pet",
        "limit=2",
        "subject=knot.nel.pet",
    ];
    let w = &world;
    stream::iter(queries.iter().copied())
        .for_each(|query| async move {
            let (status, _) =
                get_error(w, &format!("/xrpc/sh.tangled.knot.listMembers?{query}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "query {query}");
        })
        .await;

    let clamped = get_json(
        &world,
        "/xrpc/sh.tangled.knot.listMembers?subject=did:web:knot.nel.pet&limit=5000",
    )
    .await;
    assert_eq!(clamped["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn list_collaborators_is_scoped_to_the_repo() {
    let world = World::new();
    let squid = RepoDid::new("did:plc:squid").unwrap();
    let clam = RepoDid::new("did:plc:clam").unwrap();
    world.layout.create(&squid).unwrap();
    world.layout.create(&clam).unwrap();
    world.register(&squid, "squid");
    world.register(&clam, "clam");
    world.add_collaborator(&squid, "did:plc:lyna", OWNER, 1_000);
    world.add_collaborator(&clam, "did:plc:bailey", OWNER, 2_000);

    let page = get_json(
        &world,
        "/xrpc/sh.tangled.repo.listCollaborators?subject=did:plc:squid",
    )
    .await;
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["subject"], "did:plc:lyna");
    assert_eq!(items[0]["addedBy"], OWNER);
    assert_eq!(items[0]["createdAt"], "1970-01-01T00:16:40Z");
    assert!(items[0].get("uri").is_none() && items[0].get("cid").is_none());

    let unknown = get_json(
        &world,
        "/xrpc/sh.tangled.repo.listCollaborators?subject=did:plc:unhosted",
    )
    .await;
    assert!(
        unknown["items"].as_array().unwrap().is_empty(),
        "unhosted repo has no collaborators, the answer is an empty list"
    );

    let (status, _) = get_error(
        &world,
        "/xrpc/sh.tangled.repo.listCollaborators?subject=notadid",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_subject_tie_break_stays_ascending_in_both_directions() {
    let world = World::new();
    world.add_member("did:plc:whelk", OWNER, 1_000);
    world.add_member("did:plc:limpet", OWNER, 1_000);

    let subjects = |page: &serde_json::Value| -> Vec<String> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["subject"].as_str().unwrap().to_string())
            .collect()
    };

    let asc = get_json(
        &world,
        "/xrpc/sh.tangled.knot.listMembers?subject=did:web:knot.nel.pet&order=asc",
    )
    .await;
    let desc = get_json(
        &world,
        "/xrpc/sh.tangled.knot.listMembers?subject=did:web:knot.nel.pet&order=desc",
    )
    .await;
    assert_eq!(subjects(&asc), vec!["did:plc:limpet", "did:plc:whelk"]);
    assert_eq!(
        subjects(&desc),
        vec!["did:plc:limpet", "did:plc:whelk"],
        "equal-createdAt entries keep an ascending subject tie-break regardless of sort direction"
    );
}

#[tokio::test]
async fn repo_error_outranks_paging_in_any_param_order() {
    let world = World::new();
    let orders: &[&str] = &[
        "/xrpc/sh.tangled.repo.listCollaborators?subject=notadid&limit=abc",
        "/xrpc/sh.tangled.repo.listCollaborators?limit=abc&subject=notadid",
    ];
    let w = &world;
    stream::iter(orders.iter().copied())
        .for_each(|query| async move {
            let (_, error) = get_error(w, query).await;
            assert_eq!(
                error, "InvalidRepo",
                "the repo extractor runs before paging by signature position for {query}"
            );
        })
        .await;
}

#[tokio::test]
async fn service_metadata_endpoints_answer() {
    let world = World::new();
    let wire = get_json(&world, "/xrpc/sh.tangled.knot.version").await;
    assert_eq!(wire["version"], "v1.15.0");
    assert_eq!(
        wire["capabilities"],
        serde_json::json!(["knot-acl", "repo-did-input"])
    );

    let owner = get_json(&world, "/xrpc/sh.tangled.owner").await;
    assert_eq!(owner["owner"], OWNER);
}

fn publish_update(world: &World, repo: &str) -> EventCursor {
    world.state.events.publish(&GitRefUpdate::new(
        RepoDid::new(repo).unwrap(),
        Some(OwnerDid::new(OWNER).unwrap()),
        AccountDid::new("did:plc:nel").unwrap(),
    ))
}

async fn serve_events(world: &World) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = world.router.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn next_event(ws: &mut Ws) -> Pin<Box<dyn Future<Output = serde_json::Value> + '_>> {
    Box::pin(async move {
        let received = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("an event arrives within the timeout")
            .expect("the stream stays open")
            .expect("the frame is readable");
        match received {
            tungstenite::Message::Text(text) => serde_json::from_str(text.as_str()).unwrap(),
            _ => next_event(ws).await,
        }
    })
}

#[tokio::test]
async fn the_events_stream_replays_resumes_and_rejects_bad_cursors() {
    let world = World::new();
    let first = publish_update(&world, "did:plc:squid");
    publish_update(&world, "did:plc:anemone");
    let addr = serve_events(&world).await;

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/events"))
        .await
        .unwrap();
    let replayed_first = next_event(&mut ws).await;
    let replayed_second = next_event(&mut ws).await;
    assert_eq!(replayed_first["nsid"], "sh.tangled.git.refUpdate");
    assert_eq!(replayed_first["event"]["repo"], "did:plc:squid");
    assert_eq!(replayed_first["event"]["ownerDid"], OWNER);
    assert_eq!(replayed_first["event"]["committerDid"], "did:plc:nel");
    assert_eq!(replayed_first["rkey"].as_str().unwrap().len(), 13);
    assert_eq!(replayed_second["event"]["repo"], "did:plc:anemone");
    assert!(
        replayed_first["created"].as_i64().unwrap() < replayed_second["created"].as_i64().unwrap()
    );

    publish_update(&world, "did:plc:whelk");
    let live = next_event(&mut ws).await;
    assert_eq!(live["event"]["repo"], "did:plc:whelk");

    let (mut resumed, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/events?cursor={}", first.get()))
            .await
            .unwrap();
    let resumed_event = next_event(&mut resumed).await;
    assert_eq!(
        resumed_event["event"]["repo"], "did:plc:anemone",
        "a cursor resumes past the event it names"
    );

    let (mut garbled, _) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/events?cursor=banana"))
            .await
            .unwrap();
    let replayed = next_event(&mut garbled).await;
    assert_eq!(
        replayed["event"]["repo"], "did:plc:squid",
        "a garbled cursor replays from the start"
    );
}

fn refused<T>(result: Result<T, tungstenite::Error>) {
    match result {
        Err(tungstenite::Error::Http(response)) => {
            assert_eq!(response.status().as_u16(), 503);
        }
        Err(other) => panic!("expected an http refusal: {other}"),
        Ok(_) => panic!("a subscriber past the limit connected"),
    }
}

#[tokio::test]
async fn a_subscriber_beyond_the_events_limit_is_refused() {
    let world = World::new();
    let addr = serve_events(&world).await;
    let saturated: Vec<_> = (0..16u8)
        .map(|octet| {
            world
                .state
                .subscriber_gate
                .try_admit(std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                    10, 0, 0, octet,
                )))
                .expect("distinct peers fill the global limit")
        })
        .collect();
    refused(tokio_tungstenite::connect_async(format!("ws://{addr}/events")).await);
    drop(saturated);
}

#[tokio::test]
async fn a_single_peer_cannot_monopolize_the_events_stream() {
    let world = World::new();
    let addr = serve_events(&world).await;
    let held: Vec<_> = stream::iter(0..4)
        .then(|_| async {
            tokio_tungstenite::connect_async(format!("ws://{addr}/events"))
                .await
                .expect("a connection within the per-peer limit is admitted")
                .0
        })
        .collect()
        .await;
    refused(tokio_tungstenite::connect_async(format!("ws://{addr}/events")).await);
    drop(held);
}

#[tokio::test]
async fn set_default_branch_resolves_a_repo_did_and_an_existing_branch() {
    let world = World::new();
    let (did, work) = seeded(&world, "coral");
    let bare = world.layout.repo_path(&did).unwrap();
    sh_git(work.path(), &["branch", "release", "main"]);
    sh_git(
        work.path(),
        &["push", "-q", bare.to_str().unwrap(), "refs/heads/release"],
    );

    let (status, _) = post_authed(
        &world,
        "/xrpc/sh.tangled.repo.setDefaultBranch",
        OWNER,
        serde_json::json!({
            "repo": did,
            "defaultBranch": "release",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let repo = world.layout.open(&did).unwrap();
    assert_eq!(
        repo.default_branch().unwrap().as_str(),
        "refs/heads/release",
        "the default head moved to the requested branch"
    );
}

#[tokio::test]
async fn delete_branch_removes_a_non_default_branch_then_reports_it_gone() {
    let world = World::new();
    let (did, work) = seeded(&world, "kelp");
    let bare = world.layout.repo_path(&did).unwrap();
    sh_git(work.path(), &["branch", "feature", "main"]);
    sh_git(
        work.path(),
        &["push", "-q", bare.to_str().unwrap(), "refs/heads/feature"],
    );

    let (status, _) = post_authed(
        &world,
        "/xrpc/sh.tangled.repo.deleteBranch",
        OWNER,
        serde_json::json!({ "repo": did, "branch": "feature" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = post_authed(
        &world,
        "/xrpc/sh.tangled.repo.deleteBranch",
        OWNER,
        serde_json::json!({ "repo": did, "branch": "feature" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "second delete: {body}");
}

#[tokio::test]
async fn bad_post_bodies_are_invalid_request() {
    let world = World::new();
    let (_kelp, _wk) = seeded(&world, "kelp");
    let (_barnacle, _wb) = seeded(&world, "barnacle");

    let cases: &[(&str, serde_json::Value)] = &[
        (
            "/xrpc/sh.tangled.repo.setDefaultBranch",
            serde_json::json!({ "repo": "not-a-repo-did", "defaultBranch": "main" }),
        ),
        (
            "/xrpc/sh.tangled.repo.deleteBranch",
            serde_json::json!({
                "repo": "did:plc:kelpfixture",
                "branch": "bad branch",
            }),
        ),
        (
            "/xrpc/sh.tangled.repo.forkSync",
            serde_json::json!({ "repo": "did:plc:barnaclefixture", "branch": "bad branch" }),
        ),
        (
            "/xrpc/sh.tangled.repo.hiddenRef",
            serde_json::json!({ "repo": "nope", "forkRef": "feature", "remoteRef": "main" }),
        ),
    ];
    let w = &world;
    stream::iter(cases)
        .for_each(|(path, value)| async move {
            assert_post_rejected(w, path, OWNER, value.clone()).await;
        })
        .await;
}

#[tokio::test]
async fn merge_applies_a_patch_under_the_supplied_author() {
    let world = World::new();
    let (_did, main_sha, feature_sha) = seeded_feature_branch(&world, "mussel");
    let registered = RepoDid::new("did:plc:musselfixture").unwrap();

    let compared = get_json(
        &world,
        &format!(
            "/xrpc/sh.tangled.repo.compare?repo={registered}&rev1={main_sha}&rev2={feature_sha}"
        ),
    )
    .await;
    let patch = compared["combined_patch_raw"].as_str().unwrap().to_string();

    let (status, body) = post_authed(
        &world,
        "/xrpc/sh.tangled.repo.merge",
        OWNER,
        serde_json::json!({
            "repo": registered,
            "branch": "main",
            "patch": patch,
            "authorName": "Teq",
            "authorEmail": "teq@nel.pet",
            "commitMessage": "merged kelp",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "merge failed: {body}");

    let log = get_json(
        &world,
        &format!("/xrpc/sh.tangled.repo.log?repo={registered}&ref=main"),
    )
    .await;
    let top = &log["commits"][0];
    assert_eq!(
        top["author"]["Name"], "Teq",
        "the supplied author rode through"
    );
    assert!(
        top["message"].as_str().unwrap().contains("merged kelp"),
        "the supplied commit message rode through: {}",
        top["message"]
    );
}

#[tokio::test]
async fn create_mints_a_did_plc_repo_with_the_requested_default_branch() {
    let world = World::new();
    world.add_member(OWNER, OWNER, 1_000);
    let (status, body) = post_authed(
        &world,
        "/xrpc/sh.tangled.repo.create",
        OWNER,
        serde_json::json!({ "rkey": "squidkey", "name": "squid", "defaultBranch": "trunk" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create failed: {body}");
    let repo_did = body["repoDid"].as_str().unwrap();
    assert!(
        repo_did.starts_with("did:plc:"),
        "minted a did:plc: {repo_did}"
    );
    let did = RepoDid::new(repo_did).unwrap();
    let repo = world.layout.open(&did).unwrap();
    assert_eq!(
        repo.default_branch().unwrap().as_str(),
        "refs/heads/trunk",
        "the requested default branch became HEAD"
    );
}
