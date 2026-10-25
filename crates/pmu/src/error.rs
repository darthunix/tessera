//! The error of [`Counters::open`](crate::Counters::open) and of a read:
//! a kind for code, a message for people.

use std::{fmt, io};

/// Why the counters cannot be opened, or a read failed.
///
/// [`Error::kind`] names the cause, for a caller that decides what to do.
/// The message, printed with `{}`, names the cause and the fix for a
/// person. Where a call of the system failed, its error is the
/// [`source`](std::error::Error::source).
///
/// An `Error` is `Send` and `Sync`, so it travels in `anyhow::Error` and
/// across threads.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    source: Option<io::Error>,
}

/// The cause of an [`Error`].
///
/// Every kind exists on every target, so a caller matches on them
/// without `cfg`. New kinds may come in a minor version, so a `match`
/// needs an arm for the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The crate cannot count on this system or processor: a system
    /// without a backend, a version of macOS without the frameworks the
    /// crate loads, or an x86-64 program under Rosetta, whose counts would
    /// be those of the translated code. Fix: none on this machine; under
    /// Rosetta, build for the native target.
    Unsupported,
    /// The machine gives no hardware counters: a virtual machine without
    /// a virtual PMU, or a kernel of macOS without kpc, as in a virtual
    /// Mac. Fix: run on hardware, or in a virtual machine that is given a
    /// PMU.
    NoCounters,
    /// The system refuses the process its counters. On macOS the process
    /// is not root: run `sudo -v`, then the program with `sudo -n`. On
    /// Linux the setting `kernel.perf_event_paranoid` refuses the user:
    /// root lowers it to 2, or gives the program `CAP_PERFMON` where the
    /// kernel accepts it.
    Permission,
    /// The system keeps the counters for another user. On Linux the group
    /// of four does not stay on the PMU: another program pins counters of
    /// its own, or the NMI watchdog holds one. On macOS kperf refuses a
    /// root process with `EACCES` or `EBUSY`; the kernel does not refuse a
    /// second process that counts, as [`Counters`](crate::Counters) says.
    /// Fix: let the other user finish; on Linux, root frees the counter of
    /// the NMI watchdog with `sysctl kernel.nmi_watchdog=0`.
    Busy,
    /// The processor lacks an event the crate counts, such as an Arm core
    /// without `BR_RETIRED`, or a chip whose kperf database names the
    /// event otherwise. The message names the event. Fix: none on this
    /// machine; the crate counts no other event in its place.
    MissingEvent,
    /// The thread may run on CPUs that the core PMU does not count on, as
    /// on a processor with two kinds of cores, each with its own PMU, and
    /// the system can bind it, as Linux can. The message names the CPUs of
    /// each PMU. Fix: start the program bound to the CPUs of one, such as
    /// `taskset -c <cpus> <program>`.
    MixedCores,
    /// The counters stopped counting since they were opened, so a reading
    /// would not be exact. Fix: open the counters again; on Linux, check
    /// that no other program pins its own counters.
    Lost,
    /// Any other failure of the system. The message and the source say
    /// which call failed and how.
    Other,
}

impl Error {
    /// An error of `kind` with its message for people.
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// An error of `kind` caused by a failed call of the system.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn with_source(
        kind: ErrorKind,
        message: impl Into<String>,
        source: io::Error,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(source),
        }
    }

    /// The cause of the error.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        if let Some(source) = &self.source {
            write!(f, ": {source}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_keeps_its_kind_message_and_source() {
        let error = Error::with_source(
            ErrorKind::Permission,
            "kpc_force_all_ctrs_set refused the process",
            io::Error::from_raw_os_error(1),
        );
        assert_eq!(error.kind(), ErrorKind::Permission);
        let text = error.to_string();
        assert!(text.starts_with("kpc_force_all_ctrs_set refused the process: "));
        assert!(std::error::Error::source(&error).is_some());
        let plain = Error::new(ErrorKind::Unsupported, "no backend");
        assert_eq!(plain.to_string(), "no backend");
        assert!(std::error::Error::source(&plain).is_none());
    }

    #[test]
    fn an_error_is_send_and_sync() {
        fn both<T: Send + Sync + 'static>() {}
        both::<Error>();
    }
}
