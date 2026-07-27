#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gix_packetline::blocking_io::encode;
use knot_lfs::{LfsOid, LfsSize};
use knot_types::RepoDid;
use sha2::{Digest, Sha256};

pub const SQUID: &str = "did:plc:squid";
pub const PKT_DATA_MAX: usize = 65516;
pub const PEAK_CEILING: u64 = 512 * 1024 * 1024;
pub const GROWTH_SLACK: u64 = 64 * 1024 * 1024;

pub fn repo() -> RepoDid {
    RepoDid::new(SQUID).unwrap()
}

pub fn oid_of(bytes: &[u8]) -> LfsOid {
    LfsOid::from_digest(Sha256::digest(bytes).into())
}

pub fn incompressible(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state & 0xff) as u8
        })
        .collect()
}

pub fn pointer_blob(oid: &LfsOid, size: LfsSize) -> Vec<u8> {
    format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {size}\n")
        .into_bytes()
}

pub fn object_path(store_dir: &Path, oid: &LfsOid) -> PathBuf {
    store_dir
        .join("plc/sq/uid")
        .join(&oid.as_str()[0..2])
        .join(&oid.as_str()[2..4])
        .join(oid.as_str())
}

pub fn backdate(path: &Path, past: Duration) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - past)
        .unwrap();
}

pub fn put_text(buf: &mut Vec<u8>, line: &str) {
    encode::data_to_write(format!("{line}\n").as_bytes(), &mut *buf).unwrap();
}

pub fn upload_script(body: &[u8]) -> (LfsOid, Vec<u8>) {
    let oid = oid_of(body);
    let mut script = Vec::with_capacity(body.len() + 4096);
    put_text(&mut script, &format!("put-object {oid}"));
    put_text(&mut script, &format!("size={}", body.len()));
    encode::delim_to_write(&mut script).unwrap();
    body.chunks(PKT_DATA_MAX).for_each(|chunk| {
        encode::data_to_write(chunk, &mut script).unwrap();
    });
    encode::flush_to_write(&mut script).unwrap();
    put_text(&mut script, &format!("verify-object {oid}"));
    put_text(&mut script, &format!("size={}", body.len()));
    encode::flush_to_write(&mut script).unwrap();
    put_text(&mut script, "quit");
    encode::flush_to_write(&mut script).unwrap();
    (oid, script)
}

pub fn download_script(oid: &LfsOid) -> Vec<u8> {
    let mut script = Vec::new();
    put_text(&mut script, &format!("get-object {oid}"));
    encode::flush_to_write(&mut script).unwrap();
    put_text(&mut script, "quit");
    encode::flush_to_write(&mut script).unwrap();
    script
}

pub fn rss_bytes() -> u64 {
    const PAGE_BYTES: u64 = 4096;
    let statm = std::fs::read_to_string("/proc/self/statm").expect("/proc/self/statm is readable");
    statm
        .split_whitespace()
        .nth(1)
        .and_then(|pages| pages.parse::<u64>().ok())
        .map(|pages| pages * PAGE_BYTES)
        .expect("statm lists the resident page count")
}
