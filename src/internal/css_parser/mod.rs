//! Port of upstream `internal/css_parser`.

use std::collections::HashMap;

use crate::internal::{
    ast::{CharFreq, ImportKind, ImportRecord, LocRef, Ref, Symbol, SymbolKind},
    css_ast::{
        Ast, AtCharsetRule, AtImportRule, AtKeyframesRule, AtLayerRule, AtMediaRule,
        BadDeclarationRule, ClassSelector, Combinator, CommentRule, ComplexSelector, Composes,
        CompoundSelector, DeclarationRule, HashSelector, ImportConditions, ImportedComposesName,
        KeyframeBlock, KnownAtRule, MediaArbitraryTokensQuery, MediaQuery, MediaQueryData,
        NameToken, NamespacedName, PseudoClassSelector, QualifiedRule, Rule, RuleData,
        SelectorRule, SubclassData, SubclassSelector, Token, UnknownAtRule, WhitespaceFlags,
        rules_equal, tokens_are_comma_separated,
    },
    css_lexer::{self, TokenKind},
    logger::{Loc, Log, Path, Range, Source},
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Options {
    pub minify_syntax: bool,
    pub minify_whitespace: bool,
    pub minify_identifiers: bool,
    pub symbol_mode: SymbolMode,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SymbolMode {
    #[default]
    Disabled,
    Global,
    Local,
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
        legal_comments: result.legal_comments,
        legal_comment_index: 0,
        minify_syntax: options.minify_syntax,
        minify_whitespace: options.minify_whitespace,
        index: 0,
        import_records: Vec::new(),
        symbols: Vec::new(),
        local_symbols: Vec::new(),
        local_scope: HashMap::new(),
        global_scope: HashMap::new(),
        make_local_symbols: options.symbol_mode == SymbolMode::Local,
        composes: HashMap::new(),
        composes_target: None,
    };
    let rules = parser.parse_rule_list(false, true);
    let char_freq = if options.minify_identifiers {
        let mut frequency = CharFreq::default();
        frequency.scan(&parser.source.contents, 1);
        for comment in &result.all_comments {
            frequency.scan(parser.source.text_for_range(*comment), -1);
        }
        for record in &parser.import_records {
            frequency.scan(record.path.text.as_bytes(), -1);
        }
        for symbol in &parser.symbols {
            if symbol.kind == SymbolKind::LocalCss {
                frequency.scan(
                    symbol.original_name.as_bytes(),
                    -i32::try_from(symbol.use_count_estimate).unwrap_or(i32::MAX),
                );
            }
        }
        Some(frequency)
    } else {
        None
    };
    Ast {
        rules,
        symbols: parser.symbols,
        char_freq,
        import_records: parser.import_records,
        local_symbols: parser.local_symbols,
        local_scope: parser.local_scope,
        global_scope: parser.global_scope,
        composes: parser.composes,
        source_map_comment: result.source_map_comment,
        approximate_line_count: result.approximate_line_count,
        ..Ast::default()
    }
}

struct Parser {
    source: Source,
    tokens: Vec<css_lexer::Token>,
    legal_comments: Vec<css_lexer::Comment>,
    legal_comment_index: usize,
    minify_syntax: bool,
    minify_whitespace: bool,
    index: usize,
    import_records: Vec<ImportRecord>,
    symbols: Vec<Symbol>,
    local_symbols: Vec<LocRef>,
    local_scope: HashMap<String, LocRef>,
    global_scope: HashMap<String, LocRef>,
    make_local_symbols: bool,
    composes: HashMap<Ref, Composes>,
    composes_target: Option<Ref>,
}

impl Parser {
    fn parse_rule_list(
        &mut self,
        stop_at_close_brace: bool,
        preserve_legal_comments: bool,
    ) -> Vec<Rule> {
        let mut rules = Vec::new();
        loop {
            self.append_legal_comments(&mut rules, preserve_legal_comments);
            self.skip_whitespace();
            match self.current_kind() {
                TokenKind::EndOfFile => break,
                TokenKind::CloseBrace if stop_at_close_brace => {
                    self.index += 1;
                    break;
                }
                TokenKind::Semicolon => self.index += 1,
                TokenKind::AtKeyword => {
                    rules.push(self.parse_at_rule(preserve_legal_comments));
                }
                _ if stop_at_close_brace && self.starts_declaration() => {
                    if let Some(rule) = self.parse_declaration() {
                        rules.push(rule);
                    }
                }
                _ => rules.push(self.parse_qualified_rule()),
            }
        }
        if self.minify_syntax {
            rules.retain(|rule| {
                !matches!(
                    &rule.data,
                    RuleData::Selector(selector) if selector.rules.is_empty()
                )
            });
            merge_adjacent_selector_rules(&mut rules);
        }
        rules
    }

    fn append_legal_comments(&mut self, rules: &mut Vec<Rule>, preserve: bool) {
        while let Some(comment) = self.legal_comments.get(self.legal_comment_index) {
            let token_index_after =
                usize::try_from(comment.token_index_after).unwrap_or(usize::MAX);
            if token_index_after > self.index {
                break;
            }
            if preserve && token_index_after == self.index {
                rules.push(Rule {
                    loc: comment.loc,
                    data: RuleData::Comment(CommentRule {
                        text: String::from_utf8_lossy(&comment.text).into_owned(),
                    }),
                });
            }
            self.legal_comment_index += 1;
        }
    }

    fn parse_at_rule(&mut self, preserve_legal_comments: bool) -> Rule {
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
        let mut prelude = self.convert_tokens(prelude_start, end);
        trim_token_boundary_whitespace(&mut prelude);
        self.index = end;
        if self.current_kind() == TokenKind::OpenBrace {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "keyframes" | "-webkit-keyframes"
            ) {
                return self.parse_keyframes(loc, name, &prelude);
            }
            if name.eq_ignore_ascii_case("media") {
                let queries = split_media_queries(prelude, loc);
                self.index += 1;
                let rules = self.parse_rule_list(true, preserve_legal_comments);
                return Rule {
                    loc,
                    data: RuleData::AtMedia(AtMediaRule {
                        queries,
                        rules,
                        ..AtMediaRule::default()
                    }),
                };
            }
            if name.eq_ignore_ascii_case("layer") {
                let names = parse_layer_names(&prelude);
                self.index += 1;
                let rules = self.parse_rule_list(true, preserve_legal_comments);
                return Rule {
                    loc,
                    data: RuleData::AtLayer(AtLayerRule {
                        names,
                        rules,
                        ..AtLayerRule::default()
                    }),
                };
            }
            if is_known_block_at_rule(&name) {
                self.index += 1;
                let preserve_legal_comments =
                    preserve_legal_comments && known_at_rule_preserves_legal_comments(&name);
                let rules = self.parse_rule_list(true, preserve_legal_comments);
                return Rule {
                    loc,
                    data: RuleData::KnownAt(KnownAtRule {
                        at_token: name,
                        prelude,
                        rules,
                        ..KnownAtRule::default()
                    }),
                };
            }
        }
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

    fn parse_keyframes(&mut self, loc: Loc, at_token: String, prelude: &[Token]) -> Rule {
        let name_token = prelude.first();
        let name = name_token.map_or_else(String::new, |token| token.text.clone());
        let name_loc = name_token.map_or(loc, |token| token.loc);
        let name_ref = self.new_css_symbol(&name, name_loc);
        self.index += 1;
        let mut blocks = Vec::new();
        loop {
            self.skip_whitespace();
            if self.current_kind() == TokenKind::CloseBrace {
                self.index += 1;
                break;
            }
            if self.current_kind() == TokenKind::EndOfFile {
                break;
            }
            let selector_loc = self.current().range.loc;
            let selector_start = self.index;
            let selector_end = self.scan_to_rule_delimiter();
            let selector_tokens = self.convert_tokens(selector_start, selector_end);
            self.index = selector_end;
            if self.current_kind() != TokenKind::OpenBrace {
                if self.current_kind() == TokenKind::Semicolon {
                    self.index += 1;
                }
                continue;
            }
            self.index += 1;
            let rules = self.parse_rule_list(true, false);
            let mut selectors = keyframe_selectors(&selector_tokens);
            if self.minify_syntax {
                for selector in &mut selectors {
                    if selector.eq_ignore_ascii_case("from") {
                        "0%".clone_into(selector);
                    }
                }
            }
            blocks.push(KeyframeBlock {
                selectors,
                rules,
                loc: selector_loc,
                ..KeyframeBlock::default()
            });
        }
        Rule {
            loc,
            data: RuleData::AtKeyframes(AtKeyframesRule {
                at_token,
                name: LocRef {
                    loc: name_loc,
                    reference: name_ref,
                },
                blocks,
                ..AtKeyframesRule::default()
            }),
        }
    }

    fn new_css_symbol(&mut self, name: &str, loc: Loc) -> Ref {
        let existing = if self.make_local_symbols {
            self.local_scope.get(name)
        } else {
            self.global_scope.get(name)
        };
        if let Some(reference) = existing.map(|loc_ref| loc_ref.reference) {
            self.symbols[reference.inner_index as usize].use_count_estimate += 1;
            return reference;
        }
        let reference = Ref {
            source_index: self.source.index,
            inner_index: u32::try_from(self.symbols.len()).expect("CSS symbol count fits in u32"),
        };
        let kind = if self.make_local_symbols {
            SymbolKind::LocalCss
        } else {
            SymbolKind::GlobalCss
        };
        let mut symbol = Symbol::new(kind, name);
        symbol.use_count_estimate = 1;
        self.symbols.push(symbol);
        let loc_ref = LocRef { loc, reference };
        if self.make_local_symbols {
            self.local_symbols.push(loc_ref);
            self.local_scope.insert(name.into(), loc_ref);
        } else {
            self.global_scope.insert(name.into(), loc_ref);
        }
        reference
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
        let mut conditions = self.convert_tokens(conditions_start, self.index);
        trim_token_boundary_whitespace(&mut conditions);
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
        let prelude = self.convert_tokens_preserving_whitespace(prelude_start, end);
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
        let selectors = self.parse_complex_selectors(&prelude);
        let old_composes_target = self.composes_target;
        self.composes_target = selectors.as_deref().and_then(single_class_selector);
        let rules = self.parse_rule_list(true, false);
        self.composes_target = old_composes_target;
        if let Some(selectors) = selectors {
            Rule {
                loc,
                data: RuleData::Selector(SelectorRule {
                    selectors,
                    rules,
                    ..SelectorRule::default()
                }),
            }
        } else {
            Rule {
                loc,
                data: RuleData::Qualified(QualifiedRule {
                    prelude,
                    rules,
                    ..QualifiedRule::default()
                }),
            }
        }
    }

    fn parse_complex_selectors(&mut self, tokens: &[Token]) -> Option<Vec<ComplexSelector>> {
        let mut selectors = Vec::new();
        let mut start = 0;
        for index in 0..=tokens.len() {
            if index == tokens.len() || tokens[index].kind == TokenKind::Comma {
                if start == index {
                    return None;
                }
                selectors.push(self.parse_complex_selector(&tokens[start..index])?);
                start = index + 1;
            }
        }
        Some(selectors)
    }

    #[allow(clippy::too_many_lines)]
    fn parse_complex_selector(&mut self, tokens: &[Token]) -> Option<ComplexSelector> {
        let mut selectors = Vec::new();
        let mut compound = CompoundSelector::default();
        let mut pending_combinator = Combinator::default();
        let mut index = 0;
        while index < tokens.len() {
            let token = &tokens[index];
            if matches!(
                token.kind,
                TokenKind::DelimGreaterThan | TokenKind::DelimPlus | TokenKind::DelimTilde
            ) {
                push_compound(&mut selectors, &mut compound);
                pending_combinator = Combinator {
                    loc: token.loc,
                    byte: token.text.as_bytes().first().copied().unwrap_or_default(),
                };
                index += 1;
                continue;
            }
            if token.whitespace.contains(WhitespaceFlags::BEFORE) && !compound_is_empty(&compound) {
                push_compound(&mut selectors, &mut compound);
            }
            if compound_is_empty(&compound) {
                compound.combinator = pending_combinator;
                pending_combinator = Combinator::default();
            }
            match token.kind {
                TokenKind::Ident | TokenKind::DelimAsterisk
                    if compound.type_selector.is_none()
                        && compound.subclass_selectors.is_empty()
                        && compound.nesting_selector_locs.is_empty() =>
                {
                    compound.type_selector = Some(NamespacedName {
                        name: NameToken {
                            text: token.text.clone(),
                            range: Range {
                                loc: token.loc,
                                len: i32::try_from(token.text.len()).unwrap_or(i32::MAX),
                            },
                            kind: token.kind,
                        },
                        ..NamespacedName::default()
                    });
                }
                TokenKind::DelimAmpersand => compound.nesting_selector_locs.push(token.loc),
                TokenKind::DelimDot => {
                    let name = tokens.get(index + 1)?;
                    if name.kind != TokenKind::Ident {
                        return None;
                    }
                    let reference = self.new_css_symbol(&name.text, name.loc);
                    compound.subclass_selectors.push(SubclassSelector {
                        data: SubclassData::Class(ClassSelector {
                            name: LocRef {
                                loc: name.loc,
                                reference,
                            },
                        }),
                        range: Range {
                            loc: token.loc,
                            len: i32::try_from(name.text.len() + 1).unwrap_or(i32::MAX),
                        },
                    });
                    index += 1;
                }
                TokenKind::Hash => {
                    let reference = self.new_css_symbol(&token.text, token.loc);
                    compound.subclass_selectors.push(SubclassSelector {
                        data: SubclassData::Hash(HashSelector {
                            name: LocRef {
                                loc: token.loc,
                                reference,
                            },
                        }),
                        range: Range {
                            loc: token.loc,
                            len: i32::try_from(token.text.len() + 1).unwrap_or(i32::MAX),
                        },
                    });
                }
                TokenKind::Colon => {
                    let mut is_element = false;
                    let mut name_index = index + 1;
                    if tokens
                        .get(name_index)
                        .is_some_and(|token| token.kind == TokenKind::Colon)
                    {
                        is_element = true;
                        name_index += 1;
                    }
                    let name = tokens.get(name_index)?;
                    if !matches!(name.kind, TokenKind::Ident | TokenKind::Function) {
                        return None;
                    }
                    if !is_element
                        && name.kind == TokenKind::Function
                        && matches!(name.text.to_ascii_lowercase().as_str(), "global" | "local")
                    {
                        let old_make_local_symbols = self.make_local_symbols;
                        self.make_local_symbols = name.text.eq_ignore_ascii_case("local");
                        let parsed = self
                            .parse_complex_selectors(name.children.as_deref().unwrap_or_default());
                        self.make_local_symbols = old_make_local_symbols;
                        let parsed = parsed?;
                        if parsed.len() != 1 || parsed[0].selectors.len() != 1 {
                            return None;
                        }
                        let mut inner = parsed.into_iter().next()?.selectors.into_iter().next()?;
                        if inner.combinator.byte != 0
                            || compound.type_selector.is_some() && inner.type_selector.is_some()
                        {
                            return None;
                        }
                        if compound.type_selector.is_none() {
                            compound.type_selector = inner.type_selector.take();
                        }
                        compound
                            .nesting_selector_locs
                            .append(&mut inner.nesting_selector_locs);
                        compound
                            .subclass_selectors
                            .append(&mut inner.subclass_selectors);
                        index = name_index + 1;
                        continue;
                    }
                    compound.subclass_selectors.push(SubclassSelector {
                        data: SubclassData::PseudoClass(PseudoClassSelector {
                            name: name.text.clone(),
                            args: name.children.clone().unwrap_or_default(),
                            is_element,
                        }),
                        range: Range {
                            loc: token.loc,
                            len: i32::try_from(name.text.len() + usize::from(is_element) + 1)
                                .unwrap_or(i32::MAX),
                        },
                    });
                    index = name_index;
                }
                _ => return None,
            }
            index += 1;
        }
        push_compound(&mut selectors, &mut compound);
        if selectors.is_empty() {
            None
        } else {
            Some(ComplexSelector { selectors })
        }
    }

    fn parse_declaration(&mut self) -> Option<Rule> {
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
        if self.minify_syntax {
            reduce_calc_expressions(&mut value);
        }
        if self.make_local_symbols && key_text.eq_ignore_ascii_case("composes") {
            self.process_composes(&value);
            self.index = value_end;
            if self.current_kind() == TokenKind::Semicolon {
                self.index += 1;
            }
            return None;
        }
        match key_text.to_ascii_lowercase().as_str() {
            "animation" | "-webkit-animation" => self.process_animation_shorthand(&mut value),
            "animation-name" | "-webkit-animation-name" => {
                self.process_animation_names(&mut value);
            }
            _ => {}
        }
        if self.minify_syntax {
            minify_numeric_tokens(&mut value);
            let key_lower = key_text.to_ascii_lowercase();
            if is_single_color_property(&key_lower) {
                minify_single_color(&mut value);
            } else if key_lower == "background" {
                for token in &mut value {
                    minify_single_color(std::slice::from_mut(token));
                }
            }
            if matches!(key_lower.as_str(), "margin" | "padding" | "inset") {
                minify_four_side_shorthand(&mut value);
            }
            if key_lower == "font-weight"
                && let [token] = value.as_mut_slice()
                && token.kind == TokenKind::Ident
            {
                if token.text.eq_ignore_ascii_case("normal") {
                    token.kind = TokenKind::Number;
                    token.text = "400".into();
                } else if token.text.eq_ignore_ascii_case("bold") {
                    token.kind = TokenKind::Number;
                    token.text = "700".into();
                }
            }
            if key_lower == "transform" {
                minify_transforms(&mut value);
            }
        }
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
        Some(Rule {
            loc,
            data: RuleData::Declaration(DeclarationRule {
                key_text,
                value,
                key_range: key_token.range,
                important,
                ..DeclarationRule::default()
            }),
        })
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

    fn process_animation_names(&mut self, tokens: &mut [Token]) {
        for index in 0..tokens.len() {
            if matches!(tokens[index].kind, TokenKind::Ident | TokenKind::String) {
                self.mark_animation_name(tokens, index);
            }
        }
    }

    fn process_composes(&mut self, tokens: &[Token]) {
        let Some(target) = self.composes_target else {
            return;
        };
        let mut names_end = tokens.len();
        let mut from_global = false;
        let mut external = None;
        if tokens.len() >= 2
            && tokens[tokens.len() - 2].kind == TokenKind::Ident
            && tokens[tokens.len() - 2].text.eq_ignore_ascii_case("from")
        {
            names_end -= 2;
            let location = &tokens[tokens.len() - 1];
            match location.kind {
                TokenKind::Ident if location.text.eq_ignore_ascii_case("global") => {
                    from_global = true;
                }
                TokenKind::String => {
                    let import_record_index = u32::try_from(self.import_records.len())
                        .expect("CSS import count fits in u32");
                    self.import_records.push(ImportRecord {
                        path: Path {
                            text: location.text.clone(),
                            ..Path::default()
                        },
                        range: Range {
                            loc: location.loc,
                            len: i32::try_from(location.text.len()).unwrap_or(i32::MAX),
                        },
                        kind: ImportKind::ComposesFrom,
                        ..ImportRecord::default()
                    });
                    external = Some(import_record_index);
                }
                TokenKind::Url => {
                    if let Some(record) =
                        self.import_records.get_mut(location.payload_index as usize)
                    {
                        record.kind = ImportKind::ComposesFrom;
                        external = Some(location.payload_index);
                    }
                }
                _ => return,
            }
        }
        if !tokens[..names_end]
            .iter()
            .all(|token| token.kind == TokenKind::Ident)
        {
            return;
        }
        if let Some(import_record_index) = external {
            let imported_names = tokens[..names_end]
                .iter()
                .map(|token| ImportedComposesName {
                    alias: token.text.clone(),
                    alias_loc: token.loc,
                    import_record_index,
                })
                .collect::<Vec<_>>();
            self.composes
                .entry(target)
                .or_default()
                .imported_names
                .extend(imported_names);
            return;
        }

        let old_make_local_symbols = self.make_local_symbols;
        if from_global {
            self.make_local_symbols = false;
        }
        let names = tokens[..names_end]
            .iter()
            .map(|token| LocRef {
                loc: token.loc,
                reference: self.new_css_symbol(&token.text, token.loc),
            })
            .collect::<Vec<_>>();
        self.make_local_symbols = old_make_local_symbols;
        self.composes.entry(target).or_default().names.extend(names);
    }

    fn process_animation_shorthand(&mut self, tokens: &mut [Token]) {
        #[derive(Default)]
        #[allow(clippy::struct_excessive_bools)]
        struct Found {
            timing_function: bool,
            iteration_count: bool,
            direction: bool,
            fill_mode: bool,
            play_state: bool,
            name: bool,
        }

        let mut found = Found::default();
        for index in 0..tokens.len() {
            match tokens[index].kind {
                TokenKind::Comma => found = Found::default(),
                TokenKind::Number if !found.iteration_count => found.iteration_count = true,
                TokenKind::Ident => {
                    let lower = tokens[index].text.to_ascii_lowercase();
                    if !found.timing_function
                        && matches!(
                            lower.as_str(),
                            "linear"
                                | "ease"
                                | "ease-in"
                                | "ease-out"
                                | "ease-in-out"
                                | "step-start"
                                | "step-end"
                        )
                    {
                        found.timing_function = true;
                    } else if !found.iteration_count && lower == "infinite" {
                        found.iteration_count = true;
                    } else if !found.direction
                        && matches!(
                            lower.as_str(),
                            "normal" | "reverse" | "alternate" | "alternate-reverse"
                        )
                    {
                        found.direction = true;
                    } else if !found.fill_mode
                        && matches!(lower.as_str(), "none" | "forwards" | "backwards" | "both")
                    {
                        found.fill_mode = true;
                    } else if !found.play_state && matches!(lower.as_str(), "running" | "paused") {
                        found.play_state = true;
                    } else if !found.name {
                        self.mark_animation_name(tokens, index);
                        found.name = true;
                    }
                }
                TokenKind::String if !found.name => {
                    self.mark_animation_name(tokens, index);
                    found.name = true;
                }
                _ => {}
            }
        }
    }

    fn mark_animation_name(&mut self, tokens: &mut [Token], index: usize) {
        let token = &tokens[index];
        let lower = token.text.to_ascii_lowercase();
        let is_reserved = matches!(
            lower.as_str(),
            "none" | "initial" | "inherit" | "unset" | "default" | "revert" | "revert-layer"
        );
        if is_reserved && (token.kind == TokenKind::Ident || !self.make_local_symbols) {
            return;
        }
        let reference = self.new_css_symbol(&token.text, token.loc);
        tokens[index].kind = TokenKind::Symbol;
        tokens[index].payload_index = reference.inner_index;
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
        self.convert_tokens_with_context(start, end, false)
    }

    fn convert_tokens_with_context(
        &mut self,
        start: usize,
        end: usize,
        inside_calc: bool,
    ) -> Vec<Token> {
        let mut result: Vec<Token> = Vec::new();
        let mut pending_whitespace = false;
        let mut index = start;
        while index < end {
            let token = self.tokens[index];
            if token.kind == TokenKind::Whitespace {
                pending_whitespace = true;
                index += 1;
                continue;
            }
            let mut converted = self.convert_token(token);
            if pending_whitespace {
                let keep_whitespace = !self.minify_whitespace
                    || result
                        .last()
                        .is_some_and(|previous| whitespace_is_required(previous, &converted))
                    || inside_calc
                        && numeric_calc_division_whitespace(
                            &result,
                            &converted,
                            next_non_whitespace_kind(&self.tokens, index + 1, end),
                        )
                    || inside_calc && calc_product_operator_whitespace(&result, &converted);
                if keep_whitespace {
                    if let Some(previous) = result.last_mut() {
                        previous.whitespace |= WhitespaceFlags::AFTER;
                    }
                    converted.whitespace |= WhitespaceFlags::BEFORE;
                }
                pending_whitespace = false;
            }
            if let Some(close) = matching_close(token.kind) {
                let close_index = find_matching_close(&self.tokens, index + 1, end, close);
                let child_inside_calc = inside_calc
                    || converted.kind == TokenKind::Function
                        && converted.text.eq_ignore_ascii_case("calc");
                converted.children = Some(self.convert_tokens_with_context(
                    index + 1,
                    close_index,
                    child_inside_calc,
                ));
                index = close_index.saturating_add(1);
            } else {
                index += 1;
            }
            result.push(converted);
        }
        result
    }

    fn convert_tokens_preserving_whitespace(&mut self, start: usize, end: usize) -> Vec<Token> {
        let minify_whitespace = self.minify_whitespace;
        self.minify_whitespace = false;
        let result = self.convert_tokens(start, end);
        self.minify_whitespace = minify_whitespace;
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

fn compound_is_empty(compound: &CompoundSelector) -> bool {
    compound.type_selector.is_none()
        && compound.subclass_selectors.is_empty()
        && compound.nesting_selector_locs.is_empty()
}

fn merge_adjacent_selector_rules(rules: &mut Vec<Rule>) {
    let mut previous_selector_index: Option<usize> = None;
    let mut index = 0;
    while index < rules.len() {
        let current = match &rules[index].data {
            RuleData::Selector(selector) => selector.clone(),
            RuleData::Comment(_) => {
                index += 1;
                continue;
            }
            _ => {
                previous_selector_index = None;
                index += 1;
                continue;
            }
        };
        if let Some(previous_index) = previous_selector_index
            && let RuleData::Selector(previous) = &mut rules[previous_index].data
            && rules_equal(&current.rules, &previous.rules, None)
            && selectors_are_safe_to_merge(&current.selectors)
            && selectors_are_safe_to_merge(&previous.selectors)
        {
            for selector in current.selectors {
                if !previous
                    .selectors
                    .iter()
                    .any(|existing| selector.equal(existing, None))
                {
                    previous.selectors.push(selector);
                }
            }
            rules.remove(index);
            continue;
        }
        previous_selector_index = Some(index);
        index += 1;
    }
}

fn selectors_are_safe_to_merge(selectors: &[ComplexSelector]) -> bool {
    selectors.iter().all(|complex| {
        complex.selectors.iter().all(|compound| {
            compound.nesting_selector_locs.is_empty()
                && compound.combinator.byte == 0
                && compound.type_selector.as_ref().is_none_or(|type_selector| {
                    type_selector.namespace_prefix.is_none()
                        && (type_selector.name.kind != TokenKind::Ident
                            || SAFE_TYPE_SELECTORS.contains(&type_selector.name.text.as_str()))
                })
                && compound
                    .subclass_selectors
                    .iter()
                    .all(|subclass| match &subclass.data {
                        SubclassData::Hash(_) | SubclassData::Class(_) => true,
                        SubclassData::Attribute(attribute) => attribute.matcher_modifier == 0,
                        SubclassData::PseudoClass(pseudo) => {
                            !pseudo.is_element
                                && pseudo.args.is_empty()
                                && matches!(
                                    pseudo.name.as_str(),
                                    "active" | "first-child" | "hover" | "link" | "visited"
                                )
                        }
                        SubclassData::PseudoWithSelectorList(_) => false,
                    })
        })
    })
}

const SAFE_TYPE_SELECTORS: &[&str] = &[
    "a",
    "abbr",
    "address",
    "area",
    "b",
    "base",
    "blockquote",
    "body",
    "br",
    "button",
    "caption",
    "cite",
    "code",
    "col",
    "colgroup",
    "dd",
    "del",
    "dfn",
    "div",
    "dl",
    "dt",
    "em",
    "embed",
    "fieldset",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "hr",
    "html",
    "i",
    "iframe",
    "img",
    "input",
    "ins",
    "kbd",
    "label",
    "legend",
    "li",
    "link",
    "map",
    "menu",
    "meta",
    "noscript",
    "object",
    "ol",
    "optgroup",
    "option",
    "p",
    "param",
    "pre",
    "q",
    "ruby",
    "s",
    "samp",
    "script",
    "select",
    "small",
    "span",
    "strong",
    "style",
    "sub",
    "sup",
    "table",
    "tbody",
    "td",
    "textarea",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "u",
    "ul",
    "var",
];

fn single_class_selector(selectors: &[ComplexSelector]) -> Option<Ref> {
    if selectors.len() != 1 || selectors[0].selectors.len() != 1 {
        return None;
    }
    let compound = &selectors[0].selectors[0];
    if compound.combinator.byte != 0
        || compound.type_selector.is_some()
        || !compound.nesting_selector_locs.is_empty()
        || compound.subclass_selectors.len() != 1
    {
        return None;
    }
    match &compound.subclass_selectors[0].data {
        SubclassData::Class(class) => Some(class.name.reference),
        _ => None,
    }
}

fn push_compound(selectors: &mut Vec<CompoundSelector>, compound: &mut CompoundSelector) {
    if !compound_is_empty(compound) {
        selectors.push(std::mem::take(compound));
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

fn split_media_queries(tokens: Vec<Token>, loc: Loc) -> Vec<MediaQuery> {
    let mut queries = Vec::new();
    let mut current = Vec::new();
    for token in tokens {
        if token.kind == TokenKind::Comma {
            trim_token_boundary_whitespace(&mut current);
            queries.push(MediaQuery {
                loc,
                data: MediaQueryData::ArbitraryTokens(MediaArbitraryTokensQuery {
                    tokens: std::mem::take(&mut current),
                }),
            });
        } else {
            current.push(token);
        }
    }
    if !current.is_empty() {
        trim_token_boundary_whitespace(&mut current);
        queries.push(MediaQuery {
            loc,
            data: MediaQueryData::ArbitraryTokens(MediaArbitraryTokensQuery { tokens: current }),
        });
    }
    queries
}

fn trim_token_boundary_whitespace(tokens: &mut [Token]) {
    if tokens.is_empty() {
        return;
    }
    let first_has_after = tokens[0].whitespace.contains(WhitespaceFlags::AFTER);
    tokens[0].whitespace = if first_has_after {
        WhitespaceFlags::AFTER
    } else {
        WhitespaceFlags::default()
    };
    let last = tokens.len() - 1;
    let last_has_before = tokens[last].whitespace.contains(WhitespaceFlags::BEFORE);
    tokens[last].whitespace = if last_has_before {
        WhitespaceFlags::BEFORE
    } else {
        WhitespaceFlags::default()
    };
}

fn whitespace_is_required(left: &Token, right: &Token) -> bool {
    if matches!(left.kind, TokenKind::DelimPlus | TokenKind::DelimMinus)
        || matches!(right.kind, TokenKind::DelimPlus | TokenKind::DelimMinus)
    {
        return true;
    }
    let can_end_word = matches!(
        left.kind,
        TokenKind::Ident
            | TokenKind::Symbol
            | TokenKind::Number
            | TokenKind::Dimension
            | TokenKind::Percentage
            | TokenKind::Hash
            | TokenKind::String
            | TokenKind::Url
            | TokenKind::Function
    );
    let can_start_word = matches!(
        right.kind,
        TokenKind::Ident
            | TokenKind::Symbol
            | TokenKind::Number
            | TokenKind::Dimension
            | TokenKind::Percentage
            | TokenKind::Hash
            | TokenKind::String
            | TokenKind::Url
            | TokenKind::Function
    );
    can_end_word && can_start_word
}

fn numeric_calc_division_whitespace(
    previous: &[Token],
    current: &Token,
    next_kind: TokenKind,
) -> bool {
    let is_numeric = |kind| {
        matches!(
            kind,
            TokenKind::Number | TokenKind::Percentage | TokenKind::Dimension
        )
    };
    match previous {
        [.., left] if current.kind == TokenKind::DelimSlash => {
            is_numeric(left.kind) && is_numeric(next_kind)
        }
        [.., left, slash] if slash.kind == TokenKind::DelimSlash => {
            is_numeric(left.kind) && is_numeric(current.kind)
        }
        _ => false,
    }
}

fn calc_product_operator_whitespace(previous: &[Token], current: &Token) -> bool {
    previous
        .last()
        .is_some_and(|token| matches!(token.kind, TokenKind::DelimAsterisk | TokenKind::DelimSlash))
        || matches!(
            current.kind,
            TokenKind::DelimAsterisk | TokenKind::DelimSlash
        )
}

fn next_non_whitespace_kind(
    tokens: &[css_lexer::Token],
    mut index: usize,
    end: usize,
) -> TokenKind {
    while index < end && tokens[index].kind == TokenKind::Whitespace {
        index += 1;
    }
    tokens
        .get(index)
        .map_or(TokenKind::EndOfFile, |token| token.kind)
}

fn minify_single_color(tokens: &mut [Token]) {
    let [token] = tokens else {
        return;
    };
    if token.kind == TokenKind::Ident {
        let Some(hex) = named_color_hex(&token.text.to_ascii_lowercase()) else {
            return;
        };
        let Some((red, green, blue, alpha)) = parse_hex_color(hex) else {
            return;
        };
        set_color_token(token, red, green, blue, alpha);
        return;
    }
    if token.kind == TokenKind::Hash {
        if let Some((red, green, blue, alpha)) = parse_hex_color(&token.text) {
            set_color_token(token, red, green, blue, alpha);
        }
        return;
    }
    if token.kind == TokenKind::Function
        && (token.text.eq_ignore_ascii_case("rgb") || token.text.eq_ignore_ascii_case("rgba"))
        && let Some((red, green, blue, alpha)) = parse_rgb(token)
    {
        set_color_token(token, red, green, blue, alpha);
    }
}

fn is_single_color_property(key: &str) -> bool {
    matches!(
        key,
        "background-color"
            | "border-block-end-color"
            | "border-block-start-color"
            | "border-bottom-color"
            | "border-color"
            | "border-inline-end-color"
            | "border-inline-start-color"
            | "border-left-color"
            | "border-right-color"
            | "border-top-color"
            | "caret-color"
            | "color"
            | "column-rule-color"
            | "fill"
            | "flood-color"
            | "lighting-color"
            | "outline-color"
            | "stop-color"
            | "stroke"
            | "text-decoration-color"
            | "text-emphasis-color"
    )
}

fn parse_hex_color(text: &str) -> Option<(u8, u8, u8, u8)> {
    if !matches!(text.len(), 3 | 4 | 6 | 8) || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let expand = |digit: u8| (digit << 4) | digit;
    let digit = |index: usize| {
        text.as_bytes()
            .get(index)
            .copied()
            .and_then(|byte| (byte as char).to_digit(16))
            .and_then(|value| u8::try_from(value).ok())
    };
    if text.len() <= 4 {
        Some((
            expand(digit(0)?),
            expand(digit(1)?),
            expand(digit(2)?),
            if text.len() == 4 {
                expand(digit(3)?)
            } else {
                255
            },
        ))
    } else {
        let byte = |index| Some((digit(index)? << 4) | digit(index + 1)?);
        Some((
            byte(0)?,
            byte(2)?,
            byte(4)?,
            if text.len() == 8 { byte(6)? } else { 255 },
        ))
    }
}

fn parse_rgb(token: &Token) -> Option<(u8, u8, u8, u8)> {
    let children = token.children.as_deref()?;
    let (red, green, blue, alpha) = if token.text.eq_ignore_ascii_case("rgb") {
        let [red, comma_one, green, comma_two, blue] = children else {
            return None;
        };
        if comma_one.kind != TokenKind::Comma || comma_two.kind != TokenKind::Comma {
            return None;
        }
        (red, green, blue, None)
    } else {
        let [red, comma_one, green, comma_two, blue, comma_three, alpha] = children else {
            return None;
        };
        if comma_one.kind != TokenKind::Comma
            || comma_two.kind != TokenKind::Comma
            || comma_three.kind != TokenKind::Comma
        {
            return None;
        }
        (red, green, blue, Some(alpha))
    };
    let parse_byte = |token: &Token, number_scale: f64| {
        let value = match token.kind {
            TokenKind::Number => token.text.parse::<f64>().ok()? * number_scale,
            TokenKind::Percentage => {
                token.percentage_value().parse::<f64>().ok()? * (255.0 / 100.0)
            }
            _ => return None,
        };
        value
            .round()
            .clamp(0.0, 255.0)
            .to_string()
            .parse::<u8>()
            .ok()
    };
    Some((
        parse_byte(red, 1.0)?,
        parse_byte(green, 1.0)?,
        parse_byte(blue, 1.0)?,
        alpha.map_or(Some(255), |alpha| parse_byte(alpha, 255.0))?,
    ))
}

fn set_color_token(token: &mut Token, red: u8, green: u8, blue: u8, alpha: u8) {
    token.children = None;
    if alpha == 255 {
        let hex = format!("{red:02x}{green:02x}{blue:02x}");
        if let Some(name) = short_color_name(&hex) {
            token.kind = TokenKind::Ident;
            token.text = name.into();
            return;
        }
        token.kind = TokenKind::Hash;
        token.text = compact_hex(&hex);
    } else {
        token.kind = TokenKind::Hash;
        token.text = compact_hex(&format!("{red:02x}{green:02x}{blue:02x}{alpha:02x}"));
    }
}

fn compact_hex(hex: &str) -> String {
    let bytes = hex.as_bytes();
    if bytes.chunks_exact(2).all(|pair| pair[0] == pair[1]) {
        bytes.chunks_exact(2).map(|pair| pair[0] as char).collect()
    } else {
        hex.into()
    }
}

fn short_color_name(hex: &str) -> Option<&'static str> {
    Some(match hex {
        "000080" => "navy",
        "008000" => "green",
        "008080" => "teal",
        "4b0082" => "indigo",
        "800000" => "maroon",
        "800080" => "purple",
        "808000" => "olive",
        "808080" => "gray",
        "a0522d" => "sienna",
        "a52a2a" => "brown",
        "c0c0c0" => "silver",
        "cd853f" => "peru",
        "d2b48c" => "tan",
        "da70d6" => "orchid",
        "dda0dd" => "plum",
        "ee82ee" => "violet",
        "f0e68c" => "khaki",
        "f0ffff" => "azure",
        "f5deb3" => "wheat",
        "f5f5dc" => "beige",
        "fa8072" => "salmon",
        "faf0e6" => "linen",
        "ff0000" => "red",
        "ff6347" => "tomato",
        "ff7f50" => "coral",
        "ffa500" => "orange",
        "ffc0cb" => "pink",
        "ffd700" => "gold",
        "ffe4c4" => "bisque",
        "fffafa" => "snow",
        "fffff0" => "ivory",
        _ => return None,
    })
}

fn named_color_hex(name: &str) -> Option<&'static str> {
    named_color_hex_a_g(name).or_else(|| named_color_hex_h_z(name))
}

fn named_color_hex_a_g(name: &str) -> Option<&'static str> {
    Some(match name {
        "black" => "000000",
        "silver" => "c0c0c0",
        "gray" | "grey" => "808080",
        "white" => "ffffff",
        "maroon" => "800000",
        "red" => "ff0000",
        "purple" => "800080",
        "fuchsia" => "ff00ff",
        "green" => "008000",
        "lime" => "00ff00",
        "olive" => "808000",
        "yellow" => "ffff00",
        "navy" => "000080",
        "blue" => "0000ff",
        "teal" => "008080",
        "aqua" | "cyan" => "00ffff",
        "orange" => "ffa500",
        "aliceblue" => "f0f8ff",
        "antiquewhite" => "faebd7",
        "aquamarine" => "7fffd4",
        "azure" => "f0ffff",
        "beige" => "f5f5dc",
        "bisque" => "ffe4c4",
        "blanchedalmond" => "ffebcd",
        "blueviolet" => "8a2be2",
        "brown" => "a52a2a",
        "burlywood" => "deb887",
        "cadetblue" => "5f9ea0",
        "chartreuse" => "7fff00",
        "chocolate" => "d2691e",
        "coral" => "ff7f50",
        "cornflowerblue" => "6495ed",
        "cornsilk" => "fff8dc",
        "crimson" => "dc143c",
        "darkblue" => "00008b",
        "darkcyan" => "008b8b",
        "darkgoldenrod" => "b8860b",
        "darkgray" | "darkgrey" => "a9a9a9",
        "darkgreen" => "006400",
        "darkkhaki" => "bdb76b",
        "darkmagenta" => "8b008b",
        "darkolivegreen" => "556b2f",
        "darkorange" => "ff8c00",
        "darkorchid" => "9932cc",
        "darkred" => "8b0000",
        "darksalmon" => "e9967a",
        "darkseagreen" => "8fbc8f",
        "darkslateblue" => "483d8b",
        "darkslategray" | "darkslategrey" => "2f4f4f",
        "darkturquoise" => "00ced1",
        "darkviolet" => "9400d3",
        "deeppink" => "ff1493",
        "deepskyblue" => "00bfff",
        "dimgray" | "dimgrey" => "696969",
        "dodgerblue" => "1e90ff",
        "firebrick" => "b22222",
        "floralwhite" => "fffaf0",
        "forestgreen" => "228b22",
        "gainsboro" => "dcdcdc",
        "ghostwhite" => "f8f8ff",
        "gold" => "ffd700",
        "goldenrod" => "daa520",
        "greenyellow" => "adff2f",
        _ => return None,
    })
}

fn named_color_hex_h_z(name: &str) -> Option<&'static str> {
    Some(match name {
        "honeydew" => "f0fff0",
        "hotpink" => "ff69b4",
        "indianred" => "cd5c5c",
        "indigo" => "4b0082",
        "ivory" => "fffff0",
        "khaki" => "f0e68c",
        "lavender" => "e6e6fa",
        "lavenderblush" => "fff0f5",
        "lawngreen" => "7cfc00",
        "lemonchiffon" => "fffacd",
        "lightblue" => "add8e6",
        "lightcoral" => "f08080",
        "lightcyan" => "e0ffff",
        "lightgoldenrodyellow" => "fafad2",
        "lightgray" | "lightgrey" => "d3d3d3",
        "lightgreen" => "90ee90",
        "lightpink" => "ffb6c1",
        "lightsalmon" => "ffa07a",
        "lightseagreen" => "20b2aa",
        "lightskyblue" => "87cefa",
        "lightslategray" | "lightslategrey" => "778899",
        "lightsteelblue" => "b0c4de",
        "lightyellow" => "ffffe0",
        "limegreen" => "32cd32",
        "linen" => "faf0e6",
        "magenta" => "ff00ff",
        "mediumaquamarine" => "66cdaa",
        "mediumblue" => "0000cd",
        "mediumorchid" => "ba55d3",
        "mediumpurple" => "9370db",
        "mediumseagreen" => "3cb371",
        "mediumslateblue" => "7b68ee",
        "mediumspringgreen" => "00fa9a",
        "mediumturquoise" => "48d1cc",
        "mediumvioletred" => "c71585",
        "midnightblue" => "191970",
        "mintcream" => "f5fffa",
        "mistyrose" => "ffe4e1",
        "moccasin" => "ffe4b5",
        "navajowhite" => "ffdead",
        "oldlace" => "fdf5e6",
        "olivedrab" => "6b8e23",
        "orangered" => "ff4500",
        "orchid" => "da70d6",
        "palegoldenrod" => "eee8aa",
        "palegreen" => "98fb98",
        "paleturquoise" => "afeeee",
        "palevioletred" => "db7093",
        "papayawhip" => "ffefd5",
        "peachpuff" => "ffdab9",
        "peru" => "cd853f",
        "pink" => "ffc0cb",
        "plum" => "dda0dd",
        "powderblue" => "b0e0e6",
        "rosybrown" => "bc8f8f",
        "royalblue" => "4169e1",
        "saddlebrown" => "8b4513",
        "salmon" => "fa8072",
        "sandybrown" => "f4a460",
        "seagreen" => "2e8b57",
        "seashell" => "fff5ee",
        "sienna" => "a0522d",
        "skyblue" => "87ceeb",
        "slateblue" => "6a5acd",
        "slategray" | "slategrey" => "708090",
        "snow" => "fffafa",
        "springgreen" => "00ff7f",
        "steelblue" => "4682b4",
        "tan" => "d2b48c",
        "thistle" => "d8bfd8",
        "tomato" => "ff6347",
        "turquoise" => "40e0d0",
        "violet" => "ee82ee",
        "wheat" => "f5deb3",
        "whitesmoke" => "f5f5f5",
        "yellowgreen" => "9acd32",
        "rebeccapurple" => "663399",
        _ => return None,
    })
}

fn minify_numeric_tokens(tokens: &mut [Token]) {
    for token in tokens {
        if let Some(children) = &mut token.children {
            minify_numeric_tokens(children);
        }
        match token.kind {
            TokenKind::Number => {
                if let Some(text) = minify_decimal(&token.text) {
                    token.text = text;
                }
            }
            TokenKind::Percentage => {
                if let Some(text) = minify_decimal(token.percentage_value()) {
                    token.text = format!("{text}%");
                }
            }
            TokenKind::Dimension => {
                let unit = token.dimension_unit().to_owned();
                if let Some(text) = minify_decimal(token.dimension_value()) {
                    let Ok(unit_offset) = u16::try_from(text.len()) else {
                        continue;
                    };
                    token.unit_offset = unit_offset;
                    token.text = format!("{text}{unit}");
                }
            }
            _ => {}
        }
    }
}

fn minify_decimal(text: &str) -> Option<String> {
    if text.contains(['e', 'E']) || !text.contains('.') {
        return None;
    }
    let mut result = text.trim_end_matches('0').trim_end_matches('.').to_owned();
    if let Some(fraction) = result.strip_prefix("0.") {
        result = format!(".{fraction}");
    } else if let Some(fraction) = result.strip_prefix("-0.") {
        result = format!("-.{fraction}");
    }
    if result.is_empty() || result == "-" {
        result.push('0');
    }
    (result.len() < text.len()).then_some(result)
}

fn minify_four_side_shorthand(tokens: &mut Vec<Token>) {
    if tokens.len() == 4 && tokens[1].equal_ignoring_whitespace(&tokens[3]) {
        tokens.pop();
    }
    if tokens.len() == 3 && tokens[0].equal_ignoring_whitespace(&tokens[2]) {
        tokens.pop();
    }
    if tokens.len() == 2 && tokens[0].equal_ignoring_whitespace(&tokens[1]) {
        tokens.pop();
    }
    for (index, token) in tokens.iter_mut().enumerate() {
        token.whitespace = if index == 0 {
            WhitespaceFlags::default()
        } else {
            WhitespaceFlags::BEFORE
        };
    }
}

fn minify_transforms(tokens: &mut [Token]) {
    for token in tokens {
        if token.kind != TokenKind::Function {
            continue;
        }
        let name = token.text.to_ascii_lowercase();
        let Some(args) = &mut token.children else {
            continue;
        };
        if !tokens_are_comma_separated(args) {
            continue;
        }
        minify_2d_transform(&mut token.text, args, &name);
        minify_3d_transform(&mut token.text, args, &name);
        trim_token_boundary_whitespace(args);
    }
}

fn minify_2d_transform(text: &mut String, args: &mut Vec<Token>, name: &str) {
    match name {
        "matrix" if args.len() == 11 => minify_2d_matrix(text, args),
        "translate" | "translatey" if args.len() == 1 => {
            args[0].turn_length_or_percentage_into_number_if_zero();
        }
        "translate" if args.len() == 3 => {
            args[0].turn_length_or_percentage_into_number_if_zero();
            args[2].turn_length_or_percentage_into_number_if_zero();
            if args[2].is_zero() {
                args.truncate(1);
            } else if args[0].is_zero() {
                *text = "translateY".into();
                *args = vec![args[2].clone()];
            }
        }
        "translatex" if args.len() == 1 => {
            *text = "translate".into();
            args[0].turn_length_or_percentage_into_number_if_zero();
        }
        "scale" if args.len() == 1 => percent_to_number_if_shorter(&mut args[0]),
        "scale" if args.len() == 3 => {
            percent_to_number_if_shorter(&mut args[0]);
            percent_to_number_if_shorter(&mut args[2]);
            if args[0].equal_ignoring_whitespace(&args[2]) {
                args.truncate(1);
            } else if args[2].is_one() {
                *text = "scaleX".into();
                args.truncate(1);
            } else if args[0].is_one() {
                *text = "scaleY".into();
                *args = vec![args[2].clone()];
            }
        }
        "scalex" | "scaley" | "scalez" if args.len() == 1 => {
            percent_to_number_if_shorter(&mut args[0]);
        }
        "rotate" | "rotatex" | "rotatey" | "perspective" | "skew" | "skewy" if args.len() == 1 => {
            args[0].turn_length_into_number_if_zero();
        }
        "rotatez" if args.len() == 1 => {
            *text = "rotate".into();
            args[0].turn_length_into_number_if_zero();
        }
        "skew" if args.len() == 3 => {
            args[0].turn_length_into_number_if_zero();
            args[2].turn_length_into_number_if_zero();
            if args[2].is_zero() {
                args.truncate(1);
            }
        }
        "skewx" if args.len() == 1 => {
            *text = "skew".into();
            args[0].turn_length_into_number_if_zero();
        }
        _ => {}
    }
}

fn minify_2d_matrix(text: &mut String, args: &mut Vec<Token>) {
    let (scale_x, skew_y, skew_x, scale_y, translate_x, translate_y) = (
        args[0].clone(),
        args[2].clone(),
        args[4].clone(),
        args[6].clone(),
        args[8].clone(),
        args[10].clone(),
    );
    if skew_y.is_zero() && skew_x.is_zero() && translate_x.is_zero() && translate_y.is_zero() {
        *text = if scale_x.equal_ignoring_whitespace(&scale_y) {
            *args = vec![scale_x];
            "scale".into()
        } else if scale_y.is_one() {
            *args = vec![scale_x];
            "scaleX".into()
        } else if scale_x.is_one() {
            *args = vec![scale_y];
            "scaleY".into()
        } else {
            *args = vec![scale_x, comma_token(), scale_y];
            "scale".into()
        };
    }
}

fn minify_3d_transform(text: &mut String, args: &mut Vec<Token>, name: &str) {
    const ONLY_SCALE: u32 = 0b1000_0000_0000_0000_0111_1011_1101_1110;
    match name {
        "matrix3d" if args.len() == 31 => {
            let mut mask = 0u32;
            for (index, argument) in args.iter().step_by(2).enumerate() {
                if argument.is_zero() {
                    mask |= 1 << index;
                } else if argument.is_one() {
                    mask |= (1 << 16) << index;
                }
            }
            if mask & ONLY_SCALE == ONLY_SCALE {
                let (scale_x, scale_y, scale_z) =
                    (args[0].clone(), args[10].clone(), args[20].clone());
                if scale_x.is_one() && scale_y.is_one() {
                    *text = "scaleZ".into();
                    *args = vec![scale_z];
                } else {
                    *text = "scale3d".into();
                    *args = vec![scale_x, comma_token(), scale_y, comma_token(), scale_z];
                }
            }
        }
        "translate3d" if args.len() == 5 => {
            args[0].turn_length_or_percentage_into_number_if_zero();
            args[2].turn_length_or_percentage_into_number_if_zero();
            args[4].turn_length_into_number_if_zero();
            if args[0].is_zero() && args[2].is_zero() {
                *text = "translateZ".into();
                *args = vec![args[4].clone()];
            }
        }
        "translatez" if args.len() == 1 => {
            args[0].turn_length_into_number_if_zero();
        }
        "scale3d" if args.len() == 5 => {
            percent_to_number_if_shorter(&mut args[0]);
            percent_to_number_if_shorter(&mut args[2]);
            percent_to_number_if_shorter(&mut args[4]);
            if args[0].is_one() && args[2].is_one() {
                *text = "scaleZ".into();
                *args = vec![args[4].clone()];
            }
        }
        "rotate3d" if args.len() == 7 => {
            args[6].turn_length_into_number_if_zero();
            if args[0].is_one() && args[2].is_zero() && args[4].is_zero() {
                *text = "rotateX".into();
                *args = vec![args[6].clone()];
            } else if args[0].is_zero() && args[2].is_one() && args[4].is_zero() {
                *text = "rotateY".into();
                *args = vec![args[6].clone()];
            }
        }
        _ => {}
    }
}

fn comma_token() -> Token {
    Token {
        kind: TokenKind::Comma,
        text: ",".into(),
        ..Token::default()
    }
}

fn percent_to_number_if_shorter(token: &mut Token) {
    if token.kind != TokenKind::Percentage {
        return;
    }
    let text = token.percentage_value();
    let negative = text.starts_with('-');
    let unsigned = text.trim_start_matches(['+', '-']);
    let decimal_index = unsigned.find('.').unwrap_or(unsigned.len());
    let digits = unsigned.replace('.', "");
    let shifted_index = i32::try_from(decimal_index).unwrap_or(i32::MAX) - 2;
    let mut shifted = if shifted_index <= 0 {
        format!(
            "0.{}{}",
            "0".repeat(usize::try_from(-shifted_index).unwrap_or(0)),
            digits
        )
    } else {
        let index = usize::try_from(shifted_index).unwrap_or(digits.len());
        if index >= digits.len() {
            format!("{}{}", digits, "0".repeat(index - digits.len()))
        } else {
            format!("{}.{}", &digits[..index], &digits[index..])
        }
    };
    shifted = shifted
        .trim_start_matches('0')
        .trim_end_matches('0')
        .trim_end_matches('.')
        .into();
    if shifted.starts_with('.') {
        // Keep the leading dot omitted for minified CSS numbers.
    } else if shifted.is_empty() {
        shifted = "0".into();
    }
    if negative && shifted != "0" {
        shifted.insert(0, '-');
    }
    if shifted.len() < token.text.len() {
        token.kind = TokenKind::Number;
        token.text = shifted;
    }
}

fn reduce_calc_expressions(tokens: &mut [Token]) {
    for token in tokens.iter_mut() {
        if let Some(children) = &mut token.children {
            reduce_calc_expressions(children);
        }
        if token.kind != TokenKind::Function || !token.text.eq_ignore_ascii_case("calc") {
            continue;
        }
        let Some(children) = &token.children else {
            continue;
        };
        let mut replacement = evaluate_calc_numeric(children).and_then(|value| {
            numeric_token_from_parts(token, value.number, value.kind, &value.unit)
        });
        if replacement.is_none()
            && !contains_var_function(children)
            && let Some(children) = &mut token.children
        {
            simplify_mixed_calc(children);
            replacement = evaluate_calc_numeric(children).and_then(|value| {
                numeric_token_from_parts(token, value.number, value.kind, &value.unit)
            });
        }
        if replacement.is_none()
            && let Some(children) = &mut token.children
            && !contains_var_function(children)
            && !has_failed_numeric_calc_product(children)
        {
            clear_calc_product_whitespace(children);
        }
        if let Some(mut replacement) = replacement {
            replacement.loc = token.loc;
            replacement.whitespace = token.whitespace;
            *token = replacement;
        }
    }
}

fn has_failed_numeric_calc_product(tokens: &[Token]) -> bool {
    for window in tokens.windows(3) {
        let operator = window[1].kind;
        if matches!(operator, TokenKind::DelimAsterisk | TokenKind::DelimSlash) {
            let left = numeric_token(&window[0]);
            let right = numeric_token(&window[2]);
            if operator == TokenKind::DelimSlash
                && right.is_some_and(|right| right.kind == TokenKind::Number && right.number == 0.0)
                && left.is_some()
            {
                return true;
            }
            if let Some(value) = evaluate_calc_numeric(window)
                && numeric_token_from_parts(&window[0], value.number, value.kind, &value.unit)
                    .is_none()
            {
                return true;
            }
        }
    }
    tokens.iter().any(|token| {
        token
            .children
            .as_deref()
            .is_some_and(has_failed_numeric_calc_product)
    })
}

fn clear_calc_product_whitespace(tokens: &mut [Token]) {
    for index in 0..tokens.len() {
        if matches!(
            tokens[index].kind,
            TokenKind::DelimAsterisk | TokenKind::DelimSlash
        ) {
            tokens[index].whitespace = WhitespaceFlags::default();
            if index > 0 {
                tokens[index - 1].whitespace.remove(WhitespaceFlags::AFTER);
            }
            if index + 1 < tokens.len() {
                tokens[index + 1].whitespace.remove(WhitespaceFlags::BEFORE);
            }
        }
        if let Some(children) = &mut tokens[index].children {
            clear_calc_product_whitespace(children);
        }
    }
}

fn contains_var_function(tokens: &[Token]) -> bool {
    tokens.iter().any(|token| {
        token.kind == TokenKind::Function && token.text.eq_ignore_ascii_case("var")
            || token.children.as_deref().is_some_and(contains_var_function)
    })
}

fn simplify_mixed_calc(tokens: &mut Vec<Token>) -> bool {
    let original = tokens.clone();
    for token in tokens.iter_mut() {
        let is_group = token.kind == TokenKind::OpenParen
            || token.kind == TokenKind::Function && token.text.eq_ignore_ascii_case("calc");
        if is_group
            && let Some(children) = &mut token.children
            && !contains_var_function(children)
        {
            simplify_mixed_calc(children);
        }
    }
    unwrap_single_calc_groups(tokens);
    flatten_calc_sums(tokens);
    flatten_calc_products(tokens);
    reduce_numeric_calc_products(tokens);
    rewrite_shorter_calc_reciprocals(tokens);
    combine_calc_sum_terms(tokens);
    trim_token_boundary_whitespace(tokens);
    *tokens != original
}

fn unwrap_single_calc_groups(tokens: &mut [Token]) {
    for token in tokens.iter_mut() {
        let is_group = token.kind == TokenKind::OpenParen
            || token.kind == TokenKind::Function && token.text.eq_ignore_ascii_case("calc");
        if !is_group
            || token
                .children
                .as_ref()
                .is_none_or(|children| children.len() != 1)
        {
            continue;
        }
        let outer_whitespace = token.whitespace;
        let mut replacement = token.children.take().unwrap_or_default().remove(0);
        replacement.whitespace |= outer_whitespace;
        *token = replacement;
    }
}

fn flatten_calc_sums(tokens: &mut Vec<Token>) {
    let mut index = 0;
    while index < tokens.len() {
        let can_flatten_after = index == 0 || tokens[index - 1].kind == TokenKind::DelimPlus;
        let can_flatten_before = index + 1 == tokens.len()
            || matches!(
                tokens[index + 1].kind,
                TokenKind::DelimPlus | TokenKind::DelimMinus
            );
        let is_group = tokens[index].kind == TokenKind::OpenParen
            || tokens[index].kind == TokenKind::Function
                && tokens[index].text.eq_ignore_ascii_case("calc");
        if can_flatten_after
            && can_flatten_before
            && is_group
            && tokens[index].children.as_ref().is_some_and(|children| {
                children
                    .iter()
                    .any(|token| matches!(token.kind, TokenKind::DelimPlus | TokenKind::DelimMinus))
            })
        {
            let outer_whitespace = tokens[index].whitespace;
            let mut children = tokens[index].children.take().unwrap_or_default();
            if let Some(first) = children.first_mut()
                && outer_whitespace.contains(WhitespaceFlags::BEFORE)
            {
                first.whitespace |= WhitespaceFlags::BEFORE;
            }
            if let Some(last) = children.last_mut()
                && outer_whitespace.contains(WhitespaceFlags::AFTER)
            {
                last.whitespace |= WhitespaceFlags::AFTER;
            }
            tokens.splice(index..=index, children);
            continue;
        }
        index += 1;
    }
}

fn flatten_calc_products(tokens: &mut Vec<Token>) {
    let mut index = 0;
    while index < tokens.len() {
        let can_flatten_after = index == 0 || tokens[index - 1].kind == TokenKind::DelimAsterisk;
        let can_flatten_before = index + 1 == tokens.len()
            || matches!(
                tokens[index + 1].kind,
                TokenKind::DelimAsterisk | TokenKind::DelimSlash
            );
        let is_group = tokens[index].kind == TokenKind::OpenParen
            || tokens[index].kind == TokenKind::Function
                && tokens[index].text.eq_ignore_ascii_case("calc");
        if can_flatten_after
            && can_flatten_before
            && is_group
            && tokens[index].children.as_ref().is_some_and(|children| {
                children.iter().any(|token| {
                    matches!(token.kind, TokenKind::DelimAsterisk | TokenKind::DelimSlash)
                }) && !children
                    .iter()
                    .any(|token| matches!(token.kind, TokenKind::DelimPlus | TokenKind::DelimMinus))
            })
        {
            let outer_whitespace = tokens[index].whitespace;
            let mut children = tokens[index].children.take().unwrap_or_default();
            if let Some(first) = children.first_mut()
                && outer_whitespace.contains(WhitespaceFlags::BEFORE)
            {
                first.whitespace |= WhitespaceFlags::BEFORE;
            }
            if let Some(last) = children.last_mut()
                && outer_whitespace.contains(WhitespaceFlags::AFTER)
            {
                last.whitespace |= WhitespaceFlags::AFTER;
            }
            tokens.splice(index..=index, children);
            continue;
        }
        index += 1;
    }
}

fn reduce_numeric_calc_products(tokens: &mut Vec<Token>) {
    let mut index = 0;
    while index + 2 < tokens.len() {
        if matches!(
            tokens[index + 1].kind,
            TokenKind::DelimAsterisk | TokenKind::DelimSlash
        ) && let Some(value) = evaluate_calc_numeric(&tokens[index..index + 3])
            && let Some(mut replacement) =
                numeric_token_from_parts(&tokens[index], value.number, value.kind, &value.unit)
        {
            replacement.whitespace = boundary_whitespace(&tokens[index], &tokens[index + 2]);
            tokens.splice(index..index + 3, [replacement]);
            index = index.saturating_sub(2);
            continue;
        }
        index += 1;
    }
}

fn rewrite_shorter_calc_reciprocals(tokens: &mut [Token]) {
    for index in 0..tokens.len().saturating_sub(2) {
        let operator_kind = tokens[index + 1].kind;
        if !matches!(
            operator_kind,
            TokenKind::DelimAsterisk | TokenKind::DelimSlash
        ) || numeric_token(&tokens[index]).is_some()
        {
            continue;
        }
        let Some(right) = numeric_token(&tokens[index + 2]) else {
            continue;
        };
        if right.kind != TokenKind::Number || right.number == 0.0 {
            continue;
        }
        let Some(original) = float_to_string_for_calc(right.number) else {
            continue;
        };
        let Some(reciprocal) = float_to_string_for_calc(1.0 / right.number) else {
            continue;
        };
        if reciprocal.len() >= original.len() {
            continue;
        }
        let Some(replacement) = numeric_token_from_parts(
            &tokens[index + 2],
            1.0 / right.number,
            TokenKind::Number,
            "",
        ) else {
            continue;
        };
        tokens[index + 1].kind = if operator_kind == TokenKind::DelimAsterisk {
            TokenKind::DelimSlash
        } else {
            TokenKind::DelimAsterisk
        };
        tokens[index + 1].text = if operator_kind == TokenKind::DelimAsterisk {
            "/".into()
        } else {
            "*".into()
        };
        tokens[index + 2] = replacement;
    }
}

fn combine_calc_sum_terms(tokens: &mut Vec<Token>) {
    let mut processed_units: Vec<(TokenKind, String)> = Vec::new();
    loop {
        let candidates: Vec<_> = (0..tokens.len())
            .filter_map(|index| {
                let numeric = numeric_token(&tokens[index])?;
                let has_left_boundary = index == 0
                    || matches!(
                        tokens[index - 1].kind,
                        TokenKind::DelimPlus | TokenKind::DelimMinus
                    );
                let has_right_boundary = index + 1 == tokens.len()
                    || matches!(
                        tokens[index + 1].kind,
                        TokenKind::DelimPlus | TokenKind::DelimMinus
                    );
                (has_left_boundary && has_right_boundary).then_some((
                    index,
                    numeric.kind,
                    numeric.unit.to_ascii_lowercase(),
                    numeric.number,
                ))
            })
            .collect();
        let Some((_, kind, unit, _)) = candidates.iter().find(|(_, kind, unit, _)| {
            !processed_units
                .iter()
                .any(|(seen_kind, seen_unit)| seen_kind == kind && seen_unit == unit)
        }) else {
            break;
        };
        let kind = *kind;
        let unit = unit.clone();
        processed_units.push((kind, unit.clone()));
        let matching: Vec<_> = candidates
            .into_iter()
            .filter(|(_, candidate_kind, candidate_unit, _)| {
                *candidate_kind == kind && *candidate_unit == unit
            })
            .collect();
        let Some(&(base_index, _, _, _)) = matching.first() else {
            continue;
        };
        let total = matching.iter().fold(0.0, |sum, (index, _, _, number)| {
            let sign = if *index > 0 && tokens[*index - 1].kind == TokenKind::DelimMinus {
                -1.0
            } else {
                1.0
            };
            sum + sign * number
        });
        let output_number = if base_index == 0 { total } else { total.abs() };
        let Some(mut replacement) = numeric_token_from_parts(
            &tokens[base_index],
            output_number,
            kind,
            numeric_token(&tokens[base_index]).map_or("", |numeric| numeric.unit),
        ) else {
            continue;
        };
        replacement.whitespace = tokens[base_index].whitespace;
        tokens[base_index] = replacement;
        if base_index > 0 {
            let operator = &mut tokens[base_index - 1];
            operator.kind = if total < 0.0 {
                TokenKind::DelimMinus
            } else {
                TokenKind::DelimPlus
            };
            operator.text = if total < 0.0 { "-".into() } else { "+".into() };
        }
        for &(index, _, _, _) in matching.iter().skip(1).rev() {
            tokens.drain(index - 1..=index);
        }
    }
}

fn boundary_whitespace(left: &Token, right: &Token) -> WhitespaceFlags {
    let mut whitespace = WhitespaceFlags::default();
    if left.whitespace.contains(WhitespaceFlags::BEFORE) {
        whitespace |= WhitespaceFlags::BEFORE;
    }
    if right.whitespace.contains(WhitespaceFlags::AFTER) {
        whitespace |= WhitespaceFlags::AFTER;
    }
    whitespace
}

#[derive(Clone)]
struct CalcNumeric {
    number: f64,
    kind: TokenKind,
    unit: String,
}

fn evaluate_calc_numeric(tokens: &[Token]) -> Option<CalcNumeric> {
    let mut index = 0;
    let result = parse_calc_sum(tokens, &mut index)?;
    (index == tokens.len()).then_some(result)
}

fn parse_calc_sum(tokens: &[Token], index: &mut usize) -> Option<CalcNumeric> {
    let mut left = parse_calc_product(tokens, index)?;
    while let Some(operator) = tokens.get(*index) {
        if !matches!(operator.kind, TokenKind::DelimPlus | TokenKind::DelimMinus) {
            break;
        }
        if !operator.whitespace.contains(WhitespaceFlags::BEFORE)
            || !operator.whitespace.contains(WhitespaceFlags::AFTER)
        {
            return None;
        }
        *index += 1;
        let right = parse_calc_product(tokens, index)?;
        if left.kind != right.kind || !left.unit.eq_ignore_ascii_case(&right.unit) {
            return None;
        }
        if operator.kind == TokenKind::DelimPlus {
            left.number += right.number;
        } else {
            left.number -= right.number;
        }
    }
    Some(left)
}

fn parse_calc_product(tokens: &[Token], index: &mut usize) -> Option<CalcNumeric> {
    let mut left = parse_calc_value(tokens, index)?;
    while let Some(operator) = tokens.get(*index) {
        if !matches!(
            operator.kind,
            TokenKind::DelimAsterisk | TokenKind::DelimSlash
        ) {
            break;
        }
        *index += 1;
        let right = parse_calc_value(tokens, index)?;
        match operator.kind {
            TokenKind::DelimAsterisk if left.kind == TokenKind::Number => {
                left.number *= right.number;
                left.kind = right.kind;
                left.unit = right.unit;
            }
            TokenKind::DelimAsterisk if right.kind == TokenKind::Number => {
                left.number *= right.number;
            }
            TokenKind::DelimSlash if right.kind == TokenKind::Number && right.number != 0.0 => {
                left.number /= right.number;
            }
            _ => return None,
        }
    }
    Some(left)
}

fn parse_calc_value(tokens: &[Token], index: &mut usize) -> Option<CalcNumeric> {
    let token = tokens.get(*index)?;
    *index += 1;
    if matches!(token.kind, TokenKind::OpenParen)
        || token.kind == TokenKind::Function && token.text.eq_ignore_ascii_case("calc")
    {
        return evaluate_calc_numeric(token.children.as_deref()?);
    }
    let numeric = numeric_token(token)?;
    Some(CalcNumeric {
        number: numeric.number,
        kind: numeric.kind,
        unit: numeric.unit.into(),
    })
}

#[derive(Clone, Copy)]
struct NumericToken<'a> {
    number: f64,
    kind: TokenKind,
    unit: &'a str,
}

fn numeric_token(token: &Token) -> Option<NumericToken<'_>> {
    let (number, unit) = match token.kind {
        TokenKind::Number => (token.text.parse::<f64>().ok()?, ""),
        TokenKind::Percentage => (token.percentage_value().parse::<f64>().ok()?, "%"),
        TokenKind::Dimension => (
            token.dimension_value().parse::<f64>().ok()?,
            token.dimension_unit(),
        ),
        _ => return None,
    };
    number.is_finite().then_some(NumericToken {
        number,
        kind: token.kind,
        unit,
    })
}

fn numeric_token_from_parts(
    original: &Token,
    number: f64,
    kind: TokenKind,
    unit: &str,
) -> Option<Token> {
    let number = float_to_string_for_calc(number)?;
    let mut result = original.clone();
    result.kind = kind;
    result.children = None;
    result.payload_index = 0;
    result.unit_offset = 0;
    result.text = match kind {
        TokenKind::Number => number,
        TokenKind::Percentage => format!("{number}%"),
        TokenKind::Dimension => {
            result.unit_offset = u16::try_from(number.len()).ok()?;
            format!("{number}{unit}")
        }
        _ => return None,
    };
    Some(result)
}

fn float_to_string_for_calc(number: f64) -> Option<String> {
    if !number.is_finite() {
        return None;
    }
    let mut text = format!("{number:.5}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    if let Some(fraction) = text.strip_prefix("0.") {
        text = format!(".{fraction}");
    } else if let Some(fraction) = text.strip_prefix("-0.") {
        text = format!("-.{fraction}");
    }
    (text.parse::<f64>().ok()?.partial_cmp(&number) == Some(std::cmp::Ordering::Equal))
        .then_some(text)
}

fn parse_layer_names(tokens: &[Token]) -> Vec<Vec<String>> {
    let mut names = Vec::new();
    let mut parts = Vec::new();
    for token in tokens {
        match token.kind {
            TokenKind::Ident => parts.push(token.text.clone()),
            TokenKind::Comma if !parts.is_empty() => {
                names.push(std::mem::take(&mut parts));
            }
            _ => {}
        }
    }
    if !parts.is_empty() {
        names.push(parts);
    }
    names
}

fn keyframe_selectors(tokens: &[Token]) -> Vec<String> {
    let mut selectors = Vec::new();
    let mut current = String::new();
    for token in tokens {
        if token.kind == TokenKind::Comma {
            if !current.is_empty() {
                selectors.push(std::mem::take(&mut current));
            }
        } else if token.kind != TokenKind::Whitespace {
            current.push_str(&token.text);
        }
    }
    if !current.is_empty() {
        selectors.push(current);
    }
    selectors
}

fn is_known_block_at_rule(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "container"
            | "document"
            | "font-face"
            | "font-feature-values"
            | "font-palette-values"
            | "page"
            | "position-try"
            | "property"
            | "starting-style"
            | "supports"
            | "view-transition"
    )
}

fn known_at_rule_preserves_legal_comments(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "container" | "document" | "starting-style" | "supports"
    )
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::{Options, SymbolMode, parse};
    use crate::internal::{
        ast::{ImportKind, SymbolKind, SymbolMap},
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

    #[test]
    fn parses_structured_media_layer_and_supports_rules() {
        assert_eq!(
            parse_and_print(
                "@media screen,(width > 1px) {\
                   @supports (display: grid) { a { display: grid } }\
                 }\
                 @layer framework { b { color: blue } }",
                false
            ),
            "@media screen, (width > 1px) {\n\
             \x20\x20@supports (display: grid) {\n\
             \x20\x20\x20\x20a {\n\
             \x20\x20\x20\x20\x20\x20display: grid;\n\
             \x20\x20\x20\x20}\n\
             \x20\x20}\n\
             }\n\
             @layer framework {\n\
             \x20\x20b {\n\
             \x20\x20\x20\x20color: blue;\n\
             \x20\x20}\n\
             }\n"
        );
        assert_eq!(
            parse_and_print(
                "@media screen { @supports (display: grid) { a { display: grid } } }",
                true
            ),
            "@media screen{@supports (display:grid){a{display:grid}}}"
        );
    }

    #[test]
    fn parses_keyframes_with_css_symbols_and_declaration_blocks() {
        assert_eq!(
            parse_and_print(
                "@keyframes fade {\
                   from, 50% { opacity: 0 }\
                   to { opacity: 1 }\
                 }",
                false
            ),
            "@keyframes fade {\n\
             \x20\x20from, 50% {\n\
             \x20\x20\x20\x20opacity: 0;\n\
             \x20\x20}\n\
             \x20\x20to {\n\
             \x20\x20\x20\x20opacity: 1;\n\
             \x20\x20}\n\
             }\n"
        );
        assert_eq!(
            parse_and_print(
                "@keyframes fade { from { opacity: 0 } to { opacity: 1 } }",
                true
            ),
            "@keyframes fade{from{opacity:0}to{opacity:1}}"
        );
    }

    #[test]
    fn parses_semantic_selectors_and_reuses_css_symbols() {
        let contents = "a + b c > d ~ e, .item#root:hover { color: red }\
                        section { & .item { color: blue } }";
        assert_eq!(
            parse_and_print(contents, true),
            "a+b c>d~e,.item#root:hover{color:red}section{& .item{color:blue}}"
        );

        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let tree = parse(log.clone(), source(contents), Options::default());
        assert!(log.done().is_empty());
        assert_eq!(
            tree.symbols
                .iter()
                .filter(|symbol| symbol.original_name == "item")
                .count(),
            1
        );
    }

    #[test]
    fn records_local_css_composes_and_removes_the_pragma() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let tree = parse(
            log.clone(),
            source(
                ".foo { composes: bar from \"./other.css\"; color: red }\
                 .bar { color: blue }",
            ),
            Options {
                symbol_mode: SymbolMode::Local,
                ..Options::default()
            },
        );
        assert!(log.done().is_empty());
        assert_eq!(
            tree.symbols
                .iter()
                .filter(|symbol| symbol.kind == SymbolKind::LocalCss)
                .count(),
            2
        );
        assert_eq!(tree.composes.len(), 1);
        let composes = tree.composes.values().next().expect("composes entry");
        assert!(composes.names.is_empty());
        assert_eq!(composes.imported_names.len(), 1);
        assert_eq!(composes.imported_names[0].alias, "bar");
        assert_eq!(tree.import_records.len(), 1);
        assert_eq!(tree.import_records[0].kind, ImportKind::ComposesFrom);
        assert_eq!(tree.import_records[0].path.text, "./other.css");
        let selector = &tree.rules[0];
        let crate::internal::css_ast::RuleData::Selector(selector) = &selector.data else {
            panic!("expected selector rule");
        };
        assert_eq!(selector.rules.len(), 1);
    }
}
