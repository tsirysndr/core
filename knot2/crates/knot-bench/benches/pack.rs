use std::time::Duration;

use divan::Bencher;
use divan::counter::{BytesCount, ItemsCount};
use knot_bench::{ChurnCount, CommitCount, HistorySpec, PathCount, build_history};
use knot_git::{Filter, Haves, PackBudget, Repo, Wants};
use knot_pack::{
    HaveOids, PackLimits, ReceiveCommand, ReceiveGuard, RefDecision, WantOids, count_expanded,
    local_pack, receive_pack_guarded, upload_archive, write_expanded, write_pack,
};
use knot_types::{ObjectCount, Oid};

#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

const GRADES: &[u32] = &[64, 256, 1024];
const CAP: u64 = 512 * 1024 * 1024;

fn spec_for(commits: u32) -> HistorySpec {
    HistorySpec {
        commits: CommitCount::new(commits),
        paths: PathCount::new(commits.max(64)),
        churn: ChurnCount::new(8),
    }
}

fn pkt(payload: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
    out.extend_from_slice(payload);
    out
}

struct AllowAll;

impl ReceiveGuard for AllowAll {
    fn authorize(&self, _staged: &Repo, commands: &[ReceiveCommand]) -> Vec<RefDecision> {
        commands.iter().map(|_| RefDecision::Allow).collect()
    }
}

fn main() {
    divan::main();
}

#[divan::bench(args = GRADES)]
fn select(bencher: Bencher, commits: u32) {
    let history = build_history(spec_for(commits));
    let tips = history.tips();
    let count = history
        .repo()
        .select_pack_objects_filtered(
            Wants::new(&tips),
            Haves::new(&[]),
            Filter::None,
            PackBudget::unbounded(),
        )
        .unwrap()
        .send
        .len();
    bencher.counter(ItemsCount::new(count)).bench_local(|| {
        history
            .repo()
            .select_pack_objects_filtered(
                Wants::new(&tips),
                Haves::new(&[]),
                Filter::None,
                PackBudget::unbounded(),
            )
            .unwrap()
    });
}

#[divan::bench(args = GRADES)]
fn clone(bencher: Bencher, commits: u32) {
    let history = build_history(spec_for(commits));
    let wants = WantOids::new(history.tips());
    let no_haves = HaveOids::default();
    let bytes = local_pack(history.repo(), &wants, &no_haves, CAP)
        .unwrap()
        .len();
    bencher
        .counter(BytesCount::new(bytes))
        .bench_local(|| local_pack(history.repo(), &wants, &no_haves, CAP).unwrap());
}

#[divan::bench(args = GRADES)]
fn full_clone_manual(bencher: Bencher, commits: u32) {
    let history = build_history(spec_for(commits));
    let tips = history.tips();
    let dir = history.repo().objects_dir();
    let count = history
        .repo()
        .select_pack_objects_filtered(
            Wants::new(&tips),
            Haves::new(&[]),
            Filter::None,
            PackBudget::unbounded(),
        )
        .unwrap()
        .send
        .len();
    bencher.counter(ItemsCount::new(count)).bench_local(|| {
        let send = history
            .repo()
            .select_pack_objects_filtered(
                Wants::new(&tips),
                Haves::new(&[]),
                Filter::None,
                PackBudget::unbounded(),
            )
            .unwrap()
            .send;
        write_pack(
            &dir,
            send,
            None,
            &mut std::io::sink(),
            history.repo().object_format().kind(),
        )
        .unwrap();
    });
}

#[divan::bench(args = GRADES)]
fn full_clone_expanding(bencher: Bencher, commits: u32) {
    let history = build_history(spec_for(commits));
    let tips = history.tips();
    let dir = history.repo().objects_dir();
    let far = Duration::from_secs(3600);
    let roots = history
        .repo()
        .clone_roots(&tips, PackBudget::unbounded())
        .unwrap();
    let kind = history.repo().object_format().kind();
    let count = count_expanded(&dir, roots, ObjectCount::new(usize::MAX), far, kind)
        .unwrap()
        .len();
    bencher.counter(ItemsCount::new(count)).bench_local(|| {
        let roots = history
            .repo()
            .clone_roots(&tips, PackBudget::unbounded())
            .unwrap();
        let pack = count_expanded(&dir, roots, ObjectCount::new(usize::MAX), far, kind).unwrap();
        write_expanded(pack, &mut std::io::sink()).unwrap();
    });
}

#[divan::bench(args = GRADES)]
fn fetch(bencher: Bencher, commits: u32) {
    let history = build_history(spec_for(commits));
    let tips = history.tips();
    let walk = history
        .repo()
        .rev_walk(Wants::new(&tips), Haves::new(&[]))
        .unwrap();
    let haves = HaveOids::new(vec![walk[walk.len() / 2]]);
    let wants = WantOids::new(tips);
    let bytes = local_pack(history.repo(), &wants, &haves, CAP)
        .unwrap()
        .len();
    bencher
        .counter(BytesCount::new(bytes))
        .bench_local(|| local_pack(history.repo(), &wants, &haves, CAP).unwrap());
}

#[divan::bench(args = GRADES)]
fn push(bencher: Bencher, commits: u32) {
    let history = build_history(spec_for(commits));
    let tips = history.tips();
    let pack = local_pack(
        history.repo(),
        &WantOids::new(tips.clone()),
        &HaveOids::default(),
        CAP,
    )
    .unwrap();
    let walk = history
        .repo()
        .rev_walk(Wants::new(&tips), Haves::new(&[]))
        .unwrap();
    let stride = walk.len().max(1) / 8 + 1;
    let branch_tips: Vec<Oid> = walk.iter().step_by(stride).copied().collect();
    let request = build_receive_request(&branch_tips, &pack);
    let limits = PackLimits::default();
    bencher
        .counter(ItemsCount::new(branch_tips.len()))
        .with_inputs(fresh_target)
        .bench_local_values(|target| {
            receive_pack_guarded(
                target.repo(),
                &request,
                &limits,
                &AllowAll,
                &|_| {},
                &knot_pack::default_catalog().reject,
            )
            .unwrap()
        });
}

#[divan::bench(args = GRADES)]
fn archive(bencher: Bencher, commits: u32) {
    let history = build_history(spec_for(commits));
    let request = build_archive_request(history.tip());
    let bytes = upload_archive(history.repo(), &request, knot_git::ArchiveLimit::default())
        .unwrap()
        .len();
    bencher.counter(BytesCount::new(bytes)).bench_local(|| {
        upload_archive(history.repo(), &request, knot_git::ArchiveLimit::default()).unwrap()
    });
}

struct FreshTarget {
    _dir: tempfile::TempDir,
    repo: Repo,
}

impl FreshTarget {
    fn repo(&self) -> &Repo {
        &self.repo
    }
}

fn fresh_target() -> FreshTarget {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repo::create(dir.path().join("target.git")).expect("create target");
    FreshTarget { _dir: dir, repo }
}

fn build_receive_request(branch_tips: &[Oid], pack: &[u8]) -> Vec<u8> {
    let null = Oid::null().to_hex();
    let commands: Vec<Vec<u8>> = branch_tips
        .iter()
        .enumerate()
        .map(|(index, tip)| {
            let line = format!("{null} {} refs/heads/b{index}", tip.to_hex());
            match index {
                0 => {
                    let mut payload = line.into_bytes();
                    payload.push(0);
                    payload.extend_from_slice(b"report-status\n");
                    pkt(&payload)
                }
                _ => pkt(format!("{line}\n").as_bytes()),
            }
        })
        .collect();
    let mut request: Vec<u8> = commands.concat();
    request.extend_from_slice(b"0000");
    request.extend_from_slice(pack);
    request
}

fn build_archive_request(tip: Oid) -> Vec<u8> {
    let mut request = pkt(b"argument --format=tar");
    request.extend_from_slice(&pkt(format!("argument {}", tip.to_hex()).as_bytes()));
    request.extend_from_slice(b"0000");
    request
}
