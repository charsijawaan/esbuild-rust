// Port of upstream internal/logger.

use crate::internal::helpers::{REPLACEMENT_CHARACTER, decode_wtf8_rune};
use std::any::Any;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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

impl Msg {
    #[must_use]
    pub fn new(kind: MsgKind, text: impl Into<String>) -> Self {
        Self {
            notes: Vec::new(),
            plugin_name: String::new(),
            data: MsgData {
                text: text.into(),
                ..MsgData::default()
            },
            kind,
            id: MsgId::None,
        }
    }
}

#[derive(Clone, Default)]
pub struct MsgData {
    pub user_detail: Option<Arc<dyn Any + Send + Sync>>,
    pub location: Option<MsgLocation>,
    pub text: String,
    pub disable_maximum_width: bool,
}

#[derive(Clone)]
pub struct Log {
    add_msg_callback: Arc<dyn Fn(Msg) + Send + Sync>,
    has_errors_callback: Arc<dyn Fn() -> bool + Send + Sync>,
    peek_callback: Arc<dyn Fn() -> Vec<Msg> + Send + Sync>,
    done_callback: Arc<dyn Fn() -> Vec<Msg> + Send + Sync>,
    pub level: LogLevel,
    pub overrides: Arc<HashMap<MsgId, LogLevel>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DeferLogKind {
    #[default]
    All,
    NoVerboseOrDebug,
}

#[derive(Default)]
struct DeferLogState {
    messages: Vec<Msg>,
    has_errors: bool,
}

impl Log {
    /// # Panics
    ///
    /// Operations on the returned log panic if its private mutex is poisoned.
    #[must_use]
    pub fn new_defer(kind: DeferLogKind, overrides: HashMap<MsgId, LogLevel>) -> Self {
        let state = Arc::new(Mutex::new(DeferLogState::default()));
        let add_state = Arc::clone(&state);
        let has_errors_state = Arc::clone(&state);
        let peek_state = Arc::clone(&state);
        let done_state = Arc::clone(&state);
        Self {
            level: LogLevel::Info,
            overrides: Arc::new(overrides),
            add_msg_callback: Arc::new(move |message| {
                if kind == DeferLogKind::NoVerboseOrDebug
                    && matches!(message.kind, MsgKind::Verbose | MsgKind::Debug)
                {
                    return;
                }
                let mut state = add_state.lock().expect("deferred log mutex was poisoned");
                if message.kind == MsgKind::Error {
                    state.has_errors = true;
                }
                state.messages.push(message);
            }),
            has_errors_callback: Arc::new(move || {
                has_errors_state
                    .lock()
                    .expect("deferred log mutex was poisoned")
                    .has_errors
            }),
            peek_callback: Arc::new(move || {
                peek_state
                    .lock()
                    .expect("deferred log mutex was poisoned")
                    .messages
                    .clone()
            }),
            done_callback: Arc::new(move || {
                let mut state = done_state.lock().expect("deferred log mutex was poisoned");
                sort_messages(&mut state.messages);
                state.messages.clone()
            }),
        }
    }

    pub fn add_msg(&self, message: Msg) {
        (self.add_msg_callback)(message);
    }

    #[must_use]
    pub fn has_errors(&self) -> bool {
        (self.has_errors_callback)()
    }

    #[must_use]
    pub fn peek(&self) -> Vec<Msg> {
        (self.peek_callback)()
    }

    #[must_use]
    pub fn done(&self) -> Vec<Msg> {
        (self.done_callback)()
    }

    pub fn add_error(
        &self,
        tracker: Option<&mut LineColumnTracker>,
        range: Range,
        text: impl Into<String>,
    ) {
        self.add_msg(Msg {
            data: tracked_msg_data(tracker, range, text),
            ..Msg::new(MsgKind::Error, "")
        });
    }

    pub fn add_id(
        &self,
        id: MsgId,
        kind: MsgKind,
        tracker: Option<&mut LineColumnTracker>,
        range: Range,
        text: impl Into<String>,
    ) {
        if let Some(overridden_kind) = allow_override(&self.overrides, id, kind) {
            self.add_msg(Msg {
                id,
                kind: overridden_kind,
                data: tracked_msg_data(tracker, range, text),
                ..Msg::new(overridden_kind, "")
            });
        }
    }

    pub fn add_error_with_notes(
        &self,
        tracker: Option<&mut LineColumnTracker>,
        range: Range,
        text: impl Into<String>,
        notes: Vec<MsgData>,
    ) {
        self.add_msg(Msg {
            notes,
            data: tracked_msg_data(tracker, range, text),
            ..Msg::new(MsgKind::Error, "")
        });
    }

    pub fn add_id_with_notes(
        &self,
        id: MsgId,
        kind: MsgKind,
        tracker: Option<&mut LineColumnTracker>,
        range: Range,
        text: impl Into<String>,
        notes: Vec<MsgData>,
    ) {
        if let Some(overridden_kind) = allow_override(&self.overrides, id, kind) {
            self.add_msg(Msg {
                notes,
                id,
                kind: overridden_kind,
                data: tracked_msg_data(tracker, range, text),
                ..Msg::new(overridden_kind, "")
            });
        }
    }

    pub fn add_msg_id(&self, id: MsgId, mut message: Msg) {
        if let Some(overridden_kind) = allow_override(&self.overrides, id, message.kind) {
            message.id = id;
            message.kind = overridden_kind;
            self.add_msg(message);
        }
    }
}

fn tracked_msg_data(
    tracker: Option<&mut LineColumnTracker>,
    range: Range,
    text: impl Into<String>,
) -> MsgData {
    let text = text.into();
    match tracker {
        Some(tracker) => tracker.msg_data(range, text),
        None => MsgData {
            text,
            ..MsgData::default()
        },
    }
}

fn allow_override(
    overrides: &HashMap<MsgId, LogLevel>,
    id: MsgId,
    kind: MsgKind,
) -> Option<MsgKind> {
    overrides.get(&id).map_or(Some(kind), |level| match level {
        LogLevel::Verbose => Some(MsgKind::Verbose),
        LogLevel::Debug => Some(MsgKind::Debug),
        LogLevel::Info => Some(MsgKind::Info),
        LogLevel::Warning => Some(MsgKind::Warning),
        LogLevel::Error => Some(MsgKind::Error),
        LogLevel::None | LogLevel::Silent => None,
    })
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

/// Wraps a log so locations from JSON embedded in a JavaScript string are
/// remapped back into the outer JavaScript source.
///
/// # Panics
///
/// Adding a message with a location panics if `table` is empty, if a remapped
/// range is invalid, or if the private tracker mutex is poisoned.
#[must_use]
pub fn new_string_in_js_log(
    mut log: Log,
    outer_tracker: &LineColumnTracker,
    table: Vec<StringInJsTableEntry>,
) -> Log {
    let old_add_msg = Arc::clone(&log.add_msg_callback);
    let outer_tracker = Arc::new(Mutex::new(outer_tracker.clone()));

    log.add_msg_callback = Arc::new(move |mut message| {
        fn remap_line_and_column_to_loc(
            table: &[StringInJsTableEntry],
            line: i32,
            column: i32,
        ) -> Loc {
            let mut count = table.len();
            let mut index = 0;
            while count > 0 {
                let step = count / 2;
                let candidate = index + step;
                if candidate + 1 < table.len() {
                    let entry = table[candidate + 1];
                    if entry.inner_line < line
                        || (entry.inner_line == line && entry.inner_column < column)
                    {
                        index = candidate + 1;
                        count -= step + 1;
                        continue;
                    }
                }
                count = step;
            }
            let entry = table[index];
            Loc {
                start: entry.outer_loc.start + column - entry.inner_column,
            }
        }

        fn remap_data(
            table: &[StringInJsTableEntry],
            tracker: &mut LineColumnTracker,
            mut data: MsgData,
        ) -> MsgData {
            let Some(inner_location) = data.location.as_ref() else {
                return data;
            };
            let line = i32::try_from(inner_location.line).expect("line number must fit in i32");
            let column =
                i32::try_from(inner_location.column).expect("column number must fit in i32");
            let start = remap_line_and_column_to_loc(table, line, column);
            let mut range = Range { loc: start, len: 0 };
            if inner_location.length != 0 {
                let end_column = i32::try_from(inner_location.column + inner_location.length)
                    .expect("column number must fit in i32");
                range.len =
                    remap_line_and_column_to_loc(table, line, end_column).start - start.start;
            }
            let suggestion = inner_location.suggestion.clone();
            let mut location = tracker
                .msg_data(range, data.text.clone())
                .location
                .expect("the outer tracker must contain a source");
            location.suggestion = suggestion;
            data.location = Some(location);
            data
        }

        let mut tracker = outer_tracker
            .lock()
            .expect("string-in-JavaScript tracker mutex was poisoned");
        message.data = remap_data(&table, &mut tracker, message.data);
        for note in &mut message.notes {
            *note = remap_data(&table, &mut tracker, note.clone());
        }
        old_add_msg(message);
    });
    log
}

#[must_use]
pub fn linkify_text(mut text: &str, underline: &str, reset: &str) -> String {
    if underline.is_empty() || !text.contains("https://") {
        return text.to_string();
    }

    let mut result = String::new();
    while let Some(https) = text.find("https://") {
        let mut end = text[https..]
            .find(' ')
            .map_or(text.len(), |offset| https + offset);
        if end > https
            && matches!(
                text.as_bytes()[end - 1],
                b'.' | b',' | b'?' | b'!' | b')' | b']' | b'}'
            )
        {
            end -= 1;
        }
        result.push_str(&text[..https]);
        result.push_str(underline);
        result.push_str(&text[https..end]);
        result.push_str(reset);
        text = &text[end..];
    }
    result.push_str(text);
    result
}

#[must_use]
pub fn wrap_words_in_string(mut text: &str, width: usize) -> Vec<String> {
    let mut runs = Vec::new();

    'outer: while !text.is_empty() {
        let mut index = 0;
        let mut columns = 0;
        let mut word_end = 0;

        while index < text.len() && text.as_bytes()[index] == b' ' {
            index += 1;
            columns += 1;
        }

        while index < text.len() {
            let old_word_end = word_end;
            let word_start = index;
            while index < text.len() {
                let Some(character) = text[index..].chars().next() else {
                    break;
                };
                if character == ' ' {
                    break;
                }
                index += character.len_utf8();
                columns += 1;
            }
            word_end = index;

            if word_start > 0 && columns > width {
                runs.push(text[..old_word_end].to_string());
                text = &text[word_start..];
                continue 'outer;
            }

            while index < text.len() && text.as_bytes()[index] == b' ' {
                index += 1;
                columns += 1;
            }
        }
        break;
    }

    runs.push(text.trim_end_matches(' ').to_string());
    runs
}

/// Estimates printed columns by treating each code point as one column.
#[must_use]
pub fn estimate_width_in_terminal(text: &str) -> usize {
    text.chars()
        .filter(|character| *character != '\u{feff}')
        .count()
}

/// # Panics
///
/// Panics if `spaces_per_tab` is zero and `with_tabs` contains a tab.
#[must_use]
pub fn render_tab_stops(with_tabs: &str, spaces_per_tab: usize) -> String {
    if !with_tabs.contains('\t') {
        return with_tabs.to_string();
    }
    let mut without_tabs = String::new();
    let mut count = 0;
    for character in with_tabs.chars() {
        if character == '\t' {
            let spaces = spaces_per_tab - count % spaces_per_tab;
            without_tabs.extend(std::iter::repeat_n(' ', spaces));
            count += spaces;
        } else {
            without_tabs.push(character);
            count += 1;
        }
    }
    without_tabs
}

const DEFAULT_TERMINAL_WIDTH: usize = 80;
const EXTRA_MARGIN_CHARS: usize = 9;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalInfo {
    pub is_tty: bool,
    pub use_color_escapes: bool,
    pub width: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UseColor {
    #[default]
    IfTerminal,
    Never,
    Always,
}

#[derive(Clone, Debug)]
pub struct OutputOptions {
    pub message_limit: usize,
    pub include_source: bool,
    pub color: UseColor,
    pub log_level: LogLevel,
    pub path_style: PathStyle,
    pub overrides: HashMap<MsgId, LogLevel>,
}

impl Default for OutputOptions {
    fn default() -> Self {
        Self {
            message_limit: 0,
            include_source: false,
            color: UseColor::IfTerminal,
            log_level: LogLevel::None,
            path_style: PathStyle::Relative,
            overrides: HashMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Colors {
    pub reset: &'static str,
    pub bold: &'static str,
    pub dim: &'static str,
    pub underline: &'static str,
    pub red: &'static str,
    pub green: &'static str,
    pub blue: &'static str,
    pub cyan: &'static str,
    pub magenta: &'static str,
    pub yellow: &'static str,
    pub red_bg_red: &'static str,
    pub red_bg_white: &'static str,
    pub green_bg_green: &'static str,
    pub green_bg_white: &'static str,
    pub blue_bg_blue: &'static str,
    pub blue_bg_white: &'static str,
    pub cyan_bg_cyan: &'static str,
    pub cyan_bg_black: &'static str,
    pub magenta_bg_magenta: &'static str,
    pub magenta_bg_black: &'static str,
    pub yellow_bg_yellow: &'static str,
    pub yellow_bg_black: &'static str,
}

pub const TERMINAL_COLORS: Colors = Colors {
    reset: "\x1b[0m",
    bold: "\x1b[1m",
    dim: "\x1b[37m",
    underline: "\x1b[4m",
    red: "\x1b[31m",
    green: "\x1b[32m",
    blue: "\x1b[34m",
    cyan: "\x1b[36m",
    magenta: "\x1b[35m",
    yellow: "\x1b[33m",
    red_bg_red: "\x1b[41;31m",
    red_bg_white: "\x1b[41;97m",
    green_bg_green: "\x1b[42;32m",
    green_bg_white: "\x1b[42;97m",
    blue_bg_blue: "\x1b[44;34m",
    blue_bg_white: "\x1b[44;97m",
    cyan_bg_cyan: "\x1b[46;36m",
    cyan_bg_black: "\x1b[46;30m",
    magenta_bg_magenta: "\x1b[45;35m",
    magenta_bg_black: "\x1b[45;30m",
    yellow_bg_yellow: "\x1b[43;33m",
    yellow_bg_black: "\x1b[43;30m",
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MsgDetail {
    pub source_before: Vec<u8>,
    pub source_marked: Vec<u8>,
    pub source_after: Vec<u8>,
    pub indent: String,
    pub marker: String,
    pub suggestion: String,
    pub content_after: Vec<u8>,
    pub path: String,
    pub line: usize,
    pub column: usize,
}

fn margin_with_line_text(max_margin: usize, line: usize) -> String {
    let number = line.to_string();
    format!(
        "      {}{} │ ",
        " ".repeat(max_margin.saturating_sub(number.len())),
        number
    )
}

fn empty_margin_text(max_margin: usize, is_last: bool) -> String {
    format!(
        "      {} {} ",
        " ".repeat(max_margin),
        if is_last { '╵' } else { '│' }
    )
}

#[must_use]
pub fn message_detail(
    data: &MsgData,
    path_style: PathStyle,
    terminal_info: TerminalInfo,
    max_margin: usize,
) -> Option<MsgDetail> {
    let mut location = data.location.clone()?;
    let end_of_first_line = location
        .line_text
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(location.line_text.len());
    let first_line = &location.line_text[..end_of_first_line];
    let mut after_first_line = location.line_text[end_of_first_line..].to_vec();
    if !after_first_line.is_empty() && !after_first_line.ends_with(b"\n") {
        after_first_line.push(b'\n');
    }

    location.column = location.column.min(end_of_first_line);
    location.length = location.length.min(end_of_first_line - location.column);

    let spaces_per_tab = 2;
    let mut line_text = render_tab_stops_bytes(first_line, spaces_per_tab);
    let text_up_to_location =
        render_tab_stops_bytes(&first_line[..location.column], spaces_per_tab);
    let mut marker_start = text_up_to_location.len();
    let mut marker_end = marker_start;
    let mut indent = " ".repeat(estimate_width_in_terminal_bytes(&text_up_to_location));
    let mut marker = "^".to_string();

    if location.length > 0 {
        marker_end = render_tab_stops_bytes(
            &first_line[..location.column + location.length],
            spaces_per_tab,
        )
        .len();
    }
    marker_start = marker_start.min(line_text.len());
    marker_end = marker_end.min(line_text.len()).max(marker_start);

    let mut width = if terminal_info.width < 1 {
        DEFAULT_TERMINAL_WIDTH
    } else {
        terminal_info.width
    };
    width = width.saturating_sub(max_margin + EXTRA_MARGIN_CHARS).max(1);
    if location.column == end_of_first_line {
        width = width.saturating_sub(1);
    }

    if line_text.len() > width {
        let mut slice_start = (marker_start + marker_end).saturating_sub(width) / 2;
        let preferred_start = marker_start.saturating_sub(width / 5);
        if slice_start > preferred_start {
            slice_start = preferred_start;
        }
        slice_start = slice_start.min(line_text.len() - width);
        let slice_end = slice_start + width;

        let mut sliced_line = line_text[slice_start..slice_end].to_vec();
        marker_start = marker_start.saturating_sub(slice_start);
        marker_end = marker_end
            .saturating_sub(slice_start)
            .min(sliced_line.len());

        if sliced_line.len() > 3 && slice_start > 0 {
            sliced_line[..3].copy_from_slice(b"...");
            marker_start = marker_start.max(3);
        }
        if sliced_line.len() > 3 && slice_end < line_text.len() {
            let dots = sliced_line.len() - 3;
            sliced_line[dots..].copy_from_slice(b"...");
            marker_end = marker_end.min(dots).max(marker_start);
        }
        line_text = sliced_line;
        indent = " ".repeat(estimate_width_in_terminal_bytes(&line_text[..marker_start]));
    }

    if marker_end - marker_start > 1 {
        marker = "~".repeat(estimate_width_in_terminal_bytes(
            &line_text[marker_start..marker_end],
        ));
    }
    let mut source_before = margin_with_line_text(max_margin, location.line).into_bytes();
    source_before.extend_from_slice(&line_text[..marker_start]);

    Some(MsgDetail {
        path: location.file.select(path_style).to_string(),
        line: location.line,
        column: location.column,
        source_before,
        source_marked: line_text[marker_start..marker_end].to_vec(),
        source_after: line_text[marker_end..].to_vec(),
        indent,
        marker,
        suggestion: location.suggestion,
        content_after: after_first_line,
    })
}

fn strict_utf8_rune(bytes: &[u8]) -> (Option<char>, usize) {
    if bytes.is_empty() {
        return (None, 0);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => {
            let character = text.chars().next().expect("non-empty UTF-8 text");
            (Some(character), character.len_utf8())
        }
        Err(error) if error.valid_up_to() > 0 => {
            let valid = std::str::from_utf8(&bytes[..error.valid_up_to()])
                .expect("prefix reported as valid UTF-8");
            let character = valid.chars().next().expect("valid prefix is non-empty");
            (Some(character), character.len_utf8())
        }
        Err(_) => (None, 1),
    }
}

fn estimate_width_in_terminal_bytes(mut bytes: &[u8]) -> usize {
    let mut width = 0;
    while !bytes.is_empty() {
        let (character, size) = strict_utf8_rune(bytes);
        bytes = &bytes[size..];
        if character != Some('\u{feff}') {
            width += 1;
        }
    }
    width
}

fn render_tab_stops_bytes(with_tabs: &[u8], spaces_per_tab: usize) -> Vec<u8> {
    if !with_tabs.contains(&b'\t') {
        return with_tabs.to_vec();
    }
    let mut without_tabs = Vec::with_capacity(with_tabs.len());
    let mut count = 0;
    let mut index = 0;
    while index < with_tabs.len() {
        let (character, size) = strict_utf8_rune(&with_tabs[index..]);
        if character == Some('\t') {
            let spaces = spaces_per_tab - count % spaces_per_tab;
            without_tabs.extend(std::iter::repeat_n(b' ', spaces));
            count += spaces;
        } else {
            without_tabs.extend_from_slice(&with_tabs[index..index + size]);
            count += 1;
        }
        index += size;
    }
    without_tabs
}

impl Msg {
    #[must_use]
    pub fn to_bytes(&self, options: &OutputOptions, terminal_info: TerminalInfo) -> Vec<u8> {
        let mut output = format_message_bytes(
            options.include_source,
            options.path_style,
            terminal_info,
            self.id,
            self.kind,
            &self.data,
            &self.plugin_name,
        );
        let mut old_data: Option<&MsgData> = None;
        for (index, note) in self.notes.iter().enumerate() {
            if options.include_source
                && (index == 0
                    || old_data
                        .is_some_and(|old| old.text.contains('\n') || old.location.is_some()))
            {
                output.push(b'\n');
            }
            output.extend(format_message_bytes(
                options.include_source,
                options.path_style,
                terminal_info,
                MsgId::None,
                MsgKind::Note,
                note,
                "",
            ));
            old_data = Some(note);
        }
        if options.include_source {
            output.push(b'\n');
        }
        output
    }

    #[must_use]
    pub fn to_string_lossy(&self, options: &OutputOptions, terminal_info: TerminalInfo) -> String {
        String::from_utf8_lossy(&self.to_bytes(options, terminal_info)).into_owned()
    }
}

fn format_message_bytes(
    include_source: bool,
    path_style: PathStyle,
    terminal_info: TerminalInfo,
    id: MsgId,
    kind: MsgKind,
    data: &MsgData,
    plugin_name: &str,
) -> Vec<u8> {
    if !include_source {
        return data.location.as_ref().map_or_else(
            || format!("{}: {}\n", kind.as_str(), data.text).into_bytes(),
            |location| {
                format!(
                    "{}: {}: {}\n",
                    location.file.select(path_style),
                    kind.as_str(),
                    data.text
                )
                .into_bytes()
            },
        );
    }

    let colors = if terminal_info.use_color_escapes {
        TERMINAL_COLORS
    } else {
        Colors::default()
    };
    let location_output = format_location_bytes(data, path_style, terminal_info, &colors);

    if kind == MsgKind::Note {
        let mut output = Vec::new();
        for line in data.text.split('\n') {
            let mut wrap_width = terminal_info.width;
            if wrap_width > 2 {
                if !data.disable_maximum_width && wrap_width > 100 {
                    wrap_width = 100;
                }
                for run in wrap_words_in_string(line, wrap_width - 2) {
                    output.extend_from_slice(b"  ");
                    append_str(
                        &mut output,
                        &linkify_text(&run, colors.underline, colors.reset),
                    );
                    output.push(b'\n');
                }
            } else {
                output.extend_from_slice(b"  ");
                append_str(
                    &mut output,
                    &linkify_text(line, colors.underline, colors.reset),
                );
                output.push(b'\n');
            }
        }
        output.extend(location_output);
        return output;
    }

    let (icon_color, kind_color_brackets, kind_color_text) = match kind {
        MsgKind::Verbose => (colors.cyan, colors.cyan_bg_cyan, colors.cyan_bg_black),
        MsgKind::Debug => (colors.green, colors.green_bg_green, colors.green_bg_white),
        MsgKind::Info => (colors.blue, colors.blue_bg_blue, colors.blue_bg_white),
        MsgKind::Error => (colors.red, colors.red_bg_red, colors.red_bg_white),
        MsgKind::Warning => (
            colors.yellow,
            colors.yellow_bg_yellow,
            colors.yellow_bg_black,
        ),
        MsgKind::Note => unreachable!("notes returned above"),
    };

    let plugin = if plugin_name.is_empty() {
        String::new()
    } else {
        format!(
            " {}{}[plugin {}]{}",
            colors.bold, colors.magenta, plugin_name, colors.reset
        )
    };
    let message_id = msg_id_to_string(id);
    let message_id = if message_id.is_empty() {
        String::new()
    } else {
        format!(" [{message_id}]")
    };
    let mut output = format!(
        "{}{} {}[{}{}{}]{} {}{}{}{}{}\n",
        icon_color,
        kind.icon(),
        kind_color_brackets,
        kind_color_text,
        kind.as_str(),
        kind_color_brackets,
        colors.reset,
        colors.bold,
        data.text,
        colors.reset,
        plugin,
        message_id
    )
    .into_bytes();
    output.extend(location_output);
    output
}

fn format_location_bytes(
    data: &MsgData,
    path_style: PathStyle,
    terminal_info: TerminalInfo,
    colors: &Colors,
) -> Vec<u8> {
    let Some(location) = &data.location else {
        return Vec::new();
    };
    let max_margin = location.line.to_string().len();
    let Some(detail) = message_detail(data, path_style, terminal_info, max_margin) else {
        return Vec::new();
    };
    let mut output =
        format!("\n    {}:{}:{}:\n", detail.path, detail.line, detail.column).into_bytes();
    append_str(&mut output, colors.dim);
    output.extend_from_slice(&detail.source_before);
    append_str(&mut output, colors.green);
    output.extend_from_slice(&detail.source_marked);
    append_str(&mut output, colors.dim);
    output.extend_from_slice(&detail.source_after);
    output.push(b'\n');

    append_str(
        &mut output,
        &empty_margin_text(max_margin, detail.suggestion.is_empty()),
    );
    append_str(&mut output, &detail.indent);
    append_str(&mut output, colors.green);
    append_str(&mut output, &detail.marker);
    if detail.suggestion.is_empty() {
        append_str(&mut output, colors.reset);
        output.push(b'\n');
    } else {
        append_str(&mut output, colors.dim);
        output.push(b'\n');
        append_str(&mut output, &empty_margin_text(max_margin, true));
        append_str(&mut output, &detail.indent);
        append_str(&mut output, colors.green);
        append_str(&mut output, &detail.suggestion);
        append_str(&mut output, colors.reset);
        output.push(b'\n');
    }
    output.extend_from_slice(&detail.content_after);
    output
}

fn append_str(output: &mut Vec<u8>, text: &str) {
    output.extend_from_slice(text.as_bytes());
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
        DeferLogKind, ImportAttributes, LineColumnTracker, Loc, Log, LogLevel, Msg, MsgData, MsgId,
        MsgKind, MsgLocation, OutputOptions, PathFlags, PathStyle, PrettyPaths, Range, Source,
        TerminalInfo, estimate_width_in_terminal, generate_string_in_js_table, linkify_text,
        message_detail, new_string_in_js_log, platform_independent_path_dir_base_ext,
        remap_string_in_js_loc, render_tab_stops, wrap_words_in_string,
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
    fn wrapped_log_remaps_embedded_diagnostic_locations() {
        let source = Source {
            pretty_paths: PrettyPaths {
                abs: "/outer.js".into(),
                rel: "outer.js".into(),
            },
            contents: Arc::from(&b"prefix \"abc\" suffix"[..]),
            ..Source::default()
        };
        let table = generate_string_in_js_table(&source.contents, Loc { start: 7 }, b"abc");
        let log = new_string_in_js_log(
            Log::new_defer(DeferLogKind::All, HashMap::new()),
            &LineColumnTracker::new(Some(&source)),
            table,
        );
        let mut message = Msg::new(MsgKind::Error, "embedded");
        message.data.location = Some(MsgLocation {
            line: 1,
            column: 2,
            length: 1,
            suggestion: "c".into(),
            ..MsgLocation::default()
        });
        log.add_msg(message);

        let messages = log.done();
        let location = messages[0].data.location.as_ref().unwrap();
        assert_eq!(location.file.rel, "outer.js");
        assert_eq!(
            (location.line, location.column, location.length),
            (1, 10, 1)
        );
        assert_eq!(location.suggestion, "c");
    }

    #[test]
    fn deferred_logs_track_errors_filter_and_sort() {
        let log = Log::new_defer(DeferLogKind::NoVerboseOrDebug, HashMap::new());
        log.add_msg(Msg::new(MsgKind::Debug, "hidden"));
        let mut later = Msg::new(MsgKind::Warning, "later");
        later.data.location = Some(MsgLocation {
            file: PrettyPaths {
                abs: "/b.js".into(),
                rel: "b.js".into(),
            },
            line: 2,
            ..MsgLocation::default()
        });
        log.add_msg(later);
        let mut earlier = Msg::new(MsgKind::Error, "earlier");
        earlier.data.location = Some(MsgLocation {
            file: PrettyPaths {
                abs: "/a.js".into(),
                rel: "a.js".into(),
            },
            line: 1,
            ..MsgLocation::default()
        });
        log.add_msg(earlier);

        assert!(log.has_errors());
        assert_eq!(log.peek().len(), 2);
        let messages = log.done();
        assert_eq!(messages[0].data.text, "earlier");
        assert_eq!(messages[1].data.text, "later");
    }

    #[test]
    fn message_id_overrides_change_or_silence_diagnostics() {
        let log = Log::new_defer(
            DeferLogKind::All,
            HashMap::from([
                (MsgId::JsDirectEval, LogLevel::Error),
                (MsgId::JsBigInt, LogLevel::Silent),
            ]),
        );
        log.add_id(
            MsgId::JsDirectEval,
            MsgKind::Warning,
            None,
            Range::default(),
            "promoted",
        );
        log.add_id(
            MsgId::JsBigInt,
            MsgKind::Warning,
            None,
            Range::default(),
            "hidden",
        );
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].kind, MsgKind::Error);
        assert_eq!(messages[0].id, MsgId::JsDirectEval);
        assert!(log.has_errors());
    }

    #[test]
    fn deferred_log_is_safe_for_parallel_parsers() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut threads = Vec::new();
        for thread_index in 0..4 {
            let log = log.clone();
            threads.push(std::thread::spawn(move || {
                for message_index in 0..25 {
                    log.add_msg(Msg::new(
                        MsgKind::Info,
                        format!("{thread_index}:{message_index}"),
                    ));
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(log.done().len(), 100);
    }

    #[test]
    fn linkifies_urls_without_underlining_trailing_punctuation() {
        assert_eq!(
            linkify_text(
                "See https://example.com/a, then https://example.com/b.",
                "<u>",
                "</u>"
            ),
            "See <u>https://example.com/a</u>, then <u>https://example.com/b</u>."
        );
        assert_eq!(
            linkify_text("https://example.com", "", "reset"),
            "https://example.com"
        );
    }

    #[test]
    fn wraps_words_by_code_points_and_preserves_long_words() {
        assert_eq!(
            wrap_words_in_string("one two three", 7),
            ["one two", "three"]
        );
        assert_eq!(
            wrap_words_in_string("🙂🙂🙂 trailing   ", 2),
            ["🙂🙂🙂", "trailing"]
        );
        assert_eq!(wrap_words_in_string("", 10), [""]);
    }

    #[test]
    fn estimates_width_and_expands_tab_stops() {
        assert_eq!(estimate_width_in_terminal("a\u{feff}🙂"), 2);
        assert_eq!(render_tab_stops("a\tb\t", 4), "a   b   ");
        assert_eq!(render_tab_stops("no tabs", 0), "no tabs");
    }

    #[test]
    fn lays_out_source_markers_using_byte_columns_and_tab_stops() {
        let detail = message_detail(
            &MsgData {
                location: Some(MsgLocation {
                    file: PrettyPaths {
                        abs: "/abs/a.js".into(),
                        rel: "a.js".into(),
                    },
                    line_text: b"\tfoo bar".to_vec(),
                    suggestion: "fix".into(),
                    line: 4,
                    column: 1,
                    length: 3,
                    ..MsgLocation::default()
                }),
                ..MsgData::default()
            },
            PathStyle::Relative,
            TerminalInfo {
                width: 80,
                ..TerminalInfo::default()
            },
            1,
        )
        .unwrap();
        assert_eq!(detail.path, "a.js");
        assert!(detail.source_before.ends_with(b"  "));
        assert_eq!(detail.source_marked, b"foo");
        assert_eq!(detail.source_after, b" bar");
        assert_eq!(detail.indent, "  ");
        assert_eq!(detail.marker, "~~~");
        assert_eq!(detail.suggestion, "fix");
    }

    #[test]
    fn formats_plain_and_source_annotated_diagnostics() {
        let mut message = Msg::new(MsgKind::Error, "boom");
        message.id = MsgId::JsDirectEval;
        message.plugin_name = "test".into();
        message.data.location = Some(MsgLocation {
            file: PrettyPaths {
                abs: "/abs/a.js".into(),
                rel: "a.js".into(),
            },
            line_text: b"let x = 1".to_vec(),
            line: 1,
            column: 4,
            length: 1,
            ..MsgLocation::default()
        });

        assert_eq!(
            message.to_bytes(&OutputOptions::default(), TerminalInfo::default()),
            b"a.js: ERROR: boom\n"
        );

        let output = message.to_bytes(
            &OutputOptions {
                include_source: true,
                ..OutputOptions::default()
            },
            TerminalInfo {
                width: 80,
                ..TerminalInfo::default()
            },
        );
        assert_eq!(
            output,
            concat!(
                "✘ [ERROR] boom [plugin test] [direct-eval]\n",
                "\n    a.js:1:4:\n",
                "      1 │ let x = 1\n",
                "        ╵     ^\n",
                "\n"
            )
            .as_bytes()
        );
    }

    #[test]
    fn formatter_preserves_invalid_source_bytes() {
        let mut message = Msg::new(MsgKind::Warning, "raw");
        message.data.location = Some(MsgLocation {
            file: PrettyPaths {
                rel: "raw.js".into(),
                ..PrettyPaths::default()
            },
            line_text: vec![b'a', 0xff, b'b'],
            line: 1,
            column: 1,
            length: 1,
            ..MsgLocation::default()
        });
        let output = message.to_bytes(
            &OutputOptions {
                include_source: true,
                ..OutputOptions::default()
            },
            TerminalInfo {
                width: 80,
                ..TerminalInfo::default()
            },
        );
        assert!(output.contains(&0xff));
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
