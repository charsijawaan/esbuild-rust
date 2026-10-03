use std::{collections::HashMap, sync::Arc};

use super::{Message, build_option_error};
use crate::internal::config;

/// Property names map to replacement strings or `false` to prevent mangling.
pub type MangleCache = HashMap<String, serde_json::Value>;

pub(super) fn validate(cache: Option<&MangleCache>) -> Vec<Message> {
    let Some(cache) = cache else {
        return Vec::new();
    };
    let mut entries = cache.iter().collect::<Vec<_>>();
    entries.sort_by_key(|(name, _)| *name);
    entries
        .into_iter()
        .filter_map(|(name, value)| {
            if value.as_str() == Some("__proto__") {
                Some(build_option_error(format!(
                    "Invalid identifier name {name:?} in mangle cache"
                )))
            } else if value.is_string() || value == &serde_json::Value::Bool(false) {
                None
            } else {
                Some(build_option_error(format!(
                    "Expected {name:?} in mangle cache to map to either a string or false"
                )))
            }
        })
        .collect()
}

pub(super) fn to_internal(cache: Option<&MangleCache>) -> config::MangleCache {
    cache
        .into_iter()
        .flatten()
        .map(|(name, value)| {
            let value: Arc<dyn std::any::Any + Send + Sync> = if let Some(value) = value.as_str() {
                Arc::new(value.to_string())
            } else {
                Arc::new(false)
            };
            (name.clone(), value)
        })
        .collect()
}

pub(super) fn to_public(cache: &config::MangleCache) -> MangleCache {
    cache
        .iter()
        .map(|(name, value)| {
            let value = value
                .downcast_ref::<String>()
                .map_or(serde_json::Value::Bool(false), |value| {
                    serde_json::Value::String(value.clone())
                });
            (name.clone(), value)
        })
        .collect()
}
