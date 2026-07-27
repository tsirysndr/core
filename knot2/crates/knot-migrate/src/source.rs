use std::collections::BTreeSet;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Row};

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("source database query failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(
        "source database predates DID-keyed repos. Upgrade tangled-knot to its latest release, let it finish its own migrations, then run knot-migrate again."
    )]
    PreDid,
    #[error("source database has no acl table for the casbin cross-check")]
    MissingAcl,
    #[error(
        "source database has a collaborators table but no knot_members table. Upgrade tangled-knot to its latest release, let it finish its own migrations, then run knot-migrate again."
    )]
    CollaboratorsWithoutMembers,
    #[error(
        "source table `{table}` is missing expected columns {}. This tangled-knot predates the schema knot-migrate reads. Upgrade tangled-knot to its latest release, let it finish its own migrations, then run knot-migrate again.",
        .missing.join(", ")
    )]
    SchemaMismatch { table: String, missing: Vec<String> },
}

knot_types::text_newtype! {
    pub struct SourceRepoDid(String) => verbatim as from_column;
    pub struct SourceDid(String) => verbatim as from_column;
    pub struct SourceRkey(String) => verbatim as from_column;
    pub struct SourceRepoName(String) => verbatim as from_column;
    pub struct SourceRepoObject(String) => verbatim as from_column;
    pub struct SourceTimestamp(String) => verbatim as from_column;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKeyType {
    K256,
    Other(String),
}

impl SourceKeyType {
    pub fn from_column(value: impl Into<String>) -> Self {
        let value = value.into();
        match value.as_str() {
            "k256" => Self::K256,
            _ => Self::Other(value),
        }
    }

    pub fn is_k256(&self) -> bool {
        matches!(self, Self::K256)
    }
}

impl ::std::fmt::Display for SourceKeyType {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        match self {
            Self::K256 => f.write_str("k256"),
            Self::Other(value) => f.write_str(value),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceSchema {
    Tables,
    PreFlip,
}

#[derive(Clone, PartialEq, Eq, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
// Every repo's private key, straight outta the old knot's db.
// `Debug` doesn't prints any bytes on purpose,
// as to not compromise a migration.
pub struct SourceSigningKey(Vec<u8>);

impl SourceSigningKey {
    pub fn from_column(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for SourceSigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SourceSigningKey").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRow {
    pub repo_did: SourceRepoDid,
    pub owner_did: SourceDid,
    pub repo_name: SourceRepoName,
    pub signing_key: SourceSigningKey,
    pub key_type: SourceKeyType,
    pub created_at: SourceTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRow {
    pub did: SourceDid,
    pub subject: SourceDid,
    pub created: SourceTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollabRow {
    pub repo_did: SourceRepoDid,
    pub subject_did: SourceDid,
    pub added_by_did: SourceDid,
    pub created: SourceTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclRow {
    pub p_type: String,
    pub v0: String,
    pub v1: String,
    pub v2: String,
    pub v3: String,
}

pub struct SourceDb {
    conn: Connection,
}

impl SourceDb {
    pub fn open(path: &Path) -> Result<Self, SourceError> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self { conn })
    }

    pub fn schema(&self) -> Result<SourceSchema, SourceError> {
        let variant = match (
            self.has_table("repo_keys")?,
            self.has_table("repo_aliases")?,
            self.has_table("knot_members")?,
            self.has_table("collaborators")?,
        ) {
            (true, true, true, true) => SourceSchema::Tables,
            (true, true, false, true) => return Err(SourceError::CollaboratorsWithoutMembers),
            (true, true, _, false) => SourceSchema::PreFlip,
            _ => return Err(SourceError::PreDid),
        };
        let checks: Vec<(&str, &[&str])> = [
            Some((
                "repo_keys",
                &[
                    "repo_did",
                    "owner_did",
                    "repo_name",
                    "signing_key",
                    "key_type",
                    "created_at",
                ][..],
            )),
            Some(("repo_aliases", &["rkey", "repo_did", "rev"][..])),
            self.has_table("acl")?
                .then_some(("acl", &["p_type", "v0", "v1", "v2", "v3"][..])),
            self.has_table("knot_members")?
                .then_some(("knot_members", &["id", "did", "subject", "created"][..])),
            (variant == SourceSchema::Tables).then_some((
                "collaborators",
                &["id", "repo_did", "subject_did", "added_by_did", "created"][..],
            )),
        ]
        .into_iter()
        .flatten()
        .collect();
        checks
            .into_iter()
            .try_for_each(|(table, columns)| self.require_columns(table, columns))?;
        Ok(variant)
    }

    fn has_table(&self, name: &str) -> Result<bool, SourceError> {
        let count: i64 = self.conn.query_row(
            "select count(*) from sqlite_master where type = 'table' and name = ?1",
            [name],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    fn require_columns(&self, table: &str, required: &[&str]) -> Result<(), SourceError> {
        let present: BTreeSet<String> = self
            .conn
            .prepare("select name from pragma_table_info(?1)")?
            .query_map([table], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        let missing: Vec<String> = required
            .iter()
            .filter(|column| !present.contains(**column))
            .map(|column| (*column).to_string())
            .collect();
        missing
            .is_empty()
            .then_some(())
            .ok_or(SourceError::SchemaMismatch {
                table: table.to_string(),
                missing,
            })
    }

    pub fn repos(&self) -> Result<Vec<RepoRow>, SourceError> {
        self.collect(
            "select repo_did, owner_did, repo_name, signing_key, key_type, created_at
             from repo_keys order by created_at, repo_did",
            |row| {
                Ok(RepoRow {
                    repo_did: SourceRepoDid::from_column(row.get::<_, String>(0)?),
                    owner_did: SourceDid::from_column(row.get::<_, String>(1)?),
                    repo_name: SourceRepoName::from_column(row.get::<_, String>(2)?),
                    signing_key: SourceSigningKey::from_column(row.get(3)?),
                    key_type: SourceKeyType::from_column(row.get::<_, String>(4)?),
                    created_at: SourceTimestamp::from_column(row.get::<_, String>(5)?),
                })
            },
        )
    }

    pub fn members(&self) -> Result<Vec<MemberRow>, SourceError> {
        if !self.has_table("knot_members")? {
            return Ok(Vec::new());
        }
        self.collect(
            "select did, subject, created from knot_members
             where id in (select min(id) from knot_members group by subject)
             order by id",
            |row| {
                Ok(MemberRow {
                    did: SourceDid::from_column(row.get::<_, String>(0)?),
                    subject: SourceDid::from_column(row.get::<_, String>(1)?),
                    created: SourceTimestamp::from_column(row.get::<_, String>(2)?),
                })
            },
        )
    }

    pub fn collaborators(&self) -> Result<Vec<CollabRow>, SourceError> {
        if !self.has_table("collaborators")? {
            return Ok(Vec::new());
        }
        self.collect(
            "select repo_did, subject_did, added_by_did, created from collaborators order by id",
            |row| {
                Ok(CollabRow {
                    repo_did: SourceRepoDid::from_column(row.get::<_, String>(0)?),
                    subject_did: SourceDid::from_column(row.get::<_, String>(1)?),
                    added_by_did: SourceDid::from_column(row.get::<_, String>(2)?),
                    created: SourceTimestamp::from_column(row.get::<_, String>(3)?),
                })
            },
        )
    }

    pub fn acl(&self) -> Result<Vec<AclRow>, SourceError> {
        if !self.has_table("acl")? {
            return Err(SourceError::MissingAcl);
        }
        self.collect(
            "select p_type, v0, v1, v2, v3 from acl order by rowid",
            |row| {
                Ok(AclRow {
                    p_type: row.get(0)?,
                    v0: row.get(1)?,
                    v1: row.get(2)?,
                    v2: row.get(3)?,
                    v3: row.get(4)?,
                })
            },
        )
    }

    pub fn current_rkey(
        &self,
        repo_did: &SourceRepoDid,
    ) -> Result<Option<SourceRkey>, SourceError> {
        self.conn
            .query_row(
                "select rkey from repo_aliases
                 where repo_did = ?
                 order by rev desc
                 limit 1",
                [repo_did.as_str()],
                |row| row.get::<_, String>(0).map(SourceRkey::from_column),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn orphan_alias_count(&self) -> Result<u64, SourceError> {
        let count: i64 = self.conn.query_row(
            "select count(*) from repo_aliases ra
             where not exists (select 1 from repo_keys rk where rk.repo_did = ra.repo_did)",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    fn collect<T>(
        &self,
        sql: &str,
        map: impl Fn(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>, SourceError> {
        let mut statement = self.conn.prepare(sql)?;
        let rows = statement
            .query_map([], map)?
            .collect::<rusqlite::Result<Vec<T>>>()?;
        Ok(rows)
    }
}
