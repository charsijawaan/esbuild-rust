//! Port of upstream `internal/js_parser`.

mod control_flow;
mod dead_control_flow;
mod deferred_errors;
mod duplicate_properties;
mod global_name;
mod json;
mod options;
mod parser_core;
mod source_map;
mod symbols;

pub use global_name::parse_global_name;
pub use json::{JsonOptions, is_valid_json, parse_json};
pub use options::{Options, options_for_yarn_pnp, options_from_config};
pub use source_map::parse_source_map;
