//! A Parquet file opened for reading: its descriptor, what `fstat` said of
//! it when it was opened, and its footer.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;

use parquet::file::metadata::{ParquetMetaData, ParquetMetaDataReader};

use crate::error::{self, Error, OBJECT_NOT_IN_PREREQUISITE_STATE, Result};

/// What `fstat` says of a file: its size, its time of modification, its
/// device and its inode. Two reads of a file under one identity read one
/// version of it, where the file system gives a write a new time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Identity {
    /// The size in bytes.
    pub size: u64,
    /// The time of modification, seconds since 1970.
    pub mtime_sec: i64,
    /// The nanoseconds of the time of modification.
    pub mtime_nsec: i64,
    /// The device of the file system.
    pub dev: u64,
    /// The inode.
    pub ino: u64,
}

impl Identity {
    fn of(metadata: &fs::Metadata) -> Self {
        Identity {
            size: metadata.size(),
            mtime_sec: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }

    /// Whether `other` has this one's size and time: the check after each
    /// read of an open file, whose device and inode cannot change.
    fn same_contents(&self, other: &Identity) -> bool {
        self.size == other.size
            && self.mtime_sec == other.mtime_sec
            && self.mtime_nsec == other.mtime_nsec
    }
}

/// A Parquet file opened for reading, its footer read.
///
/// The descriptor is closed when the last clone of the file is dropped.
#[derive(Clone, Debug)]
pub struct ParquetFile {
    file: Arc<fs::File>,
    identity: Identity,
    metadata: Arc<ParquetMetaData>,
}

impl ParquetFile {
    /// Opens the file at `path` and reads its footer.
    ///
    /// # Errors
    ///
    /// `58P01` for a file that does not exist, `58030` for one that cannot
    /// be opened or read, `XX001` for a footer the format cannot decode,
    /// and `55000` for a file whose size or time changed while its footer
    /// was read.
    pub fn open(path: &Path) -> Result<Self> {
        let file = fs::File::open(path).map_err(|e| error::io(&e, "open Parquet file"))?;
        let identity = stat(&file)?;
        let metadata = ParquetMetaDataReader::new()
            .parse_and_finish(&file)
            .map_err(|e| error::damaged(&e))?;
        let opened = ParquetFile {
            file: Arc::new(file),
            identity,
            metadata: Arc::new(metadata),
        };
        opened.check_unchanged()?;
        Ok(opened)
    }

    /// Opens the file at `path` and checks it against the identity a scan
    /// found when it began, as `fstat` of the open file gives it.
    ///
    /// # Errors
    ///
    /// Those of [`ParquetFile::open`], and `55000` for a file of another
    /// identity.
    pub fn open_expecting(path: &Path, expected: &Identity) -> Result<Self> {
        let opened = ParquetFile::open(path)?;
        if opened.identity != *expected {
            return Err(changed());
        }
        Ok(opened)
    }

    /// Checks that the open file still has the size and the time it had
    /// when it was opened: the check after each read, before what it read
    /// is used.
    ///
    /// # Errors
    ///
    /// `55000` for a file whose size or time changed, `58030` for an
    /// `fstat` that fails.
    pub fn check_unchanged(&self) -> Result<()> {
        if !stat(&self.file)?.same_contents(&self.identity) {
            return Err(changed());
        }
        Ok(())
    }

    /// What `fstat` said of the file when it was opened.
    pub fn identity(&self) -> Identity {
        self.identity
    }

    /// The footer.
    pub fn metadata(&self) -> &Arc<ParquetMetaData> {
        &self.metadata
    }

    /// The open file, for the readers of its pages.
    pub(crate) fn handle(&self) -> &Arc<fs::File> {
        &self.file
    }

    /// The count of groups of rows.
    pub fn groups(&self) -> usize {
        self.metadata.num_row_groups()
    }

    /// The rows of group `group`, or `None` past the last group.
    pub fn group_rows(&self, group: usize) -> Option<u64> {
        self.metadata
            .row_groups()
            .get(group)
            .and_then(|g| u64::try_from(g.num_rows()).ok())
    }

    /// The rows of the file, the sum of its groups'.
    pub fn rows(&self) -> u64 {
        (0..self.groups()).filter_map(|g| self.group_rows(g)).sum()
    }
}

fn stat(file: &fs::File) -> Result<Identity> {
    let metadata = file
        .metadata()
        .map_err(|e| error::io(&e, "stat Parquet file"))?;
    Ok(Identity::of(&metadata))
}

fn changed() -> Error {
    Error::new(
        OBJECT_NOT_IN_PREREQUISITE_STATE,
        "Parquet file changed while it was read",
    )
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::testing::{self, Column};

    #[test]
    fn a_file_and_its_groups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.parquet");
        testing::write(&path, &[Column::int32("a", &[1, 2, 3, 4, 5])], 2, false).unwrap();
        let file = ParquetFile::open(&path).unwrap();
        assert_eq!(file.groups(), 3);
        assert_eq!(
            (file.group_rows(0), file.group_rows(2), file.group_rows(3)),
            (Some(2), Some(1), None)
        );
        assert_eq!(file.rows(), 5);
        let again = ParquetFile::open_expecting(&path, &file.identity()).unwrap();
        assert_eq!(again.identity(), file.identity());
        file.check_unchanged().unwrap();
    }

    #[test]
    fn a_missing_file_a_damaged_one_and_one_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let missing = ParquetFile::open(&dir.path().join("none.parquet")).unwrap_err();
        assert_eq!(missing.sqlstate(), "58P01");
        let path = dir.path().join("cut.parquet");
        fs::write(&path, b"PAR1 not a footer").unwrap();
        assert_eq!(ParquetFile::open(&path).unwrap_err().sqlstate(), "XX001");

        let path = dir.path().join("a.parquet");
        testing::write(&path, &[Column::int32("a", &[1, 2])], 2, false).unwrap();
        let first = ParquetFile::open(&path).unwrap();
        // Another file of other rows under the same path: another inode.
        let other = dir.path().join("b.parquet");
        testing::write(&other, &[Column::int32("a", &[3, 4])], 2, false).unwrap();
        fs::rename(&other, &path).unwrap();
        let error = ParquetFile::open_expecting(&path, &first.identity()).unwrap_err();
        assert_eq!(error.sqlstate(), "55000");
    }

    #[test]
    fn a_time_changed_with_the_size_kept_is_seen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.parquet");
        testing::write(&path, &[Column::int32("a", &[1, 2])], 2, false).unwrap();
        let file = ParquetFile::open(&path).unwrap();
        let handle = fs::File::options().write(true).open(&path).unwrap();
        let then = fs::metadata(&path).unwrap().modified().unwrap();
        // A minute later, and a nanosecond later: the size kept each time.
        handle
            .set_modified(then + std::time::Duration::from_secs(60))
            .unwrap();
        assert_eq!(file.check_unchanged().unwrap_err().sqlstate(), "55000");
        handle
            .set_modified(then + std::time::Duration::from_nanos(1000))
            .unwrap();
        assert_eq!(file.check_unchanged().unwrap_err().sqlstate(), "55000");
        handle.set_modified(then).unwrap();
        file.check_unchanged().unwrap();
    }

    #[test]
    fn a_file_rewritten_in_place_is_seen_after_a_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.parquet");
        testing::write(&path, &[Column::int32("a", &[1, 2])], 2, false).unwrap();
        let file = ParquetFile::open(&path).unwrap();
        let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writer.write_all(b"more").unwrap();
        drop(writer);
        assert_eq!(file.check_unchanged().unwrap_err().sqlstate(), "55000");
    }
}
