use bobbin_types::edges::{ExtractError, Record};
use bobbin_types::legacy::{
    LegacyCollaborator, LegacyIssue, LegacyKnotMember, LegacyPublicKey, LegacyPull, LegacyRecord,
    LegacyRefUpdate, LegacyRepo, LegacySource, LegacyStar, LegacyTarget,
};
use bobbin_types::sh_tangled::feed::star::{Repo as StarRepo, Star, StarString, StarSubject};
use bobbin_types::sh_tangled::git::ref_update::RefUpdate;
use bobbin_types::sh_tangled::knot::member::Member as KnotMember;
use bobbin_types::sh_tangled::public_key::PublicKey;
use bobbin_types::sh_tangled::repo::Repo;
use bobbin_types::sh_tangled::repo::collaborator::Collaborator;
use bobbin_types::sh_tangled::repo::issue::Issue;
use bobbin_types::sh_tangled::repo::pull::{Pull, Round, Source, Target};
use jacquard_common::types::did::Did;
use jacquard_common::types::nsid::Nsid;
use jacquard_common::types::string::AtUri;
use jacquard_common::{BosStr, DefaultStr};

use crate::normalize::{is_repo_at_uri, resolve_repo_uri};
use crate::{RepoIdResolver, Resolution};
use jacquard_common::IntoStatic;
use jacquard_common::types::ident::AtIdentifier;
use jacquard_common::types::recordkey::Rkey;

#[derive(Debug)]
pub enum DecodedRecord {
    Canon(Record),
    Legacy(LegacyRecord),
}

impl DecodedRecord {
    pub fn try_decode<S: BosStr + AsRef<str>>(
        nsid: &Nsid<S>,
        bytes: &[u8],
    ) -> Result<Self, ExtractError> {
        match Record::from_json_bytes(nsid, bytes) {
            Ok(record) => Ok(Self::Canon(record)),
            Err(canon_err) => {
                let normalized = normalize_record_fields(bytes);
                let working: &[u8] = normalized.as_deref().unwrap_or(bytes);
                if normalized.is_some()
                    && let Ok(record) = Record::from_json_bytes(nsid, working)
                {
                    return Ok(Self::Canon(record));
                }
                if let Some(scrubbed) = scrub_record_bytes(nsid, working) {
                    if let Ok(record) = Record::from_json_bytes(nsid, &scrubbed) {
                        return Ok(Self::Canon(record));
                    }
                    if let Ok(legacy) = LegacyRecord::from_json_bytes(nsid, &scrubbed) {
                        return Ok(Self::Legacy(legacy));
                    }
                }
                match LegacyRecord::from_json_bytes(nsid, working) {
                    Ok(legacy) => Ok(Self::Legacy(legacy)),
                    Err(_) => Err(canon_err),
                }
            }
        }
    }
}

pub fn normalize_record_fields(bytes: &[u8]) -> Option<alloc::vec::Vec<u8>> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let reserialized = serde_json::to_vec(&value).ok()?;
    (reserialized.as_slice() != bytes).then_some(reserialized)
}

pub fn synthesize_created_at(bytes: &[u8], fallback_rfc3339: &str) -> Option<alloc::vec::Vec<u8>> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let obj = value.as_object_mut()?;
    let needs_fill = match obj.get("createdAt") {
        None => true,
        Some(serde_json::Value::String(s)) if s.is_empty() => true,
        _ => false,
    };
    if !needs_fill {
        return None;
    }
    obj.insert(
        "createdAt".to_owned(),
        serde_json::Value::String(fallback_rfc3339.to_owned()),
    );
    serde_json::to_vec(&value).ok()
}

#[derive(Clone, Copy, Debug)]
enum FieldRule {
    DropIfEmptyString,
    NullToEmptyArray,
}

fn scrub_rules(nsid: &str) -> &'static [(&'static str, FieldRule)] {
    match nsid {
        "sh.tangled.actor.profile" => &[("preferredHandle", FieldRule::DropIfEmptyString)],
        "sh.tangled.label.op" => &[
            ("add", FieldRule::NullToEmptyArray),
            ("delete", FieldRule::NullToEmptyArray),
        ],
        "sh.tangled.repo.pull" => &[("rounds", FieldRule::NullToEmptyArray)],
        _ => &[],
    }
}

pub fn scrub_record_bytes<S: BosStr + AsRef<str>>(
    nsid: &Nsid<S>,
    bytes: &[u8],
) -> Option<alloc::vec::Vec<u8>> {
    let rules = scrub_rules(nsid.as_ref());
    if rules.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let mut obj = value.as_object()?.clone();
    let touched: alloc::vec::Vec<(&str, FieldRule)> = rules
        .iter()
        .filter_map(|(field, rule)| match (rule, obj.get(*field)) {
            (FieldRule::DropIfEmptyString, Some(serde_json::Value::String(s))) if s.is_empty() => {
                Some((*field, *rule))
            }
            (FieldRule::NullToEmptyArray, Some(serde_json::Value::Null)) => Some((*field, *rule)),
            _ => None,
        })
        .collect();
    if touched.is_empty() {
        return None;
    }
    touched.iter().for_each(|(field, rule)| match rule {
        FieldRule::DropIfEmptyString => {
            obj.remove(*field);
        }
        FieldRule::NullToEmptyArray => {
            obj.insert(
                (*field).to_owned(),
                serde_json::Value::Array(alloc::vec::Vec::new()),
            );
        }
    });
    tracing::debug!(nsid = %nsid.as_ref(), ?touched, "scrubbing fields before record retry");
    serde_json::to_vec(&serde_json::Value::Object(obj)).ok()
}

async fn upgrade_repo_did(
    resolver: &RepoIdResolver,
    at_uri: Option<AtUri<DefaultStr>>,
    explicit_did: Option<Did<DefaultStr>>,
) -> Option<Did<DefaultStr>> {
    if let Some(d) = explicit_did {
        return Some(d);
    }
    let uri = at_uri?;
    resolve_repo_uri(resolver, &uri).await
}

pub async fn upgrade_wire_bytes<S: BosStr + AsRef<str>>(
    nsid: &Nsid<S>,
    bytes: &[u8],
    resolver: &RepoIdResolver,
) -> Result<alloc::vec::Vec<u8>, ExtractError> {
    let legacy = LegacyRecord::from_json_bytes(nsid, bytes)?;
    let canon = upgrade(legacy, resolver)
        .await
        .ok_or_else(|| upgrade_failed(nsid))?;
    serialize_canon_variant(&canon).map_err(ExtractError::DecodeJson)
}

fn upgrade_failed<S: BosStr + AsRef<str>>(nsid: &Nsid<S>) -> ExtractError {
    ExtractError::UnknownCollection(alloc::format!("{}: legacy upgrade failed", nsid.as_ref()))
}

pub async fn decode_canon_or_upgrade<S: BosStr + AsRef<str>>(
    nsid: &Nsid<S>,
    bytes: &[u8],
    resolver: &RepoIdResolver,
) -> Result<Record, ExtractError> {
    match DecodedRecord::try_decode(nsid, bytes)? {
        DecodedRecord::Canon(r) => Ok(r),
        DecodedRecord::Legacy(legacy) => upgrade(legacy, resolver)
            .await
            .ok_or_else(|| upgrade_failed(nsid)),
    }
}

pub async fn decode_canon_or_upgrade_bytes<'a, S: BosStr + AsRef<str>>(
    nsid: &Nsid<S>,
    bytes: &'a [u8],
    resolver: &RepoIdResolver,
) -> Result<(Record, alloc::borrow::Cow<'a, [u8]>), ExtractError> {
    let decoded = DecodedRecord::try_decode(nsid, bytes)?;
    match decoded {
        DecodedRecord::Canon(r) => Ok((r, alloc::borrow::Cow::Borrowed(bytes))),
        DecodedRecord::Legacy(legacy) => {
            let canon = upgrade(legacy, resolver)
                .await
                .ok_or_else(|| upgrade_failed(nsid))?;
            let canon_bytes = serialize_canon_variant(&canon).map_err(ExtractError::DecodeJson)?;
            Ok((canon, alloc::borrow::Cow::Owned(canon_bytes)))
        }
    }
}

fn serialize_canon_variant(record: &Record) -> Result<alloc::vec::Vec<u8>, serde_json::Error> {
    match record {
        Record::Issue(r) => serde_json::to_vec(r),
        Record::Pull(r) => serde_json::to_vec(r),
        Record::Collaborator(r) => serde_json::to_vec(r),
        Record::RefUpdate(r) => serde_json::to_vec(r),
        Record::Star(r) => serde_json::to_vec(r),
        Record::PublicKey(r) => serde_json::to_vec(r),
        Record::Repo(r) => serde_json::to_vec(r),
        Record::KnotMember(r) => serde_json::to_vec(r),
        _ => unreachable!(
            "upgrade only produces Issue/Pull/Collaborator/RefUpdate/Star/PublicKey/Repo/KnotMember"
        ),
    }
}

pub async fn upgrade(legacy: LegacyRecord, resolver: &RepoIdResolver) -> Option<Record> {
    match legacy {
        LegacyRecord::Issue(l) => upgrade_issue(l, resolver).await.map(Record::Issue),
        LegacyRecord::Pull(l) => upgrade_pull(l, resolver).await.map(Record::Pull),
        LegacyRecord::Collaborator(l) => upgrade_collaborator(l, resolver)
            .await
            .map(Record::Collaborator),
        LegacyRecord::RefUpdate(l) => Some(Record::RefUpdate(upgrade_ref_update(l))),
        LegacyRecord::Star(l) => upgrade_star(l, resolver).await.map(Record::Star),
        LegacyRecord::PublicKey(l) => Some(Record::PublicKey(upgrade_public_key(l))),
        LegacyRecord::Repo(l) => Some(Record::Repo(upgrade_repo(l))),
        LegacyRecord::KnotMember(l) => Some(Record::KnotMember(upgrade_knot_member(l))),
    }
}

async fn upgrade_issue(
    l: LegacyIssue<DefaultStr>,
    resolver: &RepoIdResolver,
) -> Option<Issue<DefaultStr>> {
    let repo = upgrade_repo_did(resolver, l.repo, l.repo_did).await?;
    Some(Issue {
        created_at: l.created_at,
        body: l.body,
        mentions: l.mentions,
        references: l.references,
        repo,
        title: l.title,
        extra_data: l.extra_data,
    })
}

async fn upgrade_target(
    l: LegacyTarget<DefaultStr>,
    resolver: &RepoIdResolver,
) -> Option<Target<DefaultStr>> {
    let repo = upgrade_repo_did(resolver, l.repo, l.repo_did).await?;
    Some(Target {
        branch: l.branch,
        repo,
        extra_data: None,
    })
}

async fn upgrade_source(
    l: LegacySource<DefaultStr>,
    resolver: &RepoIdResolver,
) -> Source<DefaultStr> {
    let repo = upgrade_repo_did(resolver, l.repo, l.repo_did).await;
    Source {
        branch: l.branch,
        repo,
        extra_data: None,
    }
}

async fn upgrade_pull(
    l: LegacyPull<DefaultStr>,
    resolver: &RepoIdResolver,
) -> Option<Pull<DefaultStr>> {
    let target = upgrade_target(l.target, resolver).await?;
    let source = match l.source {
        Some(s) => Some(upgrade_source(s, resolver).await),
        None => None,
    };
    let rounds = if l.rounds.is_empty() {
        l.patch_blob
            .map(|patch_blob| {
                alloc::vec![Round {
                    created_at: l.created_at.clone(),
                    patch_blob,
                    extra_data: None,
                }]
            })
            .unwrap_or_default()
    } else {
        l.rounds
    };
    Some(Pull {
        created_at: l.created_at,
        body: l.body,
        dependent_on: l.dependent_on,
        mentions: l.mentions,
        references: l.references,
        rounds,
        source,
        target,
        title: l.title,
        extra_data: l.extra_data,
    })
}

fn upgrade_public_key(l: LegacyPublicKey<DefaultStr>) -> PublicKey<DefaultStr> {
    PublicKey {
        created_at: l.created,
        key: l.key,
        name: l.name,
        extra_data: l.extra_data,
    }
}

fn upgrade_repo(l: LegacyRepo<DefaultStr>) -> Repo<DefaultStr> {
    let _ = l.owner;
    Repo {
        created_at: l.added_at,
        description: l.description,
        knot: l.knot,
        labels: None,
        name: l.name,
        repo_did: None,
        source: None,
        spindle: None,
        topics: None,
        website: None,
        extra_data: l.extra_data,
    }
}

fn upgrade_knot_member(l: LegacyKnotMember<DefaultStr>) -> KnotMember<DefaultStr> {
    KnotMember {
        created_at: l.added_at,
        domain: l.domain,
        subject: l.member,
        extra_data: l.extra_data,
    }
}

async fn upgrade_collaborator(
    l: LegacyCollaborator<DefaultStr>,
    resolver: &RepoIdResolver,
) -> Option<Collaborator<DefaultStr>> {
    let repo = upgrade_repo_did(resolver, l.repo, l.repo_did).await?;
    Some(Collaborator {
        created_at: l.created_at,
        repo,
        subject: l.subject,
        extra_data: l.extra_data,
    })
}

fn upgrade_ref_update(l: LegacyRefUpdate<DefaultStr>) -> RefUpdate<DefaultStr> {
    RefUpdate {
        committer_did: l.committer_did,
        meta: l.meta,
        new_sha: l.new_sha,
        old_sha: l.old_sha,
        owner_did: l.owner_did,
        r#ref: l.r#ref,
        repo: l.repo_did,
        extra_data: l.extra_data,
    }
}

async fn upgrade_star(
    l: LegacyStar<DefaultStr>,
    resolver: &RepoIdResolver,
) -> Option<Star<DefaultStr>> {
    let subject = if let Some(did) = l.subject_did {
        StarSubject::Repo(alloc::boxed::Box::new(StarRepo {
            did,
            extra_data: None,
        }))
    } else {
        let uri = l.subject?;
        let resolved = if is_repo_at_uri(&uri) {
            cached_repo_did(resolver, &uri).await
        } else {
            None
        };
        match resolved {
            Some(did) => StarSubject::Repo(alloc::boxed::Box::new(StarRepo {
                did,
                extra_data: None,
            })),
            None => StarSubject::String(alloc::boxed::Box::new(StarString {
                uri,
                extra_data: None,
            })),
        }
    };
    Some(Star {
        created_at: l.created_at,
        subject,
        extra_data: l.extra_data,
    })
}

async fn cached_repo_did(
    resolver: &RepoIdResolver,
    uri: &jacquard_common::types::string::AtUri<DefaultStr>,
) -> Option<Did<DefaultStr>> {
    let owner = match uri.authority() {
        AtIdentifier::Did(d) => d.clone().into_static(),
        AtIdentifier::Handle(_) => return None,
    };
    let rkey: Rkey<DefaultStr> = uri.rkey()?.clone().into_static();
    match resolver.cached_resolution(&owner, &rkey).await? {
        Resolution::Mapped(did) => Some(did),
        Resolution::NoRepoDid | Resolution::Unresolvable => None,
    }
}

extern crate alloc;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RepoIdResolver;
    use bobbin_runtime::RuntimeHasher;
    use bobbin_types::edges::Record;
    use jacquard_common::DefaultStr;
    use jacquard_common::types::did::Did;
    use jacquard_common::types::recordkey::Rkey;

    fn did(s: &str) -> Did<DefaultStr> {
        Did::new_owned(s).unwrap()
    }

    fn rkey(s: &str) -> Rkey<DefaultStr> {
        Rkey::new_owned(s).unwrap()
    }

    fn nsid(s: &'static str) -> Nsid<DefaultStr> {
        Nsid::new_static(s).unwrap()
    }

    #[test]
    fn legacy_decode_routes_through_try_decode_for_known_nsids() {
        let json = br#"{"$type":"sh.tangled.repo.issue","repoDid":"did:plc:squid","title":"t","createdAt":"2026-05-01T00:00:00Z"}"#;
        let decoded = DecodedRecord::try_decode(&nsid("sh.tangled.repo.issue"), json)
            .expect("legacy issue must decode");
        assert!(matches!(
            decoded,
            DecodedRecord::Legacy(LegacyRecord::Issue(_))
        ));
    }

    #[test]
    fn canon_decode_wins_when_wire_matches_new_shape() {
        let json = br#"{"$type":"sh.tangled.repo.issue","repo":"did:plc:squid","title":"t","createdAt":"2026-05-01T00:00:00Z"}"#;
        let decoded = DecodedRecord::try_decode(&nsid("sh.tangled.repo.issue"), json)
            .expect("canon issue must decode");
        match decoded {
            DecodedRecord::Canon(Record::Issue(i)) => {
                assert_eq!(i.repo.as_ref(), "did:plc:squid")
            }
            other => panic!("expected canon issue, got {other:?}"),
        }
    }

    #[test]
    fn scrub_returns_none_for_unknown_nsid() {
        let json = br#"{"$type":"sh.tangled.repo.issue","preferredHandle":""}"#;
        assert!(scrub_record_bytes(&nsid("sh.tangled.repo.issue"), json).is_none());
    }

    #[test]
    fn scrub_returns_none_when_target_field_is_non_empty() {
        let json =
            br#"{"$type":"sh.tangled.actor.profile","bluesky":true,"preferredHandle":"nel.pet"}"#;
        assert!(scrub_record_bytes(&nsid("sh.tangled.actor.profile"), json).is_none());
    }

    #[test]
    fn scrub_returns_none_when_target_field_is_absent() {
        let json = br#"{"$type":"sh.tangled.actor.profile","bluesky":true}"#;
        assert!(scrub_record_bytes(&nsid("sh.tangled.actor.profile"), json).is_none());
    }

    #[test]
    fn scrub_returns_none_for_non_string_value() {
        let json = br#"{"$type":"sh.tangled.actor.profile","bluesky":true,"preferredHandle":42}"#;
        assert!(scrub_record_bytes(&nsid("sh.tangled.actor.profile"), json).is_none());
    }

    #[test]
    fn scrub_returns_none_for_non_object_json() {
        assert!(scrub_record_bytes(&nsid("sh.tangled.actor.profile"), b"[]").is_none());
        assert!(scrub_record_bytes(&nsid("sh.tangled.actor.profile"), b"null").is_none());
        assert!(scrub_record_bytes(&nsid("sh.tangled.actor.profile"), b"123").is_none());
    }

    #[test]
    fn scrub_returns_none_for_invalid_json() {
        assert!(scrub_record_bytes(&nsid("sh.tangled.actor.profile"), b"{not json").is_none());
    }

    #[test]
    fn scrub_drops_empty_preferred_handle_and_preserves_other_fields() {
        let json = br#"{"$type":"sh.tangled.actor.profile","bluesky":true,"preferredHandle":"","description":"hi"}"#;
        let scrubbed = scrub_record_bytes(&nsid("sh.tangled.actor.profile"), json)
            .expect("empty preferredHandle must trigger scrub");
        let value: serde_json::Value = serde_json::from_slice(&scrubbed).expect("valid json");
        let obj = value.as_object().expect("object");
        assert!(!obj.contains_key("preferredHandle"));
        assert_eq!(obj.get("bluesky"), Some(&serde_json::json!(true)));
        assert_eq!(obj.get("description"), Some(&serde_json::json!("hi")));
        assert_eq!(
            obj.get("$type"),
            Some(&serde_json::json!("sh.tangled.actor.profile"))
        );
    }

    #[test]
    fn scrub_replaces_null_arrays_with_empty_for_label_op() {
        let json = br#"{"$type":"sh.tangled.label.op","add":[{"key":"at://did:plc:limpet/sh.tangled.label.definition/k","value":"v"}],"delete":null,"performedAt":"2026-05-01T00:00:00Z","subject":"at://did:plc:limpet/sh.tangled.repo.issue/3aaa"}"#;
        let scrubbed = scrub_record_bytes(&nsid("sh.tangled.label.op"), json)
            .expect("null delete must trigger scrub");
        let value: serde_json::Value = serde_json::from_slice(&scrubbed).expect("valid json");
        let obj = value.as_object().expect("object");
        assert_eq!(obj.get("delete"), Some(&serde_json::json!([])));
        assert!(obj.get("add").is_some_and(|v| v.is_array()));
    }

    #[test]
    fn scrub_passes_through_when_label_op_arrays_are_non_null() {
        let json = br#"{"$type":"sh.tangled.label.op","add":[],"delete":[],"performedAt":"2026-05-01T00:00:00Z","subject":"at://did:plc:limpet/sh.tangled.repo.issue/3aaa"}"#;
        assert!(scrub_record_bytes(&nsid("sh.tangled.label.op"), json).is_none());
    }

    #[test]
    fn try_decode_recovers_label_op_with_null_delete() {
        let json = br#"{"$type":"sh.tangled.label.op","add":[{"key":"at://did:plc:limpet/sh.tangled.label.definition/k","value":"v"}],"delete":null,"performedAt":"2026-05-01T00:00:00Z","subject":"at://did:plc:limpet/sh.tangled.repo.issue/3aaa"}"#;
        let decoded = DecodedRecord::try_decode(&nsid("sh.tangled.label.op"), json)
            .expect("label.op with null delete must scrub-recover");
        assert!(matches!(decoded, DecodedRecord::Canon(Record::LabelOp(_))));
    }

    #[test]
    fn try_decode_recovers_profile_with_empty_preferred_handle() {
        let json = br#"{"$type":"sh.tangled.actor.profile","bluesky":true,"preferredHandle":"","description":"hi"}"#;
        let decoded = DecodedRecord::try_decode(&nsid("sh.tangled.actor.profile"), json)
            .expect("profile with empty preferredHandle must scrub-recover");
        match decoded {
            DecodedRecord::Canon(Record::Profile(p)) => {
                assert!(p.preferred_handle.is_none());
                assert_eq!(p.description.as_deref(), Some("hi"));
            }
            other => panic!("expected canon profile, got {other:?}"),
        }
    }

    #[test]
    fn legacy_decode_passes_through_for_unaffected_nsids() {
        let json = br#"{"$type":"sh.tangled.graph.follow","subject":"did:plc:bailey","createdAt":"2026-05-01T00:00:00Z"}"#;
        let decoded = DecodedRecord::try_decode(&nsid("sh.tangled.graph.follow"), json)
            .expect("follow has no legacy form, must decode canon");
        assert!(matches!(decoded, DecodedRecord::Canon(Record::Follow(_))));
    }

    #[test]
    fn normalize_returns_none_for_invalid_json() {
        assert!(normalize_record_fields(b"{not json").is_none());
    }

    #[test]
    fn normalize_drops_repeated_dollar_type_key() {
        let json =
            br#"{"$type":"sh.tangled.repo.pull","title":"t","$type":"sh.tangled.repo.pull"}"#;
        let normalized =
            normalize_record_fields(json).expect("duplicate key must produce normalized output");
        let value: serde_json::Value = serde_json::from_slice(&normalized).expect("valid json");
        let obj = value.as_object().expect("object");
        assert_eq!(obj.len(), 2);
        assert_eq!(
            obj.get("$type"),
            Some(&serde_json::json!("sh.tangled.repo.pull"))
        );
        assert_eq!(obj.get("title"), Some(&serde_json::json!("t")));
    }

    #[test]
    fn normalize_keeps_last_value_for_repeated_keys() {
        let json = br#"{"$type":"sh.tangled.repo.issue","$type":"sh.tangled.repo.pull"}"#;
        let normalized =
            normalize_record_fields(json).expect("duplicate key must produce normalized output");
        let value: serde_json::Value = serde_json::from_slice(&normalized).expect("valid json");
        assert_eq!(
            value.get("$type"),
            Some(&serde_json::json!("sh.tangled.repo.pull")),
        );
    }

    #[test]
    fn synthesize_fills_empty_created_at_with_fallback() {
        let json = br#"{"$type":"sh.tangled.repo.issue","title":"meow","createdAt":""}"#;
        let patched = synthesize_created_at(json, "2026-05-01T00:00:00.000000Z")
            .expect("empty createdAt must be filled");
        let value: serde_json::Value = serde_json::from_slice(&patched).expect("valid json");
        assert_eq!(
            value.get("createdAt"),
            Some(&serde_json::json!("2026-05-01T00:00:00.000000Z")),
        );
    }

    #[test]
    fn synthesize_fills_missing_created_at_with_fallback() {
        let json = br#"{"$type":"sh.tangled.repo.issue","title":"meow"}"#;
        let patched = synthesize_created_at(json, "2026-05-01T00:00:00.000000Z")
            .expect("missing createdAt must be filled");
        let value: serde_json::Value = serde_json::from_slice(&patched).expect("valid json");
        assert_eq!(
            value.get("createdAt"),
            Some(&serde_json::json!("2026-05-01T00:00:00.000000Z")),
        );
    }

    #[test]
    fn synthesize_returns_none_when_created_at_already_set() {
        let json = br#"{"$type":"sh.tangled.repo.issue","createdAt":"2026-05-01T00:00:00Z"}"#;
        assert!(synthesize_created_at(json, "2026-04-01T00:00:00Z").is_none());
    }

    #[test]
    fn synthesize_returns_none_for_non_string_created_at() {
        let json = br#"{"$type":"sh.tangled.repo.issue","createdAt":null}"#;
        assert!(synthesize_created_at(json, "2026-04-01T00:00:00Z").is_none());
    }

    #[tokio::test]
    async fn try_decode_recovers_legacy_issue_after_synthesized_created_at() {
        let json = br#"{"$type":"sh.tangled.repo.issue","body":"a bug","createdAt":"","repo":"at://did:plc:scallop/sh.tangled.repo/limpet","repoDid":"did:plc:scallop","title":"a bug"}"#;
        let patched = synthesize_created_at(json, "2025-08-01T12:00:00.000000Z")
            .expect("empty createdAt must be filled");
        let decoded = DecodedRecord::try_decode(&nsid("sh.tangled.repo.issue"), &patched)
            .expect("issue must legacy-decode after createdAt fill");
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let canon = match decoded {
            DecodedRecord::Canon(r) => r,
            DecodedRecord::Legacy(l) => upgrade(l, &resolver).await.expect("upgrade"),
        };
        match canon {
            Record::Issue(i) => {
                assert_eq!(AsRef::<str>::as_ref(&i.title), "a bug");
                assert_eq!(i.repo.as_ref(), "did:plc:scallop");
            }
            other => panic!("expected issue, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn try_decode_recovers_canon_pull_with_duplicate_dollar_type() {
        let json = br#"{"$type":"sh.tangled.repo.pull","createdAt":"2026-05-01T00:00:00Z","title":"meow","target":{"branch":"main","repo":"at://did:plc:scallop/sh.tangled.repo/limpet"},"rounds":[],"$type":"sh.tangled.repo.pull"}"#;
        let decoded = DecodedRecord::try_decode(&nsid("sh.tangled.repo.pull"), json)
            .expect("duplicate $type pull must normalize-recover");
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        resolver
            .observe(
                did("did:plc:scallop"),
                rkey("limpet"),
                Some(did("did:plc:scallop")),
            )
            .await;
        let canon = match decoded {
            DecodedRecord::Canon(r) => r,
            DecodedRecord::Legacy(l) => upgrade(l, &resolver).await.expect("upgrade"),
        };
        match canon {
            Record::Pull(p) => assert_eq!(AsRef::<str>::as_ref(&p.title), "meow"),
            other => panic!("expected pull, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upgrade_issue_uses_repo_did_directly() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let json = br#"{"$type":"sh.tangled.repo.issue","repoDid":"did:plc:scallop","title":"t","createdAt":"2026-05-01T00:00:00Z"}"#;
        let legacy =
            LegacyRecord::from_json_bytes(&nsid("sh.tangled.repo.issue"), json).expect("decode");
        let canon = upgrade(legacy, &resolver).await.expect("upgrade");
        match canon {
            Record::Issue(i) => assert_eq!(i.repo, did("did:plc:scallop")),
            other => panic!("expected canon issue, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upgrade_issue_resolves_repo_uri_via_observed_resolver() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let owner = did("did:plc:nel");
        let key = rkey("abcabcabcabcz");
        resolver
            .observe(owner.clone(), key.clone(), Some(did("did:plc:scallop")))
            .await;
        let json = br#"{"$type":"sh.tangled.repo.issue","repo":"at://did:plc:nel/sh.tangled.repo/abcabcabcabcz","title":"t","createdAt":"2026-05-01T00:00:00Z"}"#;
        let legacy =
            LegacyRecord::from_json_bytes(&nsid("sh.tangled.repo.issue"), json).expect("decode");
        let canon = upgrade(legacy, &resolver).await.expect("upgrade");
        match canon {
            Record::Issue(i) => assert_eq!(i.repo, did("did:plc:scallop")),
            other => panic!("expected canon issue, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upgrade_issue_drops_when_resolver_cannot_map() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let json = br#"{"$type":"sh.tangled.repo.issue","repo":"at://did:plc:nel/sh.tangled.repo/abcabcabcabcz","title":"t","createdAt":"2026-05-01T00:00:00Z"}"#;
        let legacy =
            LegacyRecord::from_json_bytes(&nsid("sh.tangled.repo.issue"), json).expect("decode");
        assert!(
            upgrade(legacy, &resolver).await.is_none(),
            "no resolver entry and no repoDid means the canon Did cannot be constructed",
        );
    }

    #[tokio::test]
    async fn upgrade_pull_propagates_target_resolution() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let json = br#"{"$type":"sh.tangled.repo.pull","title":"t","createdAt":"2026-05-01T00:00:00Z","rounds":[],"target":{"branch":"main","repoDid":"did:plc:scallop"}}"#;
        let legacy =
            LegacyRecord::from_json_bytes(&nsid("sh.tangled.repo.pull"), json).expect("decode");
        let canon = upgrade(legacy, &resolver).await.expect("upgrade");
        match canon {
            Record::Pull(p) => {
                assert_eq!(p.target.repo, did("did:plc:scallop"));
                assert!(p.source.is_none());
            }
            other => panic!("expected canon pull, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upgrade_pull_pre_rounds_synthesizes_round_from_top_level_patch_blob() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let json = br#"{"$type":"sh.tangled.repo.pull","title":"t","createdAt":"2026-05-01T00:00:00Z","target":{"branch":"main","repoDid":"did:plc:scallop"},"patchBlob":{"$type":"blob","mimeType":"application/gzip","ref":{"$link":"bafkreibpatvbeajtwzlr4jwr4s2hnwo5l7sgdbfnqu6n7ctd2bcbtluw4a"},"size":920}}"#;
        let legacy =
            LegacyRecord::from_json_bytes(&nsid("sh.tangled.repo.pull"), json).expect("decode");
        let canon = upgrade(legacy, &resolver).await.expect("upgrade");
        match canon {
            Record::Pull(p) => {
                assert_eq!(
                    p.rounds.len(),
                    1,
                    "pre-rounds wire must yield exactly one synthesized round"
                );
                assert_eq!(
                    p.rounds[0].patch_blob.blob().mime_type.as_ref(),
                    "application/gzip"
                );
                assert_eq!(p.rounds[0].created_at, p.created_at);
            }
            other => panic!("expected canon pull, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upgrade_pull_omits_round_when_neither_rounds_nor_patch_blob_present() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let json = br#"{"$type":"sh.tangled.repo.pull","title":"t","createdAt":"2026-05-01T00:00:00Z","target":{"branch":"main","repoDid":"did:plc:scallop"}}"#;
        let legacy =
            LegacyRecord::from_json_bytes(&nsid("sh.tangled.repo.pull"), json).expect("decode");
        let canon = upgrade(legacy, &resolver).await.expect("upgrade");
        match canon {
            Record::Pull(p) => assert!(p.rounds.is_empty()),
            other => panic!("expected canon pull, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upgrade_public_key_renames_created_to_created_at() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let json = br#"{"$type":"sh.tangled.publicKey","created":"2025-04-15T18:35:38Z","key":"ssh-ed25519 AAAA","name":"laptop"}"#;
        let legacy =
            LegacyRecord::from_json_bytes(&nsid("sh.tangled.publicKey"), json).expect("decode");
        let canon = upgrade(legacy, &resolver).await.expect("upgrade");
        match canon {
            Record::PublicKey(k) => {
                assert_eq!(k.created_at.as_str(), "2025-04-15T18:35:38Z");
                assert_eq!(k.key.as_str(), "ssh-ed25519 AAAA");
                assert_eq!(k.name.as_str(), "laptop");
            }
            other => panic!("expected canon publicKey, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upgrade_repo_renames_added_at_and_drops_owner() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let json = br#"{"$type":"sh.tangled.repo","addedAt":"2025-03-21T10:18:58Z","description":"hi","knot":"knot1.tangled.sh","name":"site","owner":"did:plc:nel"}"#;
        let legacy = LegacyRecord::from_json_bytes(&nsid("sh.tangled.repo"), json).expect("decode");
        let canon = upgrade(legacy, &resolver).await.expect("upgrade");
        match canon {
            Record::Repo(r) => {
                assert_eq!(r.created_at.as_str(), "2025-03-21T10:18:58Z");
                assert_eq!(r.description.as_deref(), Some("hi"));
                assert_eq!(r.knot.as_str(), "knot1.tangled.sh");
                assert_eq!(r.name.as_deref(), Some("site"));
                assert!(r.repo_did.is_none(), "legacy repos have no repo_did");
            }
            other => panic!("expected canon repo, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upgrade_knot_member_renames_added_at_and_member() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let json = br#"{"$type":"sh.tangled.knot.member","addedAt":"2025-03-31T05:14:09Z","domain":"knot.example","member":"did:plc:nel"}"#;
        let legacy =
            LegacyRecord::from_json_bytes(&nsid("sh.tangled.knot.member"), json).expect("decode");
        let canon = upgrade(legacy, &resolver).await.expect("upgrade");
        match canon {
            Record::KnotMember(m) => {
                assert_eq!(m.created_at.as_str(), "2025-03-31T05:14:09Z");
                assert_eq!(m.domain.as_str(), "knot.example");
                assert_eq!(m.subject, did("did:plc:nel"));
            }
            other => panic!("expected canon knot.member, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn legacy_pull_target_with_empty_repo_did_treats_as_none() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        resolver
            .observe(
                did("did:plc:nel"),
                rkey("abcabcabcabcz"),
                Some(did("did:plc:scallop")),
            )
            .await;
        let json = br#"{"$type":"sh.tangled.repo.pull","title":"t","createdAt":"2026-05-01T00:00:00Z","rounds":[],"target":{"branch":"main","repo":"at://did:plc:nel/sh.tangled.repo/abcabcabcabcz","repoDid":""}}"#;
        let legacy =
            LegacyRecord::from_json_bytes(&nsid("sh.tangled.repo.pull"), json).expect("decode");
        let canon = upgrade(legacy, &resolver).await.expect("upgrade");
        match canon {
            Record::Pull(p) => assert_eq!(p.target.repo, did("did:plc:scallop")),
            other => panic!("expected canon pull, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn try_decode_recovers_publickey_with_created_field() {
        let json = br#"{"$type":"sh.tangled.publicKey","created":"2025-04-15T18:35:38Z","key":"k","name":"n"}"#;
        let decoded = DecodedRecord::try_decode(&nsid("sh.tangled.publicKey"), json)
            .expect("legacy publicKey must decode");
        assert!(matches!(
            decoded,
            DecodedRecord::Legacy(LegacyRecord::PublicKey(_))
        ));
    }

    #[tokio::test]
    async fn try_decode_recovers_repo_with_added_at_field() {
        let json = br#"{"$type":"sh.tangled.repo","addedAt":"2025-03-21T10:18:58Z","knot":"knot1.tangled.sh","owner":"did:plc:nel"}"#;
        let decoded = DecodedRecord::try_decode(&nsid("sh.tangled.repo"), json)
            .expect("legacy repo must decode");
        assert!(matches!(
            decoded,
            DecodedRecord::Legacy(LegacyRecord::Repo(_))
        ));
    }

    #[tokio::test]
    async fn upgrade_pull_source_repo_resolution_is_independent_of_target() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let json = br#"{"$type":"sh.tangled.repo.pull","title":"t","createdAt":"2026-05-01T00:00:00Z","rounds":[],"target":{"branch":"main","repoDid":"did:plc:scallop"},"source":{"branch":"feat","repo":"at://did:plc:nel/sh.tangled.repo/missing"}}"#;
        let legacy =
            LegacyRecord::from_json_bytes(&nsid("sh.tangled.repo.pull"), json).expect("decode");
        let canon = upgrade(legacy, &resolver).await.expect("upgrade");
        match canon {
            Record::Pull(p) => {
                assert_eq!(p.target.repo, did("did:plc:scallop"));
                let source = p.source.expect("source struct retained");
                assert_eq!(source.branch.as_str(), "feat");
                assert!(
                    source.repo.is_none(),
                    "unresolvable source repo at-uri leaves the source.repo None rather than dropping the whole pull",
                );
            }
            other => panic!("expected canon pull, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upgrade_ref_update_renames_repo_did_to_repo() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let json = br#"{"$type":"sh.tangled.git.refUpdate","ref":"refs/heads/main","committerDid":"did:plc:olaren","repoDid":"did:plc:scallop","oldSha":"0000000000000000000000000000000000000000","newSha":"1111111111111111111111111111111111111111","meta":{"isDefaultRef":true,"commitCount":{}}}"#;
        let legacy =
            LegacyRecord::from_json_bytes(&nsid("sh.tangled.git.refUpdate"), json).expect("decode");
        let canon = upgrade(legacy, &resolver).await.expect("upgrade");
        match canon {
            Record::RefUpdate(r) => assert_eq!(r.repo, did("did:plc:scallop")),
            other => panic!("expected canon ref update, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upgrade_star_prefers_subject_did_over_subject_uri() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let json = br#"{"$type":"sh.tangled.feed.star","createdAt":"2026-05-01T00:00:00Z","subject":"at://did:plc:nel/sh.tangled.string/k1","subjectDid":"did:plc:scallop"}"#;
        let legacy =
            LegacyRecord::from_json_bytes(&nsid("sh.tangled.feed.star"), json).expect("decode");
        let canon = upgrade(legacy, &resolver).await.expect("upgrade");
        match canon {
            Record::Star(s) => match s.subject {
                StarSubject::Repo(r) => assert_eq!(r.did, did("did:plc:scallop")),
                StarSubject::String(_) => panic!("subjectDid must win"),
            },
            other => panic!("expected canon star, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upgrade_star_falls_back_to_string_when_repo_uri_not_in_cache() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let json = br#"{"$type":"sh.tangled.feed.star","createdAt":"2026-05-01T00:00:00Z","subject":"at://did:plc:nel/sh.tangled.repo/abcabcabcabcz"}"#;
        let legacy =
            LegacyRecord::from_json_bytes(&nsid("sh.tangled.feed.star"), json).expect("decode");
        let canon = upgrade(legacy, &resolver).await.expect("upgrade");
        match canon {
            Record::Star(s) => match s.subject {
                StarSubject::String(s) => assert_eq!(
                    s.uri.as_ref(),
                    "at://did:plc:nel/sh.tangled.repo/abcabcabcabcz",
                    "cache-miss on repo uri preserves the uri under the #string variant for later normalization",
                ),
                StarSubject::Repo(_) => panic!("cold cache must not upgrade to Repo variant"),
            },
            other => panic!("expected canon star, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upgrade_star_uses_cached_repo_did_when_observed() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let owner = did("did:plc:nel");
        let key = rkey("abcabcabcabcz");
        resolver
            .observe(owner.clone(), key.clone(), Some(did("did:plc:scallop")))
            .await;
        let json = br#"{"$type":"sh.tangled.feed.star","createdAt":"2026-05-01T00:00:00Z","subject":"at://did:plc:nel/sh.tangled.repo/abcabcabcabcz"}"#;
        let legacy =
            LegacyRecord::from_json_bytes(&nsid("sh.tangled.feed.star"), json).expect("decode");
        let canon = upgrade(legacy, &resolver).await.expect("upgrade");
        match canon {
            Record::Star(s) => match s.subject {
                StarSubject::Repo(r) => assert_eq!(r.did, did("did:plc:scallop")),
                StarSubject::String(_) => panic!("observed cache must upgrade to Repo variant"),
            },
            other => panic!("expected canon star, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upgrade_collaborator_requires_repo_did() {
        let resolver = RepoIdResolver::detached(RuntimeHasher::default());
        let with_did = br#"{"$type":"sh.tangled.repo.collaborator","createdAt":"2026-05-01T00:00:00Z","subject":"did:plc:lyna","repoDid":"did:plc:scallop"}"#;
        let canon = upgrade(
            LegacyRecord::from_json_bytes(&nsid("sh.tangled.repo.collaborator"), with_did)
                .expect("decode"),
            &resolver,
        )
        .await
        .expect("upgrade");
        match canon {
            Record::Collaborator(c) => assert_eq!(c.repo, did("did:plc:scallop")),
            other => panic!("expected canon collaborator, got {other:?}"),
        }

        let no_resolution = br#"{"$type":"sh.tangled.repo.collaborator","createdAt":"2026-05-01T00:00:00Z","subject":"did:plc:lyna","repo":"at://did:plc:nel/sh.tangled.repo/abcabcabcabcz"}"#;
        let legacy =
            LegacyRecord::from_json_bytes(&nsid("sh.tangled.repo.collaborator"), no_resolution)
                .expect("decode");
        assert!(upgrade(legacy, &resolver).await.is_none());
    }
}
