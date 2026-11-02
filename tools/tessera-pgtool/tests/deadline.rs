#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

//! A step that ends while the tool cannot see it, past its deadline: the
//! deadline still ends the processes the step left. A test binary of its
//! own, since it stops its whole process for a moment.

use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tessera_pgtool::run_until;

fn alive(pid: &str) -> bool {
    for _ in 0..100 {
        let running = Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        if !running {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
    true
}

#[test]
fn a_step_that_ends_unseen_past_its_deadline_leaves_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let pid = directory.path().join("pid");
    // This process is stopped from 0.1 s to 0.6 s; the step ends at
    // 0.3 s, past its deadline of 0.2 s, while nothing can poll it, and
    // leaves a descendant that ignores SIGTERM.
    let me = std::process::id();
    let mut freezer = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "sleep 0.1; kill -STOP {me}; sleep 0.5; kill -CONT {me}"
        ))
        .spawn()
        .unwrap();
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(format!(
        "(trap '' TERM; exec sleep 60) & echo $! > '{}'; sleep 0.3; exit 0",
        pid.display()
    ));
    let outcome = run_until(
        &mut command,
        Some(Instant::now() + Duration::from_millis(200)),
    );
    freezer.wait().unwrap();
    assert!(
        outcome.is_err(),
        "a step that ended past its deadline passed"
    );
    assert!(
        !alive(&fs::read_to_string(&pid).unwrap()),
        "a descendant outlived the deadline"
    );
}
