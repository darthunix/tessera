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
        ParquetFile::open_checked(path, None, false)
    }

    /// Opens the file at `path` as [`ParquetFile::open_expecting`] does,
    /// or as [`ParquetFile::open`] where `expected` is `None`, taking its
    /// footer from the backend's cache of footers where the cache holds
    /// one for the same path and identity, and keeping it there otherwise.
    ///
    /// The cache holds the footers of [`CACHED_FILES`] files at most, the
    /// least recently used dropped first; a file replaced, even by one of
    /// the same size and time, has another inode and is read anew.
    ///
    /// # Errors
    ///
    /// Those of [`ParquetFile::open`] and [`ParquetFile::open_expecting`].
    pub fn open_cached(path: &Path, expected: Option<&Identity>) -> Result<Self> {
        ParquetFile::open_checked(path, expected, true)
    }

    fn open_checked(path: &Path, expected: Option<&Identity>, cached: bool) -> Result<Self> {
        let file = fs::File::open(path).map_err(|e| error::io(&e, "open Parquet file"))?;
        let identity = stat(&file)?;
        // Another version of the file is refused before its footer is read.
        if expected.is_some_and(|e| *e != identity) {
            return Err(changed());
        }
        if cached && let Some(metadata) = cache::get(path, &identity) {
            return Ok(ParquetFile {
                file: Arc::new(file),
                identity,
                metadata,
            });
        }
        let footer = ParquetMetaDataReader::new()
            .parse_and_finish(&file)
            .map_err(|e| error::damaged(&e));
        // A footer read under a file that changed, whether it decoded or
        // not: the change.
        if !stat(&file)?.same_contents(&identity) {
            return Err(changed());
        }
        let metadata = Arc::new(footer?);
        check_rows(&metadata)?;
        if cached {
            cache::put(path, identity, Arc::clone(&metadata));
        }
        Ok(ParquetFile {
            file: Arc::new(file),
            identity,
            metadata,
        })
    }

    /// Opens the file at `path` and checks it against the identity a scan
    /// found when it began, as `fstat` of the open file gives it.
    ///
    /// # Errors
    ///
    /// Those of [`ParquetFile::open`], and `55000` for a file of another
    /// identity.
    pub fn open_expecting(path: &Path, expected: &Identity) -> Result<Self> {
        ParquetFile::open_checked(path, Some(expected), false)
    }

    /// The error of a read of the file: `55000` where the file changed
    /// since it was opened, so that a read of pages rewritten under a scan
    /// is the change, not bytes the format cannot decode; `error` itself
    /// otherwise.
    pub(crate) fn read_error(&self, error: Error) -> Error {
        match self.check_unchanged() {
            Err(changed) => changed,
            Ok(()) => error,
        }
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

    /// The same footer over another open file, its identity that file's:
    /// for tests of reads the system fails.
    #[cfg(test)]
    pub(crate) fn with_handle(&self, file: fs::File) -> Result<Self> {
        Ok(ParquetFile {
            identity: stat(&file)?,
            file: Arc::new(file),
            metadata: Arc::clone(&self.metadata),
        })
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

    /// The rows of the file, the sum of its groups', which its opening
    /// checked against the footer's count.
    pub fn rows(&self) -> u64 {
        self.metadata
            .row_groups()
            .iter()
            .map(|g| g.num_rows().unsigned_abs())
            .sum()
    }
}

/// The footers of at most this many files are kept in a backend's cache.
pub const CACHED_FILES: usize = 1000;

/// The backend's cache of footers, by path and identity: the one object
/// of the reader that outlives a query, bounded by its count of files.
mod cache {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use parquet::file::metadata::ParquetMetaData;

    use super::{CACHED_FILES, Identity};

    struct Entry {
        path: PathBuf,
        identity: Identity,
        metadata: Arc<ParquetMetaData>,
        used: u64,
    }

    struct Cache {
        entries: Vec<Entry>,
        clock: u64,
    }

    static CACHE: Mutex<Cache> = Mutex::new(Cache {
        entries: Vec::new(),
        clock: 0,
    });

    pub(super) fn get(path: &Path, identity: &Identity) -> Option<Arc<ParquetMetaData>> {
        let mut cache = CACHE.lock().ok()?;
        cache.clock += 1;
        let now = cache.clock;
        let entry = cache
            .entries
            .iter_mut()
            .find(|e| e.identity == *identity && e.path == path)?;
        entry.used = now;
        Some(Arc::clone(&entry.metadata))
    }

    pub(super) fn put(path: &Path, identity: Identity, metadata: Arc<ParquetMetaData>) {
        let Ok(mut cache) = CACHE.lock() else {
            return;
        };
        cache.clock += 1;
        let used = cache.clock;
        // Another version of the same path leaves the cache.
        cache.entries.retain(|e| e.path != path);
        if cache.entries.len() >= CACHED_FILES
            && let Some(oldest) = (0..cache.entries.len()).min_by_key(|&i| cache.entries[i].used)
        {
            cache.entries.swap_remove(oldest);
        }
        cache.entries.push(Entry {
            path: path.to_path_buf(),
            identity,
            metadata,
            used,
        });
    }

    /// The count of footers the cache holds, and whether it holds the one
    /// of `path` under `identity`: for tests.
    #[cfg(test)]
    pub(super) fn holds(path: &Path, identity: &Identity) -> (usize, bool) {
        let Ok(cache) = CACHE.lock() else {
            return (0, false);
        };
        let held = cache
            .entries
            .iter()
            .any(|e| e.identity == *identity && e.path == path);
        (cache.entries.len(), held)
    }
}

/// Refuses a footer whose groups give a negative count of rows, or whose
/// counts do not add up to the file's.
fn check_rows(metadata: &ParquetMetaData) -> Result<()> {
    let mut total = 0u64;
    for group in metadata.row_groups() {
        total = u64::try_from(group.num_rows())
            .ok()
            .and_then(|rows| total.checked_add(rows))
            .ok_or_else(|| bad_footer("a group of a negative count of rows"))?;
    }
    if u64::try_from(metadata.file_metadata().num_rows()).ok() != Some(total) {
        return Err(bad_footer(
            "groups of rows that do not add up to the file's",
        ));
    }
    Ok(())
}

fn bad_footer(what: &str) -> Error {
    Error::new(
        error::DATA_CORRUPTED,
        format!("Parquet footer is damaged: {what}"),
    )
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

    #[test]
    fn another_version_is_refused_before_its_footer_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.parquet");
        testing::write(&path, &[Column::int32("a", &[1, 2])], 2, false).unwrap();
        let first = ParquetFile::open(&path).unwrap();
        // A file cut short under the same path: another inode, its footer
        // never read.
        let cut = dir.path().join("cut.parquet");
        fs::write(&cut, b"PAR1").unwrap();
        fs::rename(&cut, &path).unwrap();
        let error = ParquetFile::open_expecting(&path, &first.identity()).unwrap_err();
        assert_eq!(error.sqlstate(), "55000");
        assert_eq!(ParquetFile::open(&path).unwrap_err().sqlstate(), "XX001");
    }

    #[test]
    fn counts_of_rows_that_do_not_hold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.parquet");
        testing::write(&path, &[Column::int32("a", &[1, 2, 3])], 2, false).unwrap();
        testing::rewrite_footer(
            &path,
            testing::Rewrite {
                first_group_rows: Some(-1),
                ..testing::Rewrite::default()
            },
        )
        .unwrap();
        assert_eq!(ParquetFile::open(&path).unwrap_err().sqlstate(), "XX001");
        // The crate's writer counts the file's rows from its groups: a
        // footer whose counts disagree is made in memory.
        testing::write(&path, &[Column::int32("a", &[1, 2, 3])], 2, false).unwrap();
        let file = ParquetFile::open(&path).unwrap();
        let meta = file.metadata();
        let fm = meta.file_metadata();
        let disagree = ParquetMetaData::new(
            parquet::file::metadata::FileMetaData::new(
                fm.version(),
                fm.num_rows() + 1,
                None,
                None,
                fm.schema_descr_ptr(),
                None,
            ),
            meta.row_groups().to_vec(),
        );
        assert_eq!(check_rows(&disagree).unwrap_err().sqlstate(), "XX001");
        check_rows(meta).unwrap();
    }

    /// The cache is the process's: its tests take turns.
    static CACHE_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn footers_kept_by_path_and_identity() {
        let _turn = CACHE_TESTS.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cached.parquet");
        testing::write(&path, &[Column::int32("a", &[1, 2])], 2, false).unwrap();
        let first = ParquetFile::open_cached(&path, None).unwrap();
        assert!(cache::holds(&path, &first.identity()).1);
        // The same footer, not read again.
        let again = ParquetFile::open_cached(&path, Some(&first.identity())).unwrap();
        assert!(Arc::ptr_eq(first.metadata(), again.metadata()));
        // A file of the same name replaced by a rename: read anew, the old
        // footer dropped, and refused under the old identity.
        let other = dir.path().join("other.parquet");
        testing::write(&other, &[Column::int32("a", &[3, 4, 5])], 3, false).unwrap();
        fs::rename(&other, &path).unwrap();
        let error = ParquetFile::open_cached(&path, Some(&first.identity())).unwrap_err();
        assert_eq!(error.sqlstate(), "55000");
        let replaced = ParquetFile::open_cached(&path, None).unwrap();
        assert_eq!(replaced.rows(), 3);
        assert!(!cache::holds(&path, &first.identity()).1);
        assert!(cache::holds(&path, &replaced.identity()).1);
        // Never more than the bound.
        assert!(cache::holds(&path, &replaced.identity()).0 <= CACHED_FILES);
    }

    #[test]
    fn the_least_recently_used_footer_leaves_a_full_cache() {
        let _turn = CACHE_TESTS.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.parquet");
        testing::write(&first, &[Column::int32("a", &[1])], 1, false).unwrap();
        let kept = ParquetFile::open_cached(&first, None).unwrap();
        let path = dir.path().join("many.parquet");
        testing::write(&path, &[Column::int32("a", &[1])], 1, false).unwrap();
        // Links of one file under many names fill the cache past its bound.
        for i in 0..CACHED_FILES {
            let link = dir.path().join(format!("l{i}.parquet"));
            fs::hard_link(&path, &link).unwrap();
            ParquetFile::open_cached(&link, None).unwrap();
        }
        let (count, held) = cache::holds(&first, &kept.identity());
        assert!(count <= CACHED_FILES);
        assert!(!held);
    }
}
