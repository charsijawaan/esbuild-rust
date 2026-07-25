// This is a source-faithful lexical port of the subset of Go's `path/filepath`
// fork used by esbuild. Keeping this implementation local also gives mock and
// WebAssembly file systems stable Windows behavior on non-Windows hosts.

use super::{FsError, FsErrorKind};

const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

#[derive(Clone, Debug)]
pub(crate) struct GoFilepath {
    cwd: String,
    is_windows: bool,
    path_separator: char,
}

impl GoFilepath {
    pub(crate) fn new(cwd: impl Into<String>, is_windows: bool) -> Self {
        Self {
            cwd: cwd.into(),
            is_windows,
            path_separator: if is_windows { '\\' } else { '/' },
        }
    }

    pub(crate) fn is_abs(&self, path: &str) -> bool {
        if !self.is_windows {
            return path.starts_with('/');
        }
        if is_reserved_name(path) {
            return true;
        }
        let volume_len = self.volume_name_len(path);
        volume_len != 0
            && path
                .as_bytes()
                .get(volume_len)
                .is_some_and(|byte| is_slash(*byte))
    }

    pub(crate) fn cwd(&self) -> &str {
        &self.cwd
    }

    pub(crate) fn abs(&self, path: &str) -> String {
        if self.is_abs(path) {
            self.clean(path)
        } else {
            self.join(&[&self.cwd, path])
        }
    }

    pub(crate) fn clean(&self, original_path: &str) -> String {
        let volume_len = self.volume_name_len(original_path);
        let volume = &original_path[..volume_len];
        let path = &original_path[volume_len..];
        if path.is_empty() {
            if volume_len > 1 && original_path.as_bytes().get(1) != Some(&b':') {
                return self.convert_from_slash(original_path);
            }
            return format!("{original_path}.");
        }

        let rooted = path
            .as_bytes()
            .first()
            .is_some_and(|byte| self.is_path_separator(*byte));
        let mut parts: Vec<&str> = Vec::new();
        for part in path.split(|character| self.is_separator_char(character)) {
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

        let separator = self.path_separator.to_string();
        let mut result = String::with_capacity(original_path.len());
        result.push_str(volume);
        if rooted {
            result.push(self.path_separator);
        }
        result.push_str(&parts.join(&separator));
        if result.len() == volume_len {
            result.push('.');
        }
        self.convert_from_slash(&result)
    }

    pub(crate) fn volume_name<'a>(&self, path: &'a str) -> &'a str {
        &path[..self.volume_name_len(path)]
    }

    pub(crate) fn base(&self, path: &str) -> String {
        if path.is_empty() {
            return ".".into();
        }
        let path = path.trim_end_matches(|character| self.is_separator_char(character));
        let path = &path[self.volume_name_len(path)..];
        let base = path
            .rsplit(|character| self.is_separator_char(character))
            .next()
            .unwrap_or(path);
        if base.is_empty() {
            self.path_separator.into()
        } else {
            base.into()
        }
    }

    pub(crate) fn dir(&self, path: &str) -> String {
        let volume = self.volume_name(path);
        let separator = path.char_indices().rev().find_map(|(index, character)| {
            (index >= volume.len() && self.is_separator_char(character)).then_some(index)
        });
        let directory_end = separator.map_or(volume.len(), |index| index + 1);
        let directory = self.clean(&path[volume.len()..directory_end]);
        if directory == "." && volume.len() > 2 {
            return volume.into();
        }
        format!("{volume}{directory}")
    }

    pub(crate) fn ext(&self, path: &str) -> String {
        let final_component = path
            .rsplit(|character| self.is_separator_char(character))
            .next()
            .unwrap_or(path);
        final_component
            .rfind('.')
            .map_or_else(String::new, |dot| final_component[dot..].into())
    }

    pub(crate) fn join(&self, elements: &[&str]) -> String {
        let Some(first) = elements.iter().position(|element| !element.is_empty()) else {
            return String::new();
        };
        let elements = &elements[first..];
        if !self.is_windows {
            return self.clean(&elements.join("/"));
        }
        self.join_non_empty(elements)
    }

    pub(crate) fn rel(&self, base_path: &str, target_path: &str) -> Result<String, FsError> {
        let base_volume = self.volume_name(base_path);
        let target_volume = self.volume_name(target_path);
        let clean_base = self.clean(base_path);
        let clean_target = self.clean(target_path);
        if self.same_word(&clean_target, &clean_base) {
            return Ok(".".into());
        }

        let mut base = &clean_base[base_volume.len()..];
        let target = &clean_target[target_volume.len()..];
        if base == "." {
            base = "";
        }
        let base_slashed = base.starts_with(self.path_separator);
        let target_slashed = target.starts_with(self.path_separator);
        if base_slashed != target_slashed || !self.same_word(base_volume, target_volume) {
            return Err(FsError::new(
                FsErrorKind::InvalidInput,
                format!("Rel: can't make {target_path} relative to {base_path}"),
            ));
        }

        let base_parts: Vec<_> = base
            .split(self.path_separator)
            .filter(|part| !part.is_empty())
            .collect();
        let target_parts: Vec<_> = target
            .split(self.path_separator)
            .filter(|part| !part.is_empty())
            .collect();
        let mut common = 0;
        while common < base_parts.len()
            && common < target_parts.len()
            && self.same_word(base_parts[common], target_parts[common])
        {
            common += 1;
        }
        if base_parts.get(common) == Some(&"..") {
            return Err(FsError::new(
                FsErrorKind::InvalidInput,
                format!("Rel: can't make {target_path} relative to {base_path}"),
            ));
        }

        let mut result = vec![".."; base_parts.len() - common];
        result.extend_from_slice(&target_parts[common..]);
        Ok(if result.is_empty() {
            ".".into()
        } else {
            result.join(&self.path_separator.to_string())
        })
    }

    fn join_non_empty(&self, elements: &[&str]) -> String {
        if elements[0].len() == 2 && elements[0].as_bytes()[1] == b':' {
            let tail = elements[1..]
                .iter()
                .filter(|element| !element.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join("\\");
            return self.clean(&format!("{}{tail}", elements[0]));
        }

        let path = self.clean(&elements.join("\\"));
        if !self.is_unc(&path) {
            return path;
        }
        let head = self.clean(elements[0]);
        if self.is_unc(&head) {
            return path;
        }
        let tail = self.clean(&elements[1..].join("\\"));
        if head.ends_with(self.path_separator) {
            format!("{head}{tail}")
        } else {
            format!("{head}{}{tail}", self.path_separator)
        }
    }

    fn is_unc(&self, path: &str) -> bool {
        self.volume_name_len(path) > 2
    }

    fn same_word(&self, left: &str, right: &str) -> bool {
        if self.is_windows {
            left.eq_ignore_ascii_case(right)
        } else {
            left == right
        }
    }

    fn convert_from_slash(&self, path: &str) -> String {
        if self.is_windows {
            path.replace('/', "\\")
        } else {
            path.into()
        }
    }

    fn is_path_separator(&self, byte: u8) -> bool {
        byte == b'/' || (self.is_windows && byte == b'\\')
    }

    fn is_separator_char(&self, character: char) -> bool {
        character == '/' || (self.is_windows && character == '\\')
    }

    fn volume_name_len(&self, path: &str) -> usize {
        if !self.is_windows {
            return 0;
        }
        let bytes = path.as_bytes();
        if bytes.len() < 2 {
            return 0;
        }
        if bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            return 2;
        }
        if bytes.len() >= 5
            && is_slash(bytes[0])
            && is_slash(bytes[1])
            && !is_slash(bytes[2])
            && bytes[2] != b'.'
        {
            let mut index = 3;
            while index < bytes.len() - 1 {
                if is_slash(bytes[index]) {
                    index += 1;
                    if !is_slash(bytes[index]) {
                        if bytes[index] == b'.' {
                            break;
                        }
                        while index < bytes.len() && !is_slash(bytes[index]) {
                            index += 1;
                        }
                        return index;
                    }
                    break;
                }
                index += 1;
            }
        }
        0
    }
}

fn is_slash(byte: u8) -> bool {
    matches!(byte, b'\\' | b'/')
}

fn is_reserved_name(path: &str) -> bool {
    !path.is_empty()
        && RESERVED_NAMES
            .iter()
            .any(|reserved| path.eq_ignore_ascii_case(reserved))
}

#[cfg(test)]
mod tests {
    use super::GoFilepath;

    #[test]
    fn unix_clean_join_base_dir_ext_and_rel() {
        let filepath = GoFilepath::new("/work", false);
        assert_eq!(filepath.clean("a//b/../c/."), "a/c");
        assert_eq!(filepath.clean("/../../a"), "/a");
        assert_eq!(filepath.abs("src/index.js"), "/work/src/index.js");
        assert_eq!(filepath.join(&["/a", "b", "..", "c"]), "/a/c");
        assert_eq!(filepath.base("/a/b.txt/"), "b.txt");
        assert_eq!(filepath.dir("/a/b.txt"), "/a");
        assert_eq!(filepath.ext("/a/b.test.js"), ".js");
        assert_eq!(filepath.rel("/a/b/c", "/a/d").expect("relative"), "../../d");
    }

    #[test]
    fn windows_volumes_unc_cleaning_and_rel() {
        let filepath = GoFilepath::new("C:\\work", true);
        assert!(filepath.is_abs("C:\\x"));
        assert!(!filepath.is_abs("C:x"));
        assert!(filepath.is_abs("NUL"));
        assert_eq!(filepath.volume_name("C:\\x"), "C:");
        assert_eq!(
            filepath.volume_name("\\\\server\\share\\folder"),
            "\\\\server\\share"
        );
        assert_eq!(filepath.clean("C:\\a\\.\\b\\..\\c"), "C:\\a\\c");
        assert_eq!(filepath.abs("src\\index.js"), "C:\\work\\src\\index.js");
        assert_eq!(
            filepath.rel("C:\\a\\b", "C:\\a\\c").expect("relative"),
            "..\\c"
        );
        assert!(filepath.rel("C:\\a", "D:\\a").is_err());
    }
}
