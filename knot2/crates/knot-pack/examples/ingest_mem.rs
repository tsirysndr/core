use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const PAGE: u64 = 4096;

fn rss_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    statm
        .split_whitespace()
        .nth(1)
        .and_then(|pages| pages.parse::<u64>().ok())
        .map(|pages| pages * PAGE)
        .unwrap()
}

fn vm_hwm_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap()
}

fn mib(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

fn main() {
    let pack_path = std::path::PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: ingest_mem <pack-file>"),
    );

    let pack_size = std::fs::metadata(&pack_path).map(|m| m.len()).unwrap_or(0);
    println!(
        "pack: {} MiB on disk, streamed not resident",
        mib(pack_size)
    );
    println!(
        "governor: {} threads, {}",
        knot_resource::threads().get(),
        knot_resource::available_bytes()
            .map(|bytes| format!("{} MiB available in cgroup", mib(bytes.get())))
            .unwrap_or_else(|| "unconstrained".to_string())
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let objects_dir = dir.path().to_path_buf();

    let peak = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let peak = Arc::clone(&peak);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let now = rss_bytes();
                peak.fetch_max(now, Ordering::Relaxed);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        })
    };

    let before = rss_bytes();
    let start = Instant::now();
    let folded = knot_pack::bench_ingest_fresh(&objects_dir, &pack_path, gix::hash::Kind::Sha1)
        .expect("ingest");
    let elapsed = start.elapsed();
    stop.store(true, Ordering::Relaxed);
    sampler.join().ok();

    let sampled_peak = peak.load(Ordering::Relaxed);
    println!("fold engaged: {folded}");
    println!("took: {:.1}s", elapsed.as_secs_f64());
    println!("rss before ingest: {} MiB", mib(before));
    println!("rss sampled peak: {} MiB", mib(sampled_peak));
    println!("VmHWM, kernel peak: {} MiB", mib(vm_hwm_bytes()));
}
