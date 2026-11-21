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
