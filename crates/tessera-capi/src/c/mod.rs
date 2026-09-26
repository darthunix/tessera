//! The C entry points, declared in `include/tessera/kernels.h` and
//! `include/tessera/table.h`.
//!
//! Every entry point takes its inputs as the C structures of the batch
//! contract ([`DatumColumn`], [`Mask`]), runs a kernel under a panic guard,
//! and returns a [`Code`] that it also stores in the caller's [`Status`].
//! The rules of the boundary are in the crate documentation; the C-side
//! contract, in `docs/kernels.md`.

mod args;
mod cast;
mod column;
mod count;
mod int32;
mod int64;
mod mask;
mod status;
mod table;

pub use cast::{tess_int4_to_int8, tess_int8_to_int4};
pub use column::DatumColumn;
pub use count::tess_count;
pub use int32::{
    tess_int4_arith_columns, tess_int4_arith_scalar, tess_int4_arith_scalar_left,
    tess_int4_compare_columns, tess_int4_count, tess_int4_filter, tess_int4_hash,
    tess_int4_hash_next, tess_int4_max, tess_int4_min, tess_int4_sum, tess_kernels_abi_version,
    tess_kernels_layout, tess_kernels_test_panic,
};
pub use int64::{
    tess_int8_arith_columns, tess_int8_arith_scalar, tess_int8_arith_scalar_left,
    tess_int8_compare_columns, tess_int8_filter, tess_int8_hash, tess_int8_hash_next,
    tess_int8_max, tess_int8_min,
};
pub use mask::Mask;
pub use status::{Code, MESSAGE_SIZE, Status};
pub use table::{
    TableKey, TableKeys, TableRecord, TableRef, TableStats, tess_bloom_probe,
    tess_bloom_shared_init, tess_bloom_shared_probe, tess_bloom_shared_ready,
    tess_bloom_shared_words, tess_build_add_duplicates, tess_build_counters_init,
    tess_build_report, tess_build_step, tess_build_take_chunk, tess_build_totals,
    tess_table_accumulate, tess_table_append, tess_table_attach, tess_table_bloom,
    tess_table_bloom_words, tess_table_chunk_init, tess_table_create, tess_table_find_or_insert,
    tess_table_format_version, tess_table_gather, tess_table_gather_key, tess_table_layout,
    tess_table_link, tess_table_link_grouped, tess_table_next_in_group, tess_table_next_match,
    tess_table_payload, tess_table_probe, tess_table_record, tess_table_regrow, tess_table_scan,
    tess_table_size, tess_table_stats, tess_table_try_build_bloom,
};
