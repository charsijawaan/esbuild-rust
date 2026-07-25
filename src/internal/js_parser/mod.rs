//! Port of upstream `internal/js_parser`.

mod global_name;
mod json;

pub use global_name::parse_global_name;
pub use json::{JsonOptions, is_valid_json, parse_json};
