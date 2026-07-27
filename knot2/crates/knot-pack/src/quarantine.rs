//! The same way one would do this for uploading images onto a server,
//! we stage pushes such that objects unpack into a separate bare repo
//! under the incoming prefix, because we treat a push as real only once
//! every object it referenced actually made it over.
//!
//! Hence aborting is as simple as removal of the directory that
//! represents the push in flight.
//!
//! "Why not use `GIT_QUARANTINE_PATH`?" - because we never forked `receive-pack`.
//!
//! If you were wondering, this `sweep_incoming` is for a crash between a stage
//! and migration that would otherwise leak a staging dir.

use std::path::Path;

use knot_git::{INCOMING_PREFIX, Repo, Staging};

use crate::error::PackError;
use crate::meter::PackLimits;
use crate::objects;

pub(crate) struct Quarantine {
    staging: Staging,
}

impl Quarantine {
    pub(crate) fn stage(
        live: &Repo,
        pack: Option<&gix_pack::data::File>,
        limits: &PackLimits,
        kind: gix::hash::Kind,
        live_empty: bool,
    ) -> Result<(Self, Option<objects::FreshClosure>), PackError> {
        let staging = Staging::new(live)?;
        let (unpack, closure) = crate::receive::ingest(
            &staging.repo().objects_dir(),
            pack,
            limits,
            kind,
            live_empty,
        );
        unpack?;
        Ok((Self { staging }, closure))
    }

    pub(crate) fn repo(&self) -> &Repo {
        self.staging.repo()
    }

    pub(crate) fn migrate_into(&self, live: &Repo) -> Result<(), PackError> {
        self.staging.migrate_into(live).map_err(PackError::from)
    }
}

pub fn sweep_incoming(scan_path: &Path) -> usize {
    walkdir::WalkDir::new(scan_path)
        .into_iter()
        .filter_entry(|entry| entry.file_name().to_str() != Some("objects"))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_dir())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(INCOMING_PREFIX))
        })
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>()
        .into_iter()
        .filter(|path| std::fs::remove_dir_all(path).is_ok())
        .count()
}
