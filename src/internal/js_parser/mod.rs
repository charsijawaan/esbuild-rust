//! Port of upstream `internal/js_parser`.

mod control_flow;
mod dead_control_flow;
mod deferred_errors;
mod define;
mod duplicate_properties;
mod global_name;
mod json;
mod options;
mod parser_core;
mod parser_types;
mod source_map;
mod standalone_helpers;
mod symbols;
mod syntax_expression;
mod syntax_import;
mod syntax_literals;
mod syntax_suffix;

pub use define::parse_define_expr;
pub use global_name::parse_global_name;
pub use json::{JsonOptions, is_valid_json, parse_json};
pub use options::{Options, options_for_yarn_pnp, options_from_config};
pub use source_map::parse_source_map;
