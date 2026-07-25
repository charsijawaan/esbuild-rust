//! Port of upstream `internal/fs`.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

mod filepath;
mod mock;
mod real;

pub use mock::{MockFs, MockKind, mock_fs};
pub use real::{RealFs, RealFsOptions, real_fs};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum EntryKind {
    None = 0,
    Dir = 1,
    File = 2,
}

#[derive(Debug)]
struct EntryState {
    symlink: String,
    kind: EntryKind,
    need_stat: bool,
}

#[derive(Debug)]
pub struct Entry {
    dir: String,
    base: String,
    state: Mutex<EntryState>,
}

impl Entry {
    fn new(dir: String, base: String, kind: EntryKind, need_stat: bool) -> Self {
        Self {
            dir,
            base,
            state: Mutex::new(EntryState {
                symlink: String::new(),
                kind,
                need_stat,
            }),
        }
    }

    #[must_use]
    pub fn kind(&self, fs: &dyn Fs) -> EntryKind {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.need_stat {
            state.need_stat = false;
            (state.symlink, state.kind) = fs.kind(&self.dir, &self.base);
        }
        state.kind
    }

    #[must_use]
    pub fn symlink(&self, fs: &dyn Fs) -> String {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.need_stat {
            state.need_stat = false;
            (state.symlink, state.kind) = fs.kind(&self.dir, &self.base);
        }
        state.symlink.clone()
    }

    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }
}

#[derive(Debug, Default)]
struct AccessedEntries {
    was_present: HashMap<String, bool>,
    all_entries: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default)]
pub struct DirEntries {
    data: HashMap<String, Arc<Entry>>,
    accessed_entries: Option<Arc<Mutex<AccessedEntries>>>,
    dir: String,
}

impl DirEntries {
    #[must_use]
    pub fn empty(dir: impl Into<String>) -> Self {
        Self {
            dir: dir.into(),
            data: HashMap::new(),
            accessed_entries: None,
        }
    }

    #[must_use]
    pub fn get(&self, query: &str) -> (Option<Arc<Entry>>, Option<DifferentCase>) {
        let key = query.to_lowercase();
        let entry = self.data.get(&key).cloned();
        if let Some(accessed) = &self.accessed_entries {
            accessed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .was_present
                .insert(key, entry.is_some());
        }
        let different_case = entry.as_ref().and_then(|entry| {
            (entry.base != query).then(|| DifferentCase {
                dir: self.dir.clone(),
                query: query.into(),
                actual: entry.base.clone(),
            })
        });
        (entry, different_case)
    }

    #[must_use]
    pub fn peek_entry_count(&self) -> usize {
        self.data.len()
    }

    #[must_use]
    pub fn sorted_keys(&self) -> Vec<String> {
        let mut keys: Vec<_> = self.data.values().map(|entry| entry.base.clone()).collect();
        keys.sort();
        if let Some(accessed) = &self.accessed_entries {
            accessed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .all_entries = Some(keys.clone());
        }
        keys
    }

    fn insert(&mut self, entry: Entry) {
        self.data.insert(entry.base.to_lowercase(), Arc::new(entry));
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DifferentCase {
    pub dir: String,
    pub query: String,
    pub actual: String,
}

pub trait OpenedFile: Send {
    fn len(&self) -> usize;

    /// # Errors
    ///
    /// Returns an error if the range is invalid or the underlying read fails.
    fn read(&mut self, start: usize, end: usize) -> Result<Vec<u8>, FsError>;

    /// # Errors
    ///
    /// Returns an error if the underlying file cannot be closed.
    fn close(&mut self) -> Result<(), FsError>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone, Debug, Default)]
pub struct InMemoryOpenedFile {
    pub contents: Vec<u8>,
}

impl OpenedFile for InMemoryOpenedFile {
    fn len(&self) -> usize {
        self.contents.len()
    }

    fn read(&mut self, start: usize, end: usize) -> Result<Vec<u8>, FsError> {
        self.contents
            .get(start..end)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| FsError::new(FsErrorKind::InvalidInput, "invalid read range"))
    }

    fn close(&mut self) -> Result<(), FsError> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsErrorKind {
    NotFound,
    NotDirectory,
    InvalidInput,
    PermissionDenied,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsError {
    pub kind: FsErrorKind,
    pub message: String,
}

impl FsError {
    #[must_use]
    pub fn new(kind: FsErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn not_found(path: &str) -> Self {
        Self::new(
            FsErrorKind::NotFound,
            format!("no such file or directory: {path}"),
        )
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FsError {}

pub type ReadDirectoryResult = (DirEntries, Option<FsError>, Option<FsError>);
pub type ReadFileResult = (String, Option<FsError>, Option<FsError>);
pub type OpenFileResult = (
    Option<Box<dyn OpenedFile>>,
    Option<FsError>,
    Option<FsError>,
);

pub trait Fs: Send + Sync {
    fn read_directory(&self, path: &str) -> ReadDirectoryResult;
    fn read_file(&self, path: &str) -> ReadFileResult;
    fn open_file(&self, path: &str) -> OpenFileResult;
    /// # Errors
    ///
    /// Returns an error if metadata cannot produce a safe modification key.
    fn mod_key(&self, path: &str) -> Result<ModKey, FsError>;
    fn is_abs(&self, path: &str) -> bool;
    fn abs(&self, path: &str) -> Option<String>;
    fn dir(&self, path: &str) -> String;
    fn base(&self, path: &str) -> String;
    fn ext(&self, path: &str) -> String;
    fn join(&self, parts: &[&str]) -> String;
    fn cwd(&self) -> &str;
    fn rel(&self, base: &str, target: &str) -> Option<String>;
    fn eval_symlinks(&self, path: &str) -> Option<String>;
    fn kind(&self, dir: &str, base: &str) -> (String, EntryKind);
    fn watch_data(&self) -> WatchData;
}

pub type WatchCallback = Arc<dyn Fn() -> String + Send + Sync>;

#[derive(Default)]
pub struct WatchData {
    pub paths: HashMap<String, WatchCallback>,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct ModKey {
    pub inode: u64,
    pub size: i64,
    pub mtime_sec: i64,
    pub mtime_nsec: i64,
    pub mode: u32,
    pub uid: u32,
}

#[cfg(test)]
mod tests {
    use super::{DirEntries, Entry, EntryKind, Fs, InMemoryOpenedFile, OpenedFile};

    #[test]
    fn directory_entries_track_case_and_sorting() {
        let mut entries = DirEntries::empty("/src");
        entries.insert(Entry::new(
            "/src".into(),
            "Index.js".into(),
            EntryKind::File,
            false,
        ));
        let (entry, different) = entries.get("index.js");
        assert!(entry.is_some());
        assert_eq!(different.expect("different case").actual, "Index.js");
        assert_eq!(entries.sorted_keys(), ["Index.js"]);
    }

    #[test]
    fn in_memory_opened_file_checks_ranges() {
        let mut file = InMemoryOpenedFile {
            contents: b"hello".to_vec(),
        };
        assert_eq!(file.len(), 5);
        assert_eq!(file.read(1, 4).expect("read"), b"ell");
        assert!(file.read(4, 6).is_err());
        file.close().expect("close");
    }

    // Keep the object-safety requirement explicit because all higher-level
    // packages store this abstraction behind a trait object.
    fn _assert_object_safe(_: &dyn Fs) {}
}
