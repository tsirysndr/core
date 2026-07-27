use std::ops::ControlFlow;

use crate::ids::RepoPath;

const MAX_BYTES: usize = 524_288;
const ENTRY_OVERHEAD_BYTES: usize = 48;
const MAX_ENTRIES: usize = 8_192;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Listing {
    #[default]
    Complete,
    Truncated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFiles {
    paths: Vec<RepoPath>,
    listing: Listing,
}

impl ChangedFiles {
    pub fn none() -> Self {
        Self {
            paths: Vec::new(),
            listing: Listing::Complete,
        }
    }

    pub fn unknown() -> Self {
        Self {
            paths: Vec::new(),
            listing: Listing::Truncated,
        }
    }

    pub fn paths(&self) -> &[RepoPath] {
        &self.paths
    }

    pub fn listing(&self) -> Listing {
        self.listing
    }

    pub fn into_paths(self) -> Vec<RepoPath> {
        self.paths
    }
}

#[derive(Debug, Default)]
pub struct ChangedFilesBudget {
    paths: Vec<RepoPath>,
    spent: usize,
    listing: Listing,
}

impl ChangedFilesBudget {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn admit(&mut self, path: RepoPath) -> ControlFlow<()> {
        let spent = self.spent + ENTRY_OVERHEAD_BYTES + path.as_str().len();
        match self.listing == Listing::Truncated
            || spent > MAX_BYTES
            || self.paths.len() == MAX_ENTRIES
        {
            true => self.truncate(),
            false => {
                self.spent = spent;
                self.paths.push(path);
                ControlFlow::Continue(())
            }
        }
    }

    pub fn truncate(&mut self) -> ControlFlow<()> {
        self.listing = Listing::Truncated;
        ControlFlow::Break(())
    }

    pub fn finish(self) -> ChangedFiles {
        ChangedFiles {
            listing: self.listing,
            paths: self.paths,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ChangedFiles, ChangedFilesBudget, Listing, MAX_BYTES, MAX_ENTRIES};
    use crate::ids::RepoPath;
    use std::ops::ControlFlow;

    fn fill(mut paths: impl Iterator<Item = String>) -> ChangedFiles {
        let mut budget = ChangedFilesBudget::new();
        let _ = paths.try_for_each(|path| budget.admit(RepoPath::new(path).unwrap()));
        budget.finish()
    }

    #[test]
    fn a_listing_within_the_budget_stays_complete() {
        let changed = fill(["a.txt", "src/deep/main.rs"].into_iter().map(String::from));
        assert_eq!(changed.listing(), Listing::Complete);
        assert_eq!(changed.paths().len(), 2);
        assert_eq!(ChangedFiles::none().listing(), Listing::Complete);
        assert_eq!(
            ChangedFiles::unknown().listing(),
            Listing::Truncated,
            "a listing that couldn't be computed rules no path constraint out"
        );
    }

    #[test]
    fn the_listing_stops_at_whichever_bound_it_hits_first() {
        let wide = fill((0..MAX_ENTRIES * 2).map(|index| format!("f{index}.txt")));
        assert_eq!(wide.listing(), Listing::Truncated);
        assert_eq!(
            wide.paths().len(),
            MAX_ENTRIES,
            "the record codec refuses a longer array"
        );

        let deep = fill((0..MAX_ENTRIES).map(|index| format!("{}/f{index}.txt", "d".repeat(256))));
        assert_eq!(deep.listing(), Listing::Truncated);
        assert!(
            deep.paths().len() < MAX_ENTRIES,
            "long paths run out the byte budget before the entry bound: {}",
            deep.paths().len()
        );
    }

    #[test]
    fn truncation_is_sticky_and_keeps_only_the_paths_already_admitted() {
        let mut budget = ChangedFilesBudget::new();
        assert_eq!(
            budget.admit(RepoPath::new("a.txt").unwrap()),
            ControlFlow::Continue(())
        );
        assert_eq!(
            budget.admit(RepoPath::new("x".repeat(MAX_BYTES)).unwrap()),
            ControlFlow::Break(()),
            "a path larger than the whole remaining budget is refused"
        );
        assert_eq!(budget.truncate(), ControlFlow::Break(()));
        assert_eq!(
            budget.admit(RepoPath::new("b.txt").unwrap()),
            ControlFlow::Break(()),
            "a short path after the break can't reopen the listing"
        );
        let changed = budget.finish();
        assert_eq!(changed.paths(), [RepoPath::new("a.txt").unwrap()]);
        assert_eq!(changed.listing(), Listing::Truncated);
    }
}
