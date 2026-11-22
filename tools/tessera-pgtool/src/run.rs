//! What a run of a tool records about itself: an identifier for its
//! directory, the commit it ran on, the machine's power and load, and the
//! hashes of the files it measured.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// Six letters and digits, as the run directories of bench/pg have.
pub fn identifier() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_nanos() as u64);
    let mut state = nanos ^ u64::from(std::process::id()).rotate_left(32);
    (0..6)
        .map(|_| {
            state = splitmix(&mut state);
            ALPHABET[(state % ALPHABET.len() as u64) as usize] as char
        })
        .collect()
}

/// One step of SplitMix64.
pub fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The output of a command, empty when it fails or is missing.
pub fn output(program: &str, args: &[&str], dir: &Path) -> String {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string()
        })
        .unwrap_or_default()
}

/// The commit, marked `+` when tracked files changed.
pub fn head(root: &Path) -> String {
    let commit = output("git", &["rev-parse", "--short=12", "HEAD"], root);
    let changed = !output("git", &["status", "--short", "--untracked-files=no"], root).is_empty();
    format!("{commit}{}", if changed { "+" } else { "" })
}

/// The power source, which changes the speed of a laptop.
pub fn power(root: &Path) -> String {
    let text = output("pmset", &["-g", "batt"], root);
    text.lines()
        .next()
        .and_then(|line| line.split('\'').nth(1))
        .unwrap_or("unknown")
        .to_string()
}

pub fn sha256(path: &Path) -> String {
    match fs::read(path) {
        Ok(bytes) => Sha256::digest(&bytes)
            .iter()
            .fold(String::new(), |mut text, byte| {
                let _ = write!(text, "{byte:02x}");
                text
            }),
        Err(_) => "missing".into(),
    }
}

/// The load average, as the system reports it.
pub fn load_average(root: &Path) -> String {
    if cfg!(target_os = "macos") {
        output("sysctl", &["-n", "vm.loadavg"], root)
    } else {
        fs::read_to_string("/proc/loadavg")
            .unwrap_or_default()
            .trim()
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_six_letters_or_digits() {
        let first = identifier();
        assert_eq!(first.len(), 6);
        assert!(first.bytes().all(|b| b.is_ascii_alphanumeric()));
    }

    #[test]
    fn splitmix_is_the_reference_sequence() {
        // The first outputs of SplitMix64 seeded with 1234567.
        let mut state = 1_234_567;
        assert_eq!(splitmix(&mut state), 6_457_827_717_110_365_317);
        assert_eq!(splitmix(&mut state), 3_203_168_211_198_807_973);
    }

    #[test]
    fn hashes_are_hex() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("empty");
        fs::write(&file, b"").unwrap();
        assert_eq!(
            sha256(&file),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(sha256(&dir.path().join("none")), "missing");
    }
}
