use std::collections::HashMap;
use std::ops::ControlFlow;
use std::time::Instant;

use gengo_language::{Category, Language};
use knot_git::{EntryKind, GitError, MAX_TREE_DEPTH, Repo, SizedEntry};
use knot_types::{LanguageBytes, Oid};

use crate::linguist;

const READ_LIMIT: usize = 16 * 1024;
const SIZE_LIMIT: u64 = 1024 * 1024;

pub use knot_types::LanguageName;

fn looks_binary(content: &[u8]) -> bool {
    content.contains(&0)
}

fn category_of(name: &'static str, fallback: Category) -> Category {
    name.parse::<Language>()
        .map(|language| language.category())
        .unwrap_or(fallback)
}

pub fn analyze(
    repo: &Repo,
    commit: Oid,
    deadline: Option<Instant>,
) -> Result<HashMap<LanguageName, LanguageBytes>, GitError> {
    let mut sizes: HashMap<LanguageName, LanguageBytes> = HashMap::new();
    let root = repo.peel_to_tree(commit)?;
    let _budget = walk(repo, root, "", 0, deadline, &mut sizes)?;
    Ok(sizes)
}

fn walk(
    repo: &Repo,
    tree: Oid,
    dir: &str,
    depth: usize,
    deadline: Option<Instant>,
    sizes: &mut HashMap<LanguageName, LanguageBytes>,
) -> Result<ControlFlow<()>, GitError> {
    if depth > MAX_TREE_DEPTH || deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Ok(ControlFlow::Break(()));
    }
    let entries = repo.tree_entries(tree)?;
    entries
        .iter()
        .try_fold(ControlFlow::Continue(()), |flow, entry| {
            if flow.is_break() || deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(ControlFlow::Break(()));
            }
            let path = match dir.is_empty() {
                true => entry.name.clone(),
                false => format!("{dir}/{}", entry.name),
            };
            match entry.kind {
                EntryKind::Tree => match linguist::is_vendor_dir(&path) {
                    true => Ok(ControlFlow::Continue(())),
                    false => walk(repo, entry.oid, &path, depth + 1, deadline, sizes),
                },
                EntryKind::Blob | EntryKind::BlobExecutable => {
                    if !linguist::is_skipped_path(&path) {
                        tally(repo, entry, &path, sizes)?;
                    }
                    Ok(ControlFlow::Continue(()))
                }
                EntryKind::Link | EntryKind::Commit => Ok(ControlFlow::Continue(())),
            }
        })
}

fn tally(
    repo: &Repo,
    entry: &SizedEntry,
    path: &str,
    sizes: &mut HashMap<LanguageName, LanguageBytes>,
) -> Result<(), GitError> {
    let content = match entry.size <= SIZE_LIMIT {
        true => {
            let blob = repo.read_blob(entry.oid)?;
            blob[..blob.len().min(READ_LIMIT)].to_vec()
        }
        false => Vec::new(),
    };
    if looks_binary(&content) {
        return Ok(());
    }
    let Some(language) = Language::pick(path, &content, READ_LIMIT) else {
        return Ok(());
    };
    let name = LanguageName::new(linguist::group(language.name()));
    if !matches!(
        category_of(name.as_str(), language.category()),
        Category::Programming | Category::Markup
    ) {
        return Ok(());
    }
    let slot = sizes.entry(name).or_default();
    *slot = slot.saturating_add_bytes(entry.size);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_name_round_trips_through_as_str() {
        let rust = LanguageName::new("Rust");
        assert_eq!(rust.as_str(), "Rust");
    }

    #[test]
    fn language_names_compare_and_hash_by_value() {
        use std::collections::HashSet;

        assert_eq!(LanguageName::new("Go"), LanguageName::new("Go"));
        assert_ne!(LanguageName::new("Go"), LanguageName::new("Zig"));
        let set: HashSet<LanguageName> = [LanguageName::new("Go"), LanguageName::new("Go")]
            .into_iter()
            .collect();
        assert_eq!(set.len(), 1);
    }
}
