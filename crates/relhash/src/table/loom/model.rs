//! The model table, which the loom models of the kernels share, and the
//! steps of the table's own models over the memory of [`super::memory`].

use crate::{RowMask, RowMaskView};
use anyhow::Result;

use super::memory::{HEADS, LoomRegion};
use crate::table::header::{CHUNK_HEADER, Header, Layout, TableConfig, chunk_layout};
use crate::table::record::Access;
use crate::table::region::Region;
use crate::table::testing::IntKeys;
use crate::table::{Batch, batch, init};

/// A table over loom's atomics for the models of participants that share
/// one: chunks of their own to append to, an index one of them makes,
/// then links and probes. Its records have one int4 key, whose value is
/// also their 8-byte payload, and every key hashes to one bucket, so that
/// the participants race for its head.
pub struct ModelTable(LoomRegion);

impl ModelTable {
    /// A table of `chunks` empty chunks of `records` records each, whose
    /// index is not made yet.
    pub fn new(records: usize, chunks: usize) -> Result<Self> {
        LoomRegion::new(records as u64, chunks, records, HEADS).map(Self)
    }

    /// Make the index for `records` records, as the participant elected
    /// to size a shared table does.
    ///
    /// # Safety
    ///
    /// No other thread uses the index during the call.
    pub unsafe fn size(&self, records: u64) -> Result<()> {
        // SAFETY: the caller's contract.
        unsafe { init(&self.0, &CONFIG, records) }.map(drop)
    }

    /// Append `keys` to chunk `chunk`; the rows appended are returned.
    /// The index need not exist yet.
    pub fn append(&self, chunk: usize, keys: &[i32]) -> Result<usize> {
        append(&self.0, &chunk_layout(&CONFIG)?, chunk, keys)
    }

    /// Link chunk `chunk`'s records into the index its header describes;
    /// the count linked is returned. A header that holds no table yet is
    /// an error.
    pub fn link(&self, chunk: usize) -> Result<usize> {
        link(&self.0, &self.layout()?, chunk)
    }

    /// Whether each of `keys` has a record; a record found is read whole
    /// and must hold its key and payload.
    pub fn probe(&self, keys: &[i32]) -> Result<Vec<bool>> {
        let layout = self.layout()?;
        let found = probe(&self.0, &layout, keys)?;
        for (key, &offset) in keys.iter().zip(&found) {
            if offset != 0 {
                check_record(&self.0, &layout, offset, *key)?;
            }
        }
        Ok(found.into_iter().map(|offset| offset != 0).collect())
    }

    /// The layout the header holds, as a participant that attaches reads
    /// it.
    fn layout(&self) -> Result<Layout> {
        Header::load(&self.0).validate(self.0.len())
    }
}

/// A table of `nchunks` chunks of `records` records over the model, its
/// index made by this thread.
#[cfg(test)]
pub(super) fn table(
    nchunks: usize,
    records: usize,
    heads: super::memory::Heads,
) -> Result<(::loom::sync::Arc<LoomRegion>, Layout)> {
    let region = LoomRegion::new(records as u64, nchunks, records, heads)?;
    // SAFETY: the region is new and this thread alone has it.
    let layout = unsafe { init(&region, &CONFIG, records as u64) }?;
    Ok((::loom::sync::Arc::new(region), layout))
}

/// Every row hashes to one bucket, so that the participants race for its
/// head.
pub(super) const HASH: u32 = 0x5a5a_5a5a;
pub(super) const CONFIG: TableConfig = TableConfig {
    nkeys: 1,
    payload_size: 8,
};

/// The payload of a key: its value, so that a reader can check it.
fn payload(keys: &[i32]) -> Vec<u8> {
    keys.iter()
        .flat_map(|key| i64::from(*key).to_ne_bytes())
        .collect()
}

/// Append `keys` to a chunk; the rows appended are returned.
pub(super) fn append(
    region: &LoomRegion,
    layout: &Layout,
    chunk: usize,
    keys: &[i32],
) -> Result<usize> {
    let nrows = keys.len();
    let hashes = vec![HASH; nrows];
    let columns = IntKeys(keys);
    let mut pending_bits = [(1u64 << nrows) - 1];
    let mut pending = RowMask::try_new(nrows, &mut pending_bits)?;
    let mut offsets = vec![0; nrows];
    let payload = payload(keys);
    let mut rows = Batch::new(&hashes, &columns, &mut pending, &mut offsets)?;
    batch::append(region, layout, chunk, Some(&payload), &mut rows)
}

/// Link a chunk's records from its start; the count linked is returned.
pub(super) fn link(region: &LoomRegion, layout: &Layout, chunk: usize) -> Result<usize> {
    let mut from = CHUNK_HEADER;
    batch::link(region, layout, chunk, &mut from)
}

/// Probe for `keys`: the reference of each one's record, 0 for none.
pub(super) fn probe(region: &LoomRegion, layout: &Layout, keys: &[i32]) -> Result<Vec<u32>> {
    let nrows = keys.len();
    let hashes = vec![HASH; nrows];
    let columns = IntKeys(keys);
    let all = [(1u64 << nrows) - 1];
    let rows = RowMaskView::try_new(nrows, &all)?;
    let mut matches = vec![0; nrows];
    let mut found_bits = [0];
    let mut found = RowMask::try_new(nrows, &mut found_bits)?;
    batch::probe(
        region,
        layout,
        &hashes,
        &columns,
        &rows,
        &mut matches,
        &mut found,
    )?;
    Ok((0..nrows)
        .map(|row| {
            if found_bits[0] >> row & 1 == 1 {
                matches[row]
            } else {
                0
            }
        })
        .collect())
}

/// Check the record at `offset` holds `key` and its payload.
pub(super) fn check_record(
    region: &LoomRegion,
    layout: &Layout,
    offset: u32,
    key: i32,
) -> Result<()> {
    let mut access = Access::new(region, layout);
    let view = access.locate(offset)?;
    assert_eq!(view.keys(), [i64::from(key)]);
    assert_eq!(view.payload(), payload(&[key]));
    Ok(())
}
