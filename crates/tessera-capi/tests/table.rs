//! The table over Datum key columns, against the dense path, and through
//! the C entry points called as C would call them.

use std::ptr;

use anyhow::Result;
use tessera_capi::c::sort_flags::{DESCENDING, NULLABLE, NULLS_FIRST};
use tessera_capi::c::{
    CSortKey, Code, DatumColumn, Mask, Status, TableKey, TableRecord, TableRef, TableStats,
    TableSumArg, tess_int4_hash, tess_int8_hash, tess_sort, tess_sort_item_words, tess_sort_items,
    tess_sort_layout, tess_table_accumulate, tess_table_accumulate_sums, tess_table_append,
    tess_table_append_columns, tess_table_append_partitioned_columns, tess_table_chunk_init,
    tess_table_create, tess_table_find_or_insert, tess_table_format_version, tess_table_gather,
    tess_table_gather_key, tess_table_layout, tess_table_link, tess_table_link_grouped,
    tess_table_next_in_group, tess_table_next_match, tess_table_payloads, tess_table_probe,
    tess_table_record, tess_table_regrow, tess_table_scan, tess_table_size, tess_table_stats,
};
use tessera_core::{ColumnView, RowMask, RowMaskView};
use tessera_kernels::int32::{self, NullKeys};
use tessera_kernels::table::{CHUNK_HEADER, KeyKind, LocalTable, TableConfig, index_size};

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
            ..DatumColumn::EMPTY
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

/// A table as C holds one: its index and chunks in vectors, handed to the
/// entry points as a `TessTableRef`.
struct CTable {
    index: Vec<u64>,
    chunks: Vec<Vec<u64>>,
    bases: Vec<*mut u8>,
    lens: Vec<usize>,
    linked: Vec<usize>,
    table: TableRef,
    payload_size: usize,
}

impl CTable {
    /// An empty table of one key of `kind` for `capacity` records.
    ///
    /// # Safety
    ///
    /// None beyond the entry points' own: every buffer is local.
    unsafe fn new(kind: u32, payload_size: usize, capacity: u64) -> Self {
        let kinds = [kind];
        let mut status = Status::new();
        let mut size = 0;
        // SAFETY: local buffers of the declared sizes.
        unsafe {
            let code = tess_table_size(
                1,
                kinds.as_ptr(),
                payload_size,
                capacity,
                &raw mut size,
                &raw mut status,
            );
            assert_eq!(code, Code::Ok, "{}", status.message());
            let mut index = vec![0_u64; size / 8];
            let code = tess_table_create(
                index.as_mut_ptr().cast(),
                size,
                1,
                kinds.as_ptr(),
                payload_size,
                capacity,
                &raw mut status,
            );
            assert_eq!(code, Code::Ok, "{}", status.message());
            let mut table = Self {
                index,
                chunks: Vec::new(),
                bases: Vec::new(),
                lens: Vec::new(),
                linked: Vec::new(),
                table: TableRef {
                    index: ptr::null_mut(),
                    index_len: 0,
                    chunks: ptr::null(),
                    chunk_lens: ptr::null(),
                    nchunks: 0,
                },
                payload_size,
            };
            table.refresh();
            table
        }
    }

    fn refresh(&mut self) {
        self.table = TableRef {
            index: self.index.as_mut_ptr().cast(),
            index_len: self.index.len() * 8,
            chunks: self.bases.as_ptr(),
            chunk_lens: self.lens.as_ptr(),
            nchunks: self.bases.len() as i32,
        };
    }

    fn ptr(&self) -> *const TableRef {
        &raw const self.table
    }

    /// Add an empty chunk of `bytes` bytes.
    fn add_chunk(&mut self, bytes: usize) {
        let mut chunk = vec![0_u64; bytes / 8];
        let mut status = Status::new();
        // SAFETY: the vector is the chunk's, aligned to 8 and of its length.
        let code =
            unsafe { tess_table_chunk_init(chunk.as_mut_ptr().cast(), bytes, &raw mut status) };
        assert_eq!(code, Code::Ok, "{}", status.message());
        self.bases.push(chunk.as_mut_ptr().cast());
        self.lens.push(bytes);
        self.chunks.push(chunk);
        self.linked.push(CHUNK_HEADER);
        self.refresh();
    }

    /// Append the pending rows to chunks of `bytes` bytes, adding them as
    /// they fill, then link the new records, grouped or not.
    ///
    /// # Safety
    ///
    /// The arguments as for `tess_table_append`.
    #[allow(clippy::too_many_arguments)]
    unsafe fn insert(
        &mut self,
        bytes: usize,
        hashes: *const u32,
        key: *const TableKey,
        payload: *const u8,
        pending: *mut Mask,
        offsets: *mut u32,
        grouped: bool,
    ) -> u64 {
        let mut status = Status::new();
        // SAFETY: the caller's contract.
        unsafe {
            loop {
                if self.chunks.is_empty() {
                    self.add_chunk(bytes);
                }
                let code = tess_table_append(
                    self.ptr(),
                    self.chunks.len() as i32 - 1,
                    self.payload_size,
                    hashes,
                    1,
                    key,
                    payload,
                    pending,
                    offsets,
                    &raw mut status,
                );
                assert_eq!(code, Code::Ok, "{}", status.message());
                let words = ((*pending).nrows as usize).div_ceil(64);
                if slice_of((*pending).bits, words)
                    .iter()
                    .all(|&word| word == 0)
                {
                    break;
                }
                self.add_chunk(bytes);
            }
            let mut duplicates = 0;
            for chunk in 0..self.chunks.len() {
                let mut from = self.linked[chunk];
                let mut repeated = 0;
                let code = if grouped {
                    tess_table_link_grouped(
                        self.ptr(),
                        chunk as i32,
                        &raw mut from,
                        ptr::null_mut(),
                        &raw mut repeated,
                        &raw mut status,
                    )
                } else {
                    tess_table_link(
                        self.ptr(),
                        chunk as i32,
                        &raw mut from,
                        ptr::null_mut(),
                        &raw mut repeated,
                        &raw mut status,
                    )
                };
                assert_eq!(code, Code::Ok, "{}", status.message());
                self.linked[chunk] = from;
                duplicates += repeated;
            }
            duplicates
        }
    }

    /// Append the pending rows to chunks of `bytes` bytes, adding them as
    /// they fill, without linking them: records a sort reads.
    ///
    /// # Safety
    ///
    /// The arguments as for `tess_table_append`, with no payload.
    unsafe fn insert_unlinked(
        &mut self,
        bytes: usize,
        hashes: *const u32,
        key: *const TableKey,
        pending: *mut Mask,
        offsets: *mut u32,
    ) {
        let mut status = Status::new();
        // SAFETY: the caller's contract.
        unsafe {
            loop {
                if self.chunks.is_empty() {
                    self.add_chunk(bytes);
                }
                let code = tess_table_append(
                    self.ptr(),
                    self.chunks.len() as i32 - 1,
                    self.payload_size,
                    hashes,
                    1,
                    key,
                    ptr::null(),
                    pending,
                    offsets,
                    &raw mut status,
                );
                assert_eq!(code, Code::Ok, "{}", status.message());
                let words = ((*pending).nrows as usize).div_ceil(64);
                if slice_of((*pending).bits, words)
                    .iter()
                    .all(|&word| word == 0)
                {
                    break;
                }
                self.add_chunk(bytes);
            }
        }
    }

    /// Move the table to an index for `capacity` records.
    fn regrow(&mut self, capacity: u64) -> Code {
        let kinds = [1_u32];
        let mut status = Status::new();
        let mut size = 0;
        // SAFETY: local buffers of the declared sizes; the new index takes
        // the old one's place once the call succeeded.
        unsafe {
            let code = tess_table_size(
                1,
                kinds.as_ptr(),
                self.payload_size,
                capacity,
                &raw mut size,
                &raw mut status,
            );
            assert_eq!(code, Code::Ok);
            let mut index = vec![0_u64; size / 8];
            let code = tess_table_regrow(
                self.ptr(),
                index.as_mut_ptr().cast(),
                size,
                capacity,
                &raw mut status,
            );
            if code == Code::Ok {
                self.index = index;
                self.refresh();
            }
            code
        }
    }
}

/// The words of a mask.
///
/// # Safety
///
/// `bits` points to `words` initialized words.
unsafe fn slice_of<'a>(bits: *const u64, words: usize) -> &'a [u64] {
    // SAFETY: the caller's contract.
    unsafe { std::slice::from_raw_parts(bits, words) }
}

/// Whether the reference holds a table of this format, as every call
/// checks: its counts read.
///
/// # Safety
///
/// As for [`tess_table_stats`].
unsafe fn attach(table: *const TableRef, status: *mut Status) -> Code {
    let mut stats = TableStats {
        struct_size: size_of::<TableStats>(),
        records: 0,
        buckets: 0,
        bytes_used: 0,
        region_len: 0,
    };
    // SAFETY: the caller's contract, the counts local.
    unsafe { tess_table_stats(table, &raw mut stats, status) }
}

/// The payload of the record at `offset`, through the call for a batch.
///
/// # Safety
///
/// As for [`tess_table_payloads`] over one row.
unsafe fn payload_of(
    table: *const TableRef,
    offset: u32,
    payload: *mut *mut u8,
    status: *mut Status,
) -> Code {
    let mut word = 1_u64;
    let rows = Mask {
        nrows: 1,
        bits: &raw mut word,
    };
    // SAFETY: the caller's contract, the offset and the mask local.
    unsafe { tess_table_payloads(table, &raw const offset, &raw const rows, payload, status) }
}

/// Insert every valid row of a batch and probe it back: the offsets, the
/// found words and the matches.
fn round_trip<K: tessera_kernels::table::KeySource + ?Sized>(
    keys: &K,
    hashes: &[u32],
    valid: &[u64],
) -> Result<(Vec<u32>, Vec<u64>, Vec<u32>)> {
    let nrows = hashes.len();
    let mut owner = LocalTable::new(&CONFIG, nrows as u64, 4096)?;
    let mut pending_words = valid.to_vec();
    let mut pending = RowMask::try_new(nrows, &mut pending_words)?;
    let mut offsets = vec![0; nrows];
    owner.insert(hashes, keys, None, &mut pending, &mut offsets)?;
    assert_eq!(pending.as_view().selected_count(), 0);
    let table = owner.table()?;
    let rows = RowMaskView::try_new(nrows, valid)?;
    let mut found_words = vec![0; nrows.div_ceil(64)];
    let mut found = RowMask::try_new(nrows, &mut found_words)?;
    let mut matches = vec![0; nrows];
    table.probe(hashes, keys, &rows, &mut matches, &mut found)?;
    Ok((offsets, found_words, matches))
}

#[test]
fn datum_and_dense_keys_build_the_same_table() -> Result<()> {
    // Miri interprets every row; the largest batch only adds time there.
    let sizes: &[usize] = if cfg!(miri) {
        &[1, 64, 65, 130]
    } else {
        &[1, 64, 65, 130, 1024]
    };
    for &nrows in sizes {
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
    assert_eq!(tess_table_layout(8), size_of::<TableRef>());
    assert_eq!(tess_table_layout(9), 32);
    assert_eq!(tess_table_layout(10), 17);
    assert_eq!(tess_table_layout(11), 0);
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
        assert_eq!(size, index_size(&CONFIG, 100)?);
        let mut table = CTable::new(1, 8, 100);
        assert_eq!(attach(table.ptr(), &raw mut status), Code::Ok);

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
        // Chunks of 1 KiB hold 31 records of 32 bytes: three chunks. The
        // links count the rows whose key an earlier valid row had.
        let duplicates = table.insert(
            1024,
            hashes.as_ptr(),
            &raw const key,
            payload.as_ptr(),
            &raw mut pending,
            offsets.as_mut_ptr(),
            false,
        );
        assert_eq!(pending_words, vec![0; 2]);
        assert_eq!(table.chunks.len(), 3);
        let distinct: std::collections::HashSet<i32> =
            valid_rows.iter().map(|&row| keys.values[row]).collect();
        assert_eq!(duplicates, (valid_rows.len() - distinct.len()) as u64);
        let mut stats = TableStats {
            struct_size: size_of::<TableStats>(),
            records: 0,
            buckets: 0,
            bytes_used: 0,
            region_len: 0,
        };
        assert_eq!(
            tess_table_stats(table.ptr(), &raw mut stats, &raw mut status),
            Code::Ok
        );
        assert_eq!((stats.records, stats.buckets), (80, 1024));
        assert_eq!(stats.region_len, size as u64);
        assert_eq!(stats.bytes_used, 96 + 4096);

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
            table.ptr(),
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
                tess_table_record(table.ptr(), matches[row], &raw mut record, &raw mut status);
            assert_eq!(code, Code::Ok);
            assert_eq!((record.hash, record.null_bits), (hashes[row], 0));
            assert_eq!(*record.keys, i64::from(keys.values[row]));
            assert_eq!(record.payload_size, 8);
            let stored = u64::from_ne_bytes(*record.payload.cast::<[u8; 8]>());
            assert_eq!(keys.values[stored as usize], keys.values[row]);
        }

        // The payload word of every match in one call; a word past the
        // payload is refused.
        let mut gathered = vec![u64::MAX; 100];
        let code = tess_table_gather(
            table.ptr(),
            matches.as_ptr(),
            &raw const valid_view,
            0,
            gathered.as_mut_ptr(),
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        for (row, &word) in gathered.iter().enumerate() {
            if valid_rows.contains(&row) {
                assert_eq!(keys.values[word as usize], keys.values[row], "row {row}");
            } else {
                assert_eq!(word, u64::MAX, "row {row}");
            }
        }
        let code = tess_table_gather(
            table.ptr(),
            matches.as_ptr(),
            &raw const valid_view,
            1,
            gathered.as_mut_ptr(),
            &raw mut status,
        );
        assert_eq!(code, Code::InvalidArgument);

        // The second record of each key, in place; then no third.
        let mut chain = matches.clone();
        let code = tess_table_next_match(
            table.ptr(),
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
            table.ptr(),
            chain.as_mut_ptr(),
            &raw const twin_rows,
            &raw mut found,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok);
        assert_eq!(found_words, vec![0; 2], "no key has three records");

        // Errors: a null or foreign index, a short length, a reference
        // past the chunks, the wrong key count or kind, an undersized
        // structure, a corrupt version.
        let invalid = Code::InvalidArgument;
        assert_eq!(attach(ptr::null(), &raw mut status), invalid);
        let base = table.table.index;
        let mut other = TableRef { ..table.table };
        other.index_len = size - 8;
        assert_eq!(attach(&raw const other, &raw mut status), invalid);
        other.index = base.add(8);
        assert_eq!(attach(&raw const other, &raw mut status), invalid);
        assert!(status.message().contains("does not hold a table"));
        other = TableRef { ..table.table };
        other.nchunks = 1;
        let last = valid_rows[valid_rows.len() - 1];
        assert_eq!(
            tess_table_record(
                &raw const other,
                matches[last],
                &raw mut record,
                &raw mut status
            ),
            invalid,
            "the last record lies in the third chunk"
        );
        let two = [key_copy(&key), key_copy(&key)];
        let code = tess_table_probe(
            table.ptr(),
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
            table.ptr(),
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
            tess_table_stats(table.ptr(), &raw mut stats, &raw mut status),
            invalid
        );
        record.struct_size = 16;
        assert_eq!(
            tess_table_record(table.ptr(), matches[1], &raw mut record, &raw mut status),
            invalid
        );
        assert_eq!(
            tess_table_record(table.ptr(), 1, &raw mut record, &raw mut status),
            invalid
        );
        let version = base.add(8).cast::<u32>();
        version.write_unaligned(2);
        assert_eq!(attach(table.ptr(), &raw mut status), invalid);
        assert!(
            status.message().contains("version 2"),
            "{}",
            status.message()
        );
        version.write_unaligned(1);
        assert_eq!(attach(table.ptr(), &raw mut status), Code::Ok);
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

#[test]
fn grouped_states_accumulate_through_the_entry_points() -> Result<()> {
    // Keys row % 10 over 100 rows; the payload is a flags word and a sum.
    let values: Vec<u64> = (0..100_i64).map(|row| (row % 10) as u64).collect();
    let isnull = [false; 100];
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: values.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 100,
        ..DatumColumn::EMPTY
    };
    let key = TableKey {
        kind: 1,
        column: &raw const column,
        prepared: ptr::null(),
    };
    let hashes: Vec<u32> = values
        .iter()
        .map(|&value| int32::murmurhash32(value as u32))
        .collect();
    let mut status = Status::new();
    let mut all = [u64::MAX, (1 << 36) - 1];
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        let mut table = CTable::new(1, 16, 16);
        table.add_chunk(4096);
        let mut pending_words = all;
        let mut pending = Mask {
            nrows: 100,
            bits: pending_words.as_mut_ptr(),
        };
        let mut inserted_words = [0; 2];
        let mut inserted = Mask {
            nrows: 100,
            bits: inserted_words.as_mut_ptr(),
        };
        let mut offsets = vec![0; 100];
        let code = tess_table_find_or_insert(
            table.ptr(),
            0,
            hashes.as_ptr(),
            1,
            &raw const key,
            &raw mut pending,
            offsets.as_mut_ptr(),
            &raw mut inserted,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        let rows = Mask {
            nrows: 100,
            bits: all.as_mut_ptr(),
        };
        // sum(int4) at byte 8, its flag bit 3 in the word at byte 0.
        let code = tess_table_accumulate(
            table.ptr(),
            offsets.as_ptr(),
            &raw const rows,
            3,
            &raw const column,
            ptr::null(),
            8,
            0,
            3,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        let mut first_ten = [0x3ff_u64];
        let groups = Mask {
            nrows: 10,
            bits: first_ten.as_mut_ptr(),
        };
        let mut sums = [0_u64; 10];
        let mut flags = [0_u64; 10];
        let mut keys = [u64::MAX; 10];
        let mut nulls = [true; 10];
        assert_eq!(
            tess_table_gather(
                table.ptr(),
                offsets.as_ptr(),
                &raw const groups,
                8,
                sums.as_mut_ptr(),
                &raw mut status
            ),
            Code::Ok
        );
        assert_eq!(
            tess_table_gather(
                table.ptr(),
                offsets.as_ptr(),
                &raw const groups,
                0,
                flags.as_mut_ptr(),
                &raw mut status
            ),
            Code::Ok
        );
        let code = tess_table_gather_key(
            table.ptr(),
            offsets.as_ptr(),
            &raw const groups,
            0,
            keys.as_mut_ptr(),
            nulls.as_mut_ptr(),
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        for group in 0..10 {
            assert_eq!(keys[group], group as u64);
            assert!(!nulls[group]);
            assert_eq!(sums[group], 10 * group as u64);
            assert_eq!(flags[group], 1 << 3);
        }
        // sum(int8), the merge of partial counts and sums, at byte 8 again
        // with flag bit 4: values past the int4 range, NULL skipped, and a
        // sum past the int8 range refused as the transition would.
        let wide: Vec<u64> = (0..100_i64)
            .map(|row| ((row % 10) * 3_000_000_000) as u64)
            .collect();
        let mut wide_null = [false; 100];
        wide_null[0] = true;
        let wide_column = DatumColumn {
            struct_size: size_of::<DatumColumn>(),
            values: wide.as_ptr(),
            isnull: wide_null.as_ptr(),
            nrows: 100,
            ..DatumColumn::EMPTY
        };
        let code = tess_table_accumulate(
            table.ptr(),
            offsets.as_ptr(),
            &raw const rows,
            8,
            &raw const wide_column,
            ptr::null(),
            8,
            0,
            4,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(
            tess_table_gather(
                table.ptr(),
                offsets.as_ptr(),
                &raw const groups,
                8,
                sums.as_mut_ptr(),
                &raw mut status
            ),
            Code::Ok
        );
        for (group, &sum) in sums.iter().take(10).enumerate() {
            // Bit 4 was clear: the first value replaced the int4 sum there.
            assert_eq!(sum as i64, 10 * group as i64 * 3_000_000_000);
        }
        let huge = vec![i64::MAX as u64; 100];
        let huge_column = DatumColumn {
            struct_size: size_of::<DatumColumn>(),
            values: huge.as_ptr(),
            isnull: isnull.as_ptr(),
            nrows: 100,
            ..DatumColumn::EMPTY
        };
        let code = tess_table_accumulate(
            table.ptr(),
            offsets.as_ptr(),
            &raw const rows,
            8,
            &raw const huge_column,
            ptr::null(),
            8,
            0,
            4,
            &raw mut status,
        );
        assert_eq!(code, Code::IntegerOutOfRange);
        assert_eq!(status.sqlstate(), "22003");
        // An unknown operation is refused.
        let code = tess_table_accumulate(
            table.ptr(),
            offsets.as_ptr(),
            &raw const rows,
            99,
            &raw const column,
            ptr::null(),
            8,
            0,
            3,
            &raw mut status,
        );
        assert_eq!(code, Code::InvalidArgument);
    }
    Ok(())
}

/// Sum states of three kinds through the entry point, as C calls it: a
/// numeric column with its decimals at scale 2 and a NaN, int4 words with
/// a NULL, int8 words with one past 18 digits, which goes to the rest.
#[test]
fn sum_states_accumulate_through_the_entry_point() -> Result<()> {
    use tessera_kernels::decimal::SumState;

    let nrows = 100;
    let keys: Vec<u64> = (0..nrows as u64).map(|row| row % 4).collect();
    let no_nulls = vec![false; nrows];
    let key_column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: keys.as_ptr(),
        isnull: no_nulls.as_ptr(),
        nrows: nrows as i32,
        ..DatumColumn::EMPTY
    };
    let key = TableKey {
        kind: 1,
        column: &raw const key_column,
        prepared: ptr::null(),
    };
    let hashes: Vec<u32> = keys
        .iter()
        .map(|&value| int32::murmurhash32(value as u32))
        .collect();
    // NaN as a short varlena: a 1-byte header of 3 bytes, the header word 0xC000.
    let nan = [7_u8, 0x00, 0xC0, 0];
    let numerics: Vec<u64> = (0..nrows as i64)
        .map(|row| {
            if row == 5 {
                nan.as_ptr() as u64
            } else {
                (row * 25) as u64
            }
        })
        .collect();
    let mut decimal_rows = [u64::MAX, (1 << 36) - 1];
    decimal_rows[0] &= !(1 << 5);
    let numeric_column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: numerics.as_ptr(),
        isnull: no_nulls.as_ptr(),
        nrows: nrows as i32,
        accept_decimals: true,
        decimal_rows: decimal_rows.as_ptr(),
        decimal_scale: 2,
    };
    let int4s: Vec<u64> = (0..nrows as i64)
        .map(|row| u64::from((row as i32 - 50) as u32) | 0xABCD << 40)
        .collect();
    let mut int4_nulls = vec![false; nrows];
    int4_nulls[7] = true;
    let int4_column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: int4s.as_ptr(),
        isnull: int4_nulls.as_ptr(),
        nrows: nrows as i32,
        ..DatumColumn::EMPTY
    };
    let int8s: Vec<u64> = (0..nrows as i64)
        .map(|row| {
            if row == 9 {
                i64::MAX as u64
            } else {
                (row << 33) as u64
            }
        })
        .collect();
    let int8_column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: int8s.as_ptr(),
        isnull: no_nulls.as_ptr(),
        nrows: nrows as i32,
        ..DatumColumn::EMPTY
    };
    let mut status = Status::new();
    let mut all = [u64::MAX, (1 << 36) - 1];
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        let mut table = CTable::new(1, 8 + 32 * 3, 16);
        table.add_chunk(8192);
        let mut pending_words = all;
        let mut pending = Mask {
            nrows: nrows as i32,
            bits: pending_words.as_mut_ptr(),
        };
        let mut inserted_words = [0; 2];
        let mut inserted = Mask {
            nrows: nrows as i32,
            bits: inserted_words.as_mut_ptr(),
        };
        let mut offsets = vec![0; nrows];
        let code = tess_table_find_or_insert(
            table.ptr(),
            0,
            hashes.as_ptr(),
            1,
            &raw const key,
            &raw mut pending,
            offsets.as_mut_ptr(),
            &raw mut inserted,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        let rows = Mask {
            nrows: nrows as i32,
            bits: all.as_mut_ptr(),
        };
        let mut rest_words = [[0_u64; 2]; 3];
        let [first, second, third] = &mut rest_words;
        let mut rests = [first, second, third].map(|words| Mask {
            nrows: nrows as i32,
            bits: words.as_mut_ptr(),
        });
        let sums: Vec<TableSumArg> = [
            (0, &raw const numeric_column),
            (1, &raw const int4_column),
            (2, &raw const int8_column),
        ]
        .into_iter()
        .zip(rests.iter_mut())
        .enumerate()
        .map(|(sum, ((kind, column), rest))| TableSumArg {
            kind,
            column,
            value_at: 8 + 32 * sum,
            rest,
        })
        .collect();
        let code = tess_table_accumulate_sums(
            table.ptr(),
            offsets.as_ptr(),
            &raw const rows,
            3,
            sums.as_ptr(),
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(rest_words[0], [0, 0]);
        assert_eq!(rest_words[1], [0, 0]);
        assert_eq!(rest_words[2], [1 << 9, 0], "the int8 of 19 digits");
        // The first four rows' records are the four groups.
        let mut four = [0xf_u64];
        let groups = Mask {
            nrows: 4,
            bits: four.as_mut_ptr(),
        };
        for sum in 0..3 {
            let mut words = [[0_u64; 4]; 4];
            for (word, out) in words.iter_mut().enumerate() {
                assert_eq!(
                    tess_table_gather(
                        table.ptr(),
                        offsets.as_ptr(),
                        &raw const groups,
                        8 + 32 * sum + 8 * word,
                        out.as_mut_ptr(),
                        &raw mut status
                    ),
                    Code::Ok
                );
            }
            let states: [SumState; 4] = std::array::from_fn(|group| {
                SumState::from_words(std::array::from_fn(|word| words[word][group]))
            });
            for (group, state) in states.iter().enumerate() {
                let rows = (0..nrows as i64).filter(|row| row % 4 == group as i64);
                let (value, count, scale, nan) = match sum {
                    0 => (
                        rows.clone()
                            .filter(|&row| row != 5)
                            .map(|row| row * 25)
                            .sum::<i64>(),
                        rows.clone().filter(|&row| row != 5).count(),
                        2,
                        group == 1,
                    ),
                    1 => (
                        rows.clone()
                            .filter(|&row| row != 7)
                            .map(|row| row - 50)
                            .sum(),
                        rows.clone().filter(|&row| row != 7).count(),
                        0,
                        false,
                    ),
                    _ => (
                        rows.clone()
                            .filter(|&row| row != 9)
                            .map(|row| row << 33)
                            .sum(),
                        rows.clone().filter(|&row| row != 9).count(),
                        0,
                        false,
                    ),
                };
                assert_eq!(
                    (state.sum.value, state.sum.count, state.sum.scale, state.nan),
                    (i128::from(value), count as u64, scale, nan),
                    "sum {sum}, group {group}"
                );
            }
        }
        // More sums than a call takes are refused.
        let code = tess_table_accumulate_sums(
            table.ptr(),
            offsets.as_ptr(),
            &raw const rows,
            33,
            sums.as_ptr(),
            &raw mut status,
        );
        assert_eq!(code, Code::InvalidArgument);
    }
    Ok(())
}

#[test]
fn the_writer_entry_points_round_trip() -> Result<()> {
    let values: Vec<u64> = (0..100_i64).map(|row| (row % 10) as u64).collect();
    let isnull = [false; 100];
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: values.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 100,
        ..DatumColumn::EMPTY
    };
    let key = TableKey {
        kind: 1,
        column: &raw const column,
        prepared: ptr::null(),
    };
    let hashes: Vec<u32> = values
        .iter()
        .map(|&value| int32::murmurhash32(value as u32))
        .collect();
    let kinds = [1_u32];
    let mut status = Status::new();
    let mut size = 0;
    let all = [u64::MAX, (1 << 36) - 1];
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        // An index for eight records, a chunk for four: the chunk fills
        // up, and a walk sees its records in order.
        let mut table = CTable::new(1, 8, 8);
        table.add_chunk(8 + 4 * 32);
        let mut pending_words = all;
        let mut pending = Mask {
            nrows: 100,
            bits: pending_words.as_mut_ptr(),
        };
        let mut inserted_words = [0; 2];
        let mut inserted = Mask {
            nrows: 100,
            bits: inserted_words.as_mut_ptr(),
        };
        let mut offsets = vec![0; 100];
        let code = tess_table_find_or_insert(
            table.ptr(),
            0,
            hashes.as_ptr(),
            1,
            &raw const key,
            &raw mut pending,
            offsets.as_mut_ptr(),
            &raw mut inserted,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(
            pending_words,
            [u64::MAX << 4, (1 << 36) - 1],
            "four keys got records"
        );
        assert_eq!(inserted_words, [0b1111, 0]);
        let mut cursor = 0;
        let mut walked = [0; 8];
        let mut count = 0;
        let code = tess_table_scan(
            table.ptr(),
            &raw mut cursor,
            walked.as_mut_ptr(),
            8,
            &raw mut count,
            &raw mut status,
        );
        assert_eq!((code, count), (Code::Ok, 4));
        assert_eq!(walked[..4], offsets[..4]);
        assert_ne!(cursor, 0);
        let code = tess_table_scan(
            table.ptr(),
            &raw mut cursor,
            walked.as_mut_ptr(),
            8,
            &raw mut count,
            &raw mut status,
        );
        assert_eq!((code, count), (Code::Ok, 0));

        // The records stay where they are under an index for a hundred; in
        // a second chunk the rest resolve to ten records, and payloads
        // change in place.
        assert_eq!(table.regrow(100), Code::Ok);
        let code = tess_table_size(1, kinds.as_ptr(), 8, 100, &raw mut size, &raw mut status);
        assert_eq!(code, Code::Ok);
        let mut again = [0; 4];
        let mut cursor = 0;
        let code = tess_table_scan(
            table.ptr(),
            &raw mut cursor,
            again.as_mut_ptr(),
            4,
            &raw mut count,
            &raw mut status,
        );
        assert_eq!((code, count), (Code::Ok, 4));
        assert_eq!(again, offsets[..4]);
        table.add_chunk(4096);
        let code = tess_table_find_or_insert(
            table.ptr(),
            1,
            hashes.as_ptr(),
            1,
            &raw const key,
            &raw mut pending,
            offsets.as_mut_ptr(),
            &raw mut inserted,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(pending_words, [0, 0]);
        assert_eq!(inserted_words, [0b11_1111_0000, 0], "keys 4 to 9 are new");
        for row in 0..100 {
            assert_eq!(offsets[row], offsets[row % 10]);
            let mut payload: *mut u8 = ptr::null_mut();
            let code = payload_of(table.ptr(), offsets[row], &raw mut payload, &raw mut status);
            assert_eq!(code, Code::Ok);
            let counter = payload.cast::<u64>();
            counter.write_unaligned(counter.read_unaligned() + row as u64);
        }
        let mut record = TableRecord {
            struct_size: size_of::<TableRecord>(),
            hash: 0,
            null_bits: 0,
            keys: ptr::null(),
            payload: ptr::null(),
            payload_size: 0,
        };
        for (key, &offset) in offsets[..10].iter().enumerate() {
            let code = tess_table_record(table.ptr(), offset, &raw mut record, &raw mut status);
            assert_eq!(code, Code::Ok);
            let sum = u64::from_ne_bytes(*record.payload.cast::<[u8; 8]>());
            assert_eq!(
                sum,
                (0..100).filter(|row| row % 10 == key).sum::<usize>() as u64
            );
        }
        let mut stats = TableStats {
            struct_size: size_of::<TableStats>(),
            records: 0,
            buckets: 0,
            bytes_used: 0,
            region_len: 0,
        };
        assert_eq!(
            tess_table_stats(table.ptr(), &raw mut stats, &raw mut status),
            Code::Ok
        );
        assert_eq!((stats.records, stats.region_len), (10, size as u64));
        let mut cursor = 0;
        let mut all_offsets = [0; 16];
        let code = tess_table_scan(
            table.ptr(),
            &raw mut cursor,
            all_offsets.as_mut_ptr(),
            16,
            &raw mut count,
            &raw mut status,
        );
        assert_eq!((code, count), (Code::Ok, 10));
        assert_eq!(all_offsets[..10], offsets[..10]);

        // Errors: a cursor inside a record, an index too short, a bad
        // offset.
        let mut cursor = 100;
        let code = tess_table_scan(
            table.ptr(),
            &raw mut cursor,
            all_offsets.as_mut_ptr(),
            16,
            &raw mut count,
            &raw mut status,
        );
        assert_eq!(code, Code::InvalidArgument);
        let mut short = vec![0_u64; size / 8 - 1];
        let code = tess_table_regrow(
            table.ptr(),
            short.as_mut_ptr().cast(),
            size - 8,
            100,
            &raw mut status,
        );
        assert_eq!(code, Code::InvalidArgument);
        let mut payload: *mut u8 = ptr::null_mut();
        let code = payload_of(
            table.ptr(),
            offsets[0] + 1,
            &raw mut payload,
            &raw mut status,
        );
        assert_eq!(code, Code::InvalidArgument);
    }
    Ok(())
}

#[test]
fn datum_words_normalize_like_rows_under_partial_readiness() -> Result<()> {
    use tessera_kernels::table::normalize_word;
    for nrows in [64, 130] {
        let keys = Keys::new(nrows);
        let column = keys.column();
        // Every row prepared except one in each word: those words go row
        // by row, the fully prepared ones whole.
        let mut prepared = keys.all_rows();
        prepared[0] &= !(1 << 3);
        let prepared_view = RowMaskView::try_new(nrows, &prepared)?;
        // SAFETY: the buffers hold `nrows` initialized values and flags
        // that outlive both readers.
        let (whole, partial) = unsafe { (column.int32(None)?, column.int32(Some(prepared_view))?) };
        for (index, &selected) in prepared.iter().enumerate() {
            let (mut from_whole, mut from_partial) = ([7; 64], [7; 64]);
            let whole_bits = normalize_word(&whole, index, selected, &mut from_whole)?;
            let partial_bits = normalize_word(&partial, index, selected, &mut from_partial)?;
            assert_eq!(whole_bits, partial_bits, "{nrows} rows, word {index}");
            for bit in (0..64).filter(|bit| selected >> bit & 1 != 0) {
                assert_eq!(from_whole[bit], from_partial[bit]);
                let row = index * 64 + bit;
                let expected = if keys.isnull[row] {
                    0
                } else {
                    i64::from(keys.values[row])
                };
                assert_eq!(from_whole[bit], expected, "row {row}");
            }
        }
    }
    Ok(())
}

/// Hash a Datum column of `nrows` rows, none NULL, all selected, with the
/// entry point of its kind: the hashes and the valid mask.
///
/// # Safety
///
/// `column` holds `nrows` Datums of the kind.
unsafe fn hash_column(column: &DatumColumn, kind: u32, nrows: usize) -> (Vec<u32>, Vec<u64>) {
    let mut rows_words = vec![u64::MAX; nrows.div_ceil(64)];
    let rows = Mask {
        nrows: nrows as i32,
        bits: rows_words.as_mut_ptr(),
    };
    let mut hashes = vec![0; nrows];
    let mut valid_words = vec![0; nrows.div_ceil(64)];
    let mut valid = Mask {
        nrows: nrows as i32,
        bits: valid_words.as_mut_ptr(),
    };
    let mut status = Status::new();
    let hash = if kind == 1 {
        tess_int4_hash
    } else {
        tess_int8_hash
    };
    // SAFETY: the caller's contract; local buffers of the declared sizes.
    let code = unsafe {
        hash(
            column,
            ptr::null(),
            &raw const rows,
            0,
            hashes.as_mut_ptr(),
            &raw mut valid,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    (hashes, valid_words)
}

#[test]
fn int4_keys_find_the_records_of_int8_keys() -> Result<()> {
    // Build rows: even rows inside the int4 range, odd rows past it. An odd
    // row's value `row << 33` folds to `2 * row`, the hash of probe row
    // `2 * row + 32`, which must still find only its own record: keys are
    // compared whole.
    let built: Vec<i64> = (0..64_i64)
        .map(|row| if row % 2 == 0 { row - 32 } else { row << 33 })
        .collect();
    let built_datums: Vec<u64> = built.iter().map(|&value| value as u64).collect();
    // Probe rows: int4 values row - 32, so an even row finds its twin.
    let probed_datums: Vec<u64> = (0..64_i64).map(|row| (row - 32) as u64).collect();
    let isnull = [false; 64];
    let column = |datums: &[u64]| DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: datums.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 64,
        ..DatumColumn::EMPTY
    };
    let (built_column, probed_column) = (column(&built_datums), column(&probed_datums));
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else.
    unsafe {
        let mut table = CTable::new(2, 0, 64);

        let (built_hashes, mut pending_words) = hash_column(&built_column, 2, 64);
        let built_key = TableKey {
            kind: 2,
            column: &raw const built_column,
            prepared: ptr::null(),
        };
        let mut pending = Mask {
            nrows: 64,
            bits: pending_words.as_mut_ptr(),
        };
        let mut offsets = vec![0; 64];
        table.insert(
            4096,
            built_hashes.as_ptr(),
            &raw const built_key,
            ptr::null(),
            &raw mut pending,
            offsets.as_mut_ptr(),
            false,
        );
        assert_eq!(pending_words, [0]);

        let (probed_hashes, mut rows_words) = hash_column(&probed_column, 1, 64);
        for row in (0..64).step_by(2) {
            assert_eq!(probed_hashes[row], built_hashes[row], "row {row}");
        }
        let probed_key = TableKey {
            kind: 1,
            column: &raw const probed_column,
            prepared: ptr::null(),
        };
        let rows = Mask {
            nrows: 64,
            bits: rows_words.as_mut_ptr(),
        };
        let mut found_words = [0];
        let mut found = Mask {
            nrows: 64,
            bits: found_words.as_mut_ptr(),
        };
        let mut matches = vec![0; 64];
        let code = tess_table_probe(
            table.ptr(),
            probed_hashes.as_ptr(),
            1,
            &raw const probed_key,
            &raw const rows,
            matches.as_mut_ptr(),
            &raw mut found,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(found_words, [0x5555_5555_5555_5555]);
        for row in (0..64).step_by(2) {
            assert_eq!(matches[row], offsets[row], "row {row}");
        }
    }
    Ok(())
}

#[test]
fn grouped_insertion_steps_through_a_key_in_one_call_each() -> Result<()> {
    // Keys row % 16 over 64 rows: four records per key.
    let datums: Vec<u64> = (0..64_u64).map(|row| row % 16).collect();
    let isnull = [false; 64];
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: datums.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 64,
        ..DatumColumn::EMPTY
    };
    let key = TableKey {
        kind: 1,
        column: &raw const column,
        prepared: ptr::null(),
    };
    let payload: Vec<u8> = (0..64_u64).flat_map(|row| row.to_ne_bytes()).collect();
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else.
    unsafe {
        let mut table = CTable::new(1, 8, 64);
        let (hashes, _) = hash_column(&column, 1, 64);
        let mut pending_words = [u64::MAX];
        let mut pending = Mask {
            nrows: 64,
            bits: pending_words.as_mut_ptr(),
        };
        let mut offsets = vec![0; 64];
        let duplicates = table.insert(
            4096,
            hashes.as_ptr(),
            &raw const key,
            payload.as_ptr(),
            &raw mut pending,
            offsets.as_mut_ptr(),
            true,
        );
        assert_eq!(pending_words, [0]);
        assert_eq!(duplicates, 48, "the first row of each key is new");

        // From each key's first row: three more records, one step each.
        let mut current = offsets.clone();
        let mut rows_words = [0xffff];
        for step in 0..4 {
            let rows = Mask {
                nrows: 64,
                bits: rows_words.as_mut_ptr(),
            };
            let mut found_words = [0];
            let mut found = Mask {
                nrows: 64,
                bits: found_words.as_mut_ptr(),
            };
            let code = tess_table_next_in_group(
                table.ptr(),
                current.as_mut_ptr(),
                &raw const rows,
                &raw mut found,
                &raw mut status,
            );
            assert_eq!(code, Code::Ok, "{}", status.message());
            assert_eq!(
                found_words,
                [if step < 3 { 0xffff } else { 0 }],
                "step {step}"
            );
            rows_words = found_words;
        }
    }
    Ok(())
}

#[test]
fn the_sort_layout_probes_match_the_type() {
    assert_eq!(tess_sort_layout(0), size_of::<CSortKey>());
    assert_eq!(tess_sort_layout(0), 8);
    assert_eq!(tess_sort_layout(1), 4);
    assert_eq!(tess_sort_layout(2), 0);
}

/// Sort the records of a table through the C entry points, every row with
/// its NULL (every fifth) last, and check the keys come back in order: the
/// references are every record's, each once.
#[test]
fn the_sort_entry_points_order_every_record() -> Result<()> {
    let keys = Keys::new(300);
    let column = keys.column();
    let key = TableKey {
        kind: 1,
        column: &raw const column,
        prepared: ptr::null(),
    };
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        let mut table = CTable::new(1, 8, 0);
        let hashes = vec![0_u32; keys.nrows()];
        let mut pending_words = keys.all_rows();
        let mut pending = Mask {
            nrows: keys.nrows() as i32,
            bits: pending_words.as_mut_ptr(),
        };
        let mut offsets = vec![0_u32; keys.nrows()];
        // Chunks of 64 records: the rows take five, and none is linked.
        table.insert_unlinked(
            CHUNK_HEADER + 64 * 32,
            hashes.as_ptr(),
            &raw const key,
            &raw mut pending,
            offsets.as_mut_ptr(),
        );
        for (descending, nulls_first) in [(false, false), (true, true), (true, false)] {
            let flags = NULLABLE
                | if descending { DESCENDING } else { 0 }
                | if nulls_first { NULLS_FIRST } else { 0 };
            let sort_key = CSortKey { kind: 1, flags };
            let mut words = 0;
            let code =
                tess_sort_item_words(1, &raw const sort_key, &raw mut words, &raw mut status);
            assert_eq!(code, Code::Ok, "{}", status.message());
            assert_eq!(
                words, 2,
                "an int4 that may be NULL and a reference take 65 bits"
            );
            let words = words as usize;
            let mut items = vec![0_u64; keys.nrows() * words];
            let mut count = 0;
            let code = tess_sort_items(
                table.ptr(),
                1,
                &raw const sort_key,
                items.as_mut_ptr(),
                items.len(),
                &raw mut count,
                &raw mut status,
            );
            assert_eq!(code, Code::Ok, "{}", status.message());
            assert_eq!(count as usize, keys.nrows());
            let mut refs = vec![0_u32; keys.nrows()];
            let code = tess_sort(
                items.as_mut_ptr(),
                keys.nrows(),
                words as i32,
                refs.as_mut_ptr(),
                &raw mut status,
            );
            assert_eq!(code, Code::Ok, "{}", status.message());
            let mut sorted = refs.clone();
            sorted.sort_unstable();
            let mut all = offsets.clone();
            all.sort_unstable();
            assert_eq!(sorted, all, "every record once");
            // Each reference's row, then its key as the model orders it.
            let order: Vec<Option<i32>> = refs
                .iter()
                .map(|reference| {
                    let row = offsets
                        .iter()
                        .position(|offset| offset == reference)
                        .unwrap();
                    (!keys.isnull[row]).then_some(keys.values[row])
                })
                .collect();
            let mut expected = order.clone();
            expected.sort_by(|a, b| {
                let order = match (a, b) {
                    (None, None) => std::cmp::Ordering::Equal,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (Some(a), Some(b)) => a.cmp(b),
                };
                let order = match (a.is_some() && b.is_some(), descending) {
                    (true, true) => order.reverse(),
                    _ => order,
                };
                match (a.is_none() != b.is_none(), nulls_first) {
                    (true, true) => order.reverse(),
                    _ => order,
                }
            });
            assert_eq!(
                order, expected,
                "descending {descending}, nulls first {nulls_first}"
            );
        }

        // Misuse: unknown flags or kinds, keys that are not the table's,
        // items too few, a width the items do not have.
        let bad_flags = CSortKey {
            kind: 1,
            flags: 0x8,
        };
        let mut words = 0;
        assert_ne!(
            tess_sort_item_words(1, &raw const bad_flags, &raw mut words, &raw mut status),
            Code::Ok
        );
        let bad_kind = CSortKey { kind: 3, flags: 0 };
        assert_ne!(
            tess_sort_item_words(1, &raw const bad_kind, &raw mut words, &raw mut status),
            Code::Ok
        );
        let int8 = CSortKey {
            kind: 2,
            flags: NULLABLE,
        };
        let mut items = vec![0_u64; keys.nrows() * 2];
        let mut count = 0;
        assert_ne!(
            tess_sort_items(
                table.ptr(),
                1,
                &raw const int8,
                items.as_mut_ptr(),
                items.len(),
                &raw mut count,
                &raw mut status
            ),
            Code::Ok
        );
        let int4 = CSortKey {
            kind: 1,
            flags: NULLABLE,
        };
        assert_ne!(
            tess_sort_items(
                table.ptr(),
                1,
                &raw const int4,
                items.as_mut_ptr(),
                10,
                &raw mut count,
                &raw mut status
            ),
            Code::Ok
        );
        let mut refs = vec![0_u32; 3];
        assert_ne!(
            tess_sort(items.as_mut_ptr(), 3, 0, refs.as_mut_ptr(), &raw mut status),
            Code::Ok
        );
    }
    Ok(())
}

/// Records appended from payload columns through the C entry point hold a
/// word of each row's NULL bits and a word per column, 0 for a NULL; a
/// column of another row count is refused.
#[test]
fn the_columns_entry_point_writes_each_payload_word() -> Result<()> {
    let keys = Keys::new(70);
    let column = keys.column();
    let key = TableKey {
        kind: 1,
        column: &raw const column,
        prepared: ptr::null(),
    };
    let first: Vec<u64> = (0..70).map(|row| row * 3).collect();
    let second: Vec<u64> = (0..70).map(|row| row << 40).collect();
    let first_nulls: Vec<bool> = (0..70).map(|row| row % 4 == 1).collect();
    let second_nulls = [false; 70];
    let columns = [
        DatumColumn {
            struct_size: size_of::<DatumColumn>(),
            values: first.as_ptr(),
            isnull: first_nulls.as_ptr(),
            nrows: 70,
            ..DatumColumn::EMPTY
        },
        DatumColumn {
            struct_size: size_of::<DatumColumn>(),
            values: second.as_ptr(),
            isnull: second_nulls.as_ptr(),
            nrows: 70,
            ..DatumColumn::EMPTY
        },
    ];
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        let mut table = CTable::new(1, 24, 0);
        table.add_chunk(CHUNK_HEADER + 80 * 48);
        let hashes = vec![0_u32; 70];
        let mut pending_words = keys.all_rows();
        let mut pending = Mask {
            nrows: 70,
            bits: pending_words.as_mut_ptr(),
        };
        let mut offsets = vec![0_u32; 70];
        let code = tess_table_append_columns(
            table.ptr(),
            0,
            hashes.as_ptr(),
            1,
            &raw const key,
            2,
            columns.as_ptr(),
            &raw mut pending,
            offsets.as_mut_ptr(),
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert!(pending_words.iter().all(|&word| word == 0));
        let mut record = TableRecord {
            struct_size: size_of::<TableRecord>(),
            hash: 0,
            null_bits: 0,
            keys: ptr::null(),
            payload: ptr::null(),
            payload_size: 0,
        };
        for row in 0..70 {
            let code =
                tess_table_record(table.ptr(), offsets[row], &raw mut record, &raw mut status);
            assert_eq!(code, Code::Ok, "{}", status.message());
            let words: Vec<u64> = std::slice::from_raw_parts(record.payload, 24)
                .chunks(8)
                .map(|word| u64::from_ne_bytes(word.try_into().unwrap()))
                .collect();
            let null = first_nulls[row];
            assert_eq!(
                words,
                [
                    u64::from(null),
                    if null { 0 } else { first[row] },
                    second[row]
                ]
            );
        }
        // A column of another row count is refused.
        let short = [
            DatumColumn {
                nrows: 69,
                ..columns[0]
            },
            DatumColumn { ..columns[1] },
        ];
        let mut pending_words = keys.all_rows();
        let mut pending = Mask {
            nrows: 70,
            bits: pending_words.as_mut_ptr(),
        };
        assert_ne!(
            tess_table_append_columns(
                table.ptr(),
                0,
                hashes.as_ptr(),
                1,
                &raw const key,
                2,
                short.as_ptr(),
                &raw mut pending,
                offsets.as_mut_ptr(),
                &raw mut status,
            ),
            Code::Ok
        );
    }
    Ok(())
}

/// Rows appended to partitions from payload columns: each record lands in
/// its partition's chunk with its words, every partition counts its rows,
/// the NULL bits of the rows appended gather into one word, and the rows
/// of a partition whose chunk is full stay pending while the others go on.
#[test]
fn the_partitioned_columns_entry_point_counts_each_partition() -> Result<()> {
    let keys = Keys::new(70);
    let column = keys.column();
    let key = TableKey {
        kind: 1,
        column: &raw const column,
        prepared: ptr::null(),
    };
    let first: Vec<u64> = (0..70).map(|row| row * 3).collect();
    let second: Vec<u64> = (0..70).map(|row| row << 40).collect();
    let first_nulls: Vec<bool> = (0..70).map(|row| row % 4 == 1).collect();
    let second_nulls = [false; 70];
    let columns = [
        DatumColumn {
            struct_size: size_of::<DatumColumn>(),
            values: first.as_ptr(),
            isnull: first_nulls.as_ptr(),
            nrows: 70,
            ..DatumColumn::EMPTY
        },
        DatumColumn {
            struct_size: size_of::<DatumColumn>(),
            values: second.as_ptr(),
            isnull: second_nulls.as_ptr(),
            nrows: 70,
            ..DatumColumn::EMPTY
        },
    ];
    // Partition bits 3 and 4 of the hash; partition 3 has room for 5 records.
    let hashes: Vec<u32> = (0..70).map(|row| (row as u32 % 4) << 3).collect();
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        let mut table = CTable::new(1, 24, 0);
        for partition in 0..4 {
            let records = if partition == 3 { 5 } else { 40 };
            table.add_chunk(CHUNK_HEADER + records * 48);
        }
        let partition_chunks = [0_u32, 1, 2, 3];
        let mut rows = [7_u64, 0, 0, 0];
        let mut nulls = 0_u64;
        let mut pending_words = keys.all_rows();
        let mut pending = Mask {
            nrows: 70,
            bits: pending_words.as_mut_ptr(),
        };
        let mut offsets = vec![0_u32; 70];
        let code = tess_table_append_partitioned_columns(
            table.ptr(),
            partition_chunks.as_ptr(),
            4,
            3,
            hashes.as_ptr(),
            1,
            &raw const key,
            2,
            columns.as_ptr(),
            &raw mut pending,
            offsets.as_mut_ptr(),
            rows.as_mut_ptr(),
            &raw mut nulls,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        // 18 rows of each of partitions 0 and 1, 17 of 2 and 3; the counts add up.
        assert_eq!(rows, [7 + 18, 18, 17, 5]);
        assert_eq!(nulls, 1);
        let left: Vec<usize> = (0..70)
            .filter(|row| pending_words[row / 64] >> (row % 64) & 1 == 1)
            .collect();
        let expected: Vec<usize> = (0..70).filter(|row| row % 4 == 3).skip(5).collect();
        assert_eq!(left, expected);
        let mut record = TableRecord {
            struct_size: size_of::<TableRecord>(),
            hash: 0,
            null_bits: 0,
            keys: ptr::null(),
            payload: ptr::null(),
            payload_size: 0,
        };
        for row in (0..70).filter(|row| !expected.contains(row)) {
            assert_eq!((offsets[row] >> 17) as usize, row % 4, "row {row}");
            let code =
                tess_table_record(table.ptr(), offsets[row], &raw mut record, &raw mut status);
            assert_eq!(code, Code::Ok, "{}", status.message());
            let words: Vec<u64> = std::slice::from_raw_parts(record.payload, 24)
                .chunks(8)
                .map(|word| u64::from_ne_bytes(word.try_into().unwrap()))
                .collect();
            let null = first_nulls[row];
            assert_eq!(
                words,
                [
                    u64::from(null),
                    if null { 0 } else { first[row] },
                    second[row]
                ]
            );
        }
        // A null word of NULL bits is refused, before anything is appended.
        let mut pending_words = keys.all_rows();
        let mut pending = Mask {
            nrows: 70,
            bits: pending_words.as_mut_ptr(),
        };
        assert_ne!(
            tess_table_append_partitioned_columns(
                table.ptr(),
                partition_chunks.as_ptr(),
                4,
                3,
                hashes.as_ptr(),
                1,
                &raw const key,
                2,
                columns.as_ptr(),
                &raw mut pending,
                offsets.as_mut_ptr(),
                rows.as_mut_ptr(),
                ptr::null_mut(),
                &raw mut status,
            ),
            Code::Ok
        );
        assert_eq!(pending_words, keys.all_rows());
        assert_eq!(rows, [7 + 18, 18, 17, 5]);
    }
    Ok(())
}

/// Chunks of columns through the C entry points: the header's layout the
/// C side mirrors, rows appended by partition with a by-value and a
/// by-reference word, packed and unpacked into a chunk of their rows.
#[test]
fn chunks_of_columns_round_trip_through_the_entry_points() -> Result<()> {
    use tessera_capi::c::{
        tess_spill_columns_append_partitioned, tess_spill_columns_init, tess_spill_columns_layout,
        tess_spill_columns_pack, tess_spill_columns_unpack,
    };
    assert_eq!(tess_spill_columns_layout(0), 16);
    assert_eq!(tess_spill_columns_layout(1), 0);
    assert_eq!(tess_spill_columns_layout(2), 4);
    let nrows = 50;
    let first: Vec<u64> = (0..nrows as u64)
        .map(|row| (row as i64 - 20) as u64)
        .collect();
    let second: Vec<u64> = (0..nrows as u64)
        .map(|row| ((row + 1) << 32) | (row * 8))
        .collect();
    let first_nulls: Vec<bool> = (0..nrows).map(|row| row % 9 == 4).collect();
    let second_nulls = vec![false; nrows];
    let columns = [
        DatumColumn {
            struct_size: size_of::<DatumColumn>(),
            values: first.as_ptr(),
            isnull: first_nulls.as_ptr(),
            nrows: nrows as i32,
            ..DatumColumn::EMPTY
        },
        DatumColumn {
            struct_size: size_of::<DatumColumn>(),
            values: second.as_ptr(),
            isnull: second_nulls.as_ptr(),
            nrows: nrows as i32,
            ..DatumColumn::EMPTY
        },
    ];
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        let lens = [16_usize, 16 + 8 * 3 * 40, 16 + 8 * 3 * 40];
        let mut chunks: Vec<Vec<u64>> = lens.iter().map(|&len| vec![0_u64; len / 8]).collect();
        for (chunk, &len) in chunks.iter_mut().zip(&lens) {
            let mut capacity = 0;
            let code = tess_spill_columns_init(
                chunk.as_mut_ptr().cast(),
                len,
                2,
                &raw mut capacity,
                &raw mut status,
            );
            assert_eq!(code, Code::Ok, "{}", status.message());
            assert_eq!(capacity, (len - 16) / 24);
        }
        let bases: Vec<*mut u8> = chunks
            .iter_mut()
            .map(|chunk| chunk.as_mut_ptr().cast())
            .collect();
        let hashes: Vec<u32> = (0..nrows as u32).map(|row| row % 2).collect();
        let mut pending_words = vec![(1_u64 << nrows) - 1];
        let mut pending = Mask {
            nrows: nrows as i32,
            bits: pending_words.as_mut_ptr(),
        };
        let mut offsets = vec![0_u32; nrows];
        let mut rows = [0_u64; 2];
        let code = tess_spill_columns_append_partitioned(
            bases.as_ptr(),
            lens.as_ptr(),
            3,
            [1_u32, 2].as_ptr(),
            2,
            0,
            hashes.as_ptr(),
            2,
            columns.as_ptr(),
            &raw mut pending,
            offsets.as_mut_ptr(),
            rows.as_mut_ptr(),
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(rows, [25, 25]);
        assert_eq!(pending_words, [0]);
        for (index, chunk) in chunks.iter().enumerate().skip(1) {
            let bytes: &[u8] = std::slice::from_raw_parts(chunk.as_ptr().cast(), lens[index]);
            let mut out = vec![0_u8; lens[index] + 8 + 16 * 65];
            let (mut packed, mut unpacked) = (0, 0);
            let code = tess_spill_columns_pack(
                bytes.as_ptr(),
                bytes.len(),
                out.as_mut_ptr(),
                out.len(),
                &raw mut packed,
                &raw mut unpacked,
                &raw mut status,
            );
            assert_eq!(code, Code::Ok, "{}", status.message());
            assert_eq!(unpacked, 16 + 8 * 3 * 25);
            let mut back = vec![0_u64; unpacked / 8];
            let code = tess_spill_columns_unpack(
                out.as_ptr(),
                packed,
                back.as_mut_ptr().cast(),
                unpacked,
                &raw mut status,
            );
            assert_eq!(code, Code::Ok, "{}", status.message());
            // The rows of the partition in order: nulls, then each word.
            for place in 0..25 {
                let row = 2 * place + (index - 1);
                let null = first_nulls[row];
                assert_eq!(back[2 + place], u64::from(null), "row {row}");
                assert_eq!(back[2 + 25 + place], if null { 0 } else { first[row] });
                assert_eq!(back[2 + 50 + place], second[row]);
                assert_eq!(offsets[row], ((index as u32) << 17) | place as u32);
            }
        }
    }
    Ok(())
}
