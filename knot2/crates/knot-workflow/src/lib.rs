use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use knot_types::{ChangedFiles, Listing, ParseError, RefName};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowName(String);

impl WorkflowName {
    pub fn new(value: impl Into<String>) -> Result<Self, ParseError> {
        let value = value.into();
        let valid =
            !value.is_empty() && !value.contains('/') && !value.chars().any(char::is_control);
        match valid {
            true => Ok(Self(value)),
            false => Err(ParseError::Invalid {
                kind: "workflow name",
                value,
            }),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn engine_is_named(engine: &str) -> Result<(), ParseError> {
    match !engine.chars().any(|c| c.is_whitespace() || c.is_control()) {
        true => Ok(()),
        false => Err(ParseError::Invalid {
            kind: "engine reference",
            value: engine.to_string(),
        }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloneDepth {
    Full,
    Limited,
}

impl CloneDepth {
    fn parse(depth: i64) -> Result<Self, ParseError> {
        match u32::try_from(depth) {
            Ok(0) => Ok(Self::Full),
            Ok(_) => Ok(Self::Limited),
            Err(_) => Err(ParseError::Invalid {
                kind: "clone depth",
                value: depth.to_string(),
            }),
        }
    }
}

pub struct RawWorkflow {
    pub name: WorkflowName,
    pub contents: Vec<u8>,
}

pub enum Trigger {
    Push { ref_name: RefName },
}

impl Trigger {
    fn kind(&self) -> &'static str {
        match self {
            Trigger::Push { .. } => "push",
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Diagnostics {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl Diagnostics {
    fn error(&mut self, path: &str, message: impl AsRef<str>) {
        self.errors
            .push(format!("error: {path}: {}", message.as_ref()));
    }

    fn warning(&mut self, path: &str, kind: &str, reason: &str) {
        self.warnings
            .push(format!("warning: {path}: {kind}: {reason}"));
    }

    fn combine(mut self, other: Diagnostics) -> Diagnostics {
        self.errors.extend(other.errors);
        self.warnings.extend(other.warnings);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.errors.is_empty() && self.warnings.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PathMatch {
    Assumed,
    Listed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledWorkflow {
    pub name: WorkflowName,
    pub paths: PathMatch,
}

pub struct Compiled {
    pub workflows: Vec<CompiledWorkflow>,
    pub diagnostics: Diagnostics,
}

impl Compiled {
    pub fn any_listed_match(&self) -> bool {
        self.workflows
            .iter()
            .any(|workflow| workflow.paths == PathMatch::Listed)
    }
}

pub fn compile(raw: &[RawWorkflow], trigger: &Trigger, changed: &ChangedFiles) -> Compiled {
    let (workflows, diagnostics) = raw
        .iter()
        .map(|workflow| compile_one(workflow, trigger, changed))
        .fold(
            (Vec::new(), Diagnostics::default()),
            |(mut workflows, diagnostics), (compiled, diag)| {
                workflows.extend(compiled);
                (workflows, diagnostics.combine(diag))
            },
        );
    Compiled {
        workflows,
        diagnostics,
    }
}

fn compile_one(
    raw: &RawWorkflow,
    trigger: &Trigger,
    changed: &ChangedFiles,
) -> (Option<CompiledWorkflow>, Diagnostics) {
    let mut diag = Diagnostics::default();
    let parsed = match parse(&raw.contents) {
        Ok(parsed) => parsed,
        Err(error) => {
            diag.error(raw.name.as_str(), error.to_string());
            return (None, diag);
        }
    };
    let matched = match workflow_matches(&parsed.when, trigger, changed) {
        Ok(matched) => matched,
        Err(error) => {
            diag.error(
                raw.name.as_str(),
                format!("failed to execute workflow: {error}"),
            );
            return (None, diag);
        }
    };
    let Some(paths) = matched else {
        diag.warning(
            raw.name.as_str(),
            "workflow skipped",
            &format!("didn't match trigger {}", trigger.kind()),
        );
        return (None, diag);
    };
    let depth = match CloneDepth::parse(parsed.clone.depth) {
        Ok(depth) => depth,
        Err(error) => {
            diag.error(raw.name.as_str(), error.to_string());
            return (None, diag);
        }
    };
    analyze_clone(&parsed.clone, depth, raw.name.as_str(), &mut diag);
    if parsed.engine.is_empty() {
        diag.error(raw.name.as_str(), "missing engine");
        return (None, diag);
    }
    match engine_is_named(&parsed.engine) {
        Ok(()) => (
            Some(CompiledWorkflow {
                name: raw.name.clone(),
                paths,
            }),
            diag,
        ),
        Err(error) => {
            diag.error(raw.name.as_str(), error.to_string());
            (None, diag)
        }
    }
}

fn analyze_clone(clone: &CloneOpts, depth: CloneDepth, path: &str, diag: &mut Diagnostics) {
    if !clone.skip {
        return;
    }
    [
        ("tags", clone.tags.is_some()),
        ("submodules", clone.submodules.is_some()),
        ("depth", depth == CloneDepth::Limited),
    ]
    .into_iter()
    .filter(|(_, set)| *set)
    .for_each(|(key, _)| {
        diag.warning(
            path,
            "invalid configuration",
            &format!("`clone.{key}` has no effect with `clone.skip`"),
        );
    });
}

fn workflow_matches(
    when: &[Constraint],
    trigger: &Trigger,
    changed: &ChangedFiles,
) -> Result<Option<PathMatch>, String> {
    if when.is_empty() {
        return Ok(Some(PathMatch::Listed));
    }
    when.iter()
        .map(|constraint| constraint_matches(constraint, trigger, changed))
        .collect::<Result<Vec<Option<PathMatch>>, String>>()
        .map(|results| results.into_iter().flatten().max())
}

fn constraint_matches(
    constraint: &Constraint,
    trigger: &Trigger,
    changed: &ChangedFiles,
) -> Result<Option<PathMatch>, String> {
    match trigger {
        Trigger::Push { ref_name } => {
            let event = constraint.event.0.iter().any(|kind| kind == "push");
            let reference = match ref_kind(ref_name.as_str()) {
                Some((RefKind::Branch, short)) => glob_set(&constraint.branch.0)?.is_match(short),
                Some((RefKind::Tag, short)) => glob_set(&constraint.tag.0)?.is_match(short),
                None => false,
            };
            let globs = glob_set(&constraint.paths.0)?;
            let listed = changed
                .paths()
                .iter()
                .any(|path| globs.is_match(path.as_str()));
            // We aren't the one deciding whether this workflow runs, remember,
            // spindles are, and spindles will decide from the
            // atproto record we will emit.
            let paths = match (constraint.paths.0.is_empty(), listed, changed.listing()) {
                (true, _, _) | (false, true, _) => Some(PathMatch::Listed),
                (false, false, Listing::Truncated) => Some(PathMatch::Assumed),
                (false, false, Listing::Complete) => None,
            };
            Ok(paths.filter(|_| event && reference))
        }
    }
}

enum RefKind {
    Branch,
    Tag,
}

fn ref_kind(reference: &str) -> Option<(RefKind, &str)> {
    reference
        .strip_prefix("refs/heads/")
        .map(|short| (RefKind::Branch, short))
        .or_else(|| {
            reference
                .strip_prefix("refs/tags/")
                .map(|short| (RefKind::Tag, short))
        })
}

fn glob_set(patterns: &[String]) -> Result<GlobSet, String> {
    patterns
        .iter()
        .try_fold(GlobSetBuilder::new(), |mut builder, pattern| {
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map(|glob| {
                    builder.add(glob);
                    builder
                })
                .map_err(|error| error.to_string())
        })
        .and_then(|builder| builder.build().map_err(|error| error.to_string()))
}

fn parse(contents: &[u8]) -> Result<WorkflowFile, serde_norway::Error> {
    serde_norway::from_slice(contents)
}

#[derive(Debug, Default, Deserialize)]
struct WorkflowFile {
    #[serde(default)]
    engine: String,
    #[serde(default)]
    when: Vec<Constraint>,
    #[serde(default)]
    clone: CloneOpts,
}

#[derive(Debug, Default, Deserialize)]
struct Constraint {
    #[serde(default)]
    event: StringList,
    #[serde(default)]
    branch: StringList,
    #[serde(default)]
    tag: StringList,
    #[serde(default)]
    paths: StringList,
}

#[derive(Debug, Default, Deserialize)]
struct CloneOpts {
    #[serde(default)]
    skip: bool,
    #[serde(default)]
    depth: i64,
    #[serde(default)]
    submodules: Option<bool>,
    #[serde(default)]
    tags: Option<bool>,
}

#[derive(Debug, Default)]
struct StringList(Vec<String>);

impl<'de> Deserialize<'de> for StringList {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOrMany {
            One(String),
            Many(Vec<String>),
        }
        Ok(match OneOrMany::deserialize(deserializer)? {
            OneOrMany::One(value) => StringList(vec![value]),
            OneOrMany::Many(values) => StringList(values),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(name: &str, contents: &str) -> RawWorkflow {
        RawWorkflow {
            name: WorkflowName::new(name).unwrap(),
            contents: contents.as_bytes().to_vec(),
        }
    }

    fn changed(values: &[&str]) -> ChangedFiles {
        let mut budget = knot_types::ChangedFilesBudget::new();
        let _ = values
            .iter()
            .try_for_each(|value| budget.admit(knot_types::RepoPath::new(*value).unwrap()));
        budget.finish()
    }

    fn push(reference: &str) -> Trigger {
        Trigger::Push {
            ref_name: RefName::new(reference).unwrap(),
        }
    }

    #[test]
    fn workflow_name_round_trips_through_as_str() {
        let name = WorkflowName::new("test.yml").unwrap();
        assert_eq!(name.as_str(), "test.yml");
        assert_eq!(name, WorkflowName::new("test.yml".to_string()).unwrap());
        assert_ne!(name, WorkflowName::new("ci.yml").unwrap());
    }

    #[test]
    fn a_matching_branch_push_compiles_the_workflow() {
        let workflows = [raw(
            "test.yml",
            "engine: nixery.dev/x\nwhen:\n  - event: push\n    branch: [main]\n",
        )];
        let compiled = compile(&workflows, &push("refs/heads/main"), &ChangedFiles::none());
        assert_eq!(
            compiled
                .workflows
                .iter()
                .map(|workflow| workflow.name.as_str())
                .collect::<Vec<&str>>(),
            vec!["test.yml"]
        );
        assert!(
            compiled.any_listed_match(),
            "a workflow with no paths constraint never depends on the listing"
        );
        assert!(
            compiled.diagnostics.is_empty(),
            "{:?}",
            compiled.diagnostics
        );
    }

    #[test]
    fn a_paths_constraint_matches_changed_files_and_never_their_parent_directories() {
        let changed = changed(&["src/deep/main.rs"]);
        let cases: &[(&str, usize)] = &[
            ("", 1),
            ("\n    paths: ['src/**']", 1),
            ("\n    paths: ['**/main.rs']", 1),
            ("\n    paths: ['docs/**']", 0),
            ("\n    paths: ['src']", 0),
            ("\n    paths: ['*']", 0),
            ("\n    paths: ['docs/**', 'src/**']", 1),
        ];
        cases.iter().for_each(|(constraint, count)| {
            let yaml = format!(
                "engine: nixery.dev/x\nwhen:\n  - event: push\n    branch: ['**']{constraint}\n"
            );
            let compiled = compile(&[raw("ci.yml", &yaml)], &push("refs/heads/main"), &changed);
            assert_eq!(compiled.workflows.len(), *count, "{yaml}");
        });

        let unmatched_branch = compile(
            &[raw(
                "ci.yml",
                "engine: nixery.dev/x\nwhen:\n  - event: push\n    branch: [release]\n    paths: ['[']\n",
            )],
            &push("refs/heads/main"),
            &changed,
        );
        assert!(
            !unmatched_branch.diagnostics.errors.is_empty(),
            "compile reports a malformed paths glob even when the branch already decided the match: {:?}",
            unmatched_branch.diagnostics
        );
    }

    #[test]
    fn a_truncated_listing_assumes_a_paths_match_and_only_a_listed_hit_promises_the_run() {
        let yaml = "engine: nixery.dev/x\nwhen:\n  - event: push\n    branch: ['**']\n    paths: ['src/**']\n";
        assert_eq!(
            compile(
                &[raw("ci.yml", yaml)],
                &push("refs/heads/main"),
                &changed(&["docs/only.md"])
            )
            .workflows
            .len(),
            0,
            "a complete listing that misses the globs skips the workflow"
        );

        let assumed = compile(
            &[raw("ci.yml", yaml)],
            &push("refs/heads/main"),
            &ChangedFiles::unknown(),
        );
        assert_eq!(
            assumed.workflows.len(),
            1,
            "a listing the record couldn't hold rules no glob out"
        );
        assert_eq!(assumed.workflows[0].paths, PathMatch::Assumed);
        assert!(
            !assumed.any_listed_match(),
            "spindle reads the same truncated listing and skips this run"
        );

        let mut budget = knot_types::ChangedFilesBudget::new();
        let _ = budget.admit(knot_types::RepoPath::new("src/deep/main.rs").unwrap());
        let _ = budget.truncate();
        let listed = compile(
            &[raw("ci.yml", yaml)],
            &push("refs/heads/main"),
            &budget.finish(),
        );
        assert_eq!(listed.workflows[0].paths, PathMatch::Listed);
        assert!(
            listed.any_listed_match(),
            "spindle sees the same listed path and runs this workflow"
        );

        let strongest = compile(
            &[raw(
                "ci.yml",
                "engine: nixery.dev/x\nwhen:\n  - event: push\n    branch: ['**']\n    paths: ['never/**']\n  - event: push\n    branch: ['**']\n",
            )],
            &push("refs/heads/main"),
            &ChangedFiles::unknown(),
        );
        assert_eq!(
            strongest.workflows[0].paths,
            PathMatch::Listed,
            "an assumed constraint never weakens an unconstrained one"
        );
    }

    #[test]
    fn compile_cases() {
        let tag = "engine: nixery.dev/x\nwhen:\n  - event: push\n    tag: ['v*']\n";
        let single_star =
            "engine: nixery.dev/x\nwhen:\n  - event: push\n    branch: ['feature/*']\n";
        let cases: &[(&str, &str, usize, Option<&str>)] = &[
            (
                "engine: nixery.dev/x\nwhen:\n  - event: push\n    branch: [dev]\n",
                "refs/heads/main",
                0,
                Some("workflow skipped"),
            ),
            ("engine: nixery.dev/x\n", "refs/heads/anything", 1, None),
            (
                "when:\n  - event: push\n    branch: ['*']\n",
                "refs/heads/main",
                0,
                Some("missing engine"),
            ),
            (
                "engine: nixery.dev/x\nclone:\n  skip: true\n  submodules: true\n",
                "refs/heads/main",
                1,
                Some("`clone.submodules` has no effect with `clone.skip`"),
            ),
            (
                "engine: nixery.dev/x\nclone:\n  skip: true\n  submodules: false\n",
                "refs/heads/main",
                1,
                Some("`clone.submodules` has no effect with `clone.skip`"),
            ),
            (
                "engine: nixery.dev/x\nclone:\n  skip: true\n  tags: true\n",
                "refs/heads/main",
                1,
                Some("`clone.tags` has no effect with `clone.skip`"),
            ),
            (
                "engine: nixery.dev/x\nclone:\n  skip: true\n",
                "refs/heads/main",
                1,
                None,
            ),
            (
                "engine: nixery.dev/x\nwhen:\n  - event: push\n    branch: '**'\n",
                "refs/heads/feature/x",
                1,
                None,
            ),
            (tag, "refs/tags/v1.0", 1, None),
            (tag, "refs/heads/v1.0", 0, None),
            (single_star, "refs/heads/feature/x", 1, None),
            (single_star, "refs/heads/feature/x/y", 0, None),
        ];
        cases.iter().for_each(|(yaml, reference, count, diag)| {
            let compiled = compile(
                &[raw("ci.yml", yaml)],
                &push(reference),
                &ChangedFiles::none(),
            );
            assert_eq!(compiled.workflows.len(), *count, "{yaml}");
            let messages: Vec<&String> = compiled
                .diagnostics
                .errors
                .iter()
                .chain(compiled.diagnostics.warnings.iter())
                .collect();
            match diag {
                Some(sub) => assert!(
                    messages.iter().any(|message| message.contains(sub)),
                    "{sub}: {messages:?}"
                ),
                None => assert!(
                    compiled.diagnostics.errors.is_empty()
                        && !messages
                            .iter()
                            .any(|message| message.contains("invalid configuration")),
                    "{yaml}: {messages:?}"
                ),
            }
        });
    }
}
