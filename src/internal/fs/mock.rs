use super::{
    DirEntries, Entry, EntryKind, Fs, FsError, ModKey, OpenFileResult, ReadDirectoryResult,
    ReadFileResult, WatchData,
};
use std::collections::HashMap;
use std::hash::BuildHasher;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MockKind {
    #[default]
    Unix,
    Windows,
}

#[derive(Debug)]
pub struct MockFs {
    dirs: HashMap<String, DirEntries>,
    files: HashMap<String, String>,
    abs_working_dir: String,
    default_volume: String,
    kind: MockKind,
}

#[must_use]
pub fn mock_fs<S: BuildHasher>(
    input: &HashMap<String, String, S>,
    kind: MockKind,
    abs_working_dir: impl Into<String>,
) -> MockFs {
    let abs_working_dir = abs_working_dir.into();
    let default_volume = if kind == MockKind::Windows {
        win_to_unix(&abs_working_dir).1
    } else {
        String::new()
    };
    let files = input
        .iter()
        .map(|(path, contents)| (path.clone(), contents.clone()))
        .collect();
    let mut dirs: HashMap<String, DirEntries> = HashMap::new();

    for original_path in input.keys() {
        let (mut path, volume) = if kind == MockKind::Windows {
            win_to_unix(original_path)
        } else {
            (original_path.clone(), String::new())
        };
        let original = path.clone();
        loop {
            let path_dir = slash_dir(&path);
            let key = if kind == MockKind::Windows {
                unix_to_win(&path_dir, &volume, &default_volume)
            } else {
                path_dir.clone()
            };
            let dir = dirs
                .entry(key.clone())
                .or_insert_with(|| DirEntries::empty(key));
            if path_dir == path {
                break;
            }
            let base = slash_base(&path);
            let entry_kind = if path == original {
                EntryKind::File
            } else {
                EntryKind::Dir
            };
            dir.insert(Entry::new(String::new(), base, entry_kind, false));
            path = path_dir;
        }
    }

    MockFs {
        dirs,
        files,
        abs_working_dir,
        default_volume,
        kind,
    }
}

impl Fs for MockFs {
    fn read_directory(&self, path: &str) -> ReadDirectoryResult {
        let mut path = self.normalize_slashes(path);
        let slash = if self.kind == MockKind::Windows {
            '\\'
        } else {
            '/'
        };
        let first_slash = path.find(slash);
        while path.ends_with(slash)
            && path
                .rfind(slash)
                .is_some_and(|last| Some(last) != first_slash)
        {
            path.pop();
        }
        if let Some(entries) = self.dirs.get(&path) {
            return (entries.clone(), None, None);
        }
        let error = FsError::not_found(&path);
        (DirEntries::default(), Some(error.clone()), Some(error))
    }

    fn read_file(&self, path: &str) -> ReadFileResult {
        let path = self.normalize_slashes(path);
        if let Some(contents) = self.files.get(&path) {
            return (contents.clone(), None, None);
        }
        let error = FsError::not_found(&path);
        (String::new(), Some(error.clone()), Some(error))
    }

    fn open_file(&self, path: &str) -> OpenFileResult {
        let path = self.normalize_slashes(path);
        if let Some(contents) = self.files.get(&path) {
            return (
                Some(Box::new(super::InMemoryOpenedFile {
                    contents: contents.as_bytes().to_vec(),
                })),
                None,
                None,
            );
        }
        let error = FsError::not_found(&path);
        (None, Some(error.clone()), Some(error))
    }

    fn mod_key(&self, _path: &str) -> Result<ModKey, FsError> {
        Err(FsError::new(
            super::FsErrorKind::Other,
            "this is not available during tests",
        ))
    }

    fn is_abs(&self, path: &str) -> bool {
        let path = if self.kind == MockKind::Windows {
            win_to_unix(path).0
        } else {
            path.into()
        };
        path.starts_with('/')
    }

    fn abs(&self, path: &str) -> Option<String> {
        let (path, volume) = if self.kind == MockKind::Windows {
            win_to_unix(path)
        } else {
            (path.into(), String::new())
        };
        let path = slash_clean(&format!("/{path}"));
        Some(if self.kind == MockKind::Windows {
            unix_to_win(&path, &volume, &self.default_volume)
        } else {
            path
        })
    }

    fn dir(&self, path: &str) -> String {
        let (path, volume) = if self.kind == MockKind::Windows {
            win_to_unix(path)
        } else {
            (path.into(), String::new())
        };
        let path = slash_dir(&path);
        if self.kind == MockKind::Windows {
            unix_to_win(&path, &volume, &self.default_volume)
        } else {
            path
        }
    }

    fn base(&self, path: &str) -> String {
        let (path, volume) = if self.kind == MockKind::Windows {
            win_to_unix(path)
        } else {
            (path.into(), String::new())
        };
        let mut path = slash_base(&path);
        if self.kind == MockKind::Windows && path == "/" {
            path = format!("{volume}:\\");
        }
        path
    }

    fn ext(&self, path: &str) -> String {
        let path = if self.kind == MockKind::Windows {
            win_to_unix(path).0
        } else {
            path.into()
        };
        slash_ext(&path)
    }

    fn join(&self, parts: &[&str]) -> String {
        let mut volume = String::new();
        let converted: Vec<String> = if self.kind == MockKind::Windows {
            parts
                .iter()
                .enumerate()
                .map(|(index, part)| {
                    let (part, part_volume) = win_to_unix(part);
                    if index == 0 {
                        volume = part_volume;
                    }
                    part
                })
                .collect()
        } else {
            parts.iter().map(|part| (*part).into()).collect()
        };
        let path = slash_join(&converted);
        if self.kind == MockKind::Windows {
            unix_to_win(&path, &volume, &self.default_volume)
        } else {
            path
        }
    }

    fn cwd(&self) -> &str {
        &self.abs_working_dir
    }

    fn rel(&self, base: &str, target: &str) -> Option<String> {
        let (mut base, mut target, volume) = if self.kind == MockKind::Windows {
            let (base, mut base_volume) = win_to_unix(base);
            let (target, mut target_volume) = win_to_unix(target);
            if base_volume.is_empty() {
                base_volume.clone_from(&self.default_volume);
            }
            if target_volume.is_empty() {
                target_volume.clone_from(&self.default_volume);
            }
            if !base_volume.eq_ignore_ascii_case(&target_volume) {
                return None;
            }
            (base, target, base_volume)
        } else {
            (base.into(), target.into(), String::new())
        };
        base = slash_clean(&base);
        target = slash_clean(&target);
        if base == target {
            return Some(".".into());
        }
        if base == "." {
            base.clear();
        }
        if base.starts_with('/') != target.starts_with('/') {
            return None;
        }

        loop {
            let (base_head, base_tail) = split_on_slash(&base);
            let (target_head, target_tail) = split_on_slash(&target);
            if base_head != target_head {
                break;
            }
            base = base_tail.into();
            target = target_tail.into();
        }

        let result = if base.is_empty() {
            target
        } else {
            let parent = "../".repeat(base.matches('/').count() + 1);
            if target.is_empty() {
                parent[..parent.len() - 1].into()
            } else {
                parent + &target
            }
        };
        Some(if self.kind == MockKind::Windows {
            unix_to_win(&result, &volume, &self.default_volume)
        } else {
            result
        })
    }

    fn eval_symlinks(&self, _path: &str) -> Option<String> {
        None
    }

    fn kind(&self, _dir: &str, _base: &str) -> (String, EntryKind) {
        panic!("this should never be called for the mock file system")
    }

    fn watch_data(&self) -> WatchData {
        panic!("this should never be called for the mock file system")
    }
}

impl MockFs {
    fn normalize_slashes(&self, path: &str) -> String {
        if self.kind == MockKind::Windows {
            path.replace('/', "\\")
        } else {
            path.into()
        }
    }
}

fn win_to_unix(path: &str) -> (String, String) {
    let bytes = path.as_bytes();
    let mut volume = String::new();
    let mut path = path;
    if bytes.len() >= 3 && bytes[1] == b':' && bytes[2] == b'\\' && bytes[0].is_ascii_alphabetic() {
        volume.push(char::from(bytes[0]));
        path = &path[2..];
    }
    (path.replace('\\', "/"), volume)
}

fn unix_to_win(path: &str, volume: &str, default_volume: &str) -> String {
    let mut path = path.replace('/', "\\");
    if path.starts_with('\\') {
        let volume = if volume.is_empty() {
            default_volume
        } else {
            volume
        };
        path = format!("{volume}:{path}");
    }
    path
}

fn slash_clean(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let rooted = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            if parts.last().is_some_and(|part| *part != "..") {
                parts.pop();
            } else if !rooted {
                parts.push(part);
            }
        } else {
            parts.push(part);
        }
    }
    if parts.is_empty() {
        return if rooted { "/" } else { "." }.into();
    }
    let result = parts.join("/");
    if rooted { format!("/{result}") } else { result }
}

fn slash_dir(path: &str) -> String {
    let end = path.rfind('/').map_or(0, |index| index + 1);
    slash_clean(&path[..end])
}

fn slash_base(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return "/".into();
    }
    path.rsplit('/').next().unwrap_or(path).into()
}

fn slash_ext(path: &str) -> String {
    let base = slash_base(path);
    base.rfind('.')
        .map_or_else(String::new, |dot| base[dot..].into())
}

fn slash_join(parts: &[String]) -> String {
    let joined = parts
        .iter()
        .filter(|part| !part.is_empty())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("/");
    if joined.is_empty() {
        String::new()
    } else {
        slash_clean(&joined)
    }
}

fn split_on_slash(path: &str) -> (&str, &str) {
    path.find('/')
        .map_or((path, ""), |slash| (&path[..slash], &path[slash + 1..]))
}

#[cfg(test)]
mod tests {
    use super::{MockKind, mock_fs};
    use crate::internal::fs::{EntryKind, Fs};
    use std::collections::HashMap;

    fn files(items: &[(&str, &str)]) -> HashMap<String, String> {
        items
            .iter()
            .map(|(path, contents)| ((*path).into(), (*contents).into()))
            .collect()
    }

    #[test]
    fn basic_unix_file_and_directory_behavior() {
        let fs = mock_fs(
            &files(&[
                ("/README.md", "// README.md"),
                ("/package.json", "// package.json"),
                ("/src/index.js", "// src/index.js"),
                ("/src/util.js", "// src/util.js"),
            ]),
            MockKind::Unix,
            "/",
        );
        assert!(fs.read_file("/missing.txt").1.is_some());
        assert_eq!(fs.read_file("/README.md").0, "// README.md");
        assert_eq!(fs.read_file("/src/index.js").0, "// src/index.js");
        assert!(fs.read_directory("/missing").1.is_some());

        let src = fs.read_directory("/src").0;
        assert_eq!(src.peek_entry_count(), 2);
        assert_eq!(
            src.get("index.js").0.expect("index.js").kind(&fs),
            EntryKind::File
        );
        assert_eq!(
            src.get("util.js").0.expect("util.js").kind(&fs),
            EntryKind::File
        );

        let root = fs.read_directory("/").0;
        assert_eq!(root.peek_entry_count(), 3);
        assert_eq!(root.get("src").0.expect("src").kind(&fs), EntryKind::Dir);
    }

    #[test]
    fn basic_windows_file_and_directory_behavior() {
        let fs = mock_fs(
            &files(&[
                ("C:\\README.md", "// README.md"),
                ("C:\\package.json", "// package.json"),
                ("C:\\src\\index.js", "// src/index.js"),
                ("C:\\src\\util.js", "// src/util.js"),
                ("D:\\other\\file.txt", "// other/file.txt"),
            ]),
            MockKind::Windows,
            "C:\\",
        );
        assert_eq!(fs.read_file("C:\\README.md").0, "// README.md");
        assert_eq!(fs.read_file("C:/src/index.js").0, "// src/index.js");
        assert_eq!(fs.read_file("D:\\other\\file.txt").0, "// other/file.txt");
        assert!(fs.read_file("C:\\other\\file.txt").1.is_some());
        assert_eq!(fs.read_directory("C:\\src\\").0.peek_entry_count(), 2);
        assert_eq!(fs.read_directory("D:\\other").0.peek_entry_count(), 1);
        assert_eq!(fs.read_directory("C:\\").0.peek_entry_count(), 3);
    }

    #[test]
    fn relative_paths_match_upstream_unix_cases() {
        let fs = mock_fs(&HashMap::new(), MockKind::Unix, "/");
        let cases = [
            ("/a/b", "/a/b", "."),
            ("/a/b", "/a/b/c", "c"),
            ("/a/b", "/a/b/c/d", "c/d"),
            ("/a/b/c", "/a/b", ".."),
            ("/a/b/c/d", "/a/b", "../.."),
            ("/a/b/c", "/a/b/x", "../x"),
            ("/a/b/c/d", "/a/b/x", "../../x"),
            ("/a/b/c", "/a/b/x/y", "../x/y"),
            ("/a/b/c/d", "/a/b/x/y", "../../x/y"),
            ("a/b", "a/c", "../c"),
            ("./a/b", "./a/c", "../c"),
            (".", "./a/b", "a/b"),
            (".", ".//a/b", "a/b"),
        ];
        for (base, target, expected) in cases {
            assert_eq!(fs.rel(base, target).as_deref(), Some(expected));
        }
    }

    #[test]
    fn relative_paths_match_upstream_windows_cases() {
        let fs = mock_fs(&HashMap::new(), MockKind::Windows, "C:\\");
        let cases = [
            ("C:\\a\\b", "C:\\a\\b", Some(".")),
            ("C:\\a\\b", "C:\\a\\b\\c", Some("c")),
            ("C:\\a\\b\\c", "C:\\a\\b", Some("..")),
            ("C:\\a\\b\\c\\d", "C:\\a\\b\\x", Some("..\\..\\x")),
            ("a\\b", "a\\c", Some("..\\c")),
            ("C:\\a\\b", "\\a\\b", Some(".")),
            ("\\a", "\\b", Some("..\\b")),
            ("C:\\a", "D:\\a", None),
        ];
        for (base, target, expected) in cases {
            assert_eq!(fs.rel(base, target).as_deref(), expected);
        }
    }
}
