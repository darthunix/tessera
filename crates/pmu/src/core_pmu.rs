//! The choice of the core PMU on Linux, and of its four events.
//!
//! The kernel lists its PMUs under `/sys/bus/event_source/devices`. The
//! PMU of the processor's cores is the device `cpu`, or every device with
//! a file `cpus` that lists its CPUs, one for each kind of core; the
//! devices of the rest of the chip have no such file. The choice takes the
//! root of that tree, the CPUs the thread may run on and the architecture
//! as arguments, so its tests run on any host over trees written for them.
//! It is plain code over files: the system calls are the Linux backend's.

use crate::{Error, ErrorKind};
use std::{collections::BTreeSet, fmt::Write as _, fs, io, path::Path};

/// The `type` of the kernel's generic hardware events.
const PERF_TYPE_HARDWARE: u32 = 0;
// The generic ids of the kernel, `enum perf_hw_id` of its ABI.
const PERF_COUNT_HW_CPU_CYCLES: u64 = 0;
const PERF_COUNT_HW_INSTRUCTIONS: u64 = 1;
const PERF_COUNT_HW_BRANCH_INSTRUCTIONS: u64 = 4;
const PERF_COUNT_HW_BRANCH_MISSES: u64 = 5;
// The common events of the Arm PMU architecture (PMUv3), raw codes on the
// type of the PMU. The kernel's generic ids are not used on AArch64: it
// maps branch misses to the speculative BR_MIS_PRED, and branches to an
// event whose meaning has changed between kernels.
const ARM_INST_RETIRED: u64 = 0x08;
const ARM_CPU_CYCLES: u64 = 0x11;
const ARM_BR_RETIRED: u64 = 0x21;
const ARM_BR_MIS_PRED_RETIRED: u64 = 0x22;

/// The architectures the Linux backend counts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Arch {
    X86_64,
    Aarch64,
}

/// One event of the group: the `type` and `config` of `perf_event_attr`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Event {
    pub kind: u32,
    pub config: u64,
}

/// The chosen PMU and its events, in the order of the fields of
/// [`Reading`](crate::Reading): instructions, cycles, branch misses,
/// branches.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Choice {
    pub pmu: String,
    pub events: [Event; 4],
}

/// A core PMU of the tree: its name, its `type`, and its CPUs, `None` for
/// the device `cpu`, which covers them all.
struct CorePmu {
    name: String,
    kind: u32,
    cpus: Option<BTreeSet<usize>>,
}

/// The CPUs of a list as the kernel writes it, such as `0-3,8,10-11`, or
/// `None` when it is not one.
pub(crate) fn parse_cpu_list(list: &str) -> Option<BTreeSet<usize>> {
    let mut cpus = BTreeSet::new();
    let list = list.trim();
    if list.is_empty() {
        return Some(cpus);
    }
    for part in list.split(',') {
        match part.split_once('-') {
            Some((first, last)) => {
                let (first, last) = (first.parse::<usize>().ok()?, last.parse::<usize>().ok()?);
                if first > last {
                    return None;
                }
                cpus.extend(first..=last);
            }
            None => {
                cpus.insert(part.parse().ok()?);
            }
        }
    }
    Some(cpus)
}

/// A set of CPUs written back as the kernel writes a list, for messages.
fn cpu_list(cpus: &BTreeSet<usize>) -> String {
    let mut list = String::new();
    let mut cpus = cpus.iter().copied().peekable();
    while let Some(first) = cpus.next() {
        let mut last = first;
        while cpus.peek() == Some(&(last + 1)) {
            last += 1;
            cpus.next();
        }
        if !list.is_empty() {
            list.push(',');
        }
        // Writing to a String does not fail.
        let _ = if first == last {
            write!(list, "{first}")
        } else {
            write!(list, "{first}-{last}")
        };
    }
    list
}

/// The CPUs the calling thread may run on, from the `Cpus_allowed_list`
/// line of `/proc/thread-self/status`.
pub(crate) fn allowed_cpus(status: &str) -> Option<BTreeSet<usize>> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .and_then(parse_cpu_list)
}

fn read_error(path: &Path, cause: io::Error) -> Error {
    Error::with_source(
        ErrorKind::Other,
        format!("cannot read {}", path.display()),
        cause,
    )
}

/// The core PMUs listed under `devices`, sorted by name.
fn core_pmus(devices: &Path) -> Result<Vec<CorePmu>, Error> {
    let entries = match fs::read_dir(devices) {
        Ok(entries) => entries,
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(cause) => return Err(read_error(devices, cause)),
    };
    let mut pmus = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|cause| read_error(devices, cause))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let dir = entry.path();
        let cpus = match fs::read_to_string(dir.join("cpus")) {
            Ok(list) => Some(parse_cpu_list(&list).ok_or_else(|| {
                Error::new(
                    ErrorKind::Other,
                    format!(
                        "{} is not a list of CPUs: {list:?}",
                        dir.join("cpus").display()
                    ),
                )
            })?),
            Err(cause) if cause.kind() == io::ErrorKind::NotFound => {
                if name != "cpu" {
                    continue;
                }
                None
            }
            Err(cause) => return Err(read_error(&dir.join("cpus"), cause)),
        };
        let path = dir.join("type");
        let text = fs::read_to_string(&path).map_err(|cause| read_error(&path, cause))?;
        let kind = text.trim().parse().map_err(|_| {
            Error::new(
                ErrorKind::Other,
                format!("{} is not a number: {text:?}", path.display()),
            )
        })?;
        pmus.push(CorePmu { name, kind, cpus });
    }
    pmus.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(pmus)
}

/// The core PMU to count on, and its four events, for a thread that may
/// run on `allowed` CPUs.
///
/// # Errors
///
/// [`ErrorKind::NoCounters`] when the tree lists no core PMU, as in a
/// virtual machine without a virtual one; [`ErrorKind::MixedCores`] when
/// the thread may run on CPUs of two core PMUs; [`ErrorKind::Unsupported`]
/// for a PMU of another kind than the Arm architecture's on AArch64;
/// [`ErrorKind::MissingEvent`] when an Arm PMU lacks a branch event.
pub(crate) fn choose(
    devices: &Path,
    allowed: &BTreeSet<usize>,
    arch: Arch,
) -> Result<Choice, Error> {
    let pmus = core_pmus(devices)?;
    let pmu = match pmus.as_slice() {
        [] => {
            return Err(Error::new(
                ErrorKind::NoCounters,
                format!(
                    "{} lists no PMU of the processor's cores: the machine gives \
                     no hardware counters, as a virtual machine without a virtual \
                     PMU; count on hardware",
                    devices.display()
                ),
            ));
        }
        [only] => only,
        several => match several.iter().find(|pmu| {
            pmu.cpus
                .as_ref()
                .is_none_or(|cpus| !allowed.is_empty() && allowed.is_subset(cpus))
        }) {
            Some(pmu) => pmu,
            None => return Err(mixed_cores(several, allowed)),
        },
    };
    let hybrid = pmus.len() > 1;
    match arch {
        Arch::X86_64 => {
            // On a processor with two kinds of cores the kernel takes the
            // type of the PMU in the high 32 bits of a generic id.
            let extended = if hybrid { u64::from(pmu.kind) << 32 } else { 0 };
            let event = |id| Event {
                kind: PERF_TYPE_HARDWARE,
                config: extended | id,
            };
            Ok(Choice {
                pmu: pmu.name.clone(),
                events: [
                    event(PERF_COUNT_HW_INSTRUCTIONS),
                    event(PERF_COUNT_HW_CPU_CYCLES),
                    event(PERF_COUNT_HW_BRANCH_MISSES),
                    event(PERF_COUNT_HW_BRANCH_INSTRUCTIONS),
                ],
            })
        }
        Arch::Aarch64 => {
            if !(pmu.name.starts_with("armv8_") || pmu.name.starts_with("armv9_")) {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    format!(
                        "the core PMU {} is not of the Arm architecture's kind, \
                         whose events the crate counts",
                        pmu.name
                    ),
                ));
            }
            for event in ["br_retired", "br_mis_pred_retired"] {
                if !devices.join(&pmu.name).join("events").join(event).exists() {
                    return Err(Error::new(
                        ErrorKind::MissingEvent,
                        format!(
                            "the core PMU {} has no event {event}: this core does \
                             not count it, and the crate counts no other event in \
                             its place",
                            pmu.name
                        ),
                    ));
                }
            }
            let event = |code| Event {
                kind: pmu.kind,
                config: code,
            };
            Ok(Choice {
                pmu: pmu.name.clone(),
                events: [
                    event(ARM_INST_RETIRED),
                    event(ARM_CPU_CYCLES),
                    event(ARM_BR_MIS_PRED_RETIRED),
                    event(ARM_BR_RETIRED),
                ],
            })
        }
    }
}

fn mixed_cores(pmus: &[CorePmu], allowed: &BTreeSet<usize>) -> Error {
    let mut message = format!(
        "this thread may run on CPUs {} of more than one kind of core, and an \
         event of one kind does not count on the other; start the program \
         bound to one kind:",
        cpu_list(allowed)
    );
    for pmu in pmus {
        if let Some(cpus) = &pmu.cpus {
            let list = cpu_list(cpus);
            let _ = write!(message, " {}: taskset -c {list} <program>;", pmu.name);
        }
    }
    message.pop();
    Error::new(ErrorKind::MixedCores, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A tree of PMUs written for a test, as sysfs shows them.
    struct Tree(tempfile::TempDir);

    impl Tree {
        fn new() -> Self {
            Self(tempfile::tempdir().unwrap())
        }

        fn pmu(self, name: &str, kind: u32, cpus: Option<&str>, events: &[&str]) -> Self {
            let dir = self.0.path().join(name);
            fs::create_dir_all(dir.join("events")).unwrap();
            fs::write(dir.join("type"), format!("{kind}\n")).unwrap();
            if let Some(cpus) = cpus {
                fs::write(dir.join("cpus"), format!("{cpus}\n")).unwrap();
            }
            for event in events {
                fs::write(dir.join("events").join(event), "event=0x00\n").unwrap();
            }
            self
        }

        fn path(&self) -> PathBuf {
            self.0.path().to_owned()
        }
    }

    fn cpus(list: &str) -> BTreeSet<usize> {
        parse_cpu_list(list).unwrap()
    }

    const ARM_EVENTS: [&str; 4] = [
        "inst_retired",
        "cpu_cycles",
        "br_retired",
        "br_mis_pred_retired",
    ];

    #[test]
    fn lists_of_cpus_parse_and_print_back() {
        assert_eq!(cpus("0-3,8,10-11"), BTreeSet::from([0, 1, 2, 3, 8, 10, 11]));
        assert_eq!(cpus(" 5\n"), BTreeSet::from([5]));
        assert_eq!(cpus(""), BTreeSet::new());
        assert_eq!(parse_cpu_list("3-1"), None);
        assert_eq!(parse_cpu_list("a"), None);
        assert_eq!(parse_cpu_list("1,,2"), None);
        assert_eq!(cpu_list(&cpus("0-3,8,10-11")), "0-3,8,10-11");
        let status = "Name:\ttest\nCpus_allowed:\tff\nCpus_allowed_list:\t0-7\n";
        assert_eq!(allowed_cpus(status), Some(cpus("0-7")));
        assert_eq!(allowed_cpus("Name:\ttest\n"), None);
    }

    #[test]
    fn one_x86_pmu_counts_the_generic_events() {
        let tree = Tree::new()
            .pmu("cpu", 4, None, &[])
            .pmu("software", 1, None, &[])
            .pmu("uncore_imc", 12, None, &[]);
        let choice = choose(&tree.path(), &cpus("0-7"), Arch::X86_64).unwrap();
        assert_eq!(choice.pmu, "cpu");
        let configs = choice.events.map(|event| (event.kind, event.config));
        assert_eq!(configs, [(0, 1), (0, 0), (0, 5), (0, 4)]);
    }

    #[test]
    fn two_kinds_of_x86_cores_count_on_the_bound_one() {
        let tree =
            Tree::new()
                .pmu("cpu_core", 4, Some("0-7"), &[])
                .pmu("cpu_atom", 10, Some("8-15"), &[]);
        let choice = choose(&tree.path(), &cpus("8-11"), Arch::X86_64).unwrap();
        assert_eq!(choice.pmu, "cpu_atom");
        assert_eq!(
            choice.events[0],
            Event {
                kind: 0,
                config: 10 << 32 | 1
            }
        );
        assert_eq!(
            choice.events[3],
            Event {
                kind: 0,
                config: 10 << 32 | 4
            }
        );
        let choice = choose(&tree.path(), &cpus("0-7"), Arch::X86_64).unwrap();
        assert_eq!(choice.pmu, "cpu_core");
    }

    #[test]
    fn an_unbound_thread_on_two_kinds_of_cores_is_refused() {
        let tree =
            Tree::new()
                .pmu("cpu_core", 4, Some("0-7"), &[])
                .pmu("cpu_atom", 10, Some("8-15"), &[]);
        let error = choose(&tree.path(), &cpus("0-15"), Arch::X86_64).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MixedCores);
        let message = error.to_string();
        assert!(message.contains("CPUs 0-15"), "{message}");
        assert!(
            message.contains("cpu_atom: taskset -c 8-15 <program>"),
            "{message}"
        );
        assert!(
            message.contains("cpu_core: taskset -c 0-7 <program>"),
            "{message}"
        );
    }

    #[test]
    fn an_arm_pmu_counts_the_architectural_events() {
        let tree = Tree::new()
            .pmu("armv8_pmuv3_0", 10, Some("0-3"), &ARM_EVENTS)
            .pmu("software", 1, None, &[]);
        let choice = choose(&tree.path(), &cpus("0-3"), Arch::Aarch64).unwrap();
        assert_eq!(choice.pmu, "armv8_pmuv3_0");
        let configs = choice.events.map(|event| (event.kind, event.config));
        assert_eq!(configs, [(10, 0x08), (10, 0x11), (10, 0x22), (10, 0x21)]);
    }

    #[test]
    fn big_and_little_arm_cores_count_on_the_bound_one() {
        let tree = Tree::new()
            .pmu("armv8_cortex_a55", 8, Some("0-3"), &ARM_EVENTS)
            .pmu("armv8_cortex_a76", 9, Some("4-7"), &ARM_EVENTS);
        let choice = choose(&tree.path(), &cpus("4-7"), Arch::Aarch64).unwrap();
        assert_eq!(choice.pmu, "armv8_cortex_a76");
        assert_eq!(
            choice.events[1],
            Event {
                kind: 9,
                config: 0x11
            }
        );
        let error = choose(&tree.path(), &cpus("3-4"), Arch::Aarch64).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MixedCores);
    }

    #[test]
    fn an_arm_pmu_without_a_branch_event_is_refused() {
        let tree = Tree::new().pmu(
            "armv8_cortex_a72",
            8,
            Some("0-3"),
            &["inst_retired", "cpu_cycles", "br_mis_pred"],
        );
        let error = choose(&tree.path(), &cpus("0-3"), Arch::Aarch64).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MissingEvent);
        assert!(error.to_string().contains("br_retired"), "{error}");
        let tree = Tree::new().pmu(
            "armv8_pmuv3_0",
            8,
            Some("0-3"),
            &["inst_retired", "cpu_cycles", "br_retired"],
        );
        let error = choose(&tree.path(), &cpus("0-3"), Arch::Aarch64).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MissingEvent);
        assert!(error.to_string().contains("br_mis_pred_retired"), "{error}");
    }

    #[test]
    fn a_pmu_of_another_kind_is_refused() {
        let tree = Tree::new()
            .pmu("apple_icestorm_pmu", 8, Some("0-3"), &[])
            .pmu("apple_firestorm_pmu", 9, Some("4-7"), &[]);
        let error = choose(&tree.path(), &cpus("4-7"), Arch::Aarch64).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn a_tree_without_a_core_pmu_has_no_counters() {
        let tree = Tree::new()
            .pmu("software", 1, None, &[])
            .pmu("tracepoint", 2, None, &[]);
        let error = choose(&tree.path(), &cpus("0-3"), Arch::X86_64).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NoCounters);
        let missing = tree.path().join("missing");
        let error = choose(&missing, &cpus("0-3"), Arch::Aarch64).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NoCounters);
    }
}
