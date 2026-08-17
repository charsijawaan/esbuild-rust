//! Port of upstream `internal/js_printer`.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use crate::internal::ast::{
    AssertOrWithKeyword, INVALID_REF, ImportKind, ImportPhase, ImportRecord, ImportRecordFlags,
    Ref, SymbolFlags, SymbolKind,
};
use crate::internal::compat::JsFeature;
use crate::internal::config::{LegalComments, MetafileFormat};
use crate::internal::helpers::{escape_closing_tag, quote_for_json, utf16_to_string};
use crate::internal::js_ast::{
    AssignTarget, Ast, Binding, BindingData, BlockStmt, CallKind, ConstValueKind, Expr, ExprData,
    ExprStmt, IfStmt, LocalKind, OpCode, OptionalChain, Precedence, PropertyFlags, PropertyKind,
    ReturnStmt, SideEffects, Stmt, StmtData, StringExpr, fold_binary_operator,
    inline_primitives_into_template, is_identifier_es5_and_es_next, is_optional_chain,
    join_with_comma, make_helper_context, should_fold_binary_operator_when_minifying,
    string_to_equivalent_number_value, to_boolean_with_side_effects, to_int32,
    to_number_without_side_effects,
};
use crate::internal::renamer::Renamer;
use crate::internal::sourcemap::{
    Chunk as SourceMapChunk, ChunkBuilder, LineOffsetTable, SourceMap, make_chunk_builder,
};

const HEX_CHARS: &[u8; 16] = b"0123456789ABCDEF";
const FIRST_ASCII: u32 = 0x20;
const LAST_ASCII: u32 = 0x7e;
const FIRST_HIGH_SURROGATE: u16 = 0xd800;
const LAST_HIGH_SURROGATE: u16 = 0xdbff;
const FIRST_LOW_SURROGATE: u16 = 0xdc00;
const LAST_LOW_SURROGATE: u16 = 0xdfff;

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub unsupported_features: JsFeature,
    pub line_limit: usize,
    pub indent: usize,
    pub minify_syntax: bool,
    pub minify_whitespace: bool,
    pub ascii_only: bool,
    pub legal_comments: LegalComments,
    pub needs_metafile: bool,
    pub metafile_format: MetafileFormat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequireOrImportMeta {
    pub wrapper_ref: Ref,
    pub exports_ref: Ref,
    pub is_wrapper_async: bool,
}

#[derive(Clone, Copy)]
pub struct LinkerOptions<'a> {
    pub require_or_import_meta_for_source: &'a dyn Fn(u32) -> RequireOrImportMeta,
    pub const_values: Option<&'a HashMap<Ref, crate::internal::js_ast::ConstValue>>,
    pub ts_enums: Option<&'a HashMap<Ref, HashMap<String, crate::internal::js_ast::TsEnumValue>>>,
    pub to_common_js_ref: Ref,
    pub to_esm_ref: Ref,
    pub runtime_require_ref: Ref,
}

impl Default for RequireOrImportMeta {
    fn default() -> Self {
        Self {
            wrapper_ref: INVALID_REF,
            exports_ref: INVALID_REF,
            is_wrapper_async: false,
        }
    }
}

#[must_use]
/// Appends an identifier, escaping non-ASCII code points for ASCII-only output.
///
/// # Panics
///
/// Panics when an astral code point cannot be represented because Unicode code
/// point escapes are unsupported by the configured target.
pub fn quote_identifier(
    mut output: Vec<u8>,
    name: &str,
    unsupported_features: JsFeature,
) -> Vec<u8> {
    let mut ascii_start = 0;
    let mut is_ascii = false;
    for (index, character) in name.char_indices() {
        let code_point = character as u32;
        if (FIRST_ASCII..=LAST_ASCII).contains(&code_point) {
            if !is_ascii {
                is_ascii = true;
                ascii_start = index;
            }
            continue;
        }
        if is_ascii {
            output.extend_from_slice(&name.as_bytes()[ascii_start..index]);
            is_ascii = false;
        }
        if let Ok(code_unit) = u16::try_from(code_point) {
            push_u16_escape(&mut output, code_unit);
        } else if !unsupported_features.contains(JsFeature::UNICODE_ESCAPES) {
            output.extend_from_slice(format!("\\u{{{code_point:X}}}").as_bytes());
        } else {
            panic!("Internal error: Cannot encode identifier: Unicode escapes are unsupported");
        }
    }
    if is_ascii {
        output.extend_from_slice(&name.as_bytes()[ascii_start..]);
    }
    output
}

#[must_use]
pub fn quote_utf16(data: &[u16], options: Options, allow_backtick: bool) -> Vec<u8> {
    let allow_backtick = allow_backtick
        && !options
            .unsupported_features
            .contains(JsFeature::TEMPLATE_LITERAL);
    let mut single_cost = 0;
    let mut double_cost = 0;
    let mut backtick_cost = 0;
    for (index, code_unit) in data.iter().copied().enumerate() {
        match code_unit {
            10 if options.minify_syntax => backtick_cost -= 1,
            39 => single_cost += 1,
            34 => double_cost += 1,
            96 => backtick_cost += 1,
            36 if data.get(index + 1) == Some(&u16::from(b'{')) => backtick_cost += 1,
            _ => {}
        }
    }

    let quote = if double_cost > single_cost {
        if allow_backtick && single_cost > backtick_cost {
            b'`'
        } else {
            b'\''
        }
    } else if allow_backtick && double_cost > backtick_cost {
        b'`'
    } else {
        b'"'
    };
    quote_utf16_with_quote(data, options, quote)
}

fn quote_utf16_with_quote(data: &[u16], options: Options, quote: u8) -> Vec<u8> {
    let mut output = vec![quote];
    print_unquoted_utf16(&mut output, data, quote, options);
    output.push(quote);
    output
}

#[allow(clippy::too_many_lines)]
fn print_unquoted_utf16(output: &mut Vec<u8>, text: &[u16], quote: u8, options: Options) {
    let mut index = 0;
    let mut start_line_length = output
        .iter()
        .rev()
        .position(|byte| *byte == b'\n')
        .map_or(output.len(), |position| position);
    if options.line_limit > 0 {
        start_line_length = start_line_length.min(options.line_limit);
    }

    while index < text.len() {
        if options.line_limit > 0 && start_line_length + index >= options.line_limit {
            output.extend_from_slice(b"\\\n");
            start_line_length = start_line_length.saturating_sub(options.line_limit);
        }

        let code_unit = text[index];
        index += 1;
        match code_unit {
            0 => {
                if text
                    .get(index)
                    .is_some_and(|next| (u16::from(b'0')..=u16::from(b'9')).contains(next))
                {
                    output.extend_from_slice(b"\\x00");
                } else {
                    output.extend_from_slice(b"\\0");
                }
            }
            7 => output.extend_from_slice(b"\\x07"),
            8 => output.extend_from_slice(b"\\b"),
            12 => output.extend_from_slice(b"\\f"),
            10 if quote == b'`' => {
                start_line_length = 0;
                output.push(b'\n');
            }
            10 => output.extend_from_slice(b"\\n"),
            13 => output.extend_from_slice(b"\\r"),
            11 => output.extend_from_slice(b"\\v"),
            0x1b => output.extend_from_slice(b"\\x1B"),
            92 => output.extend_from_slice(b"\\\\"),
            47 => {
                if !options
                    .unsupported_features
                    .contains(JsFeature::INLINE_SCRIPT)
                    && index >= 2
                    && text[index - 2] == u16::from(b'<')
                    && index + 6 <= text.len()
                    && text[index..index + 6]
                        .iter()
                        .copied()
                        .map(|unit| u8::try_from(unit).unwrap_or_default().to_ascii_lowercase())
                        .eq(b"script".iter().copied())
                {
                    output.push(b'\\');
                }
                output.push(b'/');
            }
            39 if quote == b'\'' => output.extend_from_slice(b"\\'"),
            34 if quote == b'"' => output.extend_from_slice(b"\\\""),
            96 if quote == b'`' => output.extend_from_slice(b"\\`"),
            36 if quote == b'`' && text.get(index) == Some(&u16::from(b'{')) => {
                output.extend_from_slice(b"\\$");
            }
            0x2028 => output.extend_from_slice(b"\\u2028"),
            0x2029 => output.extend_from_slice(b"\\u2029"),
            0xfeff => output.extend_from_slice(b"\\uFEFF"),
            code_unit if u32::from(code_unit) <= LAST_ASCII => {
                output.push(u8::try_from(code_unit).expect("ASCII code unit"));
            }
            high if (FIRST_HIGH_SURROGATE..=LAST_HIGH_SURROGATE).contains(&high) => {
                if let Some(&low) = text.get(index)
                    && (FIRST_LOW_SURROGATE..=LAST_LOW_SURROGATE).contains(&low)
                {
                    index += 1;
                    let code_point = ((u32::from(high) - u32::from(FIRST_HIGH_SURROGATE)) << 10)
                        + (u32::from(low) - u32::from(FIRST_LOW_SURROGATE))
                        + 0x1_0000;
                    if options.ascii_only {
                        if options
                            .unsupported_features
                            .contains(JsFeature::UNICODE_ESCAPES)
                        {
                            push_u16_escape(output, high);
                            push_u16_escape(output, low);
                        } else {
                            output.extend_from_slice(format!("\\u{{{code_point:X}}}").as_bytes());
                        }
                    } else if let Some(character) = char::from_u32(code_point) {
                        let mut bytes = [0; 4];
                        output.extend_from_slice(character.encode_utf8(&mut bytes).as_bytes());
                    }
                    continue;
                }
                push_u16_escape(output, high);
            }
            code_unit
                if (FIRST_LOW_SURROGATE..=LAST_LOW_SURROGATE).contains(&code_unit)
                    || (options.ascii_only && code_unit > u16::from(u8::MAX)) =>
            {
                push_u16_escape(output, code_unit);
            }
            code_unit if options.ascii_only => {
                output.extend_from_slice(b"\\x");
                output.push(HEX_CHARS[usize::from(code_unit >> 4)]);
                output.push(HEX_CHARS[usize::from(code_unit & 15)]);
            }
            code_unit => {
                if let Some(character) = char::from_u32(u32::from(code_unit)) {
                    let mut bytes = [0; 4];
                    output.extend_from_slice(character.encode_utf8(&mut bytes).as_bytes());
                }
            }
        }
    }
}

fn push_u16_escape(output: &mut Vec<u8>, code_unit: u16) {
    output.extend_from_slice(b"\\u");
    output.push(HEX_CHARS[usize::from(code_unit >> 12)]);
    output.push(HEX_CHARS[usize::from((code_unit >> 8) & 15)]);
    output.push(HEX_CHARS[usize::from((code_unit >> 4) & 15)]);
    output.push(HEX_CHARS[usize::from(code_unit & 15)]);
}

#[must_use]
pub fn format_number(
    value: f64,
    level: Precedence,
    options: Options,
    with_nesting: bool,
) -> String {
    if value.is_nan() {
        if with_nesting {
            let text = if options.minify_whitespace {
                "0/0"
            } else {
                "0 / 0"
            };
            return if level >= Precedence::Multiply {
                format!("({text})")
            } else {
                text.into()
            };
        }
        return "NaN".into();
    }

    if value.is_infinite() {
        let is_negative = value.is_sign_negative();
        let wrap = (with_nesting && level >= Precedence::Multiply)
            || (options.minify_syntax && level > Precedence::Multiply)
            || (is_negative && level > Precedence::Prefix);
        let magnitude = if !options.minify_syntax && !with_nesting {
            "Infinity"
        } else if options.minify_whitespace {
            "1/0"
        } else {
            "1 / 0"
        };
        let text = if is_negative {
            format!("-{magnitude}")
        } else {
            magnitude.into()
        };
        return if wrap { format!("({text})") } else { text };
    }

    let magnitude = format_non_negative_float(value.abs(), options.minify_whitespace);
    if !value.is_sign_negative() {
        magnitude
    } else if level > Precedence::Prefix {
        format!("(-{magnitude})")
    } else {
        format!("-{magnitude}")
    }
}

#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
/// Formats a finite, non-negative JavaScript number using the shortest of the
/// decimal, exponent, and (when minifying) hexadecimal representations.
///
/// # Panics
///
/// Panics if an internally formatted exponent or string length cannot fit the
/// platform integer types. These values originate from one `f64` and are
/// necessarily small.
pub fn format_non_negative_float(value: f64, minify_whitespace: bool) -> String {
    debug_assert!(value.is_finite() && !value.is_sign_negative());
    if value < 1000.0 && value.fract() == 0.0 {
        return format!("{value:.0}");
    }

    let mut result = value.to_string();
    simplify_exponent(&mut result);
    if let Some(dot) = result.find('.') {
        if dot == 1 && result.starts_with('0') {
            let mut after_dot = 2;
            if minify_whitespace {
                result.remove(0);
                after_dot -= 1;
            }
            if result.as_bytes().get(after_dot) == Some(&b'0') {
                let first_non_zero = result.as_bytes()[after_dot..]
                    .iter()
                    .position(|byte| *byte != b'0')
                    .map(|offset| after_dot + offset);
                if let Some(first_non_zero) = first_non_zero {
                    let remaining = &result[first_non_zero..];
                    let exponent = i64::try_from(after_dot).expect("float length")
                        - i64::try_from(first_non_zero).expect("float length")
                        - i64::try_from(remaining.len()).expect("float length");
                    let alternative = format!("{remaining}e{exponent}");
                    if alternative.len() < result.len() {
                        result = alternative;
                    }
                }
            }
        } else if let Some(exponent_index) = result.rfind('e') {
            let integer = &result[..dot];
            let fraction = &result[dot + 1..exponent_index];
            let exponent = result[exponent_index + 1..]
                .parse::<i64>()
                .expect("formatted float exponent")
                - i64::try_from(fraction.len()).expect("float length");
            let alternative = if (0..=2).contains(&exponent) {
                format!(
                    "{integer}{fraction}{}",
                    "0".repeat(usize::try_from(exponent).expect("small non-negative exponent"))
                )
            } else {
                format!("{integer}{fraction}e{exponent}")
            };
            if alternative.len() <= result.len() {
                result = alternative;
            }
        }
    } else if result.ends_with('0') {
        let remaining_len = result.trim_end_matches('0').len();
        let remaining = &result[..remaining_len];
        let exponent = result.len() - remaining_len;
        let alternative = format!("{remaining}e{exponent}");
        if alternative.len() < result.len() {
            result = alternative;
        }
    }

    if minify_whitespace
        && (1_000_000_000_000.0..=18_446_744_073_709_549_568.0).contains(&value)
        && value.fract() == 0.0
    {
        let hexadecimal = format!("0x{:x}", value as u64);
        if hexadecimal.len() < result.len() {
            result = hexadecimal;
        }
    }
    result
}

fn bigint_to_decimal(value: &str) -> String {
    let (digits, radix) = if let Some(digits) = value
        .strip_prefix("0b")
        .or_else(|| value.strip_prefix("0B"))
    {
        (digits, 2)
    } else if let Some(digits) = value
        .strip_prefix("0o")
        .or_else(|| value.strip_prefix("0O"))
    {
        (digits, 8)
    } else if let Some(digits) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        (digits, 16)
    } else {
        (value, 10)
    };
    num_bigint::BigUint::parse_bytes(digits.as_bytes(), radix)
        .expect("lexer only produces valid big integer tokens")
        .to_str_radix(10)
}

fn bigint_can_be_exact_number(decimal: &str) -> bool {
    decimal
        .parse::<f64>()
        .is_ok_and(|number| number.is_finite() && format!("{number:.0}") == decimal)
}

fn simplify_exponent(result: &mut String) {
    let Some(exponent_index) = result.find('e') else {
        return;
    };
    let exponent = result[exponent_index + 1..]
        .parse::<i64>()
        .expect("formatted float exponent");
    result.truncate(exponent_index + 1);
    result.push_str(&exponent.to_string());
}

#[must_use]
pub fn print_expr(expr: &Expr, renamer: &dyn Renamer, options: Options) -> Vec<u8> {
    let mut printer = Printer {
        output: Vec::new(),
        renamer,
        options,
        linker_options: None,
        module_type_is_esm: false,
        with_nesting: 0,
        forbid_in: false,
        space_after_unary_not: false,
        expr_comments: None,
        printed_expr_comments: HashSet::new(),
        suppress_parenthesized_object_or_class: false,
        for_of_init_start: false,
        suppress_next_indent: false,
        stmt_start: None,
        arrow_expr_start: None,
        export_default_start: None,
        indent: options.indent,
        import_records: &[],
        has_legal_comment: HashSet::new(),
        extracted_legal_comments: Vec::new(),
        json_metadata_imports: Vec::new(),
        source_map_builder: None,
    };
    printer.print_expr_at(expr, Precedence::Lowest);
    printer.output
}

#[derive(Clone, Debug, Default)]
pub struct PrintResult {
    pub js: Vec<u8>,
    pub extracted_legal_comments: Vec<String>,
    pub json_metadata_imports: Vec<String>,
    pub source_map_chunk: SourceMapChunk,
}

/// Prints all live AST parts as JavaScript.
///
/// # Panics
///
/// Panics if the tree contains an AST node whose printer case has not yet been
/// ported.
#[must_use]
pub fn print(tree: &Ast, renamer: &dyn Renamer, options: Options) -> PrintResult {
    print_internal(tree, renamer, options, None, None)
}

/// Print JavaScript using linker metadata to lower internal imports.
#[must_use]
pub fn print_linked(
    tree: &Ast,
    renamer: &dyn Renamer,
    options: Options,
    linker_options: LinkerOptions<'_>,
) -> PrintResult {
    print_internal(tree, renamer, options, Some(linker_options), None)
}

/// Print JavaScript while recording a reusable per-file source-map chunk.
///
/// # Panics
///
/// Panics if AST locations fall outside the supplied line-offset tables.
#[must_use]
pub fn print_with_source_map(
    tree: &Ast,
    renamer: &dyn Renamer,
    options: Options,
    input_source_map: Option<Arc<SourceMap>>,
    line_offset_tables: impl Into<Arc<[LineOffsetTable]>>,
) -> PrintResult {
    print_internal(
        tree,
        renamer,
        options,
        None,
        Some(make_chunk_builder(
            input_source_map,
            line_offset_tables,
            options.ascii_only,
        )),
    )
}

/// Print linked JavaScript while recording a reusable per-file source-map chunk.
#[must_use]
pub fn print_linked_with_source_map(
    tree: &Ast,
    renamer: &dyn Renamer,
    options: Options,
    linker_options: LinkerOptions<'_>,
    input_source_map: Option<Arc<SourceMap>>,
    line_offset_tables: impl Into<Arc<[LineOffsetTable]>>,
) -> PrintResult {
    print_internal(
        tree,
        renamer,
        options,
        Some(linker_options),
        Some(make_chunk_builder(
            input_source_map,
            line_offset_tables,
            options.ascii_only,
        )),
    )
}

fn print_internal<'a>(
    tree: &'a Ast,
    renamer: &'a dyn Renamer,
    options: Options,
    linker_options: Option<LinkerOptions<'a>>,
    source_map_builder: Option<ChunkBuilder>,
) -> PrintResult {
    let mut printer = Printer {
        output: Vec::new(),
        renamer,
        options,
        linker_options,
        module_type_is_esm: tree.module_type_data.module_type.is_esm(),
        with_nesting: 0,
        forbid_in: false,
        space_after_unary_not: false,
        expr_comments: Some(&tree.expr_comments),
        printed_expr_comments: HashSet::new(),
        suppress_parenthesized_object_or_class: false,
        for_of_init_start: false,
        suppress_next_indent: false,
        stmt_start: None,
        arrow_expr_start: None,
        export_default_start: None,
        indent: options.indent,
        import_records: &tree.import_records,
        has_legal_comment: HashSet::new(),
        extracted_legal_comments: Vec::new(),
        json_metadata_imports: Vec::new(),
        source_map_builder,
    };
    if !tree.hashbang.is_empty() {
        printer.output.extend_from_slice(b"#!");
        printer.output.extend_from_slice(tree.hashbang.as_bytes());
        printer.print_newline();
    }
    for directive in &tree.directives {
        printer.print_indent();
        printer.output.extend(quote_utf16(
            &directive.encode_utf16().collect::<Vec<_>>(),
            options,
            false,
        ));
        printer.output.push(b';');
        printer.print_newline();
    }
    let statements = tree
        .parts
        .iter()
        .enumerate()
        .flat_map(|(part_index, part)| {
            part.statements
                .iter()
                .map(move |statement| (statement, part_index))
        })
        .collect::<Vec<_>>();
    printer.print_statements_with_parts(&statements);
    let source_map_chunk = printer
        .source_map_builder
        .take()
        .map_or_else(SourceMapChunk::default, |builder| {
            builder.generate_chunk(&printer.output)
        });
    PrintResult {
        js: printer.output,
        extracted_legal_comments: printer.extracted_legal_comments,
        json_metadata_imports: printer.json_metadata_imports,
        source_map_chunk,
    }
}

struct Printer<'a> {
    output: Vec<u8>,
    renamer: &'a dyn Renamer,
    options: Options,
    linker_options: Option<LinkerOptions<'a>>,
    module_type_is_esm: bool,
    with_nesting: usize,
    forbid_in: bool,
    space_after_unary_not: bool,
    expr_comments: Option<&'a HashMap<crate::internal::logger::Loc, Vec<String>>>,
    printed_expr_comments: HashSet<crate::internal::logger::Loc>,
    suppress_parenthesized_object_or_class: bool,
    for_of_init_start: bool,
    suppress_next_indent: bool,
    stmt_start: Option<usize>,
    arrow_expr_start: Option<usize>,
    export_default_start: Option<usize>,
    indent: usize,
    import_records: &'a [ImportRecord],
    has_legal_comment: HashSet<String>,
    extracted_legal_comments: Vec<String>,
    json_metadata_imports: Vec<String>,
    source_map_builder: Option<ChunkBuilder>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SubstitutionContext {
    CallTargetOrTemplateTag,
    DeleteTarget,
}

impl Printer<'_> {
    fn substitute_imported_enum(&self, expression: &Expr) -> Option<Expr> {
        let linker_options = self.linker_options?;
        let ts_enums = linker_options.ts_enums?;
        let (target, name) = match expression.data.as_deref()? {
            ExprData::Dot(dot) if dot.optional_chain == OptionalChain::None => {
                (&dot.target, dot.name.clone())
            }
            ExprData::Index(index) if index.optional_chain == OptionalChain::None => {
                let ExprData::String(name) = index.index.data.as_deref()? else {
                    return None;
                };
                (
                    &index.target,
                    String::from_utf8_lossy(&utf16_to_string(&name.value)).into_owned(),
                )
            }
            _ => return None,
        };
        let ExprData::ImportIdentifier(identifier) = target.data.as_deref()? else {
            return None;
        };
        let canonical = self.renamer.canonical_ref_for_symbol(identifier.reference);
        let values = ts_enums
            .get(&canonical)
            .or_else(|| ts_enums.get(&identifier.reference))?;
        let value = values.get(&name)?;
        let value = if value.is_string {
            Expr::new(
                expression.loc,
                ExprData::String(StringExpr {
                    value: value.string.clone(),
                    ..StringExpr::default()
                }),
            )
        } else {
            Expr::new(expression.loc, ExprData::Number(value.number))
        };
        if name.contains("*/") {
            Some(value)
        } else {
            Some(Expr::new(
                expression.loc,
                ExprData::InlinedEnum(crate::internal::js_ast::InlinedEnumExpr {
                    value,
                    comment: name,
                }),
            ))
        }
    }

    fn late_constant_fold(&self, expression: &Expr) -> (Expr, bool) {
        match expression.data.as_deref() {
            Some(ExprData::ImportIdentifier(identifier)) => {
                let reference = self.renamer.canonical_ref_for_symbol(identifier.reference);
                if let Some(value) = self
                    .linker_options
                    .and_then(|options| options.const_values)
                    .and_then(|values| values.get(&reference))
                    && value.kind != ConstValueKind::None
                {
                    return (
                        crate::internal::js_ast::const_value_to_expr(expression.loc, value),
                        true,
                    );
                }
            }
            Some(ExprData::Dot(_) | ExprData::Index(_)) => {
                if let Some(replacement) = self.substitute_imported_enum(expression) {
                    return (replacement, true);
                }
            }
            Some(ExprData::Unary(unary)) => {
                let (value, changed) = self.late_constant_fold(&unary.value);
                if changed {
                    if let Some(number) = to_number_without_side_effects(value.data.as_deref()) {
                        let folded = match unary.op {
                            OpCode::UnaryPositive => Some(number),
                            OpCode::UnaryNegative => Some(-number),
                            OpCode::UnaryComplement => Some(f64::from(!to_int32(number))),
                            _ => None,
                        };
                        if let Some(folded) = folded {
                            return (Expr::new(expression.loc, ExprData::Number(folded)), true);
                        }
                    }
                    let mut replacement = unary.clone();
                    replacement.value = value;
                    return (
                        Expr::new(expression.loc, ExprData::Unary(replacement)),
                        true,
                    );
                }
            }
            Some(ExprData::Binary(binary)) => {
                let (left, left_changed) = self.late_constant_fold(&binary.left);
                let (right, right_changed) = self.late_constant_fold(&binary.right);
                if left_changed || right_changed {
                    let mut replacement = binary.clone();
                    replacement.left = left;
                    replacement.right = right;
                    if should_fold_binary_operator_when_minifying(&replacement)
                        && let Some(folded) = fold_binary_operator(expression.loc, &replacement)
                    {
                        return (folded, true);
                    }
                    return (
                        Expr::new(expression.loc, ExprData::Binary(replacement)),
                        true,
                    );
                }
            }
            Some(ExprData::If(conditional)) => {
                let (test, changed) = self.late_constant_fold(&conditional.test);
                if changed {
                    if let Some((boolean, SideEffects::NoSideEffects)) =
                        to_boolean_with_side_effects(test.data.as_deref())
                    {
                        let (replacement, _) = self.late_constant_fold(if boolean {
                            &conditional.yes
                        } else {
                            &conditional.no
                        });
                        return (replacement, true);
                    }
                    let mut replacement = conditional.clone();
                    replacement.test = test;
                    return (Expr::new(expression.loc, ExprData::If(replacement)), true);
                }
            }
            _ => {}
        }
        (expression.clone(), false)
    }

    fn substitute_known_function_calls(
        &self,
        expression: &Expr,
        result_is_unused: bool,
    ) -> Option<Expr> {
        self.substitute_known_function_calls_with_empty_result(expression, result_is_unused, true)
    }

    fn substitute_known_function_calls_with_empty_result(
        &self,
        expression: &Expr,
        result_is_unused: bool,
        preserve_empty_result: bool,
    ) -> Option<Expr> {
        match expression.data.as_deref() {
            Some(ExprData::Call(call)) => {
                let reference = match call.target.data.as_deref() {
                    Some(ExprData::Identifier(identifier)) => Some(identifier.reference),
                    Some(ExprData::ImportIdentifier(identifier)) => Some(identifier.reference),
                    _ => None,
                };
                if let Some(reference) = reference {
                    let flags = self.renamer.flags_for_symbol(reference);
                    let can_inline = !flags.contains(SymbolFlags::COULD_POTENTIALLY_BE_MUTATED);
                    if can_inline && flags.contains(SymbolFlags::IS_EMPTY_FUNCTION) {
                        let mut replacement = Expr::default();
                        for argument in &call.args {
                            let argument =
                                if matches!(argument.data.as_deref(), Some(ExprData::Spread(_))) {
                                    Expr::new(
                                        argument.loc,
                                        ExprData::Array(crate::internal::js_ast::ArrayExpr {
                                            items: vec![argument.clone()],
                                            is_single_line: true,
                                            ..crate::internal::js_ast::ArrayExpr::default()
                                        }),
                                    )
                                } else {
                                    argument.clone()
                                };
                            let argument = self
                                .substitute_known_function_calls_with_empty_result(
                                    &argument,
                                    true,
                                    preserve_empty_result,
                                )
                                .unwrap_or(argument);
                            let helpers = make_helper_context(|reference| {
                                self.renamer.kind_for_symbol(reference) == SymbolKind::Unbound
                            });
                            let argument = helpers
                                .simplify_unused_expr(&argument, self.options.unsupported_features);
                            replacement = join_with_comma(replacement, argument);
                        }
                        if replacement.data.is_none() && preserve_empty_result || !result_is_unused
                        {
                            replacement = join_with_comma(
                                replacement,
                                Expr::new(expression.loc, ExprData::Undefined),
                            );
                        }
                        return Some(
                            self.substitute_known_function_calls_with_empty_result(
                                &replacement,
                                result_is_unused,
                                preserve_empty_result,
                            )
                            .unwrap_or(replacement),
                        );
                    }
                    if can_inline
                        && flags.contains(SymbolFlags::IS_IDENTITY_FUNCTION)
                        && matches!(call.args.as_slice(), [argument]
                            if !matches!(argument.data.as_deref(), Some(ExprData::Spread(_))))
                    {
                        let replacement = self
                            .substitute_known_function_calls_with_empty_result(
                                &call.args[0],
                                result_is_unused,
                                preserve_empty_result,
                            )
                            .unwrap_or_else(|| call.args[0].clone());
                        if result_is_unused {
                            let helpers = make_helper_context(|reference| {
                                self.renamer.kind_for_symbol(reference) == SymbolKind::Unbound
                            });
                            let mut replacement = helpers.simplify_unused_expr(
                                &replacement,
                                self.options.unsupported_features,
                            );
                            if replacement.data.is_none() && preserve_empty_result {
                                replacement = Expr::new(expression.loc, ExprData::Undefined);
                            }
                            return Some(replacement);
                        }
                        return Some(replacement);
                    }
                }
            }
            Some(ExprData::Binary(binary)) if binary.op == OpCode::BinaryComma => {
                let left = self.substitute_known_function_calls_with_empty_result(
                    &binary.left,
                    true,
                    false,
                );
                let right = result_is_unused
                    .then(|| {
                        self.substitute_known_function_calls_with_empty_result(
                            &binary.right,
                            true,
                            false,
                        )
                    })
                    .flatten();
                if left.is_some() || right.is_some() {
                    return Some(join_with_comma(
                        left.unwrap_or_else(|| binary.left.clone()),
                        right.unwrap_or_else(|| binary.right.clone()),
                    ));
                }
            }
            _ => {}
        }
        None
    }

    fn guard_substitution(&self, expression: Expr, context: SubstitutionContext) -> Expr {
        let needs_guard = match context {
            SubstitutionContext::DeleteTarget => !matches!(
                expression.data.as_deref(),
                Some(ExprData::Binary(binary)) if binary.op == OpCode::BinaryComma
            ),
            SubstitutionContext::CallTargetOrTemplateTag => match expression.data.as_deref() {
                Some(ExprData::Dot(_) | ExprData::Index(_)) => true,
                Some(ExprData::Identifier(identifier)) => {
                    self.renamer.original_name_for_symbol(identifier.reference) == "eval"
                }
                _ => false,
            },
        };
        if needs_guard {
            let loc = expression.loc;
            join_with_comma(Expr::new(loc, ExprData::Number(0.0)), expression)
        } else {
            expression
        }
    }

    fn print_expr_with_substitution_context(
        &mut self,
        expression: &Expr,
        level: Precedence,
        result_is_unused: bool,
        is_new_target: bool,
        context: SubstitutionContext,
    ) {
        if let Some(replacement) =
            self.substitute_known_function_calls(expression, result_is_unused)
        {
            let replacement = self.guard_substitution(replacement, context);
            self.print_expr_at_with_usage_and_new_target(
                &replacement,
                level,
                result_is_unused,
                is_new_target,
            );
        } else {
            self.print_expr_at_with_usage_and_new_target(
                expression,
                level,
                result_is_unused,
                is_new_target,
            );
        }
    }

    fn add_source_mapping(&mut self, location: crate::internal::logger::Loc, original_name: &str) {
        if location.start < 0 {
            return;
        }
        if let Some(builder) = &mut self.source_map_builder {
            builder.add_source_mapping(location, original_name, &self.output);
        }
    }

    fn will_print_expr_comments_at_loc(&self, loc: crate::internal::logger::Loc) -> bool {
        !self.options.minify_whitespace
            && !self.printed_expr_comments.contains(&loc)
            && self
                .expr_comments
                .is_some_and(|comments| comments.contains_key(&loc))
    }

    fn print_expr_comments_at_loc(&mut self, loc: crate::internal::logger::Loc) {
        if !self.will_print_expr_comments_at_loc(loc) {
            return;
        }
        let output_len = self.output.len();
        let preserve_stmt_start = self.stmt_start == Some(output_len);
        let preserve_arrow_expr_start = self.arrow_expr_start == Some(output_len);
        let preserve_export_default_start = self.export_default_start == Some(output_len);
        let comments = self
            .expr_comments
            .and_then(|comments| comments.get(&loc))
            .cloned()
            .unwrap_or_default();
        self.printed_expr_comments.insert(loc);
        for comment in comments {
            self.print_indented_comment(&comment);
            self.print_indent();
        }
        if preserve_stmt_start {
            self.stmt_start = Some(self.output.len());
        }
        if preserve_arrow_expr_start {
            self.arrow_expr_start = Some(self.output.len());
        }
        if preserve_export_default_start {
            self.export_default_start = Some(self.output.len());
        }
    }

    fn print_expr_comments_after_close_token_at_loc(&mut self, loc: crate::internal::logger::Loc) {
        if !self.will_print_expr_comments_at_loc(loc) {
            return;
        }
        let output_len = self.output.len();
        let preserve_stmt_start = self.stmt_start == Some(output_len);
        let preserve_arrow_expr_start = self.arrow_expr_start == Some(output_len);
        let preserve_export_default_start = self.export_default_start == Some(output_len);
        let comments = self
            .expr_comments
            .and_then(|comments| comments.get(&loc))
            .cloned()
            .unwrap_or_default();
        self.printed_expr_comments.insert(loc);
        for comment in comments {
            self.print_indent();
            self.print_indented_comment(&comment);
        }
        if preserve_stmt_start {
            self.stmt_start = Some(self.output.len());
        }
        if preserve_arrow_expr_start {
            self.arrow_expr_start = Some(self.output.len());
        }
        if preserve_export_default_start {
            self.export_default_start = Some(self.output.len());
        }
    }

    fn print_expr_without_leading_newline(&mut self, expr: &Expr, level: Precedence) {
        if !self.will_print_expr_comments_at_loc(expr.loc) {
            self.print_expr_at(expr, level);
            return;
        }
        self.output.push(b'(');
        self.print_newline();
        self.indent += 1;
        self.print_indent();
        let old_suppress = self.suppress_parenthesized_object_or_class;
        self.suppress_parenthesized_object_or_class = true;
        self.print_expr_at(expr, level);
        self.suppress_parenthesized_object_or_class = old_suppress;
        self.print_newline();
        self.indent -= 1;
        self.print_indent();
        self.output.push(b')');
    }

    fn print_indented_comment(&mut self, text: &str) {
        let escaped;
        let mut text = text;
        if !self
            .options
            .unsupported_features
            .contains(JsFeature::INLINE_SCRIPT)
        {
            escaped = escape_closing_tag(text, "/script");
            text = &escaped;
        }

        if text.starts_with("/*") {
            let mut lines = text.split_inclusive('\n').peekable();
            while let Some(line) = lines.next() {
                self.output.extend_from_slice(line.as_bytes());
                if line.ends_with('\n') && lines.peek().is_some() {
                    self.print_indent();
                }
            }
            self.print_newline();
        } else {
            self.output.extend_from_slice(text.as_bytes());
            self.output.push(b'\n');
        }
    }

    fn print_statements(&mut self, statements: &[&Stmt]) {
        let statements = statements
            .iter()
            .map(|statement| (*statement, 0))
            .collect::<Vec<_>>();
        self.print_statements_with_parts(&statements);
    }

    fn print_statements_with_parts(&mut self, statements: &[(&Stmt, usize)]) {
        let mut statement_index = 0;
        while statement_index < statements.len() {
            let (statement, part_index) = statements[statement_index];
            if self.options.minify_syntax
                && let Some(StmtData::Local(first)) = statement.data.as_deref()
            {
                let mut merged = first.clone();
                let mut next_index = statement_index + 1;
                while let Some((next, _)) = statements.get(next_index)
                    && let Some(StmtData::Local(next)) = next.data.as_deref()
                    && next.kind == merged.kind
                    && next.is_export == merged.is_export
                    && next.was_ts_import_equals == merged.was_ts_import_equals
                {
                    merged
                        .declarations
                        .extend(next.declarations.iter().cloned());
                    next_index += 1;
                }
                if next_index > statement_index + 1 {
                    self.print_stmt(&Stmt::new(statement.loc, StmtData::Local(merged)));
                    statement_index = next_index;
                    continue;
                }
            }
            if self.options.minify_syntax
                && let Some(StmtData::Expr(first)) = statement.data.as_deref()
                && !first.is_from_class_or_fn_that_can_be_removed_if_unused
                && !first.must_not_be_merged
            {
                let mut combined = first.value.clone();
                let mut next_index = statement_index + 1;
                while let Some((next, next_part_index)) = statements.get(next_index)
                    && *next_part_index == part_index
                    && let Some(StmtData::Expr(next)) = next.data.as_deref()
                    && !next.is_from_class_or_fn_that_can_be_removed_if_unused
                    && !next.must_not_be_merged
                {
                    combined = join_with_comma(combined, next.value.clone());
                    next_index += 1;
                }
                if let Some((next, next_part_index)) = statements.get(next_index)
                    && *next_part_index == part_index
                    && let Some(StmtData::Return(next)) = next.data.as_deref()
                    && next.value_or_nil.data.is_some()
                {
                    combined = join_with_comma(combined, next.value_or_nil.clone());
                    self.print_stmt(&Stmt::new(
                        statement.loc,
                        StmtData::Return(ReturnStmt {
                            value_or_nil: combined,
                        }),
                    ));
                    statement_index = next_index + 1;
                    continue;
                }
                if next_index > statement_index + 1 {
                    self.print_stmt(&Stmt::new(
                        statement.loc,
                        StmtData::Expr(ExprStmt {
                            value: combined,
                            ..ExprStmt::default()
                        }),
                    ));
                    statement_index = next_index;
                    continue;
                }
            }
            self.print_stmt(statement);
            statement_index += 1;
        }
    }

    #[allow(clippy::too_many_lines)]
    fn print_stmt(&mut self, statement: &Stmt) {
        let Some(data) = statement.data.as_deref() else {
            return;
        };
        let should_add_source_mapping = match data {
            StmtData::TypeScript(_) => false,
            StmtData::Comment(comment) if comment.is_legal_comment => {
                self.options.legal_comments == LegalComments::Inline
            }
            _ => true,
        };
        if should_add_source_mapping {
            self.add_source_mapping(statement.loc, "");
        }
        match data {
            StmtData::TypeScript(_) => {}
            StmtData::Empty => {
                self.print_indent();
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::Comment(comment) => {
                if comment.is_legal_comment {
                    match self.options.legal_comments {
                        LegalComments::None => return,
                        LegalComments::EndOfFile
                        | LegalComments::LinkedWithComment
                        | LegalComments::ExternalWithoutComment => {
                            if self.has_legal_comment.insert(comment.text.clone()) {
                                self.extracted_legal_comments.push(comment.text.clone());
                            }
                            return;
                        }
                        LegalComments::Inline => {}
                    }
                }
                self.print_indent();
                self.print_indented_comment(&comment.text);
            }
            StmtData::Debugger => {
                self.print_indent();
                self.output.extend_from_slice(b"debugger;");
                self.print_newline();
            }
            StmtData::Directive(directive) => {
                self.print_indent();
                self.output
                    .extend(quote_utf16(&directive.value, self.options, false));
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::Expr(expression) => {
                let replacement = self
                    .options
                    .minify_syntax
                    .then(|| {
                        self.substitute_known_function_calls_with_empty_result(
                            &expression.value,
                            true,
                            false,
                        )
                    })
                    .flatten();
                let value = replacement.as_ref().unwrap_or(&expression.value);
                if value.data.is_none() {
                    return;
                }
                self.print_indent();
                let old_stmt_start = self.stmt_start;
                self.stmt_start = Some(self.output.len());
                self.print_expr_at_with_usage(value, Precedence::Lowest, true);
                self.stmt_start = old_stmt_start;
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::Local(local) => {
                self.print_indent();
                self.print_local(local, true);
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::Block(block) => {
                self.print_indent();
                self.print_block(block, true);
            }
            StmtData::Function(function) => {
                if !self.options.minify_whitespace && function.function.has_no_side_effects_comment
                {
                    self.print_indent();
                    self.output.extend_from_slice(b"// @__NO_SIDE_EFFECTS__");
                    self.print_newline();
                }
                self.print_indent();
                if function.is_export {
                    self.output.extend_from_slice(b"export ");
                }
                self.print_function(&function.function);
                self.print_newline();
            }
            StmtData::Class(class) => {
                let omit_indent = self.print_decorators(&class.class.decorators, true);
                if !omit_indent {
                    self.print_indent();
                }
                if class.is_export {
                    self.output.extend_from_slice(b"export ");
                }
                self.print_class_body(&class.class);
                self.print_newline();
            }
            StmtData::Return(return_statement) => {
                self.print_indent();
                self.output.extend_from_slice(b"return");
                if return_statement.value_or_nil.data.is_some() {
                    let can_omit_space =
                        can_omit_space_after_return(&return_statement.value_or_nil, self.options);
                    if !self.options.minify_whitespace || !can_omit_space {
                        self.output.push(b' ');
                    }
                    self.print_expr_at(&return_statement.value_or_nil, Precedence::Lowest);
                }
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::Throw(throw_statement) => {
                self.print_indent();
                self.output.extend_from_slice(b"throw ");
                self.print_expr_at(&throw_statement.value, Precedence::Lowest);
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::If(if_statement) => {
                self.print_if(if_statement, true);
            }
            StmtData::While(while_statement) => {
                self.print_indent();
                self.output.extend_from_slice(b"while");
                self.print_optional_space();
                self.output.push(b'(');
                self.print_expr_at(&while_statement.test, Precedence::Lowest);
                self.output.push(b')');
                self.print_loop_body(&while_statement.body, while_statement.is_single_line_body);
            }
            StmtData::With(with_statement) => {
                self.print_indent();
                self.output.extend_from_slice(b"with");
                self.print_optional_space();
                self.output.push(b'(');
                self.print_expr_at(&with_statement.value, Precedence::Lowest);
                self.output.push(b')');
                self.with_nesting += 1;
                self.print_loop_body(&with_statement.body, with_statement.is_single_line_body);
                self.with_nesting -= 1;
            }
            StmtData::DoWhile(do_while) => {
                self.print_indent();
                self.output.extend_from_slice(b"do");
                if let Some(StmtData::Block(block)) = do_while.body.data.as_deref() {
                    self.print_optional_space();
                    self.print_block(block, false);
                    self.print_optional_space();
                } else {
                    if self.options.minify_whitespace
                        && !matches!(do_while.body.data.as_deref(), None | Some(StmtData::Empty))
                    {
                        self.output.push(b' ');
                    }
                    self.print_newline();
                    self.indent += 1;
                    self.print_stmt(&do_while.body);
                    self.indent -= 1;
                    self.print_indent();
                }
                self.output.extend_from_slice(b"while");
                self.print_optional_space();
                self.output.push(b'(');
                self.print_expr_at(&do_while.test, Precedence::Lowest);
                self.output.extend_from_slice(b");");
                self.print_newline();
            }
            StmtData::For(for_statement) => {
                let mut init = for_statement.init_or_nil.clone();
                if self.options.minify_syntax
                    && let Some(StmtData::Expr(expression)) = init.data.as_deref_mut()
                    && let Some(replacement) = self
                        .substitute_known_function_calls_with_empty_result(
                            &expression.value,
                            true,
                            false,
                        )
                {
                    expression.value = replacement;
                    if expression.value.data.is_none() {
                        init.data = None;
                    }
                }
                let mut update = for_statement.update_or_nil.clone();
                if self.options.minify_syntax
                    && let Some(replacement) =
                        self.substitute_known_function_calls_with_empty_result(&update, true, false)
                {
                    update = replacement;
                }
                self.print_indent();
                self.output.extend_from_slice(b"for");
                self.print_optional_space();
                self.output.push(b'(');
                self.print_for_init(&init);
                self.output.push(b';');
                self.print_optional_space();
                if for_statement.test_or_nil.data.is_some() {
                    self.print_expr_at(&for_statement.test_or_nil, Precedence::Lowest);
                }
                self.output.push(b';');
                self.print_optional_space();
                if update.data.is_some() {
                    self.print_expr_at_with_usage(&update, Precedence::Lowest, true);
                }
                self.output.push(b')');
                self.print_loop_body(&for_statement.body, for_statement.is_single_line_body);
            }
            StmtData::ForIn(for_statement) => {
                self.print_indent();
                self.output.extend_from_slice(b"for");
                self.print_optional_space();
                self.output.push(b'(');
                self.print_for_init(&for_statement.init);
                self.output.extend_from_slice(b" in ");
                self.print_expr_at(&for_statement.value, Precedence::Lowest);
                self.output.push(b')');
                self.print_loop_body(&for_statement.body, for_statement.is_single_line_body);
            }
            StmtData::ForOf(for_statement) => {
                self.print_indent();
                self.output
                    .extend_from_slice(if for_statement.await_range.len > 0 {
                        b"for await"
                    } else {
                        b"for"
                    });
                self.print_optional_space();
                self.output.push(b'(');
                let has_init_comment = match for_statement.init.data.as_deref() {
                    Some(StmtData::Expr(expression)) => {
                        self.will_print_expr_comments_at_loc(expression.value.loc)
                    }
                    _ => false,
                };
                let is_multi_line = has_init_comment
                    || self.will_print_expr_comments_at_loc(for_statement.value.loc);
                if is_multi_line {
                    self.print_newline();
                    self.indent += 1;
                    self.print_indent();
                }
                let old_for_of_init_start = self.for_of_init_start;
                self.for_of_init_start = true;
                self.print_for_init(&for_statement.init);
                self.for_of_init_start = old_for_of_init_start;
                self.output.extend_from_slice(b" of ");
                self.print_expr_at(&for_statement.value, Precedence::Spread);
                if is_multi_line {
                    self.print_newline();
                    self.indent -= 1;
                    self.print_indent();
                }
                self.output.push(b')');
                self.print_loop_body(&for_statement.body, for_statement.is_single_line_body);
            }
            StmtData::Label(label) => {
                self.print_indent();
                self.print_identifier(&self.renamer.name_for_symbol(label.name.reference));
                self.output.push(b':');
                if self.options.minify_whitespace {
                    let mut body = &label.statement;
                    let mut is_single_line = label.is_single_line_stmt;
                    while let Some(StmtData::Label(nested)) = body.data.as_deref() {
                        self.print_identifier(&self.renamer.name_for_symbol(nested.name.reference));
                        self.output.push(b':');
                        body = &nested.statement;
                        is_single_line = nested.is_single_line_stmt;
                    }
                    self.print_loop_body(body, is_single_line);
                } else {
                    if label.is_single_line_stmt
                        && matches!(
                            label.statement.data.as_deref(),
                            Some(StmtData::For(loop_statement))
                                if loop_statement.is_lowered_for_await
                        )
                    {
                        self.print_optional_space();
                        self.suppress_next_indent = true;
                        self.print_stmt(&label.statement);
                    } else {
                        self.print_loop_body(&label.statement, label.is_single_line_stmt);
                    }
                }
            }
            StmtData::Try(try_statement) => {
                self.print_indent();
                self.output.extend_from_slice(b"try");
                self.print_optional_space();
                self.print_block(&try_statement.block, false);
                if let Some(catch) = &try_statement.catch {
                    self.print_optional_space();
                    self.output.extend_from_slice(b"catch");
                    if catch.binding_or_nil.data.is_some() {
                        self.print_optional_space();
                        self.output.push(b'(');
                        self.print_binding(&catch.binding_or_nil);
                        self.output.push(b')');
                    }
                    self.print_optional_space();
                    self.print_block(&catch.block, false);
                }
                if let Some(finally) = &try_statement.finally {
                    self.print_optional_space();
                    self.output.extend_from_slice(b"finally");
                    self.print_optional_space();
                    self.print_block(&finally.block, false);
                }
                self.print_newline();
            }
            StmtData::Switch(switch_statement) => {
                self.print_indent();
                self.output.extend_from_slice(b"switch");
                self.print_optional_space();
                self.output.push(b'(');
                self.print_expr_at(&switch_statement.test, Precedence::Lowest);
                self.output.push(b')');
                self.print_optional_space();
                self.output.push(b'{');
                self.print_newline();
                self.indent += 1;
                for case in &switch_statement.cases {
                    self.print_indent();
                    self.print_expr_comments_at_loc(case.loc);
                    if case.value_or_nil.data.is_some() {
                        self.output.extend_from_slice(b"case ");
                        self.print_expr_at(&case.value_or_nil, Precedence::Lowest);
                        self.output.push(b':');
                    } else {
                        self.output.extend_from_slice(b"default:");
                    }
                    self.print_newline();
                    self.indent += 1;
                    let statements = case.body.iter().collect::<Vec<_>>();
                    self.print_statements(&statements);
                    self.indent -= 1;
                }
                if self.options.minify_whitespace
                    && let Some(statement) = switch_statement
                        .cases
                        .iter()
                        .rev()
                        .flat_map(|case| case.body.iter().rev())
                        .find(|statement| statement.data.is_some())
                    && statement_can_omit_semicolon_before_close_brace(statement)
                    && self.output.last() == Some(&b';')
                {
                    self.output.pop();
                }
                self.indent -= 1;
                self.print_indent();
                self.output.push(b'}');
                self.print_newline();
            }
            StmtData::Break(break_statement) => {
                self.print_indent();
                self.output.extend_from_slice(b"break");
                if let Some(label) = break_statement.label {
                    self.output.push(b' ');
                    self.print_identifier(&self.renamer.name_for_symbol(label.reference));
                }
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::Continue(continue_statement) => {
                self.print_indent();
                self.output.extend_from_slice(b"continue");
                if let Some(label) = continue_statement.label {
                    self.output.push(b' ');
                    self.print_identifier(&self.renamer.name_for_symbol(label.reference));
                }
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::ExportEquals(export) => {
                self.print_indent();
                self.output.extend_from_slice(b"module.exports");
                self.print_optional_space();
                self.output.push(b'=');
                self.print_optional_space();
                self.print_expr_at(&export.value, Precedence::Spread);
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::LazyExport(export) => {
                self.print_indent();
                self.print_expr_at(&export.value, Precedence::Lowest);
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::Import(import) => {
                self.print_indent();
                self.output.extend_from_slice(b"import");
                match self.import_records[import.import_record_index as usize].phase {
                    ImportPhase::Evaluation => {}
                    ImportPhase::Defer => self.output.extend_from_slice(b" defer"),
                    ImportPhase::Source => self.output.extend_from_slice(b" source"),
                }
                let has_clause = import.default_name.is_some()
                    || import.star_name_loc.is_some()
                    || import.items.is_some();
                if has_clause {
                    if import.default_name.is_some() {
                        self.output.push(b' ');
                    } else {
                        self.print_optional_space();
                    }
                    let mut needs_comma = false;
                    if let Some(default_name) = import.default_name {
                        self.print_identifier(
                            &self.renamer.name_for_symbol(default_name.reference),
                        );
                        needs_comma = true;
                    }
                    if import.star_name_loc.is_some() {
                        if needs_comma {
                            self.output.push(b',');
                            self.print_optional_space();
                        }
                        self.output.push(b'*');
                        self.print_optional_space();
                        self.output.extend_from_slice(b"as ");
                        self.print_identifier(&self.renamer.name_for_symbol(import.namespace_ref));
                    } else if let Some(items) = &import.items {
                        if needs_comma {
                            self.output.push(b',');
                            self.print_optional_space();
                        }
                        self.print_import_items(items, true, import.is_single_line);
                    }
                    let ends_with_identifier =
                        import.star_name_loc.is_some() || import.items.is_none();
                    if ends_with_identifier || !self.options.minify_whitespace {
                        self.output.push(b' ');
                    }
                    self.output.extend_from_slice(b"from");
                    self.print_optional_space();
                } else {
                    self.print_optional_space();
                }
                self.print_import_path(import.import_record_index, false);
                self.print_import_attributes(import.import_record_index, false, false);
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::ExportClause(export) => {
                self.print_indent();
                self.output.extend_from_slice(b"export");
                self.print_optional_space();
                self.print_import_items(&export.items, false, export.is_single_line);
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::ExportFrom(export) => {
                self.print_indent();
                self.output.extend_from_slice(b"export");
                self.print_optional_space();
                self.print_export_from_items(&export.items);
                self.print_optional_space();
                self.output.extend_from_slice(b"from");
                self.print_optional_space();
                self.print_import_path(export.import_record_index, false);
                self.print_import_attributes(export.import_record_index, false, false);
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::ExportStar(export) => {
                self.print_indent();
                self.output.extend_from_slice(b"export");
                self.print_optional_space();
                self.output.push(b'*');
                if let Some(alias) = &export.alias {
                    self.print_optional_space();
                    self.output.extend_from_slice(b"as");
                    let alias_is_identifier = is_identifier_es5_and_es_next(&alias.original_name);
                    if !self.options.minify_whitespace || alias_is_identifier {
                        self.output.push(b' ');
                    }
                    self.print_clause_name(&alias.original_name);
                    if !self.options.minify_whitespace || alias_is_identifier {
                        self.output.push(b' ');
                    }
                } else {
                    self.print_optional_space();
                }
                self.output.extend_from_slice(b"from");
                self.print_optional_space();
                self.print_import_path(export.import_record_index, false);
                self.print_import_attributes(export.import_record_index, false, false);
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::ExportDefault(export) => {
                if !self.options.minify_whitespace
                    && matches!(
                        export.value.data.as_deref(),
                        Some(StmtData::Function(function))
                            if function.function.has_no_side_effects_comment
                    )
                {
                    self.print_indent();
                    self.output.extend_from_slice(b"// @__NO_SIDE_EFFECTS__");
                    self.print_newline();
                }
                let omit_indent = match export.value.data.as_deref() {
                    Some(StmtData::Class(class)) => {
                        self.print_decorators(&class.class.decorators, true)
                    }
                    _ => false,
                };
                if !omit_indent {
                    self.print_indent();
                }
                self.output.extend_from_slice(b"export default ");
                match export.value.data.as_deref() {
                    Some(StmtData::Expr(expression)) => {
                        let old_export_default_start = self.export_default_start;
                        if !matches!(
                            expression.value.data.as_deref(),
                            Some(ExprData::Function(function)) if !function.is_parenthesized
                        ) {
                            self.export_default_start = Some(self.output.len());
                        }
                        self.print_expr_without_leading_newline(
                            &expression.value,
                            Precedence::Spread,
                        );
                        self.export_default_start = old_export_default_start;
                        if !matches!(
                            expression.value.data.as_deref(),
                            Some(ExprData::Function(function)) if !function.is_parenthesized
                        ) {
                            self.output.push(b';');
                        }
                    }
                    Some(StmtData::Function(function)) => self.print_function(&function.function),
                    Some(StmtData::Class(class)) => self.print_class_body(&class.class),
                    _ => panic!("Internal error: invalid default export"),
                }
                self.print_newline();
            }
            StmtData::Enum(_) | StmtData::Namespace(_) => {
                panic!("Internal error: statement printer case has not been ported yet")
            }
        }
    }

    fn print_block(&mut self, block: &BlockStmt, trailing_newline: bool) {
        self.output.push(b'{');
        self.print_newline();
        self.indent += 1;
        let statements = block.statements.iter().collect::<Vec<_>>();
        self.print_statements(&statements);
        if self.options.minify_whitespace
            && let Some(statement) = block
                .statements
                .iter()
                .rfind(|statement| statement.data.is_some())
            && statement_can_omit_semicolon_before_close_brace(statement)
            && self.output.last() == Some(&b';')
        {
            self.output.pop();
        }
        self.indent -= 1;
        self.print_indent();
        self.output.push(b'}');
        if trailing_newline {
            self.print_newline();
        }
    }

    fn print_if(&mut self, if_statement: &IfStmt, print_indent: bool) {
        let mut no_or_nil = if_statement.no_or_nil.clone();
        if self.options.minify_syntax
            && let Some(StmtData::Expr(expression)) = no_or_nil.data.as_deref_mut()
            && let Some(replacement) = self.substitute_known_function_calls_with_empty_result(
                &expression.value,
                true,
                false,
            )
        {
            expression.value = replacement;
            if expression.value.data.is_none() {
                no_or_nil.data = None;
            }
        }
        if print_indent {
            self.print_indent();
        }
        self.output.extend_from_slice(b"if");
        self.print_optional_space();
        self.output.push(b'(');
        self.print_expr_at(&if_statement.test, Precedence::Lowest);
        self.output.push(b')');
        let yes_is_block = self.print_if_body(
            &if_statement.yes,
            if_statement.is_single_line_yes,
            no_or_nil.data.is_some(),
        );
        if no_or_nil.data.is_none() {
            return;
        }
        if yes_is_block {
            self.print_optional_space();
        } else if !self.options.minify_whitespace {
            self.print_indent();
        }
        self.output.extend_from_slice(b"else");
        if let Some(StmtData::If(nested)) = no_or_nil.data.as_deref() {
            self.output.push(b' ');
            self.print_if(nested, false);
            return;
        }
        if self.options.minify_whitespace
            && statement_starts_with_identifier(&no_or_nil, self.options)
        {
            self.output.push(b' ');
        }
        self.print_if_body(&no_or_nil, if_statement.is_single_line_no, false);
    }

    fn print_body(&mut self, body: &Stmt) {
        if body.data.is_none() {
            self.print_optional_space();
            self.output.push(b';');
            self.print_newline();
        } else if let Some(StmtData::Block(block)) = body.data.as_deref() {
            self.print_optional_space();
            self.print_block(block, true);
        } else {
            self.print_newline();
            self.indent += 1;
            self.print_stmt(body);
            self.indent -= 1;
        }
    }

    fn print_loop_body(&mut self, body: &Stmt, is_single_line: bool) {
        let mut simplified_body = body.clone();
        if self.options.minify_syntax
            && let Some(StmtData::Expr(expression)) = simplified_body.data.as_deref_mut()
            && let Some(replacement) = self.substitute_known_function_calls_with_empty_result(
                &expression.value,
                true,
                false,
            )
        {
            expression.value = replacement;
            if expression.value.data.is_none() {
                simplified_body.data = None;
            }
        }
        let body = &simplified_body;
        if body.data.is_none() {
            self.print_optional_space();
            self.output.push(b';');
            self.print_newline();
        } else if is_single_line && !matches!(body.data.as_deref(), Some(StmtData::Block(_))) {
            self.print_optional_space();
            let indent = std::mem::take(&mut self.indent);
            self.print_stmt(body);
            self.indent = indent;
        } else {
            self.print_body(body);
        }
    }

    fn print_if_body(&mut self, body: &Stmt, is_single_line: bool, has_else: bool) -> bool {
        if let Some(StmtData::Block(block)) = body.data.as_deref() {
            self.print_optional_space();
            self.print_block(block, !has_else);
            return true;
        }
        if is_single_line && body.data.is_some() {
            self.print_optional_space();
            let indent = std::mem::take(&mut self.indent);
            self.print_stmt(body);
            self.indent = indent;
            return false;
        }
        self.print_body(body);
        false
    }

    fn print_binding(&mut self, binding: &Binding) {
        let original_name = match binding.data.as_deref() {
            Some(BindingData::Identifier(identifier)) => {
                self.renamer.original_name_for_symbol(identifier.reference)
            }
            _ => String::new(),
        };
        self.add_source_mapping(binding.loc, &original_name);
        match binding.data.as_deref() {
            None | Some(BindingData::Missing) => {}
            Some(BindingData::Identifier(identifier)) => {
                self.print_identifier(&self.renamer.name_for_symbol(identifier.reference));
            }
            Some(BindingData::Array(array)) => {
                self.output.push(b'[');
                for (index, item) in array.items.iter().enumerate() {
                    if index > 0 {
                        self.output.push(b',');
                        self.print_optional_space();
                    }
                    if array.has_spread && index + 1 == array.items.len() {
                        self.output.extend_from_slice(b"...");
                    }
                    self.print_binding(&item.binding);
                    if item.default_value_or_nil.data.is_some() {
                        self.print_optional_space();
                        self.output.push(b'=');
                        self.print_optional_space();
                        self.print_expr_at(&item.default_value_or_nil, Precedence::Spread);
                    }
                }
                if !array.has_spread
                    && matches!(
                        array
                            .items
                            .last()
                            .and_then(|item| item.binding.data.as_deref()),
                        Some(BindingData::Missing)
                    )
                {
                    self.output.push(b',');
                }
                self.output.push(b']');
            }
            Some(BindingData::Object(object)) => {
                self.output.push(b'{');
                if !object.properties.is_empty() {
                    self.print_optional_space();
                }
                for (index, property) in object.properties.iter().enumerate() {
                    if index > 0 {
                        self.output.push(b',');
                        self.print_optional_space();
                    }
                    if property.is_spread {
                        self.output.extend_from_slice(b"...");
                        self.print_binding(&property.value);
                        continue;
                    }
                    let is_shorthand = if let (
                        Some(ExprData::String(key)),
                        Some(BindingData::Identifier(value)),
                    ) =
                        (property.key.data.as_deref(), property.value.data.as_deref())
                    {
                        !property.prefer_quoted_key
                            && String::from_utf16_lossy(&key.value)
                                == self.renamer.name_for_symbol(value.reference)
                    } else {
                        false
                    };
                    if is_shorthand {
                        self.print_binding(&property.value);
                    } else {
                        if property.is_computed {
                            self.output.push(b'[');
                            self.print_expr_at(&property.key, Precedence::Lowest);
                            self.output.push(b']');
                        } else if property.prefer_quoted_key
                            && let Some(ExprData::String(key)) = property.key.data.as_deref()
                        {
                            self.output
                                .extend(quote_utf16(&key.value, self.options, false));
                        } else {
                            self.print_property_key(&property.key);
                        }
                        self.output.push(b':');
                        self.print_optional_space();
                        self.print_binding(&property.value);
                    }
                    if property.default_value_or_nil.data.is_some() {
                        self.print_optional_space();
                        self.output.push(b'=');
                        self.print_optional_space();
                        self.print_expr_at(&property.default_value_or_nil, Precedence::Spread);
                    }
                }
                if !object.properties.is_empty() {
                    self.print_optional_space();
                }
                self.output.push(b'}');
            }
        }
    }

    fn print_local(&mut self, local: &crate::internal::js_ast::LocalStmt, include_export: bool) {
        if include_export && local.is_export {
            self.output.extend_from_slice(b"export ");
        }
        self.output.extend_from_slice(match local.kind {
            LocalKind::Var => b"var",
            LocalKind::Let => b"let",
            LocalKind::Const => b"const",
            LocalKind::Using => b"using",
            LocalKind::AwaitUsing => b"await using",
        });
        if matches!(
            local
                .declarations
                .first()
                .and_then(|declaration| declaration.binding.data.as_deref()),
            Some(BindingData::Identifier(_))
        ) {
            self.output.push(b' ');
        } else {
            self.print_optional_space();
        }
        for (index, declaration) in local.declarations.iter().enumerate() {
            if index > 0 {
                self.output.push(b',');
                self.print_optional_space();
            }
            self.print_binding(&declaration.binding);
            if declaration.value_or_nil.data.is_some() {
                self.print_optional_space();
                self.output.push(b'=');
                self.print_optional_space();
                self.print_expr_without_leading_newline(
                    &declaration.value_or_nil,
                    Precedence::Spread,
                );
            }
        }
    }

    fn print_for_init(&mut self, statement: &Stmt) {
        let old_forbid_in = self.forbid_in;
        self.forbid_in = true;
        match statement.data.as_deref() {
            None | Some(StmtData::Empty) => {}
            Some(StmtData::Local(local)) => self.print_local(local, false),
            Some(StmtData::Expr(expression)) => {
                self.print_expr_at_with_usage(&expression.value, Precedence::Lowest, true);
            }
            _ => panic!("Internal error: invalid for-loop initializer"),
        }
        self.forbid_in = old_forbid_in;
    }

    fn print_import_items(
        &mut self,
        items: &[crate::internal::js_ast::ClauseItem],
        is_import: bool,
        is_single_line: bool,
    ) {
        let is_multi_line = !self.options.minify_whitespace && !is_single_line;
        self.output.push(b'{');
        if is_multi_line {
            self.indent += 1;
        } else if !items.is_empty() {
            self.print_optional_space();
        }
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                self.output.push(b',');
            }
            if is_multi_line {
                self.print_newline();
                self.print_indent();
            } else if index > 0 {
                self.print_optional_space();
            }
            let local_name = self.renamer.name_for_symbol(item.name.reference);
            let (original, alias) = if is_import {
                (&item.alias, &local_name)
            } else {
                (&local_name, &item.alias)
            };
            self.print_clause_name(original);
            if original != alias {
                if !self.options.minify_whitespace || is_identifier_es5_and_es_next(original) {
                    self.output.push(b' ');
                }
                self.output.extend_from_slice(b"as");
                if !self.options.minify_whitespace || is_identifier_es5_and_es_next(alias) {
                    self.output.push(b' ');
                }
                self.print_clause_name(alias);
            }
        }
        if is_multi_line {
            self.print_newline();
            self.indent -= 1;
            self.print_indent();
        } else if !items.is_empty() {
            self.print_optional_space();
        }
        self.output.push(b'}');
    }

    fn print_export_from_items(&mut self, items: &[crate::internal::js_ast::ClauseItem]) {
        self.output.push(b'{');
        if !items.is_empty() {
            self.print_optional_space();
        }
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                self.output.push(b',');
                self.print_optional_space();
            }
            self.print_clause_name(&item.original_name);
            if item.original_name != item.alias {
                if !self.options.minify_whitespace
                    || is_identifier_es5_and_es_next(&item.original_name)
                {
                    self.output.push(b' ');
                }
                self.output.extend_from_slice(b"as");
                if !self.options.minify_whitespace || is_identifier_es5_and_es_next(&item.alias) {
                    self.output.push(b' ');
                }
                self.print_clause_name(&item.alias);
            }
        }
        if !items.is_empty() {
            self.print_optional_space();
        }
        self.output.push(b'}');
    }

    fn print_clause_name(&mut self, name: &str) {
        if is_identifier_es5_and_es_next(name) {
            self.print_identifier(name);
        } else {
            self.output.extend(quote_utf16(
                &name.encode_utf16().collect::<Vec<_>>(),
                self.options,
                false,
            ));
        }
    }

    fn print_import_path(&mut self, index: u32, is_require: bool) {
        let record = &self.import_records[usize::try_from(index).expect("import record index")];
        self.output.extend(quote_utf16(
            &record.path.text.encode_utf16().collect::<Vec<_>>(),
            self.options,
            false,
        ));
        if self.options.needs_metafile {
            let kind = if is_require {
                if record.kind == crate::internal::ast::ImportKind::RequireResolve {
                    crate::internal::ast::ImportKind::RequireResolve
                } else {
                    crate::internal::ast::ImportKind::Require
                }
            } else if record.kind == crate::internal::ast::ImportKind::Dynamic {
                crate::internal::ast::ImportKind::Dynamic
            } else {
                crate::internal::ast::ImportKind::Stmt
            };
            let external = if record.flags.contains(
                crate::internal::ast::ImportRecordFlags::SHOULD_NOT_BE_EXTERNAL_IN_METAFILE,
            ) {
                String::new()
            } else {
                self.options
                    .metafile_format
                    .maybe_remove_whitespace(",\n          \"external\": true")
            };
            let path = String::from_utf8(quote_for_json(
                record.path.text.as_bytes(),
                self.options.ascii_only,
            ))
            .expect("quoted import path is UTF-8");
            let kind = String::from_utf8(quote_for_json(
                kind.string_for_metafile().as_bytes(),
                self.options.ascii_only,
            ))
            .expect("quoted import kind is UTF-8");
            self.json_metadata_imports.push(
                self.options.metafile_format.maybe_remove_whitespace(&format!(
                    "\n        {{\n          \"path\": {path},\n          \"kind\": {kind}{external}\n        }}"
                )),
            );
        }
    }

    fn print_import_attributes(&mut self, index: u32, is_dynamic: bool, is_multi_line: bool) {
        let Some(attributes) = self.import_records
            [usize::try_from(index).expect("import record index")]
        .assert_or_with
        .clone() else {
            return;
        };

        if is_dynamic {
            if self
                .options
                .unsupported_features
                .contains(JsFeature::IMPORT_ASSERTIONS)
                && self
                    .options
                    .unsupported_features
                    .contains(JsFeature::IMPORT_ATTRIBUTES)
            {
                return;
            }

            let attributes_are_multi_line = self
                .will_print_expr_comments_at_loc(attributes.keyword_loc)
                || self.will_print_expr_comments_at_loc(attributes.inner_open_brace_loc)
                || self.will_print_expr_comments_at_loc(attributes.outer_close_brace_loc);
            self.output.push(b',');
            if is_multi_line {
                self.print_newline();
                self.print_indent();
            } else {
                self.print_optional_space();
            }
            self.print_expr_comments_at_loc(attributes.outer_open_brace_loc);
            self.output.push(b'{');
            if attributes_are_multi_line {
                self.print_newline();
                self.indent += 1;
                self.print_indent();
            } else {
                self.print_optional_space();
            }

            self.print_expr_comments_at_loc(attributes.keyword_loc);
            self.output
                .extend_from_slice(attributes.keyword.as_str().as_bytes());
            self.output.push(b':');
            if self.will_print_expr_comments_at_loc(attributes.inner_open_brace_loc) {
                self.print_newline();
                self.indent += 1;
                self.print_indent();
                self.print_expr_comments_at_loc(attributes.inner_open_brace_loc);
                self.print_import_assert_or_with_clause(&attributes);
                self.indent -= 1;
            } else {
                self.print_optional_space();
                self.print_import_assert_or_with_clause(&attributes);
            }

            if attributes_are_multi_line {
                self.print_newline();
                self.print_expr_comments_after_close_token_at_loc(attributes.outer_close_brace_loc);
                self.indent -= 1;
                self.print_indent();
            } else {
                self.print_optional_space();
            }
            self.output.push(b'}');
        } else {
            let feature = if attributes.keyword == AssertOrWithKeyword::Assert {
                JsFeature::IMPORT_ASSERTIONS
            } else {
                JsFeature::IMPORT_ATTRIBUTES
            };
            if self.options.unsupported_features.contains(feature) {
                return;
            }

            self.print_optional_space();
            self.output
                .extend_from_slice(attributes.keyword.as_str().as_bytes());
            self.print_optional_space();
            self.print_import_assert_or_with_clause(&attributes);
        }
    }

    fn print_import_assert_or_with_clause(
        &mut self,
        attributes: &crate::internal::ast::ImportAssertOrWith,
    ) {
        let is_multi_line = self.will_print_expr_comments_at_loc(attributes.inner_close_brace_loc)
            || attributes.entries.iter().any(|entry| {
                self.will_print_expr_comments_at_loc(entry.key_loc)
                    || self.will_print_expr_comments_at_loc(entry.value_loc)
            });

        self.output.push(b'{');
        if is_multi_line {
            self.indent += 1;
        }
        for (entry_index, entry) in attributes.entries.iter().enumerate() {
            if entry_index > 0 {
                self.output.push(b',');
            }
            if is_multi_line {
                self.print_newline();
                self.print_indent();
            } else {
                self.print_optional_space();
            }

            self.print_expr_comments_at_loc(entry.key_loc);
            let key = String::from_utf16_lossy(&entry.key);
            if !entry.prefer_quoted_key && is_identifier_es5_and_es_next(&key) {
                self.print_identifier(&key);
            } else {
                self.output
                    .extend(quote_utf16(&entry.key, self.options, false));
            }
            self.output.push(b':');
            if self.will_print_expr_comments_at_loc(entry.value_loc) {
                self.print_newline();
                self.indent += 1;
                self.print_indent();
                self.print_expr_comments_at_loc(entry.value_loc);
                self.output
                    .extend(quote_utf16(&entry.value, self.options, false));
                self.indent -= 1;
            } else {
                self.print_optional_space();
                self.output
                    .extend(quote_utf16(&entry.value, self.options, false));
            }
        }

        if is_multi_line {
            self.print_newline();
            self.print_expr_comments_after_close_token_at_loc(attributes.inner_close_brace_loc);
            self.indent -= 1;
            self.print_indent();
        } else if !attributes.entries.is_empty() {
            self.print_optional_space();
        }
        self.output.push(b'}');
    }

    fn print_function(&mut self, function: &crate::internal::js_ast::Function) {
        if function.is_async {
            self.output.extend_from_slice(b"async ");
        }
        self.output.extend_from_slice(b"function");
        if function.is_generator {
            self.output.push(b'*');
            self.print_optional_space();
        }
        if let Some(name) = function.name {
            if !function.is_generator {
                self.output.push(b' ');
            }
            self.print_identifier(&self.renamer.name_for_symbol(name.reference));
        }
        self.print_function_arguments(function);
        self.print_optional_space();
        self.print_block(&function.body.block, false);
    }

    fn print_function_arguments(&mut self, function: &crate::internal::js_ast::Function) {
        self.output.push(b'(');
        for (index, argument) in function.args.iter().enumerate() {
            if index > 0 {
                self.output.push(b',');
                self.print_optional_space();
            }
            if function.has_rest_arg && index + 1 == function.args.len() {
                self.output.extend_from_slice(b"...");
            }
            self.print_binding(&argument.binding);
            if argument.default_or_nil.data.is_some() {
                self.print_optional_space();
                self.output.push(b'=');
                self.print_optional_space();
                self.print_expr_at(&argument.default_or_nil, Precedence::Spread);
            }
        }
        self.output.push(b')');
    }

    fn print_decorator(&mut self, decorator: &crate::internal::js_ast::Decorator) {
        self.output.push(b'@');
        let wrap = matches!(decorator.value.data.as_deref(), Some(ExprData::New(_)));
        if wrap {
            self.output.push(b'(');
        }
        self.print_expr_at(&decorator.value, Precedence::Lowest);
        if wrap {
            self.output.push(b')');
        }
    }

    fn print_decorators(
        &mut self,
        decorators: &[crate::internal::js_ast::Decorator],
        newline_by_default: bool,
    ) -> bool {
        let mut omit_indent = !newline_by_default;
        for (index, decorator) in decorators.iter().enumerate() {
            if newline_by_default && !omit_indent {
                self.print_indent();
            }
            self.print_decorator(decorator);
            omit_indent = decorator.omit_newline_after || !newline_by_default;
            if omit_indent {
                if index + 1 == decorators.len() {
                    self.output.push(b' ');
                } else {
                    self.print_optional_space();
                }
            } else {
                self.print_newline();
            }
        }
        !decorators.is_empty() && omit_indent
    }

    fn print_class(&mut self, class: &crate::internal::js_ast::Class) {
        self.print_decorators(&class.decorators, false);
        self.print_class_body(class);
    }

    fn print_class_body(&mut self, class: &crate::internal::js_ast::Class) {
        self.output.extend_from_slice(b"class");
        if let Some(name) = class.name {
            self.output.push(b' ');
            self.print_identifier(&self.renamer.name_for_symbol(name.reference));
        }
        if class.extends_or_nil.data.is_some() {
            self.output.extend_from_slice(b" extends ");
            self.print_expr_at(&class.extends_or_nil, Precedence::Postfix);
        }
        self.print_optional_space();
        self.output.push(b'{');
        self.print_newline();
        self.indent += 1;
        for property in &class.properties {
            let mut needs_indent = true;
            for decorator in &property.decorators {
                if needs_indent {
                    self.print_indent();
                }
                self.print_decorator(decorator);
                if decorator.omit_newline_after {
                    self.print_optional_space();
                    needs_indent = false;
                } else {
                    self.print_newline();
                    needs_indent = true;
                }
            }
            if needs_indent {
                self.print_indent();
            }
            self.print_expr_comments_at_loc(property.loc);
            if self.options.minify_whitespace && !property.decorators.is_empty() {
                self.output.push(b' ');
            }
            if property.kind == PropertyKind::ClassStaticBlock {
                self.output.extend_from_slice(b"static");
                self.print_optional_space();
                if let Some(block) = &property.class_static_block {
                    self.print_block(&block.block, true);
                }
                continue;
            }
            if property.flags.contains(PropertyFlags::IS_STATIC) {
                self.output.extend_from_slice(b"static ");
            }
            if property.kind == PropertyKind::AutoAccessor {
                self.output.extend_from_slice(b"accessor");
                if property.flags.contains(PropertyFlags::IS_COMPUTED) {
                    self.print_optional_space();
                } else {
                    self.output.push(b' ');
                }
            }
            if property.kind.is_method_definition()
                && let Some(ExprData::Function(function)) = property.value_or_nil.data.as_deref()
            {
                match property.kind {
                    PropertyKind::Getter => self.output.extend_from_slice(b"get "),
                    PropertyKind::Setter => self.output.extend_from_slice(b"set "),
                    _ => {}
                }
                if function.function.is_async {
                    self.output.extend_from_slice(b"async ");
                }
                if function.function.is_generator {
                    self.output.push(b'*');
                }
                self.print_class_key(property);
                self.print_function_arguments(&function.function);
                self.print_optional_space();
                self.print_block(&function.function.body.block, true);
                continue;
            }
            self.print_class_key(property);
            let initializer = if property.initializer_or_nil.data.is_some() {
                &property.initializer_or_nil
            } else {
                &property.value_or_nil
            };
            if initializer.data.is_some() {
                self.print_optional_space();
                self.output.push(b'=');
                self.print_optional_space();
                self.print_expr_at(initializer, Precedence::Spread);
            }
            self.output.push(b';');
            self.print_newline();
        }
        self.print_expr_comments_after_close_token_at_loc(class.close_brace_loc);
        self.indent -= 1;
        if self.options.minify_whitespace && self.output.last() == Some(&b';') {
            self.output.pop();
        }
        self.print_indent();
        self.output.push(b'}');
    }

    fn print_class_key(&mut self, property: &crate::internal::js_ast::Property) {
        let mut key = property.key.clone();
        let mut is_computed = property.flags.contains(PropertyFlags::IS_COMPUTED);
        if self.options.minify_syntax && is_computed {
            let (replacement, changed) = self.late_constant_fold(&key);
            if changed {
                key = replacement;
            }
            if let Some(ExprData::InlinedEnum(value)) = key.data.as_deref() {
                key = value.value.clone();
            }
            match key.data.as_deref() {
                Some(ExprData::Number(_)) => is_computed = false,
                Some(ExprData::String(value)) => {
                    let value = String::from_utf16_lossy(&value.value);
                    if !matches!(value.as_str(), "__proto__" | "constructor" | "prototype") {
                        is_computed = false;
                    }
                }
                _ => {}
            }
        }
        if self.options.minify_syntax
            && let Some(ExprData::String(value)) = key.data.as_deref()
            && let Some(number) = string_to_equivalent_number_value(&value.value)
            && number >= 0.0
        {
            key = Expr::new(key.loc, ExprData::Number(number));
            is_computed = false;
        }
        if is_computed {
            self.output.push(b'[');
            let wrap = matches!(
                key.data.as_deref(),
                Some(ExprData::Binary(binary)) if binary.op == OpCode::BinaryComma
            );
            if wrap {
                self.output.push(b'(');
            }
            self.print_expr_at(&key, Precedence::Lowest);
            if wrap {
                self.output.push(b')');
            }
            self.output.push(b']');
        } else if property.flags.contains(PropertyFlags::PREFER_QUOTED_KEY)
            && let Some(ExprData::String(key)) = key.data.as_deref()
        {
            self.output
                .extend(quote_utf16(&key.value, self.options, false));
        } else {
            self.print_property_key(&key);
        }
    }

    fn print_indent(&mut self) {
        if std::mem::take(&mut self.suppress_next_indent) {
            return;
        }
        if !self.options.minify_whitespace {
            for _ in 0..self.indent {
                self.output.extend_from_slice(b"  ");
            }
        }
    }

    fn print_newline(&mut self) {
        if !self.options.minify_whitespace {
            self.output.push(b'\n');
        }
    }

    #[allow(clippy::too_many_lines)]
    fn print_expr_at(&mut self, expr: &Expr, level: Precedence) {
        self.print_expr_at_with_usage(expr, level, false);
    }

    #[allow(clippy::too_many_lines)]
    fn print_expr_at_with_usage(&mut self, expr: &Expr, level: Precedence, result_is_unused: bool) {
        self.print_expr_at_with_usage_and_new_target(expr, level, result_is_unused, false);
    }

    #[allow(clippy::too_many_lines)]
    fn print_expr_at_with_usage_and_new_target(
        &mut self,
        expr: &Expr,
        level: Precedence,
        result_is_unused: bool,
        is_new_target: bool,
    ) {
        stacker::maybe_grow(64 * 1024, 2 * 1024 * 1024, || {
            self.print_expr_at_with_usage_and_new_target_inner(
                expr,
                level,
                result_is_unused,
                is_new_target,
            );
        });
    }

    #[allow(clippy::too_many_lines)]
    fn print_expr_at_with_usage_and_new_target_inner(
        &mut self,
        expr: &Expr,
        level: Precedence,
        result_is_unused: bool,
        is_new_target: bool,
    ) {
        if self.options.minify_syntax
            && matches!(
                expr.data.as_deref(),
                Some(ExprData::Unary(_) | ExprData::Binary(_) | ExprData::If(_))
            )
        {
            let (replacement, changed) = self.late_constant_fold(expr);
            if changed {
                self.print_expr_at_with_usage_and_new_target(
                    &replacement,
                    level,
                    result_is_unused,
                    is_new_target,
                );
                return;
            }
        }
        if let Some(replacement) = self.substitute_imported_enum(expr) {
            self.print_expr_at_with_usage_and_new_target(
                &replacement,
                level,
                result_is_unused,
                is_new_target,
            );
            return;
        }
        if let Some(replacement) = self.substitute_known_function_calls(expr, result_is_unused) {
            self.print_expr_at_with_usage_and_new_target(
                &replacement,
                level,
                result_is_unused,
                is_new_target,
            );
            return;
        }
        self.print_expr_comments_at_loc(expr.loc);
        let Some(data) = expr.data.as_deref() else {
            return;
        };
        let lower_bigint = matches!(data, ExprData::BigInt(_))
            && self
                .options
                .unsupported_features
                .contains(JsFeature::BIGINT);
        let original_name = match data {
            ExprData::Identifier(identifier) => {
                self.renamer.original_name_for_symbol(identifier.reference)
            }
            _ => String::new(),
        };
        self.add_source_mapping(expr.loc, &original_name);
        let own_level = expr_precedence(data);
        let has_pure_comment = !self.options.minify_whitespace
            && match data {
                ExprData::Call(call) => call.can_be_unwrapped_if_unused,
                ExprData::New(new) => new.can_be_unwrapped_if_unused,
                ExprData::BigInt(_) => lower_bigint,
                _ => false,
            };
        let wrap_for_new_target = is_new_target
            && match data {
                ExprData::Call(_)
                | ExprData::RequireString(_)
                | ExprData::RequireResolveString(_)
                | ExprData::ImportString(_)
                | ExprData::ImportCall(_) => true,
                ExprData::Dot(dot) => dot.optional_chain != OptionalChain::None,
                ExprData::Index(index) => index.optional_chain != OptionalChain::None,
                _ => false,
            };
        let wrap_lowered_bigint = lower_bigint && (level >= Precedence::New || is_new_target);
        let wrap_forbidden_in = self.forbid_in
            && matches!(data, ExprData::Binary(binary) if binary.op == OpCode::BinaryIn);
        let wrap_destructuring_assignment = (self.stmt_start == Some(self.output.len())
            || self.arrow_expr_start == Some(self.output.len()))
            && matches!(
                data,
                ExprData::Binary(binary)
                    if matches!(binary.left.data.as_deref(), Some(ExprData::Object(_)))
            );
        let wrap = own_level < level
            || (has_pure_comment && level >= Precedence::Postfix)
            || wrap_for_new_target
            || wrap_lowered_bigint
            || wrap_forbidden_in
            || wrap_destructuring_assignment;
        if wrap {
            self.output.push(b'(');
        }
        let old_forbid_in = self.forbid_in;
        if wrap {
            self.forbid_in = false;
        }
        match data {
            ExprData::Missing => {}
            ExprData::Null => self.output.extend_from_slice(b"null"),
            ExprData::Undefined => {
                let wrap_undefined = level >= Precedence::Prefix;
                if wrap_undefined {
                    self.output.push(b'(');
                }
                self.output.extend_from_slice(b"void 0");
                if wrap_undefined {
                    self.output.push(b')');
                }
            }
            ExprData::Boolean(value) => {
                if self.options.minify_syntax {
                    let wrap_boolean = level >= Precedence::Prefix;
                    if wrap_boolean {
                        self.output.push(b'(');
                    }
                    self.output
                        .extend_from_slice(if *value { b"!0" } else { b"!1" });
                    if wrap_boolean {
                        self.output.push(b')');
                    }
                } else {
                    self.output
                        .extend_from_slice(if *value { b"true" } else { b"false" });
                }
            }
            ExprData::Number(value) => self.output.extend_from_slice(
                format_number(*value, level, self.options, self.with_nesting != 0).as_bytes(),
            ),
            ExprData::BigInt(value) if !lower_bigint => {
                self.output.extend_from_slice(value.as_bytes());
                self.output.push(b'n');
            }
            ExprData::BigInt(value) => {
                if has_pure_comment {
                    self.output.extend_from_slice(b"/* @__PURE__ */ ");
                }
                let decimal = self.options.minify_syntax.then(|| bigint_to_decimal(value));
                let use_quotes = decimal
                    .as_deref()
                    .is_none_or(|decimal| !bigint_can_be_exact_number(decimal));
                let printed_value = decimal
                    .as_deref()
                    .filter(|decimal| decimal.len() < value.len())
                    .unwrap_or(value);
                self.output.extend_from_slice(b"BigInt(");
                if use_quotes {
                    self.output.push(b'"');
                }
                self.output.extend_from_slice(printed_value.as_bytes());
                if use_quotes {
                    self.output.push(b'"');
                }
                self.output.push(b')');
            }
            ExprData::String(value) => {
                if value.has_property_key_comment && !self.options.minify_whitespace {
                    self.output.extend_from_slice(b"/* @__KEY__ */ ");
                }
                if value.prefer_template
                    && !self.options.minify_syntax
                    && !self
                        .options
                        .unsupported_features
                        .contains(JsFeature::TEMPLATE_LITERAL)
                {
                    self.output
                        .extend(quote_utf16_with_quote(&value.value, self.options, b'`'));
                } else {
                    self.output
                        .extend(quote_utf16(&value.value, self.options, true));
                }
            }
            ExprData::RegExp(value) => self.output.extend_from_slice(value.as_bytes()),
            ExprData::This => self.output.extend_from_slice(b"this"),
            ExprData::Super => self.output.extend_from_slice(b"super"),
            ExprData::NewTarget(_) => self.output.extend_from_slice(b"new.target"),
            ExprData::ImportMeta(_) => self.output.extend_from_slice(b"import.meta"),
            ExprData::Identifier(identifier) => {
                self.print_symbol_expr(identifier.reference);
            }
            ExprData::ImportIdentifier(identifier) => {
                let reference = self.renamer.canonical_ref_for_symbol(identifier.reference);
                if let Some(value) = self
                    .linker_options
                    .and_then(|options| options.const_values)
                    .and_then(|values| values.get(&reference))
                {
                    self.print_expr_at(
                        &crate::internal::js_ast::const_value_to_expr(expr.loc, value),
                        level,
                    );
                } else {
                    self.print_import_symbol_expr(
                        identifier.reference,
                        identifier.prefer_quoted_key,
                    );
                }
            }
            ExprData::PrivateIdentifier(identifier) => {
                self.print_identifier(&self.renamer.name_for_symbol(identifier.reference));
            }
            ExprData::NameOfSymbol(name) => {
                if name.has_property_key_comment && !self.options.minify_whitespace {
                    self.output.extend_from_slice(b"/* @__KEY__ */ ");
                }
                let value: Vec<u16> = self
                    .renamer
                    .name_for_symbol(name.reference)
                    .encode_utf16()
                    .collect();
                self.output.extend(quote_utf16(&value, self.options, true));
            }
            ExprData::Array(array) => {
                self.output.push(b'[');
                let old_forbid_in = self.forbid_in;
                self.forbid_in = false;
                let is_multi_line = !self.options.minify_whitespace
                    && ((!array.items.is_empty() && !array.is_single_line)
                        || array
                            .items
                            .iter()
                            .any(|item| self.will_print_expr_comments_at_loc(item.loc))
                        || self.will_print_expr_comments_at_loc(array.close_bracket_loc));
                if is_multi_line {
                    self.indent += 1;
                }
                for (index, item) in array.items.iter().enumerate() {
                    if index > 0 {
                        self.output.push(b',');
                    }
                    if is_multi_line {
                        self.print_newline();
                        self.print_indent();
                    } else if index > 0 {
                        self.print_optional_space();
                    }
                    self.print_expr_at(item, Precedence::Spread);
                }
                if matches!(
                    array.items.last().and_then(|item| item.data.as_deref()),
                    Some(ExprData::Missing)
                ) {
                    self.output.push(b',');
                }
                if is_multi_line {
                    self.print_newline();
                    self.print_expr_comments_after_close_token_at_loc(array.close_bracket_loc);
                    self.indent -= 1;
                    self.print_indent();
                }
                self.forbid_in = old_forbid_in;
                self.output.push(b']');
            }
            ExprData::Object(object) => {
                let is_multi_line = !self.options.minify_whitespace
                    && ((!object.properties.is_empty() && !object.is_single_line)
                        || self.will_print_expr_comments_at_loc(object.close_brace_loc)
                        || object
                            .properties
                            .iter()
                            .any(|property| self.will_print_expr_comments_at_loc(property.loc)));
                let output_len = self.output.len();
                let wrap = self.stmt_start == Some(output_len)
                    || self.arrow_expr_start == Some(output_len);
                if wrap {
                    self.output.push(b'(');
                }
                self.output.push(b'{');
                if is_multi_line {
                    self.indent += 1;
                } else if !object.properties.is_empty() {
                    self.print_optional_space();
                }
                for (index, property) in object.properties.iter().enumerate() {
                    if index > 0 {
                        self.output.push(b',');
                    }
                    if is_multi_line {
                        self.print_newline();
                        self.print_indent();
                    } else if index > 0 {
                        self.print_optional_space();
                    }
                    self.print_expr_comments_at_loc(property.loc);
                    if property.kind == PropertyKind::Spread {
                        self.output.extend_from_slice(b"...");
                        self.print_expr_at(&property.value_or_nil, Precedence::Spread);
                        continue;
                    }
                    if property.kind.is_method_definition()
                        && let Some(ExprData::Function(function)) =
                            property.value_or_nil.data.as_deref()
                    {
                        match property.kind {
                            PropertyKind::Getter => self.output.extend_from_slice(b"get "),
                            PropertyKind::Setter => self.output.extend_from_slice(b"set "),
                            _ => {}
                        }
                        if function.function.is_async {
                            self.output.extend_from_slice(b"async ");
                        }
                        if function.function.is_generator {
                            self.output.push(b'*');
                        }
                        self.print_class_key(property);
                        self.print_function_arguments(&function.function);
                        self.print_optional_space();
                        self.print_block(&function.function.body.block, false);
                        continue;
                    }
                    self.print_class_key(property);
                    let key_name = property.key.data.as_deref().and_then(|key| match key {
                        ExprData::String(key) => Some(String::from_utf16_lossy(&key.value)),
                        _ => None,
                    });
                    let shorthand_value_name = match property.value_or_nil.data.as_deref() {
                        Some(ExprData::Identifier(value)) => {
                            Some(self.renamer.name_for_symbol(value.reference))
                        }
                        Some(ExprData::ImportIdentifier(value)) => {
                            let reference = self.renamer.canonical_ref_for_symbol(value.reference);
                            let is_constant = self
                                .linker_options
                                .and_then(|options| options.const_values)
                                .and_then(|values| values.get(&reference))
                                .is_some_and(|value| value.kind != ConstValueKind::None);
                            if self.renamer.namespace_alias_for_symbol(reference).is_none()
                                && !is_constant
                            {
                                Some(self.renamer.name_for_symbol(reference))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    let is_shorthand = property.value_or_nil.data.is_none()
                        || (!property.flags.contains(PropertyFlags::IS_COMPUTED)
                            && !property.flags.contains(PropertyFlags::PREFER_QUOTED_KEY)
                            && !self
                                .options
                                .unsupported_features
                                .contains(JsFeature::OBJECT_EXTENSIONS)
                            && !self.will_print_expr_comments_at_loc(property.value_or_nil.loc)
                            && key_name == shorthand_value_name
                            && key_name.as_deref().is_some_and(|name| {
                                name != "__proto__"
                                    || property.flags.contains(PropertyFlags::WAS_SHORTHAND)
                            }));
                    if !is_shorthand {
                        self.output.push(b':');
                        self.print_optional_space();
                        let old_forbid_in = self.forbid_in;
                        self.forbid_in = false;
                        self.print_expr_without_leading_newline(
                            &property.value_or_nil,
                            Precedence::Spread,
                        );
                        self.forbid_in = old_forbid_in;
                    }
                    if property.initializer_or_nil.data.is_some() {
                        self.print_optional_space();
                        self.output.push(b'=');
                        self.print_optional_space();
                        self.print_expr_without_leading_newline(
                            &property.initializer_or_nil,
                            Precedence::Spread,
                        );
                    }
                }
                if is_multi_line {
                    self.print_newline();
                    self.print_expr_comments_after_close_token_at_loc(object.close_brace_loc);
                    self.indent -= 1;
                    self.print_indent();
                } else if !object.properties.is_empty() {
                    self.print_optional_space();
                }
                self.output.push(b'}');
                if wrap {
                    self.output.push(b')');
                }
            }
            ExprData::Spread(spread) => {
                self.output.extend_from_slice(b"...");
                self.print_expr_at(&spread.value, Precedence::Spread);
            }
            ExprData::Unary(unary) => {
                let operator = unary.op.table_entry();
                if matches!(
                    unary.op,
                    OpCode::UnaryPostDecrement | OpCode::UnaryPostIncrement
                ) {
                    self.print_expr_at(&unary.value, Precedence::Postfix);
                    self.output.extend_from_slice(operator.text.as_bytes());
                } else {
                    self.output.extend_from_slice(operator.text.as_bytes());
                    if operator.is_keyword {
                        self.output.push(b' ');
                    } else if unary.op == OpCode::UnaryNot && self.space_after_unary_not {
                        self.output.push(b' ');
                    } else if (unary.op == OpCode::UnaryPositive
                        && expression_starts_with_sign(&unary.value, true))
                        || (unary.op == OpCode::UnaryNegative
                            && expression_starts_with_sign(&unary.value, false))
                    {
                        self.output.push(b' ');
                    }
                    if unary.op == OpCode::UnaryDelete {
                        self.print_expr_with_substitution_context(
                            &unary.value,
                            Precedence::Prefix,
                            false,
                            false,
                            SubstitutionContext::DeleteTarget,
                        );
                    } else {
                        self.print_expr_at(&unary.value, Precedence::Prefix);
                    }
                }
            }
            ExprData::Binary(binary) => {
                let operator = binary.op.table_entry();
                let higher = higher_precedence(operator.level);
                let left_level = if binary.op == OpCode::BinaryNullishCoalescing
                    && matches!(binary.left.data.as_deref(), Some(ExprData::Binary(left)) if matches!(left.op, OpCode::BinaryLogicalOr | OpCode::BinaryLogicalAnd))
                {
                    Precedence::Prefix
                } else if binary.op == OpCode::BinaryPower
                    && power_left_requires_parentheses(&binary.left, self.options.minify_syntax)
                {
                    Precedence::Call
                } else if binary.op.is_right_associative() {
                    higher
                } else {
                    operator.level
                };
                self.print_expr_at_with_usage(
                    &binary.left,
                    left_level,
                    binary.op == OpCode::BinaryComma,
                );
                self.print_binary_operator(binary.op, &binary.right);
                if self.options.minify_whitespace
                    && ((binary.op == OpCode::BinaryAdd
                        && expression_starts_with_sign(&binary.right, true))
                        || (binary.op == OpCode::BinarySubtract
                            && expression_starts_with_sign(&binary.right, false)))
                {
                    self.output.push(b' ');
                }
                let right_level = if binary.op == OpCode::BinaryNullishCoalescing
                    && matches!(binary.right.data.as_deref(), Some(ExprData::Binary(right)) if matches!(right.op, OpCode::BinaryLogicalOr | OpCode::BinaryLogicalAnd))
                {
                    Precedence::Prefix
                } else if binary.op.binary_assign_target() != AssignTarget::None
                    && matches!(binary.right.data.as_deref(), Some(ExprData::Yield(_)))
                {
                    Precedence::Yield
                } else if binary.op.is_right_associative() {
                    operator.level
                } else if binary.op == OpCode::BinaryComma {
                    operator.level
                } else {
                    higher
                };
                let old_space_after_unary_not = self.space_after_unary_not;
                self.space_after_unary_not = binary.op == OpCode::BinaryLessThan
                    && matches!(
                        binary.right.data.as_deref(),
                        Some(ExprData::Unary(unary))
                            if unary.op == OpCode::UnaryNot
                                && expression_starts_with_sign(&unary.value, false)
                    );
                if binary.op.is_right_associative() {
                    self.print_expr_at_with_usage(
                        &binary.right,
                        right_level,
                        binary.op == OpCode::BinaryComma && result_is_unused,
                    );
                } else {
                    self.print_expr_at_with_usage(
                        &binary.right,
                        right_level,
                        binary.op == OpCode::BinaryComma && result_is_unused,
                    );
                }
                self.space_after_unary_not = old_space_after_unary_not;
            }
            ExprData::If(conditional) => {
                self.print_expr_at(
                    &conditional.test,
                    higher_precedence(Precedence::Conditional),
                );
                self.print_optional_space();
                self.output.push(b'?');
                self.print_optional_space();
                self.print_expr_at(&conditional.yes, Precedence::Spread);
                self.print_optional_space();
                self.output.push(b':');
                self.print_optional_space();
                self.print_expr_at(&conditional.no, Precedence::Assign);
            }
            ExprData::Dot(dot) => {
                let target_is_for_of_let = self.for_of_init_start
                    && matches!(
                        dot.target.data.as_deref(),
                        Some(ExprData::Identifier(identifier))
                            if self.renamer.original_name_for_symbol(identifier.reference) == "let"
                    );
                if dot.optional_chain == OptionalChain::None
                    && (is_optional_chain(&dot.target) || target_is_for_of_let)
                {
                    self.output.push(b'(');
                    self.print_expr_at(&dot.target, Precedence::Lowest);
                    self.output.push(b')');
                } else {
                    let target_start = self.output.len();
                    self.print_expr_at_with_usage_and_new_target(
                        &dot.target,
                        Precedence::Postfix,
                        false,
                        is_new_target && dot.optional_chain == OptionalChain::None,
                    );
                    if matches!(dot.target.data.as_deref(), Some(ExprData::Number(_)))
                        && self.output[target_start..].iter().all(u8::is_ascii_digit)
                        && dot.optional_chain != OptionalChain::Start
                        && is_identifier_es5_and_es_next(&dot.name)
                    {
                        self.output.push(b' ');
                    }
                }
                if is_identifier_es5_and_es_next(&dot.name) {
                    if dot.optional_chain == OptionalChain::Start {
                        self.output.extend_from_slice(b"?.");
                    } else {
                        self.output.push(b'.');
                    }
                    self.print_identifier(&dot.name);
                } else {
                    if dot.optional_chain == OptionalChain::Start {
                        self.output.extend_from_slice(b"?.");
                    }
                    self.output.push(b'[');
                    self.output.extend(quote_utf16(
                        &dot.name.encode_utf16().collect::<Vec<_>>(),
                        self.options,
                        true,
                    ));
                    self.output.push(b']');
                }
            }
            ExprData::Index(index) => {
                let inlined_dot_name = if self.options.minify_syntax {
                    let inlined = match index.index.data.as_deref() {
                        Some(ExprData::InlinedEnum(value)) => Some(value.value.clone()),
                        _ => self
                            .substitute_imported_enum(&index.index)
                            .and_then(|replacement| match replacement.data.as_deref() {
                                Some(ExprData::InlinedEnum(value)) => Some(value.value.clone()),
                                _ => None,
                            }),
                    };
                    inlined.and_then(|value| match value.data.as_deref() {
                        Some(ExprData::String(value)) => {
                            let name = String::from_utf16_lossy(&value.value);
                            is_identifier_es5_and_es_next(&name).then_some(name)
                        }
                        _ => None,
                    })
                } else {
                    None
                };
                if index.optional_chain == OptionalChain::None && is_optional_chain(&index.target) {
                    self.output.push(b'(');
                    self.print_expr_at(&index.target, Precedence::Lowest);
                    self.output.push(b')');
                } else {
                    self.print_expr_at_with_usage_and_new_target(
                        &index.target,
                        Precedence::Postfix,
                        false,
                        is_new_target && index.optional_chain == OptionalChain::None,
                    );
                }
                if let Some(name) = inlined_dot_name {
                    self.output.extend_from_slice(
                        if index.optional_chain == OptionalChain::Start {
                            b"?."
                        } else {
                            b"."
                        },
                    );
                    self.print_identifier(&name);
                } else if matches!(
                    index.index.data.as_deref(),
                    Some(ExprData::PrivateIdentifier(_))
                ) {
                    self.output.extend_from_slice(
                        if index.optional_chain == OptionalChain::Start {
                            b"?."
                        } else {
                            b"."
                        },
                    );
                    self.print_expr_at(&index.index, Precedence::Member);
                } else {
                    if index.optional_chain == OptionalChain::Start {
                        self.output.extend_from_slice(b"?.");
                    }
                    self.output.push(b'[');
                    let old_forbid_in = self.forbid_in;
                    self.forbid_in = false;
                    self.print_expr_at(&index.index, Precedence::Lowest);
                    self.forbid_in = old_forbid_in;
                    self.output.push(b']');
                }
            }
            ExprData::Call(call) => {
                if has_pure_comment {
                    self.output.extend_from_slice(b"/* @__PURE__ */ ");
                }
                let target_is_unbound_eval = matches!(
                    call.target.data.as_deref(),
                    Some(ExprData::Identifier(identifier))
                        if self.renamer.original_name_for_symbol(identifier.reference) == "eval"
                );
                let target_became_property_access = call.kind
                    != CallKind::TargetWasOriginallyPropertyAccess
                    && match call.target.data.as_deref() {
                        Some(ExprData::Dot(_) | ExprData::Index(_)) => true,
                        Some(ExprData::ImportIdentifier(identifier)) => {
                            identifier.was_originally_identifier
                                && self
                                    .renamer
                                    .namespace_alias_for_symbol(identifier.reference)
                                    .is_some()
                        }
                        Some(ExprData::Identifier(identifier)) => self
                            .renamer
                            .namespace_alias_for_symbol(identifier.reference)
                            .is_some(),
                        _ => false,
                    };
                let force_indirect_call = (target_is_unbound_eval
                    && call.kind != CallKind::DirectEval
                    && call.optional_chain == OptionalChain::None)
                    || target_became_property_access;
                let postfix_target_needs_wrap = matches!(
                    call.target.data.as_deref(),
                    Some(ExprData::Unary(unary))
                        if matches!(unary.op, OpCode::UnaryPostDecrement | OpCode::UnaryPostIncrement)
                );
                if force_indirect_call {
                    self.output.extend_from_slice(b"(0,");
                    self.print_optional_space();
                    self.print_expr_with_substitution_context(
                        &call.target,
                        Precedence::Postfix,
                        false,
                        false,
                        SubstitutionContext::CallTargetOrTemplateTag,
                    );
                    self.output.push(b')');
                } else if call.optional_chain == OptionalChain::None
                    && (is_optional_chain(&call.target) || postfix_target_needs_wrap)
                {
                    self.output.push(b'(');
                    self.print_expr_with_substitution_context(
                        &call.target,
                        Precedence::Lowest,
                        false,
                        false,
                        SubstitutionContext::CallTargetOrTemplateTag,
                    );
                    self.output.push(b')');
                } else {
                    self.print_expr_with_substitution_context(
                        &call.target,
                        Precedence::Postfix,
                        false,
                        false,
                        SubstitutionContext::CallTargetOrTemplateTag,
                    );
                }
                if call.optional_chain == OptionalChain::Start {
                    self.output.extend_from_slice(b"?.");
                }
                self.print_arguments(&call.args, call.close_paren_loc, call.is_multi_line);
            }
            ExprData::New(new) => {
                if has_pure_comment {
                    self.output.extend_from_slice(b"/* @__PURE__ */ ");
                }
                self.output.extend_from_slice(b"new");
                if !self.options.minify_whitespace
                    || new_target_needs_space(&new.target, self.options)
                {
                    self.output.push(b' ');
                }
                self.print_expr_at_with_usage_and_new_target(
                    &new.target,
                    Precedence::New,
                    false,
                    true,
                );
                if !self.options.minify_whitespace
                    || !new.args.is_empty()
                    || level >= Precedence::Postfix
                {
                    self.print_arguments(&new.args, new.close_paren_loc, new.is_multi_line);
                }
            }
            ExprData::InlinedEnum(inlined) => {
                self.print_expr_at_with_usage_and_new_target(
                    &inlined.value,
                    level,
                    result_is_unused,
                    is_new_target,
                );
                if !self.options.minify_whitespace && !inlined.comment.contains("*/") {
                    self.output.extend_from_slice(b" /* ");
                    self.output.extend_from_slice(inlined.comment.as_bytes());
                    self.output.extend_from_slice(b" */");
                }
            }
            ExprData::Annotation(annotation) => {
                self.print_expr_at_with_usage_and_new_target(
                    &annotation.value,
                    level,
                    result_is_unused,
                    is_new_target,
                );
            }
            ExprData::Await(await_expression) => {
                self.output.extend_from_slice(b"await ");
                self.print_expr_at(&await_expression.value, Precedence::Prefix);
            }
            ExprData::Yield(yield_expression) => {
                if yield_expression.is_star {
                    self.output.extend_from_slice(b"yield*");
                    self.print_optional_space();
                } else {
                    self.output.extend_from_slice(b"yield");
                    if yield_expression.value_or_nil.data.is_some() {
                        self.output.push(b' ');
                    }
                }
                self.print_expr_without_leading_newline(
                    &yield_expression.value_or_nil,
                    Precedence::Yield,
                );
            }
            ExprData::Function(function) => {
                let wrap = function.is_parenthesized
                    || self.stmt_start == Some(self.output.len())
                    || self.export_default_start == Some(self.output.len());
                if wrap {
                    self.output.push(b'(');
                }
                if !self.options.minify_whitespace && function.function.has_no_side_effects_comment
                {
                    self.output
                        .extend_from_slice(b"/* @__NO_SIDE_EFFECTS__ */ ");
                }
                self.print_function(&function.function);
                if wrap {
                    self.output.push(b')');
                }
            }
            ExprData::Arrow(arrow) => {
                let wrap_arrow = arrow.is_parenthesized && !wrap;
                if wrap_arrow {
                    self.output.push(b'(');
                }
                if !self.options.minify_whitespace && arrow.has_no_side_effects_comment {
                    self.output
                        .extend_from_slice(b"/* @__NO_SIDE_EFFECTS__ */ ");
                }
                let can_omit_parameter_parentheses = self.options.minify_whitespace
                    && !arrow.has_rest_arg
                    && matches!(
                        arrow.args.as_slice(),
                        [argument]
                            if argument.default_or_nil.data.is_none()
                                && matches!(
                                    argument.binding.data.as_deref(),
                                    Some(BindingData::Identifier(_))
                                )
                    );
                if arrow.is_async {
                    self.output.extend_from_slice(b"async");
                    if can_omit_parameter_parentheses {
                        self.output.push(b' ');
                    } else {
                        self.print_optional_space();
                    }
                }
                if !can_omit_parameter_parentheses {
                    self.output.push(b'(');
                }
                for (index, argument) in arrow.args.iter().enumerate() {
                    if index > 0 {
                        self.output.push(b',');
                        self.print_optional_space();
                    }
                    if arrow.has_rest_arg && index + 1 == arrow.args.len() {
                        self.output.extend_from_slice(b"...");
                    }
                    self.print_binding(&argument.binding);
                    if argument.default_or_nil.data.is_some() {
                        self.print_optional_space();
                        self.output.push(b'=');
                        self.print_optional_space();
                        self.print_expr_at(&argument.default_or_nil, Precedence::Spread);
                    }
                }
                if !can_omit_parameter_parentheses {
                    self.output.push(b')');
                }
                self.print_optional_space();
                self.output.extend_from_slice(b"=>");
                self.print_optional_space();
                let old_forbid_in = self.forbid_in;
                self.forbid_in = false;
                if arrow.prefer_expr
                    && let [statement] = arrow.body.block.statements.as_slice()
                    && let Some(StmtData::Return(return_statement)) = statement.data.as_deref()
                    && return_statement.value_or_nil.data.is_some()
                {
                    let wrap_body = matches!(
                        return_statement.value_or_nil.data.as_deref(),
                        Some(ExprData::Binary(binary)) if binary.op == OpCode::BinaryComma
                    );
                    if wrap_body {
                        self.output.push(b'(');
                    }
                    let old_arrow_expr_start = self.arrow_expr_start;
                    self.arrow_expr_start = Some(self.output.len());
                    self.print_expr_without_leading_newline(
                        &return_statement.value_or_nil,
                        Precedence::Comma,
                    );
                    self.arrow_expr_start = old_arrow_expr_start;
                    if wrap_body {
                        self.output.push(b')');
                    }
                } else {
                    self.print_block(&arrow.body.block, false);
                }
                self.forbid_in = old_forbid_in;
                if wrap_arrow {
                    self.output.push(b')');
                }
            }
            ExprData::Class(class) => {
                let print_parentheses = self.stmt_start == Some(self.output.len())
                    || self.export_default_start == Some(self.output.len());
                if print_parentheses {
                    self.output.push(b'(');
                }
                self.print_class(&class.class);
                if print_parentheses {
                    self.output.push(b')');
                }
            }
            ExprData::Template(template) => self.print_template(template, is_new_target),
            ExprData::RequireString(require) => {
                let wrap_as_target = level >= Precedence::New && !wrap;
                if wrap_as_target {
                    self.output.push(b'(');
                }
                let nested_level = if wrap_as_target {
                    Precedence::Lowest
                } else {
                    level
                };
                if !self.print_linked_require_or_import(
                    require.import_record_index,
                    nested_level,
                    result_is_unused,
                ) && !self.print_external_require(require.import_record_index)
                {
                    self.output.extend_from_slice(b"require(");
                    self.print_import_path(require.import_record_index, true);
                    self.output.push(b')');
                }
                if wrap_as_target {
                    self.output.push(b')');
                }
            }
            ExprData::RequireResolveString(require) => {
                let wrap_as_target = level >= Precedence::New && !wrap;
                if wrap_as_target {
                    self.output.push(b'(');
                }
                self.output.extend_from_slice(b"require.resolve(");
                self.print_import_path(require.import_record_index, true);
                self.output.push(b')');
                if wrap_as_target {
                    self.output.push(b')');
                }
            }
            ExprData::ImportString(import) => {
                let wrap_as_target = level >= Precedence::New && !wrap;
                if wrap_as_target {
                    self.output.push(b'(');
                }
                let nested_level = if wrap_as_target {
                    Precedence::Lowest
                } else {
                    level
                };
                if !self.print_linked_require_or_import(
                    import.import_record_index,
                    nested_level,
                    result_is_unused,
                ) && !self.print_external_dynamic_import_fallback(import.import_record_index)
                {
                    let (phase, record_loc) = {
                        let record =
                            &self.import_records[usize::try_from(import.import_record_index)
                                .expect("import record index")];
                        (record.phase, record.range.loc)
                    };
                    let is_multi_line = !self.options.minify_whitespace
                        && (self.will_print_expr_comments_at_loc(record_loc)
                            || self.will_print_expr_comments_at_loc(import.close_paren_loc));
                    self.print_import_start(phase);
                    if is_multi_line {
                        self.indent += 1;
                        self.print_newline();
                        self.print_indent();
                        self.print_expr_comments_at_loc(record_loc);
                    }
                    self.print_import_path(import.import_record_index, false);
                    self.print_import_attributes(import.import_record_index, true, is_multi_line);
                    if is_multi_line {
                        self.print_newline();
                        self.print_expr_comments_after_close_token_at_loc(import.close_paren_loc);
                        self.indent -= 1;
                        self.print_indent();
                    }
                    self.output.push(b')');
                }
                if wrap_as_target {
                    self.output.push(b')');
                }
            }
            ExprData::ImportCall(import) => {
                let is_multi_line = !self.options.minify_whitespace
                    && (self.will_print_expr_comments_at_loc(import.expr.loc)
                        || (import.options_or_nil.data.is_some()
                            && self.will_print_expr_comments_at_loc(import.options_or_nil.loc))
                        || self.will_print_expr_comments_at_loc(import.close_paren_loc));
                self.print_import_start(import.phase);
                if is_multi_line {
                    self.indent += 1;
                    self.print_newline();
                    self.print_indent();
                }
                self.print_expr_at(&import.expr, Precedence::Spread);
                if import.options_or_nil.data.is_some() {
                    self.output.push(b',');
                    if is_multi_line {
                        self.print_newline();
                        self.print_indent();
                    } else {
                        self.print_optional_space();
                    }
                    self.print_expr_at(&import.options_or_nil, Precedence::Spread);
                }
                if is_multi_line {
                    self.print_newline();
                    self.print_expr_comments_after_close_token_at_loc(import.close_paren_loc);
                    self.indent -= 1;
                    self.print_indent();
                }
                self.output.push(b')');
            }
            ExprData::JsxElement(element) => self.print_jsx_element(element),
            ExprData::JsxText(text) => self.output.extend_from_slice(text.raw.as_bytes()),
        }
        self.forbid_in = old_forbid_in;
        if wrap {
            self.output.push(b')');
        }
    }

    fn print_linked_require_or_import(
        &mut self,
        import_record_index: u32,
        level: Precedence,
        result_is_unused: bool,
    ) -> bool {
        let Some(linker_options) = self.linker_options else {
            return false;
        };
        let record = self.import_records
            [usize::try_from(import_record_index).expect("import record index")]
        .clone();
        if !record.source_index.is_valid() {
            return false;
        }
        let mut meta =
            (linker_options.require_or_import_meta_for_source)(record.source_index.get_index());
        if result_is_unused {
            meta.exports_ref = INVALID_REF;
        }

        if record.kind == ImportKind::Dynamic && meta.is_wrapper_async {
            self.print_symbol(meta.wrapper_ref);
            self.output.extend_from_slice(b"()");
            if meta.exports_ref != INVALID_REF {
                self.print_then_prefix();
                self.print_symbol(meta.exports_ref);
                self.print_then_suffix();
            }
            return true;
        }

        let expression_level = if record.kind == ImportKind::Dynamic {
            self.output.extend_from_slice(b"Promise.resolve()");
            self.print_then_prefix()
        } else {
            level
        };
        let wrap_comma = meta.exports_ref != INVALID_REF && expression_level >= Precedence::Comma;
        if wrap_comma {
            self.output.push(b'(');
        }

        let wrap_with_to_esm = record.flags.contains(ImportRecordFlags::WRAP_WITH_TO_ESM);
        if wrap_with_to_esm {
            self.print_symbol(linker_options.to_esm_ref);
            self.output.push(b'(');
        }

        self.print_symbol(meta.wrapper_ref);
        self.output.extend_from_slice(b"()");
        if meta.exports_ref != INVALID_REF {
            self.output.push(b',');
            self.print_optional_space();
            let wrap_with_to_cjs = record.flags.contains(ImportRecordFlags::WRAP_WITH_TO_CJS);
            if wrap_with_to_cjs {
                self.print_symbol(linker_options.to_common_js_ref);
                self.output.push(b'(');
            }
            self.print_symbol(meta.exports_ref);
            if wrap_with_to_cjs {
                self.output.push(b')');
            }
        }

        if wrap_with_to_esm {
            if self.module_type_is_esm {
                self.output.push(b',');
                self.print_optional_space();
                self.output.push(b'1');
            }
            self.output.push(b')');
        }
        if wrap_comma {
            self.output.push(b')');
        }
        if record.kind == ImportKind::Dynamic {
            self.print_then_suffix();
        }
        true
    }

    fn print_external_require(&mut self, import_record_index: u32) -> bool {
        let Some(linker_options) = self.linker_options else {
            return false;
        };
        let record = self.import_records
            [usize::try_from(import_record_index).expect("import record index")]
        .clone();
        if record.source_index.is_valid()
            || record.kind == ImportKind::Dynamic
            || (!record.flags.contains(ImportRecordFlags::WRAP_WITH_TO_ESM)
                && !record
                    .flags
                    .contains(ImportRecordFlags::CALL_RUNTIME_REQUIRE))
        {
            return false;
        }

        let wrap_with_to_esm = record.flags.contains(ImportRecordFlags::WRAP_WITH_TO_ESM);
        if wrap_with_to_esm {
            self.print_symbol(linker_options.to_esm_ref);
            self.output.push(b'(');
        }
        if record
            .flags
            .contains(ImportRecordFlags::CALL_RUNTIME_REQUIRE)
        {
            self.print_symbol(linker_options.runtime_require_ref);
        } else {
            self.output.extend_from_slice(b"require");
        }
        self.output.push(b'(');
        self.print_import_path(import_record_index, true);
        self.output.push(b')');
        if wrap_with_to_esm {
            if self.module_type_is_esm {
                self.output.push(b',');
                self.print_optional_space();
                self.output.push(b'1');
            }
            self.output.push(b')');
        }
        true
    }

    fn print_external_dynamic_import_fallback(&mut self, import_record_index: u32) -> bool {
        let Some(linker_options) = self.linker_options else {
            return false;
        };
        let record = self.import_records
            [usize::try_from(import_record_index).expect("import record index")]
        .clone();
        if record.source_index.is_valid()
            || record.kind != ImportKind::Dynamic
            || !self
                .options
                .unsupported_features
                .contains(JsFeature::DYNAMIC_IMPORT)
        {
            return false;
        }

        self.output.extend_from_slice(b"Promise.resolve()");
        self.print_then_prefix();
        let wrap_with_to_esm = record.flags.contains(ImportRecordFlags::WRAP_WITH_TO_ESM);
        if wrap_with_to_esm {
            self.print_symbol(linker_options.to_esm_ref);
            self.output.push(b'(');
        }
        if record
            .flags
            .contains(ImportRecordFlags::CALL_RUNTIME_REQUIRE)
        {
            self.print_symbol(linker_options.runtime_require_ref);
        } else {
            self.output.extend_from_slice(b"require");
        }
        self.output.push(b'(');
        self.print_import_path(import_record_index, true);
        self.output.push(b')');
        if wrap_with_to_esm {
            if self.module_type_is_esm {
                self.output.push(b',');
                self.print_optional_space();
                self.output.push(b'1');
            }
            self.output.push(b')');
        }
        self.print_then_suffix();
        true
    }

    fn print_then_prefix(&mut self) -> Precedence {
        if self.options.unsupported_features.contains(JsFeature::ARROW) {
            self.output.extend_from_slice(b".then(function()");
            self.print_optional_space();
            self.output.push(b'{');
            self.print_newline();
            self.indent += 1;
            self.print_indent();
            self.output.extend_from_slice(b"return ");
            Precedence::Lowest
        } else {
            self.output.extend_from_slice(b".then(()");
            self.print_optional_space();
            self.output.extend_from_slice(b"=>");
            self.print_optional_space();
            Precedence::Spread
        }
    }

    fn print_then_suffix(&mut self) {
        if self.options.unsupported_features.contains(JsFeature::ARROW) {
            if !self.options.minify_whitespace {
                self.output.push(b';');
            }
            self.print_newline();
            self.indent -= 1;
            self.print_indent();
            self.output.extend_from_slice(b"})");
        } else {
            self.output.push(b')');
        }
    }

    fn print_symbol(&mut self, reference: Ref) {
        self.print_identifier(&self.renamer.name_for_symbol(reference));
    }

    fn print_arguments(
        &mut self,
        arguments: &[Expr],
        close_paren_loc: crate::internal::logger::Loc,
        was_multi_line: bool,
    ) {
        self.output.push(b'(');
        let old_forbid_in = self.forbid_in;
        self.forbid_in = false;
        let is_multi_line = !self.options.minify_whitespace
            && ((was_multi_line && !arguments.is_empty())
                || arguments
                    .iter()
                    .any(|argument| self.will_print_expr_comments_at_loc(argument.loc))
                || self.will_print_expr_comments_at_loc(close_paren_loc));
        if is_multi_line {
            self.indent += 1;
        }
        for (index, argument) in arguments.iter().enumerate() {
            if index > 0 {
                self.output.push(b',');
            }
            if is_multi_line {
                self.print_newline();
                self.print_indent();
            } else if index > 0 {
                self.print_optional_space();
            }
            self.print_expr_at(argument, Precedence::Spread);
        }
        if is_multi_line {
            self.print_newline();
            self.print_expr_comments_after_close_token_at_loc(close_paren_loc);
            self.indent -= 1;
            self.print_indent();
        }
        self.forbid_in = old_forbid_in;
        self.output.push(b')');
    }

    fn print_property_key(&mut self, key: &Expr) {
        if let Some(ExprData::String(string)) = key.data.as_deref() {
            let name = String::from_utf16_lossy(&string.value);
            if is_identifier_es5_and_es_next(&name) {
                self.print_identifier(&name);
            } else {
                self.output
                    .extend(quote_utf16(&string.value, self.options, false));
            }
        } else {
            self.print_expr_at(key, Precedence::Lowest);
        }
    }

    fn print_template(
        &mut self,
        template: &crate::internal::js_ast::TemplateExpr,
        is_new_target: bool,
    ) {
        let mut rewritten_template = None;
        if template.tag_or_nil.data.is_none() && self.options.minify_syntax {
            let mut replacement = template.clone();
            let mut changed = false;
            for part in &mut replacement.parts {
                if let Some(ExprData::NameOfSymbol(name)) = part.value.data.as_deref() {
                    part.value = Expr::new(
                        part.value.loc,
                        ExprData::String(StringExpr {
                            value: self
                                .renamer
                                .name_for_symbol(name.reference)
                                .encode_utf16()
                                .collect(),
                            has_property_key_comment: name.has_property_key_comment,
                            ..StringExpr::default()
                        }),
                    );
                    changed = true;
                } else if let Some(value) = self.substitute_imported_enum(&part.value) {
                    part.value = match value.data.as_deref() {
                        Some(ExprData::InlinedEnum(value)) => value.value.clone(),
                        _ => value,
                    };
                    changed = true;
                }
            }
            if changed {
                match inline_primitives_into_template(
                    crate::internal::logger::Loc::default(),
                    &replacement,
                )
                .data
                .map(|data| *data)
                {
                    Some(ExprData::String(value)) => {
                        self.output
                            .extend(quote_utf16(&value.value, self.options, true));
                        return;
                    }
                    Some(ExprData::Template(value)) => rewritten_template = Some(value),
                    _ => {}
                }
            }
        }
        let template = rewritten_template.as_ref().unwrap_or(template);
        let is_tagged = template.tag_or_nil.data.is_some();
        if is_tagged {
            let tag_is_property_access = matches!(
                template.tag_or_nil.data.as_deref(),
                Some(ExprData::Dot(_) | ExprData::Index(_))
            );
            if !template.tag_was_originally_property_access && tag_is_property_access {
                self.output.extend_from_slice(b"(0,");
                self.print_optional_space();
                self.print_expr_at(&template.tag_or_nil, Precedence::Lowest);
                self.output.push(b')');
            } else if is_optional_chain(&template.tag_or_nil) {
                self.output.push(b'(');
                self.print_expr_at(&template.tag_or_nil, Precedence::Lowest);
                self.output.push(b')');
            } else {
                self.print_expr_with_substitution_context(
                    &template.tag_or_nil,
                    Precedence::Postfix,
                    false,
                    is_new_target,
                    SubstitutionContext::CallTargetOrTemplateTag,
                );
            }
        } else if template.parts.is_empty() && self.options.minify_syntax {
            self.output
                .extend(quote_utf16(&template.head_cooked, self.options, true));
            return;
        }
        self.output.push(b'`');
        if is_tagged {
            self.output.extend_from_slice(template.head_raw.as_bytes());
        } else {
            print_unquoted_utf16(&mut self.output, &template.head_cooked, b'`', self.options);
        }
        for part in &template.parts {
            self.output.extend_from_slice(b"${");
            self.print_expr_at(&part.value, Precedence::Lowest);
            self.output.push(b'}');
            if is_tagged {
                self.output.extend_from_slice(part.tail_raw.as_bytes());
            } else {
                print_unquoted_utf16(&mut self.output, &part.tail_cooked, b'`', self.options);
            }
        }
        self.output.push(b'`');
    }

    fn print_import_start(&mut self, phase: ImportPhase) {
        self.output.extend_from_slice(match phase {
            ImportPhase::Evaluation => b"import(",
            ImportPhase::Defer => b"import.defer(",
            ImportPhase::Source => b"import.source(",
        });
    }

    fn print_jsx_element(&mut self, element: &crate::internal::js_ast::JsxElementExpr) {
        let is_tag_single_line = element.is_tag_single_line || self.options.minify_whitespace;
        self.output.push(b'<');
        self.print_jsx_tag(&element.tag_or_nil);
        if !is_tag_single_line {
            self.indent += 1;
        }
        for property in &element.properties {
            if is_tag_single_line {
                if self.options.minify_whitespace {
                    if !matches!(property.kind, PropertyKind::Spread)
                        && !property.flags.contains(PropertyFlags::IS_COMPUTED)
                    {
                        self.print_space_before_identifier();
                    }
                } else {
                    self.output.push(b' ');
                }
            } else {
                self.output.push(b'\n');
                self.print_indent();
            }
            if property.kind == PropertyKind::Spread {
                if self.will_print_expr_comments_at_loc(property.loc) {
                    self.output.push(b'{');
                    self.print_newline();
                    self.indent += 1;
                    self.print_indent();
                    self.print_expr_comments_at_loc(property.loc);
                    self.output.extend_from_slice(b"...");
                    self.print_expr_at(&property.value_or_nil, Precedence::Spread);
                    self.print_newline();
                    self.indent -= 1;
                    self.print_indent();
                    self.output.push(b'}');
                } else {
                    self.output.extend_from_slice(b"{...");
                    self.print_expr_at(&property.value_or_nil, Precedence::Spread);
                    self.output.push(b'}');
                }
                continue;
            }
            if property.flags.contains(PropertyFlags::IS_COMPUTED) {
                self.output.extend_from_slice(b"{...{ [");
                self.print_expr_at(&property.key, Precedence::Spread);
                self.output.extend_from_slice(b"]:");
                self.print_optional_space();
                self.print_expr_at(&property.value_or_nil, Precedence::Spread);
                self.output.extend_from_slice(b" }}");
                continue;
            }
            self.print_jsx_attribute_name(&property.key);
            let is_multi_line = self.will_print_expr_comments_at_loc(property.value_or_nil.loc);
            if property.flags.contains(PropertyFlags::WAS_SHORTHAND)
                && matches!(
                    property.value_or_nil.data.as_deref(),
                    Some(ExprData::Boolean(true))
                )
            {
                continue;
            }
            self.output.push(b'=');
            match property.value_or_nil.data.as_deref() {
                Some(ExprData::JsxText(text)) => {
                    self.output.extend_from_slice(text.raw.as_bytes());
                }
                Some(ExprData::JsxElement(_))
                    if property.flags.contains(PropertyFlags::WAS_SHORTHAND) =>
                {
                    self.print_expr_at(&property.value_or_nil, Precedence::Lowest);
                }
                _ => {
                    self.output.push(b'{');
                    if is_multi_line {
                        self.print_newline();
                        self.indent += 1;
                        self.print_indent();
                    }
                    self.print_expr_at(&property.value_or_nil, Precedence::Spread);
                    if is_multi_line {
                        self.print_newline();
                        self.indent -= 1;
                        self.print_indent();
                    }
                    self.output.push(b'}');
                }
            }
        }
        if !is_tag_single_line {
            self.indent -= 1;
            if !element.properties.is_empty() {
                self.output.push(b'\n');
                self.print_indent();
            }
        }
        if element.tag_or_nil.data.is_some() && element.nullable_children.is_empty() {
            if is_tag_single_line || element.properties.is_empty() {
                self.print_optional_space();
            }
            self.output.extend_from_slice(b"/>");
            return;
        }
        self.output.push(b'>');
        for child in &element.nullable_children {
            match child.data.as_deref() {
                None => {
                    self.output.push(b'{');
                    if self.will_print_expr_comments_at_loc(child.loc) {
                        self.print_newline();
                        self.indent += 1;
                        self.print_expr_comments_after_close_token_at_loc(child.loc);
                        self.indent -= 1;
                        self.print_indent();
                    }
                    self.output.push(b'}');
                }
                Some(ExprData::JsxText(text)) => {
                    self.output.extend_from_slice(text.raw.as_bytes());
                }
                Some(ExprData::JsxElement(_)) => {
                    self.print_expr_at(child, Precedence::Lowest);
                }
                _ => {
                    let is_multi_line = self.will_print_expr_comments_at_loc(child.loc);
                    self.output.push(b'{');
                    if is_multi_line {
                        self.print_newline();
                        self.indent += 1;
                        self.print_indent();
                    }
                    self.print_expr_at(child, Precedence::Spread);
                    if is_multi_line {
                        self.print_newline();
                        self.indent -= 1;
                        self.print_indent();
                    }
                    self.output.push(b'}');
                }
            }
        }
        self.output.extend_from_slice(b"</");
        self.print_jsx_tag(&element.tag_or_nil);
        self.output.push(b'>');
    }

    fn print_jsx_tag(&mut self, tag: &Expr) {
        match tag.data.as_deref() {
            None => {}
            Some(ExprData::String(string)) => {
                self.output
                    .extend_from_slice(String::from_utf16_lossy(&string.value).as_bytes());
            }
            Some(ExprData::Identifier(identifier)) => {
                self.output.extend_from_slice(
                    self.renamer
                        .name_for_symbol(identifier.reference)
                        .as_bytes(),
                );
            }
            Some(ExprData::Dot(dot)) => {
                self.print_jsx_tag(&dot.target);
                self.output.push(b'.');
                self.output.extend_from_slice(dot.name.as_bytes());
            }
            _ => self.print_expr_at(tag, Precedence::Lowest),
        }
    }

    fn print_jsx_attribute_name(&mut self, key: &Expr) {
        if let Some(ExprData::String(string)) = key.data.as_deref() {
            self.output
                .extend_from_slice(String::from_utf16_lossy(&string.value).as_bytes());
        } else if let Some(ExprData::NameOfSymbol(name)) = key.data.as_deref() {
            self.output
                .extend_from_slice(self.renamer.name_for_symbol(name.reference).as_bytes());
        } else {
            self.print_expr_at(key, Precedence::Lowest);
        }
    }

    fn print_identifier(&mut self, name: &str) {
        if self.options.ascii_only {
            self.output = quote_identifier(
                std::mem::take(&mut self.output),
                name,
                self.options.unsupported_features,
            );
        } else {
            self.output.extend_from_slice(name.as_bytes());
        }
    }

    fn print_space_before_identifier(&mut self) {
        if self.output.last().is_some_and(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'\\') || *byte >= 0x80
        }) {
            self.output.push(b' ');
        }
    }

    fn print_symbol_expr(&mut self, reference: crate::internal::ast::Ref) {
        self.print_import_symbol_expr(reference, false);
    }

    fn print_import_symbol_expr(
        &mut self,
        reference: crate::internal::ast::Ref,
        prefer_quoted_key: bool,
    ) {
        if let Some(alias) = self.renamer.namespace_alias_for_symbol(reference) {
            self.print_symbol_expr(alias.namespace_ref);
            if !prefer_quoted_key && is_identifier_es5_and_es_next(&alias.alias) {
                self.output.push(b'.');
                self.print_identifier(&alias.alias);
            } else {
                self.output.push(b'[');
                self.output.extend(quote_utf16(
                    &alias.alias.encode_utf16().collect::<Vec<_>>(),
                    self.options,
                    true,
                ));
                self.output.push(b']');
            }
        } else {
            self.print_identifier(&self.renamer.name_for_symbol(reference));
        }
    }

    fn print_binary_operator(&mut self, operator: OpCode, right: &Expr) {
        let entry = operator.table_entry();
        if operator == OpCode::BinaryComma {
            self.output.push(b',');
            self.print_optional_space();
            return;
        }
        if !self.options.minify_whitespace {
            self.output.push(b' ');
        } else if entry.is_keyword {
            if self.output.last().is_some_and(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'_' | b'$' | b'\\' | b'/')
                    || *byte >= 0x80
            }) {
                self.output.push(b' ');
            }
        } else if matches!(
            operator,
            OpCode::BinaryGreaterThan
                | OpCode::BinaryGreaterThanOrEqual
                | OpCode::BinaryShiftRight
                | OpCode::BinaryUnsignedShiftRight
        ) && self.output.ends_with(b"--")
        {
            self.output.push(b' ');
        }
        self.output.extend_from_slice(entry.text.as_bytes());
        if !self.options.minify_whitespace
            || (entry.is_keyword && expression_starts_with_identifier(right, self.options))
            || (operator == OpCode::BinaryDivide
                && matches!(right.data.as_deref(), Some(ExprData::RegExp(_))))
            || (self.output.last() == Some(&b'<') && expression_is_script_regexp(right))
        {
            self.output.push(b' ');
        }
    }

    fn print_optional_space(&mut self) {
        if !self.options.minify_whitespace {
            self.output.push(b' ');
        }
    }
}

fn can_omit_space_after_return(expression: &Expr, options: Options) -> bool {
    match expression.data.as_deref() {
        Some(
            ExprData::Array(_)
            | ExprData::Object(_)
            | ExprData::String(_)
            | ExprData::Template(_)
            | ExprData::RegExp(_)
            | ExprData::Arrow(_)
            | ExprData::PrivateIdentifier(_)
            | ExprData::JsxElement(_),
        ) => true,
        Some(ExprData::Unary(unary)) => !unary.op.table_entry().is_keyword,
        Some(ExprData::Call(call)) => {
            matches!(call.target.data.as_deref(), Some(ExprData::Arrow(_)))
        }
        Some(ExprData::Binary(binary)) => {
            (binary.op == OpCode::BinaryPower
                && power_left_requires_parentheses(&binary.left, options.minify_syntax))
                || can_omit_space_after_return(&binary.left, options)
        }
        _ => false,
    }
}

fn power_left_requires_parentheses(left: &Expr, minify_syntax: bool) -> bool {
    matches!(
        left.data.as_deref(),
        Some(ExprData::Unary(unary)) if unary.op.unary_assign_target() == AssignTarget::None
    ) || matches!(
        left.data.as_deref(),
        Some(ExprData::Await(_) | ExprData::Undefined | ExprData::Number(_))
    ) || (minify_syntax && matches!(left.data.as_deref(), Some(ExprData::Boolean(_))))
}

fn statement_starts_with_identifier(statement: &Stmt, options: Options) -> bool {
    match statement.data.as_deref() {
        Some(StmtData::Expr(expression)) => {
            expression_starts_with_identifier(&expression.value, options)
        }
        Some(
            StmtData::Debugger
            | StmtData::Local(_)
            | StmtData::Function(_)
            | StmtData::Class(_)
            | StmtData::Return(_)
            | StmtData::Throw(_)
            | StmtData::If(_)
            | StmtData::While(_)
            | StmtData::With(_)
            | StmtData::DoWhile(_)
            | StmtData::For(_)
            | StmtData::ForIn(_)
            | StmtData::ForOf(_)
            | StmtData::Try(_)
            | StmtData::Switch(_)
            | StmtData::Break(_)
            | StmtData::Continue(_)
            | StmtData::Label(_)
            | StmtData::Import(_)
            | StmtData::ExportClause(_)
            | StmtData::ExportFrom(_)
            | StmtData::ExportStar(_)
            | StmtData::ExportDefault(_)
            | StmtData::ExportEquals(_),
        ) => true,
        _ => false,
    }
}

fn expression_starts_with_identifier(expression: &Expr, options: Options) -> bool {
    match expression.data.as_deref() {
        Some(
            ExprData::Boolean(_)
            | ExprData::Super
            | ExprData::Null
            | ExprData::Undefined
            | ExprData::This
            | ExprData::New(_)
            | ExprData::NewTarget(_)
            | ExprData::ImportMeta(_)
            | ExprData::Identifier(_)
            | ExprData::ImportIdentifier(_)
            | ExprData::NameOfSymbol(_)
            | ExprData::BigInt(_)
            | ExprData::Await(_)
            | ExprData::Yield(_)
            | ExprData::RequireString(_)
            | ExprData::RequireResolveString(_)
            | ExprData::ImportString(_)
            | ExprData::ImportCall(_)
            | ExprData::Function(_)
            | ExprData::Class(_),
        ) => true,
        Some(ExprData::Number(value)) => format_number(*value, Precedence::Lowest, options, false)
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')),
        Some(ExprData::Unary(unary)) => {
            if matches!(
                unary.op,
                OpCode::UnaryPostDecrement | OpCode::UnaryPostIncrement
            ) {
                expression_starts_with_identifier(&unary.value, options)
            } else {
                unary.op.table_entry().is_keyword
            }
        }
        Some(ExprData::Binary(binary)) => expression_starts_with_identifier(&binary.left, options),
        Some(ExprData::If(conditional)) => {
            expression_starts_with_identifier(&conditional.test, options)
        }
        Some(ExprData::Dot(dot)) => {
            !(matches!(dot.target.data.as_deref(), Some(ExprData::Number(_)))
                || dot.optional_chain == OptionalChain::None && is_optional_chain(&dot.target))
                && expr_precedence(
                    dot.target
                        .data
                        .as_deref()
                        .expect("dot target must be present"),
                ) >= Precedence::Postfix
                && expression_starts_with_identifier(&dot.target, options)
        }
        Some(ExprData::Index(index)) => {
            !(index.optional_chain == OptionalChain::None && is_optional_chain(&index.target))
                && index
                    .target
                    .data
                    .as_deref()
                    .is_some_and(|target| expr_precedence(target) >= Precedence::Postfix)
                && expression_starts_with_identifier(&index.target, options)
        }
        Some(ExprData::Call(call)) => {
            !(call.optional_chain == OptionalChain::None && is_optional_chain(&call.target))
                && call
                    .target
                    .data
                    .as_deref()
                    .is_some_and(|target| expr_precedence(target) >= Precedence::Postfix)
                && expression_starts_with_identifier(&call.target, options)
        }
        Some(ExprData::Arrow(arrow)) => {
            arrow.is_async
                || (options.minify_whitespace
                    && !arrow.has_rest_arg
                    && matches!(
                        arrow.args.as_slice(),
                        [argument]
                            if argument.default_or_nil.data.is_none()
                                && matches!(
                                    argument.binding.data.as_deref(),
                                    Some(BindingData::Identifier(_))
                                )
                    ))
        }
        Some(ExprData::Template(template)) if template.tag_or_nil.data.is_some() => {
            !is_optional_chain(&template.tag_or_nil)
                && template
                    .tag_or_nil
                    .data
                    .as_deref()
                    .is_some_and(|tag| expr_precedence(tag) >= Precedence::Postfix)
                && expression_starts_with_identifier(&template.tag_or_nil, options)
        }
        Some(ExprData::InlinedEnum(inlined)) => {
            expression_starts_with_identifier(&inlined.value, options)
        }
        Some(ExprData::Annotation(annotation)) => {
            expression_starts_with_identifier(&annotation.value, options)
        }
        _ => false,
    }
}

fn expression_starts_with_sign(expression: &Expr, positive: bool) -> bool {
    match expression.data.as_deref() {
        Some(ExprData::Number(value)) => !positive && value.is_sign_negative(),
        Some(ExprData::Unary(unary)) => {
            if positive {
                matches!(unary.op, OpCode::UnaryPositive | OpCode::UnaryPreIncrement)
            } else {
                matches!(unary.op, OpCode::UnaryNegative | OpCode::UnaryPreDecrement)
            }
        }
        Some(ExprData::Binary(binary)) => expression_starts_with_sign(&binary.left, positive),
        Some(ExprData::If(conditional)) => expression_starts_with_sign(&conditional.test, positive),
        Some(ExprData::InlinedEnum(inlined)) => {
            expression_starts_with_sign(&inlined.value, positive)
        }
        Some(ExprData::Annotation(annotation)) => {
            expression_starts_with_sign(&annotation.value, positive)
        }
        _ => false,
    }
}

fn expression_is_script_regexp(expression: &Expr) -> bool {
    match expression.data.as_deref() {
        Some(ExprData::RegExp(value)) => value
            .as_bytes()
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"/script")),
        Some(ExprData::Dot(dot)) => expression_is_script_regexp(&dot.target),
        Some(ExprData::Index(index)) => expression_is_script_regexp(&index.target),
        Some(ExprData::Call(call)) => expression_is_script_regexp(&call.target),
        Some(ExprData::InlinedEnum(inlined)) => expression_is_script_regexp(&inlined.value),
        Some(ExprData::Annotation(annotation)) => expression_is_script_regexp(&annotation.value),
        _ => false,
    }
}

fn new_target_needs_space(target: &Expr, options: Options) -> bool {
    let Some(data) = target.data.as_deref() else {
        return false;
    };
    if expr_precedence(data) < Precedence::New {
        return false;
    }
    match data {
        ExprData::Boolean(_)
        | ExprData::Super
        | ExprData::Null
        | ExprData::Undefined
        | ExprData::This
        | ExprData::New(_)
        | ExprData::NewTarget(_)
        | ExprData::ImportMeta(_)
        | ExprData::Function(_)
        | ExprData::Class(_)
        | ExprData::Identifier(_)
        | ExprData::ImportIdentifier(_)
        | ExprData::NameOfSymbol(_)
        | ExprData::Number(_) => true,
        ExprData::BigInt(_) => !options.unsupported_features.contains(JsFeature::BIGINT),
        ExprData::Dot(dot) => {
            dot.optional_chain == OptionalChain::None
                && new_target_needs_space(&dot.target, options)
        }
        ExprData::Index(index) => {
            index.optional_chain == OptionalChain::None
                && new_target_needs_space(&index.target, options)
        }
        ExprData::Template(template) if template.tag_or_nil.data.is_some() => {
            new_target_needs_space(&template.tag_or_nil, options)
        }
        ExprData::InlinedEnum(inlined) => new_target_needs_space(&inlined.value, options),
        ExprData::Annotation(annotation) => new_target_needs_space(&annotation.value, options),
        _ => false,
    }
}

fn statement_can_omit_semicolon_before_close_brace(statement: &Stmt) -> bool {
    match statement.data.as_deref() {
        Some(
            StmtData::Debugger
            | StmtData::Directive(_)
            | StmtData::Expr(_)
            | StmtData::Local(_)
            | StmtData::Return(_)
            | StmtData::Throw(_)
            | StmtData::Break(_)
            | StmtData::Continue(_)
            | StmtData::DoWhile(_),
        ) => true,
        Some(StmtData::For(statement)) => {
            statement_can_omit_semicolon_before_close_brace(&statement.body)
        }
        Some(StmtData::ForIn(statement)) => {
            statement_can_omit_semicolon_before_close_brace(&statement.body)
        }
        Some(StmtData::ForOf(statement)) => {
            statement_can_omit_semicolon_before_close_brace(&statement.body)
        }
        Some(StmtData::While(statement)) => {
            statement_can_omit_semicolon_before_close_brace(&statement.body)
        }
        Some(StmtData::With(statement)) => {
            statement_can_omit_semicolon_before_close_brace(&statement.body)
        }
        Some(StmtData::If(statement)) => {
            if statement.no_or_nil.data.is_some() {
                statement_can_omit_semicolon_before_close_brace(&statement.no_or_nil)
            } else {
                statement_can_omit_semicolon_before_close_brace(&statement.yes)
            }
        }
        Some(StmtData::Label(statement)) => {
            statement_can_omit_semicolon_before_close_brace(&statement.statement)
        }
        _ => false,
    }
}

fn expr_precedence(data: &ExprData) -> Precedence {
    match data {
        ExprData::Yield(_) => Precedence::Yield,
        ExprData::If(_) => Precedence::Conditional,
        ExprData::Binary(binary) => binary.op.table_entry().level,
        ExprData::Spread(_) => Precedence::Spread,
        ExprData::Unary(unary) => unary.op.table_entry().level,
        ExprData::Await(_) => Precedence::Prefix,
        ExprData::Arrow(_) => Precedence::Assign,
        ExprData::New(_) => Precedence::New,
        ExprData::Call(_)
        | ExprData::RequireString(_)
        | ExprData::RequireResolveString(_)
        | ExprData::ImportString(_)
        | ExprData::ImportCall(_) => Precedence::Call,
        ExprData::InlinedEnum(inlined) => inlined
            .value
            .data
            .as_deref()
            .map_or(Precedence::Lowest, expr_precedence),
        ExprData::Annotation(annotation) => annotation
            .value
            .data
            .as_deref()
            .map_or(Precedence::Lowest, expr_precedence),
        _ => Precedence::Member,
    }
}

fn higher_precedence(level: Precedence) -> Precedence {
    const LEVELS: &[Precedence] = &[
        Precedence::Lowest,
        Precedence::Comma,
        Precedence::Spread,
        Precedence::Yield,
        Precedence::Assign,
        Precedence::Conditional,
        Precedence::NullishCoalescing,
        Precedence::LogicalOr,
        Precedence::LogicalAnd,
        Precedence::BitwiseOr,
        Precedence::BitwiseXor,
        Precedence::BitwiseAnd,
        Precedence::Equals,
        Precedence::Compare,
        Precedence::Shift,
        Precedence::Add,
        Precedence::Multiply,
        Precedence::Exponentiation,
        Precedence::Prefix,
        Precedence::Postfix,
        Precedence::New,
        Precedence::Call,
        Precedence::Member,
    ];
    LEVELS
        .iter()
        .position(|candidate| *candidate == level)
        .and_then(|index| LEVELS.get(index + 1))
        .copied()
        .unwrap_or(Precedence::Member)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        sync::Arc,
    };

    use super::{
        LinkerOptions, Options, RequireOrImportMeta, format_non_negative_float, format_number,
        print, print_expr, print_linked, print_with_source_map, quote_identifier, quote_utf16,
    };
    use crate::internal::{
        ast::{INVALID_REF, ImportRecordFlags, Index32, Ref, Symbol, SymbolKind, SymbolMap},
        compat::{Engine, JsFeature, Semver, unsupported_js_features},
        config::LegalComments,
        helpers::string_to_utf16,
        js_ast::{Ast, CommentStmt, ExprData, ModuleType, Part, Precedence, Stmt, StmtData},
        js_parser,
        logger::{DeferLogKind, Loc, Log, MsgKind, Source},
        renamer::new_no_op_renamer,
        sourcemap::generate_line_offset_tables,
    };

    fn quoted(text: &str, options: Options, allow_backtick: bool) -> String {
        String::from_utf8(quote_utf16(
            &string_to_utf16(text.as_bytes()),
            options,
            allow_backtick,
        ))
        .expect("printer output is UTF-8")
    }

    #[test]
    fn applies_initial_indentation() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(b"foo();".as_slice()),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        assert_eq!(
            print(
                &ast,
                &renamer,
                Options {
                    indent: 2,
                    ..Options::default()
                },
            )
            .js,
            b"    foo();\n"
        );
    }

    #[test]
    fn wraps_object_literal_when_nested_expression_starts_a_statement() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"for (var key in value) if ({}.hasOwnProperty.call(value, key)) copy()".as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) = js_parser::parse(
            log.clone(),
            source,
            js_parser::Options {
                minify_syntax: true,
                ..js_parser::Options::default()
            },
        );
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        let output = print(
            &ast,
            &renamer,
            Options {
                minify_whitespace: true,
                ..Options::default()
            },
        )
        .js;
        let output = String::from_utf8(output).expect("printer output is UTF-8");
        assert!(output.contains("({}).hasOwnProperty.call(value,key)&&copy()"));
    }

    #[test]
    fn preserves_empty_loop_body_when_minifying() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(b"function drain() { for (; keepGoing; advance()); }".as_slice()),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        let output = print(
            &ast,
            &renamer,
            Options {
                minify_whitespace: true,
                ..Options::default()
            },
        )
        .js;
        assert_eq!(
            String::from_utf8(output).expect("printer output is UTF-8"),
            "function drain(){for(;keepGoing;advance());}"
        );
    }

    #[test]
    fn separates_adjacent_plus_and_minus_tokens_when_minifying() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(b"a + +b; a + ++b; a - -b; a - --b;".as_slice()),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        let output = print(
            &ast,
            &renamer,
            Options {
                minify_whitespace: true,
                ..Options::default()
            },
        )
        .js;
        assert_eq!(
            String::from_utf8(output).expect("printer output is UTF-8"),
            "a+ +b;a+ ++b;a- -b;a- --b;"
        );
    }

    #[test]
    fn lowers_linked_require_and_import_expressions() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"const cjs = require('./cjs');\
                  const esm = require('./esm');\
                  import('./async');\
                  require('./esm');"
                    .as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (mut ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.import_records.len(), 4);
        for (record, source_index) in ast.import_records.iter_mut().zip([1, 2, 3, 2]) {
            record.source_index = Index32::new(source_index);
        }
        ast.import_records[1].flags |= ImportRecordFlags::WRAP_WITH_TO_CJS;

        let refs = |source_index, inner_index| Ref {
            source_index,
            inner_index,
        };
        let mut symbols = SymbolMap::new(4);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        symbols.symbols_for_source[1] = vec![Symbol::new(SymbolKind::Other, "require_cjs")];
        symbols.symbols_for_source[2] = vec![
            Symbol::new(SymbolKind::Other, "init_esm"),
            Symbol::new(SymbolKind::Other, "esm_exports"),
        ];
        symbols.symbols_for_source[3] = vec![
            Symbol::new(SymbolKind::Other, "init_async"),
            Symbol::new(SymbolKind::Other, "async_exports"),
        ];
        let to_common_js_ref = refs(0, u32::try_from(ast.symbols.len()).expect("symbol count"));
        symbols.symbols_for_source[0].push(Symbol::new(SymbolKind::Other, "__toCommonJS"));
        let renamer = new_no_op_renamer(symbols);
        let metadata = |source_index| match source_index {
            1 => RequireOrImportMeta {
                wrapper_ref: refs(1, 0),
                exports_ref: INVALID_REF,
                is_wrapper_async: false,
            },
            2 => RequireOrImportMeta {
                wrapper_ref: refs(2, 0),
                exports_ref: refs(2, 1),
                is_wrapper_async: false,
            },
            3 => RequireOrImportMeta {
                wrapper_ref: refs(3, 0),
                exports_ref: refs(3, 1),
                is_wrapper_async: true,
            },
            _ => panic!("unexpected linked source"),
        };

        assert_eq!(
            print_linked(
                &ast,
                &renamer,
                Options::default(),
                LinkerOptions {
                    require_or_import_meta_for_source: &metadata,
                    const_values: None,
                    ts_enums: None,
                    to_common_js_ref,
                    to_esm_ref: INVALID_REF,
                    runtime_require_ref: INVALID_REF,
                },
            )
            .js,
            b"const cjs = require_cjs();\n\
              const esm = (init_esm(), __toCommonJS(esm_exports));\n\
              init_async();\n\
              init_esm();\n"
        );
    }

    #[test]
    fn propagates_unused_results_through_comma_and_for_expressions() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"require('./esm'), require('./esm');\
                  for (require('./esm');; require('./esm')) {}"
                    .as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (mut ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.import_records.len(), 4);
        for record in &mut ast.import_records {
            record.source_index = Index32::new(1);
        }

        let wrapper_ref = Ref {
            source_index: 1,
            inner_index: 0,
        };
        let exports_ref = Ref {
            source_index: 1,
            inner_index: 1,
        };
        let mut symbols = SymbolMap::new(2);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        symbols.symbols_for_source[1] = vec![
            Symbol::new(SymbolKind::Other, "init_esm"),
            Symbol::new(SymbolKind::Other, "esm_exports"),
        ];
        let renamer = new_no_op_renamer(symbols);
        let metadata = |_| RequireOrImportMeta {
            wrapper_ref,
            exports_ref,
            is_wrapper_async: false,
        };

        assert_eq!(
            print_linked(
                &ast,
                &renamer,
                Options::default(),
                LinkerOptions {
                    require_or_import_meta_for_source: &metadata,
                    const_values: None,
                    ts_enums: None,
                    to_common_js_ref: INVALID_REF,
                    to_esm_ref: INVALID_REF,
                    runtime_require_ref: INVALID_REF,
                },
            )
            .js,
            b"init_esm(), init_esm();\n\
              for (init_esm(); ; init_esm()) {\n\
              }\n"
        );
    }

    #[test]
    fn parenthesizes_require_expressions_used_as_targets() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"new (require('./cjs'))();\
                  require('./cjs')();\
                  new (require.resolve('./pkg'))();"
                    .as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (mut ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.import_records.len(), 3);
        ast.import_records[0].source_index = Index32::new(1);
        ast.import_records[1].source_index = Index32::new(1);

        let wrapper_ref = Ref {
            source_index: 1,
            inner_index: 0,
        };
        let mut symbols = SymbolMap::new(2);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        symbols.symbols_for_source[1] = vec![Symbol::new(SymbolKind::Other, "require_cjs")];
        let renamer = new_no_op_renamer(symbols);
        let metadata = |_| RequireOrImportMeta {
            wrapper_ref,
            exports_ref: INVALID_REF,
            is_wrapper_async: false,
        };

        assert_eq!(
            print_linked(
                &ast,
                &renamer,
                Options::default(),
                LinkerOptions {
                    require_or_import_meta_for_source: &metadata,
                    const_values: None,
                    ts_enums: None,
                    to_common_js_ref: INVALID_REF,
                    to_esm_ref: INVALID_REF,
                    runtime_require_ref: INVALID_REF,
                },
            )
            .js,
            b"new (require_cjs())();\n\
              require_cjs()();\n\
              new (require.resolve(\"./pkg\"))();\n"
        );
    }

    #[test]
    fn converts_synchronous_dynamic_imports_to_resolved_promises() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(b"import('./cjs');".as_slice()),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (mut ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        ast.module_type_data.module_type = ModuleType::EsmMjs;
        ast.import_records[0].source_index = Index32::new(1);
        ast.import_records[0].flags |= ImportRecordFlags::WRAP_WITH_TO_ESM;

        let wrapper_ref = Ref {
            source_index: 1,
            inner_index: 0,
        };
        let to_esm_ref = Ref {
            source_index: 0,
            inner_index: u32::try_from(ast.symbols.len()).expect("symbol count"),
        };
        let mut symbols = SymbolMap::new(2);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        symbols.symbols_for_source[0].push(Symbol::new(SymbolKind::Other, "__toESM"));
        symbols.symbols_for_source[1] = vec![Symbol::new(SymbolKind::Other, "require_cjs")];
        let renamer = new_no_op_renamer(symbols);
        let metadata = |_| RequireOrImportMeta {
            wrapper_ref,
            exports_ref: INVALID_REF,
            is_wrapper_async: false,
        };

        assert_eq!(
            print_linked(
                &ast,
                &renamer,
                Options::default(),
                LinkerOptions {
                    require_or_import_meta_for_source: &metadata,
                    const_values: None,
                    ts_enums: None,
                    to_common_js_ref: INVALID_REF,
                    to_esm_ref,
                    runtime_require_ref: INVALID_REF,
                },
            )
            .js,
            b"Promise.resolve().then(() => __toESM(require_cjs(), 1));\n"
        );
    }

    #[test]
    fn lowers_external_dynamic_imports_for_old_targets() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(b"import('./pkg');".as_slice()),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (mut ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        ast.module_type_data.module_type = ModuleType::EsmMjs;
        ast.import_records[0].flags |=
            ImportRecordFlags::WRAP_WITH_TO_ESM | ImportRecordFlags::CALL_RUNTIME_REQUIRE;

        let to_esm_ref = Ref {
            source_index: 0,
            inner_index: u32::try_from(ast.symbols.len()).expect("symbol count"),
        };
        let runtime_require_ref = Ref {
            source_index: 0,
            inner_index: to_esm_ref.inner_index + 1,
        };
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        symbols.symbols_for_source[0].push(Symbol::new(SymbolKind::Other, "__toESM"));
        symbols.symbols_for_source[0].push(Symbol::new(SymbolKind::Other, "__require"));
        let renamer = new_no_op_renamer(symbols);
        let metadata = |_| panic!("external imports do not have linker metadata");
        let linker_options = LinkerOptions {
            require_or_import_meta_for_source: &metadata,
            const_values: None,
            ts_enums: None,
            to_common_js_ref: INVALID_REF,
            to_esm_ref,
            runtime_require_ref,
        };

        assert_eq!(
            print_linked(
                &ast,
                &renamer,
                Options {
                    unsupported_features: JsFeature::DYNAMIC_IMPORT,
                    ..Options::default()
                },
                linker_options,
            )
            .js,
            b"Promise.resolve().then(() => __toESM(__require(\"./pkg\"), 1));\n"
        );
        assert_eq!(
            print_linked(
                &ast,
                &renamer,
                Options {
                    unsupported_features: JsFeature::DYNAMIC_IMPORT | JsFeature::ARROW,
                    ..Options::default()
                },
                linker_options,
            )
            .js,
            b"Promise.resolve().then(function() {\n\
              \x20\x20return __toESM(__require(\"./pkg\"), 1);\n\
              });\n"
        );
    }

    #[test]
    fn lowers_external_require_interop_flags() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(b"const pkg = require('./pkg');".as_slice()),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (mut ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        ast.module_type_data.module_type = ModuleType::EsmMjs;
        ast.import_records[0].flags |=
            ImportRecordFlags::WRAP_WITH_TO_ESM | ImportRecordFlags::CALL_RUNTIME_REQUIRE;

        let to_esm_ref = Ref {
            source_index: 0,
            inner_index: u32::try_from(ast.symbols.len()).expect("symbol count"),
        };
        let runtime_require_ref = Ref {
            source_index: 0,
            inner_index: to_esm_ref.inner_index + 1,
        };
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        symbols.symbols_for_source[0].push(Symbol::new(SymbolKind::Other, "__toESM"));
        symbols.symbols_for_source[0].push(Symbol::new(SymbolKind::Other, "__require"));
        let renamer = new_no_op_renamer(symbols);
        let metadata = |_| panic!("external requires do not have linker metadata");

        assert_eq!(
            print_linked(
                &ast,
                &renamer,
                Options::default(),
                LinkerOptions {
                    require_or_import_meta_for_source: &metadata,
                    const_values: None,
                    ts_enums: None,
                    to_common_js_ref: INVALID_REF,
                    to_esm_ref,
                    runtime_require_ref,
                },
            )
            .js,
            b"const pkg = __toESM(__require(\"./pkg\"), 1);\n"
        );
    }

    #[test]
    fn handles_legal_comment_modes_and_deduplicates_extracted_comments() {
        let legal_comment = || {
            Stmt::new(
                Loc::default(),
                StmtData::Comment(CommentStmt {
                    text: "/*! first */".into(),
                    is_legal_comment: true,
                }),
            )
        };
        let tree = Ast {
            parts: vec![Part {
                statements: vec![
                    legal_comment(),
                    legal_comment(),
                    Stmt::new(
                        Loc::default(),
                        StmtData::Comment(CommentStmt {
                            text: "// ordinary".into(),
                            is_legal_comment: false,
                        }),
                    ),
                ],
                ..Part::default()
            }],
            ..Ast::default()
        };
        let renamer = new_no_op_renamer(SymbolMap::new(0));

        let inline = print(&tree, &renamer, Options::default());
        assert_eq!(inline.js, b"/*! first */\n/*! first */\n// ordinary\n");
        assert!(inline.extracted_legal_comments.is_empty());

        let extracted = print(
            &tree,
            &renamer,
            Options {
                legal_comments: LegalComments::EndOfFile,
                ..Options::default()
            },
        );
        assert_eq!(extracted.js, b"// ordinary\n");
        assert_eq!(extracted.extracted_legal_comments, ["/*! first */"]);

        let omitted = print(
            &tree,
            &renamer,
            Options {
                legal_comments: LegalComments::None,
                ..Options::default()
            },
        );
        assert_eq!(omitted.js, b"// ordinary\n");
        assert!(omitted.extracted_legal_comments.is_empty());
    }

    #[test]
    fn parser_preserves_legal_comments_before_statements_and_block_ends() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"/*! top */\nfunction f() { /* @license nested */ }\n/*! eof */".as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        let result = print(
            &ast,
            &renamer,
            Options {
                legal_comments: LegalComments::EndOfFile,
                ..Options::default()
            },
        );
        assert_eq!(
            result.extracted_legal_comments,
            ["/*! top */", "/* @license nested */", "/*! eof */"]
        );
    }

    #[test]
    fn emits_reusable_source_map_chunks() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(b"let value = 1;\nvalue++;\n".as_slice()),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) =
            js_parser::parse(log.clone(), source.clone(), js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        let result = print_with_source_map(
            &ast,
            &renamer,
            Options::default(),
            None,
            generate_line_offset_tables(&source.contents, ast.approximate_line_count),
        );
        assert_eq!(result.js, b"let value = 1;\nvalue++;\n");
        assert!(!result.source_map_chunk.should_ignore);
        assert!(!result.source_map_chunk.buffer.data.is_empty());
        assert_eq!(
            result.source_map_chunk.quoted_names,
            [b"\"value\"".to_vec()]
        );
        assert_eq!(result.source_map_chunk.end_state.generated_line, 2);
        assert_eq!(result.source_map_chunk.final_generated_column, 0);
    }

    #[test]
    fn quotes_identifiers_for_ascii_output() {
        assert_eq!(
            String::from_utf8(quote_identifier(Vec::new(), "π_value", JsFeature::NONE)).unwrap(),
            "\\u03C0_value"
        );
        assert_eq!(
            String::from_utf8(quote_identifier(Vec::new(), "𐊧x", JsFeature::NONE)).unwrap(),
            "\\u{102A7}x"
        );
    }

    #[test]
    fn chooses_the_shortest_string_delimiter() {
        assert_eq!(quoted("a\"b", Options::default(), false), "'a\"b'");
        assert_eq!(quoted("a\"'b", Options::default(), true), "`a\"'b`");
        assert_eq!(quoted("${x}", Options::default(), true), "\"${x}\"");
    }

    #[test]
    fn escapes_javascript_string_hazards() {
        assert_eq!(quoted("\0x", Options::default(), false), "\"\\0x\"");
        assert_eq!(quoted("\x001", Options::default(), false), "\"\\x001\"");
        assert_eq!(
            quoted("</ScRiPt>", Options::default(), false),
            "\"<\\/ScRiPt>\""
        );
        assert_eq!(
            quoted("\u{2028}\u{2029}\u{feff}", Options::default(), false),
            "\"\\u2028\\u2029\\uFEFF\""
        );
    }

    #[test]
    fn prints_utf16_as_utf8_or_ascii_escapes() {
        assert_eq!(quoted("π😀", Options::default(), false), "\"π😀\"");
        assert_eq!(
            quoted(
                "π😀",
                Options {
                    ascii_only: true,
                    ..Options::default()
                },
                false
            ),
            "\"\\u03C0\\u{1F600}\""
        );
        assert_eq!(
            quoted(
                "😀",
                Options {
                    unsupported_features: JsFeature::UNICODE_ESCAPES,
                    ascii_only: true,
                    ..Options::default()
                },
                false
            ),
            "\"\\uD83D\\uDE00\""
        );
    }

    #[test]
    fn formats_javascript_numbers() {
        assert_eq!(
            format_number(f64::NAN, Precedence::Lowest, Options::default(), false),
            "NaN"
        );
        assert_eq!(
            format_number(
                f64::INFINITY,
                Precedence::Multiply,
                Options {
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..Options::default()
                },
                false
            ),
            "1/0"
        );
        assert_eq!(
            format_number(-0.0, Precedence::Prefix, Options::default(), false),
            "-0"
        );
        assert_eq!(
            format_number(-1.0, Precedence::Lowest, Options::default(), false),
            "-1"
        );
    }

    #[test]
    fn compacts_non_negative_floats() {
        assert_eq!(format_non_negative_float(1000.0, false), "1e3");
        assert_eq!(format_non_negative_float(0.001, false), "1e-3");
        assert_eq!(format_non_negative_float(0.01, true), ".01");
        assert_eq!(format_non_negative_float(12_000.0, false), "12e3");
        assert_eq!(format_non_negative_float(543.25, false), "543.25");
    }

    #[test]
    fn prints_parsed_expressions_with_precedence() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"const math = 1 + 2 * 3;\
                  const grouped = a - (b - c);\
                  const read = object?.value ?? fallback;"
                    .as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());

        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        let printed = ast.parts[1]
            .statements
            .iter()
            .map(|statement| {
                let Some(StmtData::Local(local)) = statement.data.as_deref() else {
                    panic!("expected local declaration");
                };
                String::from_utf8(print_expr(
                    &local.declarations[0].value_or_nil,
                    &renamer,
                    Options::default(),
                ))
                .expect("printer output is UTF-8")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            printed,
            ["1 + 2 * 3", "a - (b - c)", "object?.value ?? fallback"]
        );
        assert_eq!(
            String::from_utf8(print(&ast, &renamer, Options::default()).js)
                .expect("printer output is UTF-8"),
            "const math = 1 + 2 * 3;\n\
             const grouped = a - (b - c);\n\
             const read = object?.value ?? fallback;\n"
        );

        let Some(StmtData::Local(local)) = ast.parts[1].statements[0].data.as_deref() else {
            unreachable!();
        };
        assert!(matches!(
            local.declarations[0].value_or_nil.data.as_deref(),
            Some(ExprData::Binary(_))
        ));
    }

    #[test]
    fn omits_empty_new_parentheses_only_when_safe() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"let a=new X;\
                  let b=(new X).y;\
                  let c=new X()();\
                  let d=new (f());\
                  let e=(new X)+1;\
                  let g=new X[0];\
                  let h=new X`tag`;\
                  let i=new [];\
                  class C extends (new X){}"
                    .as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);

        assert_eq!(
            String::from_utf8(
                print(
                    &ast,
                    &renamer,
                    Options {
                        minify_whitespace: true,
                        ..Options::default()
                    },
                )
                .js,
            )
            .expect("printer output is UTF-8"),
            "let a=new X;let b=new X().y;let c=new X()();let d=new(f());\
             let e=new X+1;let g=new X[0];let h=new X`tag`;let i=new[];\
             class C extends new X(){}"
        );
    }

    #[test]
    fn preserves_parentheses_around_completed_optional_chains() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"(f?.())();\
                  (f?.x).y;\
                  (f?.[x])[y];\
                  (f?.x)`tag`;\
                  f?.()();\
                  f?.x.y;"
                    .as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);

        assert_eq!(
            String::from_utf8(
                print(
                    &ast,
                    &renamer,
                    Options {
                        minify_whitespace: true,
                        ..Options::default()
                    },
                )
                .js,
            )
            .expect("printer output is UTF-8"),
            "(f?.())();(f?.x).y;(f?.[x])[y];(f?.x)`tag`;f?.()();f?.x.y;"
        );
    }

    #[test]
    fn prints_functions_and_arrow_expressions() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"function add(a, b = 1) { return a + b; }\
                  const twice = (value) => value * 2;\
                  async function load(){await prepare();await work();return()=>1}\
                  function* values(){yield 1;yield* other}\
                  async function consume(){for await(const item of items)use(item)}"
                    .as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        assert_eq!(
            String::from_utf8(print(&ast, &renamer, Options::default()).js)
                .expect("printer output is UTF-8"),
            "function add(a, b = 1) {\n\
             \x20\x20return a + b;\n\
             }\n\
             const twice = (value) => value * 2;\n\
             async function load() {\n\
             \x20\x20await prepare();\n\
             \x20\x20await work();\n\
             \x20\x20return () => 1;\n\
             }\n\
             function* values() {\n\
             \x20\x20yield 1;\n\
             \x20\x20yield* other;\n\
             }\n\
             async function consume() {\n\
             \x20\x20for await (const item of items) use(item);\n\
             }\n"
        );
        assert_eq!(
            String::from_utf8(
                print(
                    &ast,
                    &renamer,
                    Options {
                        minify_whitespace: true,
                        ..Options::default()
                    },
                )
                .js,
            )
            .expect("printer output is UTF-8"),
            "function add(a,b=1){return a+b}const twice=value=>value*2;async function load(){await prepare();await work();return()=>1}function*values(){yield 1;yield*other}async function consume(){for await(const item of items)use(item)}"
        );
        assert_eq!(
            String::from_utf8(
                print(
                    &ast,
                    &renamer,
                    Options {
                        minify_syntax: true,
                        ..Options::default()
                    },
                )
                .js,
            )
            .expect("printer output is UTF-8"),
            "function add(a, b = 1) {\n\
             \x20\x20return a + b;\n\
             }\n\
             const twice = (value) => value * 2;\n\
             async function load() {\n\
             \x20\x20return await prepare(), await work(), () => 1;\n\
             }\n\
             function* values() {\n\
             \x20\x20yield 1, yield* other;\n\
             }\n\
             async function consume() {\n\
             \x20\x20for await (const item of items) use(item);\n\
             }\n"
        );
    }

    #[test]
    fn prints_object_literals_and_properties() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"const value = 1, rest = {};\
                  const config = {name: 'demo', ['x']: value, value, ...rest,\
                    method(){}, get current(){return value}, set current(next){}};\
                  const {name, current: selected, ...tail} = config;"
                    .as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        assert_eq!(
            String::from_utf8(print(&ast, &renamer, Options::default()).js)
                .expect("printer output is UTF-8"),
            "const value = 1, rest = {};\n\
             const config = { name: \"demo\", [\"x\"]: value, value, ...rest, method() {\n\
             }, get current() {\n\
             \x20\x20return value;\n\
             }, set current(next) {\n\
             } };\n\
             const { name, current: selected, ...tail } = config;\n"
        );
        assert_eq!(
            String::from_utf8(
                print(
                    &ast,
                    &renamer,
                    Options {
                        minify_whitespace: true,
                        ..Options::default()
                    },
                )
                .js,
            )
            .expect("printer output is UTF-8"),
            "const value=1,rest={};const config={name:\"demo\",[\"x\"]:value,value,...rest,method(){},get current(){return value},set current(next){}};const{name,current:selected,...tail}=config;"
        );
    }

    #[test]
    fn prints_loop_statements() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"for (let i = 0; i < 2; i++) { sum += i; }\
                  for (const key in object) use(key);\
                  for (const value of list) use(value);\
                  outer: for (let j = 0; j < 2; j++) { if (j) continue outer; }\
                  if (ready) { start(); } else if (waiting) { pause(); } else { stop(); }\
                  do { tick(); } while (running);"
                    .as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        assert_eq!(
            String::from_utf8(print(&ast, &renamer, Options::default()).js)
                .expect("printer output is UTF-8"),
            "for (let i = 0; i < 2; i++) {\n\
             \x20\x20sum += i;\n\
             }\n\
             for (const key in object) use(key);\n\
             for (const value of list) use(value);\n\
             outer: for (let j = 0; j < 2; j++) {\n\
             \x20\x20if (j) continue outer;\n\
             }\n\
             if (ready) {\n\
             \x20\x20start();\n\
             } else if (waiting) {\n\
             \x20\x20pause();\n\
             } else {\n\
             \x20\x20stop();\n\
             }\n\
             do {\n\
             \x20\x20tick();\n\
             } while (running);\n"
        );
        assert_eq!(
            String::from_utf8(
                print(
                    &ast,
                    &renamer,
                    Options {
                        minify_whitespace: true,
                        ..Options::default()
                    },
                )
                .js,
            )
            .expect("printer output is UTF-8"),
            "for(let i=0;i<2;i++){sum+=i}for(const key in object)use(key);for(const value of list)use(value);outer:for(let j=0;j<2;j++){if(j)continue outer}if(ready){start()}else if(waiting){pause()}else{stop()}do{tick()}while(running);"
        );
    }

    #[test]
    fn removes_dead_statements_after_jumps_when_minifying() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"label: {\
                    foo();\
                    break label;\
                    bar();\
                    var kept = sideEffect();\
                    let dropped = other();\
                  }"
                .as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) = js_parser::parse(
            log.clone(),
            source,
            js_parser::Options {
                minify_syntax: true,
                ..js_parser::Options::default()
            },
        );
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        assert_eq!(
            String::from_utf8(
                print(
                    &ast,
                    &renamer,
                    Options {
                        minify_syntax: true,
                        minify_whitespace: true,
                        ..Options::default()
                    },
                )
                .js,
            )
            .expect("printer output is UTF-8"),
            "label:{foo();break label;var kept}"
        );
    }

    #[test]
    fn minifies_expression_control_flow() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"function f() {\
                    if (a) { b(); } else if (c) { d(); } else { e(); }\
                    while (x) { y(); }\
                    do { z(); } while (q);\
                  }"
                .as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) = js_parser::parse(
            log.clone(),
            source,
            js_parser::Options {
                minify_syntax: true,
                ..js_parser::Options::default()
            },
        );
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        assert_eq!(
            String::from_utf8(
                print(
                    &ast,
                    &renamer,
                    Options {
                        minify_syntax: true,
                        ..Options::default()
                    },
                )
                .js,
            )
            .expect("printer output is UTF-8"),
            "function f() {\n\
             \x20\x20for (a ? b() : c ? d() : e(); x; )\n\
             \x20\x20\x20\x20y();\n\
             \x20\x20do\n\
             \x20\x20\x20\x20z();\n\
             \x20\x20while (q);\n\
             }\n"
        );
        assert_eq!(
            String::from_utf8(
                print(
                    &ast,
                    &renamer,
                    Options {
                        minify_syntax: true,
                        minify_whitespace: true,
                        ..Options::default()
                    },
                )
                .js,
            )
            .expect("printer output is UTF-8"),
            "function f(){for(a?b():c?d():e();x;)y();do z();while(q)}"
        );
    }

    #[test]
    fn inlines_primitive_constants_when_minifying() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                "const café=\"π😀\";\
                 const count=2;\
                 console.log(café,count)"
                    .as_bytes(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) = js_parser::parse(
            log.clone(),
            source,
            js_parser::Options {
                minify_syntax: true,
                ..js_parser::Options::default()
            },
        );
        assert!(ok);
        assert!(log.done().is_empty());
        assert_eq!(ast.const_values.len(), 2);
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        assert_eq!(
            String::from_utf8(
                print(
                    &ast,
                    &renamer,
                    Options {
                        minify_syntax: true,
                        ascii_only: true,
                        ..Options::default()
                    },
                )
                .js,
            )
            .expect("printer output is UTF-8"),
            "const caf\\u00E9 = \"\\u03C0\\u{1F600}\", count = 2;\n\
             console.log(\"\\u03C0\\u{1F600}\", 2);\n"
        );
    }

    #[test]
    fn prints_try_and_switch_statements() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"try { work(); } catch (error) { fail(error); } finally { done(); }\
                  switch (kind) { case 1: break; default: fallback(); }"
                    .as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        assert_eq!(
            String::from_utf8(print(&ast, &renamer, Options::default()).js)
                .expect("printer output is UTF-8"),
            "try {\n\
             \x20\x20work();\n\
             } catch (error) {\n\
             \x20\x20fail(error);\n\
             } finally {\n\
             \x20\x20done();\n\
             }\n\
             switch (kind) {\n\
             \x20\x20case 1:\n\
             \x20\x20\x20\x20break;\n\
             \x20\x20default:\n\
             \x20\x20\x20\x20fallback();\n\
             }\n"
        );
        assert_eq!(
            String::from_utf8(
                print(
                    &ast,
                    &renamer,
                    Options {
                        minify_whitespace: true,
                        ..Options::default()
                    },
                )
                .js,
            )
            .expect("printer output is UTF-8"),
            "try{work()}catch(error){fail(error)}finally{done()}switch(kind){case 1:break;default:fallback()}"
        );
    }

    #[test]
    fn prints_classes_fields_and_methods() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"class Point extends Base {\
                    x = 0;\
                    constructor(x) { this.x = x; }\
                    move(dx) { this.x += dx; }\
                    static origin = new Point(0);\
                    static { this.ready = true; }\
                }"
                .as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        assert_eq!(
            String::from_utf8(print(&ast, &renamer, Options::default()).js)
                .expect("printer output is UTF-8"),
            "class Point extends Base {\n\
             \x20\x20x = 0;\n\
             \x20\x20constructor(x) {\n\
             \x20\x20\x20\x20this.x = x;\n\
             \x20\x20}\n\
             \x20\x20move(dx) {\n\
             \x20\x20\x20\x20this.x += dx;\n\
             \x20\x20}\n\
             \x20\x20static origin = new Point(0);\n\
             \x20\x20static {\n\
             \x20\x20\x20\x20this.ready = true;\n\
             \x20\x20}\n\
             }\n"
        );
        assert_eq!(
            String::from_utf8(
                print(
                    &ast,
                    &renamer,
                    Options {
                        minify_whitespace: true,
                        ..Options::default()
                    },
                )
                .js,
            )
            .expect("printer output is UTF-8"),
            "class Point extends Base{x=0;constructor(x){this.x=x}move(dx){this.x+=dx}static origin=new Point(0);static{this.ready=true}}"
        );
    }

    #[test]
    fn prints_import_and_export_statements() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"import value, {named as local} from 'pkg';\
                  import * as ns from 'other';\
                  import 'side';\
                  export {local as renamed};\
                  export {external as out} from 'third';\
                  export * from 'all';\
                  export default value;"
                    .as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        assert_eq!(
            String::from_utf8(print(&ast, &renamer, Options::default()).js)
                .expect("printer output is UTF-8"),
            "import value, { named as local } from \"pkg\";\n\
             import * as ns from \"other\";\n\
             import \"side\";\n\
             export { local as renamed };\n\
             export { external as out } from \"third\";\n\
             export * from \"all\";\n\
             export default value;\n"
        );
        assert_eq!(
            String::from_utf8(
                print(
                    &ast,
                    &renamer,
                    Options {
                        minify_whitespace: true,
                        ..Options::default()
                    },
                )
                .js,
            )
            .expect("printer output is UTF-8"),
            "import value,{named as local}from\"pkg\";import*as ns from\"other\";import\"side\";export{local as renamed};export{external as out}from\"third\";export*from\"all\";export default value;"
        );
        let metadata = print(
            &ast,
            &renamer,
            Options {
                needs_metafile: true,
                ..Options::default()
            },
        )
        .json_metadata_imports;
        assert_eq!(metadata.len(), 5);
        assert!(metadata[0].contains("\"path\": \"pkg\""));
        assert!(metadata[0].contains("\"kind\": \"import-statement\""));
        assert!(metadata[0].contains("\"external\": true"));
    }

    #[test]
    fn prints_templates_and_import_expressions() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"const greeting = `hello ${name}!`;\
                  const lazy = import('./lazy', {with: {type: 'json'}});\
                  const loaded = require('./dep');\
                  const resolved = require.resolve('./dep');"
                    .as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let (ast, ok) = js_parser::parse(log.clone(), source, js_parser::Options::default());
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        assert_eq!(
            String::from_utf8(print(&ast, &renamer, Options::default()).js)
                .expect("printer output is UTF-8"),
            "const greeting = `hello ${name}!`;\n\
             const lazy = import(\"./lazy\", { with: { type: \"json\" } });\n\
             const loaded = require(\"./dep\");\n\
             const resolved = require.resolve(\"./dep\");\n"
        );
        let metadata = print(
            &ast,
            &renamer,
            Options {
                needs_metafile: true,
                ..Options::default()
            },
        )
        .json_metadata_imports;
        assert_eq!(metadata.len(), 3);
        assert!(metadata[0].contains("\"kind\": \"dynamic-import\""));
        assert!(metadata[1].contains("\"kind\": \"require-call\""));
        assert!(metadata[2].contains("\"kind\": \"require-resolve\""));
    }

    #[test]
    fn prints_preserved_jsx_elements() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"const view = <Panel title=\"Hi\" enabled {...props}><span>{name}</span> text</Panel>;\
                  const fragment = <><Item /></>;"
                    .as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let mut options = js_parser::Options::default();
        options.jsx.parse = true;
        options.jsx.preserve = true;
        let (ast, ok) = js_parser::parse(log.clone(), source, options);
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        assert_eq!(
            String::from_utf8(print(&ast, &renamer, Options::default()).js)
                .expect("printer output is UTF-8"),
            "const view = <Panel title=\"Hi\" enabled {...props}><span>{name}</span> text</Panel>;\n\
             const fragment = <><Item /></>;\n"
        );
    }

    #[test]
    fn prints_lowered_type_script_enums() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"enum Color { Red, Blue = 'blue' } const red = Color.Red;".as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let mut options = js_parser::Options::default();
        options.ts.parse = true;
        let (ast, ok) = js_parser::parse(log.clone(), source, options);
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        assert_eq!(
            String::from_utf8(print(&ast, &renamer, Options::default()).js)
                .expect("printer output is UTF-8"),
            "var Color = /* @__PURE__ */ ((Color) => {\n\
             \x20\x20Color[Color[\"Red\"] = 0] = \"Red\";\n\
             \x20\x20Color[\"Blue\"] = \"blue\";\n\
             \x20\x20return Color;\n\
             })(Color || {});\n\
             const red = 0 /* Red */;\n"
        );
    }

    #[test]
    fn prints_lowered_type_script_namespaces() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"namespace Tools {\
                    export const version = 1;\
                    export function run() { return version; }\
                    export namespace Nested { export class Item {} }\
                    export enum Mode { Ready }\
                    namespace Types { interface Hidden {} }\
                }"
                .as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let mut options = js_parser::Options::default();
        options.ts.parse = true;
        let (ast, ok) = js_parser::parse(log.clone(), source, options);
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        assert_eq!(
            String::from_utf8(print(&ast, &renamer, Options::default()).js)
                .expect("printer output is UTF-8"),
            "var Tools;\n\
             ((Tools) => {\n\
             \x20\x20Tools.version = 1;\n\
             \x20\x20function run() {\n\
             \x20\x20\x20\x20return Tools.version;\n\
             \x20\x20}\n\
             \x20\x20Tools.run = run;\n\
             \x20\x20let Nested;\n\
             \x20\x20((Nested) => {\n\
             \x20\x20\x20\x20class Item {\n\
             \x20\x20\x20\x20}\n\
             \x20\x20\x20\x20Nested.Item = Item;\n\
             \x20\x20})(Nested = Tools.Nested || (Tools.Nested = {}));\n\
             \x20\x20let Mode;\n\
             \x20\x20((Mode) => {\n\
             \x20\x20\x20\x20Mode[Mode[\"Ready\"] = 0] = \"Ready\";\n\
             \x20\x20})(Mode = Tools.Mode || (Tools.Mode = {}));\n\
             })(Tools || (Tools = {}));\n"
        );
    }

    #[test]
    fn prints_lowered_type_script_namespace_destructuring() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(b"namespace A { export var [a, b = c, ...d] = ref; }".as_slice()),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let mut options = js_parser::Options::default();
        options.ts.parse = true;
        let (ast, ok) = js_parser::parse(log.clone(), source, options);
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        assert_eq!(
            String::from_utf8(print(&ast, &renamer, Options::default()).js)
                .expect("printer output is UTF-8"),
            "var A;\n\
             ((A) => {\n\
             \x20\x20[A.a, A.b = c, ...A.d] = ref;\n\
             })(A || (A = {}));\n"
        );
    }

    #[test]
    fn prints_merged_type_script_namespaces() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"function joined() {}\
                  namespace joined { export const x = 1 }\
                  namespace split { 0 }\
                  namespace split { 1 }"
                    .as_slice(),
            ),
            identifier_name: "entry".into(),
            ..Source::default()
        };
        let mut options = js_parser::Options::default();
        options.ts.parse = true;
        let (ast, ok) = js_parser::parse(log.clone(), source, options);
        assert!(ok);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        assert_eq!(
            String::from_utf8(print(&ast, &renamer, Options::default()).js)
                .expect("printer output is UTF-8"),
            "function joined() {\n\
             }\n\
             ((joined) => {\n\
             \x20\x20joined.x = 1;\n\
             })(joined || (joined = {}));\n\
             var split;\n\
             ((split) => {\n\
             \x20\x200;\n\
             })(split || (split = {}));\n\
             ((split) => {\n\
             \x20\x201;\n\
             })(split || (split = {}));\n"
        );
    }

    #[test]
    fn matches_pinned_upstream_js_printer_corpus() {
        let cases: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/upstream/js_printer.json"
        )))
        .expect("upstream js_printer corpus must be valid JSON");
        let cases = cases
            .as_array()
            .expect("upstream js_printer corpus must be an array");
        assert_eq!(cases.len(), 695, "upstream case count changed");
        assert_eq!(
            cases
                .iter()
                .filter_map(|case| case["upstream_test"].as_str())
                .collect::<HashSet<_>>()
                .len(),
            37,
            "upstream top-level test count changed"
        );

        let mut failures = Vec::new();
        let filter = std::env::var("UPSTREAM_TEST_FILTER").ok();
        let line_filter = std::env::var("UPSTREAM_LINE_FILTER")
            .ok()
            .and_then(|line| line.parse::<u64>().ok());
        for case in cases {
            let upstream_test = case["upstream_test"]
                .as_str()
                .expect("upstream_test must be a string");
            if filter
                .as_deref()
                .is_some_and(|filter| !upstream_test.contains(filter))
            {
                continue;
            }
            let line = case["line"].as_u64().expect("line must be an integer");
            if line_filter.is_some_and(|filter| filter != line) {
                continue;
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_upstream_printer_case(case)
            }));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(message)) => failures.push(format!(
                    "internal/js_printer/js_printer_test.go:{line} {upstream_test}: {message}"
                )),
                Err(_) => failures.push(format!(
                    "internal/js_printer/js_printer_test.go:{line} {upstream_test}: panicked"
                )),
            }
            if failures.len() >= 40 {
                break;
            }
        }

        assert!(
            failures.is_empty(),
            "pinned upstream js_printer failures:\n{}",
            failures.join("\n\n")
        );
    }

    fn run_upstream_printer_case(case: &serde_json::Value) -> Result<(), String> {
        let source_text = case["source"]
            .as_str()
            .ok_or_else(|| "source must be a string".to_owned())?;
        let expected = case["expected"]
            .as_str()
            .ok_or_else(|| "expected must be a string".to_owned())?;
        let mode = case["mode"]
            .as_str()
            .ok_or_else(|| "mode must be a string".to_owned())?;
        let minify_syntax = mode.contains("minify_syntax");
        let minify_whitespace = mode.contains("minify_whitespace");
        let ascii_only = mode.contains("ascii");
        let unsupported_features = if mode.contains("target") {
            let target = case["target"]
                .as_i64()
                .ok_or_else(|| "target mode requires an integer target".to_owned())?;
            unsupported_js_features(&HashMap::from([(
                Engine::Es,
                Semver {
                    parts: vec![i32::try_from(target).map_err(|error| error.to_string())?],
                    ..Semver::default()
                },
            )]))
        } else {
            JsFeature::NONE
        };

        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(source_text.as_bytes()),
            identifier_name: "<stdin>".into(),
            ..Source::default()
        };
        let mut parser_options = js_parser::Options {
            unsupported_js_features: unsupported_features,
            minify_syntax,
            minify_whitespace,
            ascii_only,
            defines: Some(Arc::new(crate::internal::config::process_defines(&[]))),
            ..js_parser::Options::default()
        };
        if mode.contains("jsx") {
            parser_options.jsx.parse = true;
            parser_options.jsx.preserve = true;
        }
        let (ast, ok) = js_parser::parse(log.clone(), source, parser_options);
        let errors = log
            .done()
            .into_iter()
            .filter(|message| message.kind == MsgKind::Error)
            .map(|message| message.data.text)
            .collect::<Vec<_>>();
        if !ok || !errors.is_empty() {
            return Err(format!(
                "parse failed for {source_text:?}: {}",
                errors.join("; ")
            ));
        }
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = ast.symbols.clone();
        let renamer = new_no_op_renamer(symbols);
        let actual = String::from_utf8(
            print(
                &ast,
                &renamer,
                Options {
                    unsupported_features,
                    minify_syntax,
                    minify_whitespace,
                    ascii_only,
                    ..Options::default()
                },
            )
            .js,
        )
        .map_err(|error| error.to_string())?;
        if actual != expected {
            return Err(format!(
                "input {source_text:?}\nexpected {expected:?}\nactual   {actual:?}"
            ));
        }
        Ok(())
    }
}
