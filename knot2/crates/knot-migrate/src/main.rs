use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use knot_migrate::adopt::{self, AdoptError, SourcePolicy};
use knot_migrate::casbin::{self, CasbinError};
use knot_migrate::emit::{
    self, ConfigValues, EmitError, HostKeyConflict, HostKeyPlacement, HostKeyPolicy, MasterKeyEnv,
    MasterKeyError,
};
use knot_migrate::envfile::{EnvFile, EnvFileError};
use knot_migrate::mapping::{self, Mapping, MappingError, RepoList};
use knot_migrate::rehearse::{self, Rehearsal};
use knot_migrate::report::{Phase, Report};
use knot_migrate::source::{SourceDb, SourceError, SourceRepoDid, SourceRkey, SourceSchema};
use knot_runtime::OsEntropy;
use knot_secrets::{SealedStore, SecretsError};
use knot_types::{AccountDid, KnotHostname, ObjectFormat, RepoDid};
use url::Url;

// TODO: I wanted to see how well I could work without clap. I shoulda just used clap.
const USAGE: &str = "\
knot-migrate: offline conversion of a tangled-knot deployment into a knot deployment

usage:
  knot-migrate --source-db <knotserver.db> --host-key <ssh_host_key> --target <dir> [options]

options:
  --source-db <path>      tangled-knot SQLite database, opened read-only
  --source-repos <dir>    tangled-knot scan path holding <repo_did> directories
                          defaults to KNOT_REPO_SCAN_PATH from the env file
  --env-file <path>       tangled-knot environment file
  --host-key <path>       system sshd host key to import
  --target <dir>          knot data directory to create
  --hostname <host>       knot hostname, defaults to KNOT_SERVER_HOSTNAME
  --plc-url <url>         PLC directory, defaults to KNOT_SERVER_PLC_URL
  --object-format <fmt>   sha1 or sha256 for repos knot creates, default sha1
  --master-key-env <name> env var holding the base64 master key, default KNOT_MASTER_KEY
  --consume-source        move the source repos into place instead of copying them,
                          which empties the source tree and needs one filesystem
  --skip-unreadable       migrate the rest when knot-migrate can't read a source path,
                          and leave those repos on the old knot
  --force-host-key        replace the host key already at the target,
                          whose fingerprint your users already trust
  --dry-run               print the mapping and reconciliation report, leave the target
                          alone, and exit non-zero while an input is still missing
";

#[derive(Debug, thiserror::Error)]
enum MigrateError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Casbin(#[from] CasbinError),
    #[error(transparent)]
    Mapping(#[from] MappingError),
    #[error(transparent)]
    Adopt(#[from] AdoptError),
    #[error(transparent)]
    Emit(#[from] EmitError),
    #[error(transparent)]
    HostKeyConflict(#[from] HostKeyConflict),
    #[error(transparent)]
    EnvFile(#[from] EnvFileError),
    #[error(transparent)]
    Git(#[from] knot_git::GitError),
    #[error(transparent)]
    Secrets(#[from] SecretsError),
    #[error("{0}")]
    Usage(String),
    #[error("env file specifies knot owner {env} while the acl specifies {acl}")]
    OwnerMismatch { env: String, acl: String },
    #[error("--hostname {flag} doesn't match the env file's KNOT_SERVER_HOSTNAME {env}")]
    HostnameMismatch { flag: String, env: String },
    #[error(transparent)]
    MasterKey(#[from] MasterKeyError),
    #[error("{0}")]
    Refused(Refusals),
    #[error("the rehearsal lists what the migration still needs")]
    RehearsalIncomplete,
    #[error("{context}: {source}")]
    Io {
        context: String,
        source: std::io::Error,
    },
}

#[derive(Debug, thiserror::Error)]
enum Refusal {
    #[error(
        "knot-migrate can't read the source path of {repos}. Each repo is in the skip list above with the error behind it. Re-run as root, or as a user in the group that owns those trees, when that error is permission denied. Pass --skip-unreadable to migrate everything else and leave those repos on the old knot."
    )]
    UnreadableSources { repos: RepoList<RepoDid> },
    #[error(
        "the acl and repo_keys are recorded with different owners for {repos}. Both DIDs per repo are in the drift section above. Settle each repo in the old knot's database, by deleting the acl rows for the wrong owner or by correcting repo_keys.owner_did, since knot-migrate won't pick a winner for you."
    )]
    ConflictingOwners { repos: RepoList<SourceRepoDid> },
}

#[derive(Debug)]
struct Refusals(Vec<Refusal>);

impl Refusals {
    fn gather(mapping: &Mapping, skip_unreadable: bool) -> Self {
        let conflicting = mapping.conflicting_owners();
        let unreadable = mapping.unreadable_sources();
        Self(
            [
                (!conflicting.is_empty())
                    .then_some(Refusal::ConflictingOwners { repos: conflicting }),
                (!unreadable.is_empty() && !skip_unreadable)
                    .then_some(Refusal::UnreadableSources { repos: unreadable }),
            ]
            .into_iter()
            .flatten()
            .collect(),
        )
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Display for Refusals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0
            .iter()
            .enumerate()
            .try_for_each(|(position, refusal)| match position {
                0 => write!(f, "{refusal}"),
                _ => write!(f, "\n{refusal}"),
            })
    }
}

struct Args {
    source_db: PathBuf,
    source_repos: Option<PathBuf>,
    env_file: Option<PathBuf>,
    host_key: Option<PathBuf>,
    target: PathBuf,
    hostname: Option<String>,
    plc_url: Option<String>,
    object_format: ObjectFormat,
    master_key_env: MasterKeyEnv,
    source_policy: SourcePolicy,
    host_key_policy: HostKeyPolicy,
    skip_unreadable: bool,
    dry_run: bool,
}

#[derive(Debug, Default, Clone, Copy)]
struct Switches {
    dry_run: bool,
    consume_source: bool,
    skip_unreadable: bool,
    force_host_key: bool,
}

const KNOWN_FLAGS: [&str; 9] = [
    "source-db",
    "source-repos",
    "env-file",
    "host-key",
    "target",
    "hostname",
    "plc-url",
    "object-format",
    "master-key-env",
];

fn parse_args(args: &[String]) -> Result<Args, MigrateError> {
    let (mut flags, switches, pending) = args.iter().try_fold(
        (
            BTreeMap::<String, String>::new(),
            Switches::default(),
            None::<String>,
        ),
        |(mut flags, switches, pending), arg| match (pending, arg.as_str()) {
            (Some(key), value) if value.starts_with("--") => {
                Err(MigrateError::Usage(format!("--{key} needs a value")))
            }
            (Some(key), value) => match flags.insert(key.clone(), value.to_string()) {
                None => Ok((flags, switches, None)),
                Some(_) => Err(MigrateError::Usage(format!("--{key} given twice"))),
            },
            (None, "--dry-run") => Ok((
                flags,
                Switches {
                    dry_run: true,
                    ..switches
                },
                None,
            )),
            (None, "--consume-source") => Ok((
                flags,
                Switches {
                    consume_source: true,
                    ..switches
                },
                None,
            )),
            (None, "--skip-unreadable") => Ok((
                flags,
                Switches {
                    skip_unreadable: true,
                    ..switches
                },
                None,
            )),
            (None, "--force-host-key") => Ok((
                flags,
                Switches {
                    force_host_key: true,
                    ..switches
                },
                None,
            )),
            (None, flag) => match flag.strip_prefix("--").map(|rest| {
                rest.split_once('=')
                    .map_or((rest, None), |(key, value)| (key, Some(value)))
            }) {
                Some((key, None)) if KNOWN_FLAGS.contains(&key) => {
                    Ok((flags, switches, Some(key.to_string())))
                }
                Some((key, Some(value))) if KNOWN_FLAGS.contains(&key) => {
                    match flags.insert(key.to_string(), value.to_string()) {
                        None => Ok((flags, switches, None)),
                        Some(_) => Err(MigrateError::Usage(format!("--{key} given twice"))),
                    }
                }
                _ => Err(MigrateError::Usage(format!("unexpected argument {flag}"))),
            },
        },
    )?;
    pending.map_or(Ok(()), |key| {
        Err(MigrateError::Usage(format!("--{key} needs a value")))
    })?;
    let mut take = |key: &str| flags.remove(key);
    let required = |key: &str, value: Option<String>| {
        value.ok_or_else(|| MigrateError::Usage(format!("--{key} is required")))
    };
    let object_format = take("object-format").map_or(Ok(ObjectFormat::SHA1), |value| {
        ObjectFormat::from_capability(&value).ok_or_else(|| {
            MigrateError::Usage(format!(
                "--object-format must be sha1 or sha256, not {value}"
            ))
        })
    })?;
    Ok(Args {
        source_db: required("source-db", take("source-db"))?.into(),
        source_repos: take("source-repos").map(PathBuf::from),
        env_file: take("env-file").map(PathBuf::from),
        host_key: take("host-key").map(PathBuf::from),
        target: required("target", take("target"))?.into(),
        hostname: take("hostname"),
        plc_url: take("plc-url"),
        object_format,
        master_key_env: take("master-key-env")
            .map_or_else(|| MasterKeyEnv::new("KNOT_MASTER_KEY"), MasterKeyEnv::new)
            .map_err(|value| {
                MigrateError::Usage(format!(
                    "--master-key-env must be an uppercase env var name, not {value}"
                ))
            })?,
        source_policy: match switches.consume_source {
            true => SourcePolicy::Consume,
            false => SourcePolicy::Preserve,
        },
        host_key_policy: match switches.force_host_key {
            true => HostKeyPolicy::Replace,
            false => HostKeyPolicy::Keep,
        },
        skip_unreadable: switches.skip_unreadable,
        dry_run: switches.dry_run,
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), MigrateError> {
    let args = parse_args(args)?;
    let env = args
        .env_file
        .as_deref()
        .map(EnvFile::read)
        .transpose()?
        .unwrap_or_default();

    args.hostname
        .as_deref()
        .zip(env.get("KNOT_SERVER_HOSTNAME"))
        .filter(|(flag, env_value)| flag != env_value)
        .map_or(Ok(()), |(flag, env_value)| {
            Err(MigrateError::HostnameMismatch {
                flag: flag.to_string(),
                env: env_value.to_string(),
            })
        })?;
    let hostname = args
        .hostname
        .clone()
        .or_else(|| env.get("KNOT_SERVER_HOSTNAME").map(str::to_string))
        .ok_or_else(|| {
            MigrateError::Usage(
                "--hostname is required when the env file specifies none".to_string(),
            )
        })?;
    let hostname = KnotHostname::new(hostname.as_str()).map_err(|error| {
        MigrateError::Usage(format!("hostname {hostname} isn't valid: {error}"))
    })?;
    let plc_url = args
        .plc_url
        .clone()
        .or_else(|| env.get("KNOT_SERVER_PLC_URL").map(str::to_string))
        .ok_or_else(|| {
            MigrateError::Usage(
                "--plc-url is required when the env file specifies none".to_string(),
            )
        })?;
    let plc_directory = Url::parse(&plc_url)
        .ok()
        .filter(|url| url.scheme() == "https" && url.host().is_some())
        .ok_or_else(|| {
            MigrateError::Usage(format!("PLC directory {plc_url} isn't an https URL"))
        })?;
    let source_repos = args
        .source_repos
        .clone()
        .or_else(|| env.get("KNOT_REPO_SCAN_PATH").map(PathBuf::from))
        .ok_or_else(|| {
            MigrateError::Usage(
                "--source-repos is required when the env file specifies no scan path".to_string(),
            )
        })?;
    match source_repos.is_dir() {
        true => Ok(()),
        false => Err(MigrateError::Usage(format!(
            "source repos path {} isn't a directory",
            source_repos.display()
        ))),
    }?;

    let db = SourceDb::open(&args.source_db)?;
    let schema = db.schema()?;
    let repos = db.repos()?;
    let rkeys = repos
        .iter()
        .filter_map(|repo| {
            db.current_rkey(&repo.repo_did)
                .map(|rkey| rkey.map(|rkey| (repo.repo_did.clone(), rkey)))
                .transpose()
        })
        .collect::<Result<BTreeMap<SourceRepoDid, SourceRkey>, SourceError>>()?;
    let resolver = casbin::resolver(repos.iter().map(|repo| {
        (
            repo.owner_did.clone(),
            repo.repo_name.clone(),
            repo.repo_did.clone(),
        )
    }));
    let acl = casbin::decode(&db.acl()?, &resolver)?;
    let probe = |repo_did: &SourceRepoDid| adopt::probe_source(&source_repos, repo_did);
    let mapping = match schema {
        SourceSchema::Tables => mapping::map_tables(
            &repos,
            &rkeys,
            &db.members()?,
            &db.collaborators()?,
            &acl,
            probe,
        )?,
        SourceSchema::PreFlip => mapping::map_preflip(&repos, &rkeys, &db.members()?, &acl, probe)?,
    };
    env.get("KNOT_SERVER_OWNER")
        .filter(|owner| AccountDid::new(*owner).ok().as_ref() != Some(&mapping.knot_owner))
        .map_or(Ok(()), |owner| {
            Err(MigrateError::OwnerMismatch {
                env: owner.to_string(),
                acl: mapping.knot_owner.to_string(),
            })
        })?;
    let orphan_alias_count = db.orphan_alias_count()?;

    let refusals = Refusals::gather(&mapping, args.skip_unreadable);
    match (args.dry_run, refusals.is_empty()) {
        (true, ready) => {
            let rehearsal = timed("rehearsal", || {
                Rehearsal::run(rehearse::Inputs {
                    source_repos: &source_repos,
                    adopted: &mapping.repos,
                    scan_path: &repos_dir(&args.target),
                    policy: args.source_policy,
                    host_key: args.host_key.as_deref(),
                    host_key_target: &host_key_file(&args.target),
                    host_key_policy: args.host_key_policy,
                    master_key: &args.master_key_env,
                })
            });
            report(&mapping, orphan_alias_count, Phase::Rehearsed(&rehearsal));
            match ready {
                false => Err(MigrateError::Refused(refusals)),
                true => rehearsal
                    .ready()
                    .then(|| {
                        println!();
                        println!("we left the target alone. Re-run without --dry-run to migrate.");
                    })
                    .ok_or(MigrateError::RehearsalIncomplete),
            }
        }
        (false, false) => {
            report(&mapping, orphan_alias_count, Phase::Refused);
            Err(MigrateError::Refused(refusals))
        }
        (false, true) => {
            let written = materialize(&args, &hostname, &plc_directory, &source_repos, &mapping)?;
            report(
                &mapping,
                orphan_alias_count,
                Phase::Written {
                    adoption: &written.adoption,
                    cobs: &written.cobs,
                },
            );
            println!();
            println!("knot key identity: {}", written.knot_did);
            println!("host key algorithm: {}", written.host_key_algorithm);
            println!("host key fingerprint: {}", written.host_key_fingerprint);
            match written.host_key_placement {
                HostKeyPlacement::Fresh => (),
                HostKeyPlacement::Unchanged => {
                    println!("host key: this key was already at the target")
                }
                HostKeyPlacement::Replacing => {
                    println!("host key: the migration replaced the different key at the target")
                }
            }
            println!("config: {}", written.config_file.display());
            println!("key archive: {}", written.archive_file.display());
            Ok(())
        }
    }
}

fn repos_dir(target: &Path) -> PathBuf {
    target.join("repos")
}

fn host_key_file(target: &Path) -> PathBuf {
    target.join("ssh_host_key")
}

fn report<'a>(mapping: &'a Mapping, orphan_alias_count: u64, phase: Phase<'a>) {
    print!(
        "{}",
        Report {
            mapping,
            orphan_alias_count,
            phase,
        }
    );
}

fn timed<T>(phase: &str, work: impl FnOnce() -> T) -> T {
    let started = std::time::Instant::now();
    let outcome = work();
    eprintln!("{phase}: {:.1}s", started.elapsed().as_secs_f64());
    outcome
}

struct Written {
    adoption: adopt::AdoptOutcome,
    cobs: emit::CobSummary,
    knot_did: knot_types::KnotId,
    host_key_algorithm: ssh_key::Algorithm,
    host_key_fingerprint: ssh_key::Fingerprint,
    host_key_placement: HostKeyPlacement,
    config_file: PathBuf,
    archive_file: PathBuf,
}

fn materialize(
    args: &Args,
    hostname: &KnotHostname,
    plc_directory: &Url,
    source_repos: &Path,
    mapping: &Mapping,
) -> Result<Written, MigrateError> {
    let host_key_source = args.host_key.as_deref().ok_or_else(|| {
        MigrateError::Usage("--host-key is required for the migration".to_string())
    })?;
    let host_key = emit::load_host_key(host_key_source)?;
    let host_key_placement = emit::plan_host_key(
        &host_key_file(&args.target),
        &host_key,
        args.host_key_policy,
    )?;
    std::fs::create_dir_all(&args.target).map_err(|source| MigrateError::Io {
        context: format!("create {}", args.target.display()),
        source,
    })?;
    let target = args
        .target
        .canonicalize()
        .map_err(|source| MigrateError::Io {
            context: format!("canonicalize {}", args.target.display()),
            source,
        })?;
    let scan_path = repos_dir(&target);
    let sealed_key_file = target.join("sealed-keys");
    let host_key_destination = host_key_file(&target);
    let archive_file = target.join("repo-signing-keys.json");
    let config_file = target.join("config.toml");

    let knot_did = hostname.knot_did();

    let master_key = args.master_key_env.read()?;
    let secrets = SealedStore::open(sealed_key_file.clone(), &master_key, Box::new(OsEntropy))?;
    secrets.ensure(&knot_did)?;
    let signer = secrets.signer(&knot_did)?;

    let layout = knot_git::Layout::new(&scan_path)
        .with_object_format(args.object_format)
        .reserving_meta(&knot_did)?;
    let adoption = timed("adoption", || {
        adopt::adopt_all(&layout, source_repos, &mapping.repos, args.source_policy)
    })?;
    let cobs = timed("cobs", || {
        emit::write_cobs(&layout, &knot_did, mapping, &signer)
    })?;
    emit::write_key_archive(&archive_file, &mapping.repos)?;
    host_key.write_to(&host_key_destination)?;

    let config = emit::render_config(&ConfigValues {
        hostname: hostname.clone(),
        admins: vec![mapping.knot_owner.clone()],
        scan_path,
        ssh_host_key_file: host_key_destination,
        sealed_key_file,
        master_key_env: args.master_key_env.clone(),
        object_format: args.object_format,
        plc_directory: plc_directory.clone(),
    })?;
    std::fs::write(&config_file, config).map_err(|source| MigrateError::Io {
        context: format!("write {}", config_file.display()),
        source,
    })?;

    Ok(Written {
        adoption,
        cobs,
        knot_did,
        host_key_algorithm: host_key.algorithm,
        host_key_fingerprint: host_key.fingerprint,
        host_key_placement,
        config_file,
        archive_file,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(list: &[&str]) -> Result<Args, MigrateError> {
        let owned: Vec<String> = list.iter().map(|arg| arg.to_string()).collect();
        parse_args(&owned)
    }

    #[test]
    fn accepts_space_and_equals_forms() {
        let args = parse(&[
            "--source-db=/data/knotserver.db",
            "--target",
            "/data/knot",
            "--object-format=sha256",
            "--dry-run",
        ])
        .unwrap();
        assert_eq!(args.source_db, PathBuf::from("/data/knotserver.db"));
        assert_eq!(args.target, PathBuf::from("/data/knot"));
        assert_eq!(args.object_format, ObjectFormat::SHA256);
        assert!(args.dry_run);
    }

    #[test]
    fn every_switch_is_off_until_it_is_named() {
        let off = parse(&["--source-db=/db", "--target=/t"]).unwrap();
        assert!(!off.dry_run);
        assert!(!off.skip_unreadable);
        assert_eq!(off.source_policy, SourcePolicy::Preserve);
        assert_eq!(
            off.host_key_policy,
            HostKeyPolicy::Keep,
            "a fingerprint your users already trust survives a migration nobody asked to force"
        );
        let on = parse(&[
            "--source-db=/db",
            "--target=/t",
            "--dry-run",
            "--consume-source",
            "--skip-unreadable",
            "--force-host-key",
        ])
        .unwrap();
        assert!(on.dry_run);
        assert!(on.skip_unreadable);
        assert_eq!(on.source_policy, SourcePolicy::Consume);
        assert_eq!(on.host_key_policy, HostKeyPolicy::Replace);
    }

    #[test]
    fn rejects_duplicates_missing_values_and_unknown_flags() {
        [
            &[
                "--source-db=/data/knotserver.db",
                "--target=/data/knot",
                "--target",
                "/data/other",
            ][..],
            &["--source-db"],
            &["--source-db", "--target"],
            &["--mystery=1", "--source-db=/data/knotserver.db"],
            &["--object-format=blake3", "--source-db=/db", "--target=/t"],
            &["--force-host-key=yes", "--source-db=/db", "--target=/t"],
        ]
        .into_iter()
        .for_each(|args| {
            assert!(
                matches!(parse(args), Err(MigrateError::Usage(_))),
                "{args:?} mustn't parse"
            );
        });
    }
}
