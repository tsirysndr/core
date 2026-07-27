use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

fn knot_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("knot root is two levels above server crate")
        .to_path_buf()
}

fn workspace_root() -> PathBuf {
    knot_root()
        .parent()
        .expect("workspace root is the parent of the knot root")
        .to_path_buf()
}

fn crate_src_files() -> impl Iterator<Item = PathBuf> {
    WalkDir::new(knot_root().join("crates"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .filter(|path| {
            path.components()
                .any(|component| component.as_os_str() == "src")
        })
}

#[test]
fn no_subprocess_spawning_in_src() {
    let offenders: Vec<String> = crate_src_files()
        .filter(|path| {
            std::fs::read_to_string(path)
                .map(|text| text.contains("process::Command"))
                .unwrap_or(false)
        })
        .map(|path| path.display().to_string())
        .collect();
    assert!(
        offenders.is_empty(),
        "design pillar: no subprocesses for git or anything else, but these src files spawn one: {offenders:?}"
    );
}

fn lock_field<'a>(block: &'a str, key: &str) -> Option<&'a str> {
    block.lines().find_map(|line| {
        line.strip_prefix(key)?
            .strip_prefix(" = \"")?
            .strip_suffix('"')
    })
}

type PackageKey<'a> = (&'a str, &'a str);
type LockGraph<'a> = BTreeMap<PackageKey<'a>, Vec<DepRef<'a>>>;

fn lock_graph(lock: &str) -> LockGraph<'_> {
    lock.split("[[package]]")
        .skip(1)
        .filter_map(|block| {
            let name = lock_field(block, "name")?;
            let version = lock_field(block, "version")?;
            Some(((name, version), lock_dependencies(block)))
        })
        .collect()
}

fn lock_dependencies(block: &str) -> Vec<DepRef<'_>> {
    block
        .split_once("dependencies = [")
        .and_then(|(_, rest)| rest.split(']').next())
        .into_iter()
        .flat_map(str::lines)
        .filter_map(|line| line.trim().strip_prefix('"'))
        .filter_map(|entry| entry.split('"').next())
        .map(|entry| {
            entry
                .split_once(' ')
                .map_or(DepRef::Name(entry), |(name, rest)| {
                    DepRef::Exact(name, rest.split(' ').next().unwrap_or(rest))
                })
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
enum DepRef<'a> {
    Name(&'a str),
    Exact(&'a str, &'a str),
}

impl<'a> DepRef<'a> {
    fn name(&self) -> &'a str {
        match self {
            DepRef::Name(name) | DepRef::Exact(name, _) => name,
        }
    }
}

fn reachable_from<'a>(
    graph: &LockGraph<'a>,
    by_name: &BTreeMap<&'a str, Vec<PackageKey<'a>>>,
    package: PackageKey<'a>,
    seen: &mut BTreeSet<PackageKey<'a>>,
) {
    if seen.insert(package) {
        graph.get(&package).into_iter().flatten().for_each(|dep| {
            let (name, version) = match dep {
                DepRef::Name(name) => (*name, None),
                DepRef::Exact(name, version) => (*name, Some(*version)),
            };
            by_name
                .get(name)
                .into_iter()
                .flatten()
                .filter(|(_, held)| version.is_none_or(|version| *held == version))
                .for_each(|key| reachable_from(graph, by_name, *key, seen));
        });
    }
}

#[test]
fn no_durable_state_or_native_git_crates() {
    let lock = std::fs::read_to_string(workspace_root().join("Cargo.lock"))
        .expect("workspace Cargo.lock is readable");
    let graph = lock_graph(&lock);
    let by_name: BTreeMap<&str, Vec<PackageKey<'_>>> =
        graph.keys().fold(BTreeMap::new(), |mut names, key| {
            names.entry(key.0).or_default().push(*key);
            names
        });
    let server = by_name
        .get("knot-server")
        .and_then(|keys| keys.first())
        .copied()
        .expect("the lockfile parse must find knot-server");
    assert!(
        graph.get(&server).is_some_and(|deps| !deps.is_empty()),
        "the lockfile parse must find knot-server's dependency list"
    );

    let mut reachable = BTreeSet::new();
    reachable_from(&graph, &by_name, server, &mut reachable);
    assert!(
        reachable.iter().any(|(name, _)| *name == "gix"),
        "the reachability walk must reach the git engine, so an empty walk is a broken parse"
    );

    let banned = [
        "rusqlite",
        "sqlx",
        "sled",
        "fjall",
        "redb",
        "git2",
        "libgit2-sys",
    ];
    let present: Vec<&str> = banned
        .into_iter()
        .filter(|name| reachable.iter().any(|(held, _)| held == name))
        .collect();
    assert!(
        present.is_empty(),
        "design pillar: no durable state but git and all git work through gix, but the server's dependency graph includes: {present:?}"
    );
}

fn dependents_of<'a>(graph: &LockGraph<'a>, package: &str) -> Vec<&'a str> {
    graph
        .iter()
        .filter(|((name, _), _)| *name != package)
        .filter(|(_, deps)| deps.iter().any(|dep| dep.name() == package))
        .map(|((name, _), _)| *name)
        .collect()
}

#[test]
fn nothing_depends_on_the_offline_migration_tool() {
    let lock = std::fs::read_to_string(workspace_root().join("Cargo.lock"))
        .expect("workspace Cargo.lock is readable");
    let graph = lock_graph(&lock);
    assert!(
        !dependents_of(&graph, "knot-types").is_empty(),
        "the shared newtypes have dependents, so an empty answer here is a broken parse"
    );

    let dependents = dependents_of(&graph, "knot-migrate");
    assert!(
        dependents.is_empty(),
        "knot-migrate is an offline one-shot tool whose rusqlite dependency must never reach the server, but it is depended on by: {dependents:?}"
    );
}

#[test]
fn the_shared_limit_defaults_match_the_config_defaults() {
    use confique::{Config, Layer};
    use knot_xrpc::{Budgets, ByteLimits, ReadBudget};

    fn ms(budget: ReadBudget) -> u64 {
        match budget {
            ReadBudget::Within(within) => within.as_millis() as u64,
            ReadBudget::Unbounded => u64::MAX,
        }
    }

    let xrpc = <knot_config::XrpcConfig as Config>::Layer::default_values();
    let server = <knot_config::ServerConfig as Config>::Layer::default_values();
    let bytes = ByteLimits::default();
    let budgets = Budgets::default();
    let push_ms = budgets.languages_push.get().as_millis() as u64;

    let configured = [
        ("body", xrpc.max_body_bytes),
        ("patch", xrpc.max_patch_bytes),
        ("patch_decompressed", xrpc.max_patch_decompressed_bytes),
        ("response", xrpc.max_response_bytes),
        ("archive", xrpc.max_archive_bytes),
        ("fork_pack", xrpc.fork_max_pack_bytes),
        ("pack", server.ssh_max_pack_bytes),
        ("tree_last_commit", xrpc.tree_last_commit_budget_ms),
        ("blob_last_commit", xrpc.blob_last_commit_budget_ms),
        ("languages", xrpc.languages_budget_ms),
        ("languages_push", xrpc.languages_push_budget_ms),
    ]
    .map(|(name, value)| (name, value.expect("every limit has a config default")));
    let shared = [
        ("body", bytes.body.get() as u64),
        ("patch", bytes.patch.get() as u64),
        ("patch_decompressed", bytes.patch_decompressed.get()),
        ("response", bytes.response.get() as u64),
        ("archive", bytes.archive.get()),
        ("fork_pack", bytes.fork_pack.get()),
        ("pack", bytes.pack.get() as u64),
        ("tree_last_commit", ms(budgets.tree_last_commit.get())),
        ("blob_last_commit", ms(budgets.blob_last_commit.get())),
        ("languages", ms(budgets.languages.get())),
        ("languages_push", push_ms),
    ];
    assert_eq!(
        configured, shared,
        "the config defaults and the in-code defaults must match. update ByteLimits::default and Budgets::default alongside the config defaults"
    );
}
