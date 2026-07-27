#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dump = args.iter().any(|arg| arg == "dump");
    let target_flag = args.iter().position(|arg| arg == "--target");
    let target = match target_flag {
        Some(position) => match args.get(position + 1) {
            Some(path) => Some(path.clone()),
            None => {
                eprintln!("knot-sim: --target needs a path");
                std::process::exit(1);
            }
        },
        None => None,
    };
    let numbers: Vec<u64> = args
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            !matches!(target_flag, Some(position) if *index == position || *index == position + 1)
        })
        .filter_map(|(_, arg)| arg.parse().ok())
        .collect();
    let seed = numbers.first().copied().unwrap_or(1);
    let rounds = numbers.get(1).copied().unwrap_or(12) as u32;
    let trace = match target {
        Some(path) => match knot_sim::run_realdata(std::path::Path::new(&path), seed, rounds).await
        {
            Ok(trace) => trace,
            Err(error) => {
                eprintln!("knot-sim: {error}");
                std::process::exit(1);
            }
        },
        None => knot_sim::run(seed, rounds).await,
    };
    if dump {
        println!(
            "{}",
            serde_json::to_string_pretty(&trace).expect("trace serializes")
        );
    }
    println!(
        "knot-sim seed={seed} rounds={rounds} steps={} snapshots={} digest={:016x}",
        trace.steps.len(),
        trace.snapshots.len(),
        trace.digest()
    );
}
