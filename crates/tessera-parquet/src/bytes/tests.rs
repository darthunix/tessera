use proptest::prelude::*;

use super::*;

fn each_passes(values: &[Option<Vec<u8>>]) -> bool {
    values
        .iter()
        .flatten()
        .all(|v| !v.contains(&0) && std::str::from_utf8(v).is_ok())
}

fn views(values: &[Option<Vec<u8>>]) -> BinaryViewArray {
    values.iter().map(Option::as_deref).collect()
}

/// Each value of a view array checked by itself: halves of a character in
/// two values fail, a zero byte fails in a short value and in a long one,
/// and a NULL slot is not read.
#[test]
fn views_checked_value_by_value() {
    let some = |v: &[u8]| Some(v.to_vec());
    assert!(!views_pass(&views(&[some(b"a\xc3"), some(b"\xa9b")])));
    assert!(views_pass(&views(&[
        some("é".as_bytes()),
        some(b""),
        some(b"z")
    ])));
    assert!(views_pass(&views(&[])), "no value");
    assert!(!views_pass(&views(&[some(b"twelve\0bytes")])));
    assert!(!views_pass(&views(&[some(b"a long value with \0 inside")])));
    assert!(!views_pass(&views(&[some(
        &"é".repeat(7).as_bytes()[..13]
    )])));
    assert!(views_pass(&views(&[some("é".repeat(7).as_bytes()), None])));
    let nulled = views(&[some(b"\xff"), some(b"ok")]);
    let nulled = BinaryViewArray::new(
        nulled.views().clone(),
        nulled.data_buffers().to_vec(),
        Some(arrow_buffer::NullBuffer::from(vec![false, true])),
    );
    assert!(views_pass(&nulled), "a NULL slot's bytes are not read");
}

/// One value's check by words against the checks of UTF-8 and of zero
/// bytes, for every length up to 40: a zero byte, a byte that is not
/// UTF-8 and a character of two bytes at each place, and the value of
/// ASCII alone.
#[test]
fn a_value_checked_by_words_at_every_place() {
    let each = |v: &[u8]| !v.contains(&0) && std::str::from_utf8(v).is_ok();
    for len in 0..40 {
        let ascii = vec![b'a'; len];
        assert!(value_passes(&ascii), "{len}");
        for at in 0..len {
            for (bytes, width) in [
                (&b"\0"[..], 1),
                (b"\xff", 1),
                (b"\x80", 1),
                ("é".as_bytes(), 2),
            ] {
                if at + width > len {
                    continue;
                }
                let mut value = ascii.clone();
                value[at..at + width].copy_from_slice(bytes);
                assert_eq!(value_passes(&value), each(&value), "{len} {at} {bytes:?}");
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// The check of a view array passes exactly where each value's checks
    /// of UTF-8 and zero bytes pass, over values of bytes of every kind,
    /// short and long, and NULL slots.
    #[test]
    fn views_pass_where_each_value_passes(
        values in prop::collection::vec(
            prop::option::of(prop::collection::vec(prop_oneof![
                Just(0u8), Just(b'a'), Just(0x7f), Just(0x80), Just(0xa9), Just(0xbf),
                Just(0xc3), Just(0xe2), Just(0xf0), Just(0xff), any::<u8>()
            ], 0..20)),
            0..8),
        utf8 in prop::collection::vec(prop::option::of(".{0,8}"), 0..8),
        mixed in any::<bool>(),
    ) {
        let values: Vec<Option<Vec<u8>>> = if mixed {
            values
        } else {
            utf8.into_iter().map(|v| v.map(String::into_bytes)).collect()
        };
        prop_assert_eq!(views_pass(&views(&values)), each_passes(&values));
        for value in values.iter().flatten() {
            prop_assert_eq!(value_passes(value), each_passes(&[Some(value.clone())]));
        }
    }
}

/// The slots of a window: a row's bytes, by its view or by its key, none
/// past the limit of a value's length, which a smaller limit stands for
/// here so that no value of PostgreSQL's largest size is made, and none
/// where a row is past the window or a key or a slot's offsets do not
/// hold.
#[test]
fn slots_of_rows_and_the_limit_of_a_length() {
    let data = b"abcdef";
    let offsets = [0i64, 1, 3, 3, 6];
    let array = views(&[
        Some(b"a".to_vec()),
        Some(Vec::new()),
        None,
        Some(b"a value of thirteen".to_vec()),
    ]);
    let plain = Slots::Views(&array);
    assert_eq!(plain.value(0, 3), Some(&b"a"[..]));
    assert_eq!(plain.value(1, 3), Some(&b""[..]));
    assert_eq!(plain.value(3, 19), Some(&b"a value of thirteen"[..]));
    assert_eq!(plain.value(3, 18), None, "past the limit");
    assert_eq!(plain.value(4, 3), None, "past the window");
    let keys = [3i32, 0, -1, 9];
    let keyed = Slots::Dictionary {
        offsets: &offsets,
        data,
        keys: &keys,
    };
    assert_eq!(keyed.value(0, 3), Some(&b"def"[..]));
    assert_eq!(keyed.value(1, 3), Some(&b"a"[..]));
    assert_eq!(keyed.value(2, 3), None, "a key below 0");
    assert_eq!(keyed.value(3, 3), None, "a key past the offsets");
    assert_eq!(keyed.value(4, 3), None, "a row past the keys");
}

/// The `NULL` rows asked marked, past the mask's first word too: each set
/// bit stands for the row of its word and place, none past the window,
/// and a mask too short for a `NULL` row asked refused.
#[test]
fn nulls_of_rows_asked_marked() {
    let mut made = [0u64; 3];
    let null = |row| row == 1 || row == 67 || row == 129 || row == 131;
    assert_eq!(
        mark_nulls(
            130,
            &[0b11, 1 << 3 | 1 << 4, 1 << 1 | 1 << 3],
            null,
            &mut made
        ),
        Some(())
    );
    assert_eq!(made, [0b10, 1 << 3, 1 << 1]);
    let mut made = [0u64; 0];
    assert_eq!(mark_nulls(1, &[0b1], |_| true, &mut made), None);
    assert_eq!(mark_nulls(1, &[0b1], |_| false, &mut made), Some(()));
}
