use proptest::prelude::*;

use super::*;

/// The offsets of `values` laid one after another from `at` in a buffer.
fn laid(values: &[Vec<u8>], at: usize) -> (Vec<i64>, Vec<u8>) {
    let mut data = vec![b'p'; at];
    let mut offsets = vec![at as i64];
    for value in values {
        data.extend_from_slice(value);
        offsets.push(data.len() as i64);
    }
    data.extend_from_slice(b"\xff\x00 after");
    (offsets, data)
}

fn each_passes(values: &[Vec<u8>]) -> bool {
    values
        .iter()
        .all(|v| !v.contains(&0) && std::str::from_utf8(v).is_ok())
}

#[test]
fn ranges_of_slots_that_hold_and_do_not() {
    let offsets = [0i64, 2, 2, 5];
    assert_eq!(slot_range(&offsets, 5, 0), Some((0, 2)));
    assert_eq!(slot_range(&offsets, 5, 1), Some((2, 0)));
    assert_eq!(slot_range(&offsets, 5, 2), Some((2, 3)));
    assert_eq!(slot_range(&offsets, 5, 3), None, "past the offsets");
    assert_eq!(slot_range(&offsets, 4, 2), None, "past the buffer");
    assert_eq!(slot_range(&[3, 1], 5, 0), None, "going back");
    assert_eq!(slot_range(&[-1, 1], 5, 0), None, "below 0");
    assert_eq!(slot_range(&[0, 1], 5, usize::MAX), None);
}

#[test]
fn a_window_whose_values_split_a_character() {
    // "é" is c3 a9: split between two values, each fails, the whole does not.
    let values = vec![b"a\xc3".to_vec(), b"\xa9b".to_vec()];
    let (offsets, data) = laid(&values, 0);
    assert!(!window_passes(&offsets, &data));
    // Across an empty value between them.
    let values = vec![b"a\xc3".to_vec(), Vec::new(), b"\xa9b".to_vec()];
    let (offsets, data) = laid(&values, 0);
    assert!(!window_passes(&offsets, &data));
    let values = vec!["é".as_bytes().to_vec(), Vec::new(), b"z".to_vec()];
    let (offsets, data) = laid(&values, 3);
    assert!(window_passes(&offsets, &data));
    assert!(window_passes(&[], &data), "no value");
    assert!(!window_passes(&[0, 9, 2], &data), "offsets that go back");
    assert!(
        !window_passes(&[4, 2], &data),
        "a last offset before the first"
    );
    assert!(
        !window_passes(&[0, 99], &data),
        "a last offset past the buffer"
    );
    // A window that ends where its buffer ends, as an Arrow array's does.
    let exact = "é z".as_bytes();
    assert!(window_passes(&[0, 2, 2, 4], exact));
    assert!(window_passes(&[4, 4], exact), "one empty value at the end");
    assert!(
        !window_passes(&[0, 1, 4], exact),
        "a character cut at the end"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// The one pass passes exactly where each value's checks of UTF-8 and
    /// zero bytes pass, over values of bytes of every kind, laid at any
    /// place of a buffer with bytes that fail before and after them.
    #[test]
    fn the_one_pass_is_each_value_s_check(
        values in prop::collection::vec(
            prop::collection::vec(prop_oneof![
                Just(0u8), Just(b'a'), Just(0x7f), Just(0x80), Just(0xa9), Just(0xbf),
                Just(0xc3), Just(0xe2), Just(0xf0), Just(0xff), any::<u8>()
            ], 0..6),
            0..6),
        utf8 in prop::collection::vec(".{0,3}", 0..6),
        at in 0usize..4,
        mixed in any::<bool>(),
    ) {
        let values: Vec<Vec<u8>> = if mixed { values } else { utf8.into_iter().map(String::into_bytes).collect() };
        let (offsets, data) = laid(&values, at);
        prop_assert_eq!(window_passes(&offsets, &data), each_passes(&values));
        // And over a buffer that ends where the last value does.
        let end = *offsets.last().unwrap() as usize;
        prop_assert_eq!(window_passes(&offsets, &data[..end]), each_passes(&values));
    }
}

/// The slices of a window: the rows asked, a `NULL` row marked, none past
/// the limit of a value's length, which a smaller limit stands for here
/// so that no value of PostgreSQL's largest size is made; and no slices
/// where a slot's offsets do not hold.
#[test]
fn slices_of_rows_asked_and_the_limit_of_a_length() {
    let data = b"abcdef";
    let offsets = [0i64, 1, 3, 3, 6];
    let mut made = [0u64; 1];
    let got = slices(
        4,
        &[0b1011],
        |row| row == 1,
        |row| row,
        (&offsets, data),
        3,
        &mut made,
    );
    assert_eq!(got, Some(vec![&b"a"[..], &[], &[], &b"def"[..]]));
    assert_eq!(made, [0b10]);
    let mut made = [0u64; 1];
    assert_eq!(
        slices(
            4,
            &[0b1000],
            |_| false,
            |row| row,
            (&offsets, data),
            2,
            &mut made
        ),
        None
    );
    let mut made = [0u64; 1];
    assert_eq!(
        slices(4, &[0b1], |_| false, |_| 9, (&offsets, data), 3, &mut made),
        None
    );
    let mut made = [0u64; 0];
    assert_eq!(
        slices(
            1,
            &[0b1],
            |_| true,
            |row| row,
            (&offsets, data),
            3,
            &mut made
        ),
        None,
        "a mask too short"
    );
}
