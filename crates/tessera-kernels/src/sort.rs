//! Ordering rows by integer keys, as sort items of whole 64-bit words.
//!
//! A row's keys become one string of bits whose unsigned order is the
//! rows' order: each key, in key order, as its value turned unsigned with
//! the order kept (the sign bit flipped; every bit inverted when the key
//! is descending), after one bit that puts a NULL first or last when the
//! key may be NULL. The row's 32-bit reference follows in the last word's
//! low bits, so that equal keys stay distinct and the sorted items give
//! the references back in order. An item is as many words as the bits
//! need ([`item_words`]): one for an int4 key, two for an int8 one or up
//! to three int4 keys, and the items sort as arrays of words, the first
//! word most significant, with the standard library's unstable sort. A
//! new type of key needs only its transform into bits with the order
//! kept.
//!
//! The kernels allocate nothing: the caller hands the items' words and
//! the output of references in.

use anyhow::{Result, bail, ensure};

use crate::table::{KeyKind, MAX_KEYS};

/// Bits of a record reference in an item.
pub const REFERENCE_BITS: u32 = 32;

/// The most words an item may take: every key an int8 that may be NULL.
pub const MAX_ITEM_WORDS: usize = (MAX_KEYS * 65 + REFERENCE_BITS as usize).div_ceil(64);

/// How rows are ordered by one key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SortKey {
    /// The key's kind, as the records hold it.
    pub kind: KeyKind,
    /// Larger values first.
    pub descending: bool,
    /// NULL before every value, rather than after.
    pub nulls_first: bool,
    /// Whether a row may hold NULL in this key; a key that may not takes
    /// no bit for it, and a NULL there is an error.
    pub nullable: bool,
}

impl SortKey {
    fn value_bits(&self) -> u32 {
        match self.kind {
            KeyKind::Int32 => 32,
            KeyKind::Int64 => 64,
        }
    }

    fn bits(&self) -> u32 {
        self.value_bits() + u32::from(self.nullable)
    }

    /// The key's value as bits whose unsigned order is the key's order.
    #[inline(always)]
    fn encode(&self, slot: i64) -> u64 {
        match self.kind {
            KeyKind::Int32 => {
                let bits = u64::from((slot as i32 as u32) ^ 0x8000_0000);
                if self.descending {
                    !bits & 0xFFFF_FFFF
                } else {
                    bits
                }
            }
            KeyKind::Int64 => {
                let bits = (slot as u64) ^ (1 << 63);
                if self.descending { !bits } else { bits }
            }
        }
    }
}

/// The words of one item for `keys`: their bits and the reference's.
pub fn item_words(keys: &[SortKey]) -> Result<usize> {
    ensure!(
        (1..=MAX_KEYS).contains(&keys.len()),
        "a sort takes 1 to {MAX_KEYS} keys, not {}",
        keys.len()
    );
    let bits: u32 = keys.iter().map(SortKey::bits).sum::<u32>() + REFERENCE_BITS;
    Ok(bits.div_ceil(64) as usize)
}

/// Writes items: one row's keys from the first word's high bits down.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Encoder<'k> {
    keys: &'k [SortKey],
    words: usize,
}

impl<'k> Encoder<'k> {
    pub(crate) fn new(keys: &'k [SortKey]) -> Result<Self> {
        Ok(Self {
            keys,
            words: item_words(keys)?,
        })
    }

    /// Words of one item.
    pub(crate) fn words(&self) -> usize {
        self.words
    }

    /// Write the item of a row with key slots `slots`, the keys whose bit
    /// is set in `null_bits` NULL, and reference `reference` into `item`,
    /// of [`Self::words`] words.
    #[inline]
    pub(crate) fn encode(
        &self,
        slots: &[i64],
        null_bits: u32,
        reference: u32,
        item: &mut [u64],
    ) -> Result<()> {
        item.fill(0);
        let mut at = 0;
        for (index, key) in self.keys.iter().enumerate() {
            let null = (null_bits >> index) & 1 == 1;
            if key.nullable {
                put(item, &mut at, u64::from(null != key.nulls_first), 1);
            } else if null {
                bail!("sort key {index} holds a NULL but was declared not nullable");
            }
            let value = if null { 0 } else { key.encode(slots[index]) };
            put(item, &mut at, value, key.value_bits());
        }
        item[self.words - 1] |= u64::from(reference);
        Ok(())
    }
}

/// Write the low `width` bits of `value` at bit `*at`, counted from the
/// first word's most significant bit, and move `*at` past them.
#[inline(always)]
fn put(item: &mut [u64], at: &mut u32, value: u64, width: u32) {
    let word = (*at / 64) as usize;
    let used = *at % 64;
    let room = 64 - used;
    if width <= room {
        item[word] |= value << (room - width);
    } else {
        let low = width - room;
        item[word] |= value >> low;
        item[word + 1] |= value << (64 - low);
    }
    *at += width;
}

/// Sort `items`, items of `words` words each one after another, and write
/// each one's reference in order into `out`, which must hold one per
/// item.
pub fn sort_items(items: &mut [u64], words: usize, out: &mut [u32]) -> Result<()> {
    ensure!(
        (1..=MAX_ITEM_WORDS).contains(&words),
        "a sort item has 1 to {MAX_ITEM_WORDS} words, not {words}"
    );
    ensure!(
        items.len().is_multiple_of(words) && out.len() == items.len() / words,
        "{} words are not {} items of {words} words",
        items.len(),
        out.len()
    );
    macro_rules! dispatch {
        ($($n:literal)*) => {
            match words {
                $($n => sort_as::<$n>(items, out),)*
                _ => unreachable!("the width was checked"),
            }
        };
    }
    dispatch!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17);
    Ok(())
}

fn sort_as<const W: usize>(items: &mut [u64], out: &mut [u32]) {
    let (items, rest) = items.as_chunks_mut::<W>();
    debug_assert!(rest.is_empty());
    items.sort_unstable();
    for (item, reference) in items.iter().zip(out.iter_mut()) {
        *reference = item[W - 1] as u32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASC: SortKey = SortKey {
        kind: KeyKind::Int32,
        descending: false,
        nulls_first: false,
        nullable: false,
    };

    #[test]
    fn the_widest_item_fits() {
        let keys = [SortKey {
            kind: KeyKind::Int64,
            nullable: true,
            ..ASC
        }; MAX_KEYS];
        assert_eq!(item_words(&keys).unwrap(), MAX_ITEM_WORDS);
        // sort_items dispatches up to 17 words.
        assert_eq!(MAX_ITEM_WORDS, 17);
    }

    #[test]
    fn bits_cross_words() {
        let mut item = [0u64; 2];
        let mut at = 60;
        put(&mut item, &mut at, 0xABCD, 16);
        assert_eq!(item, [0xA, 0xBCD << 52]);
        assert_eq!(at, 76);
    }
}
