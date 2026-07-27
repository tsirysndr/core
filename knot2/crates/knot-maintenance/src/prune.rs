use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use knot_types::Oid;

use crate::fsio;
use crate::{FileCount, MaintError, ObjectCount, PruneReport};

pub fn run(
    objects_dir: &Path,
    reachable: &HashSet<Oid>,
    loose: &[(Oid, PathBuf)],
    grace: Duration,
) -> Result<PruneReport, MaintError> {
    let removed = loose
        .iter()
        .filter(|(oid, _)| !reachable.contains(oid))
        .filter(|(_, path)| fsio::older_than(path, grace))
        .filter(|(_, path)| std::fs::remove_file(path).is_ok())
        .count();
    if removed > 0 {
        knot_resource::fsync_path(objects_dir)?;
    }
    Ok(PruneReport {
        removed: FileCount::new(removed),
        removed_packs: FileCount::new(0),
        crufted: ObjectCount::new(0),
        ran: true,
    })
}
