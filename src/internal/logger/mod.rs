// Port of upstream internal/logger.

use std::any::Any;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

pub type MsgId = u8;

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
#[repr(i8)]
pub enum LogLevel {
    #[default]
    None,
    Verbose,
    Debug,
    Info,
    Warning,
    Error,
    Silent,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum MsgKind {
    Error,
    Warning,
    Info,
    Note,
    Debug,
    Verbose,
}

impl MsgKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warning => "WARNING",
            Self::Info => "INFO",
            Self::Note => "NOTE",
            Self::Debug => "DEBUG",
            Self::Verbose => "VERBOSE",
        }
    }

    #[must_use]
    pub fn icon(self) -> &'static str {
        let windows_command_prompt = cfg!(windows) && std::env::var_os("WT_SESSION").is_none();
        if windows_command_prompt {
            return match self {
                Self::Error => "X",
                Self::Warning => "▲",
                Self::Info => "►",
                Self::Note => "→",
                Self::Debug => "●",
                Self::Verbose => "♦",
            };
        }
        match self {
            Self::Error => "✘",
            Self::Warning => "▲",
            Self::Info => "▶",
            Self::Note => "→",
            Self::Debug => "●",
            Self::Verbose => "⬥",
        }
    }
}

#[derive(Clone)]
pub struct Msg {
    pub notes: Vec<MsgData>,
    pub plugin_name: String,
    pub data: MsgData,
    pub kind: MsgKind,
    pub id: MsgId,
}

#[derive(Clone, Default)]
pub struct MsgData {
    pub user_detail: Option<Arc<dyn Any + Send + Sync>>,
    pub location: Option<MsgLocation>,
    pub text: String,
    pub disable_maximum_width: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MsgLocation {
    pub file: PrettyPaths,
    pub namespace: String,
    pub line_text: String,
    pub suggestion: String,
    pub line: usize,
    pub column: usize,
    pub length: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Loc {
    /// Zero-based byte offset from the start of the file.
    pub start: i32,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Range {
    pub loc: Loc,
    pub len: i32,
}

impl Range {
    #[must_use]
    pub const fn end(self) -> i32 {
        self.loc.start + self.len
    }

    pub fn expand_by(&mut self, other: Self) {
        if self.len == 0 {
            *self = other;
        } else {
            let mut end = self.end().max(other.end());
            if other.loc.start < self.loc.start {
                self.loc.start = other.loc.start;
            }
            if end < self.loc.start {
                end = self.loc.start;
            }
            self.len = end - self.loc.start;
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Span {
    pub text: String,
    pub range: Range,
}

pub fn sort_messages(messages: &mut [Msg]) {
    messages.sort_by(compare_messages);
}

fn compare_messages(left: &Msg, right: &Msg) -> Ordering {
    let (Some(left_location), Some(right_location)) = (&left.data.location, &right.data.location)
    else {
        return match (&left.data.location, &right.data.location) {
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            _ => Ordering::Equal,
        };
    };
    left_location
        .file
        .abs
        .cmp(&right_location.file.abs)
        .then_with(|| left_location.file.rel.cmp(&right_location.file.rel))
        .then_with(|| left_location.line.cmp(&right_location.line))
        .then_with(|| left_location.column.cmp(&right_location.column))
        .then_with(|| left.kind.cmp(&right.kind))
        .then_with(|| left.data.text.cmp(&right.data.text))
}

/// A file-system path or abstract module path.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct Path {
    pub text: String,
    pub namespace: String,
    pub ignored_suffix: String,
    pub import_attributes: ImportAttributes,
    pub flags: PathFlags,
}

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct ImportAttributes {
    packed_data: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportAttribute {
    pub key: String,
    pub value: String,
}

impl ImportAttributes {
    /// # Panics
    ///
    /// Panics only if the private packed representation is corrupted.
    #[must_use]
    pub fn decode_into_array(&self) -> Vec<ImportAttribute> {
        let mut result = Vec::new();
        let mut bytes = self.packed_data.as_slice();
        while !bytes.is_empty() {
            let key_length = read_length(bytes);
            let key = String::from_utf8(bytes[4..4 + key_length].to_vec())
                .expect("import attribute keys are UTF-8");
            bytes = &bytes[4 + key_length..];
            let value_length = read_length(bytes);
            let value = String::from_utf8(bytes[4..4 + value_length].to_vec())
                .expect("import attribute values are UTF-8");
            bytes = &bytes[4 + value_length..];
            result.push(ImportAttribute { key, value });
        }
        result
    }

    #[must_use]
    pub fn decode_into_map(&self) -> HashMap<String, String> {
        self.decode_into_array()
            .into_iter()
            .map(|attribute| (attribute.key, attribute.value))
            .collect()
    }

    #[must_use]
    pub fn encode(value: &HashMap<String, String>) -> Self {
        let mut keys: Vec<&String> = value.keys().collect();
        keys.sort();
        let mut packed_data = Vec::new();
        for key in keys {
            let item_value = &value[key];
            append_length_prefixed(&mut packed_data, key.as_bytes());
            append_length_prefixed(&mut packed_data, item_value.as_bytes());
        }
        Self { packed_data }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.packed_data.is_empty()
    }
}

fn read_length(bytes: &[u8]) -> usize {
    u32::from_le_bytes(bytes[..4].try_into().expect("four-byte length")) as usize
}

fn append_length_prefixed(result: &mut Vec<u8>, bytes: &[u8]) {
    let length = u32::try_from(bytes.len()).expect("import attribute must fit in 32 bits");
    result.extend_from_slice(&length.to_le_bytes());
    result.extend_from_slice(bytes);
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct PathFlags(u8);

impl PathFlags {
    pub const DISABLED: Self = Self(1);

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

impl Path {
    #[must_use]
    pub const fn is_disabled(&self) -> bool {
        self.flags.contains(PathFlags::DISABLED)
    }
}

/// Splits a path consistently across Unix and Windows hosts.
#[must_use]
#[allow(clippy::almost_complete_range, clippy::manual_is_ascii_check)]
pub fn platform_independent_path_dir_base_ext(path: &str) -> (String, String, String) {
    let mut path = path;
    let bytes = path.as_bytes();
    let absolute_root_slash = if bytes
        .first()
        .is_some_and(|byte| matches!(byte, b'/' | b'\\'))
    {
        Some(0)
    } else if bytes.len() > 2
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
        && ((bytes[0] >= b'a' && bytes[0] < b'z') || (bytes[0] >= b'A' && bytes[0] <= b'Z'))
    {
        Some(2)
    } else {
        None
    };

    let (directory, mut base) = loop {
        let Some(index) = path.rfind(['/', '\\']) else {
            break (String::new(), path.to_string());
        };
        if Some(index) == absolute_root_slash {
            break (path[..=index].to_string(), path[index + 1..].to_string());
        }
        if index + 1 != path.len() {
            break (path[..index].to_string(), path[index + 1..].to_string());
        }
        path = &path[..index];
    };

    let mut extension = String::new();
    if let Some(mut dot) = base.rfind('.') {
        extension = base[dot..].to_string();
        if extension == ".css"
            && let Some(second_dot) = base[..dot].rfind('.')
            && &base[second_dot..] == ".module.css"
        {
            dot = second_dot;
            extension = base[dot..].to_string();
        }
        base.truncate(dot);
    }
    (directory, base, extension)
}

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct PrettyPaths {
    pub abs: String,
    pub rel: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PathStyle {
    #[default]
    Relative,
    Absolute,
}

impl PrettyPaths {
    #[must_use]
    pub fn select(&self, style: PathStyle) -> &str {
        match style {
            PathStyle::Relative => &self.rel,
            PathStyle::Absolute => &self.abs,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Source {
    pub pretty_paths: PrettyPaths,
    pub identifier_name: String,
    /// Raw source bytes. This intentionally supports invalid UTF-8 like Go strings.
    pub contents: Vec<u8>,
    pub key_path: Path,
    pub index: u32,
}

impl Source {
    /// # Panics
    ///
    /// Panics if the range is outside the source.
    #[must_use]
    pub fn text_for_range(&self, range: Range) -> &[u8] {
        &self.contents[range_start(range)..range_end(range)]
    }

    #[must_use]
    /// # Panics
    ///
    /// Panics if `location` is negative or outside the source.
    pub fn loc_before_whitespace(&self, mut location: Loc) -> Loc {
        while location.start > 0 {
            let index = usize::try_from(location.start).expect("source locations are non-negative");
            if !matches!(self.contents[index - 1], b' ' | b'\t' | b'\r' | b'\n') {
                break;
            }
            location.start -= 1;
        }
        location
    }

    /// # Panics
    ///
    /// Panics if `location` is outside the source.
    #[must_use]
    pub fn range_of_operator_before(&self, location: Loc, operator: &[u8]) -> Range {
        let start = usize::try_from(location.start).expect("source locations are non-negative");
        find_last_bytes(&self.contents[..start], operator).map_or(
            Range {
                loc: location,
                len: 0,
            },
            |index| Range {
                loc: Loc {
                    start: i32::try_from(index).expect("source must fit in 32 bits"),
                },
                len: i32::try_from(operator.len()).expect("operator must fit in 32 bits"),
            },
        )
    }

    /// # Panics
    ///
    /// Panics if `location` is outside the source.
    #[must_use]
    pub fn range_of_operator_after(&self, location: Loc, operator: &[u8]) -> Range {
        let start = usize::try_from(location.start).expect("source locations are non-negative");
        find_bytes(&self.contents[start..], operator).map_or(
            Range {
                loc: location,
                len: 0,
            },
            |index| Range {
                loc: Loc {
                    start: location.start
                        + i32::try_from(index).expect("source must fit in 32 bits"),
                },
                len: i32::try_from(operator.len()).expect("operator must fit in 32 bits"),
            },
        )
    }

    /// # Panics
    ///
    /// Panics if `location` is outside the source.
    #[must_use]
    pub fn range_of_string(&self, location: Loc) -> Range {
        let text = &self.contents
            [usize::try_from(location.start).expect("source locations are non-negative")..];
        let Some(&quote) = text.first() else {
            return Range {
                loc: location,
                len: 0,
            };
        };
        if matches!(quote, b'"' | b'\'') {
            let mut index = 1;
            while index < text.len() {
                if text[index] == quote {
                    return range_with_len(location, index + 1);
                }
                if text[index] == b'\\' {
                    index += 1;
                }
                index += 1;
            }
        }
        if quote == b'`' {
            let mut index = 1;
            while index < text.len() {
                if text[index] == quote {
                    return range_with_len(location, index + 1);
                }
                if text[index] == b'\\' {
                    index += 1;
                } else if text[index] == b'$' && index + 1 < text.len() && text[index + 1] == b'{' {
                    break;
                }
                index += 1;
            }
        }
        Range {
            loc: location,
            len: 0,
        }
    }

    /// # Panics
    ///
    /// Panics if `location` is outside the source.
    #[must_use]
    pub fn range_of_number(&self, location: Loc) -> Range {
        let text = &self.contents
            [usize::try_from(location.start).expect("source locations are non-negative")..];
        let mut length = usize::from(text.first().is_some_and(u8::is_ascii_digit));
        while length < text.len() {
            let byte = text[length];
            if !byte.is_ascii_alphanumeric() && !matches!(byte, b'.' | b'_') {
                break;
            }
            length += 1;
        }
        range_with_len(location, length)
    }

    /// # Panics
    ///
    /// Panics if `location` is outside the source.
    #[must_use]
    pub fn range_of_legacy_octal_escape(&self, location: Loc) -> Range {
        let text = &self.contents
            [usize::try_from(location.start).expect("source locations are non-negative")..];
        let mut length = usize::from(text.len() >= 2 && text[0] == b'\\') * 2;
        while length < 4 && length < text.len() && text[length].is_ascii_digit() {
            length += 1;
        }
        range_with_len(location, length)
    }
}

fn range_start(range: Range) -> usize {
    usize::try_from(range.loc.start).expect("source locations are non-negative")
}

fn range_end(range: Range) -> usize {
    usize::try_from(range.end()).expect("source ranges are non-negative")
}

fn range_with_len(location: Loc, length: usize) -> Range {
    Range {
        loc: location,
        len: i32::try_from(length).expect("source must fit in 32 bits"),
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|item| item == needle)
}

fn find_last_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(haystack.len());
    }
    haystack
        .windows(needle.len())
        .rposition(|item| item == needle)
}

#[cfg(test)]
mod tests {
    use super::{
        ImportAttributes, Loc, PathFlags, PathStyle, PrettyPaths, Range, Source,
        platform_independent_path_dir_base_ext,
    };
    use std::collections::HashMap;

    #[test]
    fn range_expansion_matches_upstream() {
        let mut range = Range {
            loc: Loc { start: 10 },
            len: 5,
        };
        range.expand_by(Range {
            loc: Loc { start: 5 },
            len: 20,
        });
        assert_eq!(range.loc.start, 5);
        assert_eq!(range.len, 20);
    }

    #[test]
    fn import_attributes_are_sorted_and_round_trip() {
        let values = HashMap::from([
            ("type".to_string(), "json".to_string()),
            ("mode".to_string(), "strict".to_string()),
        ]);
        let encoded = ImportAttributes::encode(&values);
        let decoded = encoded.decode_into_array();
        assert_eq!(decoded[0].key, "mode");
        assert_eq!(decoded[1].key, "type");
        assert_eq!(encoded.decode_into_map(), values);
    }

    #[test]
    fn splits_paths_platform_independently() {
        assert_eq!(
            platform_independent_path_dir_base_ext("/a/b/file.module.css"),
            (
                "/a/b".to_string(),
                "file".to_string(),
                ".module.css".to_string()
            )
        );
        assert_eq!(
            platform_independent_path_dir_base_ext(r"C:\a\b\file.js"),
            ("C:\\a\\b".into(), "file".into(), ".js".into())
        );
        assert_eq!(
            platform_independent_path_dir_base_ext("/"),
            ("/".into(), String::new(), String::new())
        );
    }

    #[test]
    fn source_range_helpers_scan_raw_bytes() {
        let source = Source {
            contents: br#"  "a\"b" 123_abc \077 `x${y}`"#.to_vec(),
            ..Source::default()
        };
        assert_eq!(source.loc_before_whitespace(Loc { start: 2 }).start, 0);
        assert_eq!(source.range_of_string(Loc { start: 2 }).len, 6);
        assert_eq!(source.range_of_number(Loc { start: 9 }).len, 7);
        assert_eq!(
            source.range_of_legacy_octal_escape(Loc { start: 17 }).len,
            4
        );
        assert_eq!(source.range_of_string(Loc { start: 22 }).len, 0);
    }

    #[test]
    fn pretty_paths_and_flags_select_expected_values() {
        let paths = PrettyPaths {
            abs: "/abs/file.js".into(),
            rel: "file.js".into(),
        };
        assert_eq!(paths.select(PathStyle::Relative), "file.js");
        assert_eq!(paths.select(PathStyle::Absolute), "/abs/file.js");
        assert!(PathFlags::DISABLED.contains(PathFlags::DISABLED));
    }
}
