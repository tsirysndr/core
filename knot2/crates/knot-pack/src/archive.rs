use std::io::{self, Read, Seek, SeekFrom};

use knot_git::{ArchiveFormat, ArchivePrefix, Repo};
use knot_types::Oid;

use crate::error::PackError;
use crate::pkt;

struct Request {
    treeish: String,
    format: ArchiveFormat,
    prefix: Option<ArchivePrefix>,
}

pub fn stream(
    repo: &Repo,
    request: &[u8],
    sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<(), PackError> {
    let args = parse_arguments(request)?;
    match build(repo, &args) {
        Ok(mut spool) => {
            let mut head = Vec::new();
            pkt::write_data(&mut head, b"ACK\n")?;
            pkt::write_flush(&mut head)?;
            emit(sink, &head)?;
            std::iter::from_fn(|| {
                let mut chunk = vec![0u8; pkt::MAX_BAND];
                match spool.read(&mut chunk) {
                    Ok(0) => None,
                    Ok(read) => {
                        chunk.truncate(read);
                        Some(Ok(chunk))
                    }
                    Err(error) => Some(Err(error)),
                }
            })
            .try_for_each(|chunk| -> Result<(), PackError> {
                let chunk = chunk.map_err(|error| PackError::Pack(error.to_string()))?;
                let mut framed = Vec::new();
                pkt::write_band(&mut framed, &chunk)?;
                emit(sink, &framed)
            })?;
            let mut tail = Vec::new();
            pkt::write_flush(&mut tail)?;
            emit(sink, &tail)
        }
        Err(error) => {
            let mut buf = Vec::new();
            pkt::write_data(
                &mut buf,
                format!("NACK {}\n", error.to_string().replace('\n', " ")).as_bytes(),
            )?;
            pkt::write_flush(&mut buf)?;
            emit(sink, &buf)
        }
    }
}

fn emit(sink: &mut dyn FnMut(&[u8]) -> io::Result<()>, bytes: &[u8]) -> Result<(), PackError> {
    sink(bytes).map_err(|error| PackError::Pack(error.to_string()))
}

fn parse_arguments(request: &[u8]) -> Result<Vec<String>, PackError> {
    Ok(pkt::data_payloads(request)?
        .iter()
        .filter_map(|line| {
            std::str::from_utf8(line)
                .ok()?
                .trim_end_matches('\n')
                .strip_prefix("argument ")
                .map(str::to_string)
        })
        .collect())
}

fn interpret(args: &[String]) -> Result<Request, PackError> {
    let format = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--format="))
        .map(format_from)
        .unwrap_or(ArchiveFormat::Tar);
    let prefix = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--prefix="))
        .map(|raw| {
            ArchivePrefix::new(raw).map_err(|_| {
                PackError::Protocol("archive prefix must not escape archive root".to_string())
            })
        })
        .transpose()?;
    let treeish = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .cloned()
        .ok_or_else(|| PackError::Protocol("archive request has no tree-ish".to_string()))?;
    Ok(Request {
        treeish,
        format,
        prefix,
    })
}

fn format_from(value: &str) -> ArchiveFormat {
    match value {
        "zip" => ArchiveFormat::Zip,
        "tar.gz" | "tgz" => ArchiveFormat::TarGz,
        _ => ArchiveFormat::Tar,
    }
}

fn build(repo: &Repo, args: &[String]) -> Result<std::fs::File, PackError> {
    let request = interpret(args)?;
    let id = repo
        .resolve_revision(&request.treeish)
        .ok_or_else(|| PackError::Protocol(format!("cannot resolve {}", request.treeish)))?;
    let commit = ensure_public_commit(repo, id)?;
    let tree = repo
        .peel_to_tree(commit)
        .map_err(|error| PackError::Pack(error.to_string()))?;
    let mut spool = tempfile::tempfile().map_err(|error| PackError::Pack(error.to_string()))?;
    repo.write_archive(tree, request.format, request.prefix.as_ref(), &mut spool)
        .map_err(|error| PackError::Pack(error.to_string()))?;
    spool
        .seek(SeekFrom::Start(0))
        .map_err(|error| PackError::Pack(error.to_string()))?;
    Ok(spool)
}

fn ensure_public_commit(repo: &Repo, id: Oid) -> Result<Oid, PackError> {
    let unreachable =
        || PackError::Protocol("tree-ish is not reachable from public ref".to_string());
    let commit = repo.peel_to_commit(id).map_err(|_| unreachable())?;
    match repo.reachable_from_public(commit) {
        Ok(true) => Ok(commit),
        Ok(false) => Err(unreachable()),
        Err(error) => Err(PackError::Pack(error.to_string())),
    }
}
