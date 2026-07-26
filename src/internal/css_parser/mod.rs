//! Port of upstream `internal/css_parser`.

use crate::internal::{
    ast::{ImportKind, ImportRecord},
    css_ast::{
        Ast, AtCharsetRule, AtImportRule, BadDeclarationRule, DeclarationRule, ImportConditions,
        QualifiedRule, Rule, RuleData, Token, UnknownAtRule, WhitespaceFlags,
    },
    css_lexer::{self, TokenKind},
    logger::{Loc, Log, Path, Range, Source},
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Options {
    pub minify_syntax: bool,
    pub minify_whitespace: bool,
    pub minify_identifiers: bool,
}

#[must_use]
pub fn parse(log: Log, source: Source, options: Options) -> Ast {
    let result = css_lexer::tokenize(
        log,
        source.clone(),
        css_lexer::Options {
            record_all_comments: options.minify_identifiers,
        },
    );
    let mut parser = Parser {
        source,
        tokens: result.tokens,
        minify_whitespace: options.minify_whitespace,
        index: 0,
        import_records: Vec::new(),
    };
    let rules = parser.parse_rule_list(false);
    Ast {
        rules,
        import_records: parser.import_records,
        source_map_comment: result.source_map_comment,
        approximate_line_count: result.approximate_line_count,
        ..Ast::default()
    }
}

struct Parser {
    source: Source,
    tokens: Vec<css_lexer::Token>,
    minify_whitespace: bool,
    index: usize,
    import_records: Vec<ImportRecord>,
}

impl Parser {
    fn parse_rule_list(&mut self, stop_at_close_brace: bool) -> Vec<Rule> {
        let mut rules = Vec::new();
        loop {
            self.skip_whitespace();
            match self.current_kind() {
                TokenKind::EndOfFile => break,
                TokenKind::CloseBrace if stop_at_close_brace => {
                    self.index += 1;
                    break;
                }
                TokenKind::Semicolon => self.index += 1,
                TokenKind::AtKeyword => rules.push(self.parse_at_rule()),
                _ if stop_at_close_brace && self.starts_declaration() => {
                    rules.push(self.parse_declaration());
                }
                _ => rules.push(self.parse_qualified_rule()),
            }
        }
        rules
    }

    fn parse_at_rule(&mut self) -> Rule {
        let token = self.current();
        let loc = token.range.loc;
        let name = self.decoded(token);
        self.index += 1;
        self.skip_whitespace();
        if name.eq_ignore_ascii_case("charset") {
            let encoding = if matches!(
                self.current_kind(),
                TokenKind::String | TokenKind::UnterminatedString
            ) {
                let encoding = self.decoded(self.current());
                self.index += 1;
                encoding
            } else {
                String::new()
            };
            self.consume_through_semicolon();
            return Rule {
                loc,
                data: RuleData::AtCharset(AtCharsetRule { encoding }),
            };
        }
        if name.eq_ignore_ascii_case("import") {
            return self.parse_at_import(loc);
        }

        let prelude_start = self.index;
        let end = self.scan_to_rule_delimiter();
        let prelude = self.convert_tokens(prelude_start, end);
        self.index = end;
        let block = match self.current_kind() {
            TokenKind::Semicolon => {
                self.index += 1;
                Vec::new()
            }
            TokenKind::OpenBrace => {
                let start = self.index;
                let end = self.scan_balanced_block(start);
                self.index = end;
                self.convert_tokens(start, end)
            }
            _ => Vec::new(),
        };
        Rule {
            loc,
            data: RuleData::UnknownAt(UnknownAtRule {
                at_token: name,
                prelude,
                block,
            }),
        }
    }

    fn parse_at_import(&mut self, loc: Loc) -> Rule {
        let path_token = self.current();
        let path = if matches!(path_token.kind, TokenKind::String | TokenKind::Url) {
            self.index += 1;
            self.decoded(path_token)
        } else {
            String::new()
        };
        let import_record_index =
            u32::try_from(self.import_records.len()).expect("CSS import count fits in u32");
        self.import_records.push(ImportRecord {
            path: Path {
                text: path,
                ..Path::default()
            },
            range: path_token.range,
            kind: ImportKind::At,
            ..ImportRecord::default()
        });
        let conditions_start = self.index;
        while !matches!(
            self.current_kind(),
            TokenKind::Semicolon | TokenKind::EndOfFile
        ) {
            self.index += 1;
        }
        let conditions = self.convert_tokens(conditions_start, self.index);
        if self.current_kind() == TokenKind::Semicolon {
            self.index += 1;
        }
        Rule {
            loc,
            data: RuleData::AtImport(AtImportRule {
                import_record_index,
                import_conditions: if conditions.is_empty() {
                    None
                } else {
                    Some(ImportConditions {
                        queries: Vec::new(),
                        layers: conditions,
                        supports: Vec::new(),
                    })
                },
            }),
        }
    }

    fn parse_qualified_rule(&mut self) -> Rule {
        let loc = self.current().range.loc;
        let prelude_start = self.index;
        let end = self.scan_to_rule_delimiter();
        let prelude = self.convert_tokens(prelude_start, end);
        self.index = end;
        if self.current_kind() != TokenKind::OpenBrace {
            if self.current_kind() == TokenKind::Semicolon {
                self.index += 1;
            }
            return Rule {
                loc,
                data: RuleData::BadDeclaration(BadDeclarationRule { tokens: prelude }),
            };
        }
        self.index += 1;
        let rules = self.parse_rule_list(true);
        Rule {
            loc,
            data: RuleData::Qualified(QualifiedRule {
                prelude,
                rules,
                ..QualifiedRule::default()
            }),
        }
    }

    fn parse_declaration(&mut self) -> Rule {
        let key_token = self.current();
        let loc = key_token.range.loc;
        let key_text = self.decoded(key_token);
        self.index += 1;
        self.skip_whitespace();
        if self.current_kind() == TokenKind::Colon {
            self.index += 1;
        }
        let value_start = self.index;
        let value_end = self.scan_declaration_end();
        let mut value = self.convert_tokens(value_start, value_end);
        let important = take_important(&mut value);
        if !important && let Some(last) = value.last_mut() {
            last.whitespace = if last.whitespace.contains(WhitespaceFlags::BEFORE) {
                WhitespaceFlags::BEFORE
            } else {
                WhitespaceFlags::default()
            };
        }
        self.index = value_end;
        if self.current_kind() == TokenKind::Semicolon {
            self.index += 1;
        }
        Rule {
            loc,
            data: RuleData::Declaration(DeclarationRule {
                key_text,
                value,
                key_range: key_token.range,
                important,
                ..DeclarationRule::default()
            }),
        }
    }

    fn starts_declaration(&self) -> bool {
        if self.current_kind() != TokenKind::Ident {
            return false;
        }
        let mut index = self.index + 1;
        while self.kind_at(index) == TokenKind::Whitespace {
            index += 1;
        }
        self.kind_at(index) == TokenKind::Colon
    }

    fn scan_to_rule_delimiter(&self) -> usize {
        let mut index = self.index;
        let mut stack = Vec::new();
        while index < self.tokens.len() {
            let kind = self.kind_at(index);
            if stack.is_empty()
                && matches!(
                    kind,
                    TokenKind::OpenBrace | TokenKind::Semicolon | TokenKind::CloseBrace
                )
            {
                break;
            }
            update_stack(&mut stack, kind);
            index += 1;
        }
        index
    }

    fn scan_declaration_end(&self) -> usize {
        let mut index = self.index;
        let mut stack = Vec::new();
        while index < self.tokens.len() {
            let kind = self.kind_at(index);
            if stack.is_empty() && matches!(kind, TokenKind::Semicolon | TokenKind::CloseBrace) {
                break;
            }
            update_stack(&mut stack, kind);
            index += 1;
        }
        index
    }

    fn scan_balanced_block(&self, start: usize) -> usize {
        let mut stack = Vec::new();
        let mut index = start;
        while index < self.tokens.len() {
            update_stack(&mut stack, self.kind_at(index));
            index += 1;
            if stack.is_empty() {
                break;
            }
        }
        index
    }

    fn convert_tokens(&mut self, start: usize, end: usize) -> Vec<Token> {
        let mut result: Vec<Token> = Vec::new();
        let mut pending_whitespace = false;
        let mut index = start;
        while index < end {
            let token = self.tokens[index];
            if token.kind == TokenKind::Whitespace {
                if !self.minify_whitespace {
                    if let Some(previous) = result.last_mut() {
                        previous.whitespace |= WhitespaceFlags::AFTER;
                    }
                    pending_whitespace = true;
                }
                index += 1;
                continue;
            }
            let mut converted = self.convert_token(token);
            if pending_whitespace {
                converted.whitespace |= WhitespaceFlags::BEFORE;
                pending_whitespace = false;
            }
            if let Some(close) = matching_close(token.kind) {
                let close_index = find_matching_close(&self.tokens, index + 1, end, close);
                converted.children = Some(self.convert_tokens(index + 1, close_index));
                index = close_index.saturating_add(1);
            } else {
                index += 1;
            }
            result.push(converted);
        }
        result
    }

    fn convert_token(&mut self, token: css_lexer::Token) -> Token {
        let text = self.decoded(token);
        let (kind, payload_index) = if token.kind == TokenKind::Url {
            let payload_index =
                u32::try_from(self.import_records.len()).expect("CSS URL count fits in u32");
            self.import_records.push(ImportRecord {
                path: Path {
                    text: text.clone(),
                    ..Path::default()
                },
                range: token.range,
                kind: ImportKind::Url,
                ..ImportRecord::default()
            });
            (TokenKind::Url, payload_index)
        } else {
            (token.kind, 0)
        };
        Token {
            text,
            loc: token.range.loc,
            payload_index,
            unit_offset: token.unit_offset,
            kind,
            ..Token::default()
        }
    }

    fn consume_through_semicolon(&mut self) {
        while !matches!(
            self.current_kind(),
            TokenKind::Semicolon | TokenKind::EndOfFile
        ) {
            self.index += 1;
        }
        if self.current_kind() == TokenKind::Semicolon {
            self.index += 1;
        }
    }

    fn skip_whitespace(&mut self) {
        while self.current_kind() == TokenKind::Whitespace {
            self.index += 1;
        }
    }

    fn current(&self) -> css_lexer::Token {
        self.tokens
            .get(self.index)
            .copied()
            .unwrap_or_else(|| css_lexer::Token {
                kind: TokenKind::EndOfFile,
                range: Range {
                    loc: Loc {
                        start: i32::try_from(self.source.contents.len())
                            .expect("CSS source fits in i32"),
                    },
                    len: 0,
                },
                ..css_lexer::Token::default()
            })
    }

    fn current_kind(&self) -> TokenKind {
        self.kind_at(self.index)
    }

    fn kind_at(&self, index: usize) -> TokenKind {
        self.tokens
            .get(index)
            .map_or(TokenKind::EndOfFile, |token| token.kind)
    }

    fn decoded(&self, token: css_lexer::Token) -> String {
        String::from_utf8_lossy(&token.decoded_text(&self.source.contents)).into_owned()
    }
}

fn update_stack(stack: &mut Vec<TokenKind>, kind: TokenKind) {
    if let Some(close) = matching_close(kind) {
        stack.push(close);
    } else if stack.last() == Some(&kind) {
        stack.pop();
    }
}

fn matching_close(kind: TokenKind) -> Option<TokenKind> {
    match kind {
        TokenKind::Function | TokenKind::OpenParen => Some(TokenKind::CloseParen),
        TokenKind::OpenBracket => Some(TokenKind::CloseBracket),
        TokenKind::OpenBrace => Some(TokenKind::CloseBrace),
        _ => None,
    }
}

fn find_matching_close(
    tokens: &[css_lexer::Token],
    start: usize,
    end: usize,
    initial_close: TokenKind,
) -> usize {
    let mut stack = vec![initial_close];
    let mut index = start;
    while index < end {
        let kind = tokens[index].kind;
        if let Some(close) = matching_close(kind) {
            stack.push(close);
        } else if stack.last() == Some(&kind) {
            stack.pop();
            if stack.is_empty() {
                return index;
            }
        }
        index += 1;
    }
    end
}

fn take_important(tokens: &mut Vec<Token>) -> bool {
    let Some(last) = tokens.last() else {
        return false;
    };
    if last.kind != TokenKind::Ident || !last.text.eq_ignore_ascii_case("important") {
        return false;
    }
    let Some(previous) = tokens.get(tokens.len().saturating_sub(2)) else {
        return false;
    };
    if previous.kind != TokenKind::DelimExclamation {
        return false;
    }
    tokens.truncate(tokens.len() - 2);
    true
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::{Options, parse};
    use crate::internal::{
        ast::SymbolMap,
        css_printer,
        logger::{DeferLogKind, Log, PrettyPaths, Source},
    };

    fn source(contents: &str) -> Source {
        Source {
            pretty_paths: PrettyPaths {
                abs: "<stdin>".into(),
                rel: "<stdin>".into(),
            },
            contents: Arc::from(contents.as_bytes()),
            ..Source::default()
        }
    }

    fn parse_and_print(contents: &str, minify_whitespace: bool) -> String {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let tree = parse(
            log.clone(),
            source(contents),
            Options {
                minify_whitespace,
                ..Options::default()
            },
        );
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = tree.symbols.clone();
        String::from_utf8(
            css_printer::print(
                &tree,
                &symbols,
                css_printer::Options {
                    minify_whitespace,
                    ..css_printer::Options::default()
                },
            )
            .css,
        )
        .expect("CSS output is UTF-8")
    }

    #[test]
    fn parses_and_prints_qualified_rules_and_declarations() {
        assert_eq!(
            parse_and_print(".card { color: red; margin: 0 !important; }", false),
            ".card {\n  color: red;\n  margin: 0 !important;\n}\n"
        );
        assert_eq!(
            parse_and_print(".card { color: red; margin: 0 !important; }", true),
            ".card{color:red;margin:0!important}"
        );
    }

    #[test]
    fn parses_nested_function_tokens() {
        assert_eq!(
            parse_and_print("a { width: calc(100% - 1px) }", false),
            "a {\n  width: calc(100% - 1px);\n}\n"
        );
    }

    #[test]
    fn extracts_import_and_url_records() {
        assert_eq!(
            parse_and_print(
                "@import \"theme.css\"; a { background: url(image.png) }",
                false
            ),
            "@import \"theme.css\";\na {\n  background: url(image.png);\n}\n"
        );
    }
}
