//! The table over Datum key columns, against the dense path, and through
//! the C entry points called as C would call them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ptr;

use anyhow::Result;
use tessera_capi::c::sort_flags::{DESCENDING, NULLABLE, NULLS_FIRST};
use tessera_capi::c::{
    CSortKey, Code, DatumColumn, Mask, Status, TableKey, TableRecord, TableRef, TableStats,
    TableSumArg, tess_bloom_add, tess_bloom_add_atomic, tess_bloom_probe, tess_bloom_shared_init,
    tess_bloom_shared_probe, tess_bloom_shared_ready, tess_bloom_shared_words,
    tess_build_counters_init, tess_build_step, tess_build_stop, tess_build_stopped,
    tess_build_take_chunk, tess_build_totals, tess_int4_hash, tess_int8_hash, tess_sort,
    tess_sort_item_words, tess_sort_items, tess_sort_layout, tess_sort_merge,
    tess_table_accumulate, tess_table_accumulate_sums, tess_table_append,
    tess_table_append_columns, tess_table_append_partitioned_columns, tess_table_bloom,
    tess_table_bloom_words, tess_table_bloom_words_within, tess_table_chunk_init,
    tess_table_clear_key, tess_table_combine, tess_table_create, tess_table_find_or_insert,
    tess_table_format_version, tess_table_gather, tess_table_gather_key, tess_table_gather_words,
    tess_table_layout, tess_table_link, tess_table_link_grouped, tess_table_mark,
    tess_table_mark_words, tess_table_next_in_group, tess_table_next_match,
    tess_table_next_unmarked, tess_table_payloads, tess_table_probe, tess_table_record,
    tess_table_regrow, tess_table_scan, tess_table_size, tess_table_stats,
    tess_table_try_build_bloom,
};
use tessera_core::{ColumnView, RowMask, RowMaskView};
use tessera_kernels::int32::{self, NullKeys};
use tessera_kernels::table::phases::{Action, PROBE, Participant};
use tessera_kernels::table::{
    Batch, CHUNK_HEADER, KeyKind, LocalTable, MAX_CHUNK_LEN, MAX_PAYLOAD_COLUMNS, TableConfig,
    index_size, payload_null_words,
};

thread_local! {
    /// The allocations this thread has made, for a test that a call makes
    /// none; tests run on threads of their own.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

/// The system's allocator, counting each thread's allocations.
struct Counting;

// SAFETY: every call goes to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        // SAFETY: the caller's contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The allocations `call` makes on this thread.
fn allocations_of<R>(call: impl FnOnce() -> R) -> (R, usize) {
    let before = ALLOCATIONS.with(Cell::get);
    let result = call();
    (result, ALLOCATIONS.with(Cell::get) - before)
}

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
    owner.insert(
        None,
        &mut Batch::new(hashes, keys, &mut pending, &mut offsets)?,
    )?;
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
    assert_eq!(tess_table_layout(11), 48);
    assert_eq!(tess_table_layout(12), 40);
    assert_eq!(tess_table_layout(13), 0);
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

/// A varlena of `body` behind a 4-byte header, or a 1-byte one.
fn varlena(body: Vec<u8>, short: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if short {
        out.push((((body.len() + 1) << 1) | 1) as u8);
    } else {
        out.extend_from_slice(&(((body.len() + 4) as u32) << 2).to_ne_bytes());
    }
    out.extend(body);
    out
}

/// A partial sum state as the node writes it, under `tag`, with `rest`
/// bytes of a numeric rest after the words.
fn partial_state(
    state: tessera_kernels::decimal::SumState,
    tag: u32,
    short: bool,
    rest: usize,
) -> Vec<u8> {
    let mut body = tag.to_ne_bytes().to_vec();
    for word in state.to_words() {
        body.extend_from_slice(&word.to_ne_bytes());
    }
    body.extend(std::iter::repeat_n(0xAB, rest));
    varlena(body, short)
}

/// The core's int8[] pair of a partial average: one dimension of 2 from
/// 1, no NULL bitmap, elements of type `elemtype`.
fn partial_pair(count: i64, sum: i64, elemtype: i32) -> Vec<u8> {
    let mut body = Vec::new();
    for int in [1_i32, 0, elemtype, 2, 1] {
        body.extend_from_slice(&int.to_ne_bytes());
    }
    body.extend_from_slice(&count.to_ne_bytes());
    body.extend_from_slice(&sum.to_ne_bytes());
    varlena(body, false)
}

/// Partial states through the entry point, as a final grouping calls it:
/// the node's own states behind either header, NULL, one with a rest
/// (to the rest), and the core's int8[] pairs of an average, merged into
/// two groups; a value of another format, and partial states with other
/// sums in one call, fail.
#[test]
fn partial_states_merge_through_the_entry_point() -> Result<()> {
    use tessera_kernels::decimal::{Sum, SumState};

    const TAG: u32 = 0x5453_4D31;
    let state = |value: i128, scale: u32, count: u64| SumState {
        sum: Sum {
            value,
            scale,
            count,
        },
        ..SumState::default()
    };
    let nrows = 8;
    let keys: Vec<u64> = (0..nrows as u64).map(|row| row % 2).collect();
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
    let states = [
        partial_state(state(1250, 2, 3), TAG, false, 0),
        partial_state(
            SumState {
                nan: true,
                ..state(-5, 1, 1)
            },
            TAG,
            true,
            0,
        ),
        partial_state(state(7, 3, 2), TAG, false, 0),
        Vec::new(),
        partial_state(state(9, 0, 1), TAG, false, 8),
        partial_state(
            SumState {
                positive_infinity: true,
                ..state(1, 0, 1)
            },
            TAG,
            false,
            0,
        ),
        partial_state(state(99, 0, 1), TAG, false, 0),
        partial_state(SumState::default(), TAG, true, 0),
    ];
    let mut state_nulls = vec![false; nrows];
    state_nulls[3] = true;
    let state_datums: Vec<u64> = states.iter().map(|bytes| bytes.as_ptr() as u64).collect();
    let state_column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: state_datums.as_ptr(),
        isnull: state_nulls.as_ptr(),
        nrows: nrows as i32,
        ..DatumColumn::EMPTY
    };
    let pairs = [
        partial_pair(2, 100, 20),
        partial_pair(1, -7, 20),
        partial_pair(3, 30, 20),
        Vec::new(),
        partial_pair(1, 5, 20),
        partial_pair(0, 0, 20),
        partial_pair(8, 800, 20),
        partial_pair(4, 40, 20),
    ];
    let pair_datums: Vec<u64> = pairs.iter().map(|bytes| bytes.as_ptr() as u64).collect();
    let pair_column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: pair_datums.as_ptr(),
        isnull: state_nulls.as_ptr(),
        nrows: nrows as i32,
        ..DatumColumn::EMPTY
    };
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        let mut table = CTable::new(1, 8 + 32 * 2, 16);
        table.add_chunk(8192);
        let mut pending_words = [(1 << nrows) - 1];
        let mut pending = Mask {
            nrows: nrows as i32,
            bits: pending_words.as_mut_ptr(),
        };
        let mut inserted_words = [0];
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
        // Row 6 is not selected.
        let mut selected = [0xBF_u64];
        let rows = Mask {
            nrows: nrows as i32,
            bits: selected.as_mut_ptr(),
        };
        let mut rest_words = [[0_u64; 1]; 2];
        let [first, second] = &mut rest_words;
        let mut rests = [first, second].map(|words| Mask {
            nrows: nrows as i32,
            bits: words.as_mut_ptr(),
        });
        let sums: Vec<TableSumArg> = [(3, &raw const state_column), (4, &raw const pair_column)]
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
            2,
            sums.as_ptr(),
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(rest_words, [[1 << 4], [0]], "the state with a rest");
        let mut two = [0b11_u64];
        let groups = Mask {
            nrows: 2,
            bits: two.as_mut_ptr(),
        };
        let found = |sum: usize, status: &mut Status| -> [SumState; 2] {
            let mut words = [[0_u64; 2]; 4];
            for (word, out) in words.iter_mut().enumerate() {
                assert_eq!(
                    tess_table_gather(
                        table.ptr(),
                        offsets.as_ptr(),
                        &raw const groups,
                        8 + 32 * sum + 8 * word,
                        out.as_mut_ptr(),
                        status
                    ),
                    Code::Ok
                );
            }
            std::array::from_fn(|group| {
                SumState::from_words(std::array::from_fn(|word| words[word][group]))
            })
        };
        assert_eq!(
            found(0, &mut status),
            [
                state(12507, 3, 5),
                SumState {
                    nan: true,
                    positive_infinity: true,
                    ..state(5, 1, 2)
                }
            ]
        );
        assert_eq!(found(1, &mut status), [state(135, 0, 6), state(33, 0, 5)]);
        // Another tag, a scale past 18, another element type, and a term
        // among the states.
        for (bytes, kind) in [
            (partial_state(state(1, 0, 1), TAG + 1, false, 0), 3),
            (partial_state(state(1, 25, 1), TAG, false, 0), 3),
            (partial_pair(1, 1, 23), 4),
        ] {
            let datums = vec![bytes.as_ptr() as u64; nrows];
            let column = DatumColumn {
                values: datums.as_ptr(),
                ..state_column
            };
            let arg = TableSumArg {
                kind,
                column: &raw const column,
                value_at: 8,
                rest: rests.as_mut_ptr(),
            };
            let code = tess_table_accumulate_sums(
                table.ptr(),
                offsets.as_ptr(),
                &raw const rows,
                1,
                &raw const arg,
                &raw mut status,
            );
            assert_ne!(code, Code::Ok, "kind {kind}");
        }
        let mixed = [
            TableSumArg {
                kind: 3,
                column: &raw const state_column,
                value_at: 8,
                rest: rests.as_mut_ptr(),
            },
            TableSumArg {
                kind: 1,
                column: &raw const key_column,
                value_at: 40,
                rest: rests.as_mut_ptr().add(1),
            },
        ];
        let code = tess_table_accumulate_sums(
            table.ptr(),
            offsets.as_ptr(),
            &raw const rows,
            2,
            mixed.as_ptr(),
            &raw mut status,
        );
        assert_ne!(code, Code::Ok, "partial states with terms");
    }
    Ok(())
}

/// Marks and walks that cannot be done fail, with the cursor and the
/// count as they were: references that name no record, record sizes that
/// are not the table's, cursors off its records, walks of no records, and
/// more chunks than a table may have.
#[test]
fn marks_and_walks_that_cannot_be_done_are_refused() -> Result<()> {
    const NROWS: usize = 100;
    let values: Vec<u64> = (0..NROWS as u64).collect();
    let isnull = [false; NROWS];
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: values.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: NROWS as i32,
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
    let payload: Vec<u8> = values
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect();
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        // Records of 32 bytes, 70 to a chunk: chunk 1 holds 30.
        let chunk_bytes = 8 + 70 * 32;
        let mut table = CTable::new(1, 8, 128);
        let mut pending_words = [u64::MAX, (1 << 36) - 1];
        let mut pending = Mask {
            nrows: NROWS as i32,
            bits: pending_words.as_mut_ptr(),
        };
        let mut offsets = vec![0; NROWS];
        table.insert(
            chunk_bytes,
            hashes.as_ptr(),
            &raw const key,
            payload.as_ptr(),
            &raw mut pending,
            offsets.as_mut_ptr(),
            false,
        );
        assert_eq!(table.chunks.len(), 2);
        let mut marks = vec![vec![0_u64; 2]; 2];
        let pointers: Vec<*mut u64> = marks.iter_mut().map(|run| run.as_mut_ptr()).collect();
        let mut one_word = [1_u64];
        let one = Mask {
            nrows: 1,
            bits: one_word.as_mut_ptr(),
        };
        let mut mark = |table: *const TableRef, reference: u32, size: usize| {
            tess_table_mark(
                table,
                size,
                &raw const reference,
                &raw const one,
                pointers.as_ptr(),
                false,
                &raw mut status,
            )
        };
        // Before the records, between two, past the chunk, in no chunk.
        let past = (8 + 70 * 32) / 8;
        for reference in [0, offsets[0] + 1, past, (5 << 17) | 1] {
            assert_eq!(
                mark(table.ptr(), reference, 32),
                Code::InvalidArgument,
                "{reference:#x}"
            );
        }
        // Sizes no record has, and one that is not the table's.
        for size in [0, 12, 1 << 20, 40] {
            assert_eq!(
                mark(table.ptr(), offsets[0], size),
                Code::InvalidArgument,
                "{size}"
            );
            if size != 40 {
                let mut words = 7;
                let mut status = Status::new();
                let code =
                    tess_table_mark_words(chunk_bytes, size, &raw mut words, &raw mut status);
                assert_eq!((code, words), (Code::InvalidArgument, 7), "{size}");
            }
        }
        assert!(marks.iter().flatten().all(|&word| word == 0), "no mark set");
        // More chunks than a table may have, each a chunk of 8 bytes.
        let mut empty = [8_u64];
        let bases = vec![empty.as_mut_ptr().cast::<u8>(); 32769];
        let lens = vec![8_usize; 32769];
        let many = TableRef {
            index: ptr::null_mut(),
            index_len: 0,
            chunks: bases.as_ptr(),
            chunk_lens: lens.as_ptr(),
            nchunks: 32769,
        };
        assert_eq!(mark(&raw const many, offsets[0], 32), Code::InvalidArgument);

        let walk = |size: usize, start: u64, capacity: i32| {
            let (mut cursor, mut count) = (start, -1);
            let mut out = [0_u32; 8];
            let mut status = Status::new();
            let code = tess_table_next_unmarked(
                table.ptr(),
                size,
                pointers.as_ptr(),
                false,
                &raw mut cursor,
                out.as_mut_ptr(),
                capacity,
                &raw mut count,
                &raw mut status,
            );
            assert_eq!((cursor, count), (start, -1), "{size} {start:#x} {capacity}");
            code
        };
        // Sizes, a cursor off a record and one past chunk 1's 30 records,
        // a walk of none and of fewer than none.
        for (size, start, capacity) in [
            (12, 0, 8),
            (40, 0, 8),
            (32, 12, 8),
            (32, (1 << 32) | (8 + 40 * 32), 8),
            (32, 0, 0),
            (32, 0, -1),
        ] {
            assert_eq!(walk(size, start, capacity), Code::InvalidArgument);
        }
    }
    Ok(())
}

/// RIGHT and FULL joins through the entry points: the records of every
/// third row marked, by one process or atomically, are left out of the
/// walk, which goes on where it stopped; without marks it visits every
/// record; a reference between records is refused.
#[test]
fn the_mark_entry_points_leave_marked_records_out() -> Result<()> {
    const NROWS: usize = 200;
    let values: Vec<u64> = (0..NROWS as u64).collect();
    let isnull = [false; NROWS];
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: values.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: NROWS as i32,
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
    let payload: Vec<u8> = values
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect();
    let all = |row: usize| row < NROWS;
    let mask_of = |keep: &dyn Fn(usize) -> bool| -> Vec<u64> {
        let mut words = vec![0_u64; NROWS.div_ceil(64)];
        for row in (0..NROWS).filter(|&row| keep(row)) {
            words[row / 64] |= 1 << (row % 64);
        }
        words
    };
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        // Chunks of 70 records of 32 bytes: two words of marks each.
        let chunk_bytes = 8 + 70 * 32;
        let mut table = CTable::new(1, 8, 256);
        let mut pending_words = mask_of(&all);
        let mut pending = Mask {
            nrows: NROWS as i32,
            bits: pending_words.as_mut_ptr(),
        };
        let mut offsets = vec![0; NROWS];
        table.insert(
            chunk_bytes,
            hashes.as_ptr(),
            &raw const key,
            payload.as_ptr(),
            &raw mut pending,
            offsets.as_mut_ptr(),
            false,
        );
        assert_eq!(table.chunks.len(), 3);
        let mut words = 0;
        let code = tess_table_mark_words(chunk_bytes, 32, &raw mut words, &raw mut status);
        assert_eq!((code, words), (Code::Ok, 2));
        let walk = |marks: *const *mut u64, shared: bool| -> Vec<u32> {
            let mut status = Status::new();
            let mut cursor = 0;
            let mut out = [0; 7];
            let mut found = Vec::new();
            // A call gives at least a record until the walk is over: a
            // cursor that stood still fails here, not hangs.
            for _ in 0..=NROWS {
                let mut count = 0;
                let code = tess_table_next_unmarked(
                    table.ptr(),
                    32,
                    marks,
                    shared,
                    &raw mut cursor,
                    out.as_mut_ptr(),
                    7,
                    &raw mut count,
                    &raw mut status,
                );
                assert_eq!(code, Code::Ok, "{}", status.message());
                if count == 0 {
                    return found;
                }
                found.extend_from_slice(&out[..count as usize]);
            }
            panic!("the walk is not over after {NROWS} calls");
        };
        for shared in [false, true] {
            let mut marks = vec![vec![0_u64; words]; table.chunks.len()];
            let pointers: Vec<*mut u64> = marks.iter_mut().map(|run| run.as_mut_ptr()).collect();
            let mut third_words = mask_of(&|row| row % 3 == 0);
            let third = Mask {
                nrows: NROWS as i32,
                bits: third_words.as_mut_ptr(),
            };
            let code = tess_table_mark(
                table.ptr(),
                32,
                offsets.as_ptr(),
                &raw const third,
                pointers.as_ptr(),
                shared,
                &raw mut status,
            );
            assert_eq!(code, Code::Ok, "{}", status.message());
            let unmarked: Vec<u32> = (0..NROWS)
                .filter(|row| row % 3 != 0)
                .map(|row| offsets[row])
                .collect();
            assert_eq!(walk(pointers.as_ptr(), shared), unmarked, "shared {shared}");
            assert_eq!(walk(ptr::null(), shared), offsets, "without marks");
            // A reference between two records names none.
            let mut between = offsets.clone();
            between[0] += 1;
            let code = tess_table_mark(
                table.ptr(),
                32,
                between.as_ptr(),
                &raw const third,
                pointers.as_ptr(),
                shared,
                &raw mut status,
            );
            assert_eq!(code, Code::InvalidArgument);
            // A walk of no records is refused: its count of 0 would read as
            // the end of the walk.
            let (mut cursor, mut count) = (0, -1);
            let mut none: [u32; 0] = [];
            let code = tess_table_next_unmarked(
                table.ptr(),
                32,
                pointers.as_ptr(),
                shared,
                &raw mut cursor,
                none.as_mut_ptr(),
                0,
                &raw mut count,
                &raw mut status,
            );
            assert_eq!((code, cursor, count), (Code::InvalidArgument, 0, -1));
        }
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
        // A walk of no records is refused: its count of 0 would read as
        // the end of the walk.
        let before = cursor;
        let code = tess_table_scan(
            table.ptr(),
            &raw mut cursor,
            walked.as_mut_ptr(),
            0,
            &raw mut count,
            &raw mut status,
        );
        assert_eq!((code, cursor, count), (Code::InvalidArgument, before, 4));
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

#[test]
fn int8_keys_find_the_records_of_int4_keys() -> Result<()> {
    // Built rows: int4 values row - 32. Probe rows of int8: an even row
    // holds row - 32 and finds its twin; an odd row holds `row << 33`, past
    // the int4 range, which folds to the hash of 2 * row, a key of the
    // table for the rows up to 15, and must not find it: keys are compared
    // whole.
    let built_datums: Vec<u64> = (0..64_i64).map(|row| (row - 32) as u64).collect();
    let probed_datums: Vec<u64> = (0..64_i64)
        .map(|row| if row % 2 == 0 { row - 32 } else { row << 33 } as u64)
        .collect();
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
        let mut table = CTable::new(1, 0, 64);
        let (built_hashes, mut pending_words) = hash_column(&built_column, 1, 64);
        let built_key = TableKey {
            kind: 1,
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

        let (probed_hashes, mut rows_words) = hash_column(&probed_column, 2, 64);
        for row in 0..64 {
            if row % 2 == 0 {
                assert_eq!(probed_hashes[row], built_hashes[row], "row {row}");
            } else if row <= 15 {
                assert_eq!(probed_hashes[row], built_hashes[2 * row + 32], "row {row}");
            }
        }
        let probed_key = TableKey {
            kind: 2,
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

/// A merge of items wider than a sort item can be, or of no words, is an
/// error before its lanes are borrowed: their count, runs × words, would
/// otherwise size a borrow and a list from an unchecked width.
#[test]
fn a_merge_refuses_an_impossible_item_width() {
    let lane = [0_u64; 4];
    let lanes = [lane.as_ptr(); 2];
    let left = [4_u32];
    let more = [false];
    let mut state = [0_u32; tessera_kernels::sort::MERGE_STATE_WORDS];
    let mut out = [0_u32; 4];
    for words in [0, 18, i32::MAX] {
        let (mut count, mut refill) = (-7, -7);
        let mut status = Status::new();
        // SAFETY: every pointer is valid for what a valid width would read.
        let code = unsafe {
            tess_sort_merge(
                1,
                words,
                lanes.as_ptr(),
                left.as_ptr(),
                more.as_ptr(),
                state.as_mut_ptr(),
                out.as_mut_ptr(),
                4,
                &mut count,
                &mut refill,
                &mut status,
            )
        };
        assert_eq!(code, Code::InvalidArgument, "{words} words");
        assert_eq!((count, refill), (-7, -7), "{words} words");
    }
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

/// The payload words of records in another order, every column of a row
/// in one call, past the kernel's group of sixteen words: each row's words
/// as appended, the rows outside the mask untouched; no words, a null
/// output and words past the payload are refused.
#[test]
fn the_words_entry_point_gathers_every_column() -> Result<()> {
    const COLUMNS: usize = 19;
    let keys = Keys::new(70);
    let column = keys.column();
    let key = TableKey {
        kind: 1,
        column: &raw const column,
        prepared: ptr::null(),
    };
    let values: Vec<Vec<u64>> = (0..COLUMNS as u64)
        .map(|column| (0..70).map(|row| row * 100 + column).collect())
        .collect();
    let no_nulls = [false; 70];
    let columns: Vec<DatumColumn> = values
        .iter()
        .map(|column| DatumColumn {
            struct_size: size_of::<DatumColumn>(),
            values: column.as_ptr(),
            isnull: no_nulls.as_ptr(),
            nrows: 70,
            ..DatumColumn::EMPTY
        })
        .collect();
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        let mut table = CTable::new(1, 8 * (1 + COLUMNS), 0);
        table.add_chunk(CHUNK_HEADER + 70 * 192);
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
            COLUMNS as i32,
            columns.as_ptr(),
            &raw mut pending,
            offsets.as_mut_ptr(),
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        // The records last first, every third row left out.
        let reversed: Vec<u32> = offsets.iter().rev().copied().collect();
        let mut selected = keys.all_rows();
        for row in (0..70).step_by(3) {
            selected[row / 64] &= !(1 << (row % 64));
        }
        let rows = Mask {
            nrows: 70,
            bits: selected.as_mut_ptr(),
        };
        let mut out = vec![vec![u64::MAX; 70]; COLUMNS];
        let pointers: Vec<*mut u64> = out.iter_mut().map(|words| words.as_mut_ptr()).collect();
        let code = tess_table_gather_words(
            table.ptr(),
            reversed.as_ptr(),
            &raw const rows,
            1,
            COLUMNS,
            pointers.as_ptr(),
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        for (column, words) in out.iter().enumerate() {
            for (row, &word) in words.iter().enumerate() {
                let expected = if row % 3 == 0 {
                    u64::MAX
                } else {
                    values[column][69 - row]
                };
                assert_eq!(word, expected, "column {column}, row {row}");
            }
        }
        for (first, nwords, pointers) in [
            (1, 0, pointers.as_ptr()),
            (1, COLUMNS, ptr::null()),
            (2, COLUMNS, pointers.as_ptr()),
        ] {
            let code = tess_table_gather_words(
                table.ptr(),
                reversed.as_ptr(),
                &raw const rows,
                first,
                nwords,
                pointers,
                &raw mut status,
            );
            assert_ne!(code, Code::Ok, "{first} {nwords}");
        }
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
        tess_spill_columns_pack, tess_spill_columns_shape, tess_spill_columns_unpack,
    };
    assert_eq!(tess_spill_columns_layout(0), 16);
    assert_eq!(tess_spill_columns_layout(1), 0);
    assert_eq!(tess_spill_columns_layout(2), 4);
    assert_eq!(tess_spill_columns_layout(3), 8);
    assert_eq!(tess_spill_columns_layout(99), 0);
    // A lane of NULL bits at least, one per 64 words; the packed slack a
    // count word and a descriptor per lane.
    assert_eq!(tess_spill_columns_shape(0, 0), 1);
    assert_eq!(tess_spill_columns_shape(64, 0), 1);
    assert_eq!(tess_spill_columns_shape(65, 0), 2);
    assert_eq!(tess_spill_columns_shape(2, 1), 8 + 16 * 3);
    assert_eq!(tess_spill_columns_shape(2, 9), 0);
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

#[test]
fn outputs_are_checked_before_anything_changes() -> Result<()> {
    let values: Vec<u64> = (1..=4).collect();
    let isnull = [false; 4];
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: values.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 4,
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
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        let mut table = CTable::new(1, 8, 8);
        table.add_chunk(8 + 4 * 32);
        let mut pending_words = [0b1111];
        let mut pending = Mask {
            nrows: 4,
            bits: pending_words.as_mut_ptr(),
        };
        let mut inserted_words = [0];
        let mut inserted = Mask {
            nrows: 4,
            bits: inserted_words.as_mut_ptr(),
        };
        let mut offsets = [0; 4];
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

        // A key cleared without a place for the count: refused, and every
        // record keeps its key.
        let code = tess_table_clear_key(table.ptr(), 0, ptr::null_mut(), &raw mut status);
        assert_eq!(code, Code::InvalidArgument);
        for (offset, value) in offsets.iter().zip(&values) {
            let mut record = TableRecord {
                struct_size: size_of::<TableRecord>(),
                hash: 0,
                null_bits: 0,
                keys: ptr::null(),
                payload: ptr::null(),
                payload_size: 0,
            };
            let code = tess_table_record(table.ptr(), *offset, &raw mut record, &raw mut status);
            assert_eq!(code, Code::Ok, "{}", status.message());
            assert_eq!(*record.keys, *value as i64, "the key is not cleared");
        }

        // A walk without a place for the count: refused, the cursor where
        // it was.
        let mut cursor = 0;
        let mut walked = [0; 4];
        let code = tess_table_scan(
            table.ptr(),
            &raw mut cursor,
            walked.as_mut_ptr(),
            4,
            ptr::null_mut(),
            &raw mut status,
        );
        assert_eq!(code, Code::InvalidArgument);
        assert_eq!(cursor, 0, "the cursor did not move");

        // A chunk number taken without a place for it: refused, and the
        // build's counters number no chunk.
        let mut counters = [u64::MAX; 4];
        let code = tess_build_counters_init(counters.as_mut_ptr(), &raw mut status);
        assert_eq!(code, Code::Ok, "{}", status.message());
        let code = tess_build_take_chunk(counters.as_mut_ptr(), ptr::null_mut(), &raw mut status);
        assert_eq!(code, Code::InvalidArgument);
        let (mut records, mut nulls, mut chunks) = (0, 0, u64::MAX);
        let code = tess_build_totals(
            counters.as_mut_ptr(),
            &raw mut records,
            &raw mut nulls,
            &raw mut chunks,
            ptr::null_mut(),
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(chunks, 0, "no chunk was numbered");
    }
    Ok(())
}

/// An append by partition takes 64 columns at most, and checks the index
/// of a table that has one, as an append does: a payload of another size
/// or a key of another kind writes nothing and leaves every row pending.
#[test]
fn an_append_by_partition_checks_what_it_is_given() -> Result<()> {
    let values: Vec<u64> = (1..=4).collect();
    let isnull = [false; 4];
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: values.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 4,
        ..DatumColumn::EMPTY
    };
    let columns: Vec<DatumColumn> = (0..65)
        .map(|_| DatumColumn {
            struct_size: size_of::<DatumColumn>(),
            values: values.as_ptr(),
            isnull: isnull.as_ptr(),
            nrows: 4,
            ..DatumColumn::EMPTY
        })
        .collect();
    let int4 = TableKey {
        kind: 1,
        column: &raw const column,
        prepared: ptr::null(),
    };
    let int8 = TableKey {
        kind: 2,
        ..key_copy(&int4)
    };
    let hashes: Vec<u32> = (0..4).collect();
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        // An int4 key and one payload column: records of 40 bytes.
        let mut table = CTable::new(1, 16, 8);
        table.add_chunk(CHUNK_HEADER + 4 * 40);
        table.add_chunk(CHUNK_HEADER + 4 * 40);
        let mut pending_words = [0b1111];
        let mut rows = [0_u64; 2];
        let mut append =
            |key: &TableKey, ncolumns: i32, pending_words: &mut [u64; 1], rows: &mut [u64; 2]| {
                let mut pending = Mask {
                    nrows: 4,
                    bits: pending_words.as_mut_ptr(),
                };
                let (mut offsets, mut nulls) = ([0; 4], 0);
                tess_table_append_partitioned_columns(
                    table.ptr(),
                    [0_u32, 1].as_ptr(),
                    2,
                    0,
                    hashes.as_ptr(),
                    1,
                    key,
                    ncolumns,
                    columns.as_ptr(),
                    &raw mut pending,
                    offsets.as_mut_ptr(),
                    rows.as_mut_ptr(),
                    &raw mut nulls,
                    &raw mut status,
                )
            };
        // 65 columns, two columns where the table has one, an int8 key
        // where it has an int4 one.
        for (key, ncolumns) in [(&int4, 65), (&int4, 2), (&int8, 1)] {
            assert_eq!(
                append(key, ncolumns, &mut pending_words, &mut rows),
                Code::InvalidArgument,
                "{ncolumns} columns"
            );
            assert_eq!(pending_words, [0b1111]);
            assert_eq!(rows, [0, 0]);
            for chunk in 0..2 {
                assert_eq!(
                    table.chunks[chunk][0], CHUNK_HEADER as u64,
                    "nothing written"
                );
            }
        }
        // The table's own go in, two to each partition.
        assert_eq!(append(&int4, 1, &mut pending_words, &mut rows), Code::Ok);
        assert_eq!(pending_words, [0]);
        assert_eq!(rows, [2, 2]);
    }
    Ok(())
}

/// Partitions that a call cannot take are refused by an append by
/// partition before it writes anything: a count that is not a power of two
/// up to 65536, bits past the hash's 32, a chunk the table lacks.
#[test]
fn an_append_by_partition_refuses_partitions_past_the_limits() -> Result<()> {
    let keys = Keys::new(4);
    let column = keys.column();
    let key = TableKey {
        kind: 1,
        column: &raw const column,
        prepared: ptr::null(),
    };
    let hashes: Vec<u32> = (0..4).collect();
    let many = vec![0_u32; 1 << 17];
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        let mut table = CTable::new(1, 8, 8);
        table.add_chunk(CHUNK_HEADER + 4 * 32);
        table.add_chunk(CHUNK_HEADER + 4 * 32);
        let cases: [(&[u32], i32, u32); 6] = [
            (&[], 0, 0),
            (&[0, 1, 0], 3, 0),
            (&many, 1 << 17, 0),
            (&[0, 1, 0, 1], 4, 31),
            (&[0], 1, 32),
            (&[0, 9], 2, 0),
        ];
        for (chunks, npartitions, shift) in cases {
            let mut pending_words = [0b1111];
            let mut pending = Mask {
                nrows: 4,
                bits: pending_words.as_mut_ptr(),
            };
            let mut rows = vec![0_u64; chunks.len()];
            let (mut offsets, mut nulls) = ([0; 4], 0);
            let code = tess_table_append_partitioned_columns(
                table.ptr(),
                chunks.as_ptr(),
                npartitions,
                shift,
                hashes.as_ptr(),
                1,
                &raw const key,
                0,
                ptr::null(),
                &raw mut pending,
                offsets.as_mut_ptr(),
                rows.as_mut_ptr(),
                &raw mut nulls,
                &raw mut status,
            );
            assert_eq!(code, Code::InvalidArgument, "{npartitions} at {shift}");
            assert_eq!(pending_words, [0b1111]);
            assert!(rows.iter().all(|&count| count == 0));
            for chunk in 0..2 {
                assert_eq!(
                    table.chunks[chunk][0], CHUNK_HEADER as u64,
                    "nothing written"
                );
            }
        }
    }
    Ok(())
}

/// A merge through its entry point: a count or a sum past the int8 range
/// fails with 22003, as the row-by-row transition does; aggregates past a
/// word of flags or a payload, an unknown kind and a merge into its own
/// source fail before anything changes.
#[test]
fn a_merge_that_cannot_be_done_fails() -> Result<()> {
    let values = [7_u64];
    let isnull = [false];
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: values.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 1,
        ..DatumColumn::EMPTY
    };
    let key = TableKey {
        kind: 1,
        column: &raw const column,
        prepared: ptr::null(),
    };
    let hashes = [int32::murmurhash32(7)];
    let payload_of = |flags: u64, state: i64| -> Vec<u8> {
        [flags, state as u64]
            .iter()
            .flat_map(|word| word.to_ne_bytes())
            .collect()
    };
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        // (the table's state, the state read back, the kind, the result)
        let overflows = [
            (i64::MAX, 1, 1_u32, "a count"),
            (i64::MAX, 1, 2, "a sum"),
            (i64::MIN, -1, 2, "a negative sum"),
        ];
        for (ours, theirs, kind, what) in overflows {
            let mut table = CTable::new(1, 16, 8);
            let mut pending_words = [1];
            let mut pending = Mask {
                nrows: 1,
                bits: pending_words.as_mut_ptr(),
            };
            let mut offsets = [0];
            let payload = payload_of(1, ours);
            table.insert(
                CHUNK_HEADER + 4 * 40,
                hashes.as_ptr(),
                &raw const key,
                payload.as_ptr(),
                &raw mut pending,
                offsets.as_mut_ptr(),
                false,
            );
            // The source: the same group, appended to a chunk of its own.
            table.add_chunk(CHUNK_HEADER + 4 * 40);
            let source = table.chunks.len() as i32 - 1;
            let mut pending_words = [1];
            let mut pending = Mask {
                nrows: 1,
                bits: pending_words.as_mut_ptr(),
            };
            let payload = payload_of(1, theirs);
            let mut status = Status::new();
            let code = tess_table_append(
                table.ptr(),
                source,
                16,
                hashes.as_ptr(),
                1,
                &raw const key,
                payload.as_ptr(),
                &raw mut pending,
                offsets.as_mut_ptr(),
                &raw mut status,
            );
            assert_eq!(code, Code::Ok, "{}", status.message());
            table.add_chunk(CHUNK_HEADER + 4 * 40);
            let into = table.chunks.len() as i32 - 1;
            let merge = |naggregates: i32, kinds: &[u32], chunk: i32, status: &mut Status| {
                let (mut from, mut merged, mut stop) = (CHUNK_HEADER, -1, -1);
                let code = tess_table_combine(
                    table.ptr(),
                    source,
                    &raw mut from,
                    chunk,
                    naggregates,
                    kinds.as_ptr(),
                    &raw mut merged,
                    &raw mut stop,
                    status,
                );
                (code, from, merged, stop)
            };
            // Wrong arguments: nothing moves.
            let kinds = vec![kind; 65];
            for (naggregates, kinds, chunk) in [
                (65, &kinds[..], into),
                (2, &kinds[..2], into),
                (1, &[5][..], into),
                (1, &kinds[..1], source),
            ] {
                let mut status = Status::new();
                let result = merge(naggregates, kinds, chunk, &mut status);
                assert_eq!(
                    result,
                    (Code::InvalidArgument, CHUNK_HEADER, -1, -1),
                    "{naggregates} of {kinds:?} into {chunk}"
                );
            }
            let mut status = Status::new();
            let (code, ..) = merge(1, &[kind], into, &mut status);
            assert_eq!(code, Code::IntegerOutOfRange, "{what}");
            assert_eq!(status.sqlstate(), "22003", "{what}");
            assert!(status.message().contains("bigint out of range"), "{what}");
        }
    }
    Ok(())
}

/// The filter's entry points as C calls them: the sizes, a filter filled
/// from a table, by adding rows and by adding them atomically, a shared
/// filter that one call builds and a second leaves, and the shapes each
/// call refuses with the words as they were.
#[test]
fn the_filter_entry_points_size_fill_and_probe() -> Result<()> {
    const NROWS: usize = 10;
    let keys = Keys::new(NROWS);
    let column = keys.column();
    let key = TableKey {
        kind: 1,
        column: &raw const column,
        prepared: ptr::null(),
    };
    let hashes: Vec<u32> = (0..NROWS as u32).map(int32::murmurhash32).collect();
    let mut all = [(1_u64 << NROWS) - 1];
    let rows = Mask {
        nrows: NROWS as i32,
        bits: all.as_mut_ptr(),
    };
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        let mut nwords = 0;
        assert_eq!(
            tess_table_bloom_words(NROWS as u64, &raw mut nwords, &raw mut status),
            Code::Ok
        );
        assert_eq!(nwords, 4, "160 bits, rounded up to a power of two");
        let mut within = 0;
        let code =
            tess_table_bloom_words_within(NROWS as u64, 64, &raw mut within, &raw mut status);
        assert_eq!((code, within), (Code::Ok, 1), "an eighth of 64 bytes");
        let mut shared_words = 0;
        let code = tess_bloom_shared_words(NROWS as u64, &raw mut shared_words, &raw mut status);
        assert_eq!((code, shared_words), (Code::Ok, 5), "a state word and four");

        let mut table = CTable::new(1, 8, 16);
        let mut pending_words = all;
        let mut pending = Mask {
            nrows: NROWS as i32,
            bits: pending_words.as_mut_ptr(),
        };
        let mut offsets = [0; NROWS];
        let payload = [0_u8; 8 * NROWS];
        table.insert(
            CHUNK_HEADER + 4 * 32,
            hashes.as_ptr(),
            &raw const key,
            payload.as_ptr(),
            &raw mut pending,
            offsets.as_mut_ptr(),
            false,
        );
        let probe = |words: &[u64]| -> Result<u64, Code> {
            let mut status = Status::new();
            let mut found_bits = [0];
            let mut found = Mask {
                nrows: NROWS as i32,
                bits: found_bits.as_mut_ptr(),
            };
            match tess_bloom_probe(
                words.as_ptr(),
                words.len(),
                hashes.as_ptr(),
                &raw const rows,
                &raw mut found,
                &raw mut status,
            ) {
                Code::Ok => Ok(found_bits[0]),
                code => Err(code),
            }
        };
        // From the table, by adding rows, by adding them atomically.
        let mut filled = vec![0_u64; nwords];
        let code = tess_table_bloom(table.ptr(), filled.as_mut_ptr(), nwords, &raw mut status);
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(probe(&filled), Ok(all[0]), "every key of the table");
        let mut added = vec![0_u64; nwords];
        let code = tess_bloom_add(
            added.as_mut_ptr(),
            nwords,
            hashes.as_ptr(),
            &raw const rows,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(added, filled, "the same bits as the table's keys");
        let mut together = vec![0_u64; nwords];
        let code = tess_bloom_add_atomic(
            together.as_mut_ptr(),
            nwords,
            hashes.as_ptr(),
            &raw const rows,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(together, filled);

        // A shared filter: not ready, then built by one call only.
        let mut shared = vec![7_u64; shared_words];
        let code = tess_bloom_shared_init(shared.as_mut_ptr(), shared_words, &raw mut status);
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert!(shared.iter().all(|&word| word == 0), "cleared");
        let ready = |shared: &mut Vec<u64>| {
            let (mut ready, mut status) = (true, Status::new());
            let code = tess_bloom_shared_ready(
                shared.as_mut_ptr(),
                shared.len(),
                &raw mut ready,
                &raw mut status,
            );
            assert_eq!(code, Code::Ok, "{}", status.message());
            ready
        };
        let shared_probe = |shared: &mut Vec<u64>| {
            let mut status = Status::new();
            let mut found_bits = [0];
            let mut found = Mask {
                nrows: NROWS as i32,
                bits: found_bits.as_mut_ptr(),
            };
            let code = tess_bloom_shared_probe(
                shared.as_mut_ptr(),
                shared.len(),
                hashes.as_ptr(),
                &raw const rows,
                &raw mut found,
                &raw mut status,
            );
            (code, found_bits[0])
        };
        assert!(!ready(&mut shared));
        assert_eq!(shared_probe(&mut shared).0, Code::InvalidArgument);
        for expected in [true, false] {
            let mut built = !expected;
            let code = tess_table_try_build_bloom(
                table.ptr(),
                shared.as_mut_ptr(),
                shared_words,
                &raw mut built,
                &raw mut status,
            );
            assert_eq!((code, built), (Code::Ok, expected), "{}", status.message());
        }
        assert!(ready(&mut shared));
        assert_eq!(shared_probe(&mut shared), (Code::Ok, all[0]));
        assert_eq!(shared[1..], filled[..], "the table's bits after the state");

        // Shapes the calls refuse: words not a power of two, a shared
        // filter of 1 or 4 words or not aligned to 8.
        let mut three = vec![7_u64; 3];
        assert_eq!(
            tess_table_bloom(table.ptr(), three.as_mut_ptr(), 3, &raw mut status),
            Code::InvalidArgument
        );
        for add in [tess_bloom_add, tess_bloom_add_atomic] {
            let code = add(
                three.as_mut_ptr(),
                3,
                hashes.as_ptr(),
                &raw const rows,
                &raw mut status,
            );
            assert_eq!(code, Code::InvalidArgument);
        }
        assert_eq!(probe(&three), Err(Code::InvalidArgument));
        assert_eq!(three, [7; 3]);
        let mut odd = vec![7_u64; 6];
        let misaligned = odd.as_mut_ptr().cast::<u8>().wrapping_add(4).cast::<u64>();
        for (words, nwords) in [
            (odd.as_mut_ptr(), 1),
            (odd.as_mut_ptr(), 4),
            (misaligned, 5),
        ] {
            let mut built = false;
            assert_eq!(
                tess_bloom_shared_init(words, nwords, &raw mut status),
                Code::InvalidArgument
            );
            assert_eq!(
                tess_table_try_build_bloom(
                    table.ptr(),
                    words,
                    nwords,
                    &raw mut built,
                    &raw mut status
                ),
                Code::InvalidArgument
            );
            let mut ready = false;
            assert_eq!(
                tess_bloom_shared_ready(words, nwords, &raw mut ready, &raw mut status),
                Code::InvalidArgument
            );
            let mut found_bits = [0];
            let mut found = Mask {
                nrows: NROWS as i32,
                bits: found_bits.as_mut_ptr(),
            };
            let code = tess_bloom_shared_probe(
                words,
                nwords,
                hashes.as_ptr(),
                &raw const rows,
                &raw mut found,
                &raw mut status,
            );
            assert_eq!(code, Code::InvalidArgument);
        }
        assert_eq!(
            tess_bloom_add_atomic(
                misaligned,
                4,
                hashes.as_ptr(),
                &raw const rows,
                &raw mut status
            ),
            Code::InvalidArgument
        );
        assert_eq!(odd, [7; 6], "nothing written");
    }
    Ok(())
}

/// A participant of a RIGHT or FULL join that leaves early: its stop word
/// is marked only while it probes, and read back by the last one; a null
/// participant or result and a word not aligned to 8 are refused.
#[test]
fn a_participant_marks_its_stop_only_while_it_probes() {
    let mut status = Status::new();
    let mut counters = [0_u64; 4];
    let mut participant = Participant::new();
    // SAFETY: local words and a participant this test alone uses.
    unsafe {
        assert_eq!(
            tess_build_counters_init(counters.as_mut_ptr(), &raw mut status),
            Code::Ok
        );
        let stop_and_read = |participant: &Participant| -> bool {
            let (mut word, mut any, mut status) = (0_u64, false, Status::new());
            let code = tess_build_stop(participant, &raw mut word, &raw mut status);
            assert_eq!(code, Code::Ok, "{}", status.message());
            let code = tess_build_stopped(&raw mut word, &raw mut any, &raw mut status);
            assert_eq!(code, Code::Ok, "{}", status.message());
            assert_eq!(word != 0, any);
            any
        };
        let mut step = |participant: &mut Participant, reply: u32| -> u32 {
            let mut action = u32::MAX;
            let mut status = Status::new();
            let code = tess_build_step(
                participant,
                counters.as_mut_ptr(),
                reply,
                &raw mut action,
                &raw mut status,
            );
            assert_eq!(code, Code::Ok, "{}", status.message());
            action
        };
        assert!(!stop_and_read(&participant), "before it attached");
        assert_eq!(step(&mut participant, 0), Action::Attach as u32);
        assert_eq!(step(&mut participant, PROBE), Action::Probe as u32);
        assert!(stop_and_read(&participant), "while it probes");
        assert_eq!(step(&mut participant, 0), Action::ArriveAndDetach as u32);
        assert_eq!(step(&mut participant, 1), Action::Free as u32);
        assert!(!stop_and_read(&participant), "once it left");

        let mut words = [0_u64; 2];
        let misaligned = words
            .as_mut_ptr()
            .cast::<u8>()
            .wrapping_add(4)
            .cast::<u64>();
        let mut any = false;
        assert_eq!(
            tess_build_stop(ptr::null(), words.as_mut_ptr(), &raw mut status),
            Code::InvalidArgument
        );
        assert_eq!(
            tess_build_stop(&participant, misaligned, &raw mut status),
            Code::InvalidArgument
        );
        assert_eq!(
            tess_build_stopped(misaligned, &raw mut any, &raw mut status),
            Code::InvalidArgument
        );
        assert_eq!(
            tess_build_stopped(words.as_mut_ptr(), ptr::null_mut(), &raw mut status),
            Code::InvalidArgument
        );
        assert_eq!(words, [0, 0]);
    }
}

#[test]
fn an_append_refuses_records_that_are_not_its_table_s() -> Result<()> {
    let values: Vec<u64> = (1..=4).collect();
    let isnull = [false; 4];
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: values.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 4,
        ..DatumColumn::EMPTY
    };
    let int4 = TableKey {
        kind: 1,
        column: &raw const column,
        prepared: ptr::null(),
    };
    let int8 = TableKey {
        kind: 2,
        ..key_copy(&int4)
    };
    let hashes: Vec<u32> = values
        .iter()
        .map(|&value| int32::murmurhash32(value as u32))
        .collect();
    let payload = [0_u8; 4 * 16];
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        // A table of an int4 key and a payload of 8 bytes: records of 32.
        let mut table = CTable::new(1, 8, 8);
        table.add_chunk(8 + 4 * 32);
        let mut pending_words = [0b1111];
        let mut pending = Mask {
            nrows: 4,
            bits: pending_words.as_mut_ptr(),
        };
        let mut offsets = [0; 4];

        // Another payload size or another kind of key: refused, nothing
        // written and every row still pending.
        for (payload_size, key) in [(16, &int4), (8, &int8)] {
            let code = tess_table_append(
                table.ptr(),
                0,
                payload_size,
                hashes.as_ptr(),
                1,
                key,
                payload.as_ptr(),
                &raw mut pending,
                offsets.as_mut_ptr(),
                &raw mut status,
            );
            assert_eq!(code, Code::InvalidArgument, "{payload_size} bytes");
            assert_eq!(pending_words, [0b1111]);
            assert_eq!(table.chunks[0][0], CHUNK_HEADER as u64, "nothing written");
        }
        // A payload of one column takes 16 bytes: its NULL bits and a word.
        let code = tess_table_append_columns(
            table.ptr(),
            0,
            hashes.as_ptr(),
            1,
            &raw const int4,
            1,
            &raw const column,
            &raw mut pending,
            offsets.as_mut_ptr(),
            &raw mut status,
        );
        assert_eq!(code, Code::InvalidArgument);
        assert_eq!(table.chunks[0][0], CHUNK_HEADER as u64, "nothing written");

        // The table's own records go in.
        let code = tess_table_append(
            table.ptr(),
            0,
            8,
            hashes.as_ptr(),
            1,
            &raw const int4,
            payload.as_ptr(),
            &raw mut pending,
            offsets.as_mut_ptr(),
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(pending_words, [0]);
        assert_eq!(table.chunks[0][0], (CHUNK_HEADER + 4 * 32) as u64);

        // Without an index, as a shared build appends, the arguments
        // describe the records.
        let mut bare_chunk = vec![0_u64; (8 + 4 * 40) / 8];
        let code =
            tess_table_chunk_init(bare_chunk.as_mut_ptr().cast(), 8 + 4 * 40, &raw mut status);
        assert_eq!(code, Code::Ok, "{}", status.message());
        let bases = [bare_chunk.as_mut_ptr().cast::<u8>()];
        let lens = [8 + 4 * 40];
        let bare = TableRef {
            index: ptr::null_mut(),
            index_len: 0,
            chunks: bases.as_ptr(),
            chunk_lens: lens.as_ptr(),
            nchunks: 1,
        };
        let mut bare_words = [0b1111];
        let mut bare_pending = Mask {
            nrows: 4,
            bits: bare_words.as_mut_ptr(),
        };
        let code = tess_table_append(
            &raw const bare,
            0,
            16,
            hashes.as_ptr(),
            1,
            &raw const int4,
            payload.as_ptr(),
            &raw mut bare_pending,
            offsets.as_mut_ptr(),
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        assert_eq!(bare_chunk[0], (CHUNK_HEADER + 4 * 40) as u64);
        assert_eq!(bare_words, [0]);
    }
    Ok(())
}

/// A payload of up to 64 columns and one wider, to the most a payload
/// has, goes in through the entry point without an allocation, each word
/// where the format puts it.
#[test]
fn payload_columns_append_without_allocating() -> Result<()> {
    let keys = Keys::new(4);
    let column = keys.column();
    let key = TableKey {
        kind: 1,
        column: &raw const column,
        prepared: ptr::null(),
    };
    let hashes = [0_u32; 4];
    for ncolumns in [64, 65, 130, MAX_PAYLOAD_COLUMNS] {
        let values: Vec<[u64; 4]> = (0..ncolumns as u64)
            .map(|column| [0, 1, 2, 3].map(|row| column * 10 + row))
            .collect();
        let nulls: Vec<[bool; 4]> = (0..ncolumns)
            .map(|column| [0, 1, 2, 3].map(|row| row == column % 5))
            .collect();
        let columns: Vec<DatumColumn> = values
            .iter()
            .zip(&nulls)
            .map(|(values, nulls)| DatumColumn {
                struct_size: size_of::<DatumColumn>(),
                values: values.as_ptr(),
                isnull: nulls.as_ptr(),
                nrows: 4,
                ..DatumColumn::EMPTY
            })
            .collect();
        let null_words = payload_null_words(ncolumns);
        let payload_size = 8 * (null_words + ncolumns);
        let mut status = Status::new();
        // SAFETY: local buffers of the declared sizes, aliased by nothing
        // else, throughout this test.
        unsafe {
            let mut table = CTable::new(1, payload_size, 4);
            table.add_chunk(CHUNK_HEADER + 4 * (24 + payload_size));
            let mut pending_words = [0b1111];
            let mut pending = Mask {
                nrows: 4,
                bits: pending_words.as_mut_ptr(),
            };
            let mut offsets = [0_u32; 4];
            let (code, allocations) = allocations_of(|| {
                tess_table_append_columns(
                    table.ptr(),
                    0,
                    hashes.as_ptr(),
                    1,
                    &raw const key,
                    ncolumns as i32,
                    columns.as_ptr(),
                    &raw mut pending,
                    offsets.as_mut_ptr(),
                    &raw mut status,
                )
            });
            assert_eq!(code, Code::Ok, "{}", status.message());
            assert_eq!(allocations, 0, "{ncolumns} columns");
            assert_eq!(pending_words, [0]);
            for (row, &offset) in offsets.iter().enumerate() {
                let mut record = TableRecord {
                    struct_size: size_of::<TableRecord>(),
                    hash: 0,
                    null_bits: 0,
                    keys: ptr::null(),
                    payload: ptr::null(),
                    payload_size: 0,
                };
                let code = tess_table_record(table.ptr(), offset, &raw mut record, &raw mut status);
                assert_eq!(code, Code::Ok, "{}", status.message());
                let words: Vec<u64> = std::slice::from_raw_parts(record.payload, payload_size)
                    .chunks(8)
                    .map(|word| u64::from_ne_bytes(word.try_into().unwrap()))
                    .collect();
                for column in 0..ncolumns {
                    let null = nulls[column][row];
                    let bit = words[column / 64] >> (column % 64) & 1;
                    assert_eq!(bit, u64::from(null), "column {column}, row {row}");
                    let wanted = if null { 0 } else { values[column][row] };
                    assert_eq!(
                        words[null_words + column],
                        wanted,
                        "column {column}, row {row}"
                    );
                }
            }
        }
    }
    Ok(())
}

/// A call checks the chunks it writes or walks; a debug build of the entry
/// points checks every chunk, so that a probe that touches none of them
/// refuses a chunk longer than 1 MiB there, and goes on in a release build.
#[test]
fn a_debug_build_checks_every_chunk() -> Result<()> {
    let values: Vec<u64> = (1..=4).collect();
    let isnull = [false; 4];
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: values.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 4,
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
    let mut long = vec![0_u64; (MAX_CHUNK_LEN + 8) / 8];
    long[0] = CHUNK_HEADER as u64;
    let mut status = Status::new();
    // SAFETY: local buffers of the declared sizes, aliased by nothing else,
    // throughout this test.
    unsafe {
        let mut table = CTable::new(1, 8, 8);
        table.bases.push(long.as_mut_ptr().cast());
        table.lens.push(MAX_CHUNK_LEN + 8);
        table.refresh();
        let rows_words = [0b1111];
        let rows = Mask {
            nrows: 4,
            bits: rows_words.as_ptr().cast_mut(),
        };
        let mut found_words = [0];
        let mut found = Mask {
            nrows: 4,
            bits: found_words.as_mut_ptr(),
        };
        let mut matches = [0_u32; 4];
        let code = tess_table_probe(
            table.ptr(),
            hashes.as_ptr(),
            1,
            &raw const key,
            &raw const rows,
            matches.as_mut_ptr(),
            &raw mut found,
            &raw mut status,
        );
        if cfg!(debug_assertions) {
            assert_eq!(code, Code::InvalidArgument);
            assert!(status.message().contains("not aligned to 8"));
        } else {
            assert_eq!(code, Code::Ok, "{}", status.message());
            assert_eq!(found_words, [0], "an empty table finds no key");
        }
    }
    Ok(())
}
