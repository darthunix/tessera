#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

//! Ctrl-C at the terminal ends a tool that runs a step under a deadline,
//! and the step with it, though the step's group is not the terminal's.
//! The test binary runs itself as the tool: the test `tool` does nothing
//! unless the variable `TESSERA_TOOL_PID_FILE` names a file.

use std::fs;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tessera_pgtool::run_until;

const PID_FILE: &str = "TESSERA_TOOL_PID_FILE";

/// Whether `kill` reaches the process or group `target`; `-` and a
/// number name a group.
fn reaches(signal: &str, target: &str) -> bool {
    Command::new("kill")
        .args([signal, "--", target])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// The tool: a step with a deadline a minute away, whose child ignores
/// SIGTERM, then writes its id, then sleeps: once the id is written,
/// only SIGKILL ends it.
#[test]
fn tool() {
    let Some(file) = std::env::var_os(PID_FILE) else {
        return;
    };
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(format!(
        "/bin/sh -c \"trap '' TERM; echo \\$\\$ > '{}'; exec sleep 60\" & wait",
        Path::new(&file).display()
    ));
    let _ = run_until(&mut command, Some(Instant::now() + Duration::from_secs(60)));
}

#[test]
fn ctrl_c_ends_a_step_under_a_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("pid");
    // The tool in a group of its own, as a terminal's foreground job.
    let mut tool = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tool", "--nocapture"])
        .env(PID_FILE, &file)
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let pid = loop {
        if let Some(pid) = fs::read_to_string(&file)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
            // Never 1 or less: a signal to them reaches other processes.
            .filter(|&pid| pid > 1)
        {
            break pid.to_string();
        }
        assert!(started.elapsed() < Duration::from_secs(20), "no step");
        thread::sleep(Duration::from_millis(20));
    };
    assert!(reaches("-INT", &format!("-{}", tool.id())));
    tool.wait().unwrap();
    // The watchdog ends the step's group within a second or two.
    let started = Instant::now();
    while reaches("-0", &pid) {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the step outlived its tool"
        );
        thread::sleep(Duration::from_millis(50));
    }
}
