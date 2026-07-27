use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

use crate::error::PackError;
use crate::frame::ReceiveFramer;
use crate::ids::MaxWireBytes;
use crate::meter::PackLimits;

#[derive(Debug)]
pub enum ReceiveReadError {
    Io(std::io::Error),
    Pack(PackError),
    TooLarge,
    Truncated,
}

pub struct ReceivedPack {
    preamble: Vec<u8>,
    pack: Option<NamedTempFile>,
    kind: gix::hash::Kind,
    total_len: usize,
}

impl ReceivedPack {
    pub fn preamble(&self) -> &[u8] {
        &self.preamble
    }

    pub fn len(&self) -> usize {
        self.total_len
    }

    pub fn is_empty(&self) -> bool {
        self.total_len == 0
    }

    pub fn open_pack(&self) -> Result<Option<gix_pack::data::File>, PackError> {
        match &self.pack {
            Some(tmp) => gix_pack::data::File::at(tmp.path(), self.kind)
                .map(Some)
                .map_err(|error| PackError::Pack(error.to_string())),
            None => Ok(None),
        }
    }
}

pub struct PackReceiver {
    dir: PathBuf,
    file: NamedTempFile,
    framer: ReceiveFramer,
    written: u64,
    limit: MaxWireBytes,
    complete: Option<usize>,
    kind: gix::hash::Kind,
}

impl PackReceiver {
    pub fn new(
        dir: &Path,
        limit: MaxWireBytes,
        limits: PackLimits,
        kind: gix::hash::Kind,
    ) -> std::io::Result<Self> {
        Ok(Self {
            dir: dir.to_path_buf(),
            file: NamedTempFile::new_in(dir)?,
            framer: ReceiveFramer::new(limits, kind),
            written: 0,
            limit,
            complete: None,
            kind,
        })
    }

    pub fn write(&mut self, chunk: &[u8]) -> Result<bool, ReceiveReadError> {
        if self.complete.is_some() {
            return Ok(true);
        }
        if self.written as usize + chunk.len() > self.limit.get() {
            return Err(ReceiveReadError::TooLarge);
        }
        self.file.write_all(chunk).map_err(ReceiveReadError::Io)?;
        self.written += chunk.len() as u64;
        self.scan()
    }

    fn scan(&mut self) -> Result<bool, ReceiveReadError> {
        if self.written == 0 {
            return Ok(false);
        }
        self.file.flush().map_err(ReceiveReadError::Io)?;
        match self
            .framer
            .advance_file(self.file.as_file(), self.written)
            .map_err(ReceiveReadError::Pack)?
        {
            Some(total) => {
                self.complete = Some(total);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub fn finish(mut self) -> Result<ReceivedPack, ReceiveReadError> {
        let total = match self.complete {
            Some(total) => total,
            None => {
                if self.written == 0 {
                    0
                } else if self.scan()? {
                    self.complete.expect("scan recorded completion")
                } else {
                    return Err(ReceiveReadError::Truncated);
                }
            }
        };
        let pack_start = self.framer.pack_start().unwrap_or(total);
        let preamble =
            read_range(self.file.as_file(), 0..pack_start).map_err(ReceiveReadError::Io)?;
        let pack = match total > pack_start {
            true => {
                let mut tmp = NamedTempFile::new_in(&self.dir).map_err(ReceiveReadError::Io)?;
                copy_range(
                    self.file.as_file(),
                    pack_start as u64..total as u64,
                    tmp.as_file_mut(),
                )
                .map_err(ReceiveReadError::Io)?;
                tmp.flush().map_err(ReceiveReadError::Io)?;
                Some(tmp)
            }
            false => None,
        };
        Ok(ReceivedPack {
            preamble,
            pack,
            kind: self.kind,
            total_len: total,
        })
    }
}

fn read_range(file: &std::fs::File, range: std::ops::Range<usize>) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let mut buf = vec![0u8; range.end.saturating_sub(range.start)];
    file.read_exact_at(&mut buf, range.start as u64)?;
    Ok(buf)
}

fn copy_range(
    src: &std::fs::File,
    range: std::ops::Range<u64>,
    dst: &mut std::fs::File,
) -> std::io::Result<()> {
    let mut reader = src.try_clone()?;
    reader.seek(SeekFrom::Start(range.start))?;
    std::io::copy(&mut reader.take(range.end.saturating_sub(range.start)), dst)?;
    Ok(())
}
