//! The spill entry points, declared in `include/tessera/spill.h`: the
//! headers of spilled blocks ([`tessera_spill`]), which carry a table's
//! layout fingerprint (`tess_table_fingerprint`).

use std::slice;

use anyhow::{Context, bail};
use tessera_spill::{BlockHeader, BlockKind, HEADER_SIZE};

use super::status::{Code, Status, guard};

/// `TessSpillHeader`: what a spilled block holds.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SpillHeader {
    /// A `TessSpillKind`: 1 records, 2 values.
    pub kind: u32,
    pub number: u32,
    pub partition: u32,
    pub level: u32,
    pub fingerprint: u64,
    /// Bytes of the body after the header.
    pub len: u64,
}

impl SpillHeader {
    fn to_block(self) -> anyhow::Result<BlockHeader> {
        let kind = match self.kind {
            1 => BlockKind::Records,
            2 => BlockKind::Values,
            other => bail!("a spilled block of unknown kind {other}"),
        };
        Ok(BlockHeader {
            kind,
            number: self.number,
            partition: self.partition,
            level: self.level,
            fingerprint: self.fingerprint,
            len: self.len,
        })
    }

    fn from_block(block: BlockHeader) -> Self {
        Self {
            kind: block.kind as u32,
            number: block.number,
            partition: block.partition,
            level: block.level,
            fingerprint: block.fingerprint,
            len: block.len,
        }
    }
}

/// `tess_spill_header_size`: bytes of a spilled block's header.
#[unsafe(no_mangle)]
pub extern "C" fn tess_spill_header_size() -> usize {
    HEADER_SIZE
}

/// `tess_spill_header_write`: lay out the header of a block in the first
/// `tess_spill_header_size` bytes at `out`, checked as a reader would
/// with `max_len` the most body bytes it accepts.
///
/// # Safety
///
/// `out` must be valid for writes of `len` bytes and `header` point to a
/// header; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_header_write(
    out: *mut u8,
    len: usize,
    header: *const SpillHeader,
    max_len: u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let header = header.as_ref().context("a null spill header")?.to_block()?;
            let out = slice::from_raw_parts_mut(
                (!out.is_null()).then_some(out).context("a null buffer")?,
                len,
            );
            header.write(out, max_len)
        })
    }
}

/// `tess_spill_header_read`: read and check the header at `bytes`: its
/// magic, version, kind, the table's `fingerprint` and a body of at most
/// `max_len` bytes.
///
/// # Safety
///
/// `bytes` must be valid for reads of `len` bytes and `header` writable;
/// `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_header_read(
    bytes: *const u8,
    len: usize,
    fingerprint: u64,
    max_len: u64,
    header: *mut SpillHeader,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let bytes = slice::from_raw_parts(
                (!bytes.is_null())
                    .then_some(bytes)
                    .context("a null buffer")?,
                len,
            );
            let block = BlockHeader::read(bytes, fingerprint, max_len)?;
            *header.as_mut().context("a null spill header")? = SpillHeader::from_block(block);
            Ok(())
        })
    }
}
