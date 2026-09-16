//! kperf backend: the private kperf and kperfdata frameworks, loaded with
//! dlopen so that nothing links against them at build time.
//!
//! The sequence follows Apple's own kpc/kpep usage: resolve the event
//! database for this CPU, build a counter configuration from event names,
//! reserve all counters (root), program and start them, and enable per-thread
//! accumulation. Reads return the calling thread's counters.

use crate::Reading;
use anyhow::{Result, bail, ensure};
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    mem::size_of,
    ptr,
};

const RTLD_LAZY: c_int = 0x1;
const KPC_CLASS_CONFIGURABLE_MASK: u32 = 1 << 1;
const KPC_MAX_COUNTERS: usize = 32;

/// Event names in the order of the fields of [`Reading`].
const EVENTS: [&str; 4] = [
    "FIXED_INSTRUCTIONS",
    "FIXED_CYCLES",
    "BRANCH_MISPRED_NONSPEC",
    "INST_BRANCH",
];

const KPERF: &str = "/System/Library/PrivateFrameworks/kperf.framework/kperf";
const KPERFDATA: &str = "/System/Library/PrivateFrameworks/kperfdata.framework/kperfdata";

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
    fn pthread_cpu_number_np(cpu_number_out: *mut usize) -> c_int;
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

struct Library(*mut c_void);

impl Library {
    fn open(path: &str) -> Result<Self> {
        let cpath = CString::new(path)?;
        // SAFETY: cpath is a valid NUL-terminated path; the handle stays open
        // for the process lifetime (never dlclose'd), so resolved symbols
        // remain valid.
        let handle = unsafe { dlopen(cpath.as_ptr(), RTLD_LAZY) };
        ensure!(!handle.is_null(), "cannot load {path}: {}", last_dl_error());
        Ok(Self(handle))
    }

    /// Resolve `name` as a function pointer of type `F`.
    ///
    /// # Safety
    ///
    /// `F` must be an `unsafe extern "C" fn` type matching the symbol's real
    /// signature; the caller vouches for the ABI of the private framework.
    unsafe fn function<F: Copy>(&self, name: &str) -> Result<F> {
        const {
            assert!(size_of::<F>() == size_of::<*mut c_void>());
        }
        let cname = CString::new(name)?;
        // SAFETY: valid handle and NUL-terminated symbol name.
        let symbol = unsafe { dlsym(self.0, cname.as_ptr()) };
        ensure!(
            !symbol.is_null(),
            "missing symbol {name}: {}",
            last_dl_error()
        );
        // SAFETY: F is a function pointer type of pointer size (checked above)
        // and the symbol is a function of that signature per the caller.
        Ok(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&symbol) })
    }
}

fn check(code: c_int, what: &str) -> Result<()> {
    ensure!(code == 0, "{what} failed with code {code}");
    Ok(())
}

pub struct Counters {
    get_thread_counters: KpcGetThreadCounters,
    map: [usize; EVENTS.len()],
}

impl Counters {
    pub fn open() -> Result<Self> {
        let kperf = Library::open(KPERF)?;
        let kperfdata = Library::open(KPERFDATA)?;
        // SAFETY: each type matches the documented kperf/kperfdata signature.
        let (
            force_all_ctrs_set,
            set_counting,
            set_thread_counting,
            set_config,
            get_thread_counters,
        ) = unsafe {
            (
                kperf.function::<KpcForceAllCtrsSet>("kpc_force_all_ctrs_set")?,
                kperf.function::<KpcSetClasses>("kpc_set_counting")?,
                kperf.function::<KpcSetClasses>("kpc_set_thread_counting")?,
                kperf.function::<KpcSetConfig>("kpc_set_config")?,
                kperf.function::<KpcGetThreadCounters>("kpc_get_thread_counters")?,
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
                kperfdata.function::<KpepDbCreate>("kpep_db_create")?,
                kperfdata.function::<KpepDbEvent>("kpep_db_event")?,
                kperfdata.function::<KpepFree>("kpep_db_free")?,
                kperfdata.function::<KpepConfigCreate>("kpep_config_create")?,
                kperfdata.function::<KpepConfigForceCounters>("kpep_config_force_counters")?,
                kperfdata.function::<KpepConfigAddEvent>("kpep_config_add_event")?,
                kperfdata.function::<KpepConfigKpcClasses>("kpep_config_kpc_classes")?,
                kperfdata.function::<KpepConfigKpcCount>("kpep_config_kpc_count")?,
                kperfdata.function::<KpepConfigKpcMap>("kpep_config_kpc_map")?,
                kperfdata.function::<KpepConfigKpc>("kpep_config_kpc")?,
                kperfdata.function::<KpepFree>("kpep_config_free")?,
            )
        };

        let mut db: Opaque = ptr::null_mut();
        // SAFETY: a null name selects the database of the running CPU; `db`
        // receives an owned handle that is freed below.
        check(unsafe { db_create(ptr::null(), &mut db) }, "kpep_db_create")?;
        let mut config: Opaque = ptr::null_mut();
        // SAFETY: valid database handle; `config` receives an owned handle.
        check(
            unsafe { config_create(db, &mut config) },
            "kpep_config_create",
        )?;
        // SAFETY: valid configuration handle.
        check(
            unsafe { config_force_counters(config) },
            "kpep_config_force_counters",
        )?;
        for name in EVENTS {
            let cname = CString::new(name)?;
            let mut event: Opaque = ptr::null_mut();
            // SAFETY: valid database handle and NUL-terminated event name;
            // `event` receives a pointer owned by the database.
            let found = unsafe { db_event(db, cname.as_ptr(), &mut event) };
            ensure!(
                found == 0 && !event.is_null(),
                "PMU event {name} is not in this CPU's kpep database (code {found})"
            );
            // SAFETY: valid configuration and event; flags 0, no error output.
            check(
                unsafe { config_add_event(config, &mut event, 0, ptr::null_mut()) },
                "kpep_config_add_event",
            )?;
        }
        let mut classes = 0_u32;
        let mut count = 0_usize;
        let mut map = [0_usize; KPC_MAX_COUNTERS];
        let mut registers = [0_u64; KPC_MAX_COUNTERS];
        // SAFETY: valid configuration; output buffers are sized as passed.
        unsafe {
            check(
                config_kpc_classes(config, &mut classes),
                "kpep_config_kpc_classes",
            )?;
            check(
                config_kpc_count(config, &mut count),
                "kpep_config_kpc_count",
            )?;
            check(
                config_kpc_map(
                    config,
                    map.as_mut_ptr(),
                    size_of::<[usize; KPC_MAX_COUNTERS]>(),
                ),
                "kpep_config_kpc_map",
            )?;
            check(
                config_kpc(
                    config,
                    registers.as_mut_ptr(),
                    size_of::<[u64; KPC_MAX_COUNTERS]>(),
                ),
                "kpep_config_kpc",
            )?;
            config_free(config);
            db_free(db);
        }
        ensure!(
            map.iter()
                .take(EVENTS.len())
                .all(|&index| index < KPC_MAX_COUNTERS),
            "kpep counter map is out of range"
        );

        // SAFETY: plain kernel calls without pointers.
        let forced = unsafe { force_all_ctrs_set(1) };
        if forced != 0 {
            bail!(
                "kpc_force_all_ctrs_set failed with code {forced}: PMU counters need root; \
                 run `sudo -v`, then start this process with `sudo -n`"
            );
        }
        if classes & KPC_CLASS_CONFIGURABLE_MASK != 0 && count > 0 {
            // SAFETY: `registers` holds `count` valid configuration words for `classes`.
            check(
                unsafe { set_config(classes, registers.as_ptr()) },
                "kpc_set_config",
            )?;
        }
        // SAFETY: plain kernel calls.
        unsafe {
            check(set_counting(classes), "kpc_set_counting")?;
            check(set_thread_counting(classes), "kpc_set_thread_counting")?;
        }
        Ok(Self {
            get_thread_counters,
            map: [map[0], map[1], map[2], map[3]],
        })
    }

    pub fn read(&self) -> Reading {
        let mut buffer = [0_u64; KPC_MAX_COUNTERS];
        // SAFETY: thread id 0 is the calling thread; the buffer holds
        // KPC_MAX_COUNTERS values as declared.
        let code =
            unsafe { (self.get_thread_counters)(0, KPC_MAX_COUNTERS as u32, buffer.as_mut_ptr()) };
        assert_eq!(code, 0, "kpc_get_thread_counters failed with code {code}");
        Reading {
            instructions: buffer[self.map[0]],
            cycles: buffer[self.map[1]],
            branch_misses: buffer[self.map[2]],
            branches: buffer[self.map[3]],
        }
    }
}

pub fn cpu_number() -> usize {
    let mut cpu = usize::MAX;
    // SAFETY: valid out-pointer to a usize (macOS 11+ API).
    let code = unsafe { pthread_cpu_number_np(&mut cpu) };
    assert_eq!(code, 0, "pthread_cpu_number_np failed with code {code}");
    cpu
}
