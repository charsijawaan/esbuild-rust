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
                self.print_complex_selectors(&rule.selectors, true);
                self.print_space();
                self.print_rule_block(&rule.rules);
            }
            RuleData::Qualified(rule) => {
                let has_whitespace_after = self.print_tokens(&rule.prelude);
                if !has_whitespace_after {
                    self.print_space();
                }
                self.print_rule_block(&rule.rules);
            }
            RuleData::Declaration(rule) => {
                self.print_ident(&rule.key_text);
                self.css.push(b':');
                if !self.options.minify_whitespace
                    && !rule.value.is_empty()
                    && !rule.value[0].whitespace.contains(WhitespaceFlags::BEFORE)
                {
                    self.css.push(b' ');
                }
                let has_whitespace_after = self.print_declaration_tokens(&rule.value);
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

        let mut has_whitespace = tokens
            .first()
            .is_some_and(|token| token.whitespace.contains(WhitespaceFlags::BEFORE));
        let mut previous_was_comma = false;
        let mut printed_any = false;
        for token in tokens {
            if token.kind == TokenKind::Whitespace {
                has_whitespace = true;
                continue;
            }
            if has_whitespace {
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
                self.print_url_value(&record.path.text);
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
            self.print_ident(&prefix.text);
            self.css.push(b'|');
        }
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
    use super::{Options, Printer, best_quote_char, print};
    use crate::internal::{
        ast::SymbolMap,
        css_ast::{
            Ast, Combinator, ComplexSelector, CompoundSelector, DeclarationRule, NameToken,
            NamespacedName, QualifiedRule, Rule, RuleData, SelectorRule, Token, WhitespaceFlags,
        },
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
}
