//! Linux backend: one group of four events of the calling thread, on the
//! core PMU, through `perf_event_open(2)`.
//!
//! The instruction counter leads the group, and one `read(2)` of it gives
//! all four counts, taken at one instant, with the times the group was
//! enabled and ran. The leader is pinned: a pinned group that cannot stay
//! on the PMU goes into an error state instead of being multiplexed, and a
//! read of it gives nothing. Every event leaves out the kernel and the
//! hypervisor. The choice of the PMU and of the events is
//! [`core_pmu`](crate::core_pmu)'s.

use crate::{
    Error, ErrorKind, Reading,
    core_pmu::{self, Arch, Event},
};
use perf_event_open_sys::{
    bindings::{
        PERF_FLAG_FD_CLOEXEC, PERF_FORMAT_GROUP, PERF_FORMAT_TOTAL_TIME_ENABLED,
        PERF_FORMAT_TOTAL_TIME_RUNNING, PERF_IOC_FLAG_GROUP, perf_event_attr,
    },
    ioctls, perf_event_open,
};
use std::{
    fs, io,
    mem::size_of,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::Path,
};

type Result<T> = std::result::Result<T, Error>;

const DEVICES: &str = "/sys/bus/event_source/devices";
const THREAD_STATUS: &str = "/proc/thread-self/status";
const ONLINE: &str = "/sys/devices/system/cpu/online";
const PARANOID: &str = "/proc/sys/kernel/perf_event_paranoid";

#[cfg(target_arch = "x86_64")]
const ARCH: Arch = Arch::X86_64;
#[cfg(target_arch = "aarch64")]
const ARCH: Arch = Arch::Aarch64;

/// A read of the group: its count of events, the times it was enabled and
/// ran, and the four counts, as `PERF_FORMAT_GROUP` with both times lays
/// them out.
type Buffer = [u64; 7];

pub struct Counters {
    leader: OwnedFd,
    // The other events of the group, open as long as the leader.
    _members: [OwnedFd; 3],
}

impl Counters {
    pub fn open() -> Result<Self> {
        let status = fs::read_to_string(THREAD_STATUS).map_err(|cause| {
            Error::with_source(
                ErrorKind::Other,
                format!("cannot read {THREAD_STATUS}"),
                cause,
            )
        })?;
        let allowed = core_pmu::allowed_cpus(&status).ok_or_else(|| {
            Error::new(
                ErrorKind::Other,
                format!("{THREAD_STATUS} has no list of the thread's CPUs"),
            )
        })?;
        let online = fs::read_to_string(ONLINE)
            .ok()
            .and_then(|list| core_pmu::parse_cpu_list(&list));
        let choice = core_pmu::choose(Path::new(DEVICES), &allowed, online.as_ref(), ARCH)?;
        Self::open_group(&choice.events, &choice.pmu)
    }

    /// Opens `events` as one group of the calling thread, then resets and
    /// enables it, and reads it once, so that a group that cannot stay on
    /// the PMU fails here rather than at the first measurement.
    pub(crate) fn open_group(events: &[Event; 4], pmu: &str) -> Result<Self> {
        let leader = open_event(&events[0], None, pmu)?;
        let members = [
            open_event(&events[1], Some(leader.as_raw_fd()), pmu)?,
            open_event(&events[2], Some(leader.as_raw_fd()), pmu)?,
            open_event(&events[3], Some(leader.as_raw_fd()), pmu)?,
        ];
        let fd = leader.as_raw_fd();
        // SAFETY: an ioctl of perf on a valid descriptor, with a flag.
        if unsafe { ioctls::RESET(fd, PERF_IOC_FLAG_GROUP) } < 0
            // SAFETY: as above.
            || unsafe { ioctls::ENABLE(fd, PERF_IOC_FLAG_GROUP) } < 0
        {
            return Err(Error::with_source(
                ErrorKind::Other,
                "the ioctl that resets and enables the group failed",
                io::Error::last_os_error(),
            ));
        }
        let counters = Self {
            leader,
            _members: members,
        };
        if let Err(error) = counters.read() {
            return Err(Error::new(
                ErrorKind::Busy,
                format!(
                    "the group of four counters does not stay on the PMU {pmu} ({error}): \
                     another program pins its own counters, or the NMI watchdog \
                     holds one; `sysctl kernel.nmi_watchdog=0` as root frees it"
                ),
            ));
        }
        Ok(counters)
    }

    pub fn read(&self) -> Result<Reading> {
        let mut buffer: Buffer = [0; 7];
        // SAFETY: the buffer is valid for writes of its whole size.
        let read = unsafe {
            libc::read(
                self.leader.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                size_of::<Buffer>(),
            )
        };
        if read < 0 {
            return Err(Error::with_source(
                ErrorKind::Other,
                "the read of the group failed",
                io::Error::last_os_error(),
            ));
        }
        let [count, enabled, running, values @ ..] = buffer;
        if read as usize != size_of::<Buffer>() || count != 4 {
            return Err(Error::new(
                ErrorKind::Lost,
                format!(
                    "the group lost the PMU: a read gave {read} bytes and {count} \
                     counts; another program may pin its own counters"
                ),
            ));
        }
        if enabled != running {
            return Err(Error::new(
                ErrorKind::Lost,
                format!(
                    "the group ran {running} ns of the {enabled} ns it was enabled: \
                     its counts are not whole"
                ),
            ));
        }
        Ok(Reading {
            instructions: values[0],
            cycles: values[1],
            branch_misses: values[2],
            branches: values[3],
        })
    }
}

/// Opens one event of the calling thread, the leader of a group when
/// `group` is `None`, user space only.
fn open_event(event: &Event, group: Option<RawFd>, pmu: &str) -> Result<OwnedFd> {
    let mut attr = perf_event_attr {
        type_: event.kind,
        size: size_of::<perf_event_attr>() as u32,
        config: event.config,
        ..perf_event_attr::default()
    };
    attr.set_exclude_kernel(1);
    attr.set_exclude_hv(1);
    if group.is_none() {
        attr.set_disabled(1);
        attr.set_pinned(1);
        attr.read_format = u64::from(
            PERF_FORMAT_GROUP | PERF_FORMAT_TOTAL_TIME_ENABLED | PERF_FORMAT_TOTAL_TIME_RUNNING,
        );
    }
    // SAFETY: `attr` is a valid structure whose size field is its own size;
    // pid 0 and cpu -1 name the calling thread on any CPU.
    let fd = unsafe {
        perf_event_open(
            &mut attr,
            0,
            -1,
            group.unwrap_or(-1),
            libc::c_ulong::from(PERF_FLAG_FD_CLOEXEC),
        )
    };
    if fd < 0 {
        return Err(open_error(io::Error::last_os_error(), event, pmu));
    }
    // SAFETY: a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The error of a `perf_event_open` that failed with `cause`.
fn open_error(cause: io::Error, event: &Event, pmu: &str) -> Error {
    let what = format!(
        "perf_event_open of the event {:#x} of type {} on the PMU {pmu} failed",
        event.config, event.kind
    );
    let (kind, fix) = match cause.raw_os_error() {
        Some(libc::EACCES | libc::EPERM) => {
            let paranoid = fs::read_to_string(PARANOID)
                .map(|value| value.trim().to_owned())
                .unwrap_or_else(|_| "unknown".to_owned());
            (
                ErrorKind::Permission,
                format!(
                    "kernel.perf_event_paranoid is {paranoid}, which refuses this user \
                     its own counters; as root, `sysctl kernel.perf_event_paranoid=2`, \
                     or give the program CAP_PERFMON where the kernel accepts it"
                ),
            )
        }
        Some(libc::ENOENT | libc::EOPNOTSUPP | libc::ENODEV | libc::ENOSYS) => (
            ErrorKind::NoCounters,
            "the kernel counts no such event here: the machine gives no hardware \
             counters, as a virtual machine without a virtual PMU"
                .to_owned(),
        ),
        Some(libc::EBUSY) => (
            ErrorKind::Busy,
            "another program holds the PMU for itself".to_owned(),
        ),
        _ => (ErrorKind::Other, "the kernel refused the event".to_owned()),
    };
    Error::with_source(kind, format!("{what}: {fix}"), cause)
}

pub fn cpu_number() -> Option<usize> {
    // SAFETY: a call without arguments.
    let cpu = unsafe { libc::sched_getcpu() };
    usize::try_from(cpu).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use perf_event_open_sys::bindings::{
        PERF_COUNT_HW_BRANCH_INSTRUCTIONS, PERF_COUNT_HW_BRANCH_MISSES, PERF_COUNT_HW_CPU_CYCLES,
        PERF_COUNT_HW_INSTRUCTIONS, PERF_COUNT_SW_CONTEXT_SWITCHES, PERF_COUNT_SW_CPU_MIGRATIONS,
        PERF_COUNT_SW_PAGE_FAULTS, PERF_COUNT_SW_TASK_CLOCK, PERF_TYPE_HARDWARE,
        PERF_TYPE_SOFTWARE,
    };
    use std::{env, hint::black_box};

    /// The group of software events opens, resets, counts and reads through
    /// the code of the hardware group: only the type and the id of each
    /// event differ. It needs no PMU, so it checks the plumbing on any
    /// Linux machine; `PMU_REQUIRE` makes it fail instead of skip.
    #[test]
    fn a_group_of_software_events_counts_through_the_same_code() {
        let event = |config| Event {
            kind: PERF_TYPE_SOFTWARE,
            config: u64::from(config),
        };
        let events = [
            event(PERF_COUNT_SW_TASK_CLOCK),
            event(PERF_COUNT_SW_PAGE_FAULTS),
            event(PERF_COUNT_SW_CONTEXT_SWITCHES),
            event(PERF_COUNT_SW_CPU_MIGRATIONS),
        ];
        let counters = match Counters::open_group(&events, "software") {
            Ok(counters) => counters,
            Err(error) if env::var_os("PMU_REQUIRE").is_none() => {
                eprintln!(
                    "software events unavailable, test skipped ({:?}): {error}",
                    error.kind()
                );
                return;
            }
            Err(error) => panic!("PMU_REQUIRE is set, but software events do not open: {error}"),
        };
        let start = counters.read().unwrap();
        let mut sum = 0_u64;
        for i in 0..1_000_000_u64 {
            sum = black_box(sum.wrapping_add(i));
        }
        let pages = vec![1_u8; 1 << 22];
        black_box(&pages);
        let reading = counters.read().unwrap().since(start);
        // The task clock counts nanoseconds of the thread, and the pages
        // just touched fault.
        assert!(reading.instructions > 0, "{reading:?}");
        assert!(reading.cycles > 0, "{reading:?}");
    }

    /// The choice of the core PMU writes the kernel's generic ids itself, so
    /// that its tests run on every host; here they meet the kernel's headers.
    #[test]
    fn the_generic_ids_of_the_choice_are_the_kernel_s() {
        assert_eq!(core_pmu::PERF_TYPE_HARDWARE, PERF_TYPE_HARDWARE);
        assert_eq!(
            core_pmu::PERF_COUNT_HW_CPU_CYCLES,
            u64::from(PERF_COUNT_HW_CPU_CYCLES)
        );
        assert_eq!(
            core_pmu::PERF_COUNT_HW_INSTRUCTIONS,
            u64::from(PERF_COUNT_HW_INSTRUCTIONS)
        );
        assert_eq!(
            core_pmu::PERF_COUNT_HW_BRANCH_INSTRUCTIONS,
            u64::from(PERF_COUNT_HW_BRANCH_INSTRUCTIONS)
        );
        assert_eq!(
            core_pmu::PERF_COUNT_HW_BRANCH_MISSES,
            u64::from(PERF_COUNT_HW_BRANCH_MISSES)
        );
    }

    #[test]
    fn the_cpu_of_the_thread_is_a_cpu_of_the_machine() {
        let cpus = std::thread::available_parallelism().map_or(1, usize::from);
        let cpu = cpu_number().unwrap();
        // The thread may be bound to fewer CPUs than the machine has, but
        // its number is below the count of the machine's possible CPUs.
        let possible = fs::read_to_string("/sys/devices/system/cpu/possible")
            .ok()
            .and_then(|list| core_pmu::parse_cpu_list(&list))
            .and_then(|cpus| cpus.last().copied())
            .map_or(cpus, |last| last + 1);
        assert!(cpu < possible, "cpu {cpu} of {possible}");
    }
}
