// Port of upstream internal/logger.

use crate::internal::helpers::{REPLACEMENT_CHARACTER, decode_wtf8_rune};
use std::any::Any;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

mod msg_ids;

pub use msg_ids::{MsgId, msg_id_to_string, string_to_maximum_msg_id, string_to_msg_ids};

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
    pub line_text: Vec<u8>,
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
    pub contents: Arc<[u8]>,
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

    /// Returns a block comment with common indentation removed.
    ///
    /// # Panics
    ///
    /// Panics if `range` is outside the source.
    #[must_use]
    pub fn comment_text_without_indent(&self, range: Range) -> Vec<u8> {
        let text = self.text_for_range(range);
        if text.len() < 2 || !text.starts_with(b"/*") {
            return text.to_vec();
        }

        let mut prefix_end = range_start(range);
        let mut indent = 0;
        while prefix_end > 0 {
            let (code_point, width) = decode_last_wtf8_rune(&self.contents[..prefix_end]);
            if matches!(code_point, 0x0d | 0x0a | 0x2028 | 0x2029) {
                break;
            }
            prefix_end -= width;
            indent += 1;
        }

        let mut lines: Vec<&[u8]> = Vec::new();
        let mut start = 0;
        let mut index = 0;
        while index < text.len() {
            let (code_point, width) = decode_wtf8(&text[index..]);
            match code_point {
                0x0d | 0x0a => {
                    if start <= index {
                        lines.push(&text[start..index]);
                    }
                    start = index + width;
                    if code_point == 0x0d && text.get(start) == Some(&b'\n') {
                        start += 1;
                        index += 1;
                    }
                }
                0x2028 | 0x2029 => {
                    lines.push(&text[start..index]);
                    start = index + width;
                }
                _ => {}
            }
            index += width;
        }
        lines.push(&text[start..]);

        for line in lines.iter().skip(1) {
            let line_indent = line
                .iter()
                .take_while(|byte| matches!(byte, b' ' | b'\t'))
                .count();
            indent = indent.min(line_indent);
        }

        let mut result = Vec::with_capacity(text.len());
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                result.push(b'\n');
                result.extend_from_slice(&line[indent..]);
            } else {
                result.extend_from_slice(line);
            }
        }
        result
    }
}

#[derive(Clone, Debug, Default)]
pub struct LineColumnTracker {
    contents: Arc<[u8]>,
    pretty_paths: PrettyPaths,
    offset: i32,
    line: i32,
    line_start: i32,
    line_end: i32,
    has_line_start: bool,
    has_line_end: bool,
    has_source: bool,
}

impl LineColumnTracker {
    #[must_use]
    pub fn new(source: Option<&Source>) -> Self {
        source.map_or_else(Self::default, |source| Self {
            contents: Arc::clone(&source.contents),
            pretty_paths: source.pretty_paths.clone(),
            has_line_start: true,
            has_source: true,
            ..Self::default()
        })
    }

    #[must_use]
    pub fn msg_data(&mut self, range: Range, text: impl Into<String>) -> MsgData {
        MsgData {
            text: text.into(),
            location: self.msg_location_or_none(range),
            ..MsgData::default()
        }
    }

    /// # Panics
    ///
    /// Panics if `range` starts outside the source.
    #[must_use]
    pub fn msg_location_or_none(&mut self, range: Range) -> Option<MsgLocation> {
        if !self.has_source {
            return None;
        }
        let offset = usize::try_from(range.loc.start).expect("source locations are non-negative");
        let (line_count, column_count, line_start, line_end) = self.compute_line_and_column(offset);
        Some(MsgLocation {
            file: self.pretty_paths.clone(),
            line: line_count + 1,
            column: column_count,
            length: usize::try_from(range.len).expect("source ranges are non-negative"),
            line_text: self.contents[line_start..line_end].to_vec(),
            ..MsgLocation::default()
        })
    }

    fn scan_to(&mut self, target: i32) {
        let mut index = self.offset;
        if index < target {
            loop {
                let start = usize::try_from(index).expect("source offsets are non-negative");
                let (code_point, width) = decode_wtf8(&self.contents[start..]);
                index += i32::try_from(width).expect("rune width fits in i32");
                match code_point {
                    0x0a => {
                        self.has_line_start = true;
                        self.has_line_end = false;
                        self.line_start = index;
                        let width = i32::try_from(width).expect("rune width fits in i32");
                        if index == width
                            || self.contents[usize::try_from(index - width - 1)
                                .expect("previous byte offset is non-negative")]
                                != b'\r'
                        {
                            self.line += 1;
                        }
                    }
                    0x0d | 0x2028 | 0x2029 => {
                        self.has_line_start = true;
                        self.has_line_end = false;
                        self.line_start = index;
                        self.line += 1;
                    }
                    _ => {}
                }
                if index >= target {
                    self.offset = index;
                    return;
                }
            }
        }

        if index > target {
            loop {
                let end = usize::try_from(index).expect("source offsets are non-negative");
                let (code_point, width) = decode_last_wtf8_rune(&self.contents[..end]);
                index -= i32::try_from(width).expect("rune width fits in i32");
                match code_point {
                    0x0a => {
                        self.has_line_start = false;
                        self.has_line_end = true;
                        self.line_end = index;
                        if index == 0
                            || self.contents[usize::try_from(index - 1)
                                .expect("previous byte offset is non-negative")]
                                != b'\r'
                        {
                            self.line -= 1;
                        }
                    }
                    0x0d | 0x2028 | 0x2029 => {
                        self.has_line_start = false;
                        self.has_line_end = true;
                        self.line_end = index;
                        self.line -= 1;
                    }
                    _ => {}
                }
                if index <= target {
                    self.offset = index;
                    return;
                }
            }
        }
    }

    fn compute_line_and_column(&mut self, offset: usize) -> (usize, usize, usize, usize) {
        self.scan_to(i32::try_from(offset).expect("source must fit in 32 bits"));
        if !self.has_line_start {
            let mut index = usize::try_from(self.offset).expect("source offsets are non-negative");
            while index > 0 {
                let (code_point, width) = decode_last_wtf8_rune(&self.contents[..index]);
                if matches!(code_point, 0x0a | 0x0d | 0x2028 | 0x2029) {
                    break;
                }
                index -= width;
            }
            self.has_line_start = true;
            self.line_start = i32::try_from(index).expect("source must fit in 32 bits");
        }
        if !self.has_line_end {
            let mut index = usize::try_from(self.offset).expect("source offsets are non-negative");
            while index < self.contents.len() {
                let (code_point, width) = decode_wtf8(&self.contents[index..]);
                if matches!(code_point, 0x0a | 0x0d | 0x2028 | 0x2029) {
                    break;
                }
                index += width;
            }
            self.has_line_end = true;
            self.line_end = i32::try_from(index).expect("source must fit in 32 bits");
        }
        (
            usize::try_from(self.line).expect("line count is non-negative"),
            offset - usize::try_from(self.line_start).expect("line start offset is non-negative"),
            usize::try_from(self.line_start).expect("line start offset is non-negative"),
            usize::try_from(self.line_end).expect("line end offset is non-negative"),
        )
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StringInJsTableEntry {
    pub inner_line: i32,
    pub inner_column: i32,
    pub inner_loc: Loc,
    pub outer_loc: Loc,
}

/// Generates a table for remapping JSON locations embedded in JavaScript.
///
/// # Panics
///
/// Panics if the outer string syntax is invalid or either source exceeds the
/// signed 32-bit source-size limit used by esbuild.
#[must_use]
pub fn generate_string_in_js_table(
    outer_contents: &[u8],
    outer_string_literal_loc: Loc,
    inner_contents: &[u8],
) -> Vec<StringInJsTableEntry> {
    let mut table = Vec::new();
    let mut index = 0;
    let mut line = 1;
    let mut column = 0;
    let mut location = Loc {
        start: outer_string_literal_loc.start + 1,
    };

    while index < inner_contents.len() {
        loop {
            let outer_index =
                usize::try_from(location.start).expect("source locations are non-negative");
            let (code_point, _) = decode_wtf8(&outer_contents[outer_index..]);
            if code_point != u32::from(b'\\') {
                break;
            }
            let (escaped, width) = decode_wtf8(&outer_contents[outer_index + 1..]);
            if !matches!(escaped, 0x0a | 0x0d | 0x2028 | 0x2029) {
                break;
            }
            location.start += 1 + i32::try_from(width).expect("rune width fits in i32");
            let after = usize::try_from(location.start).expect("source locations are non-negative");
            if escaped == 0x0d && outer_contents.get(after) == Some(&b'\n') {
                location.start += 1;
            }
        }

        let (code_point, width) = decode_wtf8(&inner_contents[index..]);
        table.push(StringInJsTableEntry {
            inner_line: line,
            inner_column: column,
            inner_loc: Loc {
                start: i32::try_from(index).expect("source must fit in 32 bits"),
            },
            outer_loc: location,
        });
        if table.len() > 1 {
            let previous = table[table.len() - 2];
            if line == previous.inner_line
                && location.start - column == previous.outer_loc.start - previous.inner_column
            {
                table.pop();
            }
        }

        match code_point {
            0x0a | 0x0d | 0x2028 | 0x2029 => {
                line += 1;
                column = 0;
                if code_point == 0x0d && inner_contents.get(index + 1) == Some(&b'\n') {
                    index += 1;
                }
            }
            _ => column += i32::try_from(width).expect("rune width fits in i32"),
        }
        index += width;

        let outer_index =
            usize::try_from(location.start).expect("source locations are non-negative");
        let (outer_code_point, outer_width) = decode_wtf8(&outer_contents[outer_index..]);
        if outer_code_point == 0x0d && outer_contents.get(outer_index + 1) == Some(&b'\n') {
            location.start += 2;
        } else if outer_code_point != u32::from(b'\\') {
            location.start += i32::try_from(outer_width).expect("rune width fits in i32");
        } else {
            let (escaped, escaped_width) = decode_wtf8(&outer_contents[outer_index + 1..]);
            match escaped {
                0x78 => location.start += 3,
                0x75 => {
                    location.start += 1;
                    let escape_index =
                        usize::try_from(location.start).expect("source locations are non-negative");
                    if outer_contents[escape_index] == b'{' {
                        while outer_contents[usize::try_from(location.start)
                            .expect("source locations are non-negative")]
                            != b'}'
                        {
                            location.start += 1;
                        }
                        location.start += 1;
                    } else {
                        location.start += 4;
                    }
                }
                0x0a | 0x0d | 0x2028 | 0x2029 => {}
                _ => {
                    location.start +=
                        1 + i32::try_from(escaped_width).expect("rune width fits in i32");
                }
            }
        }
    }
    table
}

/// # Panics
///
/// Panics if `table` is empty.
#[must_use]
pub fn remap_string_in_js_loc(table: &[StringInJsTableEntry], inner_loc: Loc) -> Loc {
    let mut count = table.len();
    let mut index = 0;
    while count > 0 {
        let step = count / 2;
        let candidate = index + step;
        if candidate + 1 < table.len() && table[candidate + 1].inner_loc.start < inner_loc.start {
            index = candidate + 1;
            count -= step + 1;
            continue;
        }
        count = step;
    }
    let entry = table[index];
    Loc {
        start: entry.outer_loc.start + inner_loc.start - entry.inner_loc.start,
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

fn decode_wtf8(bytes: &[u8]) -> (u32, usize) {
    let (code_point, width) = decode_wtf8_rune(bytes);
    (code_point, width.max(1))
}

fn decode_last_wtf8_rune(bytes: &[u8]) -> (u32, usize) {
    if bytes.is_empty() {
        return (REPLACEMENT_CHARACTER, 0);
    }
    let minimum = bytes.len().saturating_sub(4);
    for start in (minimum..bytes.len()).rev() {
        if start > minimum && bytes[start] & 0xc0 == 0x80 {
            continue;
        }
        let (code_point, width) = decode_wtf8(&bytes[start..]);
        if start + width == bytes.len() {
            return (code_point, width);
        }
    }
    (REPLACEMENT_CHARACTER, 1)
}

#[cfg(test)]
mod tests {
    use super::{
        ImportAttributes, LineColumnTracker, Loc, PathFlags, PathStyle, PrettyPaths, Range, Source,
        generate_string_in_js_table, platform_independent_path_dir_base_ext,
        remap_string_in_js_loc,
    };
    use std::collections::HashMap;
    use std::sync::Arc;

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
            contents: Arc::from(&br#"  "a\"b" 123_abc \077 `x${y}`"#[..]),
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
    fn removes_common_block_comment_indent() {
        let source = Source {
            contents: Arc::from(&b"    /* first\r\n       second\n      third */"[..]),
            ..Source::default()
        };
        let text = source.comment_text_without_indent(Range {
            loc: Loc { start: 4 },
            len: i32::try_from(source.contents.len() - 4).unwrap(),
        });
        assert_eq!(text, b"/* first\n   second\n  third */");
    }

    #[test]
    fn tracks_lines_columns_and_crlf_in_both_directions() {
        let source = Source {
            pretty_paths: PrettyPaths {
                abs: "/a.js".into(),
                rel: "a.js".into(),
            },
            contents: Arc::from(&b"one\r\ntwo\xe2\x80\xa8three\nfour"[..]),
            ..Source::default()
        };
        let mut tracker = LineColumnTracker::new(Some(&source));
        let third = tracker
            .msg_location_or_none(Range {
                loc: Loc { start: 11 },
                len: 2,
            })
            .unwrap();
        assert_eq!((third.line, third.column), (3, 0));
        assert_eq!(third.line_text, b"three");

        let second = tracker
            .msg_location_or_none(Range {
                loc: Loc { start: 5 },
                len: 1,
            })
            .unwrap();
        assert_eq!((second.line, second.column), (2, 0));
        assert_eq!(second.line_text, b"two");
        assert!(
            LineColumnTracker::new(None)
                .msg_location_or_none(Range::default())
                .is_none()
        );
    }

    #[test]
    fn remaps_locations_inside_javascript_strings() {
        let table = generate_string_in_js_table(b"prefix \"abc\" suffix", Loc { start: 7 }, b"abc");
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].outer_loc.start, 8);
        assert_eq!(
            remap_string_in_js_loc(&table, Loc { start: 2 }),
            Loc { start: 10 }
        );
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
