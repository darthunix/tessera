//! The status of an entry point and the guard that produces it.

use std::ffi::c_char;
use std::mem::offset_of;
use std::panic::{self, AssertUnwindSafe};

use anyhow::Result;
use tessera_kernels::int32::ArithmeticError;

/// The outcome of an entry point, as `TessStatusCode`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    /// The call succeeded.
    Ok = 0,
    /// A dimension, pointer, mask or operation argument is invalid.
    InvalidArgument = 1,
    /// SQLSTATE 22003.
    IntegerOutOfRange = 2,
    /// SQLSTATE 22012.
    DivisionByZero = 3,
    /// A panic was caught; the library remains usable.
    Panic = 4,
}

/// Bytes of the message buffer, terminator included
/// (`TESS_STATUS_MESSAGE_SIZE`).
pub const MESSAGE_SIZE: usize = 120;

/// The append-only status structure, as `TessStatus`: the caller sets
/// `struct_size`, an entry point fills the rest.
#[repr(C)]
#[derive(Debug)]
pub struct Status {
    /// The size the caller allocated, at least [`Status::MIN_SIZE`].
    pub struct_size: usize,
    /// The outcome.
    pub code: Code,
    /// A five-character SQLSTATE with a terminator; empty on success.
    pub sqlstate: [c_char; 6],
    /// A NUL-terminated message; empty on success.
    pub message: [c_char; MESSAGE_SIZE],
}

impl Status {
    /// The size through `message` (`TESS_STATUS_MIN_SIZE`).
    pub const MIN_SIZE: usize = offset_of!(Status, message) + MESSAGE_SIZE;

    /// A status ready for an entry point.
    pub fn new() -> Self {
        Self {
            struct_size: size_of::<Self>(),
            code: Code::Ok,
            sqlstate: [0; 6],
            message: [0; MESSAGE_SIZE],
        }
    }

    /// The message as text, up to its terminator.
    pub fn message(&self) -> String {
        text(&self.message)
    }

    /// The SQLSTATE as text.
    pub fn sqlstate(&self) -> String {
        text(&self.sqlstate)
    }
}

impl Default for Status {
    fn default() -> Self {
        Self::new()
    }
}

fn text(chars: &[c_char]) -> String {
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Run the body of an entry point: an error it returns or a panic it
/// raises becomes the code and the status. The body writes its outputs
/// itself, at its end, so that nothing is written on failure.
///
/// The guard asserts unwind safety: everything the body touches is borrowed
/// from the caller and declared unspecified after a failure, and the guard
/// keeps no state of its own.
///
/// # Safety
///
/// `status` must be null or point to writable memory of at least the
/// `struct_size` it holds; nothing else is written through it.
pub(super) unsafe fn guard(status: *mut Status, body: impl FnOnce() -> Result<()>) -> Code {
    let outcome = panic::catch_unwind(AssertUnwindSafe(body));
    let (code, sqlstate, message) = match outcome {
        Ok(Ok(())) => (Code::Ok, "", String::new()),
        Ok(Err(error)) => match error.downcast_ref::<ArithmeticError>() {
            Some(arithmetic) => (
                match arithmetic {
                    ArithmeticError::IntegerOutOfRange => Code::IntegerOutOfRange,
                    ArithmeticError::DivisionByZero => Code::DivisionByZero,
                },
                arithmetic.sqlstate(),
                arithmetic.to_string(),
            ),
            None => (Code::InvalidArgument, "XX000", format!("{error:#}")),
        },
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic with a non-text payload".to_owned());
            // Dropping the payload here must not panic; text payloads do not.
            drop(payload);
            (Code::Panic, "XX000", message)
        }
    };
    // SAFETY: the caller guarantees `status` is null or writable for its
    // `struct_size`; an undersized status is left alone.
    unsafe {
        if !status.is_null() && (*status).struct_size >= Status::MIN_SIZE {
            (*status).code = code;
            fill(&mut (*status).sqlstate, sqlstate);
            fill(&mut (*status).message, &message);
        }
    }
    code
}

/// Copy `text` into `buffer` with a terminator, truncating on a character
/// boundary.
fn fill(buffer: &mut [c_char], text: &str) {
    let room = buffer.len() - 1;
    let mut end = text.len().min(room);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    for (slot, byte) in buffer.iter_mut().zip(text.as_bytes()[..end].iter()) {
        *slot = *byte as c_char;
    }
    buffer[end] = 0;
}

#[cfg(test)]
mod tests {
    use super::{Code, MESSAGE_SIZE, Status, guard};

    #[test]
    fn layout_matches_the_header() {
        // struct_size, code, sqlstate and message, padded to 8 bytes.
        assert_eq!(size_of::<Status>(), 144);
        assert_eq!(Status::MIN_SIZE, 8 + 4 + 6 + MESSAGE_SIZE);
        assert_eq!(size_of::<Code>(), 4);
    }

    #[test]
    fn long_messages_are_cut_on_a_character_boundary() {
        let mut status = Status::new();
        let text = "я".repeat(200);
        // SAFETY: a local status of full size.
        let code = unsafe { guard(&raw mut status, || Err(anyhow::anyhow!(text))) };
        assert_eq!(code, Code::InvalidArgument);
        let message = status.message();
        assert!(message.len() < MESSAGE_SIZE && message.chars().all(|c| c == 'я'));
        assert_eq!(status.sqlstate(), "XX000");
    }

    #[test]
    fn a_null_or_undersized_status_is_left_alone() {
        // SAFETY: a null status is permitted.
        assert_eq!(unsafe { guard(std::ptr::null_mut(), || Ok(())) }, Code::Ok);
        let mut status = Status::new();
        status.struct_size = 8;
        // SAFETY: a local status; its declared size is too small to write.
        let code = unsafe { guard(&raw mut status, || Err(anyhow::anyhow!("x"))) };
        assert_eq!(code, Code::InvalidArgument);
        assert_eq!(status.code, Code::Ok);
        assert_eq!(status.message(), "");
    }
}
