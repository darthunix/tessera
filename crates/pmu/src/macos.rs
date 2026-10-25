//! kperf backend: the private kperf and kperfdata frameworks, loaded with
//! dlopen so that nothing links against them at build time.
//!
//! The sequence follows Apple's own kpc/kpep usage: resolve the event
//! database for this CPU, build a counter configuration from event names,
//! reserve all counters (root), program and start them, and enable per-thread
//! accumulation. Reads return the calling thread's counters.
//!
//! The `kpc_*` calls return -1 when they fail and leave the cause in `errno`;
//! the `kpep_*` calls return codes of their own.

use crate::{Error, ErrorKind, Reading};
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    io,
    mem::size_of,
    ptr,
    sync::{Mutex, PoisonError},
};

type Result<T> = std::result::Result<T, Error>;

const RTLD_LAZY: c_int = 0x1;
const KPC_CLASS_CONFIGURABLE_MASK: u32 = 1 << 1;
const KPC_MAX_COUNTERS: usize = 32;
/// `kpep_config_add_event` flag: 0 counts all exception levels, 1 sets the
/// EL0-only bit in the counter's configuration word.
const USER_SPACE_ONLY: u32 = 1;

// The values of errno on macOS that tell the causes apart.
const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EACCES: i32 = 13;
const EBUSY: i32 = 16;
const ENOTSUP: i32 = 45;
const EOPNOTSUPP: i32 = 102;

/// Event names in the order of the fields of [`Reading`].
///
/// The configurable events are used instead of `FIXED_INSTRUCTIONS` and
/// `FIXED_CYCLES` on purpose: fixed counters have no configuration word and
/// always count every exception level, so timer interrupts and context
/// switches inside a block would add kernel instructions. Configurable
/// counters honour the user-space-only flag. `INST_ALL` is the fallback the
/// kpep database itself names for the fixed instruction counter. From the
/// M4 on the four are the Arm events INST_RETIRED, CPU_CYCLES,
/// BR_MIS_PRED_RETIRED and BR_RETIRED; on the M1 to the M3, Apple's own
/// events of the same meaning.
const EVENTS: [&CStr; 4] = [
    c"INST_ALL",
    c"CORE_ACTIVE_CYCLE",
    c"BRANCH_MISPRED_NONSPEC",
    c"INST_BRANCH",
];

const KPERF: &CStr = c"/System/Library/PrivateFrameworks/kperf.framework/kperf";
const KPERFDATA: &CStr = c"/System/Library/PrivateFrameworks/kperfdata.framework/kperfdata";

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
    fn pthread_cpu_number_np(cpu_number_out: *mut usize) -> c_int;
    fn sysctlbyname(
        name: *const c_char,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *mut c_void,
        newlen: usize,
    ) -> c_int;
}

type Opaque = *mut c_void;
type KpcForceAllCtrsSet = unsafe extern "C" fn(c_int) -> c_int;
type KpcSetClasses = unsafe extern "C" fn(u32) -> c_int;
type KpcSetConfig = unsafe extern "C" fn(u32, *const u64) -> c_int;
type KpcGetThreadCounters = unsafe extern "C" fn(u32, u32, *mut u64) -> c_int;
type KpepDbCreate = unsafe extern "C" fn(*const c_char, *mut Opaque) -> c_int;
type KpepDbEvent = unsafe extern "C" fn(Opaque, *const c_char, *mut Opaque) -> c_int;
type KpepFree = unsafe extern "C" fn(Opaque);
type KpepConfigCreate = unsafe extern "C" fn(Opaque, *mut Opaque) -> c_int;
type KpepConfigForceCounters = unsafe extern "C" fn(Opaque) -> c_int;
type KpepConfigAddEvent = unsafe extern "C" fn(Opaque, *mut Opaque, u32, *mut u32) -> c_int;
type KpepConfigKpcClasses = unsafe extern "C" fn(Opaque, *mut u32) -> c_int;
type KpepConfigKpcCount = unsafe extern "C" fn(Opaque, *mut usize) -> c_int;
type KpepConfigKpcMap = unsafe extern "C" fn(Opaque, *mut usize, usize) -> c_int;
type KpepConfigKpc = unsafe extern "C" fn(Opaque, *mut u64, usize) -> c_int;

fn last_dl_error() -> String {
    // SAFETY: dlerror returns null or a valid C string owned by the loader.
    let message = unsafe { dlerror() };
    if message.is_null() {
        "unknown dynamic loader error".to_owned()
    } else {
        // SAFETY: non-null result of dlerror is a NUL-terminated string.
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    }
}

/// Whether the integer sysctl `name` reads 1. A sysctl the system lacks
/// reads as 0.
fn sysctl_is_one(name: &CStr) -> bool {
    let mut value: c_int = 0;
    let mut size = size_of::<c_int>();
    // SAFETY: the name is NUL-terminated, and the output is an int of the
    // size passed.
    let code = unsafe {
        sysctlbyname(
            name.as_ptr(),
            (&raw mut value).cast(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    code == 0 && value == 1
}

/// Whether this process is an x86-64 program translated by Rosetta. The
/// sysctl is missing on Intel Macs.
fn under_rosetta() -> bool {
    sysctl_is_one(c"sysctl.proc_translated")
}

/// Whether macOS runs in a virtual machine, whose kernel has no kpc and
/// whose virtual processor no kperf database names.
fn in_virtual_machine() -> bool {
    sysctl_is_one(c"kern.hv_vmm_present")
}

struct Library(*mut c_void);

impl Library {
    fn open(path: &CStr) -> Result<Self> {
        // SAFETY: path is a valid NUL-terminated path; the handle stays open
        // for the process lifetime (never dlclose'd), so resolved symbols
        // remain valid.
        let handle = unsafe { dlopen(path.as_ptr(), RTLD_LAZY) };
        if handle.is_null() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!(
                    "cannot load {}: {}; the private frameworks of kperf may \
                     have changed in this version of macOS",
                    path.to_string_lossy(),
                    last_dl_error()
                ),
            ));
        }
        Ok(Self(handle))
    }

    /// Resolve `name` as a function pointer of type `F`.
    ///
    /// # Safety
    ///
    /// `F` must be an `unsafe extern "C" fn` type matching the symbol's real
    /// signature; the caller vouches for the ABI of the private framework.
    unsafe fn function<F: Copy>(&self, name: &CStr) -> Result<F> {
        const {
            assert!(size_of::<F>() == size_of::<*mut c_void>());
        }
        // SAFETY: valid handle and NUL-terminated symbol name.
        let symbol = unsafe { dlsym(self.0, name.as_ptr()) };
        if symbol.is_null() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!(
                    "missing symbol {}: {}; the private frameworks of kperf \
                     may have changed in this version of macOS",
                    name.to_string_lossy(),
                    last_dl_error()
                ),
            ));
        }
        // SAFETY: F is a function pointer type of pointer size (checked above)
        // and the symbol is a function of that signature per the caller.
        Ok(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&symbol) })
    }
}

/// A handle of kperfdata, freed when dropped.
struct Owned {
    handle: Opaque,
    free: KpepFree,
}

impl Owned {
    fn new(free: KpepFree) -> Self {
        Self {
            handle: ptr::null_mut(),
            free,
        }
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: the handle came from the kperfdata call that `free`
            // belongs to, and is freed once.
            unsafe { (self.free)(self.handle) };
        }
    }
}

/// The result of a `kpep_*` call, whose code is its own.
fn kpep(code: c_int, what: &str) -> Result<()> {
    if code == 0 {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::Other,
        format!("{what} failed with code {code}"),
    ))
}

/// The result of a `kpc_*` call, read right after it: -1 and its cause in
/// errno, or 0.
fn kpc(code: c_int, what: &str) -> Result<()> {
    if code == 0 {
        return Ok(());
    }
    let cause = io::Error::last_os_error();
    let (kind, fix) = match cause.raw_os_error() {
        Some(EPERM) => (
            ErrorKind::Permission,
            "the counters need root: run `sudo -v`, then the program with `sudo -n`",
        ),
        Some(EACCES | EBUSY) => (
            ErrorKind::Busy,
            "another task holds the counters, such as Instruments, xctrace or \
             another program that counts; let it finish, then open again",
        ),
        Some(ENOENT | ENOTSUP | EOPNOTSUPP) => (
            ErrorKind::NoCounters,
            "this kernel has no kpc, as in a virtual Mac; count on hardware",
        ),
        _ => (ErrorKind::Other, "kperf refused the call"),
    };
    Err(Error::with_source(
        kind,
        format!("{what} failed: {fix}"),
        cause,
    ))
}

/// What the setup of kperf leaves for the reads.
#[derive(Clone, Copy)]
struct Setup {
    get_thread_counters: KpcGetThreadCounters,
    map: [usize; EVENTS.len()],
}

/// The setup of this process, once it succeeded.
///
/// kperf programs the counters of the whole system. A second setup while
/// another thread counts would reprogram them under that thread's block,
/// so the first setup that succeeds serves every later `open` of the
/// process, on any thread. A setup that fails is not kept, so an `open`
/// after Instruments has finished, say, tries again.
static SETUP: Mutex<Option<Setup>> = Mutex::new(None);

pub struct Counters {
    setup: Setup,
}

impl Counters {
    pub fn open() -> Result<Self> {
        if under_rosetta() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "an x86-64 program under Rosetta: its counts would be those \
                 of the translated code; build it for aarch64-apple-darwin",
            ));
        }
        // A thread that panicked while holding the lock left no setup, or
        // a whole one: the value is never half written.
        let mut kept = SETUP.lock().unwrap_or_else(PoisonError::into_inner);
        let setup = match *kept {
            Some(setup) => setup,
            None => *kept.insert(program()?),
        };
        Ok(Self { setup })
    }

    pub fn read(&self) -> Result<Reading> {
        let mut buffer = [0_u64; KPC_MAX_COUNTERS];
        // SAFETY: thread id 0 is the calling thread; the buffer holds
        // KPC_MAX_COUNTERS values as declared.
        let code = unsafe {
            (self.setup.get_thread_counters)(0, KPC_MAX_COUNTERS as u32, buffer.as_mut_ptr())
        };
        if code != 0 {
            return Err(Error::with_source(
                ErrorKind::Lost,
                "kpc_get_thread_counters failed: the counters stopped counting",
                io::Error::last_os_error(),
            ));
        }
        let map = self.setup.map;
        Ok(Reading {
            instructions: buffer[map[0]],
            cycles: buffer[map[1]],
            branch_misses: buffer[map[2]],
            branches: buffer[map[3]],
        })
    }
}

/// Programs and starts the counters: the four events, user space only,
/// counted per thread.
fn program() -> Result<Setup> {
    let kperf = Library::open(KPERF)?;
    let kperfdata = Library::open(KPERFDATA)?;
    // SAFETY: each type matches the documented kperf/kperfdata signature.
    let (force_all_ctrs_set, set_counting, set_thread_counting, set_config, get_thread_counters) = unsafe {
        (
            kperf.function::<KpcForceAllCtrsSet>(c"kpc_force_all_ctrs_set")?,
            kperf.function::<KpcSetClasses>(c"kpc_set_counting")?,
            kperf.function::<KpcSetClasses>(c"kpc_set_thread_counting")?,
            kperf.function::<KpcSetConfig>(c"kpc_set_config")?,
            kperf.function::<KpcGetThreadCounters>(c"kpc_get_thread_counters")?,
        )
    };
    // SAFETY: as above, for the kperfdata configuration functions.
    let (
        db_create,
        db_event,
        db_free,
        config_create,
        config_force_counters,
        config_add_event,
        config_kpc_classes,
        config_kpc_count,
        config_kpc_map,
        config_kpc,
        config_free,
    ) = unsafe {
        (
            kperfdata.function::<KpepDbCreate>(c"kpep_db_create")?,
            kperfdata.function::<KpepDbEvent>(c"kpep_db_event")?,
            kperfdata.function::<KpepFree>(c"kpep_db_free")?,
            kperfdata.function::<KpepConfigCreate>(c"kpep_config_create")?,
            kperfdata.function::<KpepConfigForceCounters>(c"kpep_config_force_counters")?,
            kperfdata.function::<KpepConfigAddEvent>(c"kpep_config_add_event")?,
            kperfdata.function::<KpepConfigKpcClasses>(c"kpep_config_kpc_classes")?,
            kperfdata.function::<KpepConfigKpcCount>(c"kpep_config_kpc_count")?,
            kperfdata.function::<KpepConfigKpcMap>(c"kpep_config_kpc_map")?,
            kperfdata.function::<KpepConfigKpc>(c"kpep_config_kpc")?,
            kperfdata.function::<KpepFree>(c"kpep_config_free")?,
        )
    };

    // Declared before the configuration, so that it is dropped after it.
    let mut db = Owned::new(db_free);
    // SAFETY: a null name selects the database of the running CPU; `db`
    // receives an owned handle, freed when dropped.
    let created = unsafe { db_create(ptr::null(), &mut db.handle) };
    if created != 0 {
        return Err(if in_virtual_machine() {
            Error::new(
                ErrorKind::NoCounters,
                format!(
                    "a virtual Mac: its kernel has no kpc, and kperf has no event \
                     database for its processor (kpep_db_create code {created}); \
                     count on hardware"
                ),
            )
        } else {
            Error::new(
                ErrorKind::Unsupported,
                format!(
                    "kperf has no event database for this processor \
                     (kpep_db_create code {created})"
                ),
            )
        });
    }
    let mut config = Owned::new(config_free);
    // SAFETY: valid database handle; `config` receives an owned handle.
    kpep(
        unsafe { config_create(db.handle, &mut config.handle) },
        "kpep_config_create",
    )?;
    // SAFETY: valid configuration handle.
    kpep(
        unsafe { config_force_counters(config.handle) },
        "kpep_config_force_counters",
    )?;
    for name in EVENTS {
        let mut event: Opaque = ptr::null_mut();
        // SAFETY: valid database handle and NUL-terminated event name;
        // `event` receives a pointer owned by the database.
        let found = unsafe { db_event(db.handle, name.as_ptr(), &mut event) };
        if found != 0 || event.is_null() {
            return Err(Error::new(
                ErrorKind::MissingEvent,
                format!(
                    "the event {} is not in this processor's kpep database (code {found})",
                    name.to_string_lossy()
                ),
            ));
        }
        // SAFETY: valid configuration and event; no error bitmap output.
        kpep(
            unsafe {
                config_add_event(config.handle, &mut event, USER_SPACE_ONLY, ptr::null_mut())
            },
            "kpep_config_add_event",
        )?;
    }
    let mut classes = 0_u32;
    let mut count = 0_usize;
    let mut map = [0_usize; KPC_MAX_COUNTERS];
    let mut registers = [0_u64; KPC_MAX_COUNTERS];
    // SAFETY: valid configuration; output buffers are sized as passed.
    unsafe {
        kpep(
            config_kpc_classes(config.handle, &mut classes),
            "kpep_config_kpc_classes",
        )?;
        kpep(
            config_kpc_count(config.handle, &mut count),
            "kpep_config_kpc_count",
        )?;
        kpep(
            config_kpc_map(
                config.handle,
                map.as_mut_ptr(),
                size_of::<[usize; KPC_MAX_COUNTERS]>(),
            ),
            "kpep_config_kpc_map",
        )?;
        kpep(
            config_kpc(
                config.handle,
                registers.as_mut_ptr(),
                size_of::<[u64; KPC_MAX_COUNTERS]>(),
            ),
            "kpep_config_kpc",
        )?;
    }
    drop(config);
    drop(db);
    if map
        .iter()
        .take(EVENTS.len())
        .any(|&index| index >= KPC_MAX_COUNTERS)
    {
        return Err(Error::new(
            ErrorKind::Other,
            "the kpep counter map is out of range",
        ));
    }

    // SAFETY: plain kernel calls without pointers.
    kpc(unsafe { force_all_ctrs_set(1) }, "kpc_force_all_ctrs_set")?;
    if classes & KPC_CLASS_CONFIGURABLE_MASK != 0 && count > 0 {
        // SAFETY: `registers` holds `count` valid configuration words for `classes`.
        kpc(
            unsafe { set_config(classes, registers.as_ptr()) },
            "kpc_set_config",
        )?;
    }
    // SAFETY: plain kernel calls.
    unsafe {
        kpc(set_counting(classes), "kpc_set_counting")?;
        kpc(set_thread_counting(classes), "kpc_set_thread_counting")?;
    }
    Ok(Setup {
        get_thread_counters,
        map: [map[0], map[1], map[2], map[3]],
    })
}

pub fn cpu_number() -> Option<usize> {
    let mut cpu = 0_usize;
    // SAFETY: valid out-pointer to a usize (macOS 11+ API).
    let code = unsafe { pthread_cpu_number_np(&mut cpu) };
    (code == 0).then_some(cpu)
}
