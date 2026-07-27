use std::path::Path;
use std::time::Duration;

use knot_events::{EventCursor, EventLog, Reservation};
use knot_git::{Layout, RefUpdate, Repo};
use knot_postreceive::{Actor, Ci, LanguagesPushBudget, OwnerLabel, PullLink, post_receive};
use knot_runtime::{ManualClock, UnixMicros};
use knot_types::{
    AccountDid, AppviewEndpoint, BranchName, CiLogsAddr, Handle, Oid, OwnerDid, PushOption,
    PushOptions, RefName, RepoDid, RepoRkey,
};

const DID: &str = "did:plc:limpet";
const OWNER: &str = "did:web:olaren.dev";
const COMMITTER: &str = "did:plc:nel";
const PUSH_BUDGET: LanguagesPushBudget = LanguagesPushBudget::new(Duration::from_secs(2));

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = knot_fixtures::command(cwd)
        .args(args)
        .output()
        .expect("git is available");
    assert!(
        output.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn commit_file(work: &Path, file: &str, contents: &str, message: &str) {
    let path = work.join(file);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
    git(work, &["add", "-A"]);
    git(work, &["commit", "-q", "-m", message]);
}

struct World {
    _scan: tempfile::TempDir,
    _work: tempfile::TempDir,
    repo: Repo,
    work: std::path::PathBuf,
    bare: std::path::PathBuf,
}

fn world() -> World {
    let scan = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path()).with_default_branch(BranchName::new("main").unwrap());
    let did = RepoDid::new(DID).unwrap();
    layout.create(&did).unwrap();
    let bare = layout.repo_path(&did).unwrap();

    let work_dir = tempfile::tempdir().unwrap();
    let work = work_dir.path().to_path_buf();
    git(&work, &["init", "-q", "-b", "main"]);
    let repo = layout.open(&did).unwrap();
    World {
        _scan: scan,
        _work: work_dir,
        repo,
        work,
        bare,
    }
}

fn push(world: &World, branch: &str) {
    git(
        &world.work,
        &["push", "-q", world.bare.to_str().unwrap(), branch],
    );
}

fn oid(world: &World, rev: &str) -> Oid {
    Oid::from_hex(&git(&world.work, &["rev-parse", rev])).unwrap()
}

fn actor() -> Actor {
    Actor {
        committer: AccountDid::new(COMMITTER).unwrap(),
        owner: Some(OwnerDid::new(OWNER).unwrap()),
        repo: RepoDid::new(DID).unwrap(),
    }
}

fn refname(name: &str) -> RefName {
    RefName::new(name).unwrap()
}

fn pull() -> PullLink {
    PullLink {
        appview: AppviewEndpoint::new("https://tangled.test").unwrap(),
        owner: OwnerLabel::Handle(Handle::new_owned("nel.pet").unwrap()),
        rkey: RepoRkey::new("anemone").unwrap(),
    }
}

fn compile(verbose: bool) -> Ci {
    Ci::Compile {
        logs: None,
        verbose,
    }
}

fn compile_with_logs(addr: &str) -> Ci {
    Ci::Compile {
        logs: Some(CiLogsAddr::new(addr).unwrap()),
        verbose: false,
    }
}

fn bounds() -> knot_events::ReplayBounds {
    knot_events::ReplayBounds::new(
        knot_events::ReplayEvents::new(32).unwrap(),
        knot_events::ReplayBytes::new(16 << 20).unwrap(),
    )
}

fn log() -> EventLog<ManualClock> {
    EventLog::new(ManualClock::new(UnixMicros::new(1_000_000_000)), bounds())
}

fn reserved(log: &EventLog<ManualClock>, applied: &[RefUpdate]) -> Vec<(RefUpdate, Reservation)> {
    applied
        .iter()
        .map(|update| (update.clone(), log.reserve()))
        .collect()
}

fn events(log: &EventLog<ManualClock>) -> Vec<(String, serde_json::Value)> {
    log.replay(EventCursor::START, bounds())
        .events
        .into_iter()
        .map(|event| {
            let wire = serde_json::to_value(&*event).unwrap();
            (event.nsid.to_string(), wire["event"].clone())
        })
        .collect()
}

fn run(
    world: &World,
    log: &EventLog<ManualClock>,
    applied: &[RefUpdate],
    ci: &Ci,
    pull: Option<&PullLink>,
) -> Vec<String> {
    run_with_options(world, log, applied, ci, &PushOptions::default(), pull)
}

fn run_with_options(
    world: &World,
    log: &EventLog<ManualClock>,
    applied: &[RefUpdate],
    ci: &Ci,
    push_options: &PushOptions,
    pull: Option<&PullLink>,
) -> Vec<String> {
    let repo = Repo::open(&world.bare).unwrap();
    post_receive(
        &repo,
        &actor(),
        reserved(log, applied),
        ci,
        push_options,
        pull,
        PUSH_BUDGET,
        &knot_messages::default_catalog().push,
    )
}

fn created(branch: &str, head: Oid) -> [RefUpdate; 1] {
    [RefUpdate::Create {
        name: refname(&format!("refs/heads/{branch}")),
        new: head,
    }]
}

fn create(world: &World, branch: &str) -> [RefUpdate; 1] {
    commit_file(&world.work, "a.txt", "one\n", "first");
    push(world, branch);
    created(branch, oid(world, "HEAD"))
}

fn create_feature(world: &World) -> Oid {
    commit_file(&world.work, "a.txt", "one\n", "first");
    push(world, "main");
    git(&world.work, &["checkout", "-q", "-b", "feature"]);
    commit_file(&world.work, "c.txt", "three\n", "feature work");
    push(world, "feature");
    oid(world, "HEAD")
}

#[test]
fn a_create_emits_a_ref_update_with_commit_counts_and_default_ref() {
    let world = world();
    commit_file(&world.work, "a.txt", "one\n", "first");
    commit_file(&world.work, "b.txt", "two\n", "second");
    push(&world, "main");
    let head = oid(&world, "HEAD");

    let log = log();
    let applied = created("main", head);
    run(&world, &log, &applied, &Ci::Skip, None);

    let events = events(&log);
    assert_eq!(events.len(), 1);
    let (nsid, payload) = &events[0];
    assert_eq!(nsid, "sh.tangled.git.refUpdate");
    assert_eq!(payload["ref"], "refs/heads/main");
    assert_eq!(payload["newSha"], head.to_hex());
    assert_eq!(
        payload["oldSha"],
        world.repo.object_format().null_oid().to_string()
    );
    assert_eq!(payload["committerDid"], COMMITTER);
    assert_eq!(payload["meta"]["isDefaultRef"], true);
    assert_eq!(
        payload["meta"]["commitCount"]["byEmail"][0]["email"],
        "nel@oyster.cafe"
    );
    assert_eq!(payload["meta"]["commitCount"]["byEmail"][0]["count"], 2);
}

#[test]
fn an_update_counts_only_the_new_commits() {
    let world = world();
    commit_file(&world.work, "a.txt", "one\n", "first");
    push(&world, "main");
    let old = oid(&world, "HEAD");
    commit_file(&world.work, "b.txt", "two\n", "second");
    push(&world, "main");
    let new = oid(&world, "HEAD");

    let log = log();
    let applied = [RefUpdate::Update {
        name: refname("refs/heads/main"),
        old,
        new,
    }];
    run(&world, &log, &applied, &Ci::Skip, None);

    let payload = &events(&log)[0].1;
    assert_eq!(payload["meta"]["commitCount"]["byEmail"][0]["count"], 1);
}

#[test]
fn a_non_default_branch_is_not_flagged_default() {
    let world = world();
    commit_file(&world.work, "a.txt", "one\n", "first");
    push(&world, "main");
    git(&world.work, &["checkout", "-q", "-b", "feature"]);
    commit_file(&world.work, "c.txt", "three\n", "feature work");
    push(&world, "feature");
    let head = oid(&world, "HEAD");

    let log = log();
    let applied = created("feature", head);
    run(&world, &log, &applied, &Ci::Skip, None);

    let payload = &events(&log)[0].1;
    assert_eq!(payload["meta"]["isDefaultRef"], false);
    assert_eq!(payload["meta"]["commitCount"]["byEmail"][0]["count"], 1);
}

#[test]
fn a_large_push_counts_every_commit_without_a_limit() {
    let world = world();
    let count = 110;
    (0..count).for_each(|n| {
        git(
            &world.work,
            &["commit", "-q", "--allow-empty", "-m", &format!("c{n}")],
        );
    });
    push(&world, "main");
    let head = oid(&world, "HEAD");

    let log = log();
    let applied = created("main", head);
    run(&world, &log, &applied, &Ci::Skip, None);

    let payload = &events(&log)[0].1;
    assert_eq!(
        payload["meta"]["commitCount"]["byEmail"][0]["count"], count,
        "every commit is tallied, none dropped"
    );
}

#[test]
fn a_non_default_branch_omits_the_language_breakdown() {
    let world = world();
    commit_file(&world.work, "src/main.rs", "fn main() {}\n", "rust");
    push(&world, "main");
    git(&world.work, &["checkout", "-q", "-b", "feature"]);
    commit_file(&world.work, "src/extra.rs", "fn extra() {}\n", "more rust");
    push(&world, "feature");
    let head = oid(&world, "HEAD");

    let log = log();
    let applied = created("feature", head);
    run(&world, &log, &applied, &Ci::Skip, None);

    let payload = &events(&log)[0].1;
    assert_eq!(payload["meta"]["isDefaultRef"], false);
    assert_eq!(
        payload["meta"]["langBreakdown"],
        serde_json::Value::Null,
        "non-default ref has no language breakdown"
    );
    assert_eq!(payload["meta"]["commitCount"]["byEmail"][0]["count"], 1);
}

#[test]
fn a_delete_emits_a_ref_update_without_meta() {
    let world = world();
    commit_file(&world.work, "a.txt", "one\n", "first");
    push(&world, "main");
    let old = oid(&world, "HEAD");

    let log = log();
    let applied = [RefUpdate::Delete {
        name: refname("refs/heads/gone"),
        old,
    }];
    run(&world, &log, &applied, &Ci::Skip, None);

    let payload = &events(&log)[0].1;
    assert_eq!(payload["ref"], "refs/heads/gone");
    assert_eq!(
        payload["newSha"],
        world.repo.object_format().null_oid().to_string()
    );
    assert_eq!(payload["meta"], serde_json::Value::Null);
    assert!(
        payload.get("changedFiles").is_none(),
        "a deletion has no tree to diff: {payload}"
    );
    assert!(payload.get("pushOptions").is_none(), "{payload}");
}

#[test]
fn language_breakdown_reports_pushed_sources() {
    let world = world();
    commit_file(
        &world.work,
        "src/main.rs",
        "fn main() {\n    println!(\"hello from nel\");\n}\n",
        "rust",
    );
    push(&world, "main");
    let head = oid(&world, "HEAD");

    let log = log();
    let applied = created("main", head);
    run(&world, &log, &applied, &Ci::Skip, None);

    let payload = &events(&log)[0].1;
    let langs = payload["meta"]["langBreakdown"]["inputs"]
        .as_array()
        .expect("language breakdown is present");
    assert!(
        langs.iter().any(|lang| lang["lang"] == "Rust"),
        "expected Rust in {langs:?}"
    );
}

#[test]
fn a_configured_logs_address_yields_an_ssh_command_for_compiled_workflows() {
    let world = world();
    commit_file(
        &world.work,
        ".tangled/workflows/ci.yml",
        "engine: nixery.dev/x\nwhen:\n  - event: push\n    branch: ['**']\n",
        "add ci",
    );
    push(&world, "main");
    let head = oid(&world, "HEAD");

    let log = log();
    let applied = created("main", head);
    let messages = run(
        &world,
        &log,
        &applied,
        &compile_with_logs("logs.oyster.cafe:3333"),
        None,
    );

    assert!(
        messages
            .iter()
            .any(|line| line.contains(&format!("ssh -t -p 3333 logs.oyster.cafe {DID} {head}"))),
        "{messages:?}"
    );
}

#[test]
fn a_push_without_workflows_yields_no_ssh_command_and_verbose_says_so() {
    let world = world();
    let applied = create(&world, "main");
    let messages = run(
        &world,
        &log(),
        &applied,
        &compile_with_logs("logs.oyster.cafe:3333"),
        None,
    );
    assert!(
        messages.iter().all(|line| !line.contains("ssh -t")),
        "{messages:?}"
    );

    let messages = run(&world, &log(), &applied, &compile(true), None);
    assert!(
        messages
            .iter()
            .any(|line| line == "no pipelines to compile"),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .all(|line| line != "pipeline compiled with no diagnostics"),
        "{messages:?}"
    );
}

#[test]
fn verbose_keeps_the_warning_that_explains_why_nothing_compiled() {
    let world = world();
    commit_file(
        &world.work,
        ".tangled/workflows/ci.yml",
        "engine: nixery.dev/x\nwhen:\n  - event: push\n    branch: [release]\n",
        "add ci",
    );
    push(&world, "main");
    let head = oid(&world, "HEAD");

    let log = log();
    let messages = run(&world, &log, &created("main", head), &compile(true), None);

    assert!(
        messages
            .iter()
            .any(|line| line.contains("workflow skipped")),
        "a workflow that misses the trigger still reports why: {messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|line| line == "no pipelines to compile"),
        "{messages:?}"
    );
}

#[test]
fn the_ref_update_event_reports_changed_files_and_push_options() {
    let world = world();
    commit_file(&world.work, "a.txt", "one\n", "first");
    commit_file(&world.work, "src/deep/main.rs", "fn main() {}\n", "nested");
    push(&world, "main");
    let head = oid(&world, "HEAD");
    git(&world.work, &["tag", "-a", "v1.0.0", "-m", "release one"]);
    git(
        &world.work,
        &["push", "-q", "--tags", world.bare.to_str().unwrap()],
    );
    let tag_object = oid(&world, "v1.0.0");
    assert_ne!(tag_object, oid(&world, "v1.0.0^{commit}"));

    let branch_log = log();
    let options = PushOptions::new([PushOption::new("verbose-ci").unwrap()]);
    run_with_options(
        &world,
        &branch_log,
        &created("main", head),
        &compile(false),
        &options,
        None,
    );

    let payload = &events(&branch_log)[0].1;
    assert_eq!(
        payload["changedFiles"],
        serde_json::json!(["a.txt", "src/deep/main.rs"]),
        "a branch creation reports every blob in the tree and no directory of them"
    );
    assert_eq!(payload["pushOptions"], serde_json::json!(["verbose-ci"]));

    let tag_log = log();
    let applied = [RefUpdate::Create {
        name: refname("refs/tags/v1.0.0"),
        new: tag_object,
    }];
    run(&world, &tag_log, &applied, &Ci::Skip, None);
    assert_eq!(
        events(&tag_log)[0].1["changedFiles"],
        serde_json::json!(["a.txt", "src/deep/main.rs"]),
        "the tag object peels to its commit before the trees are diffed"
    );
}

fn ci_yaml(paths: &str) -> String {
    format!(
        "engine: nixery.dev/x\nwhen:\n  - event: push\n    branch: ['**']\n    paths: ['{paths}']\n"
    )
}

#[test]
fn a_push_wider_than_the_record_prints_the_ssh_command_only_on_a_listed_glob_hit() {
    let world = world();
    commit_file(
        &world.work,
        ".tangled/workflows/ci.yml",
        &ci_yaml("never/**"),
        "add ci",
    );
    (0..20_000).for_each(|index| {
        std::fs::write(world.work.join(format!("f{index}.txt")), "x\n").unwrap();
    });
    git(&world.work, &["add", "-A"]);
    git(&world.work, &["commit", "-q", "-m", "many"]);
    push(&world, "main");
    let unmatched = oid(&world, "HEAD");
    commit_file(
        &world.work,
        ".tangled/workflows/ci.yml",
        &ci_yaml("f1.txt"),
        "aim ci",
    );
    push(&world, "main");
    let matched = oid(&world, "HEAD");

    let wide_log = log();
    let messages = run(
        &world,
        &wide_log,
        &created("main", unmatched),
        &compile_with_logs("logs.oyster.cafe:3333"),
        None,
    );
    let listed = events(&wide_log)[0].1["changedFiles"]
        .as_array()
        .unwrap()
        .len();
    assert!(
        (1..20_000).contains(&listed),
        "the listing stops at the byte budget instead of growing with the push: {listed}"
    );
    assert!(
        messages.iter().all(|line| !line.contains("ssh -t")),
        "spindle reads the same truncated listing, rules the globs out, \
         and skips this run: {messages:?}"
    );

    let messages = run(
        &world,
        &log(),
        &created("main", matched),
        &compile_with_logs("logs.oyster.cafe:3333"),
        None,
    );
    assert!(
        messages.iter().any(|line| line.contains("ssh -t -p 3333")),
        "spindle sees the same listed path and runs this workflow: {messages:?}"
    );
}

#[test]
fn a_compiled_workflow_emits_no_pipeline_event() {
    let world = world();
    commit_file(
        &world.work,
        ".tangled/workflows/ci.yml",
        "engine: nixery.dev/x\nwhen:\n  - event: push\n    branch: ['**']\n",
        "add ci",
    );
    push(&world, "main");
    let head = oid(&world, "HEAD");

    let log = log();
    let applied = created("main", head);
    run(&world, &log, &applied, &compile(false), None);

    let events = events(&log);
    assert!(events.iter().all(|(nsid, _)| nsid != "sh.tangled.pipeline"));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "sh.tangled.git.refUpdate");
}

#[test]
fn a_new_non_default_branch_yields_a_pull_request_link() {
    let world = world();
    let log = log();
    let head = create_feature(&world);

    let applied = created("feature", head);
    let messages = run(&world, &log, &applied, &Ci::Skip, Some(&pull()));

    let link = messages
        .iter()
        .find(|line| line.contains("/pulls/new"))
        .expect("pull-request link is offered for new non-default branch");
    assert!(
        link.contains("https://tangled.test/nel.pet/anemone/pulls/new"),
        "{link}"
    );
    assert!(link.contains("sourceBranch=feature"), "{link}");
    assert!(link.contains("targetBranch=main"), "{link}");
}

#[test]
fn no_pull_request_link_for_default_existing_forked_or_rootless_branches() {
    type Case = (&'static str, fn(&World) -> [RefUpdate; 1]);
    let cases: &[Case] = &[
        ("push to the default branch", |w| create(w, "main")),
        ("update to an existing branch", |w| {
            commit_file(&w.work, "a.txt", "one\n", "first");
            push(w, "main");
            git(&w.work, &["checkout", "-q", "-b", "feature"]);
            commit_file(&w.work, "c.txt", "three\n", "feature");
            push(w, "feature");
            let old = oid(w, "HEAD");
            commit_file(&w.work, "d.txt", "four\n", "more feature");
            push(w, "feature");
            [RefUpdate::Update {
                name: refname("refs/heads/feature"),
                old,
                new: oid(w, "HEAD"),
            }]
        }),
        ("new branch on a fork with an origin remote", |w| {
            let head = create_feature(w);
            w.repo
                .set_origin_url("https://oyster.cafe/did:plc:squid/anemone")
                .unwrap();
            created("feature", head)
        }),
        ("new branch while the default branch is absent", |w| {
            commit_file(&w.work, "a.txt", "one\n", "first");
            git(&w.work, &["checkout", "-q", "-b", "feature"]);
            commit_file(&w.work, "c.txt", "three\n", "feature work");
            push(w, "feature");
            created("feature", oid(w, "HEAD"))
        }),
    ];
    cases.iter().for_each(|(label, build)| {
        let world = world();
        let log = log();
        let applied = build(&world);
        let messages = run(&world, &log, &applied, &Ci::Skip, Some(&pull()));
        assert!(
            messages.iter().all(|line| !line.contains("/pulls/new")),
            "{label} must offer no pull-request link: {messages:?}"
        );
    });
}

#[test]
fn verbose_ci_reports_a_clean_pipeline_and_quiet_ci_stays_silent() {
    let world = world();
    let log = log();
    commit_file(
        &world.work,
        ".tangled/workflows/ci.yml",
        "engine: nixery.dev/x\nwhen:\n  - event: push\n    branch: ['**']\n",
        "add ci",
    );
    push(&world, "main");
    let head = oid(&world, "HEAD");
    let applied = created("main", head);

    let verbose = run(&world, &log, &applied, &compile(true), None);
    assert!(
        verbose.iter().any(|line| line.contains("no diagnostics")),
        "verbose ci announces clean compile: {verbose:?}"
    );

    let quiet = run(&world, &log, &applied, &compile(false), None);
    assert!(
        quiet.iter().all(|line| !line.contains("no diagnostics")),
        "quiet push says nothing about clean compile: {quiet:?}"
    );
}

#[test]
fn a_pipeline_compile_error_reaches_the_pusher_even_when_quiet() {
    let world = world();
    let log = log();
    commit_file(
        &world.work,
        ".tangled/workflows/broken.yml",
        "engine: : not valid : yaml :\n  - [\n",
        "add broken ci",
    );
    push(&world, "main");
    let head = oid(&world, "HEAD");
    let applied = created("main", head);

    let messages = run(&world, &log, &applied, &compile(false), None);
    assert!(
        messages.iter().any(|line| line.starts_with("error:")),
        "malformed workflow surfaces compile error to pusher: {messages:?}"
    );
}
