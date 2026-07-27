mod template;

use confique::Config;

pub use template::{Key, Line, Lines, NoKeys, Segment, Shape, Template, TemplateError};

// Each msg field defines the enum of placeholders it accepts,
// so a typo'ed `{handel}` in a given config gets caught at startup,
// rather than printed at some poor pusher mid-push.
macro_rules! keys {
    ( $( $name:ident { $( $variant:ident = $placeholder:literal ),+ $(,)? } )+ ) => {
        $(
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            pub enum $name {
                $( $variant, )+
            }

            impl Key for $name {
                const PLACEHOLDERS: &'static [(&'static str, Self)] =
                    &[ $( ($placeholder, Self::$variant), )+ ];
            }
        )+
    };
}

macro_rules! config_type {
    (Lines) => { Vec<String> };
    (Line) => { String };
}

macro_rules! parse_field {
    (Lines, $field:expr, $value:expr) => {
        Template::parse_lines($field, $value)
    };
    (Line, $field:expr, $value:expr) => {
        Template::parse($field, $value)
    };
}

macro_rules! message_group {
    (
        $config:ident => $catalog:ident @ $prefix:literal {
            $( $field:ident : $shape:ident<$keys:ty> = $default:tt ),+ $(,)?
        }
    ) => {
        #[derive(Debug, ::confique::Config)]
        pub struct $config {
            $(
                #[config(default = $default)]
                pub $field: config_type!($shape),
            )+
        }

        #[derive(Debug)]
        pub struct $catalog {
            $( pub $field: Template<$keys, $shape>, )+
        }

        impl $catalog {
            pub fn parse(config: &$config) -> Result<Self, TemplateError> {
                Ok(Self {
                    $(
                        $field: parse_field!(
                            $shape,
                            concat!($prefix, ".", stringify!($field)),
                            &config.$field
                        )?,
                    )+
                })
            }
        }
    };
}

keys! {
    KnotKey { Knot = "knot" }
    PushAckKey { Knot = "knot", Refs = "refs" }
    UrlKey { Url = "url" }
    CiLogsKey { Host = "host", Port = "port", Repo = "repo", Sha = "sha" }
    GreetingKey { User = "user", Knot = "knot" }
    CountKey { Count = "count" }
    RefKey { Ref = "ref" }
    ErrorKey { Error = "error" }
    CommandKey { Command = "command" }
    VersionKey { Version = "version" }
    AlgorithmKey { Algorithm = "algorithm" }
    ValueKey { Value = "value" }
    OidKey { Oid = "oid" }
    DetailKey { Detail = "detail" }
    DeclaredComputedKey { Declared = "declared", Computed = "computed" }
    DeclaredReceivedKey { Declared = "declared", Received = "received" }
    DeclaredLimitKey { Declared = "declared", Limit = "limit" }
    FreeFloorKey { Free = "free", Floor = "floor" }
    WhatLimitKey { What = "what", Limit = "limit" }
}

message_group! {
    PushConfig => PushMessages @ "messages.push" {
        ack: Lines<PushAckKey> = ["{knot} received {refs}."],
        pull_request: Lines<UrlKey> = [
            "",
            "-> Open stinky pull request for this branch:",
            " {url}",
            ""
        ],
        pipeline_clean: Lines<NoKeys> = ["pipeline compiled with no diagnostics"],
        pipeline_none: Lines<NoKeys> = ["no pipelines to compile"],
        ci_logs: Lines<CiLogsKey> = [
            "-> Browse CI logs in your terminal:",
            "   ssh -t -p {port} {host} {repo} {sha}"
        ],
    }
}

message_group! {
    FetchConfig => FetchMessages @ "messages.fetch" {
        motd: Lines<KnotKey> = ["Thanks for using {knot}!"],
        enumerating: Lines<CountKey> = ["Enumerating objects: {count}, done."],
        total: Lines<CountKey> = ["Total {count}, done."],
        fatal: Line<ErrorKey> = "knot: {error}",
    }
}

message_group! {
    RejectConfig => RejectMessages @ "messages.reject" {
        reserved_refs: Line<NoKeys> = "refs/cobs/* and refs/hidden/* are reserved and cannot be pushed",
        cob_create_only: Line<NoKeys> = "existing refs/cobs/* object cannot be modified or deleted over the wire",
        cob_delete: Line<NoKeys> = "refs/cobs/* stores append-only collaborative objects and cannot be deleted",
        hidden_reserved: Line<NoKeys> = "refs/hidden/* is reserved for server-side fork staging and cannot be pushed",
        cob_verification: Line<ErrorKey> = "collaborative-object verification failed: {error}",
        ref_exists: Line<NoKeys> = "reference already exists",
        stale_old_value: Line<NoKeys> = "stale info: old value doesn't match",
        missing_objects: Line<NoKeys> = "missing necessary objects",
        missing_objects_for: Line<RefKey> = "missing necessary objects for {ref}",
        atomic_failed: Line<NoKeys> = "atomic transaction failed",
        atomic_aborted: Line<NoKeys> = "atomic push aborted",
        authorization_unavailable: Line<NoKeys> = "authorization unavailable",
        unpacker_error: Line<NoKeys> = "unpacker error",
        ref_snapshot_unavailable: Line<NoKeys> = "ref snapshot unavailable",
        object_migration_failed: Line<NoKeys> = "object migration failed",
    }
}

message_group! {
    SshConfig => SshMessages @ "messages.ssh" {
        greeting: Lines<GreetingKey> = [
            "Hi {user}! You're authenticated to {knot} knot.",
            "This knot serves git over ssh, so there's no shell here. :P",
            "Clone repo with: git clone {knot}:<repoDID>"
        ],
        unsupported_command: Line<NoKeys> = "knot: unsupported command",
        too_many_operations: Line<NoKeys> = "knot: too many concurrent operations from your address, try again shortly",
        repo_not_found: Line<NoKeys> = "knot: repository not found",
        index_warming: Line<NoKeys> = "knot: repository index is warming, retry shortly",
        lfs_disabled: Line<NoKeys> = "knot: LFS isn't enabled on this knot",
        key_not_registered: Line<NoKeys> = "knot: your ssh key isn't registered to a user authorized to push here. If you offer several keys, make sure the registered one is offered first.",
        push_denied: Line<NoKeys> = "knot: you aren't authorized to push to this repository.",
        shutting_down: Line<NoKeys> = "knot: server is shutting down",
        archive_malformed: Line<NoKeys> = "knot: malformed upload-archive request",
        archive_timeout: Line<NoKeys> = "knot: upload-archive request timed out",
        archive_failed: Line<NoKeys> = "knot: upload-archive failed",
        advertise_failed: Line<NoKeys> = "knot: cannot advertise refs",
        push_too_large: Line<NoKeys> = "knot: push exceeds configured size limit",
        receive_deadline: Line<NoKeys> = "knot: receive exceeded its time budget",
        malformed_pack: Line<NoKeys> = "knot: malformed pack stream",
        receive_read_error: Line<NoKeys> = "knot: receive read error",
        receive_ended_early: Line<NoKeys> = "knot: receive stream ended early",
        receive_failed: Line<NoKeys> = "knot: receive-pack failed",
    }
}

message_group! {
    HttpConfig => HttpMessages @ "messages.http" {
        push_denied: Line<NoKeys> = "you aren't authorized to push to this repository",
        repo_not_found: Line<NoKeys> = "repository not found",
        push_too_large: Line<NoKeys> = "push exceeds the configured size limit",
        malformed_pack: Line<ErrorKey> = "malformed pack stream: {error}",
        receive_ended_early: Line<NoKeys> = "receive stream ended early",
    }
}

message_group! {
    LfsConfig => LfsMessages @ "messages.lfs" {
        invalid_oid: Line<ValueKey> = "invalid LFS oid {value}",
        hash_mismatch: Line<DeclaredComputedKey> = "oid mismatch, declared {declared}, computed {computed}",
        size_mismatch: Line<DeclaredReceivedKey> = "size mismatch, declared {declared}, received {received}",
        size_limit_exceeded: Line<DeclaredLimitKey> = "object size {declared} exceeds limit {limit}",
        free_space_denied: Line<FreeFloorKey> = "free space {free} below floor {floor}",
        not_found: Line<OidKey> = "object {oid} not found",
        framing: Line<DetailKey> = "protocol framing fault: {detail}",
        too_many: Line<WhatLimitKey> = "too many {what} in one message, limit {limit}",
        unknown_command: Line<CommandKey> = "unknown command {command}",
        unsupported_version: Line<VersionKey> = "unsupported version {version}",
        unsupported_hash: Line<AlgorithmKey> = "unsupported hash algorithm {algorithm}",
        put_on_download: Line<NoKeys> = "put-object isn't allowed on a download channel",
        verify_on_download: Line<NoKeys> = "verify-object isn't allowed on a download channel",
        get_on_upload: Line<NoKeys> = "get-object isn't allowed on an upload channel",
        put_no_body: Line<NoKeys> = "put-object is missing its object body",
    }
}

#[derive(Debug, Config)]
pub struct MessagesConfig {
    #[config(nested)]
    pub push: PushConfig,
    #[config(nested)]
    pub fetch: FetchConfig,
    #[config(nested)]
    pub reject: RejectConfig,
    #[config(nested)]
    pub ssh: SshConfig,
    #[config(nested)]
    pub http: HttpConfig,
    #[config(nested)]
    pub lfs: LfsConfig,
}

impl MessagesConfig {
    pub fn defaults() -> Self {
        Self::builder()
            .load()
            .expect("message defaults satisfy every field")
    }
}

#[derive(Debug)]
pub struct Catalog {
    pub push: PushMessages,
    pub fetch: FetchMessages,
    pub reject: RejectMessages,
    pub ssh: SshMessages,
    pub http: HttpMessages,
    pub lfs: LfsMessages,
}

impl Catalog {
    pub fn parse(config: &MessagesConfig) -> Result<Self, TemplateError> {
        Ok(Self {
            push: PushMessages::parse(&config.push)?,
            fetch: FetchMessages::parse(&config.fetch)?,
            reject: RejectMessages::parse(&config.reject)?,
            ssh: SshMessages::parse(&config.ssh)?,
            http: HttpMessages::parse(&config.http)?,
            lfs: LfsMessages::parse(&config.lfs)?,
        })
    }

    pub fn defaults() -> Self {
        Self::parse(&MessagesConfig::defaults()).expect("built-in message templates parse")
    }
}

pub fn default_catalog() -> &'static Catalog {
    static DEFAULTS: std::sync::LazyLock<Catalog> = std::sync::LazyLock::new(Catalog::defaults);
    &DEFAULTS
}

pub fn count_refs(applied: usize) -> String {
    match applied {
        1 => "1 ref".to_string(),
        n => format!("{n} refs"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_parse_into_a_full_catalog() {
        let catalog = Catalog::defaults();
        assert_eq!(catalog.reject.ref_exists.text(), "reference already exists");
        assert_eq!(
            catalog.ssh.repo_not_found.text(),
            "knot: repository not found"
        );
    }

    #[test]
    fn the_pull_request_block_matches_the_shipped_shape() {
        let catalog = Catalog::defaults();
        let url = "https://oyster.cafe/nel.pet/anemone/pulls/new";
        let block = catalog
            .push
            .pull_request
            .lines(|UrlKey::Url| url.to_string());
        assert_eq!(
            block,
            vec![
                "\u{200b}".to_string(),
                "-> Open stinky pull request for this branch:".to_string(),
                format!(" {url}"),
                "\u{200b}".to_string(),
            ]
        );
    }

    #[test]
    fn the_greeting_names_the_user_and_the_knot() {
        let catalog = Catalog::defaults();
        let lines = catalog.ssh.greeting.lines(|key| match key {
            GreetingKey::User => "@nel.pet".to_string(),
            GreetingKey::Knot => "oyster.cafe".to_string(),
        });
        assert!(lines[0].contains("@nel.pet"));
        assert!(lines.iter().any(|line| line.contains("oyster.cafe")));
    }

    #[test]
    fn an_empty_lines_template_mutes_the_message() {
        let template: Template<NoKeys, Lines> =
            Template::parse_lines("messages.test", &[]).unwrap();
        assert!(template.text_lines().is_empty());
    }

    #[test]
    fn an_unknown_placeholder_is_a_parse_error() {
        let error = Template::<KnotKey, Lines>::parse_lines(
            "messages.fetch.motd",
            &["hi {handle}".to_string()],
        )
        .unwrap_err();
        assert_eq!(
            error,
            TemplateError::UnknownPlaceholder {
                field: "messages.fetch.motd",
                name: "handle".to_string(),
            }
        );
    }

    #[test]
    fn doubled_braces_render_as_literal_braces() {
        let template: Template<NoKeys, Line> =
            Template::parse("messages.test", "a {{literal}} brace").unwrap();
        assert_eq!(template.text(), "a {literal} brace");
    }

    #[test]
    fn line_templates_reject_empty_and_multiline_text() {
        assert_eq!(
            Template::<NoKeys, Line>::parse("messages.test", "").unwrap_err(),
            TemplateError::Empty {
                field: "messages.test"
            }
        );
        assert_eq!(
            Template::<NoKeys, Line>::parse("messages.test", "a\nb").unwrap_err(),
            TemplateError::Multiline {
                field: "messages.test"
            }
        );
    }

    #[test]
    fn unbalanced_braces_are_parse_errors() {
        assert_eq!(
            Template::<NoKeys, Line>::parse("messages.test", "open {").unwrap_err(),
            TemplateError::UnclosedBrace {
                field: "messages.test"
            }
        );
        assert_eq!(
            Template::<NoKeys, Line>::parse("messages.test", "close }").unwrap_err(),
            TemplateError::StrayBrace {
                field: "messages.test"
            }
        );
    }

    #[test]
    fn ref_counts_pluralize() {
        assert_eq!(count_refs(1), "1 ref");
        assert_eq!(count_refs(3), "3 refs");
    }
}
