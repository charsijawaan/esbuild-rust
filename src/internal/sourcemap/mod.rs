// Port of upstream internal/sourcemap.

use crate::internal::ast::Index32;
use crate::internal::helpers::Joiner;

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
        vlq |= i64::from(digit) << shift;
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
        vlq |= i32::from(digit) << shift;
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

fn decode_utf8_rune(bytes: &[u8]) -> (char, usize) {
    match std::str::from_utf8(bytes) {
        Ok(text) => {
            let character = text.chars().next().expect("input is non-empty");
            (character, character.len_utf8())
        }
        Err(error) if error.valid_up_to() > 0 => {
            let valid = std::str::from_utf8(&bytes[..error.valid_up_to()])
                .expect("prefix reported as valid UTF-8");
            let character = valid.chars().next().expect("valid prefix is non-empty");
            (character, character.len_utf8())
        }
        Err(_) => ('\u{fffd}', 1),
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

#[cfg(test)]
mod tests {
    use super::{
        LineColumnOffset, Mapping, MappingsBuffer, SourceMap, SourceMapPieces, SourceMapShift,
        SourceMapState, append_mapping_to_buffer, append_source_map_chunk, decode_vlq,
        decode_vlq_utf16, encode_vlq,
    };
    use crate::internal::helpers::Joiner;

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
}
