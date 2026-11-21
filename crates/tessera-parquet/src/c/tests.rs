use std::ffi::c_char;
use std::ptr;

use datum_arrow::oid;

use super::*;
use crate::testing::{self, Column, Values};

fn status() -> Status {
    Status {
        sqlstate: [0; 6],
        row: -1,
        message: [0; MESSAGE_SIZE],
    }
}

fn text(chars: &[c_char]) -> String {
    chars
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8 as char)
        .collect()
}

fn open(path: &std::path::Path) -> *mut FileHandle {
    let bytes = path.as_os_str().as_encoded_bytes();
    let mut out = ptr::null_mut();
    let mut st = status();
    assert!(unsafe {
        tess_parquet_open(
            bytes.as_ptr().cast(),
            bytes.len(),
            ptr::null(),
            &mut out,
            &mut st,
        )
    });
    out
}

fn column(
    file: *const FileHandle,
    name: &str,
    type_oid: u32,
    utf8: i32,
) -> Result<*mut ColumnHandle, String> {
    let mut out = ptr::null_mut();
    let mut st = status();
    let ok = unsafe {
        tess_parquet_column(
            file,
            name.as_ptr().cast(),
            name.len(),
            type_oid,
            -1,
            -1,
            utf8,
            true,
            &mut out,
            &mut st,
        )
    };
    if ok { Ok(out) } else { Err(text(&st.sqlstate)) }
}

fn reader(file: *const FileHandle, col: *const ColumnHandle, rows: usize) -> *mut ReaderHandle {
    let selection = vec![u64::MAX; mask_words(rows)];
    let mut out = ptr::null_mut();
    let mut st = status();
    assert!(unsafe {
        tess_parquet_reader(
            file,
            col,
            0,
            selection.as_ptr(),
            selection.len(),
            64,
            &mut out,
            &mut st,
        )
    });
    out
}

fn read(r: *mut ReaderHandle, len: usize, asked: &[u64]) -> usize {
    let mut held = vec![0u64; mask_words(len)];
    let mut count = 0usize;
    let mut st = status();
    assert!(unsafe {
        tess_parquet_read(
            r,
            0,
            len,
            asked.as_ptr(),
            asked.len(),
            held.as_mut_ptr(),
            held.len(),
            &mut count,
            &mut st,
        )
    });
    count
}

#[test]
fn a_file_its_groups_and_a_column_of_integers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    testing::write(
        &path,
        &[Column::new(
            "a",
            Values::Int32(vec![Some(-1), None, Some(3), Some(4), Some(5)]),
        )],
        3,
        false,
    )
    .unwrap();
    let file = open(&path);
    let mut identity = CIdentity::default();
    let (mut groups, mut rows) = (0usize, 0u64);
    let mut st = status();
    assert!(unsafe {
        tess_parquet_file_info(file, &mut identity, &mut groups, &mut rows, &mut st)
    });
    assert_eq!(
        (groups, rows, identity.size),
        (2, 5, std::fs::metadata(&path).unwrap().len())
    );
    let mut first = 0u64;
    assert!(unsafe { tess_parquet_group_rows(file, 0, &mut first, &mut st) });
    assert_eq!(first, 3);
    assert!(!unsafe { tess_parquet_group_rows(file, 2, &mut first, &mut st) });
    assert_eq!(text(&st.sqlstate), "XX000");
    assert!(unsafe { tess_parquet_check(file, &mut st) });

    let col = column(file, "a", oid::INT8, -1).unwrap();
    let r = reader(file, col, 3);
    assert_eq!(read(r, 3, &[0b111]), 3);
    let mut values = [7u64; 3];
    let mut nulls = [0u64; 1];
    assert!(unsafe {
        tess_parquet_values(
            r,
            [0b111u64].as_ptr(),
            1,
            values.as_mut_ptr(),
            3,
            nulls.as_mut_ptr(),
            1,
            &mut st,
        )
    });
    assert_eq!((values[0], values[2], nulls[0]), ((-1i64) as u64, 3, 0b010));
    unsafe {
        tess_parquet_reader_free(r);
        tess_parquet_column_free(col);
    }

    // Opened again under the identity found first, and under another.
    let bytes = path.as_os_str().as_encoded_bytes();
    let mut again = ptr::null_mut();
    assert!(unsafe {
        tess_parquet_open(
            bytes.as_ptr().cast(),
            bytes.len(),
            &identity,
            &mut again,
            &mut st,
        )
    });
    let other = CIdentity {
        ino: identity.ino + 1,
        ..identity
    };
    let mut third = ptr::null_mut();
    assert!(!unsafe {
        tess_parquet_open(
            bytes.as_ptr().cast(),
            bytes.len(),
            &other,
            &mut third,
            &mut st,
        )
    });
    assert_eq!(text(&st.sqlstate), "55000");
    assert!(third.is_null());
    unsafe {
        tess_parquet_file_free(again);
        tess_parquet_file_free(file);
        tess_parquet_file_free(ptr::null_mut());
    }
}

#[test]
fn errors_come_back_with_their_sqlstate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    testing::write(&path, &[Column::int32("a", &[1])], 1, false).unwrap();
    let mut st = status();
    let mut out = ptr::null_mut();
    let missing = dir.path().join("none.parquet");
    let bytes = missing.as_os_str().as_encoded_bytes();
    assert!(!unsafe {
        tess_parquet_open(
            bytes.as_ptr().cast(),
            bytes.len(),
            ptr::null(),
            &mut out,
            &mut st,
        )
    });
    assert_eq!(text(&st.sqlstate), "58P01");
    assert!(text(&st.message).contains("could not open"));
    let file = open(&path);
    assert_eq!(column(file, "b", oid::INT4, -1).unwrap_err(), "42703");
    // A message longer than the status holds, cut and terminated; no row.
    let long = "x".repeat(400);
    let mut col = ptr::null_mut();
    assert!(!unsafe {
        tess_parquet_column(
            file,
            long.as_ptr().cast(),
            long.len(),
            oid::INT4,
            -1,
            -1,
            -1,
            true,
            &mut col,
            &mut st,
        )
    });
    assert_eq!(text(&st.message).len(), MESSAGE_SIZE - 1);
    assert!(text(&st.message).starts_with("Parquet file has no column \"xxx"));
    assert_eq!(st.row, -1);
    assert_eq!(column(file, "a", oid::INT4, 0).unwrap_err(), "22023");
    assert_eq!(column(file, "a", oid::INT2, -1).unwrap_err(), "42804");
    let mut col = ptr::null_mut();
    assert!(!unsafe {
        tess_parquet_column(
            file,
            "a".as_ptr().cast(),
            1,
            oid::INT4,
            -1,
            9,
            -1,
            true,
            &mut col,
            &mut st,
        )
    });
    assert_eq!(text(&st.sqlstate), "XX000");
    // A null pointer with a length, and a null file.
    assert!(!unsafe { tess_parquet_open(ptr::null(), 3, ptr::null(), &mut out, &mut st) });
    assert_eq!(text(&st.sqlstate), "XX000");
    assert!(!unsafe { tess_parquet_check(ptr::null(), &mut st) });
    // A read of values before any window.
    let col = column(file, "a", oid::INT4, -1).unwrap();
    let r = reader(file, col, 1);
    let mut values = [0u64; 1];
    let mut nulls = [0u64; 1];
    assert!(!unsafe {
        tess_parquet_values(
            r,
            [1u64].as_ptr(),
            1,
            values.as_mut_ptr(),
            1,
            nulls.as_mut_ptr(),
            1,
            &mut st,
        )
    });
    assert_eq!(text(&st.sqlstate), "XX000");
    unsafe {
        tess_parquet_reader_free(r);
        tess_parquet_column_free(col);
        tess_parquet_file_free(file);
    }
}

#[test]
fn a_panic_is_caught_and_the_library_stays_usable() {
    let mut st = status();
    assert!(!unsafe { tess_parquet_test_panic(&mut st) });
    assert_eq!(text(&st.sqlstate), "XX000");
    assert!(text(&st.message).contains("a panic on purpose"));
    assert!(!unsafe { tess_parquet_test_panic(ptr::null_mut()) });
    // A panic while decoding a file is bytes the format cannot decode.
    let caught = unsafe {
        guard(&mut st, true, || {
            #[allow(clippy::panic)]
            {
                panic!("{}", String::from("inside the decoder"));
            }
        })
    };
    assert!(!caught);
    assert_eq!(text(&st.sqlstate), "XX001");
}

/// The text of a value the arena at `base` holds at `datum`: its header
/// read as PostgreSQL reads it, and its bytes. The address is taken from
/// the arena's own pointer, so that Miri follows it.
fn varlena(base: *const u8, datum: u64) -> Vec<u8> {
    let ptr = base.wrapping_add(datum as usize - base as usize);
    let first = unsafe { *ptr };
    if first & 1 == 1 {
        let total = usize::from(first >> 1);
        unsafe { std::slice::from_raw_parts(ptr.add(1), total - 1) }.to_vec()
    } else {
        assert_eq!(ptr as usize % 4, 0, "a long header unaligned");
        let mut word = [0u8; 4];
        unsafe { ptr::copy_nonoverlapping(ptr, word.as_mut_ptr(), 4) };
        let total = (u32::from_le_bytes(word) >> 2) as usize;
        unsafe { std::slice::from_raw_parts(ptr.add(4), total - 4) }.to_vec()
    }
}

#[test]
fn text_into_an_arena_with_short_and_long_headers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    let long = vec![b'x'; 300];
    let edge = vec![b'y'; 126];
    let past = vec![b'z'; 127];
    let strings: Vec<Option<Vec<u8>>> = vec![
        Some(b"short".to_vec()),
        None,
        Some(long.clone()),
        Some(Vec::new()),
        Some(edge.clone()),
        Some(past.clone()),
        Some(b"\xff".to_vec()),
    ];
    for dictionary in [false, true] {
        testing::write(
            &path,
            &[Column::new("s", Values::Bytes(strings.clone()))],
            7,
            dictionary,
        )
        .unwrap();
        let file = open(&path);
        let col = column(file, "s", oid::TEXT, -1).unwrap();
        let r = reader(file, col, 7);
        assert_eq!(read(r, 7, &[0b111_1111]), 7);
        let mut st = status();
        let mut nulls = [0u64; 1];
        let mut bytes = 0usize;
        // The row that is not UTF-8 fails the check, at its row.
        assert!(!unsafe {
            tess_parquet_text_size(
                r,
                [0b111_1111u64].as_ptr(),
                1,
                nulls.as_mut_ptr(),
                1,
                &mut bytes,
                &mut st,
            )
        });
        assert_eq!((text(&st.sqlstate).as_str(), st.row), ("22021", 6));
        let asked = [0b011_1111u64];
        assert!(unsafe {
            tess_parquet_text_size(
                r,
                asked.as_ptr(),
                1,
                nulls.as_mut_ptr(),
                1,
                &mut bytes,
                &mut st,
            )
        });
        assert_eq!(nulls[0] & 0b011_1111, 0b10);
        let mut arena = vec![0u32; bytes.div_ceil(4) + 1];
        let mut values = [9u64; 7];
        assert!(unsafe {
            tess_parquet_text(
                r,
                arena.as_mut_ptr().cast(),
                bytes,
                values.as_mut_ptr(),
                7,
                &mut st,
            )
        });
        let base = arena.as_ptr().cast::<u8>();
        assert_eq!(varlena(base, values[0]), b"short");
        assert_eq!(values[1], 9);
        assert_eq!(varlena(base, values[2]), long);
        assert_eq!(varlena(base, values[3]), b"");
        assert_eq!(varlena(base, values[4]), edge);
        assert_eq!(varlena(base, values[5]), past);
        assert_eq!(values[6], 9);
        // An arena shorter than its size, and one unaligned.
        assert!(!unsafe {
            tess_parquet_text(
                r,
                arena.as_mut_ptr().cast(),
                bytes - 1,
                values.as_mut_ptr(),
                7,
                &mut st,
            )
        });
        let unaligned = unsafe { arena.as_mut_ptr().cast::<u8>().add(1) };
        assert!(!unsafe {
            tess_parquet_text(r, unaligned, bytes, values.as_mut_ptr(), 7, &mut st)
        });
        unsafe {
            tess_parquet_reader_free(r);
            tess_parquet_column_free(col);
            tess_parquet_file_free(file);
        }
    }
}

#[test]
fn the_bytes_of_an_arena() {
    let short = |len| Some((ptr::null(), len));
    assert_eq!(arena_bytes(&[]), 0);
    assert_eq!(arena_bytes(&[short(0), None, short(126)]), 1 + 127);
    // A long value aligns its header: 1 + 3 of padding + 4 + 127.
    assert_eq!(arena_bytes(&[short(0), short(127)]), 1 + 3 + 4 + 127);
    assert_eq!(arena_bytes(&[short(127), short(1)]), 4 + 127 + 2);
}
