//! Immutable source snapshots and compatibility checks. Never checks out a branch
//! or stages files. Generated snapshots, logs and results stay in the run directory.

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub fn output(command: &mut Command) -> Result<Vec<u8>> {
    let output = command
        .output()
        .with_context(|| format!("cannot start {command:?}"))?;
    ensure!(
        output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

pub fn text(command: &mut Command) -> Result<String> {
    Ok(String::from_utf8(output(command)?)?.trim().to_owned())
}

pub fn files(root: &Path) -> Result<Vec<PathBuf>> {
    fn walk(root: &Path, dir: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            ensure!(
                !kind.is_symlink(),
                "snapshot contains a symlink: {}",
                entry.path().display()
            );
            if kind.is_dir() {
                if entry.file_name() != "target" && entry.file_name() != ".git" {
                    walk(root, &entry.path(), paths)?;
                }
            } else {
                ensure!(
                    kind.is_file(),
                    "not a regular file: {}",
                    entry.path().display()
                );
                paths.push(entry.path().strip_prefix(root)?.to_owned());
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    walk(root, root, &mut paths)?;
    paths.sort();
    Ok(paths)
}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Serialize)]
pub struct Snapshot {
    pub revision: String,
    pub source_sha256: String,
    pub compatibility: BTreeMap<PathBuf, String>,
    pub directory: PathBuf,
}

fn copy_files(source: &Path, destination: &Path, paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        ensure!(
            path.is_relative()
                && path
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "invalid source path"
        );
        let from = source.join(path);
        ensure!(
            fs::symlink_metadata(&from)?.is_file(),
            "not a regular source file: {}",
            from.display()
        );
        let to = destination.join(path);
        fs::create_dir_all(to.parent().unwrap())?;
        fs::copy(&from, &to).with_context(|| format!("cannot snapshot {}", from.display()))?;
    }
    Ok(())
}

impl Snapshot {
    pub fn capture(repo: &Path, revision: &str, directory: PathBuf) -> Result<Self> {
        let commit = text(Command::new("git").current_dir(repo).args([
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!(
                "{}^{{commit}}",
                if revision == "WORKTREE" {
                    "HEAD"
                } else {
                    revision
                }
            ),
        ]))?;
        fs::create_dir(&directory)?;
        if revision == "WORKTREE" {
            let list = output(Command::new("git").current_dir(repo).args([
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ]))?;
            let mut paths = Vec::new();
            for path in list.split(|&b| b == 0).filter(|p| !p.is_empty()) {
                let path =
                    PathBuf::from(std::str::from_utf8(path).context("non-UTF-8 source path")?);
                // Missing tracked files represent unstaged deletions, not snapshot errors.
                match fs::symlink_metadata(repo.join(&path)) {
                    Ok(_) => paths.push(path),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            paths.sort();
            paths.dedup();
            copy_files(repo, &directory, &paths)?;
        } else {
            let archive = directory.with_extension("tar");
            output(
                Command::new("git")
                    .current_dir(repo)
                    .args(["archive", "--format=tar", "--output"])
                    .arg(&archive)
                    .arg(&commit),
            )?;
            output(
                Command::new("tar")
                    .args(["-xf"])
                    .arg(&archive)
                    .arg("-C")
                    .arg(&directory),
            )?;
        }
        Self::inspect(
            directory,
            if revision == "WORKTREE" {
                format!("WORKTREE@{commit}")
            } else {
                commit
            },
        )
    }

    pub fn duplicate(&self, directory: PathBuf) -> Result<Self> {
        fs::create_dir(&directory)?;
        copy_files(&self.directory, &directory, &files(&self.directory)?)?;
        Self::inspect(directory, self.revision.clone())
    }

    fn inspect(directory: PathBuf, revision: String) -> Result<Self> {
        let mut hash = Sha256::new();
        let mut compatibility = BTreeMap::new();
        for path in files(&directory)? {
            let name = path.to_str().context("non-UTF-8 snapshot path")?;
            let bytes = fs::read(directory.join(&path))?;
            let file_hash = digest(&bytes);
            hash.update(name.as_bytes());
            hash.update([0]);
            hash.update(file_hash.as_bytes());
            if name.ends_with("Cargo.toml")
                || name.ends_with("Cargo.lock")
                || name == "rust-toolchain"
                || name == "rust-toolchain.toml"
                || name.starts_with(".cargo/")
                || name.contains("/.cargo/")
                || (name.starts_with("crates/tessera-capi/benches/") && name.ends_with(".rs"))
            {
                compatibility.insert(path, file_hash);
            }
        }
        Ok(Self {
            revision,
            directory,
            compatibility,
            source_sha256: format!("{:x}", hash.finalize()),
        })
    }

    pub fn check_compatible(&self, other: &Self) -> Result<()> {
        let differences: Vec<_> = self
            .compatibility
            .keys()
            .chain(other.compatibility.keys())
            .filter(|path| self.compatibility.get(*path) != other.compatibility.get(*path))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        ensure!(
            differences.is_empty(),
            "incompatible benchmark/build files: {differences:?}; establish a new baseline after benchmark changes (legacy TSV runs are not supported)"
        );
        for required in [
            "Cargo.toml",
            "Cargo.lock",
            "crates/tessera-capi/benches/support/mod.rs",
        ] {
            ensure!(
                self.compatibility.contains_key(Path::new(required)),
                "missing {required}"
            );
        }
        ensure!(
            fs::read_to_string(
                self.directory
                    .join("crates/tessera-capi/benches/support/mod.rs")
            )?
            .contains("criterion::Criterion"),
            "legacy benchmarks are not supported; establish a Criterion baseline"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_preserve_index_dirty_files_and_untracked_sources() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("repo");
        fs::create_dir(&repo)?;
        let git = |args: &[&str]| text(Command::new("git").current_dir(&repo).args(args));
        git(&["init", "-q"])?;
        for (name, value) in [
            ("Cargo.toml", "manifest"),
            ("Cargo.lock", "lock"),
            (".gitignore", "target/\n"),
            ("remove", "old"),
        ] {
            fs::write(repo.join(name), value)?;
        }
        fs::create_dir_all(repo.join("crates/tessera-capi/benches/support"))?;
        fs::write(
            repo.join("crates/tessera-capi/benches/support/mod.rs"),
            "criterion::Criterion",
        )?;
        git(&["add", "."])?;
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qm",
            "fixture",
        ])?;
        let clean = Snapshot::capture(&repo, "HEAD", temp.path().join("clean"))?;
        fs::write(repo.join("new.rs"), "new")?;
        fs::write(repo.join("staged.rs"), "staged")?;
        git(&["add", "staged.rs"])?;
        fs::write(repo.join("staged.rs"), "working")?;
        fs::remove_file(repo.join("remove"))?;
        fs::create_dir(repo.join("target"))?;
        fs::write(repo.join("target/ignored"), "ignored")?;
        let status = git(&["status", "--porcelain"])?;
        let index = fs::read(repo.join(".git/index"))?;
        let current = Snapshot::capture(&repo, "WORKTREE", temp.path().join("current"))?;
        assert_eq!(git(&["status", "--porcelain"])?, status);
        assert_eq!(fs::read(repo.join(".git/index"))?, index);
        assert_eq!(
            fs::read_to_string(current.directory.join("staged.rs"))?,
            "working"
        );
        assert!(current.directory.join("new.rs").exists());
        assert!(!current.directory.join("remove").exists());
        assert!(!current.directory.join("target").exists());
        clean.check_compatible(&current)?;
        assert_ne!(clean.source_sha256, current.source_sha256);
        let duplicate = current.duplicate(temp.path().join("duplicate"))?;
        assert_eq!(current.source_sha256, duplicate.source_sha256);
        fs::write(repo.join("Cargo.lock"), "changed")?;
        let changed = Snapshot::capture(&repo, "WORKTREE", temp.path().join("changed"))?;
        assert!(current.check_compatible(&changed).is_err());
        assert!(Snapshot::capture(&repo, "HEAD", clean.directory).is_err());
        Ok(())
    }
}
