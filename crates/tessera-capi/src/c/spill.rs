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
    /// Bytes of a packed chunk of records on disk, 0 for a body as it is.
    pub packed: u32,
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
            packed: self.packed,
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
            packed: block.packed,
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

/// `tess_spill_pack`: pack the `len` bytes of a chunk of records at `chunk`
/// into `out`, of `capacity` bytes: the packed length into `packed`, or 0
/// when the chunk is not one packing takes or would not get shorter.
///
/// # Safety
///
/// `chunk` must be valid for reads of `len` bytes, `out` for writes of
/// `capacity` bytes, and `packed` writable; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_pack(
    chunk: *const u8,
    len: usize,
    out: *mut u8,
    capacity: usize,
    packed: *mut usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let chunk = slice::from_raw_parts(
                (!chunk.is_null())
                    .then_some(chunk)
                    .context("a null chunk")?,
                len,
            );
            let out = slice::from_raw_parts_mut(
                (!out.is_null()).then_some(out).context("a null buffer")?,
                capacity,
            );
            *packed.as_mut().context("a null length")? =
                tessera_spill::pack(chunk, out).unwrap_or(0);
            Ok(())
        })
    }
}

/// `tess_spill_unpack`: unpack the `len` bytes `tess_spill_pack` made at
/// `packed` into the chunk of `chunk_len` bytes at `chunk`.
///
/// # Safety
///
/// `packed` must be valid for reads of `len` bytes and `chunk` for writes
/// of `chunk_len` bytes; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_unpack(
    packed: *const u8,
    len: usize,
    chunk: *mut u8,
    chunk_len: usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let packed = slice::from_raw_parts(
                (!packed.is_null())
                    .then_some(packed)
                    .context("a null packed body")?,
                len,
            );
            let chunk = slice::from_raw_parts_mut(
                (!chunk.is_null())
                    .then_some(chunk)
                    .context("a null chunk")?,
                chunk_len,
            );
            tessera_spill::unpack(packed, chunk)
        })
    }
}
