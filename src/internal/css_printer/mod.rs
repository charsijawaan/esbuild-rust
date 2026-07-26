//! Port of upstream `internal/css_printer`.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use crate::internal::{
    ast::{ImportRecordFlags, Ref, SymbolMap},
    config::{LegalComments, MetafileFormat, SourceMap as SourceMapMode},
    css_ast::{
        Ast, ComplexSelector, MediaBinaryOp, MediaCmp, MediaQuery, MediaQueryData, MediaTypeOp,
        NamespacedName, NthIndex, Rule, RuleData, SubclassData, Token, WhitespaceFlags,
    },
    css_lexer::{TokenKind, is_name_continue, would_start_identifier_without_escapes},
    helpers::quote_for_json,
    sourcemap::{
        Chunk as SourceMapChunk, ChunkBuilder, LineOffsetTable, SourceMap, make_chunk_builder,
    },
};

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub input_source_map: Option<Arc<SourceMap>>,
    pub line_offset_tables: Vec<LineOffsetTable>,
    pub local_names: HashMap<Ref, String>,
    pub line_limit: usize,
    pub input_source_index: u32,
    pub minify_whitespace: bool,
    pub ascii_only: bool,
    pub legal_comments: LegalComments,
    pub needs_metafile: bool,
    pub metafile_format: MetafileFormat,
    pub source_map: SourceMapMode,
    pub add_source_mappings: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PrintResult {
    pub css: Vec<u8>,
    pub extracted_legal_comments: Vec<String>,
    pub json_metadata_imports: Vec<String>,
    pub source_map_chunk: SourceMapChunk,
}

fn function_multiline_comma_period(token: &Token) -> usize {
    if token.kind != TokenKind::Function {
        return 0;
    }
    let comma_count = token.children.as_ref().map_or(0, |children| {
        children
            .iter()
            .filter(|child| child.kind == TokenKind::Comma)
            .count()
    });
    match token.text.to_ascii_lowercase().as_str() {
        "linear-gradient"
        | "radial-gradient"
        | "conic-gradient"
        | "repeating-linear-gradient"
        | "repeating-radial-gradient"
        | "repeating-conic-gradient"
            if comma_count >= 2 =>
        {
            1
        }
        "matrix" if comma_count == 5 => 2,
        "matrix3d" if comma_count == 15 => 4,
        _ => 0,
    }
}

#[must_use]
pub fn print(tree: &Ast, symbols: &SymbolMap, options: Options) -> PrintResult {
    let source_map_builder = (options.source_map != SourceMapMode::None).then(|| {
        make_chunk_builder(
            options.input_source_map.clone(),
            options.line_offset_tables.clone(),
            options.ascii_only,
        )
    });
    let mut printer = Printer {
        css: Vec::new(),
        import_records: &tree.import_records,
        symbols,
        options,
        indent: 0,
        legal_comments: HashSet::new(),
        extracted_legal_comments: Vec::new(),
        json_metadata_imports: Vec::new(),
        source_map_builder,
        old_line_start: 0,
        old_line_end: 0,
    };
    for rule in &tree.rules {
        printer.print_rule(rule, false);
    }
    let source_map_chunk = printer
        .source_map_builder
        .take()
        .map(|builder| builder.generate_chunk(&printer.css))
        .unwrap_or_default();
    PrintResult {
        css: printer.css,
        extracted_legal_comments: printer.extracted_legal_comments,
        json_metadata_imports: printer.json_metadata_imports,
        source_map_chunk,
    }
}

fn best_quote_char(text: &str, for_url: bool) -> Option<char> {
    let mut url_cost = 0;
    let mut single_cost = 2;
    let mut double_cost = 2;
    for character in text.chars() {
        match character {
            '\'' => {
                url_cost += 1;
                single_cost += 1;
            }
            '"' => {
                url_cost += 1;
                double_cost += 1;
            }
            '(' | ')' | ' ' | '\t' => url_cost += 1,
            '\\' | '\n' | '\r' | '\u{000C}' => {
                url_cost += 1;
                single_cost += 1;
                double_cost += 1;
            }
            _ => {}
        }
    }
    if for_url && url_cost < single_cost && url_cost < double_cost {
        None
    } else if single_cost < double_cost {
        Some('\'')
    } else {
        Some('"')
    }
}

struct Printer<'a> {
    css: Vec<u8>,
    import_records: &'a [crate::internal::ast::ImportRecord],
    symbols: &'a SymbolMap,
    options: Options,
    indent: usize,
    legal_comments: HashSet<String>,
    extracted_legal_comments: Vec<String>,
    json_metadata_imports: Vec<String>,
    source_map_builder: Option<ChunkBuilder>,
    old_line_start: usize,
    old_line_end: usize,
}

impl Printer<'_> {
    fn current_line_length(&mut self) -> usize {
        let end = self.css.len();
        for index in (self.old_line_end..end).rev() {
            if matches!(self.css[index], b'\r' | b'\n') {
                self.old_line_start = index + 1;
                break;
            }
        }
        self.old_line_end = end;
        end - self.old_line_start
    }

    fn print_newline_past_line_limit(&mut self, indent: usize) -> bool {
        if self.current_line_length() < self.options.line_limit {
            return false;
        }
        self.css.push(b'\n');
        if !self.options.minify_whitespace {
            self.print_indent_levels(indent);
        }
        true
    }

    fn add_source_mapping(&mut self, location: crate::internal::logger::Loc, original_name: &str) {
        if self.options.add_source_mappings
            && let Some(builder) = &mut self.source_map_builder
        {
            builder.add_source_mapping(location, original_name, &self.css);
        }
    }

    fn record_import_path_for_metafile(&mut self, import_record_index: u32) {
        if !self.options.needs_metafile {
            return;
        }
        let record = &self.import_records[import_record_index as usize];
        let external = if record
            .flags
            .contains(ImportRecordFlags::SHOULD_NOT_BE_EXTERNAL_IN_METAFILE)
        {
            String::new()
        } else {
            self.options
                .metafile_format
                .maybe_remove_whitespace(",\n          \"external\": true")
        };
        let layout = self.options.metafile_format.maybe_remove_whitespace(
            "\n        {\n          \"path\": PATH,\n          \"kind\": KIND_EXTERNAL\n        }",
        );
        self.json_metadata_imports.push(
            layout
                .replace(
                    "PATH",
                    &String::from_utf8(quote_for_json(
                        record.path.text.as_bytes(),
                        self.options.ascii_only,
                    ))
                    .expect("quoted metadata path is UTF-8"),
                )
                .replace(
                    "KIND_EXTERNAL",
                    &format!(
                        "{}{external}",
                        String::from_utf8(quote_for_json(
                            record.kind.string_for_metafile().as_bytes(),
                            self.options.ascii_only,
                        ))
                        .expect("quoted metadata import kind is UTF-8")
                    ),
                ),
        );
    }

    #[allow(clippy::too_many_lines)]
    fn print_rule(&mut self, rule: &Rule, omit_trailing_semicolon: bool) {
        if let RuleData::Comment(comment) = &rule.data {
            match self.options.legal_comments {
                LegalComments::None => return,
                LegalComments::EndOfFile
                | LegalComments::LinkedWithComment
                | LegalComments::ExternalWithoutComment => {
                    if self.legal_comments.insert(comment.text.clone()) {
                        self.extracted_legal_comments.push(comment.text.clone());
                    }
                    return;
                }
                LegalComments::Inline => {}
            }
        }
        if self.options.line_limit > 0 {
            self.print_newline_past_line_limit(self.indent);
        }
        let skip_rule_mapping = (self.indent == 0 || self.options.minify_whitespace)
            && matches!(
                rule.data,
                RuleData::Selector(_) | RuleData::Qualified(_) | RuleData::BadDeclaration(_)
            );
        if !skip_rule_mapping {
            self.add_source_mapping(rule.loc, "");
        }
        if !self.options.minify_whitespace {
            self.print_indent();
        }
        match &rule.data {
            RuleData::AtCharset(rule) => {
                self.css.extend_from_slice(b"@charset ");
                self.print_quoted(&rule.encoding, Some('"'));
                self.css.push(b';');
            }
            RuleData::AtImport(rule) => {
                self.css
                    .extend_from_slice(if self.options.minify_whitespace {
                        b"@import"
                    } else {
                        b"@import "
                    });
                let record = &self.import_records[rule.import_record_index as usize];
                self.print_quoted(&record.path.text, None);
                self.record_import_path_for_metafile(rule.import_record_index);
                if let Some(conditions) = &rule.import_conditions {
                    self.print_token_group(&conditions.layers, true);
                    self.print_token_group(&conditions.supports, true);
                    if !conditions.queries.is_empty() {
                        self.print_space();
                        self.print_media_queries(&conditions.queries);
                    }
                }
                self.css.push(b';');
            }
            RuleData::AtKeyframes(rule) => {
                self.css.push(b'@');
                self.print_ident(&rule.at_token);
                self.css.push(b' ');
                self.print_symbol(rule.name.loc, rule.name.reference);
                self.print_space();
                self.open_block();
                for block in &rule.blocks {
                    self.add_source_mapping(block.loc, "");
                    if !self.options.minify_whitespace {
                        self.print_indent();
                    }
                    for (index, selector) in block.selectors.iter().enumerate() {
                        if index > 0 {
                            self.css
                                .extend_from_slice(if self.options.minify_whitespace {
                                    b","
                                } else {
                                    b", "
                                });
                        }
                        self.css.extend_from_slice(selector.as_bytes());
                    }
                    self.print_space();
                    self.print_rule_block(&block.rules, block.close_brace_loc);
                    self.print_newline();
                }
                self.close_block(rule.close_brace_loc);
            }
            RuleData::KnownAt(rule) => {
                self.css.push(b'@');
                self.print_ident(&rule.at_token);
                self.print_token_group(&rule.prelude, true);
                self.print_space();
                self.print_rule_block(&rule.rules, rule.close_brace_loc);
            }
            RuleData::UnknownAt(rule) => {
                self.css.push(b'@');
                self.print_ident(&rule.at_token);
                self.print_token_group(&rule.prelude, true);
                if rule.block.is_empty() {
                    self.css.push(b';');
                } else {
                    self.print_space();
                    self.print_tokens(&rule.block);
                }
            }
            RuleData::Selector(rule) => {
                self.print_complex_selectors(&rule.selectors, true);
                self.print_space();
                self.print_rule_block(&rule.rules, rule.close_brace_loc);
            }
            RuleData::Qualified(rule) => {
                let has_whitespace_after = self.print_tokens(&rule.prelude);
                if !has_whitespace_after {
                    self.print_space();
                }
                self.print_rule_block(&rule.rules, rule.close_brace_loc);
            }
            RuleData::Declaration(rule) => {
                self.print_ident(&rule.key_text);
                self.css.push(b':');
                let multiline_function_period =
                    if rule.value.len() == 1 && !self.options.minify_whitespace {
                        function_multiline_comma_period(&rule.value[0])
                    } else {
                        0
                    };
                let multiline_function = multiline_function_period > 0;
                let multiline = !self.options.minify_whitespace
                    && rule
                        .value
                        .iter()
                        .filter(|token| token.kind == TokenKind::Comma)
                        .count()
                        >= 2;
                if !self.options.minify_whitespace
                    && !multiline
                    && !multiline_function
                    && !rule.value.is_empty()
                    && !rule.value[0].whitespace.contains(WhitespaceFlags::BEFORE)
                {
                    self.css.push(b' ');
                }
                let has_whitespace_after = if multiline_function {
                    self.css.push(b'\n');
                    self.print_indent_levels(self.indent + 1);
                    self.print_multiline_function(&rule.value[0], multiline_function_period);
                    false
                } else {
                    self.print_declaration_tokens(&rule.value)
                };
                if rule.important {
                    if !self.options.minify_whitespace
                        && !rule.value.is_empty()
                        && !has_whitespace_after
                    {
                        self.css.push(b' ');
                    }
                    self.css.extend_from_slice(b"!important");
                }
                if !omit_trailing_semicolon {
                    self.css.push(b';');
                }
            }
            RuleData::BadDeclaration(rule) => {
                self.print_tokens(&rule.tokens);
                if !omit_trailing_semicolon {
                    self.css.push(b';');
                }
            }
            RuleData::Comment(rule) => self.css.extend_from_slice(rule.text.as_bytes()),
            RuleData::AtLayer(rule) => {
                self.css.extend_from_slice(b"@layer");
                for (index, name) in rule.names.iter().enumerate() {
                    self.css.extend_from_slice(if index == 0 {
                        b" "
                    } else if self.options.minify_whitespace {
                        b","
                    } else {
                        b", "
                    });
                    self.css.extend_from_slice(name.join(".").as_bytes());
                }
                if rule.rules.is_empty() {
                    if rule.names.is_empty() {
                        self.print_rule_block(&rule.rules, rule.close_brace_loc);
                    } else {
                        self.css.push(b';');
                    }
                } else {
                    self.print_space();
                    self.print_rule_block(&rule.rules, rule.close_brace_loc);
                }
            }
            RuleData::AtMedia(rule) => {
                self.css.extend_from_slice(b"@media");
                if !rule.queries.is_empty() {
                    if !self.options.minify_whitespace {
                        self.css.push(b' ');
                    }
                    let query_start = self.css.len();
                    self.print_media_queries(&rule.queries);
                    if self.options.minify_whitespace
                        && self.css.get(query_start).is_some_and(|byte| *byte != b'(')
                    {
                        self.css.insert(query_start, b' ');
                    }
                    self.print_space();
                }
                self.print_rule_block(&rule.rules, rule.close_brace_loc);
            }
            RuleData::AtScope(rule) => {
                self.css.extend_from_slice(b"@scope");
                if !rule.start.is_empty() {
                    self.print_space();
                    self.css.push(b'(');
                    self.print_complex_selectors(&rule.start, false);
                    self.css.push(b')');
                }
                if !rule.end.is_empty() {
                    self.css
                        .extend_from_slice(if self.options.minify_whitespace {
                            b"to ("
                        } else {
                            b" to ("
                        });
                    self.print_complex_selectors(&rule.end, false);
                    self.css.push(b')');
                }
                self.print_space();
                self.print_rule_block(&rule.rules, rule.close_brace_loc);
            }
        }
        self.print_newline();
    }

    fn print_rule_block(&mut self, rules: &[Rule], close_brace_loc: crate::internal::logger::Loc) {
        self.open_block();
        let last = rules.len().saturating_sub(1);
        for (index, rule) in rules.iter().enumerate() {
            self.print_rule(rule, self.options.minify_whitespace && index == last);
        }
        self.close_block(close_brace_loc);
    }

    fn open_block(&mut self) {
        self.css.push(b'{');
        self.indent += 1;
        self.print_newline();
    }

    fn close_block(&mut self, close_brace_loc: crate::internal::logger::Loc) {
        self.indent = self.indent.saturating_sub(1);
        if close_brace_loc.start != 0 {
            self.add_source_mapping(close_brace_loc, "");
        }
        if !self.options.minify_whitespace {
            self.print_indent();
        }
        self.css.push(b'}');
    }

    fn print_token_group(&mut self, tokens: &[Token], leading_space: bool) {
        if !tokens.is_empty() {
            if leading_space {
                self.css.push(b' ');
            }
            self.print_tokens(tokens);
        }
    }

    fn print_tokens(&mut self, tokens: &[Token]) -> bool {
        let mut has_whitespace = false;
        for (index, token) in tokens.iter().enumerate() {
            if token.kind == TokenKind::Whitespace {
                has_whitespace = true;
                continue;
            }
            if has_whitespace
                || token.whitespace.contains(WhitespaceFlags::BEFORE)
                || index > 0
                    && tokens[index - 1]
                        .whitespace
                        .contains(WhitespaceFlags::AFTER)
            {
                self.css.push(b' ');
            }
            has_whitespace = token.whitespace.contains(WhitespaceFlags::AFTER);
            self.print_token(token);
        }
        if has_whitespace {
            self.css.push(b' ');
        }
        has_whitespace
    }

    fn print_declaration_tokens(&mut self, tokens: &[Token]) -> bool {
        let multiline = !self.options.minify_whitespace
            && tokens
                .iter()
                .filter(|token| token.kind == TokenKind::Comma)
                .count()
                >= 2;
        if !multiline {
            return self.print_tokens(tokens);
        }

        let mut has_whitespace = true;
        let mut previous_was_comma = false;
        let mut printed_any = false;
        for token in tokens {
            if token.kind == TokenKind::Whitespace {
                has_whitespace = true;
                continue;
            }
            if has_whitespace || token.whitespace.contains(WhitespaceFlags::BEFORE) {
                if !printed_any || previous_was_comma {
                    self.css.push(b'\n');
                    for _ in 0..=self.indent {
                        self.css.extend_from_slice(b"  ");
                    }
                } else {
                    self.css.push(b' ');
                }
            }
            has_whitespace = token.whitespace.contains(WhitespaceFlags::AFTER);
            previous_was_comma = token.kind == TokenKind::Comma;
            printed_any = true;
            self.print_token(token);
        }
        if has_whitespace {
            self.css.push(b' ');
        }
        has_whitespace
    }

    fn print_multiline_function(&mut self, token: &Token, comma_period: usize) {
        self.print_ident(&token.text);
        self.css.push(b'(');
        let Some(children) = &token.children else {
            self.css.push(b')');
            return;
        };
        if !children.is_empty() {
            self.css.push(b'\n');
            self.print_indent_levels(self.indent + 2);
        }
        let mut comma_count = 0;
        for (index, child) in children.iter().enumerate() {
            self.print_token(child);
            if index + 1 == children.len() {
                continue;
            }
            if child.kind == TokenKind::Comma {
                comma_count += 1;
                if comma_count % comma_period == 0 {
                    self.css.push(b'\n');
                    self.print_indent_levels(self.indent + 2);
                } else {
                    self.css.push(b' ');
                }
            } else if child.whitespace.contains(WhitespaceFlags::AFTER)
                || children[index + 1]
                    .whitespace
                    .contains(WhitespaceFlags::BEFORE)
            {
                self.css.push(b' ');
            }
        }
        self.css.push(b')');
    }

    fn print_indent_levels(&mut self, levels: usize) {
        for _ in 0..levels {
            self.css.extend_from_slice(b"  ");
        }
    }

    fn print_token(&mut self, token: &Token) {
        self.add_source_mapping(token.loc, "");
        match token.kind {
            TokenKind::Ident => self.print_ident(&token.text),
            TokenKind::Symbol => self.print_symbol(
                token.loc,
                Ref {
                    source_index: self.options.input_source_index,
                    inner_index: token.payload_index,
                },
            ),
            TokenKind::Function => {
                self.print_ident(&token.text);
                self.css.push(b'(');
            }
            TokenKind::Dimension => {
                self.css
                    .extend_from_slice(token.dimension_value().as_bytes());
                self.print_ident(token.dimension_unit());
            }
            TokenKind::AtKeyword => {
                self.css.push(b'@');
                self.print_ident(&token.text);
            }
            TokenKind::Hash => {
                self.css.push(b'#');
                self.css.extend_from_slice(token.text.as_bytes());
            }
            TokenKind::String => self.print_quoted(&token.text, None),
            TokenKind::Url => {
                let record = &self.import_records[token.payload_index as usize];
                self.css.extend_from_slice(b"url(");
                self.print_url_value(&record.path.text);
                self.css.push(b')');
                self.record_import_path_for_metafile(token.payload_index);
            }
            _ => self.css.extend_from_slice(token.text.as_bytes()),
        }
        if let Some(children) = &token.children {
            self.print_tokens(children);
            self.css.push(match token.kind {
                TokenKind::OpenBrace => b'}',
                TokenKind::OpenBracket => b']',
                _ => b')',
            });
        }
    }

    fn print_media_queries(&mut self, queries: &[MediaQuery]) {
        for (index, query) in queries.iter().enumerate() {
            if index > 0 {
                self.css
                    .extend_from_slice(if self.options.minify_whitespace {
                        b","
                    } else {
                        b", "
                    });
            }
            self.print_media_query(query, false);
        }
    }

    fn print_media_query(&mut self, query: &MediaQuery, needs_parentheses: bool) {
        self.add_source_mapping(query.loc, "");
        match &query.data {
            MediaQueryData::Type(query) => {
                match query.op {
                    MediaTypeOp::None => {}
                    MediaTypeOp::Not => self.css.extend_from_slice(b"not "),
                    MediaTypeOp::Only => self.css.extend_from_slice(b"only "),
                }
                self.print_ident(&query.media_type);
                if let Some(inner) = &query.and_or_null {
                    self.css.extend_from_slice(b" and ");
                    self.print_media_query(inner, false);
                }
            }
            MediaQueryData::Not(query) => {
                self.css.extend_from_slice(b"not ");
                self.print_media_query(&query.inner, true);
            }
            MediaQueryData::Binary(query) => {
                if needs_parentheses {
                    self.css.push(b'(');
                }
                for (index, term) in query.terms.iter().enumerate() {
                    if index > 0 {
                        self.css.extend_from_slice(match query.op {
                            MediaBinaryOp::And => b" and ",
                            MediaBinaryOp::Or => b" or ",
                        });
                    }
                    self.print_media_query(term, true);
                }
                if needs_parentheses {
                    self.css.push(b')');
                }
            }
            MediaQueryData::ArbitraryTokens(query) => {
                self.print_tokens(&query.tokens);
            }
            MediaQueryData::PlainOrBoolean(query) => {
                self.css.push(b'(');
                self.print_ident(&query.name);
                if !query.value_or_nil.is_empty() {
                    self.css
                        .extend_from_slice(if self.options.minify_whitespace {
                            b":"
                        } else {
                            b": "
                        });
                    self.print_tokens(&query.value_or_nil);
                }
                self.css.push(b')');
            }
            MediaQueryData::Range(query) => {
                self.css.push(b'(');
                if query.before_cmp != MediaCmp::None {
                    self.print_tokens(&query.before);
                    self.print_comparison(query.before_cmp);
                }
                self.print_ident(&query.name);
                if query.after_cmp != MediaCmp::None {
                    self.print_comparison(query.after_cmp);
                    self.print_tokens(&query.after);
                }
                self.css.push(b')');
            }
        }
    }

    fn print_comparison(&mut self, comparison: MediaCmp) {
        if !self.options.minify_whitespace {
            self.css.push(b' ');
        }
        self.css.extend_from_slice(comparison.as_str().as_bytes());
        if !self.options.minify_whitespace {
            self.css.push(b' ');
        }
    }

    fn print_complex_selectors(&mut self, selectors: &[ComplexSelector], multiline: bool) {
        for (complex_index, complex) in selectors.iter().enumerate() {
            if complex_index > 0 {
                if self.options.minify_whitespace {
                    self.css.push(b',');
                } else if multiline {
                    self.css.extend_from_slice(b",\n");
                    self.print_indent();
                } else {
                    self.css.extend_from_slice(b", ");
                }
            }
            for (compound_index, compound) in complex.selectors.iter().enumerate() {
                self.add_source_mapping(compound.combinator.loc, "");
                if compound.combinator.byte == 0 {
                    if compound_index > 0 {
                        self.css.push(b' ');
                    }
                } else {
                    if compound_index > 0 && !self.options.minify_whitespace {
                        self.css.push(b' ');
                    }
                    self.css.push(compound.combinator.byte);
                    if !self.options.minify_whitespace {
                        self.css.push(b' ');
                    }
                }
                if let Some(name) = &compound.type_selector {
                    self.print_namespaced_name(name);
                }
                for &location in &compound.nesting_selector_locs {
                    self.add_source_mapping(location, "");
                    self.css.push(b'&');
                }
                for subclass in &compound.subclass_selectors {
                    self.add_source_mapping(subclass.range.loc, "");
                    self.print_subclass(&subclass.data);
                }
            }
        }
    }

    fn print_namespaced_name(&mut self, name: &NamespacedName) {
        if let Some(prefix) = &name.namespace_prefix {
            self.add_source_mapping(prefix.range.loc, "");
            self.print_ident(&prefix.text);
            self.css.push(b'|');
        }
        self.add_source_mapping(name.name.range.loc, "");
        if matches!(
            name.name.kind,
            TokenKind::DelimAsterisk | TokenKind::DelimAmpersand
        ) {
            self.css.extend_from_slice(name.name.text.as_bytes());
        } else {
            self.print_ident(&name.name.text);
        }
    }

    fn print_subclass(&mut self, subclass: &SubclassData) {
        match subclass {
            SubclassData::Hash(selector) => {
                self.css.push(b'#');
                self.print_symbol(selector.name.loc, selector.name.reference);
            }
            SubclassData::Class(selector) => {
                self.css.push(b'.');
                self.print_symbol(selector.name.loc, selector.name.reference);
            }
            SubclassData::Attribute(selector) => {
                self.css.push(b'[');
                self.print_namespaced_name(&selector.namespaced_name);
                if !selector.matcher_op.is_empty() {
                    self.css.extend_from_slice(selector.matcher_op.as_bytes());
                    if would_start_identifier_without_escapes(selector.matcher_value.as_bytes())
                        && selector
                            .matcher_value
                            .chars()
                            .all(|character| is_name_continue(character as i32))
                    {
                        self.print_ident(&selector.matcher_value);
                    } else {
                        self.print_quoted(&selector.matcher_value, None);
                    }
                }
                if selector.matcher_modifier != 0 {
                    self.css.push(b' ');
                    self.css.push(selector.matcher_modifier);
                }
                self.css.push(b']');
            }
            SubclassData::PseudoClass(selector) => {
                self.css
                    .extend_from_slice(if selector.is_element { b"::" } else { b":" });
                self.print_ident(&selector.name);
                if !selector.args.is_empty() {
                    self.css.push(b'(');
                    self.print_tokens(&selector.args);
                    self.css.push(b')');
                }
            }
            SubclassData::PseudoWithSelectorList(selector) => {
                self.css.push(b':');
                self.css
                    .extend_from_slice(selector.kind.as_str().as_bytes());
                self.css.push(b'(');
                self.print_nth_index(&selector.index);
                if (!selector.index.a.is_empty() || !selector.index.b.is_empty())
                    && !selector.selectors.is_empty()
                {
                    self.css.extend_from_slice(b" of ");
                }
                self.print_complex_selectors(&selector.selectors, false);
                self.css.push(b')');
            }
        }
    }

    fn print_nth_index(&mut self, index: &NthIndex) {
        if index.a.is_empty() {
            self.css.extend_from_slice(index.b.as_bytes());
        } else {
            if index.a == "-1" {
                self.css.push(b'-');
            } else if index.a != "1" {
                self.css.extend_from_slice(index.a.as_bytes());
            }
            self.css.push(b'n');
            if !index.b.is_empty() {
                if !index.b.starts_with('-') {
                    self.css.push(b'+');
                }
                self.css.extend_from_slice(index.b.as_bytes());
            }
        }
    }

    fn print_symbol(&mut self, location: crate::internal::logger::Loc, reference: Ref) {
        let reference = self.symbols.follow_symbols_const(reference);
        let original_name = self.symbols.get(reference).original_name.clone();
        let name = self
            .options
            .local_names
            .get(&reference)
            .cloned()
            .unwrap_or_else(|| original_name.clone());
        let source_map_name = if name == original_name {
            ""
        } else {
            &original_name
        };
        self.add_source_mapping(location, source_map_name);
        self.print_ident(&name);
    }

    fn print_ident(&mut self, text: &str) {
        if would_start_identifier_without_escapes(text.as_bytes())
            && text
                .chars()
                .all(|character| is_name_continue(character as i32))
        {
            self.css.extend_from_slice(text.as_bytes());
            return;
        }
        for (index, character) in text.chars().enumerate() {
            if (index == 0 && !would_start_identifier_without_escapes(text.as_bytes()))
                || !is_name_continue(character as i32)
                || self.options.ascii_only && !character.is_ascii()
            {
                self.css
                    .extend_from_slice(format!("\\{:x} ", character as u32).as_bytes());
            } else {
                self.css.extend_from_slice(character.to_string().as_bytes());
            }
        }
    }

    fn print_quoted(&mut self, text: &str, forced_quote: Option<char>) {
        let quote = forced_quote.unwrap_or_else(|| best_quote_char(text, false).unwrap_or('"'));
        self.print_quoted_with_quote(text, Some(quote));
    }

    fn print_url_value(&mut self, text: &str) {
        self.print_quoted_with_quote(text, best_quote_char(text, true));
    }

    fn print_quoted_with_quote(&mut self, text: &str, quote: Option<char>) {
        if let Some(quote) = quote {
            self.css.push(quote as u8);
        }
        let characters = text.chars().collect::<Vec<_>>();
        for (index, &character) in characters.iter().enumerate() {
            let next = characters.get(index + 1).copied();
            match character {
                '\\' => self.css.extend_from_slice(b"\\\\"),
                character if quote == Some(character) => {
                    self.css.push(b'\\');
                    self.css.push(character as u8);
                }
                '(' | ')' | ' ' | '\t' | '"' | '\'' if quote.is_none() => {
                    self.css.push(b'\\');
                    self.css.extend_from_slice(character.to_string().as_bytes());
                }
                '\n' | '\r' | '\u{000C}' | '\0' => self.print_hex_escape(character, next),
                '/' if index > 0
                    && characters[index - 1] == '<'
                    && characters
                        .iter()
                        .skip(index + 1)
                        .take(5)
                        .collect::<String>()
                        .eq_ignore_ascii_case("style") =>
                {
                    self.css.extend_from_slice(b"\\/");
                }
                character if self.options.ascii_only && !character.is_ascii() => {
                    self.print_hex_escape(character, next);
                }
                character => self.css.extend_from_slice(character.to_string().as_bytes()),
            }
        }
        if let Some(quote) = quote {
            self.css.push(quote as u8);
        }
    }

    fn print_hex_escape(&mut self, character: char, next: Option<char>) {
        self.css
            .extend_from_slice(format!("\\{:x}", character as u32).as_bytes());
        if next.is_some_and(|next| next.is_ascii_hexdigit() || matches!(next, ' ' | '\t')) {
            self.css.push(b' ');
        }
    }

    fn print_space(&mut self) {
        if !self.options.minify_whitespace {
            self.css.push(b' ');
        }
    }

    fn print_indent(&mut self) {
        for _ in 0..self.indent {
            self.css.extend_from_slice(b"  ");
        }
    }

    fn print_newline(&mut self) {
        if !self.options.minify_whitespace {
            self.css.push(b'\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{Options, Printer, best_quote_char, print};
    use crate::internal::{
        ast::{ImportKind, ImportRecord, Ref, Symbol, SymbolKind, SymbolMap},
        config::{LegalComments, MetafileFormat},
        css_ast::{
            Ast, AtImportRule, Combinator, CommentRule, ComplexSelector, CompoundSelector,
            DeclarationRule, NameToken, NamespacedName, QualifiedRule, Rule, RuleData,
            SelectorRule, Token, WhitespaceFlags,
        },
        css_lexer::TokenKind,
        logger::{Loc, Path},
        sourcemap::generate_line_offset_tables,
    };

    fn token(kind: TokenKind, text: &str) -> Token {
        Token {
            kind,
            text: text.into(),
            ..Token::default()
        }
    }

    fn stylesheet() -> Ast {
        Ast {
            rules: vec![Rule {
                loc: Loc::default(),
                data: RuleData::Qualified(QualifiedRule {
                    prelude: vec![
                        token(TokenKind::DelimDot, "."),
                        token(TokenKind::Ident, "card"),
                    ],
                    rules: vec![Rule {
                        loc: Loc::default(),
                        data: RuleData::Declaration(DeclarationRule {
                            key_text: "background".into(),
                            value: vec![Token {
                                kind: TokenKind::Function,
                                text: "linear-gradient".into(),
                                children: Some(vec![
                                    token(TokenKind::Ident, "red"),
                                    token(TokenKind::Comma, ","),
                                    Token {
                                        whitespace: WhitespaceFlags::BEFORE,
                                        ..token(TokenKind::Ident, "blue")
                                    },
                                ]),
                                ..Token::default()
                            }],
                            important: true,
                            ..DeclarationRule::default()
                        }),
                    }],
                    ..QualifiedRule::default()
                }),
            }],
            ..Ast::default()
        }
    }

    #[test]
    fn prints_pretty_css_rules_and_tokens() {
        let result = print(&stylesheet(), &SymbolMap::default(), Options::default());
        assert_eq!(
            String::from_utf8(result.css).expect("CSS output is UTF-8"),
            ".card {\n  background: linear-gradient(red, blue) !important;\n}\n"
        );
    }

    #[test]
    fn prints_minified_css_and_omits_final_semicolon() {
        let result = print(
            &stylesheet(),
            &SymbolMap::default(),
            Options {
                minify_whitespace: true,
                ..Options::default()
            },
        );
        assert_eq!(
            String::from_utf8(result.css).expect("CSS output is UTF-8"),
            ".card{background:linear-gradient(red, blue)!important}"
        );
    }

    fn quoted(text: &str, ascii_only: bool) -> String {
        let symbols = SymbolMap::default();
        let mut printer = Printer {
            css: Vec::new(),
            import_records: &[],
            symbols: &symbols,
            options: Options {
                ascii_only,
                ..Options::default()
            },
            indent: 0,
            legal_comments: std::collections::HashSet::new(),
            extracted_legal_comments: Vec::new(),
            json_metadata_imports: Vec::new(),
            source_map_builder: None,
            old_line_start: 0,
            old_line_end: 0,
        };
        printer.print_quoted(text, None);
        String::from_utf8(printer.css).expect("CSS output is UTF-8")
    }

    fn url_value(text: &str) -> String {
        let symbols = SymbolMap::default();
        let mut printer = Printer {
            css: Vec::new(),
            import_records: &[],
            symbols: &symbols,
            options: Options::default(),
            indent: 0,
            legal_comments: std::collections::HashSet::new(),
            extracted_legal_comments: Vec::new(),
            json_metadata_imports: Vec::new(),
            source_map_builder: None,
            old_line_start: 0,
            old_line_end: 0,
        };
        printer.print_url_value(text);
        String::from_utf8(printer.css).expect("CSS output is UTF-8")
    }

    #[test]
    fn chooses_and_escapes_css_strings_like_upstream() {
        assert_eq!(best_quote_char("f\"o", false), Some('\''));
        assert_eq!(quoted("", false), "\"\"");
        assert_eq!(quoted("f\"o", false), "'f\"o'");
        assert_eq!(quoted("f'\"'o", false), "\"f'\\\"'o\"");
        assert_eq!(quoted("f\ro", false), "\"f\\do\"");
        assert_eq!(quoted("f\n0", false), "\"f\\a 0\"");
        assert_eq!(quoted("</StYlE", false), "\"<\\/StYlE\"");
        assert_eq!(quoted("π", true), "\"\\3c0\"");
    }

    #[test]
    fn omits_url_quotes_when_the_unquoted_form_is_shorter() {
        assert_eq!(url_value("foo"), "foo");
        assert_eq!(url_value("f o"), "f\\ o");
        assert_eq!(url_value("f  o"), "\"f  o\"");
        assert_eq!(url_value("(foo)"), "\"(foo)\"");
        assert_eq!(url_value("\"foo\""), "'\"foo\"'");
    }

    #[test]
    fn prints_a_leading_relative_selector_combinator() {
        let tree = Ast {
            rules: vec![Rule {
                loc: Loc::default(),
                data: RuleData::Selector(SelectorRule {
                    selectors: vec![ComplexSelector {
                        selectors: vec![CompoundSelector {
                            type_selector: Some(NamespacedName {
                                name: NameToken {
                                    text: "item".into(),
                                    kind: TokenKind::Ident,
                                    ..NameToken::default()
                                },
                                ..NamespacedName::default()
                            }),
                            combinator: Combinator {
                                byte: b'>',
                                ..Combinator::default()
                            },
                            ..CompoundSelector::default()
                        }],
                    }],
                    ..SelectorRule::default()
                }),
            }],
            ..Ast::default()
        };
        assert_eq!(
            String::from_utf8(print(&tree, &SymbolMap::default(), Options::default()).css)
                .expect("CSS output is UTF-8"),
            "> item {\n}\n"
        );
    }

    #[test]
    fn extracts_and_deduplicates_css_legal_comments() {
        let comment = Rule {
            loc: Loc::default(),
            data: RuleData::Comment(CommentRule {
                text: "/*! license */".into(),
            }),
        };
        let tree = Ast {
            rules: vec![comment.clone(), comment],
            ..Ast::default()
        };
        let result = print(
            &tree,
            &SymbolMap::default(),
            Options {
                legal_comments: LegalComments::EndOfFile,
                ..Options::default()
            },
        );
        assert!(result.css.is_empty());
        assert_eq!(result.extracted_legal_comments, ["/*! license */"]);

        let result = print(
            &tree,
            &SymbolMap::default(),
            Options {
                legal_comments: LegalComments::None,
                ..Options::default()
            },
        );
        assert!(result.css.is_empty());
        assert!(result.extracted_legal_comments.is_empty());
    }

    #[test]
    fn records_css_imports_for_the_metafile() {
        let tree = Ast {
            import_records: vec![ImportRecord {
                path: Path {
                    text: "theme.css".into(),
                    ..Path::default()
                },
                kind: ImportKind::At,
                ..ImportRecord::default()
            }],
            rules: vec![Rule {
                loc: Loc::default(),
                data: RuleData::AtImport(AtImportRule::default()),
            }],
            ..Ast::default()
        };
        let result = print(
            &tree,
            &SymbolMap::default(),
            Options {
                needs_metafile: true,
                metafile_format: MetafileFormat::Minified,
                ..Options::default()
            },
        );
        assert_eq!(result.json_metadata_imports.len(), 1);
        assert_eq!(
            result.json_metadata_imports[0],
            "{\"path\":\"theme.css\",\"kind\":\"import-rule\",\"external\":true}"
        );
    }

    #[test]
    fn generates_css_source_map_chunks() {
        let source = b".card { color: red }";
        let result = print(
            &stylesheet(),
            &SymbolMap::default(),
            Options {
                line_offset_tables: generate_line_offset_tables(source, 1),
                source_map: crate::internal::config::SourceMap::LinkedWithComment,
                add_source_mappings: true,
                ..Options::default()
            },
        );
        assert!(!result.source_map_chunk.should_ignore);
        assert!(!result.source_map_chunk.buffer.data.is_empty());
    }

    #[test]
    fn records_precise_css_symbol_token_mappings() {
        let reference = Ref {
            source_index: 0,
            inner_index: 0,
        };
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0].push(Symbol {
            kind: SymbolKind::GlobalCss,
            original_name: "original".into(),
            ..Symbol::default()
        });
        let tree = Ast {
            rules: vec![Rule {
                loc: Loc::default(),
                data: RuleData::Qualified(QualifiedRule {
                    prelude: vec![Token {
                        kind: TokenKind::Symbol,
                        payload_index: 0,
                        loc: Loc::default(),
                        ..Token::default()
                    }],
                    ..QualifiedRule::default()
                }),
            }],
            ..Ast::default()
        };
        let result = print(
            &tree,
            &symbols,
            Options {
                line_offset_tables: generate_line_offset_tables(b"original{}", 1),
                local_names: HashMap::from([(reference, "a".into())]),
                source_map: crate::internal::config::SourceMap::LinkedWithComment,
                add_source_mappings: true,
                ..Options::default()
            },
        );
        assert_eq!(result.css, b"a {\n}\n");
        assert!(!result.source_map_chunk.buffer.data.is_empty());
    }

    #[test]
    fn wraps_minified_css_past_the_line_limit() {
        let tree = Ast {
            rules: vec![
                Rule {
                    loc: Loc::default(),
                    data: RuleData::Comment(CommentRule { text: "aa".into() }),
                },
                Rule {
                    loc: Loc::default(),
                    data: RuleData::Comment(CommentRule { text: "bb".into() }),
                },
            ],
            ..Ast::default()
        };
        let result = print(
            &tree,
            &SymbolMap::default(),
            Options {
                line_limit: 2,
                minify_whitespace: true,
                ..Options::default()
            },
        );
        assert_eq!(result.css, b"aa\nbb");
    }
}
