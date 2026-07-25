#![allow(dead_code)]

use std::collections::HashMap;

use crate::internal::{
    js_ast::{ExprData, Property, PropertyFlags, PropertyKind},
    logger::Loc,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DuplicatePropertiesIn {
    Object,
    Class,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DuplicateProperty {
    pub(crate) key: Vec<u16>,
    pub(crate) original_loc: Loc,
    pub(crate) duplicate_loc: Loc,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum KeyKind {
    #[default]
    Missing,
    Normal,
    Get,
    Set,
    GetAndSet,
}

#[derive(Clone, Copy, Debug, Default)]
struct ExistingKey {
    loc: Loc,
    kind: KeyKind,
}

pub(crate) fn find_duplicate_properties(
    properties: &[Property],
    context: DuplicatePropertiesIn,
) -> Vec<DuplicateProperty> {
    if properties.len() < 2 {
        return Vec::new();
    }

    let mut instance_keys = HashMap::<Vec<u16>, ExistingKey>::new();
    let mut static_keys = HashMap::<Vec<u16>, ExistingKey>::new();
    let mut duplicates = Vec::new();

    for property in properties {
        if property.kind == PropertyKind::Spread {
            continue;
        }
        let Some(ExprData::String(string)) = property.key.data.as_deref() else {
            continue;
        };

        let keys = if property.flags.contains(PropertyFlags::IS_STATIC) {
            &mut static_keys
        } else {
            &mut instance_keys
        };
        let key = string.value.clone();
        let previous = keys.get(&key).copied().unwrap_or_default();
        let mut next = ExistingKey {
            kind: match property.kind {
                PropertyKind::Getter => KeyKind::Get,
                PropertyKind::Setter => KeyKind::Set,
                _ => KeyKind::Normal,
            },
            loc: property.key.loc,
        };

        let is_special = match context {
            DuplicatePropertiesIn::Object => key == "__proto__".encode_utf16().collect::<Vec<_>>(),
            DuplicatePropertiesIn::Class => key == "constructor".encode_utf16().collect::<Vec<_>>(),
        };
        if previous.kind != KeyKind::Missing && !is_special {
            if matches!(
                (previous.kind, next.kind),
                (KeyKind::Get, KeyKind::Set) | (KeyKind::Set, KeyKind::Get)
            ) {
                next.kind = KeyKind::GetAndSet;
            } else {
                duplicates.push(DuplicateProperty {
                    key: key.clone(),
                    original_loc: previous.loc,
                    duplicate_loc: property.key.loc,
                });
            }
        }

        keys.insert(key, next);
    }

    duplicates
}

#[cfg(test)]
mod tests {
    use super::{DuplicatePropertiesIn, find_duplicate_properties};
    use crate::internal::{
        js_ast::{Expr, ExprData, Property, PropertyFlags, PropertyKind, StringExpr},
        logger::Loc,
    };

    fn property(name: &str, kind: PropertyKind, flags: PropertyFlags, loc: i32) -> Property {
        Property {
            key: Expr::new(
                Loc { start: loc },
                ExprData::String(StringExpr {
                    value: name.encode_utf16().collect(),
                    ..StringExpr::default()
                }),
            ),
            kind,
            flags,
            ..Property::default()
        }
    }

    #[test]
    fn getter_setter_pairs_are_not_duplicates_but_third_accessors_are() {
        let properties = [
            property("x", PropertyKind::Getter, PropertyFlags::NONE, 1),
            property("x", PropertyKind::Setter, PropertyFlags::NONE, 2),
            property("x", PropertyKind::Getter, PropertyFlags::NONE, 3),
        ];
        let duplicates = find_duplicate_properties(&properties, DuplicatePropertiesIn::Object);
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].original_loc.start, 2);
        assert_eq!(duplicates[0].duplicate_loc.start, 3);
    }

    #[test]
    fn static_and_instance_keys_are_tracked_separately() {
        let properties = [
            property("x", PropertyKind::Field, PropertyFlags::NONE, 1),
            property("x", PropertyKind::Field, PropertyFlags::IS_STATIC, 2),
        ];
        assert!(find_duplicate_properties(&properties, DuplicatePropertiesIn::Class).is_empty());
    }

    #[test]
    fn special_keys_match_upstream_warning_exclusions() {
        let object = [
            property("__proto__", PropertyKind::Field, PropertyFlags::NONE, 1),
            property("__proto__", PropertyKind::Field, PropertyFlags::NONE, 2),
        ];
        assert!(find_duplicate_properties(&object, DuplicatePropertiesIn::Object).is_empty());

        let class = [
            property("constructor", PropertyKind::Method, PropertyFlags::NONE, 1),
            property("constructor", PropertyKind::Method, PropertyFlags::NONE, 2),
        ];
        assert!(find_duplicate_properties(&class, DuplicatePropertiesIn::Class).is_empty());
    }
}
