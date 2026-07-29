#![allow(dead_code, unused_imports)]

use knot_git::Layout;
use knot_types::RepoDid;

pub use knot_fixtures::{
    available as git_available, commit as commit_file, contains, must as git_ok, run as git,
};

pub fn seeded() -> (tempfile::TempDir, tempfile::TempDir, Layout, RepoDid) {
    let scan = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let layout = Layout::new(scan.path());
    let did = RepoDid::new("did:plc:squid").unwrap();
    layout.create(&did).unwrap();
    git_ok(work.path(), &["init", "-q", "-b", "main"]);
    (scan, work, layout, did)
}
