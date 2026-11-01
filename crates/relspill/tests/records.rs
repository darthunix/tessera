//! A chunk of records that a table of `relhash` wrote, packed for disk,
//! unpacked, and linked into another table.

use relhash::{
    Batch, CHUNK_HEADER, Chunks, KeySource, Matches, RECORD_NEXT_OFFSET, TableConfig, TableMut,
    index_size, ones,
};

/// Keys of a word a row, one column a key; the last key is NULL where
/// the row's bit of `non_null` is clear.
struct Keys<'a> {
    columns: &'a [Vec<i64>],
    non_null: u64,
}

impl KeySource for Keys<'_> {
    fn nkeys(&self) -> usize {
        self.columns.len()
    }
    fn nrows(&self) -> usize {
        self.columns[0].len()
    }
    fn word(&self, key: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Option<u64> {
        let present = if key + 1 == self.columns.len() {
            selected & self.non_null
        } else {
            selected
        };
        for bit in ones(selected) {
            out[bit] = if present >> bit & 1 == 1 {
                self.columns[key][index * 64 + bit]
            } else {
                0
            };
        }
        Some(present)
    }
}

/// The bytes of a chunk's words.
fn bytes(words: &[u64]) -> Vec<u8> {
    words.iter().flat_map(|word| word.to_ne_bytes()).collect()
}

#[test]
fn a_chunk_a_table_wrote_comes_back_and_links_again() -> anyhow::Result<()> {
    // The shortest record, 24 bytes; one of 32; one of 48 whose payload
    // of 5 bytes is padded.
    for (nkeys, payload_size) in [(1, 0), (1, 8), (3, 5)] {
        round_trip(&TableConfig {
            nkeys,
            payload_size,
        })?;
    }
    Ok(())
}

/// 64 rows of `config` into a chunk, packed, unpacked and linked again.
fn round_trip(config: &TableConfig) -> anyhow::Result<()> {
    let columns: Vec<Vec<i64>> = (0..config.nkeys as i64)
        .map(|key| {
            (0..64)
                .map(|row| row * 1_000_003 - 40 + (key << 40))
                .collect()
        })
        .collect();
    let keys = Keys {
        columns: &columns,
        non_null: !0x0101_0101_0101_0101,
    };
    // Eight hashes, so that the records' next fields form chains.
    let hashes: Vec<u32> = (0..64_u32).map(|row| (row % 8) << 29 | row).collect();
    let width = config.payload_size;
    let payload: Vec<u8> = (0..64 * width).map(|byte| (byte * 7) as u8).collect();

    let mut index = vec![0_u64; index_size(config, 64)? / 8];
    let mut chunk = vec![0_u64; 4096 / 8];
    let (mut pending, mut refs) = ([u64::MAX], [0_u32; 64]);
    // SAFETY: the index and the chunk are these vectors, apart, aligned to
    // 8 and alive while the table is, and nothing else uses them.
    unsafe {
        let bases = [chunk.as_mut_ptr().cast::<u8>()];
        let lens = [chunk.len() * 8];
        Chunks::init(bases[0], lens[0])?;
        let chunks = Chunks::new(&bases, &lens)?;
        let mut table = TableMut::create(
            index.as_mut_ptr().cast(),
            index.len() * 8,
            config,
            64,
            chunks,
        )?;
        let mut batch = Batch::new(&hashes, &keys, &mut pending, &mut refs)?;
        table.append(0, (width > 0).then_some(&payload[..]), &mut batch)?;
        table.link(0, &mut CHUNK_HEADER.clone())?;
    }
    let written = bytes(&chunk);
    let used = u64::from_ne_bytes(written[..8].try_into()?) as usize;
    let records = &written[..used];

    let mut packed = vec![0_u8; used];
    let len = relspill::pack(records, &mut packed)
        .ok_or_else(|| anyhow::anyhow!("the records of {config:?} do not pack"))?;
    let mut back = vec![0_u8; used];
    relspill::unpack(&packed[..len], &mut back)?;
    // Byte for byte, except the next fields, which a chunk no table links
    // has as zero.
    let size = (used - CHUNK_HEADER) / 64;
    let mut expected = records.to_vec();
    let mut chained = 0;
    for record in 0..64 {
        let at = CHUNK_HEADER + record * size + RECORD_NEXT_OFFSET;
        chained += usize::from(expected[at..at + 4] != [0; 4]);
        expected[at..at + 4].fill(0);
    }
    assert_eq!(chained, 56, "every record but the first of a chain links");
    assert_eq!(back, expected, "{config:?}");

    // Another table links the chunk read back and finds every row.
    let mut index = vec![0_u64; index_size(config, 64)? / 8];
    let mut chunk: Vec<u64> = back
        .as_chunks::<8>()
        .0
        .iter()
        .map(|&word| u64::from_ne_bytes(word))
        .collect();
    let (mut rows, mut found) = ([u64::MAX], [0_u32; 64]);
    // SAFETY: as above.
    unsafe {
        let bases = [chunk.as_mut_ptr().cast::<u8>()];
        let lens = [chunk.len() * 8];
        let chunks = Chunks::new(&bases, &lens)?;
        let mut table = TableMut::create(
            index.as_mut_ptr().cast(),
            index.len() * 8,
            config,
            64,
            chunks,
        )?;
        table.link(0, &mut CHUNK_HEADER.clone())?;
        let mut matches = Matches::new(&mut rows, &mut found)?;
        assert_eq!(table.probe(&hashes, &keys, &mut matches)?, 64);
        assert_eq!(matches.refs(), refs);
        for (row, &reference) in refs.iter().enumerate() {
            let record = table.record(reference)?;
            assert_eq!(record.hash, hashes[row]);
            assert_eq!(record.payload, &payload[row * width..(row + 1) * width]);
        }
    }
    Ok(())
}
