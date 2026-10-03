//! Port of the pinned `cmd/esbuild/stdio_protocol.go` packet codec.
//!
//! The seven wire tags are null, boolean, integer, string, bytes, array, and
//! object. Integer encoding keeps the low 32 bits; decoding returns an unsigned
//! 32-bit value as an `i64`, as Go's `int(value)` does on a 64-bit host. Strings
//! and object keys retain arbitrary bytes because Go strings need not be UTF-8.
//! Object encoding sorts keys by bytes, matching Go's `sort.Strings`.
//!
//! Corresponding JavaScript: `lib/shared/stdio_protocol.ts:250-460`.
//! Malformed packets return errors instead of reproducing Go's out-of-bounds
//! and unknown-tag panics. Lengths that cannot fit on the wire are rejected.

use std::{collections::BTreeMap, error::Error, fmt};

/// A Go string-keyed map. Byte keys preserve invalid UTF-8 and Go's key order.
pub type Object = BTreeMap<Vec<u8>, Value>;

/// The exact value types supported by the service protocol.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    String(Vec<u8>),
    Bytes(Vec<u8>),
    Array(Vec<Self>),
    Object(Object),
}

impl Value {
    /// Construct a wire string without requiring valid UTF-8.
    #[must_use]
    pub fn string(bytes: impl Into<Vec<u8>>) -> Self {
        Self::String(bytes.into())
    }

    /// Construct an object with string or byte keys. Duplicate keys keep the
    /// last value, just like assignments into a Go map during packet decoding.
    #[must_use]
    pub fn object<K: AsRef<[u8]>>(entries: impl IntoIterator<Item = (K, Self)>) -> Self {
        Self::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key.as_ref().to_vec(), value))
                .collect(),
        )
    }

    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(value) => Some(*value),
            _ => None,
        }
    }

    /// Return a text view only when the wire string is valid UTF-8.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(self.as_string_bytes()?).ok()
    }

    #[must_use]
    pub fn as_string_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    /// Return bytes only for tag 4, keeping byte arrays distinct from strings.
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(value) => Some(value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Array(value) => Some(value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_object(&self) -> Option<&Object> {
        match self {
            Self::Object(value) => Some(value),
            _ => None,
        }
    }

    #[must_use]
    pub fn get(&self, key: impl AsRef<[u8]>) -> Option<&Self> {
        self.as_object()?.get(key.as_ref())
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i32> for Value {
    fn from(value: i32) -> Self {
        Self::Int(i64::from(value))
    }
}

impl From<u32> for Value {
    fn from(value: u32) -> Self {
        Self::Int(i64::from(value))
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::String(value.as_bytes().to_vec())
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::String(value.into_bytes())
    }
}

impl From<Vec<u8>> for Value {
    fn from(value: Vec<u8>) -> Self {
        Self::Bytes(value)
    }
}

impl From<Vec<Self>> for Value {
    fn from(value: Vec<Self>) -> Self {
        Self::Array(value)
    }
}

/// The request flag is the inverse of the packed header's least significant
/// bit. Only the low 31 bits of `id` survive encoding, matching both upstreams.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub id: u32,
    pub is_request: bool,
    pub value: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    UnexpectedEnd,
    InvalidTag(u8),
    TrailingBytes(usize),
    LengthOverflow,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEnd => formatter.write_str("Invalid packet: unexpected end"),
            Self::InvalidTag(tag) => write!(formatter, "Invalid packet: unknown value tag {tag}"),
            Self::TrailingBytes(count) => {
                write!(formatter, "Invalid packet: {count} trailing bytes")
            }
            Self::LengthOverflow => formatter.write_str("Packet length exceeds 32 bits"),
        }
    }
}

impl Error for ProtocolError {}

/// Port of `readUint32`: an incomplete word consumes nothing.
#[must_use]
pub fn read_uint32(bytes: &[u8]) -> Option<(u32, &[u8])> {
    let (word, rest) = bytes.split_first_chunk::<4>()?;
    Some((u32::from_le_bytes(*word), rest))
}

/// Port of `writeUint32`.
pub fn write_uint32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

/// Port of `readLengthPrefixedSlice`. Incomplete input is left untouched.
/// Zero-length frames are valid here; packet validation happens separately.
#[must_use]
pub fn read_length_prefixed_slice(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let (length, rest) = read_uint32(bytes)?;
    let length = usize::try_from(length).ok()?;
    if rest.len() < length {
        return None;
    }
    Some(rest.split_at(length))
}

/// Frame raw bytes, including the service's initial version greeting.
///
/// # Errors
/// Returns `LengthOverflow` if the payload exceeds the 32-bit wire length.
pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    let length = wire_length(payload.len())?;
    let mut bytes = Vec::new();
    write_uint32(&mut bytes, length);
    bytes.extend_from_slice(payload);
    Ok(bytes)
}

/// Encode a complete length-prefixed packet, as upstream `encodePacket` does.
///
/// # Errors
/// Returns `LengthOverflow` if a string, byte array, container count, or the
/// final packet payload exceeds the 32-bit wire length.
pub fn encode_packet(packet: &Packet) -> Result<Vec<u8>, ProtocolError> {
    let mut bytes = Vec::new();
    write_uint32(&mut bytes, 0);
    write_uint32(&mut bytes, (packet.id << 1) | u32::from(!packet.is_request));
    encode_value(&packet.value, &mut bytes)?;
    let length = wire_length(bytes.len() - 4)?;
    bytes[..4].copy_from_slice(&length.to_le_bytes());
    Ok(bytes)
}

/// Decode an unframed packet payload, as upstream `decodePacket` does.
///
/// # Errors
/// Rejects truncated words or values, unknown tags, impossible container
/// counts, and any trailing bytes after the root value.
pub fn decode_packet(bytes: &[u8]) -> Result<Packet, ProtocolError> {
    let (header, mut rest) = read_uint32(bytes).ok_or(ProtocolError::UnexpectedEnd)?;
    let value = decode_value(&mut rest)?;
    if !rest.is_empty() {
        return Err(ProtocolError::TrailingBytes(rest.len()));
    }
    Ok(Packet {
        id: header >> 1,
        is_request: header & 1 == 0,
        value,
    })
}

fn wire_length(length: usize) -> Result<u32, ProtocolError> {
    u32::try_from(length).map_err(|_| ProtocolError::LengthOverflow)
}

fn encode_slice(value: &[u8], bytes: &mut Vec<u8>) -> Result<(), ProtocolError> {
    write_uint32(bytes, wire_length(value.len())?);
    bytes.extend_from_slice(value);
    Ok(())
}

fn encode_value(value: &Value, bytes: &mut Vec<u8>) -> Result<(), ProtocolError> {
    match value {
        Value::Null => bytes.push(0),
        Value::Bool(value) => bytes.extend_from_slice(&[1, u8::from(*value)]),
        Value::Int(value) => {
            bytes.push(2);
            bytes.extend_from_slice(&value.to_le_bytes()[..4]);
        }
        Value::String(value) => {
            bytes.push(3);
            encode_slice(value, bytes)?;
        }
        Value::Bytes(value) => {
            bytes.push(4);
            encode_slice(value, bytes)?;
        }
        Value::Array(value) => {
            bytes.push(5);
            write_uint32(bytes, wire_length(value.len())?);
            for item in value {
                encode_value(item, bytes)?;
            }
        }
        Value::Object(value) => {
            bytes.push(6);
            write_uint32(bytes, wire_length(value.len())?);
            for (key, item) in value {
                encode_slice(key, bytes)?;
                encode_value(item, bytes)?;
            }
        }
    }
    Ok(())
}

fn decode_value(bytes: &mut &[u8]) -> Result<Value, ProtocolError> {
    let (&kind, rest) = bytes.split_first().ok_or(ProtocolError::UnexpectedEnd)?;
    *bytes = rest;
    match kind {
        0 => Ok(Value::Null),
        1 => {
            let (&value, rest) = bytes.split_first().ok_or(ProtocolError::UnexpectedEnd)?;
            *bytes = rest;
            Ok(Value::Bool(value != 0))
        }
        2 => {
            let (value, rest) = read_uint32(bytes).ok_or(ProtocolError::UnexpectedEnd)?;
            *bytes = rest;
            Ok(Value::Int(i64::from(value)))
        }
        3 | 4 => {
            let (value, rest) =
                read_length_prefixed_slice(bytes).ok_or(ProtocolError::UnexpectedEnd)?;
            *bytes = rest;
            Ok(if kind == 3 {
                Value::String(value.to_vec())
            } else {
                Value::Bytes(value.to_vec())
            })
        }
        5 | 6 => {
            let (count, rest) = read_uint32(bytes).ok_or(ProtocolError::UnexpectedEnd)?;
            *bytes = rest;
            // Even null array elements occupy one byte. Object entries also
            // require a four-byte key length. Check this before allocating so
            // a tiny malformed packet cannot request a multi-gigabyte vector.
            let minimum_entry_size = if kind == 5 { 1 } else { 5 };
            if usize::try_from(count).map_or(true, |count| count > rest.len() / minimum_entry_size)
            {
                return Err(ProtocolError::UnexpectedEnd);
            }
            if kind == 5 {
                let mut value = Vec::new();
                for _ in 0..count {
                    value.push(decode_value(bytes)?);
                }
                Ok(Value::Array(value))
            } else {
                let mut value = Object::new();
                for _ in 0..count {
                    let (key, rest) =
                        read_length_prefixed_slice(bytes).ok_or(ProtocolError::UnexpectedEnd)?;
                    let key = key.to_vec();
                    *bytes = rest;
                    value.insert(key, decode_value(bytes)?);
                }
                Ok(Value::Object(value))
            }
        }
        _ => Err(ProtocolError::InvalidTag(kind)),
    }
}

/// Incremental framing from `cmd/esbuild/service.go:131-161` and
/// `lib/shared/common.ts:510-541`. Complete frames are cloned so callers can
/// retain them while the stream buffer is compacted or extended.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    bytes: Vec<u8>,
    offset: usize,
}

impl FrameDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) {
        if self.offset != 0 {
            self.bytes.copy_within(self.offset.., 0);
            self.bytes.truncate(self.bytes.len() - self.offset);
            self.offset = 0;
        }
        self.bytes.extend_from_slice(chunk);
    }

    /// Extract the next complete frame, leaving partial input buffered.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        let (payload, rest) = read_length_prefixed_slice(self.pending_bytes())?;
        let offset = self.bytes.len() - rest.len();
        let payload = payload.to_vec();
        self.offset = offset;
        if self.offset == self.bytes.len() {
            self.bytes.clear();
            self.offset = 0;
        }
        Some(payload)
    }

    #[must_use]
    pub fn pending_bytes(&self) -> &[u8] {
        &self.bytes[self.offset..]
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending_bytes().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::{Path, PathBuf},
        process::{Command, Output},
        time::{SystemTime, UNIX_EPOCH},
    };

    const UPSTREAM_REVISION: &str = "6ff1d8b0d8c134e867a397eef39702a223ebef9e";

    fn packet(id: u32, is_request: bool, value: Value) -> Packet {
        Packet {
            id,
            is_request,
            value,
        }
    }

    // The same inputs are supplied to the unchanged pinned Go codec below.
    // The first ten are also supplied to the unchanged JavaScript codec.
    fn interoperability_cases() -> Vec<Packet> {
        vec![
            packet(0, true, Value::Null),
            packet(0, false, false.into()),
            packet(1, true, true.into()),
            packet(0x1234_5678, false, 0x0123_4567_u32.into()),
            packet(42, false, "π😀\0".into()),
            packet(7, true, vec![0_u8, 127, 128, 255].into()),
            packet(
                9,
                false,
                vec![
                    Value::Null,
                    false.into(),
                    true.into(),
                    0x8000_0000_u32.into(),
                    "".into(),
                    Value::Bytes(vec![]),
                    Value::Array(vec![]),
                    Value::Object(Object::new()),
                ]
                .into(),
            ),
            packet(
                31,
                true,
                Value::object([
                    ("z", Value::Null),
                    (
                        "a",
                        Value::Array(vec![7_u32.into(), "λ".into(), true.into()]),
                    ),
                    ("中", Value::Bytes(vec![0, 255])),
                    ("", Value::object([("key", false.into())])),
                ]),
            ),
            packet(0x8000_0003, false, (-1_i32).into()),
            packet(u32::MAX, true, 0x1_0000_0001_i64.into()),
            packet(33, true, Value::string(vec![255, 0, 192, 128])),
            packet(
                34,
                false,
                Value::object([
                    (vec![255], "raw key".into()),
                    (b"a".to_vec(), Value::Null),
                    ("é".as_bytes().to_vec(), false.into()),
                ]),
            ),
        ]
    }

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write;
        let mut text = String::new();
        for byte in bytes {
            write!(text, "{byte:02x}").unwrap();
        }
        text
    }

    fn unhex(text: &str) -> Vec<u8> {
        assert_eq!(text.len() % 2, 0);
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn primitive_wire_bytes_match_the_original_format() {
        let cases = [
            (packet(0, true, Value::Null), "050000000000000000"),
            (packet(0, false, false.into()), "06000000010000000100"),
            (packet(1, true, true.into()), "06000000020000000101"),
            (
                packet(0x1234_5678, false, 0x0123_4567_u32.into()),
                "09000000f1ac68240267452301",
            ),
            (
                packet(42, false, "π😀\0".into()),
                "10000000550000000307000000cf80f09f988000",
            ),
            (
                packet(7, true, vec![0_u8, 127, 128, 255].into()),
                "0d0000000e0000000404000000007f80ff",
            ),
        ];
        for (packet, expected) in cases {
            let framed = unhex(expected);
            assert_eq!(encode_packet(&packet).unwrap(), framed);
            let (payload, rest) = read_length_prefixed_slice(&framed).unwrap();
            assert!(rest.is_empty());
            assert_eq!(decode_packet(payload).unwrap(), packet);
        }
    }

    #[test]
    fn nested_array_and_object_wire_bytes_match_go() {
        let cases = interoperability_cases();
        // Captured with the pinned Go encodePacket, not with the Rust encoder.
        let expected = [
            "27000000130000000508000000000100010102000000800300000000040000000005000000000600000000",
            "470000003e0000000604000000000000000601000000030000006b657901000100000061050300000002070000000302000000cebb0101010000007a0003000000e4b8ad040200000000ff",
        ];
        for (packet, expected) in cases[6..8].iter().zip(expected) {
            let framed = unhex(expected);
            assert_eq!(encode_packet(packet).unwrap(), framed);
            assert_eq!(decode_packet(&framed[4..]).unwrap(), *packet);
        }
    }

    #[test]
    fn integer_encoding_keeps_low_bits_and_decoding_is_unsigned() {
        let cases = [
            (i64::MIN, 0, "00000000"),
            (-1, 4_294_967_295, "ffffffff"),
            (0, 0, "00000000"),
            (2_147_483_648, 2_147_483_648, "00000080"),
            (4_294_967_295, 4_294_967_295, "ffffffff"),
            (4_294_967_297, 1, "01000000"),
            (i64::MAX, 4_294_967_295, "ffffffff"),
        ];
        for (input, decoded, wire) in cases {
            let encoded = encode_packet(&packet(0, true, Value::Int(input))).unwrap();
            assert_eq!(hex(&encoded[9..]), wire);
            assert_eq!(
                decode_packet(&encoded[4..]).unwrap().value.as_int(),
                Some(decoded)
            );
        }
    }

    #[test]
    fn request_header_uses_the_low_bit_and_truncates_the_id_high_bit() {
        let cases = [
            (0, true, 0, 0),
            (0, false, 1, 0),
            (0x7fff_ffff, true, 0xffff_fffe, 0x7fff_ffff),
            (0x7fff_ffff, false, 0xffff_ffff, 0x7fff_ffff),
            (0x8000_0000, true, 0, 0),
            (0x8000_0003, false, 7, 3),
            (u32::MAX, true, 0xffff_fffe, 0x7fff_ffff),
        ];
        for (id, is_request, header, decoded_id) in cases {
            let encoded = encode_packet(&packet(id, is_request, Value::Null)).unwrap();
            assert_eq!(read_uint32(&encoded[4..]).unwrap().0, header);
            let decoded = decode_packet(&encoded[4..]).unwrap();
            assert_eq!(decoded.id, decoded_id);
            assert_eq!(decoded.is_request, is_request);
        }
    }

    #[test]
    fn nonzero_boolean_bytes_are_true() {
        for byte in [1, 2, 127, 128, 255] {
            let decoded = decode_packet(&[0, 0, 0, 0, 1, byte]).unwrap();
            assert_eq!(decoded.value, Value::Bool(true));
            assert_eq!(
                hex(&encode_packet(&decoded).unwrap()),
                "06000000000000000101"
            );
        }
    }

    #[test]
    fn decoding_unsorted_and_duplicate_keys_keeps_the_last_value() {
        let payload = unhex("000000000603000000010000007a020700000001000000610101010000007a00");
        let decoded = decode_packet(&payload).unwrap();
        assert_eq!(
            decoded.value,
            Value::object([("a", true.into()), ("z", Value::Null)])
        );
        assert_eq!(
            hex(&encode_packet(&decoded).unwrap()),
            "1600000000000000060200000001000000610101010000007a00",
        );
    }

    #[test]
    fn go_string_bytes_and_byte_arrays_are_distinct_and_lossless() {
        let cases = interoperability_cases();
        for packet in &cases[10..] {
            let encoded = encode_packet(packet).unwrap();
            assert_eq!(decode_packet(&encoded[4..]).unwrap(), *packet);
        }
        let invalid = &cases[10].value;
        assert!(invalid.as_str().is_none());
        assert_eq!(
            invalid.as_string_bytes(),
            Some([255, 0, 192, 128].as_slice())
        );
        assert!(invalid.as_bytes().is_none());
        let bytes = Value::Bytes(vec![255, 0, 192, 128]);
        assert!(bytes.as_str().is_none());
        assert_ne!(
            encode_packet(&cases[10]).unwrap(),
            encode_packet(&packet(33, true, bytes)).unwrap()
        );
        let raw_object = &cases[11].value;
        assert_eq!(raw_object.get([255]).unwrap().as_str(), Some("raw key"));
        assert_eq!(raw_object.get("é").unwrap().as_bool(), Some(false));
    }

    #[test]
    fn key_sorting_uses_go_byte_order_including_non_bmp_keys() {
        let value = Value::object([("\u{10000}", true.into()), ("\u{e000}", false.into())]);
        let encoded = encode_packet(&packet(0, true, value)).unwrap();
        assert_eq!(
            hex(&encoded),
            "1c00000000000000060200000003000000ee8080010004000000f09080800101",
        );
    }

    #[test]
    fn all_complete_fixtures_roundtrip_including_integer_normalization() {
        for packet in interoperability_cases() {
            let encoded = encode_packet(&packet).unwrap();
            let mut normalized = packet.clone();
            normalized.id &= 0x7fff_ffff;
            match normalized.value {
                Value::Int(-1) => normalized.value = Value::Int(4_294_967_295),
                Value::Int(4_294_967_297) => normalized.value = Value::Int(1),
                _ => {}
            }
            let decoded = decode_packet(&encoded[4..]).unwrap();
            assert_eq!(decoded, normalized);
            assert_eq!(encode_packet(&decoded).unwrap(), encoded);
        }
    }

    #[test]
    fn deeply_nested_empty_containers_roundtrip() {
        let mut value = Value::Object(Object::new());
        for _ in 0..128 {
            value = Value::object([("nested", Value::Array(vec![value]))]);
        }
        let packet = packet(5, false, value);
        let encoded = encode_packet(&packet).unwrap();
        assert_eq!(decode_packet(&encoded[4..]).unwrap(), packet);
    }

    #[test]
    fn every_truncated_fixture_is_rejected_and_incomplete_frames_wait() {
        for packet in interoperability_cases() {
            let framed = encode_packet(&packet).unwrap();
            for end in 0..framed.len() {
                assert!(read_length_prefixed_slice(&framed[..end]).is_none());
            }
            let payload = &framed[4..];
            for end in 0..payload.len() {
                assert!(
                    decode_packet(&payload[..end]).is_err(),
                    "prefix {end}: {}",
                    hex(payload)
                );
            }
        }
    }

    #[test]
    fn unknown_tags_and_trailing_bytes_are_rejected() {
        for tag in 7..=255 {
            assert_eq!(
                decode_packet(&[0, 0, 0, 0, tag]),
                Err(ProtocolError::InvalidTag(tag))
            );
            let mut array = unhex("000000000501000000");
            array.push(tag);
            assert_eq!(decode_packet(&array), Err(ProtocolError::InvalidTag(tag)));
            let mut object = unhex("00000000060100000000000000");
            object.push(tag);
            assert_eq!(decode_packet(&object), Err(ProtocolError::InvalidTag(tag)));
        }
        for extra in [vec![0], vec![255], vec![0, 0]] {
            let mut payload = unhex("0000000000");
            payload.extend_from_slice(&extra);
            assert_eq!(
                decode_packet(&payload),
                Err(ProtocolError::TrailingBytes(extra.len()))
            );
        }
    }

    #[test]
    fn oversized_lengths_and_counts_do_not_allocate_from_untrusted_counts() {
        for tag in 3..=6 {
            assert_eq!(
                decode_packet(&[0, 0, 0, 0, tag, 255, 255, 255, 255]),
                Err(ProtocolError::UnexpectedEnd),
            );
        }
        for malformed in [
            "00000000030200000061",
            "00000000040200000061",
            "00000000050200000000",
            "00000000060100000000000000",
            "000000000601000000ffffffff00",
            "0000000006010000000100000061",
        ] {
            assert_eq!(
                decode_packet(&unhex(malformed)),
                Err(ProtocolError::UnexpectedEnd)
            );
        }
        assert!(read_length_prefixed_slice(&[255, 255, 255, 255]).is_none());
        if usize::BITS > 32 {
            assert_eq!(wire_length(usize::MAX), Err(ProtocolError::LengthOverflow));
        }
    }

    #[test]
    fn words_and_length_prefixed_slices_are_little_endian_and_consume_exactly() {
        let bytes = unhex("7856341200ff");
        assert_eq!(
            read_uint32(&bytes),
            Some((0x1234_5678, [0, 255].as_slice()))
        );
        for end in 0..4 {
            assert!(read_uint32(&bytes[..end]).is_none());
        }
        let mut written = vec![255];
        write_uint32(&mut written, 0x1234_5678);
        assert_eq!(hex(&written), "ff78563412");
        let bytes = unhex("0200000061626364");
        assert_eq!(
            read_length_prefixed_slice(&bytes),
            Some((b"ab".as_slice(), b"cd".as_slice()))
        );
        assert_eq!(
            read_length_prefixed_slice(&[0, 0, 0, 0, 42]),
            Some(([].as_slice(), [42].as_slice()))
        );
    }

    #[test]
    fn the_version_greeting_is_a_raw_frame() {
        assert_eq!(
            hex(&encode_frame(b"0.28.1").unwrap()),
            "06000000302e32382e31"
        );
        assert_eq!(hex(&encode_frame(b"").unwrap()), "00000000");
    }

    #[test]
    fn framing_preserves_all_packets_across_every_chunk_boundary() {
        let packets = interoperability_cases();
        let expected = [
            b"0.28.1".to_vec(),
            encode_packet(&packets[0]).unwrap()[4..].to_vec(),
            vec![],
            encode_packet(&packets[7]).unwrap()[4..].to_vec(),
        ];
        let stream: Vec<u8> = expected
            .iter()
            .flat_map(|payload| encode_frame(payload).unwrap())
            .collect();
        for boundary in 0..=stream.len() {
            let mut decoder = FrameDecoder::new();
            let mut actual = vec![];
            for chunk in [&stream[..boundary], &stream[boundary..]] {
                decoder.push(chunk);
                while let Some(frame) = decoder.next_frame() {
                    actual.push(frame);
                }
            }
            assert_eq!(actual, expected, "chunk boundary {boundary}");
            assert!(decoder.is_empty());
        }
        let mut decoder = FrameDecoder::new();
        let mut actual = vec![];
        for byte in stream {
            decoder.push(&[byte]);
            while let Some(frame) = decoder.next_frame() {
                actual.push(frame);
            }
        }
        assert_eq!(actual, expected);
        assert!(decoder.is_empty());
    }

    #[test]
    fn framing_retains_partial_input_and_returned_frames_own_their_bytes() {
        let mut decoder = FrameDecoder::new();
        decoder.push(&unhex("0200000061620300000063"));
        let first = decoder.next_frame().unwrap();
        let partial = unhex("0300000063");
        for _ in 0..3 {
            assert!(decoder.next_frame().is_none());
            assert_eq!(decoder.pending_bytes(), partial);
        }
        decoder.push(b"de");
        assert_eq!(decoder.next_frame().unwrap(), b"cde");
        assert!(decoder.is_empty());
        assert_eq!(first, b"ab");
    }

    fn checked_output(command: &mut Command) -> Output {
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{command:?}: {}",
            String::from_utf8_lossy(&output.stderr),
        );
        output
    }

    struct ScratchDirectory(PathBuf);

    impl Drop for ScratchDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // External oracles are opt-in. This copies the original Go codec unchanged
    // and compiles the original TypeScript codec with the pinned Go binary.
    // No generated source, fixture, or dependency is added to the repository.
    #[test]
    #[ignore = "requires ESBUILD_RS_PROTOCOL_UPSTREAM, Go, Node, and the pinned upstream binary"]
    fn matches_pinned_upstream_go_and_javascript_protocol() {
        let checkout =
            PathBuf::from(std::env::var_os("ESBUILD_RS_PROTOCOL_UPSTREAM").expect(
                "set ESBUILD_RS_PROTOCOL_UPSTREAM to a checkout of the UPSTREAM.md revision",
            ));
        let revision = checked_output(
            Command::new("git")
                .arg("-C")
                .arg(&checkout)
                .args(["rev-parse", "HEAD"]),
        );
        assert_eq!(
            String::from_utf8(revision.stdout).unwrap().trim(),
            UPSTREAM_REVISION
        );
        let directory = std::env::temp_dir().join(format!(
            "esbuild-rust-protocol-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        fs::create_dir(&directory).unwrap();
        let scratch = ScratchDirectory(directory);
        fs::copy(
            checkout.join("cmd/esbuild/stdio_protocol.go"),
            scratch.0.join("stdio_protocol.go"),
        )
        .unwrap();
        fs::write(scratch.0.join("oracle.go"), GO_ORACLE).unwrap();
        let go = checked_output(Command::new("go").current_dir(&scratch.0).args([
            "run",
            "stdio_protocol.go",
            "oracle.go",
        ]));
        let go = String::from_utf8(go.stdout).unwrap();
        let go_lines: Vec<_> = go.lines().collect();
        let cases = interoperability_cases();
        assert_eq!(go_lines.len(), cases.len());
        for (case, expected) in cases.iter().zip(&go_lines) {
            let encoded = encode_packet(case).unwrap();
            assert_eq!(hex(&encoded), *expected, "Go fixture: {case:?}");
            let decoded = decode_packet(&unhex(expected)[4..]).unwrap();
            assert_eq!(encode_packet(&decoded).unwrap(), encoded);
        }
        let binary = checkout.join(if cfg!(windows) {
            "esbuild.exe"
        } else {
            "esbuild"
        });
        compile_javascript_protocol(&checkout, &scratch.0, &binary);
        fs::write(scratch.0.join("oracle.cjs"), JS_ORACLE).unwrap();
        let js = checked_output(
            Command::new("node")
                .current_dir(&scratch.0)
                .arg("oracle.cjs"),
        );
        let js = String::from_utf8(js.stdout).unwrap();
        let js_lines: Vec<_> = js.lines().collect();
        assert_eq!(js_lines.len(), 11);
        assert_eq!(js_lines[..10], go_lines[..10]);
        // JS preserves object insertion order on encode, while Go sorts keys.
        // Both directions must agree on the value even when the bytes differ.
        let unsorted = unhex(js_lines[10]);
        assert_ne!(js_lines[10], go_lines[7]);
        let decoded = decode_packet(&unsorted[4..]).unwrap();
        assert_eq!(decoded, cases[7]);
        assert_eq!(hex(&encode_packet(&decoded).unwrap()), go_lines[7]);
    }

    fn compile_javascript_protocol(checkout: &Path, scratch: &Path, binary: &Path) {
        let version = checked_output(Command::new(binary).arg("--version"));
        assert_eq!(String::from_utf8(version.stdout).unwrap().trim(), "0.28.1");
        checked_output(
            Command::new(binary)
                .arg(checkout.join("lib/shared/stdio_protocol.ts"))
                .args(["--platform=node", "--format=cjs"])
                .arg(format!(
                    "--outfile={}",
                    scratch.join("stdio_protocol.cjs").display()
                )),
        );
    }

    const GO_ORACLE: &str = r#"package main
import (
    "bytes"
    "encoding/hex"
    "fmt"
)
func main() {
    cases := []packet{
        {id: 0, isRequest: true, value: nil},
        {id: 0, isRequest: false, value: false},
        {id: 1, isRequest: true, value: true},
        {id: 0x12345678, isRequest: false, value: int(0x01234567)},
        {id: 42, isRequest: false, value: "π😀\x00"},
        {id: 7, isRequest: true, value: []byte{0, 127, 128, 255}},
        {id: 9, isRequest: false, value: []interface{}{
            nil, false, true, int(0x80000000), "", []byte{}, []interface{}{}, map[string]interface{}{},
        }},
        {id: 31, isRequest: true, value: map[string]interface{}{
            "z": nil, "a": []interface{}{int(7), "λ", true}, "中": []byte{0, 255},
            "": map[string]interface{}{"key": false},
        }},
        {id: 0x80000003, isRequest: false, value: int(-1)},
        {id: 0xffffffff, isRequest: true, value: int(0x100000001)},
        {id: 33, isRequest: true, value: string([]byte{255, 0, 192, 128})},
        {id: 34, isRequest: false, value: map[string]interface{}{
            string([]byte{255}): "raw key", "a": nil, "é": false,
        }},
    }
    for _, p := range cases {
        encoded := encodePacket(p)
        decoded, ok := decodePacket(encoded[4:])
        if !ok || !bytes.Equal(encoded, encodePacket(decoded)) { panic("Go roundtrip failed") }
        if integer, ok := p.value.(int); ok && decoded.value.(int) != int(uint32(integer)) {
            panic("Go integer normalization failed")
        }
        for end := 0; end < len(encoded)-4; end++ {
            if valid(encoded[4:4+end]) { panic("Go accepted truncated packet") }
        }
        trailing := append(append([]byte{}, encoded[4:]...), byte(0))
        if valid(trailing) { panic("Go accepted trailing bytes") }
        fmt.Println(hex.EncodeToString(encoded))
    }
    for tag := 7; tag < 256; tag++ {
        if valid([]byte{0, 0, 0, 0, byte(tag)}) { panic("Go accepted unknown tag") }
    }
}
func valid(bytes []byte) (ok bool) {
    defer func() { if recover() != nil { ok = false } }()
    _, ok = decodePacket(bytes)
    return
}
"#;

    const JS_ORACLE: &str = r"const assert = require('node:assert/strict')
const protocol = require('./stdio_protocol.cjs')
const cases = [
  { id: 0, isRequest: true, value: null },
  { id: 0, isRequest: false, value: false },
  { id: 1, isRequest: true, value: true },
  { id: 0x12345678, isRequest: false, value: 0x01234567 },
  { id: 42, isRequest: false, value: 'π😀\0' },
  { id: 7, isRequest: true, value: new Uint8Array([0, 127, 128, 255]) },
  { id: 9, isRequest: false, value: [null, false, true, 0x80000000, '', new Uint8Array(), [], {}] },
  // Sorted insertion order matches Go; JavaScript does not sort object keys.
  { id: 31, isRequest: true, value: { '': { key: false }, a: [7, 'λ', true], z: null, '中': new Uint8Array([0, 255]) } },
  { id: 0x80000003, isRequest: false, value: -1 },
  { id: 0xffffffff, isRequest: true, value: 0x100000001 },
]
for (const p of cases) {
  const encoded = protocol.encodePacket(p)
  const decoded = protocol.decodePacket(encoded.subarray(4))
  assert.equal(decoded.id, p.id & 0x7fffffff)
  assert.equal(decoded.isRequest, p.isRequest)
  if (typeof p.value === 'number') assert.equal(decoded.value, p.value >>> 0)
  else assert.deepEqual(decoded.value, p.value)
  assert.deepEqual(protocol.encodePacket(decoded), encoded)
  for (let end = 0; end < encoded.length - 4; end++) {
    assert.throws(() => protocol.decodePacket(encoded.subarray(4, 4 + end)))
  }
  assert.throws(() => protocol.decodePacket(new Uint8Array([...encoded.subarray(4), 0])))
  console.log(Buffer.from(encoded).toString('hex'))
}
for (let tag = 7; tag < 256; tag++) {
  assert.throws(() => protocol.decodePacket(new Uint8Array([0, 0, 0, 0, tag])))
}
const unsorted = { id: 31, isRequest: true, value: {
  z: null, a: [7, 'λ', true], '中': new Uint8Array([0, 255]), '': { key: false },
} }
console.log(Buffer.from(protocol.encodePacket(unsorted)).toString('hex'))
";
}
