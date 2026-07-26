//! Port of upstream `internal/css_printer`.

use std::collections::HashMap;

use crate::internal::{
    ast::{Ref, SymbolMap},
    css_ast::{
        Ast, ComplexSelector, MediaBinaryOp, MediaCmp, MediaQuery, MediaQueryData, MediaTypeOp,
        NamespacedName, NthIndex, Rule, RuleData, SubclassData, Token, WhitespaceFlags,
    },
    css_lexer::{TokenKind, is_name_continue, would_start_identifier_without_escapes},
};

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub local_names: HashMap<Ref, String>,
    pub line_limit: usize,
    pub input_source_index: u32,
    pub minify_whitespace: bool,
    pub ascii_only: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PrintResult {
    pub css: Vec<u8>,
}

#[must_use]
pub fn print(tree: &Ast, symbols: &SymbolMap, options: Options) -> PrintResult {
    let mut printer = Printer {
        css: Vec::new(),
        import_records: &tree.import_records,
        symbols,
        options,
        indent: 0,
    };
    for rule in &tree.rules {
        printer.print_rule(rule, false);
    }
    PrintResult { css: printer.css }
}

struct Printer<'a> {
    css: Vec<u8>,
    import_records: &'a [crate::internal::ast::ImportRecord],
    symbols: &'a SymbolMap,
    options: Options,
    indent: usize,
}

impl Printer<'_> {
    #[allow(clippy::too_many_lines)]
    fn print_rule(&mut self, rule: &Rule, omit_trailing_semicolon: bool) {
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
                self.print_symbol(rule.name.reference);
                self.print_space();
                self.open_block();
                for block in &rule.blocks {
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
                    self.print_rule_block(&block.rules);
                    self.print_newline();
                }
                self.close_block();
            }
            RuleData::KnownAt(rule) => {
                self.css.push(b'@');
                self.print_ident(&rule.at_token);
                self.print_token_group(&rule.prelude, true);
                if rule.rules.is_empty() {
                    self.css.push(b';');
                } else {
                    self.print_space();
                    self.print_rule_block(&rule.rules);
                }
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
                self.print_complex_selectors(&rule.selectors);
                self.print_space();
                self.print_rule_block(&rule.rules);
            }
            RuleData::Qualified(rule) => {
                self.print_tokens(&rule.prelude);
                self.print_space();
                self.print_rule_block(&rule.rules);
            }
            RuleData::Declaration(rule) => {
                self.print_ident(&rule.key_text);
                self.css.push(b':');
                if !self.options.minify_whitespace && !rule.value.is_empty() {
                    self.css.push(b' ');
                }
                self.print_tokens(&rule.value);
                if rule.important {
                    if !self.options.minify_whitespace && !rule.value.is_empty() {
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
                    self.css.push(b';');
                } else {
                    self.print_space();
                    self.print_rule_block(&rule.rules);
                }
            }
            RuleData::AtMedia(rule) => {
                self.css.extend_from_slice(b"@media");
                if !rule.queries.is_empty() {
                    self.css.push(b' ');
                    self.print_media_queries(&rule.queries);
                    self.print_space();
                }
                self.print_rule_block(&rule.rules);
            }
            RuleData::AtScope(rule) => {
                self.css.extend_from_slice(b"@scope");
                if !rule.start.is_empty() {
                    self.print_space();
                    self.css.push(b'(');
                    self.print_complex_selectors(&rule.start);
                    self.css.push(b')');
                }
                if !rule.end.is_empty() {
                    self.css
                        .extend_from_slice(if self.options.minify_whitespace {
                            b"to("
                        } else {
                            b" to ("
                        });
                    self.print_complex_selectors(&rule.end);
                    self.css.push(b')');
                }
                self.print_space();
                self.print_rule_block(&rule.rules);
            }
        }
        self.print_newline();
    }

    fn print_rule_block(&mut self, rules: &[Rule]) {
        self.open_block();
        let last = rules.len().saturating_sub(1);
        for (index, rule) in rules.iter().enumerate() {
            self.print_rule(rule, self.options.minify_whitespace && index == last);
        }
        self.close_block();
    }

    fn open_block(&mut self) {
        self.css.push(b'{');
        self.indent += 1;
        self.print_newline();
    }

    fn close_block(&mut self) {
        self.indent = self.indent.saturating_sub(1);
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

    fn print_tokens(&mut self, tokens: &[Token]) {
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
    }

    fn print_token(&mut self, token: &Token) {
        match token.kind {
            TokenKind::Ident => self.print_ident(&token.text),
            TokenKind::Symbol => self.print_symbol(Ref {
                source_index: self.options.input_source_index,
                inner_index: token.payload_index,
            }),
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
                self.print_ident(&token.text);
            }
            TokenKind::String => self.print_quoted(&token.text, None),
            TokenKind::Url => {
                let record = &self.import_records[token.payload_index as usize];
                self.css.extend_from_slice(b"url(");
                self.print_quoted(&record.path.text, None);
                self.css.push(b')');
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
            MediaQueryData::ArbitraryTokens(query) => self.print_tokens(&query.tokens),
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

    fn print_complex_selectors(&mut self, selectors: &[ComplexSelector]) {
        for (complex_index, complex) in selectors.iter().enumerate() {
            if complex_index > 0 {
                self.css
                    .extend_from_slice(if self.options.minify_whitespace {
                        b","
                    } else {
                        b", "
                    });
            }
            for (compound_index, compound) in complex.selectors.iter().enumerate() {
                if compound_index > 0 {
                    if compound.combinator.byte == 0 {
                        self.css.push(b' ');
                    } else {
                        self.css.push(compound.combinator.byte);
                    }
                }
                if let Some(name) = &compound.type_selector {
                    self.print_namespaced_name(name);
                }
                for _ in &compound.nesting_selector_locs {
                    self.css.push(b'&');
                }
                for subclass in &compound.subclass_selectors {
                    self.print_subclass(&subclass.data);
                }
            }
        }
    }

    fn print_namespaced_name(&mut self, name: &NamespacedName) {
        if let Some(prefix) = &name.namespace_prefix {
            self.css.extend_from_slice(prefix.text.as_bytes());
            self.css.push(b'|');
        }
        self.css.extend_from_slice(name.name.text.as_bytes());
    }

    fn print_subclass(&mut self, subclass: &SubclassData) {
        match subclass {
            SubclassData::Hash(selector) => {
                self.css.push(b'#');
                self.print_symbol(selector.name.reference);
            }
            SubclassData::Class(selector) => {
                self.css.push(b'.');
                self.print_symbol(selector.name.reference);
            }
            SubclassData::Attribute(selector) => {
                self.css.push(b'[');
                self.print_namespaced_name(&selector.namespaced_name);
                if !selector.matcher_op.is_empty() {
                    self.css.extend_from_slice(selector.matcher_op.as_bytes());
                    self.print_quoted(&selector.matcher_value, None);
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
                self.print_complex_selectors(&selector.selectors);
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

    fn print_symbol(&mut self, reference: Ref) {
        let reference = self.symbols.follow_symbols_const(reference);
        if let Some(name) = self.options.local_names.get(&reference).cloned() {
            self.print_ident(&name);
        } else {
            let name = self.symbols.get(reference).original_name.clone();
            self.print_ident(&name);
        }
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
        let quote = forced_quote.unwrap_or_else(|| {
            if text.matches('\'').count() < text.matches('"').count() {
                '\''
            } else {
                '"'
            }
        });
        self.css.push(quote as u8);
        for character in text.chars() {
            match character {
                '\\' => self.css.extend_from_slice(b"\\\\"),
                character if character == quote => {
                    self.css.push(b'\\');
                    self.css.push(character as u8);
                }
                '\n' | '\r' | '\u{000C}' | '\0' => self
                    .css
                    .extend_from_slice(format!("\\{:x} ", character as u32).as_bytes()),
                character if self.options.ascii_only && !character.is_ascii() => self
                    .css
                    .extend_from_slice(format!("\\{:x} ", character as u32).as_bytes()),
                character => self.css.extend_from_slice(character.to_string().as_bytes()),
            }
        }
        self.css.push(quote as u8);
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
    use super::{Options, print};
    use crate::internal::{
        ast::SymbolMap,
        css_ast::{Ast, DeclarationRule, QualifiedRule, Rule, RuleData, Token, WhitespaceFlags},
        css_lexer::TokenKind,
        logger::Loc,
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
}
