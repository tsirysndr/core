use std::path::Path;

use crate::data;

/// Instantiation
impl data::File {
    /// Try opening a data file at the given `path`.
    ///
    /// The `object_hash` is a way to read (and write) the same file format with different hashes, as the hash kind
    /// isn't stored within the file format itself.
    ///
    /// This constructor leaves allocation limiting disabled, allowing allocations of any size dictated by pack data.
    /// Call [`File::with_alloc_limit_bytes()`][crate::data::File::with_alloc_limit_bytes()] before decoding entries from untrusted input.
    pub fn at(
        path: impl AsRef<Path>,
        object_hash: gix_hash::Kind,
    ) -> Result<Self, data::header::decode::Error> {
        Self::at_inner(path.as_ref(), object_hash)
    }

    fn at_inner(
        path: &Path,
        object_hash: gix_hash::Kind,
    ) -> Result<Self, data::header::decode::Error> {
        use std::os::unix::fs::FileExt;

        use crate::data::header::N32_SIZE;
        let hash_len = object_hash.len_in_bytes();
        let file = std::fs::File::open(path).map_err(|e| data::header::decode::Error::Io {
            source: e,
            path: path.to_owned(),
        })?;
        let pack_len = file
            .metadata()
            .map_err(|e| data::header::decode::Error::Io {
                source: e,
                path: path.to_owned(),
            })?
            .len();
        let pack_len = usize::try_from(pack_len).map_err(|_| {
            data::header::decode::Error::Corrupt(format!(
                "Pack data of size {pack_len} is too large for this machine"
            ))
        })?;
        if pack_len < N32_SIZE * 3 + hash_len {
            return Err(data::header::decode::Error::Corrupt(format!(
                "Pack data of size {pack_len} is too small for even an empty pack with shortest hash"
            )));
        }
        let mut header = [0u8; 12];
        file.read_exact_at(&mut header, 0)
            .map_err(|e| data::header::decode::Error::Io {
                source: e,
                path: path.to_owned(),
            })?;
        let (version, num_objects) = data::header::decode(&header)?;
        let id = gix_features::hash::crc32(path.as_os_str().to_string_lossy().as_bytes());
        Ok(Self {
            file,
            len: pack_len,
            path: path.to_owned(),
            id,
            version,
            num_objects,
            hash_len,
            object_hash,
            alloc_limit_bytes: None,
        })
    }

    /// Configure the maximum size of a single allocation caused by user-controlled on-disk pack data.
    ///
    /// Use `None` to disable the limit, which is also the default.
    ///
    /// This is currently enforced when decoding pack entries and resolving delta chains.
    /// Callers that allocate from pack metadata directly should consult [`File::alloc_limit_bytes()`][crate::data::File::alloc_limit_bytes()]
    /// and apply the same limit themselves.
    pub fn with_alloc_limit_bytes(mut self, alloc_limit_bytes: Option<usize>) -> Self {
        self.alloc_limit_bytes = alloc_limit_bytes;
        self
    }
}
