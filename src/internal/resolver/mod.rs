//! Port of foundational types from upstream `internal/resolver`.

use crate::internal::logger::{Range, Source};

#[derive(Clone, Debug, Default)]
pub struct SideEffectsData {
    pub source: Option<Source>,
    pub plugin_name: String,
    pub range: Range,
    pub is_side_effects_array_in_json: bool,
}
