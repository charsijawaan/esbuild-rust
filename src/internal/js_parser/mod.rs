//! Port of upstream `internal/js_parser`.

mod control_flow;
mod global_name;
mod json;
mod options;
mod source_map;

pub use global_name::parse_global_name;
pub use json::{JsonOptions, is_valid_json, parse_json};
pub use options::{Options, options_for_yarn_pnp, options_from_config};
pub use source_map::parse_source_map;
