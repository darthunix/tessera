//! The Bloom filter of a table's hashes.

use anyhow::Result;

use super::*;
use crate::table::testing::{Column, Keys};

/// A table of `count` int4 keys 0, 3, 6, ... and a filter of its records.
fn filtered_table(count: usize) -> Result<(OwnedTable, Vec<u64>)> {
    let keys: Vec<i32> = (0..count as i32).map(|key| key * 3).collect();
    let hashes: Vec<u32> = keys.iter().map(|&key| hash_i32(key)).collect();
    let mut table = local(&ONE_INT4, count.max(1) as u64)?;
    let mut filter = vec![0; bloom::words_for(count as u64)?];
    if count > 0 {
        let column = [Column::new(&keys, None)];
        resolve_all(&mut table, &hashes, &Keys(&column))?;
    }
    table.table()?.bloom(&mut filter)?;
    Ok((table, filter))
}

fn probe_filter(filter: &[u64], hashes: &[u32]) -> Result<usize> {
    let mut rows = all_rows(hashes.len());
    let left = bloom::probe(
        filter,
        hashes,
        &mut RowMask::try_new(hashes.len(), &mut rows)?,
    )?;
    let kept: u32 = rows.iter().map(|word| word.count_ones()).sum();
    assert_eq!(left, kept as usize, "the call counts the rows it keeps");
    Ok(left)
}

#[test]
fn a_filter_lets_every_key_through_and_few_others() -> Result<()> {
    let count = 5000;
    let (table, filter) = filtered_table(count)?;
    assert!(table.chunks() > 1, "the records span chunks");
    assert_eq!(filter.len(), (count * 16).div_ceil(64).next_power_of_two());
    let present: Vec<u32> = (0..count as i32).map(|key| hash_i32(key * 3)).collect();
    assert_eq!(
        probe_filter(&filter, &present)?,
        count,
        "no key of the table is rejected"
    );
    // Keys 1, 4, 7, ... are absent: about one in a hundred gets through.
    let absent: Vec<u32> = (0..100_000).map(|key| hash_i32(key * 3 + 1)).collect();
    let through = probe_filter(&filter, &absent)?;
    assert!(through < 3000, "{through} of 100000 absent keys passed");
    Ok(())
}

/// The word and the four bits of a hash, as the spec writes the rule: the
/// hash times 0x9E3779B97F4A7C15, the word from the top bits of the
/// product, the bits from its low 24, six each.
fn bits_of(hash: u32, nwords: usize) -> (usize, u64) {
    let spread = u64::from(hash).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let k = nwords.trailing_zeros();
    let word = if k == 0 {
        0
    } else {
        (spread >> (64 - k)) as usize
    };
    let mask = (0..4).fold(0, |mask, bit| mask | 1 << ((spread >> (6 * bit)) & 63));
    (word, mask)
}

/// One hash in an empty filter sets exactly the bits the rule gives: the
/// hash then passes, and a hash missing any of them does not.
#[test]
fn a_hash_sets_the_bits_the_rule_gives() -> Result<()> {
    let one = all_rows(1);
    let rows = RowMaskView::try_new(1, &one)?;
    for nwords in [1, 2, 1024] {
        for hash in [0, 1, 0x3C27, 0xDEAD_BEEF, u32::MAX] {
            let mut filter = vec![0; nwords];
            bloom::add(&mut filter, &[hash], &rows)?;
            let (word, mask) = bits_of(hash, nwords);
            let mut expected = vec![0; nwords];
            expected[word] = mask;
            assert_eq!(filter, expected, "{hash:#x} in {nwords} words");
            assert_eq!(probe_filter(&filter, &[hash])?, 1);
            for bit in (0..64).filter(|bit| mask >> bit & 1 == 1) {
                let mut cleared = filter.clone();
                cleared[word] &= !(1 << bit);
                assert_eq!(probe_filter(&cleared, &[hash])?, 0, "bit {bit} clear");
            }
        }
    }
    Ok(())
}

/// A probe narrows its rows in place: the rows not selected stay out, and
/// rows added pass as a table's keys do.
#[test]
fn a_probe_narrows_its_rows_in_place() -> Result<()> {
    let hashes: Vec<u32> = (0..100).map(hash_i32).collect();
    let mut filter = vec![0; bloom::words_for(100)?];
    let all = all_rows(100);
    bloom::add(&mut filter, &hashes, &RowMaskView::try_new(100, &all)?)?;
    let even: Vec<u64> = vec![0x5555_5555_5555_5555, 0x5_5555_5555];
    let mut rows = even.clone();
    assert_eq!(
        bloom::probe(&filter, &hashes, &mut RowMask::try_new(100, &mut rows)?)?,
        50
    );
    assert_eq!(rows, even, "the rows not selected stay out");
    Ok(())
}

/// Words that are not a power of two, and hashes or a mask of another
/// row count, are refused by every call before it changes anything.
#[test]
fn a_filter_of_the_wrong_size_is_refused() -> Result<()> {
    let (table, _) = filtered_table(10)?;
    let hashes: Vec<u32> = (0..4).map(hash_i32).collect();
    let all = all_rows(4);
    let rows = RowMaskView::try_new(4, &all)?;
    for nwords in [0, 3] {
        let mut filter = vec![7; nwords];
        assert!(table.table()?.bloom(&mut filter).is_err(), "fill {nwords}");
        assert!(
            bloom::add(&mut filter, &hashes, &rows).is_err(),
            "add {nwords}"
        );
        let mut found = [9];
        let result = bloom::probe(&filter, &hashes, &mut RowMask::try_new(4, &mut found)?);
        assert!(result.is_err(), "probe {nwords}");
        assert_eq!((filter, found), (vec![7; nwords], [9]), "{nwords} words");
    }
    let mut filter = vec![7; 4];
    assert!(bloom::add(&mut filter, &hashes[..3], &rows).is_err());
    let mut found = [9];
    let result = bloom::probe(&filter, &hashes, &mut RowMask::try_new(5, &mut found)?);
    assert!(result.is_err(), "a mask of another row count");
    let result = bloom::probe(&filter, &hashes[..3], &mut RowMask::try_new(4, &mut found)?);
    assert!(result.is_err(), "hashes of another row count");
    assert_eq!((filter, found), (vec![7; 4], [9]));
    Ok(())
}

/// The words for a count of records: 16 bits a record at least, a power
/// of two, one at least, and halved within a limit while more than an
/// eighth of it.
#[test]
fn the_words_of_a_filter_for_its_records() -> Result<()> {
    for (records, words) in [(0, 1), (4, 1), (5, 2), (5000, 2048), (1 << 20, 1 << 18)] {
        assert_eq!(bloom::words_for(records)?, words, "{records} records");
    }
    // 2048 words are 16 KiB: an eighth of 128 KiB holds them, of 64 KiB
    // half of them, and below 64 bytes one word stays.
    for (limit, words) in [
        (128 << 10, 2048),
        (64 << 10, 1024),
        (127 << 10, 1024),
        (0, 1),
    ] {
        assert_eq!(bloom::words_within(5000, limit)?, words, "within {limit}");
    }
    assert!(bloom::words_for(u64::MAX).is_err());
    assert!(bloom::words_within(u64::MAX, usize::MAX).is_err());
    Ok(())
}

/// At 16 bits a key the filter's false passes are measured, not guessed:
/// 4096 keys fill exactly 1024 words.
#[test]
fn few_absent_keys_pass_a_filter_of_16_bits_a_key() -> Result<()> {
    let (_, filter) = filtered_table(4096)?;
    assert_eq!(filter.len(), 1024, "16 bits a key exactly");
    let absent: Vec<u32> = (0..100_000).map(|key| hash_i32(key * 3 + 1)).collect();
    let through = probe_filter(&filter, &absent)?;
    eprintln!("{through} of 100000 absent keys passed");
    assert!(through < 1000, "{through} of 100000 absent keys passed");
    Ok(())
}

#[test]
fn an_empty_table_rejects_everything_and_sizes_are_checked() -> Result<()> {
    let (_, filter) = filtered_table(0)?;
    assert_eq!(filter.len(), 1);
    let hashes: Vec<u32> = (0..200).map(hash_i32).collect();
    assert_eq!(probe_filter(&filter, &hashes)?, 0);
    // Not a power of two, and hashes of another row count.
    assert!(probe_filter(&[0; 3], &hashes).is_err());
    let mut rows = all_rows(4);
    assert!(bloom::probe(&filter, &[1, 2], &mut RowMask::try_new(4, &mut rows)?).is_err());
    assert!(bloom::words_for(u64::MAX).is_err());
    Ok(())
}

/// A spill's filter within an eighth of its memory, as the join's spill
/// sized it by hand: the words for its records, halved while larger.
#[test]
fn a_spill_filter_stays_within_an_eighth_of_its_memory() -> Result<()> {
    for records in [0, 1, 1000, 4096, 1 << 20, 100_000_000, 1 << 40] {
        for limit in [0, 64, 64 * 1024, 4 << 20, 64 << 20, 1 << 30, usize::MAX] {
            let mut model = bloom::words_for(records)?;
            while model > 1 && 8 * model > limit / 8 {
                model /= 2;
            }
            let words = bloom::words_within(records, limit)?;
            assert_eq!(words, model, "{records} records within {limit} bytes");
            assert!(words.is_power_of_two() && words <= bloom::words_for(records)?);
        }
    }
    assert!(bloom::words_within(u64::MAX, 1 << 30).is_err());
    Ok(())
}

#[test]
fn a_null_group_key_and_int8_keys_pass_their_filter() -> Result<()> {
    // Keys 1, NULL, 2^40, the NULL hashed as a grouping hashes it.
    let config = TableConfig {
        nkeys: 1,
        payload_size: 8,
    };
    let values = [1_i64, 0, 1 << 40];
    let non_null = [0b101_u64];
    let column = [Column::new(&values, Some(&non_null))];
    let hashes = [hash_i64(1), hash_i32(GROUP_KEY), hash_i64(1 << 40)];
    let mut table = local(&config, 4)?;
    let mut filter = vec![0; bloom::words_for(3)?];
    resolve_all(&mut table, &hashes, &Keys(&column))?;
    table.table()?.bloom(&mut filter)?;
    assert_eq!(probe_filter(&filter, &hashes)?, 3);
    Ok(())
}
