use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::ops::Bound;

type Key = (u64, u32);

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn adversarial_keys(n: u32) -> Vec<Key> {
    (0..n)
        .scan(0xD1CE_5EED_u64, |state, i| Some((splitmix64(state), i)))
        .collect()
}

fn vec_shift_work(keys: &[Key]) -> u128 {
    let mut sorted: Vec<Key> = Vec::with_capacity(keys.len());
    keys.iter()
        .fold(0u128, |moves, &key| match sorted.binary_search(&key) {
            Ok(_) => moves,
            Err(pos) => {
                let displaced = (sorted.len() - pos) as u128;
                sorted.insert(pos, key);
                moves + displaced
            }
        })
}

thread_local! {
    static COMPARES: Cell<u128> = const { Cell::new(0) };
}

#[derive(PartialEq, Eq)]
struct CountedKey(Key);

impl PartialOrd for CountedKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CountedKey {
    fn cmp(&self, other: &Self) -> Ordering {
        COMPARES.with(|c| c.set(c.get() + 1));
        self.0.cmp(&other.0)
    }
}

fn btree_compare_work(keys: &[Key]) -> u128 {
    COMPARES.with(|c| c.set(0));
    let mut tree: BTreeSet<CountedKey> = BTreeSet::new();
    keys.iter().for_each(|&key| {
        tree.insert(CountedKey(key));
    });
    COMPARES.with(Cell::get)
}

fn doubling_ratios(work: &[u128]) -> Vec<f64> {
    work.windows(2).map(|w| w[1] as f64 / w[0] as f64).collect()
}

#[test]
fn sorted_vec_build_is_quadratic_while_btree_is_quasilinear() {
    let sizes = [2_048u32, 4_096, 8_192, 16_384];
    let measured: Vec<(u128, u128)> = sizes
        .iter()
        .map(|&n| {
            let keys = adversarial_keys(n);
            (vec_shift_work(&keys), btree_compare_work(&keys))
        })
        .collect();

    let vec_work: Vec<u128> = measured.iter().map(|m| m.0).collect();
    let btree_work: Vec<u128> = measured.iter().map(|m| m.1).collect();

    eprintln!("{:<8} {:>16} {:>16}", "N", "vec_shifts", "btree_compares");
    sizes.iter().enumerate().for_each(|(i, n)| {
        eprintln!("{:<8} {:>16} {:>16}", n, vec_work[i], btree_work[i]);
    });

    let vec_ratios = doubling_ratios(&vec_work);
    let btree_ratios = doubling_ratios(&btree_work);
    eprintln!("vec doubling ratios:   {vec_ratios:?}");
    eprintln!("btree doubling ratios: {btree_ratios:?}");

    vec_ratios.iter().for_each(|&ratio| {
        assert!(
            (3.5..=4.5).contains(&ratio),
            "sorted-vec build must quadruple per doubling of N, proving O(n^2), got {ratio:.2}x"
        );
    });

    btree_ratios.iter().for_each(|&ratio| {
        assert!(
            ratio < 3.0,
            "btree build must stay near a doubling per doubling of N, proving sub-quadratic, got {ratio:.2}x"
        );
    });

    let advantage: Vec<f64> = (0..sizes.len())
        .map(|i| vec_work[i] as f64 / btree_work[i] as f64)
        .collect();
    advantage.windows(2).for_each(|w| {
        assert!(
            w[1] > w[0],
            "btree's advantage over the sorted vec must widen as N grows, got {advantage:?}"
        );
    });
}

fn vec_remove_work(keys: &[Key]) -> u128 {
    let mut sorted: Vec<Key> = keys.to_vec();
    sorted.sort_unstable();
    keys.iter()
        .fold(0u128, |moves, key| match sorted.binary_search(key) {
            Ok(pos) => {
                let displaced = (sorted.len() - pos - 1) as u128;
                sorted.remove(pos);
                moves + displaced
            }
            Err(_) => moves,
        })
}

fn btree_remove_work(keys: &[Key]) -> u128 {
    let mut tree: BTreeSet<CountedKey> = keys.iter().map(|&k| CountedKey(k)).collect();
    COMPARES.with(|c| c.set(0));
    keys.iter().for_each(|&k| {
        tree.remove(&CountedKey(k));
    });
    COMPARES.with(Cell::get)
}

fn sample_cursors(keys: &[Key]) -> Vec<Key> {
    let mut sorted = keys.to_vec();
    sorted.sort_unstable();
    let step = (sorted.len() / 256).max(1);
    sorted.iter().step_by(step).copied().collect()
}

fn vec_seek_work(keys: &[Key], cursors: &[Key]) -> u128 {
    let mut sorted: Vec<CountedKey> = keys.iter().map(|&k| CountedKey(k)).collect();
    sorted.sort_unstable();
    COMPARES.with(|c| c.set(0));
    cursors.iter().for_each(|&cur| {
        let _ = sorted.partition_point(|k| *k <= CountedKey(cur));
    });
    COMPARES.with(Cell::get)
}

fn btree_seek_work(keys: &[Key], cursors: &[Key]) -> u128 {
    let tree: BTreeSet<CountedKey> = keys.iter().map(|&k| CountedKey(k)).collect();
    COMPARES.with(|c| c.set(0));
    cursors.iter().for_each(|&cur| {
        let _ = tree
            .range((Bound::Excluded(CountedKey(cur)), Bound::Unbounded))
            .next();
    });
    COMPARES.with(Cell::get)
}

#[test]
fn sorted_vec_teardown_is_quadratic_while_btree_is_quasilinear() {
    let sizes = [2_048u32, 4_096, 8_192, 16_384];
    let measured: Vec<(u128, u128)> = sizes
        .iter()
        .map(|&n| {
            let keys = adversarial_keys(n);
            (vec_remove_work(&keys), btree_remove_work(&keys))
        })
        .collect();

    let vec_ratios = doubling_ratios(&measured.iter().map(|m| m.0).collect::<Vec<_>>());
    let btree_ratios = doubling_ratios(&measured.iter().map(|m| m.1).collect::<Vec<_>>());
    eprintln!("teardown vec doubling ratios:   {vec_ratios:?}");
    eprintln!("teardown btree doubling ratios: {btree_ratios:?}");

    vec_ratios.iter().for_each(|&ratio| {
        assert!(
            (3.5..=4.5).contains(&ratio),
            "sorted-vec teardown must quadruple per doubling, proving O(n^2), got {ratio:.2}x"
        );
    });
    btree_ratios.iter().for_each(|&ratio| {
        assert!(
            ratio < 3.0,
            "btree teardown must stay sub-quadratic, got {ratio:.2}x"
        );
    });
}

#[test]
fn cursor_seek_is_sublinear_for_both_representations() {
    let sizes = [2_048u32, 4_096, 8_192, 16_384];
    let measured: Vec<(u128, u128)> = sizes
        .iter()
        .map(|&n| {
            let keys = adversarial_keys(n);
            let cursors = sample_cursors(&keys);
            (
                vec_seek_work(&keys, &cursors),
                btree_seek_work(&keys, &cursors),
            )
        })
        .collect();

    let vec_ratios = doubling_ratios(&measured.iter().map(|m| m.0).collect::<Vec<_>>());
    let btree_ratios = doubling_ratios(&measured.iter().map(|m| m.1).collect::<Vec<_>>());
    eprintln!("seek vec doubling ratios:   {vec_ratios:?}");
    eprintln!("seek btree doubling ratios: {btree_ratios:?}");

    vec_ratios
        .iter()
        .chain(btree_ratios.iter())
        .for_each(|&ratio| {
            assert!(
                ratio < 1.8,
                "a fixed cursor sample must seek in logarithmic work as N grows, got {ratio:.2}x"
            );
        });
}
