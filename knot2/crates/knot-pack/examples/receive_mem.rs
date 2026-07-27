use std::io::Read;
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

fn pktline(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", 4 + data.len()).into_bytes();
    out.extend_from_slice(data);
    out
}

fn main() {
    let max_threads = std::env::var("KNOT_MAX_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok());
    knot_resource::init(knot_resource::Ceilings {
        max_threads: max_threads.map(knot_resource::ThreadCount::new),
        max_memory: None,
    });

    let pack_path = std::path::PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: receive_mem <pack-file>"),
    );
    let pack_size = std::fs::metadata(&pack_path).map(|m| m.len()).unwrap_or(0);
    println!("pack: {} MiB on disk", mib(pack_size));

    let mut preamble = pktline(
        b"0000000000000000000000000000000000000000 \
          1111111111111111111111111111111111111111 refs/heads/main\0report-status\n",
    );
    preamble.extend_from_slice(b"0000");

    let dir = tempfile::tempdir().expect("tempdir");
    let limit = knot_pack::MaxWireBytes::new(16 * 1024 * 1024 * 1024);
    let mut receiver = knot_pack::PackReceiver::new(
        dir.path(),
        limit,
        knot_pack::PackLimits::default(),
        gix::hash::Kind::Sha1,
    )
    .expect("receiver");

    let peak = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let peak = Arc::clone(&peak);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                peak.fetch_max(rss_bytes(), Ordering::Relaxed);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        })
    };

    let start = Instant::now();
    receiver.write(&preamble).expect("preamble");
    let mut file = std::fs::File::open(&pack_path).expect("open pack");
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buf).expect("read pack");
        if read == 0 {
            break;
        }
        receiver.write(&buf[..read]).expect("receive");
    }
    let received = receiver.finish().expect("finish");
    let receipt_elapsed = start.elapsed();
    let receipt_peak = peak.load(Ordering::Relaxed);
    println!(
        "receipt: {:.1}s, peak {} MiB (pack streamed to a temp file, never resident)",
        receipt_elapsed.as_secs_f64(),
        mib(receipt_peak)
    );

    let staged = received.open_pack().expect("open pack").expect("has pack");
    let objects = tempfile::tempdir().expect("objects tempdir");
    let result =
        knot_pack::bench_admit_and_ingest(objects.path(), staged.path(), gix::hash::Kind::Sha1);
    let total_elapsed = start.elapsed();
    let total_peak = peak.load(Ordering::Relaxed);
    stop.store(true, Ordering::Relaxed);
    sampler.join().ok();

    match result {
        Ok(folded) => {
            println!("ADMITTED, fold engaged: {folded}");
            println!("receive + ingest: {:.1}s", total_elapsed.as_secs_f64());
            println!("receive + ingest peak: {} MiB", mib(total_peak));
            println!("VmHWM,kernel peak: {} MiB", mib(vm_hwm_bytes()));
        }
        Err(error) => {
            println!("REFUSED cleanly before the fold: {error:?}");
            println!("peak at refusal: {} MiB", mib(total_peak));
        }
    }
}
