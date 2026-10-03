//! Ordering rows by integer keys, as sort items of whole 64-bit words.
//!
//! A row's keys become one string of bits whose unsigned order is the
//! rows' order: each key, in key order, as its value turned unsigned with
//! the order kept (the sign bit flipped; every bit inverted when the key
//! is descending), after one bit that puts a NULL first or last when the
//! key may be NULL. The row's 32-bit reference follows in the last word's
//! low bits, so that equal keys stay distinct and the sorted items give
//! the references back in order. An item is as many words as the bits
//! need ([`item_words`]): one for an int4 key, two for an int8 one or up
//! to three int4 keys, and the items sort as arrays of words, the first
//! word most significant, with the standard library's unstable sort. A
//! new type of key needs only its transform into bits with the order
//! kept.
//!
//! The kernels allocate nothing: the caller hands the items' words and
//! the output of references in.

use anyhow::{Context, Result, bail, ensure};

use tessera_core::{RowMask, RowMaskView};

use crate::table::{KeyKind, KeySource, MAX_KEYS};

/// Bits of a record reference in an item.
pub const REFERENCE_BITS: u32 = 32;

/// The most words an item may take: every key an int8 that may be NULL.
pub const MAX_ITEM_WORDS: usize = (MAX_KEYS * 65 + REFERENCE_BITS as usize).div_ceil(64);

/// How rows are ordered by one key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SortKey {
    /// The key's kind, as the records hold it.
    pub kind: KeyKind,
    /// Larger values first.
    pub descending: bool,
    /// NULL before every value, rather than after.
    pub nulls_first: bool,
    /// Whether a row may hold NULL in this key; a key that may not takes
    /// no bit for it, and a NULL there is an error.
    pub nullable: bool,
}

impl SortKey {
    fn value_bits(&self) -> u32 {
        match self.kind {
            KeyKind::Int32 => 32,
            KeyKind::Int64 => 64,
        }
    }

    fn bits(&self) -> u32 {
        self.value_bits() + u32::from(self.nullable)
    }

    /// The key's value as bits whose unsigned order is the key's order.
    #[inline(always)]
    fn encode(&self, slot: i64) -> u64 {
        match self.kind {
            KeyKind::Int32 => {
                let bits = u64::from((slot as i32 as u32) ^ 0x8000_0000);
                if self.descending {
                    !bits & 0xFFFF_FFFF
                } else {
                    bits
                }
            }
            KeyKind::Int64 => {
                let bits = (slot as u64) ^ (1 << 63);
                if self.descending { !bits } else { bits }
            }
        }
    }
}

/// The words of one item for `keys`: their bits and the reference's.
pub fn item_words(keys: &[SortKey]) -> Result<usize> {
    ensure!(
        (1..=MAX_KEYS).contains(&keys.len()),
        "a sort takes 1 to {MAX_KEYS} keys, not {}",
        keys.len()
    );
    let bits: u32 = keys.iter().map(SortKey::bits).sum::<u32>() + REFERENCE_BITS;
    Ok(bits.div_ceil(64) as usize)
}

/// The words of `keys` alone, without the reference: what a run of an
/// external sort keeps of each item, and a merge compares. The last word
/// of an item goes when it holds the reference's bits only.
pub fn run_words(keys: &[SortKey]) -> Result<usize> {
    item_words(keys)?;
    let bits: u32 = keys.iter().map(SortKey::bits).sum();
    Ok(bits.div_ceil(64) as usize)
}

/// Writes items: one row's keys from the first word's high bits down.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Encoder<'k> {
    pub(crate) keys: &'k [SortKey],
    words: usize,
}

impl<'k> Encoder<'k> {
    pub(crate) fn new(keys: &'k [SortKey]) -> Result<Self> {
        Ok(Self {
            keys,
            words: item_words(keys)?,
        })
    }

    /// Words of one item.
    pub(crate) fn words(&self) -> usize {
        self.words
    }

    /// The item of a row with key slots `slots`, the keys whose bit is set
    /// in `null_bits` NULL, and reference `reference`: `W` must be
    /// [`Self::words`]. A fixed width keeps the item in registers, with no
    /// call to clear it.
    #[inline(always)]
    pub(crate) fn encode<const W: usize>(
        &self,
        slots: &[i64],
        null_bits: u32,
        reference: u32,
    ) -> Result<[u64; W]> {
        debug_assert_eq!(W, self.words);
        let mut item = [0u64; W];
        let mut at = 0;
        for (index, key) in self.keys.iter().enumerate() {
            let null = (null_bits >> index) & 1 == 1;
            if key.nullable {
                put(&mut item, &mut at, u64::from(null != key.nulls_first), 1);
            } else if null {
                bail!("sort key {index} holds a NULL but was declared not nullable");
            }
            let value = if null { 0 } else { key.encode(slots[index]) };
            put(&mut item, &mut at, value, key.value_bits());
        }
        item[W - 1] |= u64::from(reference);
        Ok(item)
    }
}

/// Write the low `width` bits of `value` at bit `*at`, counted from the
/// first word's most significant bit, and move `*at` past them.
#[inline(always)]
fn put(item: &mut [u64], at: &mut u32, value: u64, width: u32) {
    let word = (*at / 64) as usize;
    let used = *at % 64;
    let room = 64 - used;
    if width <= room {
        item[word] |= value << (room - width);
    } else {
        let low = width - room;
        item[word] |= value >> low;
        item[word + 1] |= value << (64 - low);
    }
    *at += width;
}

/// Sort `items`, items of `words` words each one after another, and write
/// each one's reference in order into `out`, which must hold one per
/// item.
pub fn sort_items(items: &mut [u64], words: usize, out: &mut [u32]) -> Result<()> {
    ensure!(
        (1..=MAX_ITEM_WORDS).contains(&words),
        "a sort item has 1 to {MAX_ITEM_WORDS} words, not {words}"
    );
    ensure!(
        items.len().is_multiple_of(words) && out.len() == items.len() / words,
        "{} words are not {} items of {words} words",
        items.len(),
        out.len()
    );
    macro_rules! dispatch {
        ($($n:literal)*) => {
            match words {
                $($n => sort_as::<$n>(items, out),)*
                words => bail!("a sort item has 1 to {MAX_ITEM_WORDS} words, not {words}"),
            }
        };
    }
    dispatch!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17);
    Ok(())
}

fn sort_as<const W: usize>(items: &mut [u64], out: &mut [u32]) {
    let (items, rest) = items.as_chunks_mut::<W>();
    debug_assert!(rest.is_empty());
    // Two words compare as one u128, without the branches of comparing
    // arrays word by word: 2 M items of 1000 keys sort in 24 ms, not 43
    // (26 when every key differs).
    if W == 2 {
        items.sort_unstable_by_key(|item| (u128::from(item[0]) << 64) | u128::from(item[W - 1]));
    } else {
        items.sort_unstable();
    }
    for (item, reference) in items.iter().zip(out.iter_mut()) {
        *reference = item[W - 1] as u32;
    }
}

/// Top-N: the best `N` rows of an input, as a max-heap of their items in
/// the caller's words, `N` items of [`item_words`] words, the worst on
/// top. Rows are offered in batches: [`top_candidates`] keeps in a batch's
/// mask only the rows whose keys beat the worst item's once the heap is
/// full, and the rows kept, appended as records, are pushed by their
/// references ([`crate::table::Table::top_push`]). [`sort_items`] over the
/// heap's items then gives the best rows in order. A row whose keys equal
/// the worst item's is not taken: which of equal rows a limit returns is
/// not defined.
pub(crate) fn heap_push<const W: usize>(heap: &mut [[u64; W]], len: &mut usize, item: [u64; W]) {
    let capacity = heap.len();
    if *len < capacity {
        let mut at = *len;
        heap[at] = item;
        *len += 1;
        while at > 0 {
            let parent = (at - 1) / 2;
            if heap[parent] >= heap[at] {
                break;
            }
            heap.swap(parent, at);
            at = parent;
        }
        return;
    }
    if capacity == 0 || item >= heap[0] {
        return;
    }
    heap[0] = item;
    let mut at = 0;
    loop {
        let left = 2 * at + 1;
        if left >= capacity {
            break;
        }
        let right = left + 1;
        let larger = if right < capacity && heap[right] > heap[left] {
            right
        } else {
            left
        };
        if heap[at] >= heap[larger] {
            break;
        }
        heap.swap(at, larger);
        at = larger;
    }
}

/// An item's key bits: the reference cleared.
#[inline(always)]
fn key_bits<const W: usize>(mut item: [u64; W]) -> [u64; W] {
    item[W - 1] &= !u64::from(u32::MAX);
    item
}

/// Keep in `rows` only the rows of `source`, a batch's keys, whose keys
/// order strictly before the keys of `worst`, the heap's top item: the
/// candidates of a full top-N heap. Returns the rows kept.
pub fn top_candidates<K: KeySource + ?Sized>(
    keys: &[SortKey],
    source: &K,
    rows: &mut RowMask<'_>,
    worst: &[u64],
) -> Result<usize> {
    let encoder = Encoder::new(keys)?;
    ensure!(
        source.nkeys() == keys.len() && source.nrows() == rows.as_view().nrows(),
        "the batch's keys do not match the sort's keys and rows"
    );
    ensure!(
        worst.len() == encoder.words(),
        "the worst item has {} words, not {}",
        worst.len(),
        encoder.words()
    );
    macro_rules! dispatch {
        ($($n:literal)*) => {
            match encoder.words() {
                $($n => candidates_as::<K, $n>(&encoder, source, rows, worst),)*
                words => bail!("an item has at most 17 words, not {words}"),
            }
        };
    }
    dispatch!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17)
}

fn candidates_as<K: KeySource + ?Sized, const W: usize>(
    encoder: &Encoder<'_>,
    source: &K,
    rows: &mut RowMask<'_>,
    worst: &[u64],
) -> Result<usize> {
    let worst = key_bits::<W>(worst.try_into().context("the width was checked")?);
    let nkeys = encoder.keys.len();
    let nrows = rows.as_view().nrows();
    let mut slots = [[0i64; 64]; MAX_KEYS];
    let mut non_null = [0u64; MAX_KEYS];
    let mut kept = 0;
    for index in 0..nrows.div_ceil(64) {
        let selected = rows.as_view().word_at(index);
        if selected == 0 {
            continue;
        }
        for key in 0..nkeys {
            non_null[key] = source.word(key, index, selected, &mut slots[key])?;
        }
        let mut keep = 0;
        let mut bits = selected;
        while bits != 0 {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let mut row_slots = [0i64; MAX_KEYS];
            let mut null_bits = 0u32;
            for key in 0..nkeys {
                row_slots[key] = slots[key][bit];
                null_bits |= u32::from((non_null[key] >> bit) & 1 == 0) << key;
            }
            let item = encoder.encode::<W>(&row_slots[..nkeys], null_bits, 0)?;
            if key_bits(item) < worst {
                keep |= 1 << bit;
            }
        }
        rows.intersect_word(index, keep)?;
        kept += keep.count_ones() as usize;
    }
    Ok(kept)
}

/// Write the key words of the selected rows of `source`, a batch's keys,
/// in order into `lanes`: lane `w` gets word `w` of each row's item with
/// the reference 0, for the first `lanes.len()` words of the item (a
/// caller may leave out a last word that holds no key's bits), the lanes
/// a merge compares ([`merge`]). Returns the rows written.
pub fn key_lanes<K: KeySource + ?Sized>(
    keys: &[SortKey],
    source: &K,
    rows: &RowMaskView<'_>,
    lanes: &mut [&mut [u64]],
) -> Result<usize> {
    let encoder = Encoder::new(keys)?;
    ensure!(
        source.nkeys() == keys.len() && source.nrows() == rows.nrows(),
        "the batch's keys do not match the sort's keys and rows"
    );
    ensure!(
        (1..=encoder.words()).contains(&lanes.len()),
        "an item of {} words has no {} lanes",
        encoder.words(),
        lanes.len()
    );
    let selected: usize = (0..rows.nrows().div_ceil(64))
        .map(|index| rows.word_at(index).count_ones() as usize)
        .sum();
    ensure!(
        lanes.iter().all(|lane| lane.len() >= selected),
        "the lanes do not hold {selected} rows"
    );
    macro_rules! dispatch {
        ($($n:literal)*) => {
            match encoder.words() {
                $($n => lanes_as::<K, $n>(&encoder, source, rows, lanes),)*
                words => bail!("an item has at most 17 words, not {words}"),
            }
        };
    }
    dispatch!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17)
}

fn lanes_as<K: KeySource + ?Sized, const W: usize>(
    encoder: &Encoder<'_>,
    source: &K,
    rows: &RowMaskView<'_>,
    lanes: &mut [&mut [u64]],
) -> Result<usize> {
    let nkeys = encoder.keys.len();
    let mut slots = [[0i64; 64]; MAX_KEYS];
    let mut non_null = [0u64; MAX_KEYS];
    let mut out = 0;
    for index in 0..rows.nrows().div_ceil(64) {
        let selected = rows.word_at(index);
        if selected == 0 {
            continue;
        }
        for key in 0..nkeys {
            non_null[key] = source.word(key, index, selected, &mut slots[key])?;
        }
        let mut bits = selected;
        while bits != 0 {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let mut row_slots = [0i64; MAX_KEYS];
            let mut null_bits = 0u32;
            for key in 0..nkeys {
                row_slots[key] = slots[key][bit];
                null_bits |= u32::from((non_null[key] >> bit) & 1 == 0) << key;
            }
            let item = encoder.encode::<W>(&row_slots[..nkeys], null_bits, 0)?;
            for (word, lane) in lanes.iter_mut().enumerate() {
                lane[out] = item[word];
            }
            out += 1;
        }
    }
    Ok(out)
}

/// The most runs one merge takes.
pub const MAX_MERGE_RUNS: usize = 256;

/// Words of a merge's state: whether its tree is built, the run whose
/// block ran out last (or none), and a node per run.
pub const MERGE_STATE_WORDS: usize = 2 + MAX_MERGE_RUNS;

/// No run: an empty place of the tree, or no run to replay.
const NO_RUN: u32 = u32::MAX;

/// What [`merge`] did: the rows it put out, and the run whose block it
/// emptied with more blocks to come, which the caller loads before it
/// merges on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Merged {
    pub count: usize,
    pub refill: Option<usize>,
}

/// The heads of the runs as a merge reads them: each run's lanes from its
/// current row on, and the rows taken from each in this call.
struct Heads<'a> {
    words: usize,
    lanes: &'a [&'a [u64]],
    left: &'a [u32],
    at: [u32; MAX_MERGE_RUNS],
}

impl Heads<'_> {
    /// Whether run `a`'s head orders before run `b`'s: a run with no row
    /// left orders last, and equal items in run order. (A head of each run
    /// kept as one u128, compared without branches, was 10 % slower on 2 M
    /// rows in 41 runs.)
    #[inline(always)]
    fn before(&self, a: u32, b: u32) -> bool {
        let (a, b) = (a as usize, b as usize);
        let (at, bt) = (self.at[a] as usize, self.at[b] as usize);
        if at >= self.left[a] as usize {
            return false;
        }
        if bt >= self.left[b] as usize {
            return true;
        }
        match self.words {
            1 => {
                let (x, y) = (self.lanes[a][at], self.lanes[b][bt]);
                x < y || (x == y && a < b)
            }
            2 => {
                let x = (u128::from(self.lanes[a * 2][at]) << 64)
                    | u128::from(self.lanes[a * 2 + 1][at]);
                let y = (u128::from(self.lanes[b * 2][bt]) << 64)
                    | u128::from(self.lanes[b * 2 + 1][bt]);
                x < y || (x == y && a < b)
            }
            words => {
                for word in 0..words {
                    let (x, y) = (
                        self.lanes[a * words + word][at],
                        self.lanes[b * words + word][bt],
                    );
                    if x != y {
                        return x < y;
                    }
                }
                a < b
            }
        }
    }

    /// Whether run `run` has no row left in memory.
    fn done(&self, run: usize) -> bool {
        self.at[run] >= self.left[run]
    }
}

/// Play run `run`'s head up the tree of `runs` leaves from its leaf: at
/// each node the loser stays, the winner goes on and is returned. Leaf
/// `r` is place `runs + r`; node `i`'s children are `2i` and `2i + 1`.
#[inline(always)]
fn replay(tree: &mut [u32], runs: usize, heads: &Heads<'_>, run: u32) -> u32 {
    let mut winner = run;
    let mut node = (run as usize + runs) / 2;
    while node >= 1 {
        if heads.before(tree[node], winner) {
            core::mem::swap(&mut tree[node], &mut winner);
        }
        node /= 2;
    }
    winner
}

/// Build the tree over every run's head: each node keeps the loser of its
/// two sides, and the winner of all is returned.
fn build(tree: &mut [u32], runs: usize, heads: &Heads<'_>) -> u32 {
    let mut winners = [0_u32; MAX_MERGE_RUNS];
    let side = |winners: &[u32; MAX_MERGE_RUNS], child: usize| {
        if child >= runs {
            (child - runs) as u32
        } else {
            winners[child]
        }
    };
    for node in (1..runs).rev() {
        let (a, b) = (side(&winners, 2 * node), side(&winners, 2 * node + 1));
        let (won, lost) = if heads.before(b, a) { (b, a) } else { (a, b) };
        tree[node] = lost;
        winners[node] = won;
    }
    if runs == 1 { 0 } else { winners[1] }
}

/// Merge sorted runs: run `r`'s items are `words` lanes of words,
/// `lanes[r * words + w]` word `w` of its rows from its current one on,
/// `left[r]` of them, and `more[r]` says whether blocks of it follow. The
/// run of each row put out, in order, goes to `out`, as many as it takes
/// or until a run's block is done with more to come; a run's next rows are
/// the ones after those it gave. Items compare whole, the caller leaving
/// out what is no key, such as a reference; equal ones come in run order.
/// `state`,
/// [`MERGE_STATE_WORDS`] words, keeps the tree of runs (a loser tree) from
/// one call to the next; its first word 0 makes it anew.
pub fn merge(
    words: usize,
    lanes: &[&[u64]],
    left: &[u32],
    more: &[bool],
    state: &mut [u32],
    out: &mut [u32],
) -> Result<Merged> {
    let runs = left.len();
    ensure!(
        (1..=MAX_ITEM_WORDS).contains(&words),
        "a sort item has 1 to {MAX_ITEM_WORDS} words, not {words}"
    );
    ensure!(
        (1..=MAX_MERGE_RUNS).contains(&runs) && more.len() == runs && lanes.len() == runs * words,
        "a merge of {runs} runs takes a flag per run and {words} lanes each, 1 to {MAX_MERGE_RUNS} runs"
    );
    ensure!(
        state.len() >= MERGE_STATE_WORDS,
        "a merge's state takes {MERGE_STATE_WORDS} words, not {}",
        state.len()
    );
    for (run, (&rows, &follows)) in left.iter().zip(more).enumerate() {
        ensure!(
            rows > 0 || !follows,
            "run {run} has no rows in memory and more on disk: load them first"
        );
        ensure!(
            lanes[run * words..(run + 1) * words]
                .iter()
                .all(|lane| lane.len() >= rows as usize),
            "run {run} holds fewer rows than it has left"
        );
    }
    let mut heads = Heads {
        words,
        lanes,
        left,
        at: [0; MAX_MERGE_RUNS],
    };
    // state[0]: built; state[1]: the run to replay first; tree[0], the
    // winner; tree[1..runs], the losers.
    let (flags, tree) = state.split_at_mut(2);
    let tree = &mut tree[..MAX_MERGE_RUNS];
    let mut winner = if flags[0] == 0 {
        flags[0] = 1;
        build(tree, runs, &heads)
    } else if flags[1] != NO_RUN {
        // The run whose block ran out has its next rows now.
        replay(tree, runs, &heads, flags[1])
    } else {
        tree[0]
    };
    flags[1] = NO_RUN;
    let mut count = 0;
    while count < out.len() && winner != NO_RUN {
        let run = winner as usize;
        if heads.done(run) {
            break;
        }
        out[count] = winner;
        count += 1;
        heads.at[run] += 1;
        if heads.at[run] == left[run] && more[run] {
            flags[1] = winner;
            tree[0] = winner;
            return Ok(Merged {
                count,
                refill: Some(run),
            });
        }
        winner = replay(tree, runs, &heads, winner);
    }
    tree[0] = winner;
    Ok(Merged {
        count,
        refill: None,
    })
}

#[cfg(test)]
mod tests {
    use proptest::collection::vec;
    use proptest::prelude::*;
    use tessera_testing::property;

    use super::*;

    const ASC: SortKey = SortKey {
        kind: KeyKind::Int32,
        descending: false,
        nulls_first: false,
        nullable: false,
    };

    #[test]
    fn the_widest_item_fits() {
        let keys = [SortKey {
            kind: KeyKind::Int64,
            nullable: true,
            ..ASC
        }; MAX_KEYS];
        assert_eq!(item_words(&keys).unwrap(), MAX_ITEM_WORDS);
        // sort_items dispatches up to 17 words.
        assert_eq!(MAX_ITEM_WORDS, 17);
    }

    #[test]
    fn bits_cross_words() {
        let mut item = [0u64; 2];
        let mut at = 60;
        put(&mut item, &mut at, 0xABCD, 16);
        assert_eq!(item, [0xA, 0xBCD << 52]);
        assert_eq!(at, 76);
    }

    /// Sorted runs of items of `words` words from a few values, many of
    /// them equal; the last word is a key as the node writes it, the
    /// reference's low 32 bits left out.
    fn runs() -> impl Strategy<Value = (usize, Vec<Vec<Vec<u64>>>)> {
        (1..=3_usize).prop_flat_map(|words| {
            let item = vec(0_u64..5, words).prop_map(move |mut item| {
                item[words - 1] <<= 32;
                item
            });
            let run = vec(item, 0..50).prop_map(|mut items| {
                items.sort();
                items
            });
            (Just(words), vec(run, 1..=40))
        })
    }

    /// Runs of items of one to three words, cut into blocks of a few rows,
    /// merge into the order of all the items sorted at once.
    #[test]
    fn runs_merge_into_the_order_of_all_their_items() {
        property(runs(), |(words, runs)| -> Result<()> {
            runs_merge(words, &runs);
            Ok(())
        });
    }

    fn runs_merge(words: usize, runs: &[Vec<Vec<u64>>]) {
        let nruns = runs.len();
        let block = 3;
        let mut state_words = vec![0_u32; MERGE_STATE_WORDS];
        let mut cursor = vec![0_usize; nruns];
        let mut merged: Vec<Vec<u64>> = Vec::new();
        loop {
            // Each run's current block: its lanes from its cursor on.
            let lanes_data: Vec<Vec<u64>> = (0..nruns)
                .flat_map(|run| {
                    let rows = &runs[run];
                    let start = cursor[run] / block * block;
                    let end = (start + block).min(rows.len());
                    let from = cursor[run].min(end);
                    (0..words)
                        .map(move |word| rows[from..end].iter().map(|item| item[word]).collect())
                        .collect::<Vec<Vec<u64>>>()
                })
                .collect();
            let lanes: Vec<&[u64]> = lanes_data.iter().map(Vec::as_slice).collect();
            let left: Vec<u32> = (0..nruns)
                .map(|run| {
                    let end = ((cursor[run] / block + 1) * block).min(runs[run].len());
                    (end - cursor[run].min(end)) as u32
                })
                .collect();
            let more: Vec<bool> = (0..nruns)
                .map(|run| ((cursor[run] / block + 1) * block) < runs[run].len())
                .collect();
            // A run whose block is done moves to its next block.
            if let Some(run) = (0..nruns).find(|&run| left[run] == 0 && more[run]) {
                cursor[run] = (cursor[run] / block + 1) * block;
                let _ = run;
                continue;
            }
            let mut out = [0_u32; 8];
            let done = merge(words, &lanes, &left, &more, &mut state_words, &mut out).unwrap();
            if done.count == 0 {
                break;
            }
            for &run in &out[..done.count] {
                merged.push(runs[run as usize][cursor[run as usize]].clone());
                cursor[run as usize] += 1;
            }
        }
        let mut all: Vec<Vec<u64>> = runs.concat();
        all.sort();
        assert_eq!(merged, all, "{words} words, {nruns} runs");
    }

    #[test]
    fn a_merge_refuses_a_run_to_load_first() {
        let lane = [1_u64];
        let lanes: [&[u64]; 2] = [&lane, &[]];
        let mut out = [0_u32; 4];
        let mut state = [0_u32; MERGE_STATE_WORDS];
        assert!(merge(1, &lanes, &[1, 0], &[false, true], &mut state, &mut out).is_err());
        assert_eq!(
            merge(1, &lanes, &[1, 0], &[false, false], &mut state, &mut out).unwrap(),
            Merged {
                count: 1,
                refill: None
            }
        );
    }
}
