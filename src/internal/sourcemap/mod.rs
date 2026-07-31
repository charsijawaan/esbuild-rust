// Port of upstream internal/sourcemap.

use crate::internal::ast::Index32;
use crate::internal::helpers::{Joiner, quote_for_json};
use crate::internal::logger::Loc;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Mapping {
    /// Zero-based.
    pub generated_line: i32,
    /// Zero-based count of UTF-16 code units.
    pub generated_column: i32,
    /// Zero-based.
    pub source_index: i32,
    /// Zero-based.
    pub original_line: i32,
    /// Zero-based count of UTF-16 code units.
    pub original_column: i32,
    /// Zero-based and optional.
    pub original_name: Index32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SourceMap {
    pub sources: Vec<String>,
    pub sources_content: Vec<SourceContent>,
    pub mappings: Vec<Mapping>,
    pub names: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SourceContent {
    /// A cached quoted representation, when available.
    pub quoted: String,
    /// The parsed UTF-16 value used when the source must be quoted again.
    pub value: Vec<u16>,
}

impl SourceMap {
    #[must_use]
    pub fn find(&self, line: i32, column: i32) -> Option<&Mapping> {
        let mappings = &self.mappings;
        let mut count = mappings.len();
        let mut index = 0;
        while count > 0 {
            let step = count / 2;
            let candidate = index + step;
            let mapping = mappings[candidate];
            if mapping.generated_line < line
                || (mapping.generated_line == line && mapping.generated_column <= column)
            {
                index = candidate + 1;
                count -= step + 1;
            } else {
                count = step;
            }
        }

        if index > 0 {
            let mapping = &mappings[index - 1];
            if mapping.generated_line == line {
                return Some(mapping);
            }
        }
        None
    }
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode_vlq(mut encoded: Vec<u8>, value: i32) -> Vec<u8> {
    let value = i64::from(value);
    let mut vlq = if value < 0 {
        ((-value) << 1) | 1
    } else {
        value << 1
    };

    if vlq >> 5 == 0 {
        encoded.push(BASE64[usize::try_from(vlq & 31).expect("VLQ digit is non-negative")]);
        return encoded;
    }

    loop {
        let mut digit = vlq & 31;
        vlq >>= 5;
        if vlq != 0 {
            digit |= 32;
        }
        encoded.push(BASE64[usize::try_from(digit).expect("VLQ digit is non-negative")]);
        if vlq == 0 {
            break;
        }
    }
    encoded
}

/// # Panics
///
/// Panics if `start` is outside `encoded`.
#[must_use]
pub fn decode_vlq(encoded: &[u8], mut start: usize) -> (i32, usize) {
    let mut shift = 0;
    let mut vlq = 0_i64;
    while let Some(index) = BASE64.iter().position(|digit| *digit == encoded[start]) {
        let digit = u8::try_from(index & 31).unwrap_or_default();
        vlq |= i64::from(digit).checked_shl(shift).unwrap_or(0);
        start += 1;
        shift += 5;
        if index & 32 == 0 {
            break;
        }
    }

    let mut value = vlq >> 1;
    if vlq & 1 != 0 {
        value = -value;
    }
    (
        i32::try_from(value).expect("decoded VLQ value must fit in i32"),
        start,
    )
}

#[must_use]
pub fn decode_vlq_utf16(encoded: &[u16]) -> (i32, usize, bool) {
    if encoded.is_empty() {
        return (0, 0, false);
    }

    let mut current = 0;
    let mut shift = 0;
    let mut vlq = 0_i32;
    loop {
        if current >= encoded.len() {
            return (0, 0, false);
        }
        let Some(index) = u8::try_from(encoded[current])
            .ok()
            .and_then(|digit| BASE64.iter().position(|candidate| *candidate == digit))
        else {
            return (0, 0, false);
        };
        let digit = u8::try_from(index & 31).unwrap_or_default();
        vlq |= i32::from(digit).checked_shl(shift).unwrap_or(0);
        current += 1;
        shift += 5;
        if index & 32 == 0 {
            break;
        }
    }

    let mut value = vlq >> 1;
    if vlq & 1 != 0 {
        value = -value;
    }
    (value, current, true)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LineColumnOffset {
    pub lines: i32,
    pub columns: i32,
}

impl LineColumnOffset {
    #[must_use]
    pub const fn comes_before(self, other: Self) -> bool {
        self.lines < other.lines || (self.lines == other.lines && self.columns < other.columns)
    }

    pub fn add(&mut self, other: Self) {
        if other.lines == 0 {
            self.columns += other.columns;
        } else {
            self.lines += other.lines;
            self.columns = other.columns;
        }
    }

    pub fn advance_bytes(&mut self, mut bytes: &[u8]) {
        let mut columns = self.columns;
        while !bytes.is_empty() {
            let (character, width) = decode_utf8_rune(bytes);
            bytes = &bytes[width..];
            match character {
                '\r' | '\n' | '\u{2028}' | '\u{2029}' => {
                    if character == '\r' && bytes.first() == Some(&b'\n') {
                        columns += 1;
                        continue;
                    }
                    self.lines += 1;
                    columns = 0;
                }
                _ => {
                    columns += if u32::from(character) <= 0xffff { 1 } else { 2 };
                }
            }
        }
        self.columns = columns;
    }

    pub fn advance_string(&mut self, text: &str) {
        self.advance_bytes(text.as_bytes());
    }
}

#[inline]
fn decode_utf8_rune(bytes: &[u8]) -> (char, usize) {
    let first = bytes[0];
    if first < 0x80 {
        return (char::from(first), 1);
    }

    let (width, mut code_point, minimum) = if first & 0xe0 == 0xc0 {
        (2, u32::from(first & 0x1f), 0x80)
    } else if first & 0xf0 == 0xe0 {
        (3, u32::from(first & 0x0f), 0x800)
    } else if first & 0xf8 == 0xf0 {
        (4, u32::from(first & 0x07), 0x1_0000)
    } else {
        return ('\u{fffd}', 1);
    };
    let Some(continuations) = bytes.get(1..width) else {
        return ('\u{fffd}', 1);
    };

    for continuation in continuations {
        if continuation & 0xc0 != 0x80 {
            return ('\u{fffd}', 1);
        }
        code_point = (code_point << 6) | u32::from(continuation & 0x3f);
    }

    if code_point < minimum {
        return ('\u{fffd}', 1);
    }
    match char::from_u32(code_point) {
        Some(character) => (character, width),
        None => ('\u{fffd}', 1),
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SourceMapPieces {
    pub prefix: Vec<u8>,
    pub mappings: Vec<u8>,
    pub suffix: Vec<u8>,
}

impl SourceMapPieces {
    #[must_use]
    pub fn has_content(&self) -> bool {
        !self.prefix.is_empty() || !self.mappings.is_empty() || !self.suffix.is_empty()
    }

    /// # Panics
    ///
    /// Panics if mappings are malformed, source-map shifts are unordered, or a
    /// shift changes the generated line.
    #[must_use]
    pub fn finalize(mut self, mut shifts: &[SourceMapShift]) -> Vec<u8> {
        if shifts.len() == 1 {
            self.prefix.reserve(self.mappings.len() + self.suffix.len());
            self.prefix.extend_from_slice(&self.mappings);
            self.prefix.extend_from_slice(&self.suffix);
            return self.prefix;
        }

        let mut start_of_run = 0;
        let mut current = 0;
        let mut generated = LineColumnOffset::default();
        let mut previous_shift_column_delta = 0;
        let mut joiner = Joiner::default();
        joiner.add_bytes(self.prefix);

        while current < self.mappings.len() {
            if self.mappings[current] == b';' {
                generated.lines += 1;
                generated.columns = 0;
                previous_shift_column_delta = 0;
                current += 1;
                continue;
            }

            let potential_end_of_run = current;
            let (generated_column_delta, next) = decode_vlq(&self.mappings, current);
            generated.columns += generated_column_delta;
            current = next;
            let potential_start_of_run = current;

            if current < self.mappings.len() {
                (_, current) = decode_vlq(&self.mappings, current);
                (_, current) = decode_vlq(&self.mappings, current);
                (_, current) = decode_vlq(&self.mappings, current);
                if current < self.mappings.len() {
                    (_, current) = decode_vlq(&self.mappings, current);
                }
            }
            if current < self.mappings.len() && self.mappings[current] == b',' {
                current += 1;
            }

            let mut crossed_boundary = false;
            while shifts.len() > 1 && shifts[1].before.comes_before(generated) {
                shifts = &shifts[1..];
                crossed_boundary = true;
            }
            if !crossed_boundary {
                continue;
            }

            let shift = shifts[0];
            if shift.after.lines != generated.lines {
                continue;
            }

            joiner.add_bytes(self.mappings[start_of_run..potential_end_of_run].to_vec());
            assert_eq!(
                shift.before.lines, shift.after.lines,
                "unexpected line change when shifting source maps"
            );
            let shift_column_delta = shift.after.columns - shift.before.columns;
            joiner.add_bytes(encode_vlq(
                Vec::new(),
                generated_column_delta + shift_column_delta - previous_shift_column_delta,
            ));
            previous_shift_column_delta = shift_column_delta;
            start_of_run = potential_start_of_run;
        }

        joiner.add_bytes(self.mappings[start_of_run..].to_vec());
        joiner.add_bytes(self.suffix);
        joiner.done()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SourceMapShift {
    pub before: LineColumnOffset,
    pub after: LineColumnOffset,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SourceMapState {
    pub generated_line: i32,
    pub generated_column: i32,
    pub source_index: i32,
    pub original_line: i32,
    pub original_column: i32,
    pub original_name: i32,
    pub has_original_name: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MappingsBuffer {
    pub data: Vec<u8>,
    pub first_name_offset: Index32,
}

/// # Panics
///
/// Panics if `buffer` is empty or contains malformed mappings.
pub fn append_source_map_chunk(
    joiner: &mut Joiner,
    mut previous_end_state: SourceMapState,
    mut start_state: SourceMapState,
    buffer: &MappingsBuffer,
) {
    if start_state.generated_line != 0 {
        joiner.add_bytes(vec![
            b';';
            usize::try_from(start_state.generated_line)
                .expect("generated line count must be non-negative")
        ]);
        previous_end_state.generated_column = 0;
    }

    let mut semicolons = 0;
    while buffer.data[semicolons] == b';' {
        semicolons += 1;
    }
    if semicolons > 0 {
        joiner.add_bytes(buffer.data[..semicolons].to_vec());
        previous_end_state.generated_column = 0;
        start_state.generated_column = 0;
    }

    let (generated_column, mut index) = decode_vlq(&buffer.data, semicolons);
    let mut source_index = 0;
    let mut original_line = 0;
    let mut original_column = 0;
    let omit_source =
        index == buffer.data.len() || matches!(buffer.data.get(index), Some(b',' | b';'));
    if !omit_source {
        (source_index, index) = decode_vlq(&buffer.data, index);
        (original_line, index) = decode_vlq(&buffer.data, index);
        (original_column, index) = decode_vlq(&buffer.data, index);
    }

    start_state.generated_column += generated_column;
    start_state.source_index += source_index;
    start_state.original_line += original_line;
    start_state.original_column += original_column;
    previous_end_state.has_original_name = false;
    let (rewritten, _) = append_mapping_to_buffer(
        Vec::new(),
        joiner.last_byte(),
        previous_end_state,
        start_state,
        omit_source,
    );
    joiner.add_bytes(rewritten);

    if buffer.first_name_offset.is_valid() {
        let before = usize::try_from(buffer.first_name_offset.get_index())
            .expect("name offset must fit in usize");
        let (mut original_name, after) = decode_vlq(&buffer.data, before);
        original_name += start_state.original_name - previous_end_state.original_name;
        joiner.add_bytes(buffer.data[index..before].to_vec());
        joiner.add_bytes(encode_vlq(Vec::new(), original_name));
        joiner.add_bytes(buffer.data[after..].to_vec());
        return;
    }
    joiner.add_bytes(buffer.data[index..].to_vec());
}

fn append_mapping_to_buffer(
    mut buffer: Vec<u8>,
    last_byte: u8,
    previous_state: SourceMapState,
    current_state: SourceMapState,
    omit_source: bool,
) -> (Vec<u8>, Index32) {
    if !matches!(last_byte, 0 | b';' | b'"') {
        buffer.push(b',');
    }

    buffer = encode_vlq(
        buffer,
        current_state.generated_column - previous_state.generated_column,
    );
    if !omit_source {
        buffer = encode_vlq(
            buffer,
            current_state.source_index - previous_state.source_index,
        );
        buffer = encode_vlq(
            buffer,
            current_state.original_line - previous_state.original_line,
        );
        buffer = encode_vlq(
            buffer,
            current_state.original_column - previous_state.original_column,
        );
    }

    let mut name_offset = Index32::default();
    if current_state.has_original_name {
        name_offset = Index32::new(
            u32::try_from(buffer.len()).expect("source map buffer must fit in 32 bits"),
        );
        buffer = encode_vlq(
            buffer,
            current_state.original_name - previous_state.original_name,
        );
    }
    (buffer, name_offset)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LineOffsetTable {
    columns_for_non_ascii: Option<Vec<i32>>,
    byte_offset_to_first_non_ascii: i32,
    byte_offset_to_start_of_line: i32,
}

/// # Panics
///
/// Panics if the source is larger than the signed 32-bit size limit.
#[must_use]
pub fn generate_line_offset_tables(
    contents: &[u8],
    approximate_line_count: i32,
) -> Vec<LineOffsetTable> {
    let mut columns_for_non_ascii: Option<Vec<i32>> = None;
    let mut byte_offset_to_first_non_ascii = 0;
    let mut line_byte_offset = 0;
    let mut column_byte_offset = 0;
    let mut column = 0;
    let mut tables = Vec::with_capacity(
        usize::try_from(approximate_line_count).expect("line count must be non-negative"),
    );

    let mut index = 0;
    while index < contents.len() {
        let (character, width) = decode_utf8_rune(&contents[index..]);
        if column == 0 {
            line_byte_offset = index;
        }

        if u32::from(character) > 0x7f && columns_for_non_ascii.is_none() {
            column_byte_offset = index - line_byte_offset;
            byte_offset_to_first_non_ascii =
                i32::try_from(column_byte_offset).expect("source must fit in 32 bits");
            let bytes_until_ascii_line_break = contents[index..]
                .iter()
                .position(|byte| matches!(*byte, b'\r' | b'\n'))
                .unwrap_or(contents.len() - index);
            columns_for_non_ascii = Some(Vec::with_capacity(bytes_until_ascii_line_break + 1));
        }

        if let Some(columns) = &mut columns_for_non_ascii {
            let line_bytes_so_far = index - line_byte_offset;
            while column_byte_offset <= line_bytes_so_far {
                columns.push(column);
                column_byte_offset += 1;
            }
        }

        match character {
            '\r' | '\n' | '\u{2028}' | '\u{2029}' => {
                if character == '\r' && contents.get(index + 1) == Some(&b'\n') {
                    column += 1;
                    index += width;
                    continue;
                }

                tables.push(LineOffsetTable {
                    byte_offset_to_start_of_line: i32::try_from(line_byte_offset)
                        .expect("source must fit in 32 bits"),
                    byte_offset_to_first_non_ascii,
                    columns_for_non_ascii,
                });
                column_byte_offset = 0;
                byte_offset_to_first_non_ascii = 0;
                columns_for_non_ascii = None;
                column = 0;
            }
            _ => {
                column += if u32::from(character) <= 0xffff { 1 } else { 2 };
            }
        }
        index += width;
    }

    if column == 0 {
        line_byte_offset = contents.len();
    }
    if let Some(columns) = &mut columns_for_non_ascii {
        let line_bytes_so_far = contents.len() - line_byte_offset;
        while column_byte_offset <= line_bytes_so_far {
            columns.push(column);
            column_byte_offset += 1;
        }
    }
    tables.push(LineOffsetTable {
        byte_offset_to_start_of_line: i32::try_from(line_byte_offset)
            .expect("source must fit in 32 bits"),
        byte_offset_to_first_non_ascii,
        columns_for_non_ascii,
    });
    tables
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Chunk {
    pub buffer: MappingsBuffer,
    pub quoted_names: Vec<Vec<u8>>,
    pub end_state: SourceMapState,
    pub final_generated_column: i32,
    pub should_ignore: bool,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub struct ChunkBuilder {
    input_source_map: Option<Arc<SourceMap>>,
    source_map: Vec<u8>,
    quoted_names: Vec<Vec<u8>>,
    names_map: HashMap<String, u32>,
    line_offset_tables: Arc<[LineOffsetTable]>,
    previous_original_name: String,
    previous_state: SourceMapState,
    last_generated_update: usize,
    generated_column: i32,
    previous_generated_len: usize,
    previous_original_location: Loc,
    first_name_offset: Index32,
    has_previous_state: bool,
    ascii_only: bool,
    line_starts_with_mapping: bool,
    cover_lines_without_mappings: bool,
}

#[must_use]
pub fn make_chunk_builder(
    input_source_map: Option<Arc<SourceMap>>,
    line_offset_tables: impl Into<Arc<[LineOffsetTable]>>,
    ascii_only: bool,
) -> ChunkBuilder {
    let cover_lines_without_mappings = input_source_map.is_none();
    ChunkBuilder {
        input_source_map,
        source_map: Vec::new(),
        quoted_names: Vec::new(),
        names_map: HashMap::new(),
        line_offset_tables: line_offset_tables.into(),
        previous_original_name: String::new(),
        previous_state: SourceMapState::default(),
        last_generated_update: 0,
        generated_column: 0,
        previous_generated_len: 0,
        previous_original_location: Loc { start: -1 },
        first_name_offset: Index32::default(),
        has_previous_state: false,
        ascii_only,
        line_starts_with_mapping: false,
        cover_lines_without_mappings,
    }
}

impl ChunkBuilder {
    /// # Panics
    ///
    /// Panics if `original_location` is outside the source, line-offset tables
    /// are missing, or `output` shrinks between calls.
    pub fn add_source_mapping(
        &mut self,
        original_location: Loc,
        original_name: &str,
        output: &[u8],
    ) {
        if original_location == self.previous_original_location
            && (self.previous_generated_len == output.len()
                || self.previous_original_name == original_name)
        {
            return;
        }
        self.previous_original_location = original_location;
        self.previous_generated_len = output.len();
        original_name.clone_into(&mut self.previous_original_name);

        let mut count = self.line_offset_tables.len();
        let mut original_line = 0;
        while count > 0 {
            let step = count / 2;
            let candidate = original_line + step;
            if self.line_offset_tables[candidate].byte_offset_to_start_of_line
                <= original_location.start
            {
                original_line = candidate + 1;
                count -= step + 1;
            } else {
                count = step;
            }
        }
        original_line -= 1;

        let line = &self.line_offset_tables[original_line];
        let mut original_column =
            usize::try_from(original_location.start - line.byte_offset_to_start_of_line)
                .expect("original column must be non-negative");
        if let Some(columns) = &line.columns_for_non_ascii
            && original_column
                >= usize::try_from(line.byte_offset_to_first_non_ascii)
                    .expect("non-ASCII offset must be non-negative")
        {
            original_column = usize::try_from(
                columns[original_column
                    - usize::try_from(line.byte_offset_to_first_non_ascii)
                        .expect("non-ASCII offset must be non-negative")],
            )
            .expect("UTF-16 column must be non-negative");
        }

        self.update_generated_line_and_column(output);
        if self.cover_lines_without_mappings
            && !self.line_starts_with_mapping
            && self.generated_column > 0
            && self.has_previous_state
        {
            self.append_mapping_without_remapping(SourceMapState {
                generated_line: self.previous_state.generated_line,
                generated_column: 0,
                source_index: self.previous_state.source_index,
                original_line: self.previous_state.original_line,
                original_column: self.previous_state.original_column,
                ..SourceMapState::default()
            });
        }

        self.append_mapping(
            original_name,
            SourceMapState {
                generated_line: self.previous_state.generated_line,
                generated_column: self.generated_column,
                original_line: i32::try_from(original_line)
                    .expect("line index must fit in 32 bits"),
                original_column: i32::try_from(original_column)
                    .expect("column must fit in 32 bits"),
                ..SourceMapState::default()
            },
        );
        self.line_starts_with_mapping = true;
    }

    /// # Panics
    ///
    /// Panics if `output` shrinks after the last mapping.
    #[must_use]
    pub fn generate_chunk(mut self, output: &[u8]) -> Chunk {
        self.update_generated_line_and_column(output);
        let should_ignore = self.source_map.iter().all(|byte| *byte == b';');
        Chunk {
            buffer: MappingsBuffer {
                data: self.source_map,
                first_name_offset: self.first_name_offset,
            },
            quoted_names: self.quoted_names,
            end_state: self.previous_state,
            final_generated_column: self.generated_column,
            should_ignore,
        }
    }

    fn update_generated_line_and_column(&mut self, output: &[u8]) {
        let mut index = self.last_generated_update;
        while index < output.len() {
            let (character, width) = decode_utf8_rune(&output[index..]);
            match character {
                '\r' | '\n' | '\u{2028}' | '\u{2029}' => {
                    if character == '\r' && output.get(index + 1) == Some(&b'\n') {
                        index += width;
                        continue;
                    }
                    if self.cover_lines_without_mappings
                        && !self.line_starts_with_mapping
                        && self.has_previous_state
                    {
                        self.append_mapping_without_remapping(SourceMapState {
                            generated_line: self.previous_state.generated_line,
                            generated_column: 0,
                            source_index: self.previous_state.source_index,
                            original_line: self.previous_state.original_line,
                            original_column: self.previous_state.original_column,
                            ..SourceMapState::default()
                        });
                    }
                    self.previous_state.generated_line += 1;
                    self.previous_state.generated_column = 0;
                    self.generated_column = 0;
                    self.source_map.push(b';');
                    self.line_starts_with_mapping = false;
                }
                _ => {
                    self.generated_column += if u32::from(character) <= 0xffff { 1 } else { 2 };
                }
            }
            index += width;
        }
        self.last_generated_update = output.len();
    }

    fn append_mapping(&mut self, original_name: &str, mut current_state: SourceMapState) {
        let mut original_name = original_name;
        if let Some(input_source_map) = &self.input_source_map {
            let Some(mapping) =
                input_source_map.find(current_state.original_line, current_state.original_column)
            else {
                return;
            };
            current_state.source_index = mapping.source_index;
            current_state.original_line = mapping.original_line;
            current_state.original_column = mapping.original_column;
            if mapping.original_name.is_valid() {
                original_name =
                    &input_source_map.names[usize::try_from(mapping.original_name.get_index())
                        .expect("name index must fit in usize")];
            }
        }

        if !original_name.is_empty() {
            let index = if let Some(index) = self.names_map.get(original_name) {
                *index
            } else {
                let index =
                    u32::try_from(self.quoted_names.len()).expect("name count must fit in 32 bits");
                self.quoted_names
                    .push(quote_for_json(original_name.as_bytes(), self.ascii_only));
                self.names_map.insert(original_name.to_string(), index);
                index
            };
            current_state.original_name = i32::try_from(index).expect("name index must fit in i32");
            current_state.has_original_name = true;
        }
        self.append_mapping_without_remapping(current_state);
    }

    fn append_mapping_without_remapping(&mut self, current_state: SourceMapState) {
        let last_byte = self.source_map.last().copied().unwrap_or(0);
        let (source_map, name_offset) = append_mapping_to_buffer(
            std::mem::take(&mut self.source_map),
            last_byte,
            self.previous_state,
            current_state,
            false,
        );
        self.source_map = source_map;
        let previous_original_name = self.previous_state.original_name;
        self.previous_state = current_state;
        if !current_state.has_original_name {
            self.previous_state.original_name = previous_original_name;
        } else if !self.first_name_offset.is_valid() {
            self.first_name_offset = name_offset;
        }
        self.has_previous_state = true;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LineColumnOffset, Mapping, MappingsBuffer, SourceMap, SourceMapPieces, SourceMapShift,
        SourceMapState, append_mapping_to_buffer, append_source_map_chunk, decode_vlq,
        decode_vlq_utf16, encode_vlq, generate_line_offset_tables, make_chunk_builder,
    };
    use crate::internal::helpers::Joiner;
    use std::sync::Arc;

    #[test]
    fn source_map_find_uses_greatest_lower_bound_on_same_line() {
        let source_map = SourceMap {
            mappings: vec![
                Mapping {
                    generated_line: 0,
                    generated_column: 0,
                    original_column: 1,
                    ..Mapping::default()
                },
                Mapping {
                    generated_line: 0,
                    generated_column: 5,
                    original_column: 2,
                    ..Mapping::default()
                },
                Mapping {
                    generated_line: 2,
                    generated_column: 0,
                    original_column: 3,
                    ..Mapping::default()
                },
            ],
            ..SourceMap::default()
        };
        assert_eq!(source_map.find(0, 4).unwrap().original_column, 1);
        assert_eq!(source_map.find(0, 5).unwrap().original_column, 2);
        assert!(source_map.find(1, 100).is_none());
        assert!(source_map.find(2, -1).is_none());
    }

    #[test]
    fn vlq_round_trips_signed_values() {
        for value in [-123_456, -16, -1, 0, 1, 15, 16, 123_456] {
            let encoded = encode_vlq(Vec::new(), value);
            assert_eq!(decode_vlq(&encoded, 0), (value, encoded.len()));
            let utf16: Vec<u16> = encoded.iter().map(|byte| u16::from(*byte)).collect();
            assert_eq!(decode_vlq_utf16(&utf16), (value, encoded.len(), true));
        }
        assert_eq!(decode_vlq_utf16(&[]), (0, 0, false));
        assert_eq!(decode_vlq_utf16(&[u16::from(b'?')]), (0, 0, false));
    }

    #[test]
    fn offsets_count_utf16_columns_and_newlines() {
        let mut offset = LineColumnOffset {
            lines: 2,
            columns: 3,
        };
        offset.advance_string("a🙂\r\nb\u{2028}c");
        assert_eq!(
            offset,
            LineColumnOffset {
                lines: 4,
                columns: 1
            }
        );
        offset.add(LineColumnOffset {
            lines: 0,
            columns: 2,
        });
        assert_eq!(offset.columns, 3);
        assert!(LineColumnOffset::default().comes_before(offset));
    }

    #[test]
    fn pieces_finalize_rewrites_columns_across_shifts() {
        let mappings = append_mapping_to_buffer(
            Vec::new(),
            0,
            SourceMapState::default(),
            SourceMapState {
                generated_column: 1,
                ..SourceMapState::default()
            },
            true,
        )
        .0;
        let mappings = append_mapping_to_buffer(
            mappings,
            b'A',
            SourceMapState {
                generated_column: 1,
                ..SourceMapState::default()
            },
            SourceMapState {
                generated_column: 5,
                ..SourceMapState::default()
            },
            true,
        )
        .0;
        let pieces = SourceMapPieces {
            prefix: b"{\"mappings\":\"".to_vec(),
            mappings,
            suffix: b"\"}".to_vec(),
        };
        let finalized = pieces.finalize(&[
            SourceMapShift::default(),
            SourceMapShift {
                before: LineColumnOffset {
                    lines: 0,
                    columns: 2,
                },
                after: LineColumnOffset {
                    lines: 0,
                    columns: 4,
                },
            },
        ]);
        assert_eq!(finalized, b"{\"mappings\":\"C,M\"}");
    }

    #[test]
    fn chunks_rewrite_first_mapping_relative_to_previous_state() {
        let (data, name_offset) = append_mapping_to_buffer(
            Vec::new(),
            0,
            SourceMapState::default(),
            SourceMapState {
                generated_column: 2,
                source_index: 3,
                original_line: 4,
                original_column: 5,
                original_name: 6,
                has_original_name: true,
                ..SourceMapState::default()
            },
            false,
        );
        let mut joiner = Joiner::default();
        append_source_map_chunk(
            &mut joiner,
            SourceMapState {
                generated_column: 1,
                source_index: 1,
                original_line: 1,
                original_column: 1,
                original_name: 2,
                ..SourceMapState::default()
            },
            SourceMapState {
                source_index: 10,
                original_line: 20,
                original_column: 30,
                original_name: 40,
                ..SourceMapState::default()
            },
            &MappingsBuffer {
                data,
                first_name_offset: name_offset,
            },
        );
        let output = joiner.done();
        let (generated, mut index) = decode_vlq(&output, 0);
        let (source, next) = decode_vlq(&output, index);
        index = next;
        let (line, next) = decode_vlq(&output, index);
        index = next;
        let (column, next) = decode_vlq(&output, index);
        index = next;
        let (name, _) = decode_vlq(&output, index);
        assert_eq!((generated, source, line, column, name), (1, 12, 23, 34, 44));
    }

    #[test]
    fn line_tables_convert_byte_offsets_to_utf16_columns() {
        let tables = generate_line_offset_tables("a🙂b\r\nxé".as_bytes(), 2);
        assert_eq!(tables.len(), 2);

        let mut builder = make_chunk_builder(None, tables, false);
        builder.add_source_mapping(crate::internal::logger::Loc { start: 0 }, "", b"");
        builder.add_source_mapping(crate::internal::logger::Loc { start: 5 }, "", b"x");
        let chunk = builder.generate_chunk(b"x");
        assert_eq!(chunk.buffer.data, b"AAAA,CAAG");
        assert_eq!(chunk.final_generated_column, 1);
    }

    #[test]
    fn utf8_rune_decoder_only_consumes_the_first_code_point() {
        assert_eq!(super::decode_utf8_rune(b"a trailing suffix"), ('a', 1));
        assert_eq!(
            super::decode_utf8_rune("🙂 trailing suffix".as_bytes()),
            ('🙂', 4)
        );
        assert_eq!(super::decode_utf8_rune(&[0xff, b'a']), ('\u{fffd}', 1));
        assert_eq!(super::decode_utf8_rune(&[0xc0, 0x80]), ('\u{fffd}', 1));
        assert_eq!(
            super::decode_utf8_rune(&[0xed, 0xa0, 0x80]),
            ('\u{fffd}', 1)
        );
        assert_eq!(
            super::decode_utf8_rune(&[0xf4, 0x90, 0x80, 0x80]),
            ('\u{fffd}', 1)
        );
    }

    #[test]
    fn chunk_builder_covers_unmapped_line_starts_and_quotes_names() {
        let tables = generate_line_offset_tables(b"abcdef", 1);
        let mut builder = make_chunk_builder(None, tables, false);
        builder.add_source_mapping(crate::internal::logger::Loc { start: 0 }, "first", b"");
        builder.add_source_mapping(
            crate::internal::logger::Loc { start: 5 },
            "second",
            "x🙂\nq".as_bytes(),
        );
        let chunk = builder.generate_chunk("x🙂\nq".as_bytes());
        assert_eq!(chunk.buffer.data, b"AAAAA;AAAA,CAAKC");
        assert_eq!(
            chunk.quoted_names,
            [b"\"first\"".to_vec(), b"\"second\"".to_vec()]
        );
        assert_eq!(chunk.final_generated_column, 1);
        assert!(!chunk.should_ignore);
        assert!(chunk.buffer.first_name_offset.is_valid());
    }

    #[test]
    fn chunk_builder_remaps_through_nested_source_maps() {
        let input_source_map = Arc::new(SourceMap {
            mappings: vec![Mapping {
                generated_line: 0,
                generated_column: 0,
                source_index: 7,
                original_line: 8,
                original_column: 9,
                original_name: crate::internal::ast::Index32::new(0),
            }],
            names: vec!["π".into()],
            ..SourceMap::default()
        });
        let tables = generate_line_offset_tables(b"intermediate", 1);
        let mut builder = make_chunk_builder(Some(input_source_map), tables, true);
        builder.add_source_mapping(
            crate::internal::logger::Loc { start: 0 },
            "intermediate",
            b"",
        );
        let chunk = builder.generate_chunk(b"");
        assert_eq!(chunk.buffer.data, b"AOQSA");
        assert_eq!(chunk.quoted_names, [br#""\u03C0""#.to_vec()]);
    }
}
