use std::sync::atomic::AtomicBool;

use gix_features::progress::Progress;

use crate::data::File;

///
pub mod checksum {
    /// Returned by [`data::File::verify_checksum()`][crate::data::File::verify_checksum()].
    pub type Error = crate::verify::checksum::Error;
}

/// Checksums and verify checksums
impl File {
    /// The checksum in the trailer of this pack data file
    pub fn checksum(&self) -> gix_hash::ObjectId {
        let trailer = self
            .read_span(
                (self.data_len() - self.object_hash.len_in_bytes()) as u64..self.data_len() as u64,
            )
            .expect("pack trailer is within the pack data");
        gix_hash::ObjectId::from_bytes_or_panic(&trailer)
    }

    /// Verifies that the checksum of the packfile over all bytes preceding it indeed matches the actual checksum,
    /// returning the actual checksum equivalent to the return value of [`checksum()`][File::checksum()] if there
    /// is no mismatch.
    ///
    /// Note that if no `progress` is desired, one can pass [`gix_features::progress::Discard`].
    ///
    /// Have a look at [`index::File::verify_integrity(…)`][crate::index::File::verify_integrity()] for an
    /// even more thorough integrity check.
    pub fn verify_checksum(
        &self,
        progress: &mut dyn Progress,
        should_interrupt: &AtomicBool,
    ) -> Result<gix_hash::ObjectId, checksum::Error> {
        let expected = self.checksum();
        let body_len = (self.data_len() - self.hash_len) as u64;
        let actual = match gix_hash::bytes_of_file(
            self.path(),
            body_len,
            self.object_hash,
            progress,
            should_interrupt,
        ) {
            Ok(id) => id,
            Err(gix_hash::io::Error::Io(err)) if err.kind() == std::io::ErrorKind::Interrupted => {
                return Err(checksum::Error::Interrupted);
            }
            Err(gix_hash::io::Error::Io(_)) => {
                let data = self.materialized()?;
                let mut hasher = gix_hash::hasher(self.object_hash);
                hasher.update(&data[..body_len as usize]);
                hasher.try_finalize()?
            }
            Err(gix_hash::io::Error::Hasher(err)) => return Err(checksum::Error::Hasher(err)),
        };
        actual.verify(&expected)?;
        Ok(actual)
    }
}
