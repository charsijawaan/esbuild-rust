//! Port of upstream `internal/js_printer`.

use crate::internal::ast::{ImportPhase, ImportRecord};
use crate::internal::compat::JsFeature;
use crate::internal::js_ast::{
    Ast, Binding, BindingData, BlockStmt, Expr, ExprData, LocalKind, OpCode, OptionalChain,
    Precedence, PropertyFlags, PropertyKind, Stmt, StmtData, is_identifier_es5_and_es_next,
};
use crate::internal::renamer::Renamer;

const HEX_CHARS: &[u8; 16] = b"0123456789ABCDEF";
const FIRST_ASCII: u32 = 0x20;
const LAST_ASCII: u32 = 0x7e;
const FIRST_HIGH_SURROGATE: u16 = 0xd800;
const LAST_HIGH_SURROGATE: u16 = 0xdbff;
const FIRST_LOW_SURROGATE: u16 = 0xdc00;
const LAST_LOW_SURROGATE: u16 = 0xdfff;

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub unsupported_features: JsFeature,
    pub line_limit: usize,
    pub minify_syntax: bool,
    pub minify_whitespace: bool,
    pub ascii_only: bool,
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
        let wrap = ((options.minify_syntax || with_nesting) && level >= Precedence::Multiply)
            || (is_negative && level >= Precedence::Prefix);
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
    } else if level >= Precedence::Prefix {
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
        indent: 0,
        import_records: &[],
    };
    printer.print_expr_at(expr, Precedence::Lowest);
    printer.output
}

#[derive(Clone, Debug, Default)]
pub struct PrintResult {
    pub js: Vec<u8>,
}

/// Prints all live AST parts as JavaScript.
///
/// # Panics
///
/// Panics if the tree contains an AST node whose printer case has not yet been
/// ported.
#[must_use]
pub fn print(tree: &Ast, renamer: &dyn Renamer, options: Options) -> PrintResult {
    let mut printer = Printer {
        output: Vec::new(),
        renamer,
        options,
        indent: 0,
        import_records: &tree.import_records,
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
    for part in &tree.parts {
        for statement in &part.statements {
            printer.print_stmt(statement);
        }
    }
    PrintResult { js: printer.output }
}

struct Printer<'a> {
    output: Vec<u8>,
    renamer: &'a dyn Renamer,
    options: Options,
    indent: usize,
    import_records: &'a [ImportRecord],
}

impl Printer<'_> {
    #[allow(clippy::too_many_lines)]
    fn print_stmt(&mut self, statement: &Stmt) {
        let Some(data) = statement.data.as_deref() else {
            return;
        };
        match data {
            StmtData::TypeScript(_) => {}
            StmtData::Empty => {
                self.print_indent();
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::Comment(comment) => {
                self.print_indent();
                self.output.extend_from_slice(b"//");
                self.output.extend_from_slice(comment.text.as_bytes());
                self.print_newline();
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
                self.print_indent();
                let wrap = matches!(
                    expression.value.data.as_deref(),
                    Some(ExprData::Object(_) | ExprData::Function(_) | ExprData::Class(_))
                );
                if wrap {
                    self.output.push(b'(');
                }
                self.print_expr_at(&expression.value, Precedence::Lowest);
                if wrap {
                    self.output.push(b')');
                }
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::Local(local) => {
                self.print_indent();
                self.print_local(local, true);
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::Block(block) => self.print_block(block, true),
            StmtData::Function(function) => {
                self.print_indent();
                if function.is_export {
                    self.output.extend_from_slice(b"export ");
                }
                self.print_function(&function.function);
                self.print_newline();
            }
            StmtData::Class(class) => {
                self.print_indent();
                if class.is_export {
                    self.output.extend_from_slice(b"export ");
                }
                self.print_class(&class.class);
                self.print_newline();
            }
            StmtData::Return(return_statement) => {
                self.print_indent();
                self.output.extend_from_slice(b"return");
                if return_statement.value_or_nil.data.is_some() {
                    self.output.push(b' ');
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
                self.print_indent();
                self.output.extend_from_slice(b"if");
                self.print_optional_space();
                self.output.push(b'(');
                self.print_expr_at(&if_statement.test, Precedence::Lowest);
                self.output.push(b')');
                self.print_body(&if_statement.yes);
                if if_statement.no_or_nil.data.is_some() {
                    if !self.options.minify_whitespace {
                        self.print_indent();
                    }
                    self.output.extend_from_slice(b"else");
                    self.print_body(&if_statement.no_or_nil);
                }
            }
            StmtData::While(while_statement) => {
                self.print_indent();
                self.output.extend_from_slice(b"while");
                self.print_optional_space();
                self.output.push(b'(');
                self.print_expr_at(&while_statement.test, Precedence::Lowest);
                self.output.push(b')');
                self.print_body(&while_statement.body);
            }
            StmtData::With(with_statement) => {
                self.print_indent();
                self.output.extend_from_slice(b"with");
                self.print_optional_space();
                self.output.push(b'(');
                self.print_expr_at(&with_statement.value, Precedence::Lowest);
                self.output.push(b')');
                self.print_body(&with_statement.body);
            }
            StmtData::DoWhile(do_while) => {
                self.print_indent();
                self.output.extend_from_slice(b"do");
                self.print_body(&do_while.body);
                self.output.extend_from_slice(b"while");
                self.print_optional_space();
                self.output.push(b'(');
                self.print_expr_at(&do_while.test, Precedence::Lowest);
                self.output.extend_from_slice(b");");
                self.print_newline();
            }
            StmtData::For(for_statement) => {
                self.print_indent();
                self.output.extend_from_slice(b"for");
                self.print_optional_space();
                self.output.push(b'(');
                self.print_for_init(&for_statement.init_or_nil);
                self.output.push(b';');
                if for_statement.test_or_nil.data.is_some() {
                    self.print_optional_space();
                    self.print_expr_at(&for_statement.test_or_nil, Precedence::Lowest);
                }
                self.output.push(b';');
                if for_statement.update_or_nil.data.is_some() {
                    self.print_optional_space();
                    self.print_expr_at(&for_statement.update_or_nil, Precedence::Lowest);
                }
                self.output.push(b')');
                self.print_body(&for_statement.body);
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
                self.print_body(&for_statement.body);
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
                self.print_for_init(&for_statement.init);
                self.output.extend_from_slice(b" of ");
                self.print_expr_at(&for_statement.value, Precedence::Comma);
                self.output.push(b')');
                self.print_body(&for_statement.body);
            }
            StmtData::Label(label) => {
                self.print_indent();
                self.print_identifier(&self.renamer.name_for_symbol(label.name.reference));
                self.output.push(b':');
                if matches!(label.statement.data.as_deref(), Some(StmtData::Block(_))) {
                    self.print_optional_space();
                    self.print_stmt(&label.statement);
                } else {
                    self.print_newline();
                    self.indent += 1;
                    self.print_stmt(&label.statement);
                    self.indent -= 1;
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
                    if case.value_or_nil.data.is_some() {
                        self.output.extend_from_slice(b"case ");
                        self.print_expr_at(&case.value_or_nil, Precedence::Lowest);
                        self.output.push(b':');
                    } else {
                        self.output.extend_from_slice(b"default:");
                    }
                    self.print_newline();
                    self.indent += 1;
                    for statement in &case.body {
                        self.print_stmt(statement);
                    }
                    self.indent -= 1;
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
                self.print_expr_at(&export.value, Precedence::Comma);
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
                let has_clause = import.default_name.is_some()
                    || import.star_name_loc.is_some()
                    || import.items.as_ref().is_some_and(|items| !items.is_empty());
                if has_clause {
                    self.output.push(b' ');
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
                        self.output.extend_from_slice(b"* as ");
                        self.print_identifier(&self.renamer.name_for_symbol(import.namespace_ref));
                    } else if let Some(items) = &import.items {
                        if needs_comma {
                            self.output.push(b',');
                            self.print_optional_space();
                        }
                        self.print_import_items(items, true);
                    }
                    self.output.extend_from_slice(b" from ");
                } else {
                    self.output.push(b' ');
                }
                self.print_import_path(import.import_record_index);
                self.print_import_attributes(import.import_record_index, false);
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::ExportClause(export) => {
                self.print_indent();
                self.output.extend_from_slice(b"export ");
                self.print_import_items(&export.items, false);
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::ExportFrom(export) => {
                self.print_indent();
                self.output.extend_from_slice(b"export ");
                self.print_export_from_items(&export.items);
                self.output.extend_from_slice(b" from ");
                self.print_import_path(export.import_record_index);
                self.print_import_attributes(export.import_record_index, false);
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::ExportStar(export) => {
                self.print_indent();
                self.output.extend_from_slice(b"export *");
                if let Some(alias) = &export.alias {
                    self.output.extend_from_slice(b" as ");
                    self.print_identifier(&alias.original_name);
                }
                self.output.extend_from_slice(b" from ");
                self.print_import_path(export.import_record_index);
                self.print_import_attributes(export.import_record_index, false);
                self.output.push(b';');
                self.print_newline();
            }
            StmtData::ExportDefault(export) => {
                self.print_indent();
                self.output.extend_from_slice(b"export default ");
                match export.value.data.as_deref() {
                    Some(StmtData::Expr(expression)) => {
                        self.print_expr_at(&expression.value, Precedence::Comma);
                        self.output.push(b';');
                    }
                    Some(StmtData::Function(function)) => self.print_function(&function.function),
                    Some(StmtData::Class(class)) => self.print_class(&class.class),
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
        for statement in &block.statements {
            self.print_stmt(statement);
        }
        self.indent -= 1;
        self.print_indent();
        self.output.push(b'}');
        if trailing_newline {
            self.print_newline();
        }
    }

    fn print_body(&mut self, body: &Stmt) {
        if let Some(StmtData::Block(block)) = body.data.as_deref() {
            self.print_optional_space();
            self.print_block(block, true);
        } else {
            self.print_newline();
            self.indent += 1;
            self.print_stmt(body);
            self.indent -= 1;
        }
    }

    fn print_binding(&mut self, binding: &Binding) {
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
                        self.print_expr_at(&item.default_value_or_nil, Precedence::Comma);
                    }
                }
                self.output.push(b']');
            }
            Some(BindingData::Object(object)) => {
                self.output.push(b'{');
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
                    if property.is_computed {
                        self.output.push(b'[');
                        self.print_expr_at(&property.key, Precedence::Lowest);
                        self.output.push(b']');
                    } else {
                        self.print_expr_at(&property.key, Precedence::Lowest);
                    }
                    self.output.push(b':');
                    self.print_optional_space();
                    self.print_binding(&property.value);
                    if property.default_value_or_nil.data.is_some() {
                        self.print_optional_space();
                        self.output.push(b'=');
                        self.print_optional_space();
                        self.print_expr_at(&property.default_value_or_nil, Precedence::Comma);
                    }
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
            LocalKind::Var => b"var ",
            LocalKind::Let => b"let ",
            LocalKind::Const => b"const ",
            LocalKind::Using => b"using ",
            LocalKind::AwaitUsing => b"await using ",
        });
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
                self.print_expr_at(&declaration.value_or_nil, Precedence::Comma);
            }
        }
    }

    fn print_for_init(&mut self, statement: &Stmt) {
        match statement.data.as_deref() {
            None | Some(StmtData::Empty) => {}
            Some(StmtData::Local(local)) => self.print_local(local, false),
            Some(StmtData::Expr(expression)) => {
                self.print_expr_at(&expression.value, Precedence::Lowest);
            }
            _ => panic!("Internal error: invalid for-loop initializer"),
        }
    }

    fn print_import_items(
        &mut self,
        items: &[crate::internal::js_ast::ClauseItem],
        is_import: bool,
    ) {
        self.output.push(b'{');
        if !items.is_empty() {
            self.print_optional_space();
        }
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                self.output.push(b',');
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
                self.output.extend_from_slice(b" as ");
                self.print_clause_name(alias);
            }
        }
        if !items.is_empty() {
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
                self.output.extend_from_slice(b" as ");
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

    fn print_import_path(&mut self, index: u32) {
        let record = &self.import_records[usize::try_from(index).expect("import record index")];
        self.output.extend(quote_utf16(
            &record.path.text.encode_utf16().collect::<Vec<_>>(),
            self.options,
            false,
        ));
    }

    fn print_import_attributes(&mut self, index: u32, is_dynamic: bool) {
        let record = &self.import_records[usize::try_from(index).expect("import record index")];
        let Some(attributes) = &record.assert_or_with else {
            return;
        };
        if is_dynamic {
            self.output.push(b',');
            self.print_optional_space();
            self.output.push(b'{');
            self.print_optional_space();
        } else {
            self.output.push(b' ');
        }
        self.output
            .extend_from_slice(attributes.keyword.as_str().as_bytes());
        if is_dynamic {
            self.output.push(b':');
            self.print_optional_space();
        } else {
            self.output.push(b' ');
        }
        self.output.push(b'{');
        if !attributes.entries.is_empty() {
            self.print_optional_space();
        }
        for (entry_index, entry) in attributes.entries.iter().enumerate() {
            if entry_index > 0 {
                self.output.push(b',');
                self.print_optional_space();
            }
            let key = String::from_utf16_lossy(&entry.key);
            if !entry.prefer_quoted_key && is_identifier_es5_and_es_next(&key) {
                self.print_identifier(&key);
            } else {
                self.output
                    .extend(quote_utf16(&entry.key, self.options, false));
            }
            self.output.push(b':');
            self.print_optional_space();
            self.output
                .extend(quote_utf16(&entry.value, self.options, false));
        }
        if !attributes.entries.is_empty() {
            self.print_optional_space();
        }
        self.output.push(b'}');
        if is_dynamic {
            self.print_optional_space();
            self.output.push(b'}');
        }
    }

    fn print_function(&mut self, function: &crate::internal::js_ast::Function) {
        if function.is_async {
            self.output.extend_from_slice(b"async ");
        }
        self.output.extend_from_slice(b"function");
        if function.is_generator {
            self.output.push(b'*');
        }
        if let Some(name) = function.name {
            self.output.push(b' ');
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
                self.print_expr_at(&argument.default_or_nil, Precedence::Comma);
            }
        }
        self.output.push(b')');
    }

    fn print_class(&mut self, class: &crate::internal::js_ast::Class) {
        for decorator in &class.decorators {
            self.output.push(b'@');
            self.print_expr_at(&decorator.value, Precedence::Lowest);
            self.print_newline();
            self.print_indent();
        }
        self.output.extend_from_slice(b"class");
        if let Some(name) = class.name {
            self.output.push(b' ');
            self.print_identifier(&self.renamer.name_for_symbol(name.reference));
        }
        if class.extends_or_nil.data.is_some() {
            self.output.extend_from_slice(b" extends ");
            self.print_expr_at(&class.extends_or_nil, Precedence::Compare);
        }
        self.print_optional_space();
        self.output.push(b'{');
        self.print_newline();
        self.indent += 1;
        for property in &class.properties {
            self.print_indent();
            for decorator in &property.decorators {
                self.output.push(b'@');
                self.print_expr_at(&decorator.value, Precedence::Lowest);
                self.print_newline();
                self.print_indent();
            }
            if property.kind == PropertyKind::ClassStaticBlock {
                self.output.extend_from_slice(b"static ");
                if let Some(block) = &property.class_static_block {
                    self.print_block(&block.block, true);
                }
                continue;
            }
            if property.flags.contains(PropertyFlags::IS_STATIC) {
                self.output.extend_from_slice(b"static ");
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
                self.print_expr_at(initializer, Precedence::Comma);
            }
            self.output.push(b';');
            self.print_newline();
        }
        self.indent -= 1;
        self.print_indent();
        self.output.push(b'}');
    }

    fn print_class_key(&mut self, property: &crate::internal::js_ast::Property) {
        if property.flags.contains(PropertyFlags::IS_COMPUTED) {
            self.output.push(b'[');
            self.print_expr_at(&property.key, Precedence::Lowest);
            self.output.push(b']');
        } else {
            self.print_property_key(&property.key);
        }
    }

    fn print_indent(&mut self) {
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
        let Some(data) = expr.data.as_deref() else {
            return;
        };
        let own_level = expr_precedence(data);
        let has_pure_comment = !self.options.minify_whitespace
            && match data {
                ExprData::Call(call) => call.can_be_unwrapped_if_unused,
                ExprData::New(new) => new.can_be_unwrapped_if_unused,
                _ => false,
            };
        let wrap = own_level < level || (has_pure_comment && level >= Precedence::Postfix);
        if wrap {
            self.output.push(b'(');
        }
        match data {
            ExprData::Missing => {}
            ExprData::Null => self.output.extend_from_slice(b"null"),
            ExprData::Undefined => self.output.extend_from_slice(b"void 0"),
            ExprData::Boolean(value) => {
                self.output
                    .extend_from_slice(if *value { b"true" } else { b"false" });
            }
            ExprData::Number(value) => self
                .output
                .extend_from_slice(format_number(*value, level, self.options, false).as_bytes()),
            ExprData::BigInt(value) => {
                self.output.extend_from_slice(value.as_bytes());
                self.output.push(b'n');
            }
            ExprData::String(value) => {
                self.output
                    .extend(quote_utf16(&value.value, self.options, true));
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
                self.print_symbol_expr(identifier.reference);
            }
            ExprData::PrivateIdentifier(identifier) => {
                self.print_identifier(&self.renamer.name_for_symbol(identifier.reference));
            }
            ExprData::NameOfSymbol(name) => {
                self.print_identifier(&self.renamer.name_for_symbol(name.reference));
            }
            ExprData::Array(array) => {
                self.output.push(b'[');
                for (index, item) in array.items.iter().enumerate() {
                    if index > 0 {
                        self.output.push(b',');
                        self.print_optional_space();
                    }
                    self.print_expr_at(item, Precedence::Comma);
                }
                self.output.push(b']');
            }
            ExprData::Object(object) => {
                self.output.push(b'{');
                if !object.properties.is_empty() {
                    self.print_optional_space();
                }
                for (index, property) in object.properties.iter().enumerate() {
                    if index > 0 {
                        self.output.push(b',');
                        self.print_optional_space();
                    }
                    if property.kind == PropertyKind::Spread {
                        self.output.extend_from_slice(b"...");
                        self.print_expr_at(&property.value_or_nil, Precedence::Comma);
                        continue;
                    }
                    if property.flags.contains(PropertyFlags::IS_COMPUTED) {
                        self.output.push(b'[');
                        self.print_expr_at(&property.key, Precedence::Lowest);
                        self.output.push(b']');
                    } else {
                        self.print_property_key(&property.key);
                    }
                    let is_shorthand = property.flags.contains(PropertyFlags::WAS_SHORTHAND)
                        && property.initializer_or_nil.data.is_none();
                    if !is_shorthand {
                        self.output.push(b':');
                        self.print_optional_space();
                        self.print_expr_at(&property.value_or_nil, Precedence::Comma);
                    }
                    if property.initializer_or_nil.data.is_some() {
                        self.print_optional_space();
                        self.output.push(b'=');
                        self.print_optional_space();
                        self.print_expr_at(&property.initializer_or_nil, Precedence::Comma);
                    }
                }
                if !object.properties.is_empty() {
                    self.print_optional_space();
                }
                self.output.push(b'}');
            }
            ExprData::Spread(spread) => {
                self.output.extend_from_slice(b"...");
                self.print_expr_at(&spread.value, Precedence::Comma);
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
                    }
                    self.print_expr_at(&unary.value, Precedence::Prefix);
                }
            }
            ExprData::Binary(binary) => {
                let operator = binary.op.table_entry();
                let higher = higher_precedence(operator.level);
                if binary.op.is_right_associative() {
                    self.print_expr_at(&binary.left, higher);
                } else {
                    self.print_expr_at(&binary.left, operator.level);
                }
                self.print_binary_operator(binary.op);
                if binary.op.is_right_associative() {
                    self.print_expr_at(&binary.right, operator.level);
                } else {
                    self.print_expr_at(&binary.right, higher);
                }
            }
            ExprData::If(conditional) => {
                self.print_expr_at(
                    &conditional.test,
                    higher_precedence(Precedence::Conditional),
                );
                self.print_optional_space();
                self.output.push(b'?');
                self.print_optional_space();
                self.print_expr_at(&conditional.yes, Precedence::Comma);
                self.print_optional_space();
                self.output.push(b':');
                self.print_optional_space();
                self.print_expr_at(&conditional.no, Precedence::Assign);
            }
            ExprData::Dot(dot) => {
                if matches!(dot.target.data.as_deref(), Some(ExprData::Number(_))) {
                    self.output.push(b'(');
                    self.print_expr_at(&dot.target, Precedence::Lowest);
                    self.output.push(b')');
                } else {
                    self.print_expr_at(&dot.target, Precedence::Member);
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
                self.print_expr_at(&index.target, Precedence::Member);
                if index.optional_chain == OptionalChain::Start {
                    self.output.extend_from_slice(b"?.");
                }
                self.output.push(b'[');
                self.print_expr_at(&index.index, Precedence::Lowest);
                self.output.push(b']');
            }
            ExprData::Call(call) => {
                if has_pure_comment {
                    self.output.extend_from_slice(b"/* @__PURE__ */ ");
                }
                self.print_expr_at(&call.target, Precedence::Call);
                if call.optional_chain == OptionalChain::Start {
                    self.output.extend_from_slice(b"?.");
                }
                self.print_arguments(&call.args);
            }
            ExprData::New(new) => {
                if has_pure_comment {
                    self.output.extend_from_slice(b"/* @__PURE__ */ ");
                }
                self.output.extend_from_slice(b"new ");
                self.print_expr_at(&new.target, Precedence::New);
                self.print_arguments(&new.args);
            }
            ExprData::InlinedEnum(inlined) => {
                self.print_expr_at(&inlined.value, level);
                if !self.options.minify_whitespace {
                    self.output.extend_from_slice(b" /* ");
                    self.output.extend_from_slice(inlined.comment.as_bytes());
                    self.output.extend_from_slice(b" */");
                }
            }
            ExprData::Annotation(annotation) => self.print_expr_at(&annotation.value, level),
            ExprData::Await(await_expression) => {
                self.output.extend_from_slice(b"await ");
                self.print_expr_at(&await_expression.value, Precedence::Prefix);
            }
            ExprData::Yield(yield_expression) => {
                self.output.extend_from_slice(if yield_expression.is_star {
                    b"yield* "
                } else {
                    b"yield "
                });
                self.print_expr_at(&yield_expression.value_or_nil, Precedence::Yield);
            }
            ExprData::Function(function) => self.print_function(&function.function),
            ExprData::Arrow(arrow) => {
                if arrow.is_async {
                    self.output.extend_from_slice(b"async ");
                }
                self.output.push(b'(');
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
                        self.print_expr_at(&argument.default_or_nil, Precedence::Comma);
                    }
                }
                self.output.push(b')');
                self.print_optional_space();
                self.output.extend_from_slice(b"=>");
                self.print_optional_space();
                if arrow.prefer_expr
                    && let [statement] = arrow.body.block.statements.as_slice()
                    && let Some(StmtData::Return(return_statement)) = statement.data.as_deref()
                    && return_statement.value_or_nil.data.is_some()
                {
                    self.print_expr_at(&return_statement.value_or_nil, Precedence::Assign);
                } else {
                    self.print_block(&arrow.body.block, false);
                }
            }
            ExprData::Class(class) => self.print_class(&class.class),
            ExprData::Template(template) => self.print_template(template),
            ExprData::RequireString(require) => {
                self.output.extend_from_slice(b"require(");
                self.print_import_path(require.import_record_index);
                self.output.push(b')');
            }
            ExprData::RequireResolveString(require) => {
                self.output.extend_from_slice(b"require.resolve(");
                self.print_import_path(require.import_record_index);
                self.output.push(b')');
            }
            ExprData::ImportString(import) => {
                let phase = self.import_records
                    [usize::try_from(import.import_record_index).expect("import record index")]
                .phase;
                self.print_import_start(phase);
                self.print_import_path(import.import_record_index);
                self.print_import_attributes(import.import_record_index, true);
                self.output.push(b')');
            }
            ExprData::ImportCall(import) => {
                self.print_import_start(import.phase);
                self.print_expr_at(&import.expr, Precedence::Comma);
                if import.options_or_nil.data.is_some() {
                    self.output.push(b',');
                    self.print_optional_space();
                    self.print_expr_at(&import.options_or_nil, Precedence::Comma);
                }
                self.output.push(b')');
            }
            ExprData::JsxElement(element) => self.print_jsx_element(element),
            ExprData::JsxText(text) => self.output.extend_from_slice(text.raw.as_bytes()),
        }
        if wrap {
            self.output.push(b')');
        }
    }

    fn print_arguments(&mut self, arguments: &[Expr]) {
        self.output.push(b'(');
        for (index, argument) in arguments.iter().enumerate() {
            if index > 0 {
                self.output.push(b',');
                self.print_optional_space();
            }
            self.print_expr_at(argument, Precedence::Comma);
        }
        self.output.push(b')');
    }

    fn print_property_key(&mut self, key: &Expr) {
        if let Some(ExprData::String(string)) = key.data.as_deref() {
            let name = String::from_utf16_lossy(&string.value);
            if is_identifier_es5_and_es_next(&name) {
                self.print_identifier(&name);
            } else {
                self.output
                    .extend(quote_utf16(&string.value, self.options, true));
            }
        } else {
            self.print_expr_at(key, Precedence::Lowest);
        }
    }

    fn print_template(&mut self, template: &crate::internal::js_ast::TemplateExpr) {
        let is_tagged = template.tag_or_nil.data.is_some();
        if is_tagged {
            self.print_expr_at(&template.tag_or_nil, Precedence::Postfix);
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
        self.output.push(b'<');
        self.print_jsx_tag(&element.tag_or_nil);
        for property in &element.properties {
            self.output.push(b' ');
            if property.kind == PropertyKind::Spread {
                self.output.extend_from_slice(b"{...");
                self.print_expr_at(&property.value_or_nil, Precedence::Comma);
                self.output.push(b'}');
                continue;
            }
            if property.flags.contains(PropertyFlags::IS_COMPUTED) {
                self.output.extend_from_slice(b"{...{ [");
                self.print_expr_at(&property.key, Precedence::Comma);
                self.output.extend_from_slice(b"]:");
                self.print_optional_space();
                self.print_expr_at(&property.value_or_nil, Precedence::Comma);
                self.output.extend_from_slice(b" }}");
                continue;
            }
            self.print_jsx_attribute_name(&property.key);
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
                Some(ExprData::JsxElement(_)) => {
                    self.print_expr_at(&property.value_or_nil, Precedence::Lowest);
                }
                _ => {
                    self.output.push(b'{');
                    self.print_expr_at(&property.value_or_nil, Precedence::Comma);
                    self.output.push(b'}');
                }
            }
        }
        if element.tag_or_nil.data.is_some() && element.nullable_children.is_empty() {
            self.output.extend_from_slice(b" />");
            return;
        }
        self.output.push(b'>');
        for child in &element.nullable_children {
            match child.data.as_deref() {
                None => self.output.extend_from_slice(b"{}"),
                Some(ExprData::JsxText(text)) => {
                    self.output.extend_from_slice(text.raw.as_bytes());
                }
                Some(ExprData::JsxElement(_)) => {
                    self.print_expr_at(child, Precedence::Lowest);
                }
                _ => {
                    self.output.push(b'{');
                    self.print_expr_at(child, Precedence::Comma);
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

    fn print_symbol_expr(&mut self, reference: crate::internal::ast::Ref) {
        if let Some(alias) = self.renamer.namespace_alias_for_symbol(reference) {
            self.print_symbol_expr(alias.namespace_ref);
            if is_identifier_es5_and_es_next(&alias.alias) {
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

    fn print_binary_operator(&mut self, operator: OpCode) {
        let entry = operator.table_entry();
        if entry.is_keyword || !self.options.minify_whitespace {
            self.output.push(b' ');
        }
        self.output.extend_from_slice(entry.text.as_bytes());
        if entry.is_keyword || !self.options.minify_whitespace {
            self.output.push(b' ');
        }
    }

    fn print_optional_space(&mut self) {
        if !self.options.minify_whitespace {
            self.output.push(b' ');
        }
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
    use std::{collections::HashMap, sync::Arc};

    use super::{
        Options, format_non_negative_float, format_number, print, print_expr, quote_identifier,
        quote_utf16,
    };
    use crate::internal::{
        ast::SymbolMap,
        compat::JsFeature,
        helpers::string_to_utf16,
        js_ast::{ExprData, Precedence, StmtData},
        js_parser,
        logger::{DeferLogKind, Log, Source},
        renamer::new_no_op_renamer,
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
            "(1/0)"
        );
        assert_eq!(
            format_number(-0.0, Precedence::Prefix, Options::default(), false),
            "(-0)"
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
    fn prints_functions_and_arrow_expressions() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"function add(a, b = 1) { return a + b; }\
                  const twice = (value) => value * 2;"
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
             const twice = (value) => value * 2;\n"
        );
    }

    #[test]
    fn prints_object_literals_and_properties() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"const value = 1, rest = {};\
                  const config = {name: 'demo', ['x']: value, value, ...rest};"
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
             const config = { name: \"demo\", [\"x\"]: value, value, ...rest };\n"
        );
    }

    #[test]
    fn prints_loop_statements() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(
                b"for (let i = 0; i < 2; i++) { sum += i; }\
                  for (const key in object) use(key);\
                  for (const value of list) use(value);"
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
             for (const key in object)\n\
             \x20\x20use(key);\n\
             for (const value of list)\n\
             \x20\x20use(value);\n"
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
             }\n"
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
            "var Color;\n\
             Color = /* @__PURE__ */ ((Color) => {\n\
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
             \x20\x20Mode = /* @__PURE__ */ ((Mode) => {\n\
             \x20\x20\x20\x20Mode[Mode[\"Ready\"] = 0] = \"Ready\";\n\
             \x20\x20\x20\x20return Mode;\n\
             \x20\x20})(Tools.Mode || (Tools.Mode = {}));\n\
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
}
