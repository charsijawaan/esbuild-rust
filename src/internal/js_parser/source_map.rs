use std::cmp::Ordering;

use url::Url;

use crate::internal::{
    ast::Index32,
    helpers::utf16_to_string,
    js_ast::{ArrayExpr, Expr, ExprData, ObjectExpr, Property, StringExpr},
    js_lexer::JsonFlavor,
    logger::{LineColumnTracker, Loc, Log, MsgId, MsgKind, Range, Source},
    sourcemap::{Mapping, SourceContent, SourceMap, decode_vlq_utf16},
};

use super::{JsonOptions, parse_json};

#[derive(Clone)]
struct SourceMapSection {
    line_offset: i32,
    column_offset: i32,
    source_map: ObjectExpr,
}

/// Parse a version 3 source map, including indexed source-map sections.
///
/// # Panics
///
/// Panics if a source map exceeds esbuild's 32-bit index limits.
#[must_use]
#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
pub fn parse_source_map(log: Log, source: Source) -> Option<SourceMap> {
    let (expression, ok) = parse_json(
        log.clone(),
        source.clone(),
        JsonOptions {
            flavor: JsonFlavor::Json,
            error_suffix: " in source map".to_owned(),
            ..JsonOptions::default()
        },
    );
    if !ok {
        return None;
    }

    let mut tracker = LineColumnTracker::new(Some(&source));
    let Some(root) = expr_object(&expression).cloned() else {
        log.add_error(
            Some(&mut tracker),
            Range {
                loc: expression.loc,
                len: 0,
            },
            "Invalid source map",
        );
        return None;
    };

    let mut sections = Vec::new();
    let mut has_sections = false;
    for property in &root.properties {
        if property_key(property).as_deref() != Some(b"sections".as_slice()) {
            continue;
        }
        let Some(value) = expr_array(&property.value_or_nil) else {
            return source_map_error(
                &log,
                &mut tracker,
                property.value_or_nil.loc,
                "Expected \"sections\" to be an array",
            );
        };
        for item in &value.items {
            let Some(element) = expr_object(item) else {
                continue;
            };
            let mut line_offset = 0;
            let mut column_offset = 0;
            let mut section_source_map = None;
            for section_property in &element.properties {
                match property_key(section_property).as_deref() {
                    Some(b"offset") => {
                        let Some(offset) = expr_object(&section_property.value_or_nil) else {
                            return source_map_error(
                                &log,
                                &mut tracker,
                                section_property.value_or_nil.loc,
                                "Expected \"offset\" to be an object",
                            );
                        };
                        for offset_property in &offset.properties {
                            match property_key(offset_property).as_deref() {
                                Some(b"line") => {
                                    if let Some(value) = expr_number(&offset_property.value_or_nil)
                                    {
                                        line_offset = number_to_i32(value);
                                    }
                                }
                                Some(b"column") => {
                                    if let Some(value) = expr_number(&offset_property.value_or_nil)
                                    {
                                        column_offset = number_to_i32(value);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(b"map") => {
                        let Some(map) = expr_object(&section_property.value_or_nil) else {
                            return source_map_error(
                                &log,
                                &mut tracker,
                                section_property.value_or_nil.loc,
                                "Expected \"map\" to be an object",
                            );
                        };
                        section_source_map = Some(map.clone());
                    }
                    _ => {}
                }
            }
            if let Some(source_map) = section_source_map {
                sections.push(SourceMapSection {
                    line_offset,
                    column_offset,
                    source_map,
                });
            }
        }
        has_sections = true;
        break;
    }
    if !has_sections {
        sections.push(SourceMapSection {
            line_offset: 0,
            column_offset: 0,
            source_map: root,
        });
    }

    let mut sources = Vec::new();
    let mut sources_content = Vec::new();
    let mut names = Vec::new();
    let mut mappings = Vec::new();
    let mut generated_line = 0;
    let mut generated_column = 0;
    let mut need_sort = false;

    for section in sections {
        let mut sources_array = Vec::new();
        let mut sources_content_array = Vec::new();
        let mut names_array = Vec::new();
        let mut mappings_raw = Vec::new();
        let mut mappings_start = 0;
        let mut source_root = String::new();
        let mut has_version = false;

        for property in &section.source_map.properties {
            match property_key(property).as_deref() {
                Some(b"version") => {
                    has_version = expr_number(&property.value_or_nil) == Some(3.0);
                }
                Some(b"mappings") => {
                    if let Some(value) = expr_string(&property.value_or_nil) {
                        mappings_raw.clone_from(&value.value);
                        mappings_start = property.value_or_nil.loc.start + 1;
                    }
                }
                Some(b"sourceRoot") => {
                    if let Some(value) = expr_string(&property.value_or_nil) {
                        source_root =
                            String::from_utf8_lossy(&utf16_to_string(&value.value)).into_owned();
                    }
                }
                Some(b"sources") => {
                    if let Some(value) = expr_array(&property.value_or_nil) {
                        sources_array.clone_from(&value.items);
                    }
                }
                Some(b"sourcesContent") => {
                    if let Some(value) = expr_array(&property.value_or_nil) {
                        sources_content_array.clone_from(&value.items);
                    }
                }
                Some(b"names") => {
                    if let Some(value) = expr_array(&property.value_or_nil) {
                        names_array.clone_from(&value.items);
                    }
                }
                _ => {}
            }
        }
        if !has_version || mappings_raw.is_empty() || sources_array.is_empty() {
            continue;
        }
        if section.line_offset < generated_line
            || (section.line_offset == generated_line && section.column_offset < generated_column)
        {
            need_sort = true;
        }

        let line_offset = section.line_offset;
        let column_offset = section.column_offset;
        let source_offset =
            i32::try_from(sources.len()).expect("source maps must fit in 32-bit indexes");
        let name_offset =
            i32::try_from(names.len()).expect("source maps must fit in 32-bit indexes");
        generated_line = line_offset;
        generated_column = column_offset;
        let mut source_index = source_offset;
        let mut original_line = 0;
        let mut original_column = 0;
        let mut original_name = name_offset;
        let mut current = 0;
        let mut mapping_error = None;

        while current < mappings_raw.len() {
            if mappings_raw[current] == u16::from(b';') {
                generated_line += 1;
                generated_column = 0;
                current += 1;
                continue;
            }

            let (delta, width, decoded) = decode_vlq_utf16(&mappings_raw[current..]);
            if !decoded {
                mapping_error = Some(("Missing generated column".to_owned(), width));
                break;
            }
            if delta < 0 {
                need_sort = true;
            }
            generated_column += delta;
            if (generated_line == line_offset && generated_column < column_offset)
                || generated_column < 0
            {
                mapping_error = Some((
                    format!("Invalid generated column value: {generated_column}"),
                    width,
                ));
                break;
            }
            current += width;
            if current == mappings_raw.len() {
                break;
            }
            match mappings_raw[current] {
                value if value == u16::from(b',') => {
                    current += 1;
                    continue;
                }
                value if value == u16::from(b';') => continue,
                _ => {}
            }

            let (delta, width, decoded) = decode_vlq_utf16(&mappings_raw[current..]);
            if !decoded {
                mapping_error = Some(("Missing source index".to_owned(), width));
                break;
            }
            source_index += delta;
            if source_index < source_offset
                || source_index
                    >= source_offset
                        + i32::try_from(sources_array.len())
                            .expect("source maps must fit in 32-bit indexes")
            {
                mapping_error =
                    Some((format!("Invalid source index value: {source_index}"), width));
                break;
            }
            current += width;

            let (delta, width, decoded) = decode_vlq_utf16(&mappings_raw[current..]);
            if !decoded {
                mapping_error = Some(("Missing original line".to_owned(), width));
                break;
            }
            original_line += delta;
            if original_line < 0 {
                mapping_error = Some((
                    format!("Invalid original line value: {original_line}"),
                    width,
                ));
                break;
            }
            current += width;

            let (delta, width, decoded) = decode_vlq_utf16(&mappings_raw[current..]);
            if !decoded {
                mapping_error = Some(("Missing original column".to_owned(), width));
                break;
            }
            original_column += delta;
            if original_column < 0 {
                mapping_error = Some((
                    format!("Invalid original column value: {original_column}"),
                    width,
                ));
                break;
            }
            current += width;

            let mut original_name_index = Index32::default();
            let (delta, width, decoded) = decode_vlq_utf16(&mappings_raw[current..]);
            if decoded {
                original_name += delta;
                if original_name < name_offset
                    || original_name
                        >= name_offset
                            + i32::try_from(names_array.len())
                                .expect("source maps must fit in 32-bit indexes")
                {
                    mapping_error =
                        Some((format!("Invalid name index value: {original_name}"), width));
                    break;
                }
                original_name_index = Index32::new(
                    u32::try_from(original_name).expect("name indexes are non-negative"),
                );
                current += width;
            }

            if current < mappings_raw.len() {
                if mappings_raw[current] == u16::from(b',') {
                    current += 1;
                } else if mappings_raw[current] != u16::from(b';') {
                    let character = String::from_utf16_lossy(&mappings_raw[current..=current]);
                    mapping_error =
                        Some((format!("Invalid character after mapping: {character:?}"), 1));
                    break;
                }
            }
            mappings.push(Mapping {
                generated_line,
                generated_column,
                source_index,
                original_line,
                original_column,
                original_name: original_name_index,
            });
        }

        if let Some((error, length)) = mapping_error {
            let range = Range {
                loc: Loc {
                    start: mappings_start
                        + i32::try_from(current).expect("mapping offsets fit in i32"),
                },
                len: i32::try_from(length).unwrap_or_default(),
            };
            log.add_id(
                MsgId::SourceMapInvalidSourceMappings,
                MsgKind::Warning,
                Some(&mut tracker),
                range,
                format!("Bad \"mappings\" data in source map at character {current}: {error}"),
            );
            return None;
        }

        let source_url_prefix = if source_root.is_empty() {
            String::new()
        } else if let Some(index) = source_root.rfind('/') {
            source_root[..=index].to_owned()
        } else {
            format!("{source_root}/")
        };
        let base_url = if source.key_path.namespace == "file" {
            Url::from_file_path(&source.key_path.text).ok()
        } else {
            None
        };
        for item in &sources_array {
            if let Some(element) = expr_string(item) {
                let source_path = format!(
                    "{source_url_prefix}{}",
                    String::from_utf8_lossy(&utf16_to_string(&element.value))
                );
                sources.push(resolve_source_url(base_url.as_ref(), &source_path));
            } else {
                sources.push(String::new());
            }
        }

        if !sources_content_array.is_empty() {
            let target = usize::try_from(source_offset).expect("source offsets are non-negative");
            sources_content.resize(target, SourceContent::default());
            for (index, item) in sources_content_array.iter().enumerate() {
                if index == sources_array.len() {
                    break;
                }
                if let Some(element) = expr_string(item) {
                    let quoted = String::from_utf8_lossy(
                        source.text_for_range(source.range_of_string(item.loc)),
                    )
                    .into_owned();
                    sources_content.push(SourceContent {
                        value: element.value.clone(),
                        quoted,
                    });
                } else {
                    sources_content.push(SourceContent::default());
                }
            }
        }
        for item in &names_array {
            names.push(expr_string(item).map_or_else(String::new, |element| {
                String::from_utf8_lossy(&utf16_to_string(&element.value)).into_owned()
            }));
        }
    }

    if sources.is_empty() || mappings.is_empty() {
        return None;
    }
    if need_sort {
        mappings.sort_by(mapping_order);
    }
    Some(SourceMap {
        sources,
        sources_content,
        mappings,
        names,
    })
}

fn source_map_error(
    log: &Log,
    tracker: &mut LineColumnTracker,
    location: Loc,
    text: &str,
) -> Option<SourceMap> {
    log.add_error(
        Some(tracker),
        Range {
            loc: location,
            len: 0,
        },
        text,
    );
    None
}

fn property_key(property: &Property) -> Option<Vec<u8>> {
    expr_string(&property.key).map(|value| utf16_to_string(&value.value))
}

fn expr_string(expression: &Expr) -> Option<&StringExpr> {
    match expression.data.as_deref() {
        Some(ExprData::String(value)) => Some(value),
        _ => None,
    }
}

fn expr_array(expression: &Expr) -> Option<&ArrayExpr> {
    match expression.data.as_deref() {
        Some(ExprData::Array(value)) => Some(value),
        _ => None,
    }
}

fn expr_object(expression: &Expr) -> Option<&ObjectExpr> {
    match expression.data.as_deref() {
        Some(ExprData::Object(value)) => Some(value),
        _ => None,
    }
}

fn expr_number(expression: &Expr) -> Option<f64> {
    match expression.data.as_deref() {
        Some(ExprData::Number(value)) => Some(*value),
        _ => None,
    }
}

#[allow(clippy::cast_possible_truncation)]
fn number_to_i32(value: f64) -> i32 {
    value as i32
}

fn resolve_source_url(base: Option<&Url>, source_path: &str) -> String {
    if let Some(base) = base
        && let Ok(url) = base.join(source_path)
    {
        return url.to_string();
    }
    Url::parse(source_path).map_or_else(|_| source_path.to_owned(), |url| url.to_string())
}

fn mapping_order(left: &Mapping, right: &Mapping) -> Ordering {
    left.generated_line
        .cmp(&right.generated_line)
        .then_with(|| left.generated_column.cmp(&right.generated_column))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::parse_source_map;
    use crate::internal::logger::{DeferLogKind, Log, Path, Source};

    fn parse(
        text: &str,
        path: Option<&str>,
    ) -> (Option<crate::internal::sourcemap::SourceMap>, Log) {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(text.as_bytes()),
            key_path: path.map_or_else(Path::default, |path| Path {
                text: path.to_owned(),
                namespace: "file".to_owned(),
                ..Path::default()
            }),
            ..Source::default()
        };
        (parse_source_map(log.clone(), source), log)
    }

    #[test]
    fn parses_basic_source_maps() {
        let (map, log) = parse(
            r#"{"version":3,"sources":["in.js"],"sourcesContent":["let x"],"names":["x"],"mappings":"AAAAA"}"#,
            Some("/tmp/out.js.map"),
        );
        let map = map.expect("source map should parse");
        assert_eq!(map.sources, ["file:///tmp/in.js"]);
        assert_eq!(
            map.sources_content[0].value,
            "let x".encode_utf16().collect::<Vec<_>>()
        );
        assert_eq!(map.names, ["x"]);
        assert_eq!(map.mappings.len(), 1);
        assert_eq!(map.mappings[0].generated_line, 0);
        assert_eq!(map.mappings[0].source_index, 0);
        assert_eq!(map.mappings[0].original_name.get_index(), 0);
        assert!(log.done().is_empty());
    }

    #[test]
    fn parses_indexed_source_map_sections() {
        let (map, log) = parse(
            r#"{"version":3,"sections":[{"offset":{"line":2,"column":4},"map":{"version":3,"sources":["a.js"],"names":[],"mappings":"AAAA"}}]}"#,
            None,
        );
        let map = map.expect("indexed source map should parse");
        assert_eq!(map.mappings[0].generated_line, 2);
        assert_eq!(map.mappings[0].generated_column, 4);
        assert_eq!(map.sources, ["a.js"]);
        assert!(log.done().is_empty());
    }

    #[test]
    fn rejects_invalid_shapes_and_mapping_data() {
        for text in [
            "null",
            r#"{"sections":{}}"#,
            r#"{"sections":[{"offset":[],"map":{}}]}"#,
            r#"{"sections":[{"offset":{},"map":[]}]}"#,
            r#"{"version":3,"sources":["a"],"mappings":"!"}"#,
            r#"{"version":3,"sources":["a"],"mappings":"ACAA"}"#,
        ] {
            let (map, log) = parse(text, None);
            assert!(map.is_none(), "{text}");
            assert!(!log.done().is_empty(), "{text}");
        }
    }

    #[test]
    fn ignores_empty_or_wrong_version_maps() {
        for text in [
            r#"{"version":2,"sources":["a"],"mappings":"AAAA"}"#,
            r#"{"version":3,"sources":[],"mappings":"AAAA"}"#,
            r#"{"version":3,"sources":["a"],"mappings":""}"#,
        ] {
            let (map, log) = parse(text, None);
            assert!(map.is_none());
            assert!(log.done().is_empty());
        }
    }
}
