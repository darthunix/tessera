//! The decoder of Snappy's raw format, by which the reader decompresses
//! the pages of a chunk compressed with Snappy.
//!
//! A raw stream is its decompressed length as a varint of at most 5
//! bytes, then elements. Each element starts with a tag byte whose low
//! two bits give its kind: a literal, whose bytes follow, its length in
//! the tag or in the 1 to 4 bytes after it; or a copy of bytes already
//! written, at an offset back of 1, 2 or 4 bytes after the tag (11 bits
//! with 3 of them in the tag, 16 or 32 bits).
//!
//! The decoding follows that of a whole buffer in Google's snappy 1.2.1
//! (the crate's `LICENSE-snappy`, BSD-3). A fast loop decodes two
//! elements a turn while 130 bytes of input remain and 128 of output,
//! without a branch on an element's kind: a table gives each tag's
//! length and the bits of its offset, an element's bytes are moved as
//! 64 at once into room that later elements overwrite, and each
//! element's copy waits for the next one. It stops at an element it
//! leaves to the careful loop: a long literal, a copy of 4 bytes of
//! offset, or one from before the start. The careful loop, in safe Rust,
//! decodes one element, every length and offset checked, and the rest
//! of a stream near its ends.

use std::{fmt, ptr};

/// A stream the decoder cannot decode: what of it failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Damaged {
    what: &'static str,
}

impl fmt::Display for Damaged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "damaged Snappy stream: {}", self.what)
    }
}

impl std::error::Error for Damaged {}

const fn damaged(what: &'static str) -> Damaged {
    Damaged { what }
}

/// The decompressed length that the header of `input` gives, and the
/// header's length.
fn header(input: &[u8]) -> Result<(usize, usize), Damaged> {
    let mut len = 0u64;
    for (i, &b) in input.iter().take(5).enumerate() {
        len |= u64::from(b & 0x7f) << (7 * i);
        if b < 0x80 {
            if len > u64::from(u32::MAX) {
                return Err(damaged("a length past 2^32 - 1"));
            }
            return Ok((len as usize, i + 1));
        }
    }
    Err(damaged("its header"))
}

/// The decompressed length that the header of `input` gives.
///
/// # Errors
///
/// [`Damaged`] when `input` is empty, its header is not a varint of at
/// most 5 bytes or gives a length past 2^32 - 1, or a length that no
/// stream of `input`'s size can reach: an element of 3 bytes writes at
/// most 64.
pub fn decompress_len(input: &[u8]) -> Result<usize, Damaged> {
    let (len, _) = header(input)?;
    if len > input.len().saturating_mul(22) {
        return Err(damaged("a length past what its elements can write"));
    }
    Ok(len)
}

/// Decompresses the raw stream `input` into `output`, which must be of
/// the length [`decompress_len`] gives, and returns that length.
///
/// # Errors
///
/// [`Damaged`] for a header [`decompress_len`] refuses or a length other
/// than `output`'s, or when an element runs past the end of `input` or
/// of `output`, copies from before the start of `output`, or the
/// elements end short of it. What `output` holds after an error is
/// unspecified.
pub fn decompress(input: &[u8], output: &mut [u8]) -> Result<usize, Damaged> {
    if decompress_len(input)? != output.len() {
        return Err(damaged("a length other than its output's"));
    }
    let (_, start) = header(input)?;
    decode(&input[start..], output)?;
    Ok(output.len())
}

/// The little-endian number of `bytes`.
fn little(bytes: &[u8]) -> usize {
    bytes.iter().rev().fold(0, |n, &b| n * 256 + usize::from(b))
}

/// Decodes the elements of `input` into the whole of `out`: by the fast
/// loop where it can, and by the careful one element by element where it
/// stops.
fn decode(input: &[u8], out: &mut [u8]) -> Result<(), Damaged> {
    let (mut ip, mut op) = (0, 0);
    while ip < input.len() {
        // The fast loop stops short of the input's end, at an element.
        (ip, op) = fast(input, ip, out, op);
        (ip, op) = element(input, ip, out, op)?;
    }
    if op != out.len() {
        return Err(damaged("elements short of its length"));
    }
    Ok(())
}

/// Decodes the element at `ip` of `input` into `out` at `op`, every
/// length and offset checked before it is used, and returns where the
/// next element starts and the end of what is written.
///
/// Requires `ip < input.len()` and `op <= out.len()`.
fn element(input: &[u8], ip: usize, out: &mut [u8], op: usize) -> Result<(usize, usize), Damaged> {
    let tag = input[ip];
    let ip = ip + 1;
    if tag & 3 == 0 {
        let (mut len, mut ip) = (usize::from(tag >> 2) + 1, ip);
        if len > 60 {
            let extra = len - 60;
            let Some(bytes) = input.get(ip..ip + extra) else {
                return Err(damaged("a literal's length past its end"));
            };
            len = little(bytes) + 1;
            ip += extra;
        }
        let Some(from) = input.get(ip..ip + len) else {
            return Err(damaged("a literal past its end"));
        };
        let Some(to) = out.get_mut(op..op + len) else {
            return Err(damaged("a literal past its length"));
        };
        to.copy_from_slice(from);
        return Ok((ip + len, op + len));
    }
    let bytes = [0, 1, 2, 4][usize::from(tag & 3)];
    let Some(after) = input.get(ip..ip + bytes) else {
        return Err(damaged("a copy's offset past its end"));
    };
    let (len, offset) = if tag & 3 == 1 {
        (
            usize::from((tag >> 2) & 7) + 4,
            usize::from(tag >> 5) * 256 + little(after),
        )
    } else {
        (usize::from(tag >> 2) + 1, little(after))
    };
    if offset == 0 || offset > op {
        return Err(damaged("a copy from before its start"));
    }
    if len > out.len() - op {
        return Err(damaged("a copy past its length"));
    }
    copy_back(out, op, offset, len);
    Ok((ip + bytes, op + len))
}

/// `out[op + i] = out[op + i - offset]` for each `i` below `len`: a
/// pattern of `offset` bytes repeated when `offset < len`. Each step
/// copies all of the pattern written so far, twice the last step, so no
/// copy overlaps itself.
///
/// Requires `0 < offset <= op` and `op + len <= out.len()`.
fn copy_back(out: &mut [u8], op: usize, offset: usize, len: usize) {
    let mut done = 0;
    while done < len {
        let n = (offset + done).min(len - done);
        out.copy_within(op - offset..op - offset + n, op + done);
        done += n;
    }
}

/// The bytes the fast loop may read past an element's start, and write
/// past the output's frontier: a literal or a copy is moved as 64 bytes.
const SLOP: usize = 64;

/// For each tag: the element's length in the low byte, less its offset's
/// bits that the tag holds, shifted by 8, as Google's `kLengthMinusOffset`.
/// The length less the offset is then the entry less the offset's bytes
/// after the tag, so that one subtraction both gives it and tells a copy
/// whose offset is less than its length, or an element the fast loop
/// leaves to the careful one: a long literal or a copy of 4 bytes of
/// offset, whose entry 0xFF no offset reaches. A literal gets a false
/// offset of 256, so that it never looks like such a copy.
const TABLE: [i16; 256] = {
    let mut t = [0i16; 256];
    let mut tag = 0;
    while tag < 256 {
        let data = (tag >> 2) as i16;
        t[tag] = match tag & 3 {
            3 => 0xff,
            2 => data + 1,
            1 => (data & 7) + 4 - ((data >> 3) << 8),
            _ if data < 60 => data + 1 - (1 << 8),
            _ => 0xff,
        };
        tag += 1;
    }
    t
};

/// The bytes of an offset after a tag of `kind`: none for a literal or a
/// copy of 4 bytes of offset.
fn offset_mask(kind: usize) -> isize {
    ((0x0000_ffff_00ff_0000u64 >> (kind * 16)) & 0xffff) as isize
}

/// Copies `len` bytes, at most 64, from `src` to `dst`, as 32 or 64:
/// the rest is read and written but overwritten later.
///
/// # Safety
///
/// `[src, src + 64)` is readable and `[dst, dst + 64)` writable; `[src,
/// src + len)` and `[dst, dst + len)` do not overlap.
#[inline(always)]
unsafe fn copy64(dst: *mut u8, src: *const u8, len: usize) {
    // SAFETY: the caller's bounds; a copy that may overlap past `len`.
    unsafe {
        ptr::copy(src, dst, 32);
        if len > 32 {
            ptr::copy(src.add(32), dst.add(32), 32);
        }
    }
}

/// The masks of a shuffle of 16 bytes for a pattern of `size` bytes, from
/// 1 to 16: the first repeats the pattern's bytes from its start, the
/// second turns a repeated pattern into the 16 bytes that follow it.
#[cfg(any(
    all(target_arch = "aarch64", not(miri), not(kani)),
    all(target_arch = "x86_64", target_feature = "ssse3", not(miri), not(kani)),
))]
const MASKS: [[[u8; 16]; 2]; 16] = {
    let mut masks = [[[0u8; 16]; 2]; 16];
    let mut size = 1;
    while size <= 16 {
        let mut i = 0;
        while i < 16 {
            masks[size - 1][0][i] = (i % size) as u8;
            masks[size - 1][1][i] = ((16 + i) % size) as u8;
            i += 1;
        }
        size += 1;
    }
    masks
};

/// Writes 64 bytes at `dst` that repeat the pattern of the `offset`
/// bytes before it; `false` for an offset of 0.
///
/// # Safety
///
/// `[dst - offset, dst + 64)` is in one buffer, writable.
#[cfg(all(target_arch = "aarch64", not(miri), not(kani)))]
#[inline(always)]
unsafe fn pattern64(dst: *mut u8, offset: usize) -> bool {
    use std::arch::aarch64::{vld1q_u8, vqtbl1q_u8, vst1q_u8};
    if offset == 0 {
        return false;
    }
    // SAFETY: the caller's bounds; a load of 16 bytes at `dst - offset`
    // may take bytes past `dst`, which the mask leaves out.
    unsafe {
        if offset <= 16 {
            let [first, next] = &MASKS[offset - 1];
            let mut pattern = vqtbl1q_u8(vld1q_u8(dst.sub(offset)), vld1q_u8(first.as_ptr()));
            let next = vld1q_u8(next.as_ptr());
            for i in 0..4 {
                vst1q_u8(dst.add(16 * i), pattern);
                pattern = vqtbl1q_u8(pattern, next);
            }
        } else {
            for i in 0..4 {
                ptr::copy_nonoverlapping(dst.add(16 * i).sub(offset), dst.add(16 * i), 16);
            }
        }
    }
    true
}

/// Writes 64 bytes at `dst` that repeat the pattern of the `offset`
/// bytes before it; `false` for an offset of 0.
///
/// # Safety
///
/// `[dst - offset, dst + 64)` is in one buffer, writable.
#[cfg(all(target_arch = "x86_64", target_feature = "ssse3", not(miri), not(kani)))]
#[inline(always)]
unsafe fn pattern64(dst: *mut u8, offset: usize) -> bool {
    use std::arch::x86_64::{__m128i, _mm_loadu_si128, _mm_shuffle_epi8, _mm_storeu_si128};
    if offset == 0 {
        return false;
    }
    // SAFETY: the caller's bounds; a load of 16 bytes at `dst - offset`
    // may take bytes past `dst`, which the mask leaves out.
    unsafe {
        if offset <= 16 {
            let [first, next] = &MASKS[offset - 1];
            let source = _mm_loadu_si128(dst.sub(offset).cast::<__m128i>());
            let mut pattern = _mm_shuffle_epi8(source, _mm_loadu_si128(first.as_ptr().cast()));
            let next = _mm_loadu_si128(next.as_ptr().cast());
            for i in 0..4 {
                _mm_storeu_si128(dst.add(16 * i).cast(), pattern);
                pattern = _mm_shuffle_epi8(pattern, next);
            }
        } else {
            for i in 0..4 {
                ptr::copy_nonoverlapping(dst.add(16 * i).sub(offset), dst.add(16 * i), 16);
            }
        }
    }
    true
}

/// Writes 64 bytes at `dst` that repeat the pattern of the `offset`
/// bytes before it; `false` for an offset of 0. The plain code of a build
/// without a shuffle of bytes, and of Miri and Kani.
///
/// # Safety
///
/// `[dst - offset, dst + 64)` is in one buffer, writable.
#[cfg(not(any(
    all(target_arch = "aarch64", not(miri), not(kani)),
    all(target_arch = "x86_64", target_feature = "ssse3", not(miri), not(kani)),
)))]
#[inline(always)]
unsafe fn pattern64(dst: *mut u8, offset: usize) -> bool {
    if offset == 0 {
        return false;
    }
    // SAFETY: the caller's bounds. The first 16 bytes are copied one by
    // one, each from a byte written before it; then each block of 16 from
    // `o` bytes back, a multiple of the pattern of at least 16, already
    // written.
    unsafe {
        if offset < 16 {
            for i in 0..16 {
                *dst.add(i) = *dst.add(i).sub(offset);
            }
            let o = (16 / offset + 1) * offset;
            for i in 1..4 {
                ptr::copy_nonoverlapping(dst.add(16 * i).sub(o), dst.add(16 * i), 16);
            }
        } else {
            for i in 0..4 {
                ptr::copy_nonoverlapping(dst.add(16 * i).sub(offset), dst.add(16 * i), 16);
            }
        }
    }
    true
}

/// The fast loop between two elements: `ip` just past the next element's
/// tag, `tag` that tag, `op` the end of what is written, and the copy of
/// the element before, `len_from` bytes from `from`, which waits. `ip` is
/// a pointer, not an index, so that no addition stands between the load
/// of one tag and that of the next.
struct Fast {
    ip: *const u8,
    tag: usize,
    op: usize,
    from: *const u8,
    len_from: usize,
}

/// Decodes the next element in the fast loop into `ob`: writes the copy
/// that waits and leaves this element's waiting. `false`, `ip` back at the
/// element, for one the careful loop decodes: a long literal, a copy of 4
/// bytes of offset, a copy from before the start or of an offset of 0.
///
/// # Safety
///
/// `ip` is readable for 66 bytes; `ob` is writable for 128 bytes past
/// `op` plus the waiting copy's length, at most 64; the waiting copy's
/// source is readable for 64 bytes: `none`, of 64 bytes, or in the input
/// before `ip`, or in `ob` ending at or before `op`.
#[inline(always)]
unsafe fn step(s: &mut Fast, ob: *mut u8, none: *const u8) -> bool {
    // SAFETY: the caller's bounds. A literal's tag is read up to 64 bytes
    // past `ip`, a copy's 3; the copy that waits writes 64 bytes at `op`.
    unsafe {
        let at = s.ip;
        let entry = isize::from(TABLE[s.tag]);
        let kind = s.tag & 3;
        let literal_tag = *at.add((s.tag >> 2) + 1);
        let copy_tag = *at.add(kind);
        (s.tag, s.ip) = if kind == 0 {
            (usize::from(literal_tag), at.add((s.tag >> 2) + 2))
        } else {
            (usize::from(copy_tag), at.add(kind + 1))
        };
        let next = u16::from_le_bytes([*at, *at.add(1)]) as isize;
        let len = (entry & 0xff) as usize;
        let less = entry - (next & offset_mask(kind));
        let back = (s.op + s.len_from) as isize + less - len as isize;
        if less > 0 {
            // A copy whose offset is less than its length, or an element
            // for the careful loop: rare.
            std::hint::cold_path();
            if len & 0x80 != 0 {
                s.ip = at;
                return false;
            }
            copy64(ob.add(s.op), s.from, s.len_from);
            s.op += s.len_from;
            (s.from, s.len_from) = (none, 0);
            if back < 0 || !pattern64(ob.add(s.op), (len as isize - less) as usize) {
                s.ip = at;
                return false;
            }
            s.op += len;
            return true;
        }
        // Before the start: a copy goes to the careful loop, a literal of
        // the first 256 bytes, its false offset of 256 past the start, is
        // moved as any. Rare, so that the kind is never a branch here.
        if back < 0 {
            std::hint::cold_path();
            if kind != 0 {
                s.ip = at;
                return false;
            }
        }
        copy64(ob.add(s.op), s.from, s.len_from);
        s.op += s.len_from;
        s.from = if kind == 0 { at } else { ob.add(back as usize) };
        s.len_from = len;
        true
    }
}

/// Decodes the elements of `input` from `ip` into `out` from `op`, two a
/// turn, while 130 bytes of input remain and 128 of output; returns where
/// it stopped: at an element the careful loop decodes, or near an end.
fn fast(input: &[u8], ip: usize, out: &mut [u8], op: usize) -> (usize, usize) {
    if input.len() - ip <= 2 * (SLOP + 1)
        || out.len() < 2 * SLOP
        || op >= out.len() - (2 * SLOP - 1)
    {
        return (ip, op);
    }
    let out_len = out.len();
    let out_limit = out_len - (2 * SLOP - 1);
    let ib = input.as_ptr();
    let ob = out.as_mut_ptr();
    let none = [0u8; SLOP];
    // SAFETY: the guard of each turn, `ip` below `in_limit` and `op` with
    // the waiting copy below `out_limit`, gives the first `step` its
    // bounds; it moves `ip` at most 61 and `op` with the waiting copy at
    // most 64, which leaves the second its own. `in_limit` and every `ip`
    // lie in the input.
    unsafe {
        let in_limit = ib.add(input.len() - (2 * SLOP + 1));
        let mut s = Fast {
            ip: ib.add(ip + 1),
            tag: usize::from(input[ip]),
            op,
            from: none.as_ptr(),
            len_from: 0,
        };
        loop {
            // The room the proof of a turn takes for granted, counted
            // from the buffers' lengths rather than the limits above, so
            // that a test sees a guard that lets a turn start without it.
            debug_assert!(
                input.len() - s.ip.offset_from_unsigned(ib) >= 2 * SLOP + 2
                    && out_len - (s.op + s.len_from) >= 2 * SLOP
                    && s.len_from <= SLOP,
                "a turn of the fast loop without its room"
            );
            if !(step(&mut s, ob, none.as_ptr()) && step(&mut s, ob, none.as_ptr())) {
                break;
            }
            if s.ip >= in_limit || s.op + s.len_from >= out_limit {
                break;
            }
        }
        if s.len_from != 0 {
            copy64(ob.add(s.op), s.from, s.len_from);
            s.op += s.len_from;
        }
        (s.ip.offset_from_unsigned(ib) - 1, s.op)
    }
}

#[cfg(kani)]
mod proofs;

#[cfg(test)]
mod tests;
