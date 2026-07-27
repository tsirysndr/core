use std::path::{Path, PathBuf};

use knot_cob::{Checkpoint, CobError, CobHome, CobStore, Evaluate};
use knot_cobs::{
    CollaboratorsChange, CollaboratorsCob, Grant, MembersChange, MembersCob, Registration,
    RegistryChange, RegistryError, RepoRegistryCob,
};
use knot_git::{GitError, Layout};
use knot_runtime::Signer;
use knot_types::{AccountDid, ActorId, KnotHostname, KnotId, ObjectFormat, RepoDid, RepoName};
use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;

use crate::mapping::{AdoptRepo, MappedGrant, Mapping};

#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    #[error("meta-repo bootstrap: {0}")]
    Meta(#[from] GitError),
    #[error("open adopted repo {repo}: {source}")]
    OpenRepo { repo: RepoDid, source: GitError },
    #[error("registry record for {repo} has name {existing:?} and the mapping has {name:?}")]
    RegistryNameChanged {
        repo: RepoDid,
        existing: RepoName,
        name: RepoName,
    },
    #[error("{cob} write failed: {source}")]
    Cob { cob: &'static str, source: CobError },
    #[error("registry write failed: {0}")]
    Registry(#[from] RegistryError),
    #[error("{cob} is split across {count} objects")]
    SplitObject { cob: &'static str, count: usize },
    #[error("write {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("host key {path} doesn't parse as an OpenSSH private key: {source}")]
    HostKey {
        path: PathBuf,
        source: ssh_key::Error,
    },
    #[error("knot cannot load the passphrase-protected host key {path} unattended")]
    EncryptedHostKey { path: PathBuf },
    #[error("config template has no line for {section}.{key}")]
    TemplateDrift {
        section: &'static str,
        key: &'static str,
    },
    #[error("{cob} is missing {missing} entries after append")]
    Incomplete { cob: &'static str, missing: usize },
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct GrantSetOutcome {
    pub appended: u64,
    pub already_present: u64,
}

pub struct CobSummary {
    pub members: GrantSetOutcome,
    pub registrations: GrantSetOutcome,
    pub collaborators: GrantSetOutcome,
}

pub fn write_cobs(
    layout: &Layout,
    knot: &KnotId,
    mapping: &Mapping,
    signer: &dyn Signer,
) -> Result<CobSummary, EmitError> {
    let meta = layout.bootstrap_meta(knot)?;
    let home = CobHome::from(knot);
    let store = CobStore::new(&meta);

    let members = write_grant_set::<MembersCob>(
        &store,
        &home,
        "members",
        &mapping.members,
        signer,
        MembersChange::Add,
    )?;
    let registrations = write_registry(&store, &home, &mapping.repos, signer)?;
    let collaborators =
        mapping
            .repos
            .iter()
            .try_fold(GrantSetOutcome::default(), |outcome, repo| {
                let sum = write_repo_collaborators(layout, repo, signer)?;
                Ok::<_, EmitError>(GrantSetOutcome {
                    appended: outcome.appended + sum.appended,
                    already_present: outcome.already_present + sum.already_present,
                })
            })?;

    Ok(CobSummary {
        members,
        registrations,
        collaborators,
    })
}

fn write_repo_collaborators(
    layout: &Layout,
    repo: &AdoptRepo,
    signer: &dyn Signer,
) -> Result<GrantSetOutcome, EmitError> {
    if repo.collaborators.is_empty() {
        return Ok(GrantSetOutcome::default());
    }
    let git = layout
        .open(&repo.did)
        .map_err(|source| EmitError::OpenRepo {
            repo: repo.did.clone(),
            source,
        })?;
    let home = CobHome::from(&repo.did);
    let store = CobStore::new(&git);
    write_grant_set::<CollaboratorsCob>(
        &store,
        &home,
        "collaborators",
        &repo.collaborators,
        signer,
        CollaboratorsChange::Add,
    )
}

fn write_grant_set<E>(
    store: &CobStore,
    home: &CobHome,
    cob: &'static str,
    grants: &[MappedGrant],
    signer: &dyn Signer,
    make: impl Fn(Grant) -> E::Change,
) -> Result<GrantSetOutcome, EmitError>
where
    E: Checkpoint + Evaluate<State = knot_cobs::Roster>,
    E::State: Serialize + DeserializeOwned,
{
    write_batch::<E, MappedGrant>(
        store,
        home,
        cob,
        grants,
        signer,
        |grant| make(to_grant(grant)),
        |grant| grant.created_at,
        |roster, grant| roster.contains(&grant.subject),
        |_| Ok(()),
    )
}

fn write_registry(
    store: &CobStore,
    home: &CobHome,
    repos: &[AdoptRepo],
    signer: &dyn Signer,
) -> Result<GrantSetOutcome, EmitError> {
    write_batch::<RepoRegistryCob, AdoptRepo>(
        store,
        home,
        "registry",
        repos,
        signer,
        |repo| RegistryChange::Register(registration(repo)),
        |repo| repo.created_at,
        |registry, repo| {
            registry.record_of(&repo.did).is_some_and(|record| {
                record.owner == repo.owner && record.rkey == repo.rkey && record.name == repo.name
            })
        },
        |registry| {
            repos.iter().try_for_each(|repo| {
                match (
                    registry.record_of(&repo.did),
                    registry.resolve(&repo.owner, &repo.rkey),
                ) {
                    (Some(record), _) if record.owner != repo.owner || record.rkey != repo.rkey => {
                        Err(EmitError::Registry(RegistryError::AlreadyRegistered {
                            repo: repo.did.clone(),
                            owner: record.owner.clone(),
                            rkey: record.rkey.clone(),
                        }))
                    }
                    (Some(record), _) if record.name != repo.name => {
                        Err(EmitError::RegistryNameChanged {
                            repo: repo.did.clone(),
                            existing: record.name.clone(),
                            name: repo.name.clone(),
                        })
                    }
                    (None, Some(holder)) if holder != &repo.did => {
                        Err(EmitError::Registry(RegistryError::RkeyTaken {
                            owner: repo.owner.clone(),
                            rkey: repo.rkey.clone(),
                            existing: holder.clone(),
                        }))
                    }
                    _ => Ok(()),
                }
            })
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn write_batch<E, T>(
    store: &CobStore,
    home: &CobHome,
    cob: &'static str,
    items: &[T],
    signer: &dyn Signer,
    make: impl Fn(&T) -> E::Change,
    stamp: impl Fn(&T) -> knot_types::UnixSeconds,
    present: impl Fn(&E::State, &T) -> bool,
    precheck: impl Fn(&E::State) -> Result<(), EmitError>,
) -> Result<GrantSetOutcome, EmitError>
where
    E: Checkpoint,
    E::State: Serialize + DeserializeOwned,
{
    let fail = |source: CobError| EmitError::Cob { cob, source };
    let objects = store.list::<E>().map_err(fail)?;
    let (object, state, created) = match (objects.as_slice(), items) {
        (_, []) => return Ok(GrantSetOutcome::default()),
        ([], [first, ..]) => {
            let change = make(first);
            let created = store
                .create(home, &change, signer, stamp(first))
                .map_err(fail)?;
            let author = ActorId::from_secp256k1(signer.public_key().as_bytes());
            (created.object, E::apply(E::initial(), change, &author), 1)
        }
        ([object], _) => {
            let (state, _) = store.materialize::<E>(*object).map_err(fail)?;
            (*object, state, 0)
        }
        (many, _) => {
            return Err(EmitError::SplitObject {
                cob,
                count: many.len(),
            });
        }
    };
    precheck(&state)?;

    let missing: Vec<&T> = items.iter().filter(|item| !present(&state, item)).collect();
    missing.split_last().map_or(Ok(()), |(last, head)| {
        let changes: Vec<E::Change> = head.iter().map(|item| make(item)).collect();
        store
            .extend(
                home,
                object,
                changes.iter().zip(head.iter().map(|item| stamp(item))),
                signer,
            )
            .map_err(fail)?;
        store
            .update_with_checkpointed::<E, CobError>(home, object, signer, stamp(last), |_| {
                Ok(make(last))
            })
            .map(|_| ())
            .map_err(fail)
    })?;

    let (folded, _) = store.materialize::<E>(object).map_err(fail)?;
    let absent = items.iter().filter(|item| !present(&folded, item)).count();
    match absent {
        0 => Ok(GrantSetOutcome {
            appended: created + missing.len() as u64,
            already_present: (items.len() as u64)
                .saturating_sub(missing.len() as u64)
                .saturating_sub(created),
        }),
        count => Err(EmitError::Incomplete {
            cob,
            missing: count,
        }),
    }
}

fn registration(repo: &AdoptRepo) -> Registration {
    Registration {
        owner: repo.owner.clone(),
        rkey: repo.rkey.clone(),
        name: repo.name.clone(),
        repo: repo.did.clone(),
        created_at: repo.created_at,
    }
}

fn to_grant(grant: &MappedGrant) -> Grant {
    Grant {
        subject: grant.subject.clone(),
        added_by: grant.added_by.clone(),
        created_at: grant.created_at,
    }
}

#[derive(Serialize)]
struct ArchivedKey<'a> {
    repo_did: &'a RepoDid,
    key_type: &'a str,
    #[serde(serialize_with = "secret_str")]
    secret_key_hex: zeroize::Zeroizing<String>,
}

fn secret_str<S: serde::Serializer>(
    value: &zeroize::Zeroizing<String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(value)
}

// do not be alarmed, for there is a plan for this
pub fn write_key_archive(path: &Path, repos: &[AdoptRepo]) -> Result<(), EmitError> {
    let keys: Vec<ArchivedKey<'_>> = repos
        .iter()
        .map(|repo| ArchivedKey {
            repo_did: &repo.did,
            key_type: "k256",
            secret_key_hex: repo.signing_key.to_hex(),
        })
        .collect();
    let body = zeroize::Zeroizing::new(
        serde_json::to_string_pretty(&keys).expect("key archive serializes"),
    );
    write_private(path, body.as_bytes())
}

pub struct HostKey {
    bytes: zeroize::Zeroizing<Vec<u8>>,
    pub algorithm: ssh_key::Algorithm,
}

impl HostKey {
    pub fn write_to(&self, destination: &Path) -> Result<(), EmitError> {
        write_private(destination, &self.bytes)
    }
}

pub fn load_host_key(source: &Path) -> Result<HostKey, EmitError> {
    let bytes = zeroize::Zeroizing::new(std::fs::read(source).map_err(|error| EmitError::Io {
        path: source.to_path_buf(),
        source: error,
    })?);
    let key = ssh_key::PrivateKey::from_openssh(bytes.as_slice()).map_err(|error| {
        EmitError::HostKey {
            path: source.to_path_buf(),
            source: error,
        }
    })?;
    if key.is_encrypted() {
        return Err(EmitError::EncryptedHostKey {
            path: source.to_path_buf(),
        });
    }
    Ok(HostKey {
        algorithm: key.algorithm(),
        bytes,
    })
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), EmitError> {
    use std::io::Write;
    let io = |error: std::io::Error| EmitError::Io {
        path: path.to_path_buf(),
        source: error,
    };
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(io)?;
    }
    file.write_all(bytes).map_err(io)?;
    file.sync_all().map_err(io)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
// This name goes into the generated config as `secrets.master_key_env`,
// where knot-config runs `is_env_var_name`.
// So same rule here such the migration fails *now* instead of at the end.
pub struct MasterKeyEnv(String);

impl MasterKeyEnv {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let valid = !value.is_empty()
            && !value.starts_with(|c: char| c.is_ascii_digit())
            && value
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        match valid {
            true => Ok(Self(value)),
            false => Err(value),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MasterKeyEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub struct ConfigValues {
    pub hostname: KnotHostname,
    pub admins: Vec<AccountDid>,
    pub scan_path: PathBuf,
    pub ssh_host_key_file: PathBuf,
    pub sealed_key_file: PathBuf,
    pub master_key_env: MasterKeyEnv,
    pub object_format: ObjectFormat,
    pub plc_directory: Url,
}

pub fn render_config(values: &ConfigValues) -> Result<String, EmitError> {
    let fills: Vec<((&'static str, &'static str), String)> = [
        (("server", "hostname"), quote(values.hostname.as_str())),
        (
            ("server", "admins"),
            format!(
                "[{}]",
                values
                    .admins
                    .iter()
                    .map(|admin| quote(admin.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        (
            ("server", "ssh_host_key_file"),
            quote_path(&values.ssh_host_key_file),
        ),
        (("acl", "admission"), quote("closed")),
        (("repo", "scan_path"), quote_path(&values.scan_path)),
        (
            ("git", "object_format"),
            quote(values.object_format.capability()),
        ),
        (
            ("secrets", "sealed_key_file"),
            quote_path(&values.sealed_key_file),
        ),
        (
            ("secrets", "master_key_env"),
            quote(values.master_key_env.as_str()),
        ),
        (
            ("atproto", "plc_directory"),
            quote(values.plc_directory.as_str()),
        ),
    ]
    .into_iter()
    .collect();

    let template = knot_config::template();
    let (lines, pending, _) = template.lines().fold(
        (Vec::new(), fills, ""),
        |(mut lines, pending, section), line| {
            let section = line
                .trim()
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
                .unwrap_or(section);
            let matched = pending.iter().position(|((expected, key), _)| {
                *expected == section && line.trim().starts_with(&format!("#{key} ="))
            });
            let remaining = match matched {
                Some(index) => {
                    let ((_, key), value) = &pending[index];
                    lines.push(format!("{key} = {value}"));
                    pending
                        .into_iter()
                        .enumerate()
                        .filter(|(position, _)| *position != index)
                        .map(|(_, fill)| fill)
                        .collect()
                }
                None => {
                    lines.push(line.to_string());
                    pending
                }
            };
            (lines, remaining, section)
        },
    );
    pending.first().map_or(Ok(()), |((section, key), _)| {
        Err(EmitError::TemplateDrift { section, key })
    })?;
    Ok(lines.join("\n") + "\n")
}

fn quote(value: &str) -> String {
    let escaped: String = value
        .chars()
        .map(|c| match c {
            '"' => "\\\"".to_string(),
            '\\' => "\\\\".to_string(),
            '\u{8}' => "\\b".to_string(),
            '\t' => "\\t".to_string(),
            '\n' => "\\n".to_string(),
            '\u{c}' => "\\f".to_string(),
            '\r' => "\\r".to_string(),
            c if c.is_control() => format!("\\u{:04X}", u32::from(c)),
            c => c.to_string(),
        })
        .collect();
    format!("\"{escaped}\"")
}

fn quote_path(path: &Path) -> String {
    quote(&path.to_string_lossy())
}
