//! The C entry points, declared in `include/tessera/kernels.h`,
//! `include/tessera/decimal.h`, `include/tessera/calendar.h`,
//! `include/tessera/text.h`,
//! `include/tessera/table.h`,
//! `include/tessera/spill.h` and `include/tessera/sort.h`.
//!
//! Every entry point takes its inputs as the C structures of the batch
//! contract ([`DatumColumn`], [`Mask`]), runs a kernel under a panic guard,
//! and returns a [`Code`] that it also stores in the caller's [`Status`].
//! The rules of the boundary are in the crate documentation; the C-side
//! contract, in `docs/kernels.md`.

mod args;
mod calendar;
mod cast;
mod column;
mod count;
mod decimal;
mod int32;
mod int64;
mod mask;
mod set;
mod shared_spill;
mod sort;
mod spill;
mod status;
mod table;
mod text;
mod varlena;

pub use calendar::{
    CalendarArg, tess_date_add_interval, tess_date_arith, tess_date_extract,
    tess_date_to_timestamp, tess_timestamp_add_interval, tess_timestamp_extract,
    tess_timestamp_to_date, tess_timestamp_trunc, tess_timestamp_trunc_local,
};
pub use cast::{tess_int4_to_int8, tess_int8_to_int4};
pub use column::DatumColumn;
pub use count::tess_count;
pub use decimal::{
    DecimalArg, DecimalExtreme, DecimalSum, tess_decimal_compute, tess_decimal_extreme,
    tess_decimal_filter, tess_decimal_read, tess_decimal_read_datum, tess_decimal_sum,
    tess_decimal_to_int4, tess_decimal_to_int8, tess_decimal_write, tess_decimal_write_datum,
};
pub use int32::{
    tess_int4_arith_columns, tess_int4_arith_scalar, tess_int4_arith_scalar_left,
    tess_int4_compare_columns, tess_int4_filter, tess_int4_hash, tess_int4_hash_next,
    tess_int4_max, tess_int4_min, tess_int4_sum, tess_kernels_abi_version, tess_kernels_probe,
    tess_kernels_test_panic,
};
pub use int64::{
    tess_int8_arith_columns, tess_int8_arith_scalar, tess_int8_arith_scalar_left,
    tess_int8_compare_columns, tess_int8_filter, tess_int8_hash, tess_int8_hash_next,
    tess_int8_max, tess_int8_min,
};
pub use mask::Mask;
pub use set::{tess_int4_in_set, tess_int8_in_set};
pub use shared_spill::{
    tess_bloom_shared_add, tess_round_step, tess_table_spill_add_bytes, tess_table_spill_evict,
    tess_table_spill_evictions, tess_table_spill_flags, tess_table_spill_init,
    tess_table_spill_partitions, tess_table_spill_records, tess_table_spill_split,
    tess_table_spill_start, tess_table_spill_take_alone, tess_table_spill_take_file,
    tess_table_spill_words,
};
/// The `TESS_SORT_*` flags of a sort key.
pub mod sort_flags {
    pub use super::sort::{DESCENDING, NULLABLE, NULLS_FIRST};
}
pub use sort::{
    CSortKey, tess_sort, tess_sort_item_words, tess_sort_items, tess_sort_key_lanes,
    tess_sort_layout, tess_sort_merge, tess_sort_run_words, tess_sort_top_candidates,
    tess_sort_top_push,
};
pub use spill::{
    SpillHeader, tess_spill_chunk_len, tess_spill_columns_append_partitioned,
    tess_spill_columns_init, tess_spill_columns_layout, tess_spill_columns_pack,
    tess_spill_columns_shape, tess_spill_columns_unpack, tess_spill_header_read,
    tess_spill_header_size, tess_spill_header_write, tess_spill_pack, tess_spill_partitions,
    tess_spill_unpack,
};
pub use status::{Code, MESSAGE_SIZE, Status};
pub use table::{
    TableKey, TableKeys, TableRecord, TableRef, TableStats, TableSumArg, tess_bloom_add,
    tess_bloom_probe, tess_bloom_shared_init, tess_bloom_shared_probe, tess_bloom_shared_ready,
    tess_bloom_shared_words, tess_build_add_duplicates, tess_build_counters_init,
    tess_build_report, tess_build_step, tess_build_take_chunk, tess_build_totals,
    tess_table_accumulate, tess_table_accumulate_sums, tess_table_append,
    tess_table_append_columns, tess_table_append_partitioned_columns, tess_table_bloom,
    tess_table_bloom_words, tess_table_bloom_words_within, tess_table_chunk_init,
    tess_table_clear_key, tess_table_combine, tess_table_create, tess_table_find_or_insert,
    tess_table_find_or_insert_partitioned, tess_table_fingerprint, tess_table_format_version,
    tess_table_gather, tess_table_gather_key, tess_table_gather_scattered, tess_table_gather_words,
    tess_table_layout, tess_table_link, tess_table_link_grouped, tess_table_next_in_group,
    tess_table_next_match, tess_table_payloads, tess_table_probe, tess_table_record,
    tess_table_record_size, tess_table_regrow, tess_table_scan, tess_table_size, tess_table_split,
    tess_table_stats, tess_table_try_build_bloom,
};
pub use text::{
    TextArg, tess_text_compare, tess_text_lengths, tess_text_like, tess_text_pieces,
    tess_text_starts_with,
};
