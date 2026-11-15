//! The proofs by Kani of the crate's checks of the bytes it reads back,
//! for every value the bytes may hold rather than for samples: `cargo
//! kani -p relspill` runs them. Kani also checks every arithmetic
//! overflow, index and panic on the way, so a proof shows too that the
//! check cannot panic. The design names the proof of each trust point.

use crate::{BlockHeader, BlockKind, Bounds, HEADER_SIZE, MAGIC, MAX_LEVEL, VERSION, columns};

/// Any kind of block.
fn any_kind() -> BlockKind {
    match kani::any::<u8>() % 3 {
        0 => BlockKind::Records,
        1 => BlockKind::Values,
        _ => BlockKind::Columns,
    }
}

/// Any bounds of the two kinds.
fn any_bounds() -> Bounds {
    Bounds {
        chunk: kani::any(),
        values: kani::any(),
    }
}

/// Trust point `header`: whatever bytes a header holds, and however short
/// they are, a read refuses them or gives a header whose body is of a
/// length its kind allows, within the bound of its kind, at a level
/// below the most.
#[kani::proof]
fn a_header_that_passes_has_the_lengths_of_its_kind() {
    let bytes: [u8; HEADER_SIZE] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= HEADER_SIZE);
    let fingerprint: u64 = kani::any();
    let bounds = any_bounds();
    let Ok(header) = BlockHeader::read(&bytes[..len], fingerprint, bounds) else {
        return;
    };
    assert!(len == HEADER_SIZE);
    // The bytes are a block of this format, of a kind that exists.
    assert!(bytes[..8] == MAGIC.to_ne_bytes());
    assert!(bytes[8..12] == VERSION.to_ne_bytes());
    assert!(bytes[12..16] == (header.kind as u32).to_ne_bytes());
    assert!(header.fingerprint == fingerprint);
    assert!(header.len.is_multiple_of(8) && header.len <= bounds.of(header.kind));
    assert!(header.level < MAX_LEVEL);
    let packed = u64::from(header.packed);
    match header.kind {
        BlockKind::Records => {
            kani::cover!(packed > 0, "a packed chunk of records passes");
            assert!(header.len >= 8);
            assert!(
                packed == 0 || (packed.is_multiple_of(8) && packed >= 16 && packed < header.len)
            );
        }
        BlockKind::Values => {
            kani::cover!(true, "a chunk of values passes");
            assert!(packed == 0);
        }
        BlockKind::Columns => {
            kani::cover!(true, "a chunk of columns passes");
            assert!(header.len >= columns::HEADER as u64);
            assert!(packed >= 8 && packed.is_multiple_of(8));
        }
    }
}

/// Trust point `header`: a header a writer lays out reads back as it was,
/// and one its reader would refuse, or a buffer too short for, leaves the
/// buffer's bytes as they were.
#[kani::proof]
fn a_header_written_reads_back_as_it_was() {
    let header = BlockHeader {
        kind: any_kind(),
        number: kani::any(),
        partition: kani::any(),
        level: kani::any(),
        fingerprint: kani::any(),
        len: kani::any(),
        packed: kani::any(),
    };
    let bounds = any_bounds();
    // Any bytes before, and a buffer of any length.
    let mut out: [u8; HEADER_SIZE] = kani::any();
    let before = out;
    let len: usize = kani::any();
    kani::assume(len <= HEADER_SIZE);
    if header.write(&mut out[..len], bounds).is_ok() {
        kani::cover!(true, "a header is written");
        assert!(len == HEADER_SIZE);
        assert!(BlockHeader::read(&out, header.fingerprint, bounds) == Ok(header));
    } else {
        assert!(out == before);
    }
}
