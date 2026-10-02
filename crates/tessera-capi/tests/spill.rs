//! The status of a spilled block read back through the C entry points:
//! damaged bytes report SQLSTATE XX001, a misuse of the call XX000.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use tessera_capi::c::{
    Code, SpillHeader, Status, tess_spill_header_read, tess_spill_header_size,
    tess_spill_header_write, tess_spill_unpack,
};

#[test]
fn damaged_blocks_report_data_corrupted() {
    let header = SpillHeader {
        kind: 1,
        number: 3,
        partition: 2,
        level: 0,
        fingerprint: 0x5eed,
        len: 4096,
        packed: 0,
    };
    let mut bytes = vec![0_u8; tess_spill_header_size()];
    let mut status = Status::new();
    let mut back = SpillHeader { len: 0, ..header };
    // SAFETY: local buffers of the declared sizes throughout this test.
    unsafe {
        let code = tess_spill_header_write(
            bytes.as_mut_ptr(),
            bytes.len(),
            &raw const header,
            1 << 20,
            &raw mut status,
        );
        assert_eq!(code, Code::Ok, "{}", status.message());
        let read = |bytes: &[u8], status: &mut Status, back: &mut SpillHeader| {
            tess_spill_header_read(bytes.as_ptr(), bytes.len(), 0x5eed, 1 << 20, back, status)
        };
        assert_eq!(read(&bytes, &mut status, &mut back), Code::Ok);
        assert_eq!(back.number, 3);
        // A damaged magic word: damaged data.
        let mut damaged = bytes.clone();
        damaged[0] ^= 1;
        assert_eq!(read(&damaged, &mut status, &mut back), Code::DataCorrupted);
        assert_eq!(status.sqlstate(), "XX001");
        // A buffer shorter than a header: the caller's misuse.
        assert_eq!(
            read(&bytes[..bytes.len() - 1], &mut status, &mut back),
            Code::InvalidArgument
        );
        assert_eq!(status.sqlstate(), "XX000");
        // A packed body too short for its counts: damaged data.
        let mut chunk = vec![0_u8; 64];
        let code = tess_spill_unpack(
            [0_u8; 4].as_ptr(),
            4,
            chunk.as_mut_ptr(),
            chunk.len(),
            &raw mut status,
        );
        assert_eq!(code, Code::DataCorrupted);
        assert_eq!(status.sqlstate(), "XX001");
    }
}
