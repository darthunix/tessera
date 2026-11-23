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

fn reader_of(
    file: *const FileHandle,
    col: *const ColumnHandle,
    rows: usize,
    batch: usize,
) -> *mut ReaderHandle {
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
            batch,
            &mut out,
            &mut st,
        )
    });
    out
}

/// Reads the window of `len` rows from `start`, every row asked.
fn read_at(r: *mut ReaderHandle, start: usize, len: usize) -> usize {
    let asked: Vec<u64> = (0..mask_words(len))
        .map(|w| match (len - w * 64).min(64) {
            64 => u64::MAX,
            rows => (1u64 << rows) - 1,
        })
        .collect();
    let mut held = vec![0u64; mask_words(len)];
    let mut count = 0usize;
    let mut st = status();
    assert!(unsafe {
        tess_parquet_read(
            r,
            start,
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

#[test]
fn asks_that_do_not_nest_keep_the_null_flags_of_each() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    testing::write(
        &path,
        &[
            Column::new("a", Values::Int64(vec![None, Some(7), Some(8)])),
            Column::new(
                "s",
                Values::Bytes(vec![None, Some(b"x".to_vec()), Some(b"yy".to_vec())]),
            ),
        ],
        3,
        false,
    )
    .unwrap();
    let file = open(&path);
    let mut st = status();
    let a = column(file, "a", oid::INT8, -1).unwrap();
    let r = reader(file, a, 3);
    assert_eq!(read(r, 3, &[0b111]), 3);
    let mut values = [0u64; 3];
    let mut nulls = [0u64; 1];
    for asked in [0b001u64, 0b010, 0b100] {
        assert!(unsafe {
            tess_parquet_values(
                r,
                [asked].as_ptr(),
                1,
                values.as_mut_ptr(),
                3,
                nulls.as_mut_ptr(),
                1,
                &mut st,
            )
        });
    }
    assert_eq!((nulls[0], values[1], values[2]), (0b001, 7, 8));
    let s = column(file, "s", oid::TEXT, -1).unwrap();
    let rs = reader(file, s, 3);
    assert_eq!(read(rs, 3, &[0b111]), 3);
    let mut nulls = [0u64; 1];
    let mut bytes = 0usize;
    for asked in [0b001u64, 0b110] {
        assert!(unsafe {
            tess_parquet_text_size(
                rs,
                [asked].as_ptr(),
                1,
                nulls.as_mut_ptr(),
                1,
                &mut bytes,
                &mut st,
            )
        });
    }
    assert_eq!((nulls[0], bytes), (0b001, 2 + 3));
    // A read drops the text sized for the window before it.
    assert_eq!(read(rs, 3, &[0b111]), 3);
    let mut arena = [0u32; 4];
    let mut out = [0u64; 3];
    assert!(!unsafe {
        tess_parquet_text(
            rs,
            arena.as_mut_ptr().cast(),
            16,
            out.as_mut_ptr(),
            3,
            &mut st,
        )
    });
    assert_eq!(text(&st.sqlstate), "XX000");
    // So does a check that fails.
    let bad = column_of_bad_text(dir.path());
    let rb = reader(bad.0, bad.1, 2);
    assert_eq!(read(rb, 2, &[0b11]), 2);
    assert!(unsafe {
        tess_parquet_text_size(
            rb,
            [0b01u64].as_ptr(),
            1,
            nulls.as_mut_ptr(),
            1,
            &mut bytes,
            &mut st,
        )
    });
    assert!(!unsafe {
        tess_parquet_text_size(
            rb,
            [0b11u64].as_ptr(),
            1,
            nulls.as_mut_ptr(),
            1,
            &mut bytes,
            &mut st,
        )
    });
    assert!(!unsafe {
        tess_parquet_text(
            rb,
            arena.as_mut_ptr().cast(),
            16,
            out.as_mut_ptr(),
            3,
            &mut st,
        )
    });
    unsafe {
        tess_parquet_reader_free(r);
        tess_parquet_reader_free(rs);
        tess_parquet_reader_free(rb);
        tess_parquet_column_free(a);
        tess_parquet_column_free(s);
        tess_parquet_column_free(bad.1);
        tess_parquet_file_free(bad.0);
        tess_parquet_file_free(file);
    }
}

/// A file of a text column whose second row is not UTF-8, and its column.
fn column_of_bad_text(dir: &std::path::Path) -> (*mut FileHandle, *mut ColumnHandle) {
    let path = dir.join("bad.parquet");
    testing::write(&path, &[Column::bytes("s", &[b"ok", b"\xff"])], 2, false).unwrap();
    let file = open(&path);
    (file, column(file, "s", oid::TEXT, -1).unwrap())
}

#[test]
fn the_info_of_a_column_and_a_message_cut_at_a_character() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    testing::write(
        &path,
        &[
            Column::new("a", Values::Int32(vec![Some(1), None])),
            Column::bytes("s", &[b"x", b"y"]),
        ],
        2,
        false,
    )
    .unwrap();
    let file = open(&path);
    let mut st = status();
    let mut info = ColumnInfo::default();
    let a = column(file, "a", oid::INT4, -1).unwrap();
    assert!(unsafe { tess_parquet_column_info(a, &mut info, &mut st) });
    assert!(info.bytes > 0);
    assert_eq!(
        info,
        ColumnInfo {
            gives_datums: true,
            nullable: true,
            bytes: info.bytes
        }
    );
    let s = column(file, "s", oid::TEXT, -1).unwrap();
    assert!(unsafe { tess_parquet_column_info(s, &mut info, &mut st) });
    assert!(info.bytes > 0);
    assert_eq!(
        info,
        ColumnInfo {
            gives_datums: false,
            nullable: false,
            bytes: info.bytes
        }
    );
    assert!(!unsafe { tess_parquet_column_info(s, ptr::null_mut(), &mut st) });
    // A name of two-byte characters past the message's size.
    let name = "я".repeat(200);
    let mut out = ptr::null_mut();
    assert!(!unsafe {
        tess_parquet_column(
            file,
            name.as_ptr().cast(),
            name.len(),
            oid::INT4,
            -1,
            -1,
            -1,
            true,
            &mut out,
            &mut st,
        )
    });
    let bytes: Vec<u8> = st
        .message
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    assert!(String::from_utf8(bytes.clone()).is_ok());
    assert!(bytes.len() >= MESSAGE_SIZE - 2 && bytes.len() < MESSAGE_SIZE);
    unsafe {
        tess_parquet_column_free(a);
        tess_parquet_column_free(s);
        tess_parquet_file_free(file);
    }
}

#[test]
fn outputs_the_caller_has_not_initialized() {
    use std::mem::MaybeUninit;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    testing::write(&path, &[Column::int32("a", &[1, 2])], 2, false).unwrap();
    let bytes = path.as_os_str().as_encoded_bytes();
    let mut st = MaybeUninit::<Status>::uninit();
    let mut out = MaybeUninit::<*mut FileHandle>::uninit();
    assert!(unsafe {
        tess_parquet_open(
            bytes.as_ptr().cast(),
            bytes.len(),
            ptr::null(),
            out.as_mut_ptr(),
            st.as_mut_ptr(),
        )
    });
    let file = unsafe { out.assume_init() };
    let mut identity = MaybeUninit::<CIdentity>::uninit();
    let mut groups = MaybeUninit::<usize>::uninit();
    let mut rows = MaybeUninit::<u64>::uninit();
    assert!(unsafe {
        tess_parquet_file_info(
            file,
            identity.as_mut_ptr(),
            groups.as_mut_ptr(),
            rows.as_mut_ptr(),
            st.as_mut_ptr(),
        )
    });
    assert_eq!(
        unsafe { (groups.assume_init(), rows.assume_init()) },
        (1, 2)
    );
    // A failure fills a status the caller left uninitialized.
    assert!(!unsafe { tess_parquet_check(ptr::null(), st.as_mut_ptr()) });
    let st = unsafe { st.assume_init() };
    assert_eq!(text(&st.sqlstate), "XX000");
    unsafe { tess_parquet_file_free(file) };
}

#[test]
fn a_value_out_of_range_leaves_the_mask_of_nulls() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    testing::write(
        &path,
        &[Column::int64("t", &[946_684_800, i64::MAX])],
        2,
        false,
    )
    .unwrap();
    let file = open(&path);
    let mut col = ptr::null_mut();
    let mut st = status();
    assert!(unsafe {
        tess_parquet_column(
            file,
            "t".as_ptr().cast(),
            1,
            oid::TIMESTAMP,
            -1,
            1,
            -1,
            true,
            &mut col,
            &mut st,
        )
    });
    let r = reader(file, col, 2);
    assert_eq!(read(r, 2, &[0b11]), 2);
    let mut values = [7u64; 2];
    let mut nulls = [0b10u64; 1];
    assert!(!unsafe {
        tess_parquet_values(
            r,
            [0b11u64].as_ptr(),
            1,
            values.as_mut_ptr(),
            2,
            nulls.as_mut_ptr(),
            1,
            &mut st,
        )
    });
    // The failure at its row; the mask as it was, the values of no meaning.
    assert_eq!((text(&st.sqlstate).as_str(), st.row), ("22008", 1));
    assert_eq!(nulls[0], 0b10);
    unsafe {
        tess_parquet_reader_free(r);
        tess_parquet_column_free(col);
        tess_parquet_file_free(file);
    }
}

/// A data page the decoders cannot read: a status, not a crash. The
/// writer keeps the file's time by a call Miri does not know.
#[test]
#[cfg_attr(miri, ignore)]
fn bytes_the_decoders_cannot_read_give_a_status() {
    let dir = tempfile::tempdir().unwrap();
    let mut st = status();
    let path = dir.path().join("bad.parquet");
    testing::write(
        &path,
        &[Column::int32("a", &(0..50).collect::<Vec<_>>())],
        50,
        false,
    )
    .unwrap();
    testing::head_data_page_rle_dictionary(&path, 0, 0).unwrap();
    let file = open(&path);
    let col = column(file, "a", oid::INT4, -1).unwrap();
    let r = reader(file, col, 50);
    let mut held = [0u64; 1];
    let mut count = 0usize;
    let asked = [u64::MAX >> 14];
    assert!(!unsafe {
        tess_parquet_read(
            r,
            0,
            50,
            asked.as_ptr(),
            1,
            held.as_mut_ptr(),
            1,
            &mut count,
            &mut st,
        )
    });
    assert_eq!(text(&st.sqlstate), "XX001");
    unsafe {
        tess_parquet_reader_free(r);
        tess_parquet_column_free(col);
        tess_parquet_file_free(file);
    }
}

/// A group ruled out through the C entry, by the statistics and by the
/// dictionary; broken calls refused.
#[test]
fn a_group_ruled_out_through_the_c_entry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    // Two groups: 1 and 3, then 5 and 6.
    testing::write(
        &path,
        &[Column::int32("a", &[1, 3, 1, 3, 5, 6, 5, 6])],
        4,
        true,
    )
    .unwrap();
    let file = open(&path);
    let col = column(file, "a", oid::INT4, -1).unwrap();
    let mut test = ptr::null_mut();
    let mut st = status();
    // a = 2: between the first group's least and greatest, in no row.
    assert!(unsafe {
        tess_parquet_test(
            col,
            0,
            [2i64].as_ptr(),
            [false].as_ptr(),
            1,
            &mut test,
            &mut st,
        )
    });
    let ruled_out = |group: usize, dictionary: bool| {
        let mut out = false;
        let mut st = status();
        assert!(unsafe {
            tess_parquet_group_ruled_out(file, col, group, test, dictionary, &mut out, &mut st)
        });
        out
    };
    assert!(!ruled_out(0, false));
    assert!(ruled_out(0, true));
    assert!(ruled_out(1, false));
    let mut out = false;
    let mut st = status();
    assert!(!unsafe {
        tess_parquet_group_ruled_out(file, col, 0, ptr::null(), false, &mut out, &mut st)
    });
    assert_eq!(text(&st.sqlstate), "XX000");
    assert!(!unsafe {
        tess_parquet_group_ruled_out(file, col, 0, test, false, ptr::null_mut(), &mut st)
    });
    unsafe {
        tess_parquet_test_free(test);
        tess_parquet_column_free(col);
        tess_parquet_file_free(file);
    }
}

/// The kinds of comparison as the header numbers them.
#[test]
fn comparisons_by_the_header_s_kinds() {
    let kind = |strategy, negator, commuted| {
        tess_parquet_comparison(oid::INT4, oid::INT8, strategy, negator, commuted)
    };
    // TESS_PARQUET_TEST_EQ, _NE, _LT, _LE, _GT and _GE.
    assert_eq!(kind(3, 0, false), 0);
    assert_eq!(kind(0, 3, true), 1);
    assert_eq!(kind(1, 0, false), 2);
    assert_eq!(kind(2, 0, false), 3);
    assert_eq!(kind(5, 0, false), 4);
    assert_eq!(kind(4, 0, false), 5);
    assert_eq!(kind(1, 0, true), 4);
    assert_eq!(kind(0, 1, false), -1);
    assert_eq!(
        tess_parquet_comparison(oid::DATE, oid::TIMESTAMP, 3, 0, false),
        -1
    );
}

#[test]
fn a_clause_tested_on_the_window_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    testing::write(
        &path,
        &[Column::new(
            "a",
            Values::Int32(vec![Some(-1), None, Some(3), Some(4), Some(5)]),
        )],
        5,
        false,
    )
    .unwrap();
    let file = open(&path);
    let col = column(file, "a", oid::INT4, -1).unwrap();
    let r = reader(file, col, 5);
    assert_eq!(read(r, 5, &[0b11111]), 5);
    let test = |kind: i32, values: &[i64], nulls: &[bool]| {
        let mut out = ptr::null_mut();
        let mut st = status();
        let ok = unsafe {
            tess_parquet_test(
                col,
                kind,
                values.as_ptr(),
                nulls.as_ptr(),
                values.len(),
                &mut out,
                &mut st,
            )
        };
        if ok { Ok(out) } else { Err(text(&st.sqlstate)) }
    };
    let apply = |t: *mut TestHandle| {
        let mut passed = [u64::MAX; 2];
        let mut st = status();
        assert!(unsafe { tess_parquet_window_test(r, t, passed.as_mut_ptr(), 2, &mut st) });
        unsafe { tess_parquet_test_free(t) };
        passed
    };
    // a >= 3; a IN (4, NULL, -1); a IS NULL; a IS NOT NULL; a = NULL.
    assert_eq!(apply(test(5, &[3], &[false]).unwrap()), [0b11100, 0]);
    assert_eq!(
        apply(test(6, &[4, 0, -1], &[false, true, false]).unwrap()),
        [0b01001, 0]
    );
    assert_eq!(apply(test(7, &[], &[]).unwrap()), [0b00010, 0]);
    assert_eq!(apply(test(8, &[], &[]).unwrap()), [0b11101, 0]);
    assert_eq!(apply(test(0, &[3], &[true]).unwrap()), [0, 0]);
    // An unknown kind, a test of strings without bytes, a comparison of
    // no value, and null values.
    assert_eq!(test(13, &[], &[]).unwrap_err(), "XX000");
    assert_eq!(test(9, &[1], &[false]).unwrap_err(), "XX000");
    assert_eq!(test(0, &[], &[]).unwrap_err(), "XX000");
    let mut out = ptr::null_mut();
    let mut st = status();
    assert!(!unsafe { tess_parquet_test(col, 6, ptr::null(), ptr::null(), 2, &mut out, &mut st) });
    assert_eq!(text(&st.sqlstate), "XX000");
    // A test of a text column's values is refused; its NULL tests are not.
    let words = dir.path().join("s.parquet");
    testing::write(&words, &[Column::bytes("s", &[b"x"])], 1, false).unwrap();
    let text_file = open(&words);
    let s = column(text_file, "s", oid::TEXT, -1).unwrap();
    let mut st = status();
    assert!(!unsafe {
        tess_parquet_test(
            s,
            0,
            [1i64].as_ptr(),
            [false].as_ptr(),
            1,
            &mut out,
            &mut st,
        )
    });
    assert_eq!(text(&st.sqlstate), "XX000");
    assert!(unsafe { tess_parquet_test(s, 7, ptr::null(), ptr::null(), 0, &mut out, &mut st) });
    unsafe {
        tess_parquet_test_free(out);
        tess_parquet_test_free(ptr::null_mut());
        tess_parquet_reader_free(r);
        tess_parquet_column_free(col);
        tess_parquet_column_free(s);
        tess_parquet_file_free(file);
        tess_parquet_file_free(text_file);
    }
}

/// The tests of strings through the C entries, as the header numbers
/// their kinds: `=`, `<>`, `LIKE` and `NOT LIKE` on the window's bytes, a
/// clause with `NULL`, the patterns the reader takes, and broken calls
/// refused.
#[test]
fn tests_of_strings_through_the_c_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    let strings: Vec<Option<Vec<u8>>> = [
        Some(&b"aba"[..]),
        Some(b""),
        None,
        Some(b"abba"),
        Some(b"x"),
    ]
    .iter()
    .map(|v| v.map(<[u8]>::to_vec))
    .collect();
    testing::write(&path, &[Column::new("s", Values::Bytes(strings))], 5, true).unwrap();
    let file = open(&path);
    let col = column(file, "s", oid::TEXT, -1).unwrap();
    let r = reader(file, col, 5);
    assert_eq!(read(r, 5, &[0b11111]), 5);
    let test = |kind: i32, value: Option<&[u8]>| {
        let mut out = ptr::null_mut();
        let mut st = status();
        let ok = unsafe {
            tess_parquet_bytes_test(
                col,
                kind,
                value.map_or(ptr::null(), <[u8]>::as_ptr),
                value.map_or(0, <[u8]>::len),
                value.is_none(),
                &mut out,
                &mut st,
            )
        };
        if ok { Ok(out) } else { Err(text(&st.sqlstate)) }
    };
    let apply = |t: *mut TestHandle| {
        let mut passed = [u64::MAX; 2];
        let mut st = status();
        assert!(unsafe { tess_parquet_window_test(r, t, passed.as_mut_ptr(), 2, &mut st) });
        unsafe { tess_parquet_test_free(t) };
        passed
    };
    // s = 'x'; s <> ''; s LIKE 'ab%ba'; s NOT LIKE 'a%'; s = NULL.
    assert_eq!(apply(test(9, Some(b"x")).unwrap()), [0b10000, 0]);
    assert_eq!(apply(test(10, Some(b"")).unwrap()), [0b11001, 0]);
    assert_eq!(apply(test(11, Some(b"ab%ba")).unwrap()), [0b01000, 0]);
    assert_eq!(apply(test(12, Some(b"a%")).unwrap()), [0b10010, 0]);
    assert_eq!(apply(test(9, None).unwrap()), [0, 0]);
    // A pattern the reader does not take, a kind not of strings, and
    // null bytes that are not NULL.
    assert_eq!(test(11, Some(b"a_")).unwrap_err(), "XX000");
    assert_eq!(test(6, Some(b"x")).unwrap_err(), "XX000");
    let mut out = ptr::null_mut();
    let mut st = status();
    assert!(!unsafe { tess_parquet_bytes_test(col, 9, ptr::null(), 2, false, &mut out, &mut st) });
    assert!(unsafe { tess_parquet_bytes_test(col, 9, ptr::null(), 0, false, &mut out, &mut st) });
    unsafe { tess_parquet_test_free(out) };
    assert!(unsafe { tess_parquet_like_checked(b"%a%b%".as_ptr(), 5) });
    assert!(unsafe { tess_parquet_like_checked(ptr::null(), 0) });
    assert!(!unsafe { tess_parquet_like_checked(b"a_".as_ptr(), 2) });
    assert!(!unsafe { tess_parquet_like_checked(ptr::null(), 1) });
    // A test of strings on a column of integers.
    let ints = dir.path().join("i.parquet");
    testing::write(&ints, &[Column::int32("a", &[1])], 1, false).unwrap();
    let int_file = open(&ints);
    let int_col = column(int_file, "a", oid::INT4, -1).unwrap();
    let mut st = status();
    assert!(!unsafe {
        tess_parquet_bytes_test(int_col, 9, b"1".as_ptr(), 1, false, &mut out, &mut st)
    });
    assert_eq!(text(&st.sqlstate), "XX000");
    unsafe {
        tess_parquet_reader_free(r);
        tess_parquet_column_free(col);
        tess_parquet_column_free(int_col);
        tess_parquet_file_free(file);
        tess_parquet_file_free(int_file);
    }
}

/// The bytes form of one window of `asked` rows, into arrays of `len`
/// entries: the call's success, the outputs and the reference that holds
/// the window's array.
#[allow(clippy::type_complexity)]
fn bytes_of(
    r: *mut ReaderHandle,
    asked: &[u64],
    len: usize,
) -> (
    bool,
    Status,
    Vec<*const u8>,
    Vec<u32>,
    Vec<i32>,
    u64,
    u32,
    u64,
    *mut HoldHandle,
) {
    let mut data = vec![ptr::null::<u8>(); len];
    let mut lengths = vec![u32::MAX; len];
    let mut indexes = vec![-1i32; len];
    let mut nulls = [0u64; 1];
    let mut out = BytesOut {
        data: data.as_mut_ptr(),
        lengths: lengths.as_mut_ptr(),
        indexes: indexes.as_mut_ptr(),
        len,
        nvalues: u32::MAX,
        number: u64::MAX,
    };
    let mut hold = ptr::null_mut();
    let mut st = status();
    let ok = unsafe {
        tess_parquet_bytes(
            r,
            asked.as_ptr(),
            asked.len(),
            nulls.as_mut_ptr(),
            nulls.len(),
            &mut out,
            &mut hold,
            &mut st,
        )
    };
    (
        ok,
        st,
        data,
        lengths,
        indexes,
        nulls[0],
        out.nvalues,
        out.number,
        hold,
    )
}

/// The bytes form of a window, plain and through a dictionary: each row
/// asked its bytes in place, `NULL` rows marked, an empty value's pointer
/// not null, the dictionary's indexes and count; a row that is not UTF-8
/// fails only when it is asked; and every pointer still gives its bytes
/// after the window is replaced and the reader freed, while the reference
/// lives, as Miri checks.
#[test]
fn the_bytes_form_of_a_window_and_the_reference_that_holds_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.parquet");
    let long = vec![b'x'; 300];
    let strings: Vec<Option<Vec<u8>>> = vec![
        Some(b"short".to_vec()),
        None,
        Some(long.clone()),
        Some(Vec::new()),
        Some(b"short".to_vec()),
        Some(b"\xff".to_vec()),
        Some("\u{e9}t\u{e9}".as_bytes().to_vec()),
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
        // The row that is not UTF-8, asked, fails the call at its row.
        let (ok, st, ..) = bytes_of(r, &[0b111_1111], 7);
        assert!(!ok);
        assert_eq!((text(&st.sqlstate).as_str(), st.row), ("22021", 5));
        // Left out, it does not.
        let asked = [0b101_1111u64];
        let (ok, _, data, lengths, indexes, nulls, nvalues, number, hold) = bytes_of(r, &asked, 7);
        assert!(ok);
        assert_eq!(nulls, 0b10);
        assert!(
            data.iter()
                .enumerate()
                .all(|(row, p)| row == 1 || row == 5 || !p.is_null())
        );
        assert_eq!(lengths[3], 0);
        assert_eq!(lengths[5], u32::MAX, "a row not asked is left as it was");
        if dictionary {
            assert_eq!(number, 1);
            assert!(nvalues >= 5);
            assert_eq!(indexes[0], indexes[4], "one value, one index");
            assert!(
                indexes
                    .iter()
                    .enumerate()
                    .all(|(row, &i)| row == 1 || row == 5 || (i >= 0 && (i as u32) < nvalues))
            );
        } else {
            assert_eq!((nvalues, number), (0, 0));
            assert_eq!(indexes, vec![-1; 7], "no index for a plain window");
        }
        // The window replaced by a read behind the forward one, then the
        // reader freed: the reference alone holds the bytes.
        assert_eq!(read(r, 7, &[0b111_1111]), 7);
        unsafe { tess_parquet_reader_free(r) };
        for row in [0usize, 2, 3, 4, 6] {
            let got = unsafe { std::slice::from_raw_parts(data[row], lengths[row] as usize) };
            assert_eq!(Some(got.to_vec()), strings[row], "row {row}");
        }
        unsafe {
            tess_parquet_hold_free(hold);
            tess_parquet_column_free(col);
            tess_parquet_file_free(file);
        }
    }
}

/// A reader numbers the dictionary values arrays of its windows: one
/// array, one number, and a new number for the array a read behind the
/// forward one decodes anew.
#[test]
fn the_numbers_of_a_reader_s_dictionaries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("n.parquet");
    let strings: Vec<Option<Vec<u8>>> = (0..7).map(|i| Some(vec![b'a' + i % 3])).collect();
    testing::write(&path, &[Column::new("s", Values::Bytes(strings))], 7, true).unwrap();
    let file = open(&path);
    let col = column(file, "s", oid::TEXT, -1).unwrap();
    let r = reader(file, col, 7);
    let mut numbers = Vec::new();
    for _ in 0..3 {
        assert_eq!(read(r, 7, &[0b111_1111]), 7);
        let (ok, _, _, _, _, _, nvalues, number, hold) = bytes_of(r, &[0b111_1111], 7);
        assert!(ok);
        assert_eq!(nvalues, 3);
        numbers.push(number);
        unsafe { tess_parquet_hold_free(hold) };
    }
    assert_eq!(numbers[0], 1);
    assert!(numbers.windows(2).all(|w| w[1] > w[0]), "{numbers:?}");
    unsafe {
        tess_parquet_reader_free(r);
        tess_parquet_column_free(col);
        tess_parquet_file_free(file);
    }
}

/// `fill_rows` over 70 rows, past the first word of a mask: the rows
/// asked and not `NULL` get their places, counts and indexes, the others
/// are left; outputs of the window's length are taken and shorter ones,
/// keys shorter than the window or keys without indexes, are refused.
#[test]
fn rows_filled_over_two_words_and_short_outputs_refused() {
    let buffer: Vec<u8> = (0..140u8).collect();
    let values: Vec<&[u8]> = (0..70).map(|r| &buffer[r * 2..r * 2 + r % 3]).collect();
    let keys: Vec<i32> = (0..70i32).map(|r| r * 10).collect();
    // Rows 1, 64 and 69 asked; 64 NULL.
    let asked = [1u64 << 1, 1 | 1 << 5];
    let made = [0u64, 1];
    let mut data = vec![ptr::null::<u8>(); 70];
    let mut lengths = vec![u32::MAX; 70];
    let mut indexes = vec![-1i32; 70];
    fill_rows(
        &values,
        &made,
        &asked,
        Some(&keys),
        &mut data,
        &mut lengths,
        Some(&mut indexes),
    )
    .unwrap();
    for row in 0..70 {
        if row == 1 || row == 69 {
            assert_eq!(lengths[row] as usize, values[row].len(), "row {row}");
            assert_eq!(indexes[row], keys[row]);
            if values[row].is_empty() {
                assert!(ptr::eq(data[row], &EMPTY));
            } else {
                assert_eq!(data[row], values[row].as_ptr());
            }
        } else {
            assert!(data[row].is_null() && lengths[row] == u32::MAX && indexes[row] == -1);
        }
    }
    let mut short = vec![ptr::null::<u8>(); 69];
    let mut short_lengths = vec![0u32; 69];
    let mut short_indexes = vec![0i32; 69];
    assert_eq!(
        fill_rows(&values, &made, &asked, None, &mut short, &mut lengths, None),
        Err(FillError::Short)
    );
    assert_eq!(
        fill_rows(
            &values,
            &made,
            &asked,
            None,
            &mut data,
            &mut short_lengths,
            None
        ),
        Err(FillError::Short)
    );
    assert_eq!(
        fill_rows(
            &values,
            &made,
            &asked,
            Some(&keys[..69]),
            &mut data,
            &mut lengths,
            Some(&mut indexes)
        ),
        Err(FillError::Short)
    );
    assert_eq!(
        fill_rows(
            &values,
            &made,
            &asked,
            Some(&keys),
            &mut data,
            &mut lengths,
            Some(&mut short_indexes)
        ),
        Err(FillError::Short)
    );
    assert_eq!(
        fill_rows(
            &values,
            &made,
            &asked,
            Some(&keys),
            &mut data,
            &mut lengths,
            None
        ),
        Err(FillError::Short)
    );
    assert_eq!(
        fill_rows(&values, &made, &asked, None, &mut data, &mut lengths, None),
        Ok(())
    );
}

/// Holds of every kind of window keep their bytes: windows forward of 20
/// rows over arrays of 64, which the reader slices from its pending rows
/// and joins across two arrays, and a window read behind the forward one,
/// decoded anew; after every read and the reader freed, every place still
/// gives its bytes, as Miri checks, plain and through a dictionary.
#[test]
fn holds_of_sliced_joined_and_backward_windows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("h.parquet");
    let rows = 200;
    let strings: Vec<Option<Vec<u8>>> = (0..rows)
        .map(|r| Some(format!("value {} {}", r % 37, "y".repeat(r % 5)).into_bytes()))
        .collect();
    for dictionary in [false, true] {
        testing::write(
            &path,
            &[Column::new("s", Values::Bytes(strings.clone()))],
            rows,
            dictionary,
        )
        .unwrap();
        let file = open(&path);
        let col = column(file, "s", oid::TEXT, -1).unwrap();
        let r = reader_of(file, col, rows, 64);
        let mut kept = Vec::new();
        let windows: Vec<usize> = (0..rows).step_by(20).chain([0]).collect();
        for start in windows {
            assert_eq!(read_at(r, start, 20), 20);
            let asked = [(1u64 << 20) - 1];
            let (ok, _, data, lengths, _, _, _, _, hold) = bytes_of(r, &asked, 20);
            assert!(ok);
            kept.push((start, data, lengths, hold));
        }
        unsafe { tess_parquet_reader_free(r) };
        for (start, data, lengths, hold) in kept {
            for row in 0..20 {
                let got = unsafe { std::slice::from_raw_parts(data[row], lengths[row] as usize) };
                assert_eq!(
                    Some(got.to_vec()),
                    strings[start + row],
                    "row {}",
                    start + row
                );
            }
            unsafe { tess_parquet_hold_free(hold) };
        }
        unsafe {
            tess_parquet_column_free(col);
            tess_parquet_file_free(file);
        }
    }
}

/// One values array, one number: the forward windows of a chunk and the
/// asks of one window give the same number; a window read behind them,
/// decoded anew, another; the forward windows after it yet another, never
/// 0 and never one given before.
#[test]
fn a_number_kept_for_one_values_array_and_new_for_another() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("k.parquet");
    let rows = 200;
    let strings: Vec<Option<Vec<u8>>> = (0..rows)
        .map(|r| Some(vec![b'a' + (r % 7) as u8]))
        .collect();
    testing::write(
        &path,
        &[Column::new("s", Values::Bytes(strings))],
        rows,
        true,
    )
    .unwrap();
    let file = open(&path);
    let col = column(file, "s", oid::TEXT, -1).unwrap();
    let r = reader_of(file, col, rows, 64);
    let asked = [(1u64 << 20) - 1];
    let checked = || unsafe { (*r).values_checked };
    let number_of = |start: usize| {
        assert_eq!(read_at(r, start, 20), 20);
        let (ok, _, _, _, _, _, nvalues, number, hold) = bytes_of(r, &asked, 20);
        assert!(ok);
        assert_eq!(nvalues, 7);
        let (_, _, _, _, _, _, _, again, hold2) = bytes_of(r, &asked, 20);
        assert_eq!(again, number, "two asks of one window");
        unsafe {
            tess_parquet_hold_free(hold);
            tess_parquet_hold_free(hold2);
        }
        number
    };
    let forward = [number_of(0), number_of(20), number_of(40)];
    assert!(
        forward[0] != 0 && forward.iter().all(|&n| n == forward[0]),
        "{forward:?}"
    );
    // The values array of a number checked once, and each new one again.
    assert_eq!(checked(), 7, "one array, checked once");
    let behind = number_of(0);
    assert!(behind != 0 && behind != forward[0]);
    assert_eq!(checked(), 14, "a new number, checked again");
    let after = number_of(60);
    assert!(after != 0 && after != behind && after != forward[0]);
    assert_eq!(checked(), 21);
    unsafe {
        tess_parquet_reader_free(r);
        tess_parquet_column_free(col);
        tess_parquet_file_free(file);
    }
}

/// Two arrays over one buffer of bytes, at other offsets, are told apart;
/// an array is its own key.
#[test]
fn the_key_of_a_values_array_tells_slices_apart() {
    let values = arrow_array::LargeBinaryArray::from(vec![&b"a"[..], b"b", b"c"]);
    let first = values.slice(0, 2);
    let second = values.slice(1, 2);
    assert_eq!(values_key(&first).unwrap(), values_key(&first).unwrap());
    assert_ne!(values_key(&first).unwrap(), values_key(&second).unwrap());
    assert_ne!(values_key(&first).unwrap(), values_key(&values).unwrap());
    assert!(values_key(&arrow_array::Int32Array::from(vec![1])).is_err());
}

/// Past the last number of a reader the call fails, its outputs as they
/// were and no hold made.
#[test]
fn no_number_past_the_last() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("l.parquet");
    let strings: Vec<Option<Vec<u8>>> = (0..7).map(|r| Some(vec![b'a' + r % 3])).collect();
    testing::write(&path, &[Column::new("s", Values::Bytes(strings))], 7, true).unwrap();
    let file = open(&path);
    let col = column(file, "s", oid::TEXT, -1).unwrap();
    let r = reader(file, col, 7);
    unsafe { (*r).numbers = u64::MAX - 1 };
    assert_eq!(read(r, 7, &[0b111_1111]), 7);
    let (ok, _, _, _, _, _, _, number, hold) = bytes_of(r, &[0b111_1111], 7);
    assert!(ok);
    assert_eq!(number, u64::MAX);
    unsafe { tess_parquet_hold_free(hold) };
    assert_eq!(read(r, 7, &[0b111_1111]), 7);
    let (ok, _, data, lengths, _, nulls, nvalues, number, hold) = bytes_of(r, &[0b111_1111], 7);
    assert!(!ok);
    assert!(data.iter().all(|p| p.is_null()) && lengths.iter().all(|&l| l == u32::MAX));
    assert_eq!((nulls, nvalues, number), (0, u32::MAX, u64::MAX));
    assert!(hold.is_null());
    unsafe {
        tess_parquet_reader_free(r);
        tess_parquet_column_free(col);
        tess_parquet_file_free(file);
    }
}

/// A structure of the bytes form whose outputs the caller has not
/// initialized: the call reads its inputs and writes its outputs without
/// a reference to the whole, as Miri checks.
#[test]
fn a_bytes_structure_of_outputs_not_initialized() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("u.parquet");
    let strings: Vec<Option<Vec<u8>>> = (0..7).map(|r| Some(vec![b'a' + r % 3])).collect();
    testing::write(&path, &[Column::new("s", Values::Bytes(strings))], 7, true).unwrap();
    let file = open(&path);
    let col = column(file, "s", oid::TEXT, -1).unwrap();
    let r = reader(file, col, 7);
    assert_eq!(read(r, 7, &[0b111_1111]), 7);
    let mut data = vec![ptr::null::<u8>(); 7];
    let mut lengths = vec![0u32; 7];
    let mut indexes = vec![0i32; 7];
    let mut nulls = [0u64; 1];
    let mut out = std::mem::MaybeUninit::<BytesOut>::uninit();
    let place = out.as_mut_ptr();
    unsafe {
        std::ptr::addr_of_mut!((*place).data).write(data.as_mut_ptr());
        std::ptr::addr_of_mut!((*place).lengths).write(lengths.as_mut_ptr());
        std::ptr::addr_of_mut!((*place).indexes).write(indexes.as_mut_ptr());
        std::ptr::addr_of_mut!((*place).len).write(7);
    }
    let mut hold = ptr::null_mut();
    let mut st = status();
    assert!(unsafe {
        tess_parquet_bytes(
            r,
            [0b111_1111u64].as_ptr(),
            1,
            nulls.as_mut_ptr(),
            1,
            place,
            &mut hold,
            &mut st,
        )
    });
    let out = unsafe { out.assume_init() };
    assert_eq!((out.nvalues, out.number), (3, 1));
    assert_eq!(lengths, vec![1; 7]);
    unsafe {
        tess_parquet_hold_free(hold);
        tess_parquet_reader_free(r);
        tess_parquet_column_free(col);
        tess_parquet_file_free(file);
    }
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]

    /// `fill_rows` over windows of up to 130 rows, any masks and outputs,
    /// keys and indexes each given or not and of any length: it refuses
    /// exactly when an output or the keys are shorter than the window or
    /// keys and indexes are not given together, and then writes nothing;
    /// otherwise each row asked and not `NULL` gets its place inside the
    /// buffer, its count and its key, and every other row is left.
    #[test]
    fn rows_filled_for_any_shapes(
        lens in proptest::collection::vec(0usize..4, 0..130),
        asked in proptest::collection::vec(proptest::prelude::any::<u64>(), 3),
        made in proptest::collection::vec(proptest::prelude::any::<u64>(), 3),
        data_len in 0usize..131,
        lengths_len in 0usize..131,
        keys_len in proptest::option::of(0usize..131),
        indexes_len in proptest::option::of(0usize..131),
    ) {
        let buffer = vec![7u8; lens.iter().sum::<usize>() + 1];
        let mut at = 0;
        let values: Vec<&[u8]> = lens.iter().map(|&len| { let v = &buffer[at..at + len]; at += len; v }).collect();
        let rows = values.len();
        let keys: Option<Vec<i32>> = keys_len.map(|n| (0..n as i32).collect());
        let mut data = vec![ptr::null::<u8>(); data_len];
        let mut lengths = vec![u32::MAX; lengths_len];
        let mut indexes: Option<Vec<i32>> = indexes_len.map(|n| vec![-1; n]);
        let result = fill_rows(&values, &made, &asked, keys.as_deref(), &mut data, &mut lengths, indexes.as_deref_mut());
        let refused = data_len < rows || lengths_len < rows
            || keys_len.is_some_and(|n| n < rows) || indexes_len.is_some_and(|n| n < rows)
            || keys_len.is_some() != indexes_len.is_some();
        proptest::prop_assert_eq!(result.is_err(), refused);
        let bit = |mask: &[u64], row: usize| mask[row / 64] >> (row % 64) & 1 == 1;
        let base = buffer.as_ptr() as usize;
        for row in 0..rows.min(data_len).min(lengths_len) {
            let wanted = !refused && bit(&asked, row) && !bit(&made, row);
            if !wanted {
                proptest::prop_assert!(data[row].is_null() && lengths[row] == u32::MAX);
                continue;
            }
            proptest::prop_assert_eq!(lengths[row] as usize, values[row].len());
            if values[row].is_empty() {
                proptest::prop_assert!(ptr::eq(data[row], &EMPTY));
            } else {
                let place = data[row] as usize;
                proptest::prop_assert!(place >= base && place + values[row].len() <= base + buffer.len());
            }
            if let (Some(keys), Some(indexes)) = (&keys, &indexes) {
                proptest::prop_assert_eq!(indexes[row], keys[row]);
            }
        }
        if refused && let Some(indexes) = &indexes {
            proptest::prop_assert!(indexes.iter().all(|&i| i == -1));
        }
    }
}

/// A column of `type_oid` and `typmod`, its text checked, trusted or of
/// the default rule (`utf8` of the header's codes).
fn column_typed(
    file: *const FileHandle,
    type_oid: u32,
    typmod: i32,
    utf8: i32,
) -> Result<*mut ColumnHandle, String> {
    let mut out = ptr::null_mut();
    let mut st = status();
    let ok = unsafe {
        tess_parquet_column(
            file,
            "s".as_ptr().cast(),
            1,
            type_oid,
            typmod,
            -1,
            utf8,
            true,
            &mut out,
            &mut st,
        )
    };
    if ok { Ok(out) } else { Err(text(&st.sqlstate)) }
}

/// What a call of the bytes form gave, comparable: the error's code and
/// row, or each row's bytes or `NULL`, the mask of `NULL`s, the indexes of
/// the rows asked and the count of values.
#[allow(clippy::type_complexity)]
fn bytes_seen(
    r: *mut ReaderHandle,
    asked: &[u64],
    len: usize,
) -> Result<(Vec<Option<Vec<u8>>>, u64, Vec<i32>, u32), (String, i64)> {
    let (ok, st, data, lengths, indexes, nulls, nvalues, _, hold) = bytes_of(r, asked, len);
    if !ok {
        return Err((text(&st.sqlstate), st.row));
    }
    let bit = |row: usize| asked[0] >> row & 1 == 1;
    let rows = (0..len)
        .map(|row| {
            (bit(row) && nulls >> row & 1 == 0).then(|| unsafe {
                std::slice::from_raw_parts(data[row], lengths[row] as usize).to_vec()
            })
        })
        .collect();
    let indexes = (0..len)
        .map(|row| {
            if bit(row) && nulls >> row & 1 == 0 {
                indexes[row]
            } else {
                -1
            }
        })
        .collect();
    unsafe { tess_parquet_hold_free(hold) };
    Ok((rows, nulls, indexes, nvalues))
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]

    /// The cheaper check gives what the plan's check row by row gives, for
    /// strings of bytes of every kind (NULL rows, empty strings, bytes that
    /// are not UTF-8, zero bytes, characters of several bytes), plain and
    /// through a dictionary, read as checked text, trusted text, varchar of
    /// a length and bytea, for any rows asked, over two windows of a chunk:
    /// the same error at the same row, or the same bytes, NULLs, indexes
    /// and count. Not under Miri, whose run would write the files for hours;
    /// the cases of fixed files above are its.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn the_cheaper_check_is_the_plan_s_row_by_row(
        strings in proptest::collection::vec(
            proptest::option::of(proptest::collection::vec(proptest::prop_oneof![
                proptest::strategy::Just(0u8), proptest::strategy::Just(b'a'),
                proptest::strategy::Just(0xc3), proptest::strategy::Just(0xa9),
                proptest::strategy::Just(0xe2), proptest::strategy::Just(0xff),
                b'b'..b'e'
            ], 0..5)),
            40),
        asked in proptest::collection::vec(proptest::prelude::any::<u64>(), 2),
        dictionary in proptest::prelude::any::<bool>(),
        kind in 0usize..4,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.parquet");
        testing::write(&path, &[Column::new("s", Values::Bytes(strings.clone()))], 40, dictionary).unwrap();
        let file = open(&path);
        let (type_oid, typmod, utf8) = match kind {
            0 => (oid::TEXT, -1, -1),
            1 => (oid::TEXT, -1, 1),
            2 => (oid::VARCHAR, 4 + 3, -1),
            _ => (oid::BYTEA, -1, -1),
        };
        let col = column_typed(file, type_oid, typmod, utf8).unwrap();
        let cheaper = reader_of(file, col, 40, 64);
        let rows = reader_of(file, col, 40, 64);
        unsafe { (*rows).rows_only = true };
        for (start, mask) in [(0usize, asked[0] & 0xf_ffff), (20, asked[1] & 0xf_ffff)] {
            assert_eq!(read_at(cheaper, start, 20), 20);
            assert_eq!(read_at(rows, start, 20), 20);
            proptest::prop_assert_eq!(bytes_seen(cheaper, &[mask], 20), bytes_seen(rows, &[mask], 20));
        }
        unsafe {
            tess_parquet_reader_free(cheaper);
            tess_parquet_reader_free(rows);
            tess_parquet_column_free(col);
            tess_parquet_file_free(file);
        }
    }
}

/// Through a dictionary, the values array is checked whole once for the
/// chunk, and the windows after take their rows' bytes without a check;
/// a dictionary with a value that is not UTF-8 sends its windows to the
/// read row by row, which fails a window only where a row asked names the
/// value, at the first such row. A plain window is checked in one pass
/// once, however many batches ask it.
#[test]
fn a_dictionary_checked_once_and_a_window_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.parquet");
    let all = [(1u64 << 20) - 1];
    for bad in [false, true] {
        // Rows of "a" and "b", then "c" from row 40, and at row 50 bytes
        // that are not UTF-8 where the dictionary has them.
        let strings: Vec<Option<Vec<u8>>> = (0..60)
            .map(|r| {
                Some(match r {
                    50 if bad => b"\xff".to_vec(),
                    r if r >= 40 => b"c".to_vec(),
                    r => vec![b'a' + (r % 2) as u8],
                })
            })
            .collect();
        testing::write(&path, &[Column::new("s", Values::Bytes(strings))], 60, true).unwrap();
        let file = open(&path);
        let col = column(file, "s", oid::TEXT, -1).unwrap();
        let r = reader_of(file, col, 60, 64);
        let nvalues = if bad { 4 } else { 3 };
        assert_eq!(read_at(r, 0, 20), 20);
        assert!(bytes_seen(r, &all, 20).is_ok());
        assert_eq!(
            unsafe { (*r).values_checked },
            nvalues,
            "the whole dictionary"
        );
        assert_eq!(unsafe { (*r).dictionary_passes }, Some(!bad));
        assert_eq!(read_at(r, 20, 20), 20);
        assert!(bytes_seen(r, &all, 20).is_ok());
        assert_eq!(read_at(r, 40, 20), 20);
        // Row 10 of this window, the group's row 50, left out: no error.
        let without = [all[0] & !(1 << 10)];
        assert!(bytes_seen(r, &without, 20).is_ok());
        assert_eq!(unsafe { (*r).values_checked }, nvalues, "checked once");
        if bad {
            assert_eq!(bytes_seen(r, &all, 20), Err(("22021".to_string(), 10)));
        } else {
            assert!(bytes_seen(r, &all, 20).is_ok());
        }
        unsafe {
            tess_parquet_reader_free(r);
            tess_parquet_column_free(col);
        }
        // Plain: each window passes once, however many asks.
        testing::write(
            &path,
            &[Column::new(
                "s",
                Values::Bytes((0..60).map(|r| Some(vec![b'a' + (r % 5) as u8])).collect()),
            )],
            60,
            false,
        )
        .unwrap();
        unsafe { tess_parquet_file_free(file) };
        let file = open(&path);
        let col = column(file, "s", oid::TEXT, -1).unwrap();
        let r = reader_of(file, col, 60, 64);
        for start in [0usize, 20] {
            assert_eq!(read_at(r, start, 20), 20);
            for _ in 0..3 {
                assert!(bytes_seen(r, &all, 20).is_ok());
            }
        }
        assert_eq!(unsafe { (*r).windows_checked }, 2, "one pass a window");
        unsafe {
            tess_parquet_reader_free(r);
            tess_parquet_column_free(col);
            tess_parquet_file_free(file);
        }
    }
}

/// A dictionary of more values than a word of a mask counts is read whole
/// and passes: the mask over its values asks each of them, and none past.
#[test]
fn a_dictionary_of_more_than_a_word_passes_whole() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.parquet");
    let rows = 100;
    let strings = (0..rows)
        .map(|r| Some(format!("v{r}").into_bytes()))
        .collect();
    testing::write(
        &path,
        &[Column::new("s", Values::Bytes(strings))],
        rows,
        true,
    )
    .unwrap();
    let file = open(&path);
    let col = column(file, "s", oid::TEXT, -1).unwrap();
    let r = reader_of(file, col, rows, 128);
    assert_eq!(read_at(r, 0, 64), 64);
    let (_, _, _, nvalues) = bytes_seen(r, &[u64::MAX], 64).unwrap();
    assert_eq!(nvalues, 100);
    assert_eq!(unsafe { (*r).dictionary_passes }, Some(true));
    unsafe {
        tess_parquet_reader_free(r);
        tess_parquet_column_free(col);
        tess_parquet_file_free(file);
    }
}

/// A dictionary whose values array has a NULL slot is not taken as
/// checked, though its bytes would pass the plan's read of the array,
/// which skips NULL slots: a row whose key names that slot would get its
/// bytes, `0xff` here, unchecked. The file's writer never makes one, so
/// the arrays are built in Arrow.
#[test]
fn a_null_value_of_a_dictionary_is_not_taken_as_checked() {
    use arrow_array::types::Int32Type;
    use arrow_array::{DictionaryArray, Int32Array, LargeBinaryArray};
    use arrow_buffer::{Buffer, NullBuffer, OffsetBuffer, ScalarBuffer};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("n.parquet");
    let strings = (0..2).map(|_| Some(b"a".to_vec())).collect();
    testing::write(&path, &[Column::new("s", Values::Bytes(strings))], 2, true).unwrap();
    let file = open(&path);
    let col = column(file, "s", oid::TEXT, -1).unwrap();
    let r = reader_of(file, col, 2, 64);
    let values: Arc<dyn Array> = Arc::new(LargeBinaryArray::new(
        OffsetBuffer::new(ScalarBuffer::from(vec![0i64, 1, 2])),
        Buffer::from(b"a\xff".to_vec()),
        Some(NullBuffer::from(vec![true, false])),
    ));
    let keys = Int32Array::from(vec![0, 1]);
    let array = DictionaryArray::<Int32Type>::try_new(keys, Arc::clone(&values)).unwrap();
    let (mut values_checked, mut windows_checked) = (0, 0);
    let (mut dictionary_passes, mut window_passes) = (None, None);
    let mut made = [0u64; 1];
    let fast = cheaper_check(
        unsafe { &(*r).reader },
        (&mut values_checked, &mut windows_checked),
        &array,
        Some(values.as_ref()),
        &[0b11],
        &mut dictionary_passes,
        &mut window_passes,
        &mut made,
    )
    .unwrap();
    assert!(fast.is_none(), "read row by row");
    assert_eq!(dictionary_passes, Some(false));
    unsafe {
        tess_parquet_reader_free(r);
        tess_parquet_column_free(col);
        tess_parquet_file_free(file);
    }
}

/// A mask the plan refuses is refused by the bytes entry too, whichever
/// check it takes: one that asks a row past the window, one shorter than
/// the window, and a mask of NULLs shorter than it, all `XX000`. The
/// release build's tests run this without the debug build's check again.
#[test]
fn masks_the_plan_refuses_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.parquet");
    for dictionary in [false, true] {
        let strings: Vec<Option<Vec<u8>>> =
            (0..70).map(|r| Some(vec![b'a' + (r % 3) as u8])).collect();
        testing::write(
            &path,
            &[Column::new("s", Values::Bytes(strings))],
            70,
            dictionary,
        )
        .unwrap();
        let file = open(&path);
        let col = column(file, "s", oid::TEXT, -1).unwrap();
        let r = reader_of(file, col, 70, 128);
        assert_eq!(read_at(r, 0, 1), 1);
        // The plan's refusal, not the debug build's error of a cheaper
        // check that passed too much, which is XX000 too.
        let refused = |ok: bool, st: &Status| {
            !ok && text(&st.sqlstate) == "XX000" && text(&st.message).contains("a mask shorter")
        };
        let (ok, st, ..) = bytes_of(r, &[0b10], 1);
        assert!(refused(ok, &st), "a row past the window");
        assert_eq!(read_at(r, 0, 70), 70);
        // Each mask short alone, the other of the window's two words.
        let ask = |asked: &[u64], nulls_words: usize| {
            let mut data = vec![ptr::null::<u8>(); 70];
            let mut lengths = vec![0u32; 70];
            let mut indexes = vec![0i32; 70];
            let mut nulls = vec![0u64; nulls_words];
            let mut out = BytesOut {
                data: data.as_mut_ptr(),
                lengths: lengths.as_mut_ptr(),
                indexes: indexes.as_mut_ptr(),
                len: 70,
                nvalues: 0,
                number: 0,
            };
            let mut hold = ptr::null_mut();
            let mut st = status();
            let ok = unsafe {
                tess_parquet_bytes(
                    r,
                    asked.as_ptr(),
                    asked.len(),
                    nulls.as_mut_ptr(),
                    nulls.len(),
                    &mut out,
                    &mut hold,
                    &mut st,
                )
            };
            assert!(hold.is_null());
            assert!(data.iter().all(|p| p.is_null()) && lengths.iter().all(|&l| l == 0));
            (ok, st)
        };
        let (ok, st) = ask(&[u64::MAX], 2);
        assert!(refused(ok, &st), "a mask shorter than the window");
        let (ok, st) = ask(&[u64::MAX, u64::MAX], 2);
        assert!(refused(ok, &st), "a second word past the window");
        let (ok, st) = ask(&[u64::MAX, (1u64 << 6) - 1], 1);
        assert!(
            !ok && text(&st.sqlstate) == "XX000" && text(&st.message).contains("an output shorter"),
            "a mask of NULLs shorter than the window"
        );
        unsafe {
            tess_parquet_reader_free(r);
            tess_parquet_column_free(col);
            tess_parquet_file_free(file);
        }
    }
}
