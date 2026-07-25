use super::filepath::GoFilepath;
use super::{
    AccessedEntries, DirEntries, Entry, EntryKind, Fs, FsError, FsErrorKind, ModKey,
    OpenFileResult, OpenedFile, ReadDirectoryResult, ReadFileResult, WatchCallback, WatchData,
};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MOD_KEY_SAFETY_GAP: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Default)]
pub struct RealFsOptions {
    pub abs_working_dir: String,
    pub want_watch_data: bool,
    pub do_not_cache: bool,
}

#[derive(Clone)]
struct EntriesOrError {
    entries: DirEntries,
    canonical_error: Option<FsError>,
    original_error: Option<FsError>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum WatchState {
    #[default]
    None,
    DirHasAccessedEntries,
    DirUnreadable,
    FileHasModKey,
    FileNeedModKey,
    FileMissing,
    FileUnusableModKey,
}

#[derive(Clone, Default)]
struct PrivateWatchData {
    accessed_entries: Option<Arc<Mutex<AccessedEntries>>>,
    file_contents: String,
    mod_key: ModKey,
    state: WatchState,
}

pub struct RealFs {
    entries: Mutex<HashMap<String, EntriesOrError>>,
    watch_data: Option<Mutex<HashMap<String, PrivateWatchData>>>,
    filepath: GoFilepath,
    do_not_cache_entries: bool,
}

/// # Errors
///
/// Returns an error if the configured working directory is not absolute.
pub fn real_fs(options: RealFsOptions) -> Result<RealFs, FsError> {
    let is_windows = cfg!(windows);
    let configured_working_dir = options.abs_working_dir;
    let mut cwd = configured_working_dir.clone();
    if cwd.is_empty() {
        cwd = std::env::current_dir()
            .ok()
            .and_then(|path| path.to_str().map(str::to_owned))
            .unwrap_or_else(|| {
                if is_windows {
                    "C:\\".into()
                } else {
                    "/".into()
                }
            });
    }
    let mut filepath = GoFilepath::new(cwd, is_windows);
    if !filepath.is_abs(&filepath.abs("")) {
        return Err(FsError::new(
            FsErrorKind::InvalidInput,
            "the working directory is not an absolute path",
        ));
    }
    if !configured_working_dir.is_empty() && !filepath.is_abs(&configured_working_dir) {
        return Err(FsError::new(
            FsErrorKind::InvalidInput,
            format!("the working directory {configured_working_dir:?} is not an absolute path"),
        ));
    }
    if let Ok(canonical) = fs::canonicalize(filepath.abs(""))
        && let Some(canonical) = canonical.to_str()
    {
        filepath = GoFilepath::new(canonical, is_windows);
    }
    Ok(RealFs {
        entries: Mutex::new(HashMap::new()),
        watch_data: options.want_watch_data.then(Mutex::default),
        filepath,
        do_not_cache_entries: options.do_not_cache,
    })
}

impl Fs for RealFs {
    fn read_directory(&self, dir: &str) -> ReadDirectoryResult {
        if !self.do_not_cache_entries {
            let cache = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = cache.get(dir) {
                return (
                    cached.entries.clone(),
                    cached.canonical_error.clone(),
                    cached.original_error.clone(),
                );
            }
        }

        let result = fs::read_dir(dir);
        let mut entries = DirEntries::empty(dir);
        let (canonical_error, original_error) = match result {
            Ok(iter) => {
                for directory_entry in iter.flatten() {
                    if let Some(name) = directory_entry.file_name().to_str() {
                        entries.insert(Entry::new(dir.into(), name.into(), EntryKind::None, true));
                    }
                }
                (None, None)
            }
            Err(error) => {
                let original = fs_error(&error);
                (Some(readdir_canonical_error(&error)), Some(original))
            }
        };

        if let Some(watch_data) = &self.watch_data {
            let accessed_entries = Arc::new(Mutex::new(AccessedEntries::default()));
            entries.accessed_entries = Some(Arc::clone(&accessed_entries));
            watch_data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    dir.into(),
                    PrivateWatchData {
                        accessed_entries: Some(accessed_entries),
                        state: if canonical_error.is_some() {
                            WatchState::DirUnreadable
                        } else {
                            WatchState::DirHasAccessedEntries
                        },
                        ..PrivateWatchData::default()
                    },
                );
        }

        if !self.do_not_cache_entries {
            self.entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    dir.into(),
                    EntriesOrError {
                        entries: entries.clone(),
                        canonical_error: canonical_error.clone(),
                        original_error: original_error.clone(),
                    },
                );
        }
        (entries, canonical_error, original_error)
    }

    fn read_file(&self, path: &str) -> ReadFileResult {
        let result = fs::read(path);
        let (contents, canonical_error, original_error) = match result {
            Ok(bytes) => (String::from_utf8_lossy(&bytes).into_owned(), None, None),
            Err(error) => {
                let original = fs_error(&error);
                (
                    String::new(),
                    Some(canonical_file_error(&error)),
                    Some(original),
                )
            }
        };
        if let Some(watch_data) = &self.watch_data {
            let mut watch_data = watch_data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let data = watch_data.entry(path.into()).or_default();
            if canonical_error.is_some() {
                data.state = WatchState::FileMissing;
            } else if matches!(data.state, WatchState::None | WatchState::DirUnreadable) {
                data.state = WatchState::FileNeedModKey;
            }
            data.file_contents.clone_from(&contents);
        }
        (contents, canonical_error, original_error)
    }

    fn open_file(&self, path: &str) -> OpenFileResult {
        match File::open(path) {
            Ok(file) => match file.metadata() {
                Ok(metadata) => (
                    Some(Box::new(RealOpenedFile {
                        file,
                        len: usize::try_from(metadata.len()).unwrap_or(usize::MAX),
                    })),
                    None,
                    None,
                ),
                Err(error) => {
                    let original = fs_error(&error);
                    (None, Some(canonical_file_error(&error)), Some(original))
                }
            },
            Err(error) => {
                let original = fs_error(&error);
                (None, Some(canonical_file_error(&error)), Some(original))
            }
        }
    }

    fn mod_key(&self, path: &str) -> Result<ModKey, FsError> {
        let result = modification_key(path);
        if let Some(watch_data) = &self.watch_data {
            let mut watch_data = watch_data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let existed = watch_data.contains_key(path);
            let data = watch_data.entry(path.into()).or_default();
            if !existed {
                data.state = match &result {
                    Ok(_) => WatchState::FileHasModKey,
                    Err(error) if error.kind == FsErrorKind::InvalidInput => {
                        WatchState::FileUnusableModKey
                    }
                    Err(_) => WatchState::FileMissing,
                };
            } else if data.state == WatchState::FileNeedModKey {
                data.state = WatchState::FileHasModKey;
            }
            if let Ok(key) = &result {
                data.mod_key = *key;
            }
        }
        result
    }

    fn is_abs(&self, path: &str) -> bool {
        self.filepath.is_abs(path)
    }

    fn abs(&self, path: &str) -> Option<String> {
        Some(self.filepath.abs(path))
    }

    fn dir(&self, path: &str) -> String {
        self.filepath.dir(path)
    }

    fn base(&self, path: &str) -> String {
        self.filepath.base(path)
    }

    fn ext(&self, path: &str) -> String {
        self.filepath.ext(path)
    }

    fn join(&self, parts: &[&str]) -> String {
        self.filepath.clean(&self.filepath.join(parts))
    }

    fn cwd(&self) -> &str {
        self.filepath.cwd()
    }

    fn rel(&self, base: &str, target: &str) -> Option<String> {
        self.filepath.rel(base, target).ok()
    }

    fn eval_symlinks(&self, path: &str) -> Option<String> {
        fs::canonicalize(path)
            .ok()
            .and_then(|path| path.to_str().map(str::to_owned))
    }

    fn kind(&self, dir: &str, base: &str) -> (String, EntryKind) {
        let path = self.filepath.join(&[dir, base]);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            return (String::new(), EntryKind::None);
        };
        if metadata.file_type().is_symlink() {
            let Some(link) = self.eval_symlinks(&path) else {
                return (String::new(), EntryKind::None);
            };
            let Ok(target_metadata) = fs::symlink_metadata(&link) else {
                return (String::new(), EntryKind::None);
            };
            let kind = if target_metadata.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            return (link, kind);
        }
        (
            String::new(),
            if metadata.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            },
        )
    }

    fn watch_data(&self) -> WatchData {
        let Some(watch_data) = &self.watch_data else {
            return WatchData::default();
        };
        let snapshots = watch_data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut paths = HashMap::new();
        for (path, mut data) in snapshots {
            if data.state == WatchState::FileNeedModKey {
                match modification_key(&path) {
                    Ok(key) => {
                        data.state = WatchState::FileHasModKey;
                        data.mod_key = key;
                    }
                    Err(error) if error.kind == FsErrorKind::InvalidInput => {
                        data.state = WatchState::FileUnusableModKey;
                    }
                    Err(_) => data.state = WatchState::FileMissing,
                }
            }
            let callback: WatchCallback = match data.state {
                WatchState::DirUnreadable => {
                    let watched = path.clone();
                    Arc::new(move || {
                        if fs::read_dir(&watched).is_ok() {
                            watched.clone()
                        } else {
                            String::new()
                        }
                    })
                }
                WatchState::DirHasAccessedEntries => {
                    let watched = path.clone();
                    let accessed = data.accessed_entries;
                    Arc::new(move || directory_change(&watched, accessed.as_ref()))
                }
                WatchState::FileMissing => {
                    let watched = path.clone();
                    Arc::new(move || {
                        if fs::metadata(&watched).is_ok_and(|metadata| !metadata.is_dir()) {
                            watched.clone()
                        } else {
                            String::new()
                        }
                    })
                }
                WatchState::FileHasModKey => {
                    let watched = path.clone();
                    let old_key = data.mod_key;
                    Arc::new(move || {
                        if modification_key(&watched).ok() == Some(old_key) {
                            String::new()
                        } else {
                            watched.clone()
                        }
                    })
                }
                WatchState::FileUnusableModKey => {
                    let watched = path.clone();
                    let old_contents = data.file_contents;
                    Arc::new(move || {
                        if fs::read_to_string(&watched).ok().as_deref()
                            == Some(old_contents.as_str())
                        {
                            String::new()
                        } else {
                            watched.clone()
                        }
                    })
                }
                WatchState::None | WatchState::FileNeedModKey => continue,
            };
            paths.insert(path, callback);
        }
        WatchData { paths }
    }
}

struct RealOpenedFile {
    file: File,
    len: usize,
}

impl OpenedFile for RealOpenedFile {
    fn len(&self) -> usize {
        self.len
    }

    fn read(&mut self, start: usize, end: usize) -> Result<Vec<u8>, FsError> {
        let size = end
            .checked_sub(start)
            .ok_or_else(|| FsError::new(FsErrorKind::InvalidInput, "invalid read range"))?;
        self.file
            .seek(SeekFrom::Start(start as u64))
            .map_err(|error| fs_error(&error))?;
        let mut bytes = vec![0; size];
        self.file
            .read_exact(&mut bytes)
            .map_err(|error| fs_error(&error))?;
        Ok(bytes)
    }

    fn close(&mut self) -> Result<(), FsError> {
        Ok(())
    }
}

fn directory_change(path: &str, accessed: Option<&Arc<Mutex<AccessedEntries>>>) -> String {
    let Ok(iter) = fs::read_dir(path) else {
        return path.into();
    };
    let mut names: Vec<String> = iter
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .collect();
    let Some(accessed) = accessed else {
        return String::new();
    };
    let accessed = accessed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(all_entries) = &accessed.all_entries {
        names.sort();
        return if names == *all_entries {
            String::new()
        } else {
            path.into()
        };
    }
    let lookup: HashMap<String, String> = names
        .into_iter()
        .map(|name| (name.to_lowercase(), name))
        .collect();
    for (name, was_present) in &accessed.was_present {
        if *was_present != lookup.contains_key(name) {
            return lookup
                .get(name)
                .map_or_else(|| path.into(), |actual| format!("{path}/{actual}"));
        }
    }
    String::new()
}

fn modification_key(path: &str) -> Result<ModKey, FsError> {
    let metadata = fs::metadata(path).map_err(|error| fs_error(&error))?;
    let modified = metadata.modified().map_err(|error| fs_error(&error))?;
    if modified == UNIX_EPOCH
        || modified
            .checked_add(MOD_KEY_SAFETY_GAP)
            .is_some_and(|time| time > SystemTime::now())
    {
        return Err(FsError::new(
            FsErrorKind::InvalidInput,
            "the modification key is unusable",
        ));
    }
    let duration = modified
        .duration_since(UNIX_EPOCH)
        .map_err(|error| FsError::new(FsErrorKind::Other, error.to_string()))?;
    let mut key = ModKey {
        size: i64::try_from(metadata.len()).unwrap_or(i64::MAX),
        mtime_sec: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        mtime_nsec: i64::from(duration.subsec_nanos()),
        ..ModKey::default()
    };
    fill_platform_metadata(&metadata, &mut key);
    Ok(key)
}

#[cfg(unix)]
fn fill_platform_metadata(metadata: &fs::Metadata, key: &mut ModKey) {
    use std::os::unix::fs::MetadataExt;
    key.inode = metadata.ino();
    key.mode = metadata.mode();
    key.uid = metadata.uid();
}

#[cfg(not(unix))]
fn fill_platform_metadata(_metadata: &fs::Metadata, _key: &mut ModKey) {}

fn readdir_canonical_error(error: &std::io::Error) -> FsError {
    fs_error(error)
}

fn canonical_file_error(error: &std::io::Error) -> FsError {
    let mut error = fs_error(error);
    if error.kind == FsErrorKind::NotDirectory {
        error.kind = FsErrorKind::NotFound;
    }
    error
}

fn fs_error(error: &std::io::Error) -> FsError {
    let kind = match error.kind() {
        std::io::ErrorKind::NotFound => FsErrorKind::NotFound,
        std::io::ErrorKind::NotADirectory => FsErrorKind::NotDirectory,
        std::io::ErrorKind::InvalidInput => FsErrorKind::InvalidInput,
        std::io::ErrorKind::PermissionDenied => FsErrorKind::PermissionDenied,
        _ => FsErrorKind::Other,
    };
    FsError::new(kind, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{RealFsOptions, real_fs};
    use crate::internal::fs::{EntryKind, Fs};
    use std::fs;

    #[test]
    fn real_file_system_reads_opens_lists_and_resolves() {
        let root = std::env::temp_dir().join(format!("esbuild-rs-fs-{}", std::process::id()));
        let source = root.join("src");
        fs::create_dir_all(&source).expect("create temp directory");
        fs::write(source.join("index.js"), "let x = 1").expect("write temp file");
        let file_system = real_fs(RealFsOptions {
            abs_working_dir: root.to_string_lossy().into_owned(),
            do_not_cache: true,
            ..RealFsOptions::default()
        })
        .expect("real file system");

        let path = source.join("index.js").to_string_lossy().into_owned();
        assert_eq!(file_system.read_file(&path).0, "let x = 1");
        let mut opened = file_system.open_file(&path).0.expect("open");
        assert_eq!(opened.read(4, 9).expect("range"), b"x = 1");
        let entries = file_system.read_directory(&source.to_string_lossy()).0;
        assert_eq!(
            entries
                .get("index.js")
                .0
                .expect("directory entry")
                .kind(&file_system),
            EntryKind::File
        );
        assert!(file_system.mod_key(&path).is_err());
        fs::remove_dir_all(root).expect("remove temp directory");
    }

    #[test]
    fn watch_data_detects_a_missing_file_appearing() {
        let root = std::env::temp_dir().join(format!("esbuild-rs-watch-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create temp directory");
        let file_system = real_fs(RealFsOptions {
            abs_working_dir: root.to_string_lossy().into_owned(),
            want_watch_data: true,
            ..RealFsOptions::default()
        })
        .expect("real file system");
        let missing = root.join("later.js").to_string_lossy().into_owned();
        assert!(file_system.read_file(&missing).1.is_some());
        let watch = file_system.watch_data();
        fs::write(&missing, "x").expect("create watched file");
        assert_eq!(watch.paths[&missing](), missing);
        fs::remove_dir_all(root).expect("remove temp directory");
    }
}
