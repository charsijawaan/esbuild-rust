//! Port of upstream `internal/js_parser`.

mod control_flow;
mod dead_control_flow;
mod deferred_errors;
mod define;
mod duplicate_properties;
mod global_name;
mod json;
mod options;
mod parser;
mod parser_core;
mod parser_types;
mod source_map;
mod standalone_helpers;
mod symbols;
mod syntax_arrow;
mod syntax_binding;
mod syntax_class;
mod syntax_expression;
mod syntax_function;
mod syntax_import;
mod syntax_literals;
mod syntax_new;
mod syntax_object;
mod syntax_private;
mod syntax_statement;
mod syntax_suffix;
mod syntax_super;
mod syntax_yield_await;

pub use define::parse_define_expr;
pub use global_name::parse_global_name;
pub use json::{JsonOptions, is_valid_json, parse_json};
pub use options::{Options, options_for_yarn_pnp, options_from_config};
pub use parser::parse;
pub use source_map::parse_source_map;
