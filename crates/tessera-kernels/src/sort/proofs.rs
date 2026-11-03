//! The proofs by Kani of a sort item: for any two rows, any directions
//! and NULL places of their keys, any slots, NULL bits and references,
//! through [`Encoder::encode_with`], the writing of [`Encoder::encode`]:
//!
//! - a NULL in a key declared not nullable is refused, the first such key
//!   by its index;
//! - otherwise the two items compare, word by word, as the rows compare
//!   by their keys, each key's NULL before or after every value as the
//!   key says and a descending key reversed, and then by reference;
//! - their key bits alone ([`key_bits`]), what a run of an external sort
//!   keeps, compare by the keys only.
//!
//! An int4 key is compared by the low 32 bits of its slot, which hold
//! its value. Bounds: six shapes of keys, which reach each branch of the
//! packing of bits into words, a key inside a word, ending on its last
//! bit, and split across two; they do not prove every layout of up to 16
//! keys. Each harness checks that its keys' bits and the reference take
//! exactly its words.

use std::cmp::Ordering;

use super::{Encoder, KeyKind, SortKey, key_bits};

/// One key's order of two rows, a NULL as a value of its own.
fn key_order(key: &SortKey, a: Option<i64>, b: Option<i64>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => {
            if key.nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Some(_), None) => {
            if key.nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Some(x), Some(y)) => {
            let (x, y) = match key.kind {
                KeyKind::Int32 => (i64::from(x as i32), i64::from(y as i32)),
                KeyKind::Int64 => (x, y),
            };
            if key.descending { y.cmp(&x) } else { x.cmp(&y) }
        }
    }
}

/// Two items word by word, the first word most significant, as a sort
/// orders them: by hand, since Kani cannot follow a slice comparison.
fn compare<const W: usize>(a: &[u64; W], b: &[u64; W]) -> Ordering {
    let mut index = 0;
    while index < W {
        let order = a[index].cmp(&b[index]);
        if order != Ordering::Equal {
            return order;
        }
        index += 1;
    }
    Ordering::Equal
}

/// A key of `kind`, nullable or not, of any direction and NULL place.
fn key(kind: KeyKind, nullable: bool) -> SortKey {
    SortKey {
        kind,
        descending: kani::any(),
        nulls_first: kani::any(),
        nullable,
    }
}

/// How two rows compared: refused, or their order by keys, whether their
/// references differ, and whether either held a NULL.
struct Compared {
    refused: bool,
    by_keys: Ordering,
    references_differ: bool,
    nulls: bool,
}

/// The proof over two rows of `keys`, whose item takes `W` words.
fn rows_order_as_their_items<const W: usize, const K: usize>(keys: [SortKey; K]) -> Compared {
    let bits: u32 = keys
        .iter()
        .map(|key| key.value_bits() + u32::from(key.nullable))
        .sum();
    // The item takes the fewest words its bits need, as item_words says.
    assert!((bits + 32).div_ceil(64) == W as u32);
    let encoder = Encoder {
        keys: &keys,
        words: W,
    };
    let mask = (1u32 << K) - 1;
    let (slots_a, slots_b): ([i64; K], [i64; K]) = (kani::any(), kani::any());
    let (nulls_a, nulls_b) = (kani::any::<u32>() & mask, kani::any::<u32>() & mask);
    let (reference_a, reference_b): (u32, u32) = (kani::any(), kani::any());
    let refused =
        |nulls: u32| (0..K).find(|&index| !keys[index].nullable && nulls >> index & 1 == 1);
    let a = encoder.encode_with::<W, usize>(&slots_a, nulls_a, reference_a, |index| index);
    let b = encoder.encode_with::<W, usize>(&slots_b, nulls_b, reference_b, |index| index);
    let (a, b) = match (a, b) {
        (Ok(a), Ok(b)) => (a, b),
        (a, b) => {
            assert!(a.err() == refused(nulls_a) && b.err() == refused(nulls_b));
            return Compared {
                refused: true,
                by_keys: Ordering::Equal,
                references_differ: false,
                nulls: true,
            };
        }
    };
    assert!(refused(nulls_a).is_none() && refused(nulls_b).is_none());
    let value = |slots: &[i64; K], nulls: u32, index: usize| {
        (nulls >> index & 1 == 0).then_some(slots[index])
    };
    let mut by_keys = Ordering::Equal;
    for (index, key) in keys.iter().enumerate() {
        by_keys = by_keys.then(key_order(
            key,
            value(&slots_a, nulls_a, index),
            value(&slots_b, nulls_b, index),
        ));
    }
    assert!(compare(&a, &b) == by_keys.then(reference_a.cmp(&reference_b)));
    assert!(compare(&key_bits(a), &key_bits(b)) == by_keys);
    Compared {
        refused: false,
        by_keys,
        references_differ: reference_a != reference_b,
        nulls: nulls_a != 0 || nulls_b != 0,
    }
}

/// The covers every shape reaches: each order by keys, equal keys told
/// apart by their references.
fn cover_orders(compared: &Compared) {
    kani::cover!(!compared.refused && compared.by_keys == Ordering::Less);
    kani::cover!(!compared.refused && compared.by_keys == Ordering::Greater);
    kani::cover!(
        !compared.refused && compared.by_keys == Ordering::Equal && compared.references_differ
    );
}

/// One int4, not nullable: 64 bits with the reference, one word.
#[kani::proof]
#[kani::unwind(5)]
fn one_int4() {
    let compared = rows_order_as_their_items::<1, 1>([key(KeyKind::Int32, false)]);
    cover_orders(&compared);
    kani::cover!(compared.refused);
}

/// One int8, not nullable: the key fills the first word exactly.
#[kani::proof]
#[kani::unwind(5)]
fn one_int8_fills_a_word() {
    let compared = rows_order_as_their_items::<2, 1>([key(KeyKind::Int64, false)]);
    cover_orders(&compared);
    kani::cover!(compared.refused);
}

/// Two int4, not nullable: the second ends on the first word's last bit.
#[kani::proof]
#[kani::unwind(5)]
fn two_int4_fill_a_word() {
    let compared =
        rows_order_as_their_items::<2, 2>([key(KeyKind::Int32, false), key(KeyKind::Int32, false)]);
    cover_orders(&compared);
    kani::cover!(compared.refused);
}

/// An int8 and an int4, nullable: 130 bits, three words.
#[kani::proof]
#[kani::unwind(5)]
fn an_int8_and_an_int4() {
    let compared =
        rows_order_as_their_items::<3, 2>([key(KeyKind::Int64, true), key(KeyKind::Int32, true)]);
    cover_orders(&compared);
    kani::cover!(!compared.refused && compared.nulls);
}

/// Two int8 and an int4, nullable: 195 bits, four words, keys across both
/// boundaries.
#[kani::proof]
#[kani::unwind(5)]
fn two_int8_and_an_int4() {
    let compared = rows_order_as_their_items::<4, 3>([
        key(KeyKind::Int64, true),
        key(KeyKind::Int64, true),
        key(KeyKind::Int32, true),
    ]);
    cover_orders(&compared);
    kani::cover!(!compared.refused && compared.nulls);
}

/// Three int4, nullable: 131 bits, three words.
#[kani::proof]
#[kani::unwind(5)]
fn three_int4() {
    let compared = rows_order_as_their_items::<3, 3>([
        key(KeyKind::Int32, true),
        key(KeyKind::Int32, true),
        key(KeyKind::Int32, true),
    ]);
    cover_orders(&compared);
    kani::cover!(!compared.refused && compared.nulls);
}
