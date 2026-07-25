use super::{
    DirEntries, Entry, EntryKind, Fs, FsError, FsErrorKind, ModKey, OpenFileResult,
    ReadDirectoryResult, ReadFileResult, WatchData,
};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::sync::{Arc, Mutex};

pub struct ZipFs {
    inner: Box<dyn Fs>,
    zip_files: Mutex<HashMap<String, Arc<CachedZip>>>,
}

enum CachedZip {
    Archive(Mutex<ZipArchive>),
    Error,
}

struct ZipArchive {
    reader: zip::ZipArchive<File>,
    dirs: HashMap<String, CompressedDir>,
    files: HashMap<String, CompressedFile>,
}

#[derive(Default)]
struct CompressedDir {
    entries: HashMap<String, EntryKind>,
}

struct CompressedFile {
    index: usize,
    contents: Option<Result<String, FsError>>,
}

impl ZipFs {
    #[must_use]
    pub fn new(inner: Box<dyn Fs>) -> Self {
        Self {
            inner,
            zip_files: Mutex::new(HashMap::new()),
        }
    }

    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    fn check_for_zip(&self, path: &str, kind: EntryKind) -> Option<(Arc<CachedZip>, String)> {
        let path = path.replace('\\', "/");
        let (zip_path, path_tail) = if let Some(index) = path.find(".zip/") {
            (
                path[..index + ".zip".len()].to_owned(),
                path[index + ".zip/".len()..].to_owned(),
            )
        } else if kind == EntryKind::Dir && path.ends_with(".zip") {
            (path, String::new())
        } else {
            return None;
        };

        let cached = {
            let mut zip_files = self
                .zip_files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(
                zip_files
                    .entry(zip_path.clone())
                    .or_insert_with(|| Arc::new(load_zip_archive(&zip_path))),
            )
        };
        matches!(&*cached, CachedZip::Archive(_)).then_some((cached, path_tail))
    }
}

impl Fs for ZipFs {
    fn read_directory(&self, path: &str) -> ReadDirectoryResult {
        let path = mangle_yarn_pnp_virtual_path(path);
        let result = self.inner.read_directory(&path);
        let can_try_zip = result.1.as_ref().is_some_and(|error| {
            matches!(
                error.kind,
                FsErrorKind::NotFound | FsErrorKind::NotDirectory | FsErrorKind::InvalidInput
            )
        });
        if !can_try_zip {
            return result;
        }
        let Some((archive, path_tail)) = self.check_for_zip(&path, EntryKind::Dir) else {
            return result;
        };
        let CachedZip::Archive(archive) = &*archive else {
            return result;
        };
        let archive = archive
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(directory) = archive.dirs.get(&path_tail.to_lowercase()) else {
            return missing_directory(&path);
        };
        let mut entries = DirEntries::empty(&path);
        for (name, kind) in &directory.entries {
            entries.insert(Entry::new(path.clone(), name.clone(), *kind, false));
        }
        (entries, None, None)
    }

    fn read_file(&self, path: &str) -> ReadFileResult {
        let path = mangle_yarn_pnp_virtual_path(path);
        let result = self.inner.read_file(&path);
        if result.1.as_ref().map(|error| error.kind) != Some(FsErrorKind::NotFound) {
            return result;
        }
        let Some((archive, path_tail)) = self.check_for_zip(&path, EntryKind::File) else {
            return result;
        };
        let CachedZip::Archive(archive) = &*archive else {
            return result;
        };
        let mut archive = archive
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = path_tail.to_lowercase();
        let Some(index) = archive.files.get(&key).map(|file| file.index) else {
            return missing_file(&path);
        };
        if let Some(cached) = archive
            .files
            .get(&key)
            .and_then(|file| file.contents.clone())
        {
            return string_result(cached);
        }

        let contents = (|| {
            let mut file = archive
                .reader
                .by_index(index)
                .map_err(|error| zip_error(&error))?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|error| FsError::new(FsErrorKind::Other, error.to_string()))?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        })();
        archive
            .files
            .get_mut(&key)
            .expect("indexed ZIP file disappeared")
            .contents = Some(contents.clone());
        string_result(contents)
    }

    fn open_file(&self, path: &str) -> OpenFileResult {
        self.inner.open_file(&mangle_yarn_pnp_virtual_path(path))
    }

    fn mod_key(&self, path: &str) -> Result<ModKey, FsError> {
        self.inner.mod_key(&mangle_yarn_pnp_virtual_path(path))
    }

    fn is_abs(&self, path: &str) -> bool {
        self.inner.is_abs(path)
    }

    fn abs(&self, path: &str) -> Option<String> {
        self.inner.abs(path)
    }

    fn dir(&self, path: &str) -> String {
        if let Some((prefix, suffix)) = parse_yarn_pnp_virtual_path(path)
            && suffix.is_empty()
        {
            return prefix;
        }
        self.inner.dir(path)
    }

    fn base(&self, path: &str) -> String {
        self.inner.base(path)
    }

    fn ext(&self, path: &str) -> String {
        self.inner.ext(path)
    }

    fn join(&self, parts: &[&str]) -> String {
        self.inner.join(parts)
    }

    fn cwd(&self) -> &str {
        self.inner.cwd()
    }

    fn rel(&self, base: &str, target: &str) -> Option<String> {
        self.inner.rel(base, target)
    }

    fn eval_symlinks(&self, path: &str) -> Option<String> {
        self.inner.eval_symlinks(path)
    }

    fn kind(&self, dir: &str, base: &str) -> (String, EntryKind) {
        self.inner.kind(dir, base)
    }

    fn watch_data(&self) -> WatchData {
        self.inner.watch_data()
    }
}

fn load_zip_archive(path: &str) -> CachedZip {
    let Ok(file) = File::open(path) else {
        return CachedZip::Error;
    };
    let Ok(mut reader) = zip::ZipArchive::new(file) else {
        return CachedZip::Error;
    };
    let mut dirs: HashMap<String, CompressedDir> = HashMap::new();
    let mut files = HashMap::new();
    let mut seeds = Vec::new();

    for index in 0..reader.len() {
        let Ok(file) = reader.by_index_raw(index) else {
            return CachedZip::Error;
        };
        let file_name = file.name().to_owned();
        let trimmed = file_name.trim_end_matches('/');
        let (dir_path, base_name) = trimmed.rfind('/').map_or(("", trimmed), |slash| {
            (&trimmed[..slash], &trimmed[slash + 1..])
        });
        let lower_dir = dir_path.to_lowercase();
        if !dirs.contains_key(&lower_dir) {
            dirs.insert(lower_dir.clone(), CompressedDir::default());
            seeds.push(lower_dir.clone());
        }
        if file.is_dir() {
            continue;
        }
        files.insert(
            file_name.to_lowercase(),
            CompressedFile {
                index,
                contents: None,
            },
        );
        dirs.get_mut(&lower_dir)
            .expect("ZIP directory was just inserted")
            .entries
            .insert(base_name.into(), EntryKind::File);
    }

    for mut base_name in seeds {
        while !base_name.is_empty() {
            let (dir_path, child) = base_name
                .rfind('/')
                .map_or(("", base_name.as_str()), |slash| {
                    (&base_name[..slash], &base_name[slash + 1..])
                });
            let lower_dir = dir_path.to_lowercase();
            dirs.entry(lower_dir.clone())
                .or_default()
                .entries
                .insert(child.into(), EntryKind::Dir);
            base_name = lower_dir;
        }
    }

    CachedZip::Archive(Mutex::new(ZipArchive {
        reader,
        dirs,
        files,
    }))
}

#[must_use]
pub fn parse_yarn_pnp_virtual_path(path: &str) -> Option<(String, String)> {
    let mut index = 0;
    while index < path.len() {
        let start = index;
        let slash = path[index..].find(['/', '\\'])?;
        index += slash + 1;
        let segment = &path[start..index - 1];
        if !matches!(segment, "__virtual__" | "$$virtual") {
            continue;
        }
        let hash_slash = path[index..].find(['/', '\\'])?;
        let count_start = index + hash_slash + 1;
        let (count, suffix) = if let Some(count_slash) = path[count_start..].find(['/', '\\']) {
            (
                &path[count_start..count_start + count_slash],
                &path[count_start + count_slash..],
            )
        } else {
            (&path[count_start..], "")
        };
        let Ok(mut count) = count.parse::<u64>() else {
            continue;
        };
        let mut prefix = path[..start].to_owned();
        while count > 0 && (prefix.ends_with('/') || prefix.ends_with('\\')) {
            let Some(slash) = prefix[..prefix.len() - 1].rfind(['/', '\\']) else {
                break;
            };
            prefix.truncate(slash + 1);
            count -= 1;
        }
        let mut suffix = suffix.to_owned();
        let first_slash = prefix.find(['/', '\\']);
        let last_slash = prefix.rfind(['/', '\\']);
        if suffix.is_empty() && first_slash != last_slash {
            prefix.pop();
        } else if prefix.is_empty() {
            prefix.push('.');
        } else if suffix.starts_with(['/', '\\']) {
            suffix.remove(0);
        }
        return Some((prefix, suffix));
    }
    None
}

#[must_use]
pub fn mangle_yarn_pnp_virtual_path(path: &str) -> String {
    parse_yarn_pnp_virtual_path(path)
        .map_or_else(|| path.into(), |(prefix, suffix)| prefix + &suffix)
}

fn string_result(result: Result<String, FsError>) -> ReadFileResult {
    match result {
        Ok(contents) => (contents, None, None),
        Err(error) => (String::new(), Some(error.clone()), Some(error)),
    }
}

fn missing_file(path: &str) -> ReadFileResult {
    let error = FsError::not_found(path);
    (String::new(), Some(error.clone()), Some(error))
}

fn missing_directory(path: &str) -> ReadDirectoryResult {
    let error = FsError::not_found(path);
    (DirEntries::default(), Some(error.clone()), Some(error))
}

fn zip_error(error: &zip::result::ZipError) -> FsError {
    FsError::new(FsErrorKind::Other, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{ZipFs, mangle_yarn_pnp_virtual_path, parse_yarn_pnp_virtual_path};
    use crate::internal::fs::{EntryKind, Fs, RealFsOptions, real_fs_without_zip};
    use std::fs::{self, File};
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    #[test]
    fn parses_and_mangles_yarn_virtual_paths() {
        assert_eq!(
            parse_yarn_pnp_virtual_path("/project/.yarn/__virtual__/pkg/1/foo.js"),
            Some(("/project/".into(), "foo.js".into()))
        );
        assert_eq!(
            parse_yarn_pnp_virtual_path("/a/b/$$virtual/hash/2/c"),
            Some(("/".into(), "c".into()))
        );
        assert_eq!(
            mangle_yarn_pnp_virtual_path("/project/.yarn/__virtual__/pkg/1/foo.js"),
            "/project/foo.js"
        );
        assert_eq!(parse_yarn_pnp_virtual_path("/ordinary/path"), None);
    }

    #[test]
    fn reads_files_and_directories_inside_zip_archives() {
        let root = std::env::temp_dir().join(format!("esbuild-rs-zip-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create temp directory");
        let zip_path = root.join("package.zip");
        let file = File::create(&zip_path).expect("create zip");
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file("pkg/index.js", SimpleFileOptions::default())
            .expect("start zip entry");
        writer.write_all(b"export default 1").expect("write zip");
        writer.finish().expect("finish zip");

        let inner = real_fs_without_zip(RealFsOptions {
            abs_working_dir: root.to_string_lossy().into_owned(),
            do_not_cache: true,
            ..RealFsOptions::default()
        })
        .expect("real file system");
        let file_system = ZipFs::new(Box::new(inner));
        let virtual_file = format!("{}/pkg/index.js", zip_path.to_string_lossy());
        assert_eq!(file_system.read_file(&virtual_file).0, "export default 1");
        let virtual_dir = format!("{}/pkg", zip_path.to_string_lossy());
        let entries = file_system.read_directory(&virtual_dir).0;
        assert_eq!(
            entries
                .get("index.js")
                .0
                .expect("zip entry")
                .kind(&file_system),
            EntryKind::File
        );
        fs::remove_dir_all(root).expect("remove temp directory");
    }
}
