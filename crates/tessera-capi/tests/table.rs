//! The table over Datum key columns, against the dense path, and through
//! the C entry points called as C would call them.

use std::ptr;

use anyhow::Result;
use tessera_capi::c::{
    Code, DatumColumn, Mask, Status, TableKey, TableRecord, TableStats, tess_int4_hash,
    tess_table_attach, tess_table_create, tess_table_format_version, tess_table_insert,
    tess_table_layout, tess_table_next_match, tess_table_probe, tess_table_record, tess_table_size,
    tess_table_stats,
};
use tessera_core::{ColumnView, RowMask, RowMaskView};
use tessera_kernels::int32::{self, NullKeys};
use tessera_kernels::table::{KeyKind, TableConfig, TableMut, region_size};

/// int4 keys with every fifth row NULL and every value twice: as Datum
/// words with flags, and as dense values with a non-NULL mask.
struct Keys {
    datums: Vec<u64>,
    isnull: Vec<bool>,
    values: Vec<i32>,
    non_nulls: Vec<u64>,
}

impl Keys {
    fn new(nrows: usize) -> Self {
        let values: Vec<i32> = (0..nrows)
            .map(|row| (row as i32).wrapping_mul(7919) % (nrows as i32 / 2).max(1) - 25)
            .collect();
        let datums = values
            .iter()
            .map(|&value| i64::from(value) as u64)
            .collect();
        let isnull: Vec<bool> = (0..nrows).map(|row| row % 5 == 0).collect();
        let mut non_nulls = vec![0; nrows.div_ceil(64)];
        for (row, &null) in isnull.iter().enumerate() {
            if !null {
                non_nulls[row / 64] |= 1 << (row % 64);
            }
        }
        Self {
            datums,
            isnull,
            values,
            non_nulls,
        }
    }

    fn nrows(&self) -> usize {
        self.values.len()
    }

    fn column(&self) -> DatumColumn {
        DatumColumn {
            struct_size: size_of::<DatumColumn>(),
            values: self.datums.as_ptr(),
            isnull: self.isnull.as_ptr(),
            nrows: self.nrows() as i32,
        }
    }

    fn dense(&self) -> Result<ColumnView<'_, i32>> {
        ColumnView::try_new(
            &self.values,
            Some(RowMaskView::try_new(self.nrows(), &self.non_nulls)?),
        )
    }

    fn all_rows(&self) -> Vec<u64> {
        let mut words = vec![u64::MAX; self.nrows().div_ceil(64)];
        if !self.nrows().is_multiple_of(64) {
            *words.last_mut().unwrap() = (1 << (self.nrows() % 64)) - 1;
        }
        words
    }

    /// Hashes and the valid mask under a NULL policy, by the dense kernel.
    fn hashes(&self, nulls: NullKeys) -> Result<(Vec<u32>, Vec<u64>)> {
        let mut hashes = vec![0; self.nrows()];
        let mut valid_words = self.all_rows();
        let rows_words = self.all_rows();
        let rows = RowMaskView::try_new(self.nrows(), &rows_words)?;
        let mut valid = RowMask::try_new(self.nrows(), &mut valid_words)?;
        int32::hash(&self.dense()?, &rows, nulls, &mut hashes, &mut valid)?;
        Ok((hashes, valid_words))
    }
}

const CONFIG: TableConfig<'static> = TableConfig {
    keys: &[KeyKind::Int32],
    payload_size: 8,
};

/// Insert every valid row of a batch and probe it back: the offsets, the
/// found words and the matches.
fn round_trip<K: tessera_kernels::table::KeySource + ?Sized>(
    keys: &K,
    hashes: &[u32],
    valid: &[u64],
) -> Result<(Vec<u32>, Vec<u64>, Vec<u32>)> {
    let nrows = hashes.len();
    let mut words = vec![0; region_size(&CONFIG, nrows as u64)?.div_ceil(8)];
    let table = TableMut::create_in(&mut words, &CONFIG, nrows as u64)?;
    let mut pending_words = valid.to_vec();
    let mut pending = RowMask::try_new(nrows, &mut pending_words)?;
    let mut offsets = vec![0; nrows];
    table.insert(hashes, keys, None, &mut pending, &mut offsets)?;
    assert_eq!(pending.as_view().selected_count(), 0);
    let rows = RowMaskView::try_new(nrows, valid)?;
    let mut found_words = vec![0; nrows.div_ceil(64)];
    let mut found = RowMask::try_new(nrows, &mut found_words)?;
    let mut matches = vec![0; nrows];
    table.probe(hashes, keys, &rows, &mut matches, &mut found)?;
    Ok((offsets, found_words, matches))
}

#[test]
fn datum_and_dense_keys_build_the_same_table() -> Result<()> {
    for nrows in [1, 64, 130] {
        let keys = Keys::new(nrows);
        for nulls in [NullKeys::Reject, NullKeys::Group] {
            let (hashes, valid) = keys.hashes(nulls)?;
            let column = keys.column();
            // SAFETY: the buffers hold `nrows` initialized values and flags
            // that outlive the column.
            let datum = [unsafe { column.int32(None) }?];
            let dense = [keys.dense()?];
            let from_datum = round_trip(&datum[..], &hashes, &valid)?;
            let from_dense = round_trip(&dense[..], &hashes, &valid)?;
            assert_eq!(from_datum, from_dense, "{nrows} rows under {nulls:?}");
            let (offsets, found, matches) = from_dense;
            assert_eq!(found, valid, "every valid row is found");
            let valid_view = RowMaskView::try_new(nrows, &valid)?;
            for row in valid_view.selected_indices() {
                let twin = offsets.iter().enumerate().any(|(other, &offset)| {
                    offset == matches[row]
                        && valid_view.contains(other).unwrap()
                        && keys.isnull[other] == keys.isnull[row]
                        && (keys.isnull[row] || keys.values[other] == keys.values[row])
                });
                assert!(twin, "row {row} matches a record of its own key");
            }
        }
    }
    Ok(())
}

#[test]
fn the_layout_probes_match_the_types() {
    assert_eq!(tess_table_format_version(), 1);
    assert_eq!(tess_table_layout(0), 96);
    assert_eq!(tess_table_layout(1), 8);
    assert_eq!(tess_table_layout(2), size_of::<TableKey>());
    assert_eq!(tess_table_layout(3), 16);
    assert_eq!(tess_table_layout(4), size_of::<TableStats>());
    assert_eq!(tess_table_layout(5), 32);
    assert_eq!(tess_table_layout(6), size_of::<TableRecord>());
    assert_eq!(tess_table_layout(7), 24);
    assert_eq!(tess_table_layout(8), 0);
}

#[test]
fn the_entry_points_round_trip() -> Result<()> {
    let keys = Keys::new(100);
    let column = keys.column();
    let key = TableKey {
        kind: 1,
        column: &raw const column,
        prepared: ptr::null(),
    };
    let kinds = [1_u32];
    let mut status = Status::new();
    let mut size = 0;
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        let code = tess_table_size(1, kinds.as_ptr(), 8, 100, &raw mut size, &raw mut status);
        assert_eq!((code, status.code), (Code::Ok, Code::Ok));
        assert_eq!(size, region_size(&CONFIG, 100)?);
        let mut region = vec![0_u64; size / 8];
        let base = region.as_mut_ptr().cast::<u8>();
        let code = tess_table_create(base, size, 1, kinds.as_ptr(), 8, 100, &raw mut status);
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(tess_table_attach(base, size, &raw mut status), Code::Ok);

        // Hashes under the reject policy: NULL rows leave the valid mask.
        let mut hashes = vec![0; 100];
        let mut rows_words = keys.all_rows();
        let mut valid_words = keys.all_rows();
        let rows = Mask {
            nrows: 100,
            bits: rows_words.as_mut_ptr(),
        };
        let mut valid = Mask {
            nrows: 100,
            bits: valid_words.as_mut_ptr(),
        };
        let code = tess_int4_hash(
            &raw const column,
            ptr::null(),
            &raw const rows,
            0,
            hashes.as_mut_ptr(),
            &raw mut valid,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok);
        let valid_rows: Vec<usize> = RowMaskView::try_new(100, &valid_words)?
            .selected_indices()
            .collect();
        assert_eq!(valid_rows.len(), 80);

        // Insertion empties the pending mask and counts the records.
        let payload: Vec<u8> = (0..100_u64).flat_map(|row| row.to_ne_bytes()).collect();
        let mut pending_words = valid_words.clone();
        let mut pending = Mask {
            nrows: 100,
            bits: pending_words.as_mut_ptr(),
        };
        let mut offsets = vec![0; 100];
        let code = tess_table_insert(
            base,
            size,
            hashes.as_ptr(),
            1,
            &raw const key,
            payload.as_ptr(),
            &raw mut pending,
            offsets.as_mut_ptr(),
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(pending_words, vec![0; 2]);
        let mut stats = TableStats {
            struct_size: size_of::<TableStats>(),
            records: 0,
            buckets: 0,
            bytes_used: 0,
            region_len: 0,
        };
        assert_eq!(
            tess_table_stats(base, size, &raw mut stats, &raw mut status),
            Code::Ok
        );
        assert_eq!((stats.records, stats.buckets), (80, 1024));
        assert_eq!(stats.region_len, size as u64);
        assert_eq!(stats.bytes_used, 96 + 80 * 32 + 4096);

        // Every valid row is found, at its own record or its twin's.
        let mut found_words = vec![0; 2];
        let mut found = Mask {
            nrows: 100,
            bits: found_words.as_mut_ptr(),
        };
        let mut matches = vec![0; 100];
        let valid_view = Mask {
            nrows: 100,
            bits: valid_words.as_mut_ptr(),
        };
        let code = tess_table_probe(
            base,
            size,
            hashes.as_ptr(),
            1,
            &raw const key,
            &raw const valid_view,
            matches.as_mut_ptr(),
            &raw mut found,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(found_words, valid_words);
        let mut record = TableRecord {
            struct_size: size_of::<TableRecord>(),
            hash: 0,
            null_bits: 0,
            keys: ptr::null(),
            payload: ptr::null(),
            payload_size: 0,
        };
        for &row in &valid_rows {
            let code =
                tess_table_record(base, size, matches[row], &raw mut record, &raw mut status);
            assert_eq!(code, Code::Ok);
            assert_eq!((record.hash, record.null_bits), (hashes[row], 0));
            assert_eq!(*record.keys, i64::from(keys.values[row]));
            assert_eq!(record.payload_size, 8);
            let stored = u64::from_ne_bytes(*record.payload.cast::<[u8; 8]>());
            assert_eq!(keys.values[stored as usize], keys.values[row]);
        }

        // The second record of each key, in place; then no third.
        let mut chain = matches.clone();
        let code = tess_table_next_match(
            base,
            size,
            chain.as_mut_ptr(),
            &raw const valid_view,
            &raw mut found,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        let twins: Vec<usize> = RowMaskView::try_new(100, &found_words)?
            .selected_indices()
            .collect();
        let expected: Vec<usize> = valid_rows
            .iter()
            .copied()
            .filter(|&row| {
                valid_rows
                    .iter()
                    .any(|&other| other != row && keys.values[other] == keys.values[row])
            })
            .collect();
        assert_eq!(twins, expected);
        for &row in &twins {
            assert_ne!(chain[row], matches[row]);
        }
        let mut twin_words = found_words.clone();
        let twin_rows = Mask {
            nrows: 100,
            bits: twin_words.as_mut_ptr(),
        };
        let code = tess_table_next_match(
            base,
            size,
            chain.as_mut_ptr(),
            &raw const twin_rows,
            &raw mut found,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok);
        assert_eq!(found_words, vec![0; 2], "no key has three records");

        // Errors: a null or foreign region, a short length, the wrong key
        // count or kind, an undersized structure, a corrupt version.
        let invalid = Code::InvalidArgument;
        assert_eq!(
            tess_table_attach(ptr::null(), size, &raw mut status),
            invalid
        );
        assert_eq!(tess_table_attach(base, size - 8, &raw mut status), invalid);
        assert_eq!(
            tess_table_attach(base.add(8), size - 8, &raw mut status),
            invalid
        );
        assert!(status.message().contains("does not hold a table"));
        let two = [key_copy(&key), key_copy(&key)];
        let code = tess_table_probe(
            base,
            size,
            hashes.as_ptr(),
            2,
            two.as_ptr(),
            &raw const valid_view,
            matches.as_mut_ptr(),
            &raw mut found,
            &raw mut status,
        );
        assert_eq!(code, invalid);
        assert!(status.message().contains("keys"), "{}", status.message());
        let odd = TableKey {
            kind: 3,
            column: &raw const column,
            prepared: ptr::null(),
        };
        let code = tess_table_probe(
            base,
            size,
            hashes.as_ptr(),
            1,
            &raw const odd,
            &raw const valid_view,
            matches.as_mut_ptr(),
            &raw mut found,
            &raw mut status,
        );
        assert_eq!(code, invalid);
        let bad_kinds = [3_u32];
        let code = tess_table_size(1, bad_kinds.as_ptr(), 8, 1, &raw mut size, &raw mut status);
        assert_eq!(code, invalid);
        stats.struct_size = 8;
        assert_eq!(
            tess_table_stats(base, size, &raw mut stats, &raw mut status),
            invalid
        );
        record.struct_size = 16;
        assert_eq!(
            tess_table_record(base, size, matches[1], &raw mut record, &raw mut status),
            invalid
        );
        assert_eq!(
            tess_table_record(base, size, 1, &raw mut record, &raw mut status),
            invalid
        );
        let version = base.add(8).cast::<u32>();
        version.write_unaligned(2);
        assert_eq!(tess_table_attach(base, size, &raw mut status), invalid);
        assert!(
            status.message().contains("version 2"),
            "{}",
            status.message()
        );
        version.write_unaligned(1);
        assert_eq!(tess_table_attach(base, size, &raw mut status), Code::Ok);
        drop(region);
    }
    Ok(())
}

fn key_copy(key: &TableKey) -> TableKey {
    TableKey {
        kind: key.kind,
        column: key.column,
        prepared: key.prepared,
    }
}
