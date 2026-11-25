//! Any bytes into the reader's decoder of Snappy and into snap's: both
//! give the same bytes, or both an error, and neither reads or writes
//! outside its buffers, which AddressSanitizer reports.
//!
//! The first byte chooses the stream: the fuzzer's bytes after it as
//! they are; or those bytes repeated, with a drift, up to 16 times the
//! first byte's upper six bits in bytes, compressed by snap's encoder,
//! with one byte of the stream changed where the next two bytes say when
//! the first byte asks. Even a short input then gives a stream long
//! enough for the fast loop, of copies at many offsets.
#![no_main]

use libfuzzer_sys::fuzz_target;

fn mine(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = vec![0; tessera_parquet::snappy::decompress_len(input).ok()?];
    let n = tessera_parquet::snappy::decompress(input, &mut out).ok()?;
    assert_eq!(n, out.len());
    Some(out)
}

fn theirs(input: &[u8]) -> Option<Vec<u8>> {
    let len = snap::raw::decompress_len(input).ok()?;
    // A length no stream of this size reaches: snap's error too, found
    // without allocating it.
    if len > input.len() * 22 {
        return None;
    }
    let mut out = vec![0; len];
    let n = snap::raw::Decoder::new().decompress(input, &mut out).ok()?;
    out.truncate(n);
    Some(out)
}

fuzz_target!(|data: &[u8]| {
    let Some((&how, rest)) = data.split_first() else {
        return;
    };
    let stream = if how & 1 == 0 || rest.len() < 3 {
        rest.to_vec()
    } else {
        let len = rest.len() - 2 + 16 * usize::from(how >> 2) * (rest.len() - 2).min(16);
        let data: Vec<u8> = rest[2..]
            .iter()
            .cycle()
            .take(len)
            .enumerate()
            .map(|(i, &b)| b.wrapping_add((i / 251) as u8))
            .collect();
        let mut stream = snap::raw::Encoder::new().compress_vec(&data).unwrap();
        if how & 2 != 0 && !stream.is_empty() {
            let at = usize::from(u16::from_le_bytes([rest[0], rest[1]])) % stream.len();
            stream[at] ^= how | 1;
        }
        stream
    };
    assert_eq!(mine(&stream), theirs(&stream));
});
