//! Port of upstream `internal/js_parser`.

mod global_name;
mod json;
mod source_map;

pub use global_name::parse_global_name;
pub use json::{JsonOptions, is_valid_json, parse_json};
pub use source_map::parse_source_map;
