//! Port of `internal/helpers`.

mod bitset;
mod comment;
mod dataurl;
mod float;
mod glob;
mod hash;
mod joiner;
mod mime;
mod quote;
mod strings;
mod typos;
mod utf;

pub use bitset::BitSet;
pub use comment::escape_closing_tag;
pub use dataurl::{encode_string_as_percent_escaped_data_url, encode_string_as_shortest_data_url};
pub use float::{F64, lerp, max2, max3, min2, min3};
pub use glob::{GlobPart, GlobWildcard, glob_pattern_to_string, parse_glob_pattern};
pub use hash::{hash_combine, hash_combine_string};
pub use joiner::Joiner;
pub use mime::mime_type_by_extension;
pub use quote::{quote_for_json, quote_single};
pub use strings::{
    string_array_arrays_equal, string_array_to_quoted_comma_separated_string, string_arrays_equal,
};
pub use typos::TypoDetector;
pub use utf::{
    REPLACEMENT_CHARACTER, contains_non_bmp_code_point, contains_non_bmp_code_point_utf16,
    decode_wtf8_rune, string_to_utf16, utf16_equals_utf16, utf16_equals_wtf8, utf16_to_string,
    utf16_to_string_with_validation,
};
