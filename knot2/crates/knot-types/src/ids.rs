use std::fmt;
use std::str::FromStr;

use jacquard_common::types::crypto::{PublicKey, multikey};
use jacquard_common::types::string::{Did as SpecDid, Handle, Nsid, Rkey};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("invalid {kind}: {value:?}")]
    Invalid { kind: &'static str, value: String },
}

impl ParseError {
    fn invalid(kind: &'static str, value: impl Into<String>) -> Self {
        Self::Invalid {
            kind,
            value: value.into(),
        }
    }
}

#[derive(Clone)]
struct Did(String);

impl Did {
    fn as_str(&self) -> &str {
        &self.0
    }
}

macro_rules! string_id {
    (@traits $name:ident) => {
        impl PartialEq for $name {
            fn eq(&self, other: &Self) -> bool {
                self.as_str() == other.as_str()
            }
        }

        impl Eq for $name {}

        impl PartialOrd for $name {
            fn partial_cmp(&self, other: &Self) -> Option<::std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }

        impl Ord for $name {
            fn cmp(&self, other: &Self) -> ::std::cmp::Ordering {
                self.as_str().cmp(other.as_str())
            }
        }

        impl ::std::hash::Hash for $name {
            fn hash<H: ::std::hash::Hasher>(&self, state: &mut H) {
                self.as_str().hash(state);
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.as_str()).finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // Padding, not `write_str`'ing,
                // or else every {:>20} in a log line does nothing
                f.pad(self.as_str())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl FromStr for $name {
            type Err = ParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let raw = String::deserialize(deserializer)?;
                Self::new(raw).map_err(serde::de::Error::custom)
            }
        }
    };
    ($name:ident, $label:literal, via $parse:path => $inner:ty) => {
        #[derive(Clone)]
        pub struct $name($inner);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ParseError> {
                let value = value.into();
                match $parse(&value) {
                    Some(inner) => Ok(Self(inner)),
                    None => Err(ParseError::invalid($label, value)),
                }
            }

            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }
        }

        string_id!(@traits $name);
    };
    ($name:ident, $label:literal, $parse:path) => {
        #[derive(Clone)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ParseError> {
                let value = value.into();
                $parse(&value).ok_or_else(|| ParseError::invalid($label, value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        string_id!(@traits $name);
    };
}

// Names + emails come from commit objects that someone lovingly wrote,
// so refusing them probably isn't very good;
// they go back out into the author-line of a commit we write,
// in which a newline would create the rest of the header.
crate::text_newtype! {
    pub struct AuthorName(String) => strip_control;
    pub struct Email(String) => strip_control;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LanguageName(&'static str);

impl LanguageName {
    pub const fn new(name: &'static str) -> Self {
        LanguageName(name)
    }

    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for LanguageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.0)
    }
}

impl Serialize for LanguageName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0)
    }
}

fn parse_did(value: &str) -> Option<Did> {
    SpecDid::<String>::new_owned(value)
        .ok()
        .map(|did| canonical_did(did.as_str()))
}

fn canonical_did(did: &str) -> Did {
    match did.strip_prefix("did:web:") {
        Some(msid) => {
            let (authority, path) = match msid.split_once(':') {
                Some((authority, path)) => (authority, Some(path)),
                None => (msid, None),
            };
            let authority = lowercase_preserving_percent(authority);
            Did(match path {
                Some(path) => format!("did:web:{authority}:{path}"),
                None => format!("did:web:{authority}"),
            })
        }
        None => Did(did.to_string()),
    }
}

fn lowercase_preserving_percent(authority: &str) -> String {
    authority
        .chars()
        .scan(0u8, |pending, ch| {
            let emitted: String = if *pending > 0 {
                *pending -= 1;
                ch.to_string()
            } else if ch == '%' {
                *pending = 2;
                ch.to_string()
            } else {
                ch.to_lowercase().collect()
            };
            Some(emitted)
        })
        .collect()
}

fn parse_nsid(value: &str) -> Option<Nsid<String>> {
    Nsid::new_owned(value).ok()
}

fn parse_multikey(value: &str) -> Option<ActorId> {
    PublicKey::decode(value)
        .is_ok()
        .then(|| ActorId(value.to_string()))
}

fn parse_repo_name(value: &str) -> Option<RepoName> {
    is_repo_name(value).then(|| RepoName(value.to_string()))
}

fn parse_rkey(value: &str) -> Option<Rkey<String>> {
    Rkey::new_owned(value).ok()
}

fn parse_ref_name(value: &str) -> Option<RefName> {
    is_ref_name(value).then(|| RefName(value.to_string()))
}

fn is_bare_host(value: &str) -> bool {
    !value.contains([':', '/']) && KnotId::new(format!("did:web:{value}")).is_ok()
}

fn parse_knot_hostname(value: &str) -> Option<KnotHostname> {
    is_bare_host(value).then(|| KnotHostname(value.to_string()))
}

fn parse_logs_host(value: &str) -> Option<LogsHost> {
    is_bare_host(value).then(|| LogsHost(value.to_string()))
}

fn parse_ci_logs_addr(value: &str) -> Option<CiLogsAddr> {
    let url = url::Url::parse(&format!("ssh://{value}")).ok()?;
    (url.path().is_empty()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none())
    .then_some(())?;
    Some(CiLogsAddr {
        host: LogsHost::new(url.host_str()?).ok()?,
        port: LogsPort::new(url.port()?)?,
    })
}

fn parse_branch_name(value: &str) -> Option<BranchName> {
    RefName::new(format!("refs/heads/{value}"))
        .is_ok()
        .then(|| BranchName(value.to_string()))
}

fn parse_tag_name(value: &str) -> Option<TagName> {
    RefName::new(format!("refs/tags/{value}"))
        .is_ok()
        .then(|| TagName(value.to_string()))
}

fn parse_repo_path(value: &str) -> Option<RepoPath> {
    (!value.is_empty()
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".." && !part.contains('\0')))
    .then(|| RepoPath(value.to_string()))
}

fn parse_http_base(value: &str) -> Option<url::Url> {
    let trimmed = value.trim_end_matches('/');
    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))?;
    let url = url::Url::parse(trimmed).ok()?;
    (!rest.starts_with('/')
        && url.has_host()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && !trimmed.chars().any(|c| c.is_whitespace() || c.is_control()))
    .then_some(url)
}

fn http_base_string(url: url::Url) -> String {
    url.as_str().trim_end_matches('/').to_string()
}

fn parse_service_url(value: &str) -> Option<KnotServiceUrl> {
    parse_http_base(value)
        .filter(|url| matches!(url.path(), "" | "/"))
        .map(|url| KnotServiceUrl(http_base_string(url)))
}

fn parse_appview_endpoint(value: &str) -> Option<AppviewEndpoint> {
    parse_http_base(value).map(|url| AppviewEndpoint(http_base_string(url)))
}

fn parse_push_option(value: &str) -> Option<PushOption> {
    (!value.is_empty() && value.len() <= 1024 && !value.contains(['\0', '\n']))
        .then(|| PushOption(value.to_string()))
}

fn is_repo_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains("..")
        && value.chars().all(|c| !c.is_control() && !c.is_whitespace())
}

fn is_ref_name(value: &str) -> bool {
    value.starts_with("refs/")
        && !value.ends_with('.')
        && !value.contains("..")
        && !value.contains("@{")
        && value.split('/').all(|component| {
            !component.is_empty() && !component.starts_with('.') && !component.ends_with(".lock")
        })
        && value.chars().all(|c| {
            !c.is_control()
                && !matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\' | '\u{7f}')
        })
}

string_id!(RepoDid, "repo DID", via parse_did => Did);
string_id!(OwnerDid, "owner DID", via parse_did => Did);
string_id!(KnotId, "knot DID", via parse_did => Did);
string_id!(AccountDid, "account DID", via parse_did => Did);
string_id!(ServiceDid, "service DID", via parse_did => Did);
string_id!(RepoName, "repo name", parse_repo_name);
string_id!(RepoRkey, "repo record key", via parse_rkey => Rkey<String>);
string_id!(RefName, "ref name", parse_ref_name);
string_id!(TypeName, "COB type name", via parse_nsid => Nsid<String>);
string_id!(ActorId, "actor public key", parse_multikey);
string_id!(KnotHostname, "knot hostname", parse_knot_hostname);
string_id!(BranchName, "branch name", parse_branch_name);
string_id!(TagName, "tag name", parse_tag_name);
string_id!(RepoPath, "repository path", parse_repo_path);
string_id!(AppviewEndpoint, "appview endpoint", parse_appview_endpoint);
string_id!(KnotServiceUrl, "knot service URL", parse_service_url);
string_id!(LogsHost, "ci logs host", parse_logs_host);
string_id!(PushOption, "push option", parse_push_option);

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct PushOptions(Vec<PushOption>);

impl PushOptions {
    pub const MAX: usize = 50;

    pub fn new(options: impl IntoIterator<Item = PushOption>) -> Self {
        Self(options.into_iter().take(Self::MAX).collect())
    }

    pub fn as_slice(&self) -> &[PushOption] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LogsPort(std::num::NonZeroU16);

impl LogsPort {
    pub fn new(value: u16) -> Option<Self> {
        std::num::NonZeroU16::new(value).map(Self)
    }

    pub fn get(self) -> u16 {
        self.0.get()
    }
}

impl fmt::Display for LogsPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiLogsAddr {
    host: LogsHost,
    port: LogsPort,
}

impl CiLogsAddr {
    pub fn new(value: &str) -> Result<Self, ParseError> {
        parse_ci_logs_addr(value).ok_or_else(|| ParseError::invalid("ci logs address", value))
    }

    pub fn host(&self) -> &LogsHost {
        &self.host
    }

    pub fn port(&self) -> LogsPort {
        self.port
    }
}

impl fmt::Display for CiLogsAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

#[derive(Clone)]
pub enum OwnerRef {
    Did(OwnerDid),
    Handle(Handle),
}

impl OwnerRef {
    pub fn parse(segment: &str) -> Option<Self> {
        match OwnerDid::new(segment) {
            Ok(did) => Some(Self::Did(did)),
            // Trying a handle only when the segment isn't `did:`-prefixed!
            // Not perfect by any means but avoids a network call if DID
            // is malformed.
            Err(_) if !segment.starts_with("did:") => {
                Handle::new_owned(segment).ok().map(Self::Handle)
            }
            Err(_) => None,
        }
    }
}

impl KnotServiceUrl {
    pub fn authority(&self) -> &str {
        self.0
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&self.0)
    }
}

impl KnotHostname {
    pub fn knot_did(&self) -> KnotId {
        KnotId::new(format!("did:web:{}", self.0)).expect("validated knot hostname forms did:web")
    }
}

impl BranchName {
    pub fn head_ref(&self) -> RefName {
        RefName::new(format!("refs/heads/{}", self.0))
            .expect("validated branch name forms refs/heads ref")
    }
}

impl TagName {
    pub fn tag_ref(&self) -> RefName {
        RefName::new(format!("refs/tags/{}", self.0))
            .expect("validated tag name forms refs/tags ref")
    }
}

impl RefName {
    pub fn branch_name(&self) -> Option<BranchName> {
        self.0
            .strip_prefix("refs/heads/")
            .map(|name| BranchName(name.to_string()))
    }

    pub fn tag_name(&self) -> Option<TagName> {
        self.0
            .strip_prefix("refs/tags/")
            .map(|name| TagName(name.to_string()))
    }
}

impl RepoPath {
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    pub fn names_dot_git(&self) -> bool {
        self.components()
            .any(|part| part.eq_ignore_ascii_case(".git"))
    }

    pub fn parent(&self) -> Option<RepoPath> {
        self.0
            .rsplit_once('/')
            .map(|(dir, _)| RepoPath(dir.to_string()))
    }

    pub fn file_name(&self) -> &str {
        self.0
            .rsplit_once('/')
            .map(|(_, name)| name)
            .unwrap_or(&self.0)
    }
}

impl OwnerDid {
    pub fn is(&self, account: &AccountDid) -> bool {
        self.as_str() == account.as_str()
    }
}

impl From<OwnerDid> for AccountDid {
    fn from(owner: OwnerDid) -> Self {
        AccountDid(owner.0)
    }
}

impl From<AccountDid> for OwnerDid {
    fn from(account: AccountDid) -> Self {
        OwnerDid(account.0)
    }
}

impl From<RepoDid> for AccountDid {
    fn from(repo: RepoDid) -> Self {
        AccountDid(repo.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClonePath {
    rkeys: Vec<RepoRkey>,
    names: Vec<RepoName>,
}

impl ClonePath {
    pub fn parse(raw: &str) -> Option<Self> {
        let segments = || std::iter::once(raw).chain(raw.strip_suffix(".git"));
        let rkeys: Vec<RepoRkey> = segments()
            .filter_map(|segment| RepoRkey::new(segment).ok())
            .collect();
        let names: Vec<RepoName> = segments()
            .filter_map(|segment| RepoName::new(segment).ok())
            .collect();
        (!rkeys.is_empty() || !names.is_empty()).then_some(Self { rkeys, names })
    }

    pub fn rkeys(&self) -> impl Iterator<Item = &RepoRkey> {
        self.rkeys.iter()
    }

    pub fn names(&self) -> impl Iterator<Item = &RepoName> {
        self.names.iter()
    }
}

const SECP256K1_MULTICODEC: u64 = 0xe7;

impl ActorId {
    pub fn from_secp256k1(sec1_bytes: &[u8]) -> Self {
        Self(multikey(SECP256K1_MULTICODEC, sec1_bytes))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OfferedKey(Vec<u8>);

impl OfferedKey {
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid(gix_hash::ObjectId);

impl Oid {
    pub const fn null() -> Self {
        Self(gix_hash::ObjectId::null(gix_hash::Kind::Sha1))
    }

    pub fn is_null(self) -> bool {
        self.0.is_null()
    }

    pub fn from_hex(hex: &str) -> Result<Self, ParseError> {
        gix_hash::ObjectId::from_hex(hex.as_bytes())
            .map(Self)
            .map_err(|_| ParseError::invalid("oid", hex))
    }

    pub fn object_id(self) -> gix_hash::ObjectId {
        self.0
    }

    pub fn to_hex(self) -> String {
        self.0.to_hex().to_string()
    }
}

impl From<gix_hash::ObjectId> for Oid {
    fn from(value: gix_hash::ObjectId) -> Self {
        Self(value)
    }
}

impl From<Oid> for gix_hash::ObjectId {
    fn from(value: Oid) -> Self {
        value.0
    }
}

impl FromStr for Oid {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.to_hex(), f)
    }
}

impl Serialize for Oid {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Oid {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let hex = String::deserialize(deserializer)?;
        Self::from_hex(&hex).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefTransition {
    Create { new: Oid },
    Advance { old: Oid, new: Oid },
    Delete { old: Oid },
}

impl RefTransition {
    pub fn old_oid(self) -> Option<Oid> {
        match self {
            Self::Create { .. } => None,
            Self::Advance { old, .. } | Self::Delete { old } => Some(old),
        }
    }

    pub fn new_oid(self) -> Option<Oid> {
        match self {
            Self::Create { new } | Self::Advance { new, .. } => Some(new),
            Self::Delete { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectFormat(gix_hash::Kind);

impl ObjectFormat {
    pub const SHA1: Self = Self(gix_hash::Kind::Sha1);
    pub const SHA256: Self = Self(gix_hash::Kind::Sha256);

    pub fn from_kind(kind: gix_hash::Kind) -> Self {
        Self(kind)
    }

    pub fn kind(self) -> gix_hash::Kind {
        self.0
    }

    pub fn from_capability(token: &str) -> Option<Self> {
        match token {
            "sha1" => Some(Self::SHA1),
            "sha256" => Some(Self::SHA256),
            _ => None,
        }
    }

    pub fn capability(self) -> &'static str {
        match self.0 {
            gix_hash::Kind::Sha256 => "sha256",
            _ => "sha1",
        }
    }

    pub fn null_oid(self) -> Oid {
        Oid(self.0.null())
    }
}

impl Default for ObjectFormat {
    fn default() -> Self {
        Self::SHA1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixSeconds(i64);

impl UnixSeconds {
    pub const fn new(seconds: i64) -> Self {
        Self(seconds)
    }

    pub const fn get(self) -> i64 {
        self.0
    }

    pub const fn saturating_add_secs(self, secs: i64) -> Self {
        Self(self.0.saturating_add(secs))
    }

    pub const fn saturating_sub_secs(self, secs: i64) -> Self {
        Self(self.0.saturating_sub(secs))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixMicros(u64);

impl UnixMicros {
    pub const fn new(micros: u64) -> Self {
        Self(micros)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for UnixSeconds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectCount(usize);

impl ObjectCount {
    pub const fn new(count: usize) -> Self {
        Self(count)
    }

    pub const fn get(self) -> usize {
        self.0
    }

    pub const fn succ(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl From<u32> for ObjectCount {
    fn from(count: u32) -> Self {
        Self(count as usize)
    }
}

impl fmt::Display for ObjectCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct LanguageBytes(u64);

impl LanguageBytes {
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn saturating_add_bytes(self, bytes: u64) -> Self {
        Self(self.0.saturating_add(bytes))
    }
}

impl fmt::Display for LanguageBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HttpStatus(u16);

impl HttpStatus {
    pub const fn new(code: u16) -> Self {
        Self(code)
    }

    pub const fn get(self) -> u16 {
        self.0
    }

    pub const fn is_success(self) -> bool {
        self.0 >= 200 && self.0 < 300
    }

    pub const fn is_server_error(self) -> bool {
        self.0 >= 500 && self.0 < 600
    }

    pub const fn is_transient(self) -> bool {
        self.0 == 429 || self.is_server_error()
    }
}

impl From<http::StatusCode> for HttpStatus {
    fn from(status: http::StatusCode) -> Self {
        Self(status.as_u16())
    }
}

impl fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CobId(Oid);

impl CobId {
    pub fn new(oid: Oid) -> Self {
        Self(oid)
    }

    pub fn oid(self) -> Oid {
        self.0
    }
}

impl fmt::Display for CobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl Serialize for CobId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CobId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Oid::deserialize(deserializer).map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChangeId(Oid);

impl ChangeId {
    pub fn new(oid: Oid) -> Self {
        Self(oid)
    }

    pub fn oid(self) -> Oid {
        self.0
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl Serialize for ChangeId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ChangeId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Oid::deserialize(deserializer).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dids_validate_and_canonicalize() {
        [
            ("did:plc:nel", Some("did:plc:nel")),
            (
                "did:plc:7iza6de2dwap2sbkpav7c6c6",
                Some("did:plc:7iza6de2dwap2sbkpav7c6c6"),
            ),
            ("did:web:oyster.cafe", Some("did:web:oyster.cafe")),
            ("did:web:OYSTER.cafe", Some("did:web:oyster.cafe")),
            ("did:web:Oyster.Cafe", Some("did:web:oyster.cafe")),
            ("at://did:plc:nel", Some("did:plc:nel")),
            ("not-a-did", None),
            ("", None),
        ]
        .iter()
        .for_each(|&(input, expected)| {
            assert_eq!(
                RepoDid::new(input).ok().as_ref().map(|did| did.as_str()),
                expected,
                "{input:?}"
            );
        });
    }

    #[test]
    fn owner_is_account_compares_by_canonical_did() {
        [
            ("did:web:OYSTER.cafe", "did:web:oyster.cafe", true),
            ("did:plc:nel", "did:plc:nel", true),
            ("did:plc:ABC", "did:plc:abc", false),
        ]
        .iter()
        .for_each(|&(owner, account, matches)| {
            assert_eq!(
                OwnerDid::new(owner)
                    .unwrap()
                    .is(&AccountDid::new(account).unwrap()),
                matches,
                "{owner:?} vs {account:?}"
            );
        });
    }

    #[test]
    fn repo_names_reject_traversal() {
        assert!(RepoName::new("anemone").is_ok());
        assert!(RepoName::new("with-dash_and.dot").is_ok());
        assert!(RepoName::new("../escape").is_err());
        assert!(RepoName::new("a/b").is_err());
        assert!(RepoName::new("..").is_err());
        assert!(RepoName::new("").is_err());
    }

    #[test]
    fn repo_rkeys_follow_record_key_rules() {
        assert!(RepoRkey::new("anemone").is_ok());
        assert!(RepoRkey::new("3lkz2mvgyx22a").is_ok());
        assert!(RepoRkey::new("with-dash_and.dot~tilde:colon").is_ok());
        assert!(RepoRkey::new(".").is_err());
        assert!(RepoRkey::new("..").is_err());
        assert!(RepoRkey::new("a/b").is_err());
        assert!(RepoRkey::new("space bar").is_err());
        assert!(RepoRkey::new("").is_err());
        assert!(RepoRkey::new("x".repeat(513)).is_err());
    }

    #[test]
    fn appview_endpoints_require_an_http_base_url() {
        assert_eq!(
            AppviewEndpoint::new("https://tangled.test/")
                .unwrap()
                .as_str(),
            "https://tangled.test"
        );
        assert_eq!(
            AppviewEndpoint::new("https://tangled.test//")
                .unwrap()
                .as_str(),
            "https://tangled.test"
        );
        assert!(AppviewEndpoint::new("http://appview.oyster.cafe/base").is_ok());
        assert_eq!(
            AppviewEndpoint::new("https://TANGLED.test")
                .unwrap()
                .as_str(),
            "https://tangled.test"
        );
        assert_eq!(
            AppviewEndpoint::new("https://tangled.test:443")
                .unwrap()
                .as_str(),
            "https://tangled.test"
        );
        assert_eq!(
            AppviewEndpoint::new("https://tangled.test:8443/base")
                .unwrap()
                .as_str(),
            "https://tangled.test:8443/base"
        );
        assert!(AppviewEndpoint::new("tangled.test").is_err());
        assert!(AppviewEndpoint::new("https://tangled.test:+8443").is_err());
        assert!(AppviewEndpoint::new("ftp://tangled.test").is_err());
        assert!(AppviewEndpoint::new("https://").is_err());
        assert!(AppviewEndpoint::new("https:///pulls").is_err());
        assert!(AppviewEndpoint::new("https://tangled.test/a b").is_err());
        assert!(AppviewEndpoint::new("https://nel@tangled.test").is_err());
        assert!(AppviewEndpoint::new("https://nel:hunter2@tangled.test").is_err());
        assert!(AppviewEndpoint::new("https://tangled.test?utm=knot").is_err());
        assert!(AppviewEndpoint::new("https://tangled.test#pulls").is_err());
        assert!(AppviewEndpoint::new("https://tangled.test#").is_err());
    }

    #[test]
    fn clone_paths_try_the_exact_segment_before_the_stripped_one() {
        let suffixed = ClonePath::parse("anemone.git").unwrap();
        assert_eq!(
            suffixed.rkeys().cloned().collect::<Vec<_>>(),
            vec![
                RepoRkey::new("anemone.git").unwrap(),
                RepoRkey::new("anemone").unwrap()
            ],
            "literal .git rkey wins over conventional suffix interpretation"
        );
        assert_eq!(
            suffixed.names().cloned().collect::<Vec<_>>(),
            vec![
                RepoName::new("anemone.git").unwrap(),
                RepoName::new("anemone").unwrap()
            ]
        );

        let plain = ClonePath::parse("anemone").unwrap();
        assert_eq!(
            plain.rkeys().cloned().collect::<Vec<_>>(),
            vec![RepoRkey::new("anemone").unwrap()]
        );

        let bare = ClonePath::parse(".git").unwrap();
        assert_eq!(
            bare.rkeys().cloned().collect::<Vec<_>>(),
            vec![RepoRkey::new(".git").unwrap()],
            "stripping .git from bare suffix leaves nothing valid to try"
        );

        assert!(ClonePath::parse("a/b.git").is_none());
    }

    #[test]
    fn clone_paths_keep_segments_only_valid_as_one_of_the_two_kinds() {
        let plus = ClonePath::parse("c++").unwrap();
        assert_eq!(
            plus.rkeys().count(),
            0,
            "a record key allows only [A-Za-z0-9._~:-]"
        );
        assert_eq!(
            plus.names().cloned().collect::<Vec<_>>(),
            vec![RepoName::new("c++").unwrap()],
            "a repo name accepts the wider charset, so the segment resolves by name"
        );

        let long = "x".repeat(200);
        let overlong = ClonePath::parse(&long).unwrap();
        assert_eq!(overlong.names().count(), 0, "a repo name is at most 100");
        assert_eq!(
            overlong.rkeys().cloned().collect::<Vec<_>>(),
            vec![RepoRkey::new(&long).unwrap()]
        );
    }

    #[test]
    fn repo_paths_reject_traversal_and_expose_structure() {
        assert!(RepoPath::new("src/lib.rs").is_ok());
        assert!(RepoPath::new("a b/c.txt").is_ok());
        assert!(RepoPath::new("back\\slash.txt").is_ok());
        assert!(RepoPath::new(".git/config").is_ok());
        assert!(RepoPath::new("../escape").is_err());
        assert!(RepoPath::new("nested/../escape").is_err());
        assert!(RepoPath::new("nested/./here").is_err());
        assert!(RepoPath::new("/absolute").is_err());
        assert!(RepoPath::new("trailing/").is_err());
        assert!(RepoPath::new("double//slash").is_err());
        assert!(RepoPath::new("").is_err());
        assert!(RepoPath::new("nul\0byte").is_err());

        let path = RepoPath::new("src/deep/lib.rs").unwrap();
        assert_eq!(path.parent().unwrap().as_str(), "src/deep");
        assert_eq!(path.file_name(), "lib.rs");
        assert!(path.parent().unwrap().parent().unwrap().parent().is_none());
        assert_eq!(
            path.components().collect::<Vec<_>>(),
            vec!["src", "deep", "lib.rs"]
        );
        assert!(RepoPath::new(".git/hooks").unwrap().names_dot_git());
        assert!(RepoPath::new("dir/.GIT/x").unwrap().names_dot_git());
        assert!(!RepoPath::new("gitless").unwrap().names_dot_git());
    }

    #[test]
    fn ref_transitions_expose_their_edge_oids() {
        let before = Oid::from_hex(&"1".repeat(40)).unwrap();
        let after = Oid::from_hex(&"2".repeat(40)).unwrap();
        let create = RefTransition::Create { new: after };
        let advance = RefTransition::Advance {
            old: before,
            new: after,
        };
        let delete = RefTransition::Delete { old: before };
        assert_eq!(create.old_oid(), None);
        assert_eq!(create.new_oid(), Some(after));
        assert_eq!(advance.old_oid(), Some(before));
        assert_eq!(advance.new_oid(), Some(after));
        assert_eq!(delete.old_oid(), Some(before));
        assert_eq!(delete.new_oid(), None);
    }

    #[test]
    fn ref_names_follow_git_rules() {
        assert!(RefName::new("refs/heads/main").is_ok());
        assert!(RefName::new("refs/heads/feature/x").is_ok());
        assert!(RefName::new("refs/cobs/sh.tangled.repo.collaborator/limpet").is_ok());
        assert!(RefName::new("refs/heads/bad..name").is_err());
        assert!(RefName::new("refs/heads/space bar").is_err());
        assert!(RefName::new("trailing.lock").is_err());
        assert!(RefName::new("/leading").is_err());
        assert!(RefName::new("refs/heads/.hidden").is_err());
        assert!(RefName::new("refs/x.lock/y").is_err());
        assert!(RefName::new("refs/heads/ends.").is_err());
        assert!(RefName::new("refs/heads//double").is_err());
        assert!(RefName::new("refs/heads/trailing/").is_err());
        assert!(RefName::new("HEAD").is_err());
        assert!(RefName::new("CONFIG").is_err());
        assert!(RefName::new("config").is_err());
        assert!(RefName::new("FETCH_HEAD").is_err());
        assert!(RefName::new("main").is_err());
        assert!(RefName::new("refs").is_err());
        assert!(RefName::new("").is_err());
    }

    #[test]
    fn oid_roundtrips_through_hex() {
        let hex = "0123456789abcdef0123456789abcdef01234567";
        let oid = Oid::from_hex(hex).expect("valid sha1 hex");
        assert_eq!(oid.to_hex(), hex);
        assert_eq!(oid, Oid::from(oid.object_id()));
        assert!(Oid::from_hex("zz").is_err());
        assert!(Oid::null().is_null());
        assert!(!oid.is_null());
    }

    #[test]
    fn object_format_round_trips_its_capability_token() {
        assert_eq!(ObjectFormat::SHA1.capability(), "sha1");
        assert_eq!(ObjectFormat::SHA256.capability(), "sha256");
        assert_eq!(
            ObjectFormat::from_capability("sha1"),
            Some(ObjectFormat::SHA1)
        );
        assert_eq!(
            ObjectFormat::from_capability("sha256"),
            Some(ObjectFormat::SHA256)
        );
        assert_eq!(ObjectFormat::from_capability("md5"), None);
        assert_eq!(ObjectFormat::default(), ObjectFormat::SHA1);
    }

    #[test]
    fn object_format_null_oid_matches_the_hash_width() {
        let sha1 = ObjectFormat::SHA1.null_oid();
        let sha256 = ObjectFormat::SHA256.null_oid();
        assert_eq!(sha1.to_hex().len(), 40);
        assert_eq!(sha256.to_hex().len(), 64);
        assert!(sha1.is_null());
        assert!(sha256.is_null());
        assert_eq!(sha1.object_id().kind(), gix_hash::Kind::Sha1);
        assert_eq!(sha256.object_id().kind(), gix_hash::Kind::Sha256);
    }

    #[test]
    fn type_name_is_an_nsid() {
        assert!(TypeName::new("sh.tangled.repo.collaborator").is_ok());
        assert!(TypeName::new("not an nsid").is_err());
    }

    #[test]
    fn knot_service_url_validates_and_exposes_its_authority() {
        let url = KnotServiceUrl::new("https://knot.nel.pet/").unwrap();
        assert_eq!(url.as_str(), "https://knot.nel.pet");
        assert_eq!(url.authority(), "knot.nel.pet");
        assert_eq!(
            KnotServiceUrl::new("https://knot.nel.pet:8443")
                .unwrap()
                .authority(),
            "knot.nel.pet:8443"
        );
        assert_eq!(
            KnotServiceUrl::new("https://knot.nel.pet:443")
                .unwrap()
                .authority(),
            "knot.nel.pet"
        );
        assert!(KnotServiceUrl::new("knot.nel.pet").is_err());
        assert!(KnotServiceUrl::new("ftp://knot.nel.pet").is_err());
        assert!(KnotServiceUrl::new("https://knot.nel.pet/base").is_err());
        assert!(KnotServiceUrl::new("https://nel@knot.nel.pet").is_err());
        assert!(KnotServiceUrl::new("https://nel:hunter2@knot.nel.pet").is_err());
        assert!(KnotServiceUrl::new("https://knot.nel.pet?utm=knot").is_err());
        assert!(KnotServiceUrl::new("https://knot.nel.pet#pulls").is_err());
        assert!(KnotServiceUrl::new("https://").is_err());
        assert!(KnotServiceUrl::new("").is_err());
    }

    #[test]
    fn knot_hostname_validates_and_forms_did_web() {
        let host = KnotHostname::new("oyster.cafe").unwrap();
        assert_eq!(host.knot_did(), KnotId::new("did:web:oyster.cafe").unwrap());
        assert!(KnotHostname::new("").is_err());
        assert!(KnotHostname::new("not a host").is_err());
        assert!(KnotHostname::new("knot.nel.pet:8443").is_err());
        assert!(KnotHostname::new("knot.nel.pet/path").is_err());
    }

    #[test]
    fn a_ci_logs_address_splits_into_a_bare_host_and_a_nonzero_port() {
        let addr = CiLogsAddr::new("logs.oyster.cafe:3333").unwrap();
        assert_eq!(addr.host().as_str(), "logs.oyster.cafe");
        assert_eq!(addr.port().get(), 3333);
        assert_eq!(addr.to_string(), "logs.oyster.cafe:3333");

        [
            "logs.oyster.cafe",
            "logs.oyster.cafe:0",
            ":3333",
            "::1",
            "logs.oyster.cafe:+3333",
            "logs.oyster.cafe:65536",
            "logs.oyster.cafe: 3333",
            "logs.oyster.cafe:33:33",
            "logs.oyster.cafe:3333/logs",
            "nel@logs.oyster.cafe:3333",
            "logs.oyster.cafe:3333?tail=1",
            "logs.oyster.cafe:3333#tail",
        ]
        .into_iter()
        .for_each(|value| {
            assert!(CiLogsAddr::new(value).is_err(), "{value}");
        });
    }

    #[test]
    fn push_options_enforce_the_lexicon_bounds_at_construction() {
        assert_eq!(
            PushOption::new("verbose-ci").unwrap().as_str(),
            "verbose-ci"
        );
        assert!(PushOption::new("x".repeat(1024)).is_ok());
        assert!(PushOption::new("x".repeat(1025)).is_err());
        assert!(PushOption::new("").is_err());
        assert!(PushOption::new("has\0nul").is_err());

        let options = PushOptions::new(
            (0..PushOptions::MAX + 10)
                .map(|index| PushOption::new(format!("option-{index}")).unwrap()),
        );
        assert_eq!(options.as_slice().len(), PushOptions::MAX);
        assert_eq!(options.as_slice()[0].as_str(), "option-0");
        assert!(PushOptions::default().is_empty());
    }

    #[test]
    fn branch_name_validates_and_forms_head_ref() {
        let branch = BranchName::new("main").unwrap();
        assert_eq!(branch.head_ref(), RefName::new("refs/heads/main").unwrap());
        assert!(BranchName::new("feature/x").is_ok());
        assert!(BranchName::new("bad..name").is_err());
        assert!(BranchName::new("").is_err());
    }

    #[test]
    fn author_text_strips_nul_and_newline_at_construction() {
        assert_eq!(AuthorName::new("nel\nbailey").as_str(), "nelbailey");
        assert_eq!(Email::new("nel@oyster.cafe\0").as_str(), "nel@oyster.cafe");
        assert_eq!(AuthorName::new("teq").as_str(), "teq");
    }

    #[test]
    fn author_text_strips_nul_and_newline_on_deserialize() {
        let author: AuthorName = serde_json::from_str("\"nel\\nbailey\"").unwrap();
        assert_eq!(author.as_str(), "nelbailey");
        let email: Email = serde_json::from_str("\"nel@oyster.cafe\\u0000\"").unwrap();
        assert_eq!(email.as_str(), "nel@oyster.cafe");
    }

    #[test]
    fn tag_name_validates_and_forms_tag_ref() {
        let tag = TagName::new("v1.2.3").unwrap();
        assert_eq!(tag.tag_ref(), RefName::new("refs/tags/v1.2.3").unwrap());
        assert!(TagName::new("release/v1").is_ok());
        assert!(TagName::new("bad..name").is_err());
        assert!(TagName::new("v1.lock").is_err());
        assert!(TagName::new("").is_err());
    }

    #[test]
    fn unix_seconds_arithmetic_saturates_at_the_bounds() {
        let base = UnixSeconds::new(1_000);
        assert_eq!(base.saturating_add_secs(60), UnixSeconds::new(1_060));
        assert_eq!(base.saturating_sub_secs(60), UnixSeconds::new(940));
        assert_eq!(
            UnixSeconds::new(i64::MAX).saturating_add_secs(1),
            UnixSeconds::new(i64::MAX)
        );
        assert_eq!(
            UnixSeconds::new(i64::MIN).saturating_sub_secs(1),
            UnixSeconds::new(i64::MIN)
        );
        assert_eq!(base.get(), 1_000);
        assert_eq!(base.to_string(), "1000");
    }

    #[test]
    fn http_status_classifies_transient_codes() {
        assert!(HttpStatus::new(200).is_success());
        assert!(!HttpStatus::new(200).is_transient());
        assert!(HttpStatus::new(429).is_transient());
        assert!(HttpStatus::new(503).is_transient());
        assert!(HttpStatus::new(503).is_server_error());
        assert!(!HttpStatus::new(404).is_transient());
        assert_eq!(HttpStatus::new(404).get(), 404);
    }

    #[test]
    fn actor_id_wraps_a_secp256k1_multikey() {
        let mut compressed = [0u8; 33];
        compressed[0] = 0x02;
        let actor = ActorId::from_secp256k1(&compressed);
        let decoded =
            PublicKey::decode(actor.as_str()).expect("from_secp256k1 yields decodable multikey");
        assert_eq!(
            decoded.codec,
            jacquard_common::types::crypto::KeyCodec::Secp256k1
        );
        assert_eq!(decoded.bytes.as_ref(), &compressed);
        assert!(ActorId::new("not-a-multikey").is_err());
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    const DID: &str = "did:(plc:[a-z2-7]{24}|web:[a-z][a-z0-9-]{0,20}\\.(cafe|pet|dev))";

    macro_rules! parse_display_identity {
        ($name:ident, $ty:ty, $strategy:expr) => {
            proptest! {
                #[test]
                fn $name(raw in $strategy) {
                    if let Ok(value) = <$ty>::new(raw.clone()) {
                        prop_assert_eq!(value.as_str(), raw.as_str());
                        let reparsed = <$ty>::new(value.to_string())
                            .expect("display output reparses");
                        prop_assert_eq!(reparsed, value);
                    }
                }
            }
        };
    }

    parse_display_identity!(repo_did_identity, RepoDid, DID);
    parse_display_identity!(owner_did_identity, OwnerDid, DID);
    parse_display_identity!(knot_id_identity, KnotId, DID);
    parse_display_identity!(account_did_identity, AccountDid, DID);
    parse_display_identity!(
        repo_name_identity,
        RepoName,
        "[A-Za-z0-9][A-Za-z0-9._-]{0,40}"
    );
    parse_display_identity!(
        repo_rkey_identity,
        RepoRkey,
        "[A-Za-z0-9][A-Za-z0-9._:~-]{0,40}"
    );
    parse_display_identity!(
        ref_name_identity,
        RefName,
        "refs/(heads|tags|cobs)/[a-z][a-z0-9]{0,7}(/[a-z][a-z0-9]{0,7}){0,3}"
    );
    parse_display_identity!(
        repo_path_identity,
        RepoPath,
        "[a-z][a-z0-9 ._-]{0,7}(/[a-z][a-z0-9 ._-]{0,7}){0,3}"
    );
    parse_display_identity!(
        type_name_identity,
        TypeName,
        "[a-z][a-z0-9]{0,7}(\\.[a-z][a-z0-9]{0,7}){2,4}"
    );

    proptest! {
        #[test]
        fn oid_hex_identity(hex in "[0-9a-f]{40}") {
            let oid = Oid::from_hex(&hex).expect("forty lowercase hex chars are valid sha1");
            prop_assert_eq!(oid.to_hex(), hex.clone());
            prop_assert_eq!(Oid::from_hex(&oid.to_hex()).expect("reparse"), oid);
        }

        #[test]
        fn actor_id_identity(tag in 2u8..=3u8, body in prop::collection::vec(any::<u8>(), 32..=32)) {
            let sec1: Vec<u8> = std::iter::once(tag).chain(body).collect();
            let actor = ActorId::from_secp256k1(&sec1);
            let encoded = actor.as_str().to_string();
            let reparsed = ActorId::new(encoded.clone()).expect("multikey output reparses");
            prop_assert_eq!(reparsed.as_str(), encoded.as_str());
            prop_assert_eq!(reparsed, actor);
        }
    }
}
