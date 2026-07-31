//! Port of upstream `internal/css_parser`.

mod media;

use std::collections::HashMap;

use crate::internal::{
    ast::{CharFreq, ImportKind, ImportRecord, LocRef, Ref, Symbol, SymbolKind, SymbolMap},
    compat::CssFeature,
    css_ast::{
        Ast, AtCharsetRule, AtImportRule, AtKeyframesRule, AtLayerRule, AtMediaRule,
        BadDeclarationRule, ClassSelector, Combinator, CommentRule, ComplexSelector, Composes,
        CompoundSelector, CrossFileEqualityCheck, Declaration, DeclarationRule, HashSelector,
        ImportConditions, ImportedComposesName, KNOWN_DECLARATIONS, KeyframeBlock, KnownAtRule,
        MediaQuery, NameToken, NamespacedName, PseudoClassKind, PseudoClassSelector, QualifiedRule,
        Rule, RuleData, SelectorRule, SubclassData, SubclassSelector, Token, UnknownAtRule,
        WhitespaceFlags, media_queries_equal, rules_equal, tokens_are_comma_separated,
    },
    css_lexer::{self, TokenKind, is_name_continue, would_start_identifier_without_escapes},
    logger::{Loc, Log, Path, Range, Source},
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Options {
    pub minify_syntax: bool,
    pub minify_whitespace: bool,
    pub minify_identifiers: bool,
    pub symbol_mode: SymbolMode,
    pub unsupported_css_features: CssFeature,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SymbolMode {
    #[default]
    Disabled,
    Global,
    Local,
}

#[derive(Clone, Debug)]
struct DeadRuleEntry {
    data: RuleData,
    call_counter: usize,
}

#[derive(Clone, Debug, Default)]
struct DeadRuleHashEntry {
    rules: Vec<DeadRuleEntry>,
}

#[derive(Clone, Debug)]
struct DeadRuleCallEntry {
    import_records: Vec<ImportRecord>,
    source_index: u32,
}

#[derive(Clone, Debug, Default)]
pub struct DeadRuleRemover {
    entries: HashMap<u32, DeadRuleHashEntry>,
    calls: Vec<DeadRuleCallEntry>,
    symbols: SymbolMap,
}

#[must_use]
pub fn make_dead_rule_mangler(symbols: SymbolMap) -> DeadRuleRemover {
    DeadRuleRemover {
        symbols,
        ..DeadRuleRemover::default()
    }
}

impl DeadRuleRemover {
    /// Remove rules made redundant by this or any previously processed CSS
    /// file, keeping the last duplicate in overall output order.
    #[must_use]
    pub fn remove_dead_rules_in_place(
        &mut self,
        source_index: u32,
        rules: Vec<Rule>,
        import_records: &[ImportRecord],
    ) -> Vec<Rule> {
        let call_counter = self.calls.len();
        self.calls.push(DeadRuleCallEntry {
            import_records: import_records.to_vec(),
            source_index,
        });

        let mut kept = Vec::with_capacity(rules.len());
        'next_rule: for rule in rules.into_iter().rev() {
            if let RuleData::Selector(selector) = &rule.data
                && all_selectors_are_dead(&selector.selectors)
            {
                continue;
            }

            if let Some(hash) = rule.data.hash() {
                let entry = self.entries.entry(hash).or_default();
                for current in &entry.rules {
                    let equal = if current.call_counter == call_counter {
                        rule.data.equal(&current.data, None)
                    } else {
                        let previous_call = &self.calls[current.call_counter];
                        rule.data.equal(
                            &current.data,
                            Some(&CrossFileEqualityCheck {
                                import_records_a: import_records,
                                import_records_b: &previous_call.import_records,
                                symbols: Some(&self.symbols),
                                source_index_a: source_index,
                                source_index_b: previous_call.source_index,
                            }),
                        )
                    };
                    if equal {
                        continue 'next_rule;
                    }
                }
                entry.rules.push(DeadRuleEntry {
                    data: rule.data.clone(),
                    call_counter,
                });
            }
            kept.push(rule);
        }
        kept.reverse();
        kept
    }
}

fn contains_dead_selectors(selectors: &[CompoundSelector]) -> bool {
    selectors.iter().any(|selector| {
        selector.subclass_selectors.iter().any(|subclass| {
            matches!(
                &subclass.data,
                SubclassData::PseudoWithSelectorList(pseudo)
                    if pseudo.selectors.is_empty()
                        && matches!(pseudo.kind, PseudoClassKind::Is | PseudoClassKind::Where)
            )
        })
    })
}

fn all_selectors_are_dead(selectors: &[ComplexSelector]) -> bool {
    selectors
        .iter()
        .all(|selector| contains_dead_selectors(&selector.selectors))
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
        unsupported_css_features: options.unsupported_css_features,
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
    unsupported_css_features: CssFeature,
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
        let lower_inset = self
            .unsupported_css_features
            .contains(CssFeature::INSET_PROPERTY);
        if lower_inset {
            lower_inset_declarations(&mut rules, self.minify_whitespace);
        }
        if self.minify_syntax {
            mangle_border_radius_declarations(&mut rules, self.minify_whitespace);
            mangle_box_declarations(&mut rules, !lower_inset);
            mangle_empty_and_nested_rules(&mut rules);
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

    fn convert_at_rule_prelude(&mut self, name: &str, start: usize, end: usize) -> Vec<Token> {
        let preserve_whitespace = name.eq_ignore_ascii_case("container")
            || name.eq_ignore_ascii_case("supports")
            || name.eq_ignore_ascii_case("media");
        let mut prelude = if preserve_whitespace {
            self.convert_tokens_preserving_whitespace(start, end)
        } else {
            self.convert_tokens(start, end)
        };
        trim_token_boundary_whitespace(&mut prelude);
        if name.eq_ignore_ascii_case("media") && self.minify_whitespace {
            minify_group_commas(&mut prelude);
        }
        prelude
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
        let mut prelude = self.convert_at_rule_prelude(&name, prelude_start, end);
        self.index = end;
        if self.current_kind() == TokenKind::OpenBrace {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "keyframes" | "-webkit-keyframes"
            ) {
                return self.parse_keyframes(loc, name, &prelude);
            }
            if name.eq_ignore_ascii_case("media") {
                return self.parse_media_at_rule(loc, prelude, preserve_legal_comments);
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
                self.process_at_rule_symbols(&name, &mut prelude);
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

    fn parse_media_at_rule(
        &mut self,
        loc: Loc,
        mut prelude: Vec<Token>,
        preserve_legal_comments: bool,
    ) -> Rule {
        if self.minify_syntax {
            reduce_calc_expressions(&mut prelude, false);
        }
        let queries = media::parse_media_query_list(
            prelude,
            loc,
            self.unsupported_css_features,
            self.minify_syntax,
        );
        self.index += 1;
        let mut rules = self.parse_rule_list(true, preserve_legal_comments);
        if self.minify_syntax {
            unwrap_duplicate_media_rules(&mut rules, &queries);
        }
        Rule {
            loc,
            data: RuleData::AtMedia(AtMediaRule {
                queries,
                rules,
                ..AtMediaRule::default()
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

    fn process_at_rule_symbols(&mut self, name: &str, prelude: &mut [Token]) {
        let token = if name.eq_ignore_ascii_case("counter-style") {
            let [token] = prelude else {
                return;
            };
            token
        } else if name.eq_ignore_ascii_case("container") {
            let Some(token) = prelude.first_mut() else {
                return;
            };
            if token.text.eq_ignore_ascii_case("not") {
                return;
            }
            token
        } else {
            return;
        };
        if token.kind == TokenKind::Ident {
            let reference = self.new_css_symbol(&token.text, token.loc);
            token.kind = TokenKind::Symbol;
            token.payload_index = reference.inner_index;
        }
    }

    fn skip_whitespace_at(&self, mut index: usize) -> usize {
        while self.kind_at(index) == TokenKind::Whitespace {
            index += 1;
        }
        index
    }

    fn convert_import_condition_component(&mut self, start: usize, end: usize) -> Vec<Token> {
        let mut tokens = self.convert_tokens_preserving_whitespace(start, end);
        trim_token_boundary_whitespace(&mut tokens);
        if self.minify_whitespace {
            minify_group_commas(&mut tokens);
        }
        tokens
    }

    fn parse_import_conditions(
        &mut self,
        start: usize,
        end: usize,
        loc: Loc,
    ) -> Option<ImportConditions> {
        let mut layers = Vec::new();
        let mut supports = Vec::new();
        let mut index = self.skip_whitespace_at(start);

        let layer_token = self.tokens.get(index).copied().filter(|token| {
            matches!(token.kind, TokenKind::Ident | TokenKind::Function)
                && self.decoded(*token).eq_ignore_ascii_case("layer")
        });
        if let Some(token) = layer_token {
            let component_end = if token.kind == TokenKind::Function {
                self.scan_balanced_block(index)
            } else {
                index + 1
            };
            layers = self.convert_import_condition_component(index, component_end);
            index = self.skip_whitespace_at(component_end);
        }

        let has_supports = self.tokens.get(index).copied().is_some_and(|token| {
            token.kind == TokenKind::Function
                && self.decoded(token).eq_ignore_ascii_case("supports")
        });
        if has_supports {
            let component_end = self.scan_balanced_block(index);
            supports = self.convert_import_condition_component(index, component_end);
            index = self.skip_whitespace_at(component_end);
        }

        let queries = if index < end {
            let mut tokens = self.convert_import_condition_component(index, end);
            if self.minify_syntax {
                reduce_calc_expressions(&mut tokens, false);
            }
            media::parse_media_query_list(
                tokens,
                loc,
                self.unsupported_css_features,
                self.minify_syntax,
            )
        } else {
            Vec::new()
        };
        (!layers.is_empty() || !supports.is_empty() || !queries.is_empty()).then_some(
            ImportConditions {
                queries,
                layers,
                supports,
            },
        )
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
        let conditions_end = self.index;
        if self.current_kind() == TokenKind::Semicolon {
            self.index += 1;
        }
        let import_conditions = self.parse_import_conditions(conditions_start, conditions_end, loc);
        Rule {
            loc,
            data: RuleData::AtImport(AtImportRule {
                import_conditions,
                import_record_index,
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
        let mut value = if key_text.starts_with("--") {
            self.convert_custom_property_tokens(value_start, value_end)
        } else {
            self.convert_tokens(value_start, value_end)
        };
        if self.minify_syntax {
            reduce_calc_expressions(&mut value, key_text.starts_with("--"));
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
            "list-style" => self.process_list_style_shorthand(&mut value),
            "list-style-type" if value.len() == 1 => {
                self.process_list_style_type(&mut value[0]);
            }
            "container" => self.process_container_shorthand(&mut value),
            "container-name" => self.process_container_names(&mut value),
            _ => {}
        }
        process_declaration(
            &key_text,
            &mut value,
            self.minify_syntax,
            self.minify_whitespace,
            self.unsupported_css_features,
        );
        let important = take_important(&mut value);
        if !important
            && !key_text.starts_with("--")
            && let Some(last) = value.last_mut()
        {
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
                key: KNOWN_DECLARATIONS
                    .get(key_text.to_ascii_lowercase().as_str())
                    .copied()
                    .unwrap_or_default(),
                key_text,
                value,
                key_range: key_token.range,
                important,
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

    fn process_list_style_shorthand(&mut self, tokens: &mut [Token]) {
        if !(1..=3).contains(&tokens.len()) {
            return;
        }
        let mut found_image = false;
        let mut found_position = false;
        let mut type_index = None;
        let mut none_count = 0;
        for (index, token) in tokens.iter().enumerate() {
            match token.kind {
                TokenKind::String => return,
                TokenKind::Url if !found_image => {
                    found_image = true;
                    continue;
                }
                TokenKind::Function if !found_image => {
                    if matches!(
                        token.text.to_ascii_lowercase().as_str(),
                        "src"
                            | "linear-gradient"
                            | "repeating-linear-gradient"
                            | "radial-gradient"
                            | "radial-linear-gradient"
                    ) {
                        found_image = true;
                        continue;
                    }
                }
                TokenKind::Ident => {
                    let lower = token.text.to_ascii_lowercase();
                    if lower == "none" {
                        none_count += 1;
                        continue;
                    }
                    if !found_position && matches!(lower.as_str(), "inside" | "outside") {
                        found_position = true;
                        continue;
                    }
                    if type_index.is_none() {
                        if CSS_WIDE_AND_RESERVED_KEYWORDS.contains(&lower.as_str())
                            || is_predefined_counter_style(&lower)
                        {
                            return;
                        }
                        type_index = Some(index);
                        continue;
                    }
                }
                _ => {}
            }
            return;
        }

        let Some(type_index) = type_index else {
            return;
        };
        if !found_image && none_count > 0 {
            none_count -= 1;
        }
        if none_count > 0 {
            return;
        }
        self.mark_list_style_name(tokens, type_index);
    }

    fn process_list_style_type(&mut self, token: &mut Token) {
        if token.kind != TokenKind::Ident {
            return;
        }
        let lower = token.text.to_ascii_lowercase();
        if lower != "none"
            && !CSS_WIDE_AND_RESERVED_KEYWORDS.contains(&lower.as_str())
            && !is_predefined_counter_style(&lower)
        {
            let reference = self.new_css_symbol(&token.text, token.loc);
            token.kind = TokenKind::Symbol;
            token.payload_index = reference.inner_index;
        }
    }

    fn mark_list_style_name(&mut self, tokens: &mut [Token], index: usize) {
        let reference = self.new_css_symbol(&tokens[index].text, tokens[index].loc);
        tokens[index].kind = TokenKind::Symbol;
        tokens[index].payload_index = reference.inner_index;
    }

    fn process_container_shorthand(&mut self, tokens: &mut [Token]) {
        for (index, token) in tokens.iter().enumerate() {
            if token.kind == TokenKind::Ident {
                continue;
            }
            if token.kind == TokenKind::DelimSlash
                && index + 2 == tokens.len()
                && tokens[index + 1].kind == TokenKind::Ident
            {
                break;
            }
            return;
        }
        let name_count = tokens
            .iter()
            .position(|token| token.kind != TokenKind::Ident)
            .unwrap_or(tokens.len());
        for index in 0..name_count {
            self.mark_container_name(tokens, index);
        }
    }

    fn process_container_names(&mut self, tokens: &mut [Token]) {
        if !tokens.iter().all(|token| token.kind == TokenKind::Ident) {
            return;
        }
        for index in 0..tokens.len() {
            self.mark_container_name(tokens, index);
        }
    }

    fn mark_container_name(&mut self, tokens: &mut [Token], index: usize) {
        let lower = tokens[index].text.to_ascii_lowercase();
        if lower == "none" || CSS_WIDE_AND_RESERVED_KEYWORDS.contains(&lower.as_str()) {
            return;
        }
        let reference = self.new_css_symbol(&tokens[index].text, tokens[index].loc);
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
                    || result
                        .last()
                        .is_some_and(|previous| previous.kind == TokenKind::DelimSlash)
                    || converted.kind == TokenKind::DelimSlash
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
        normalize_converted_token_whitespace(&mut result, self.minify_whitespace);
        result
    }

    fn convert_tokens_preserving_whitespace(&mut self, start: usize, end: usize) -> Vec<Token> {
        let minify_whitespace = self.minify_whitespace;
        self.minify_whitespace = false;
        let result = self.convert_tokens(start, end);
        self.minify_whitespace = minify_whitespace;
        result
    }

    fn convert_custom_property_tokens(&mut self, start: usize, end: usize) -> Vec<Token> {
        let has_leading_whitespace = self.kind_at(start) == TokenKind::Whitespace;
        let has_trailing_whitespace =
            end > start && self.kind_at(end.saturating_sub(1)) == TokenKind::Whitespace;
        let mut result = self.convert_tokens_preserving_whitespace(start, end);
        if has_leading_whitespace && let Some(first) = result.first_mut() {
            first.whitespace |= WhitespaceFlags::BEFORE;
        }
        if has_trailing_whitespace && let Some(last) = result.last_mut() {
            last.whitespace |= WhitespaceFlags::AFTER;
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

fn normalize_converted_token_whitespace(tokens: &mut [Token], minify_whitespace: bool) {
    if let Some(first) = tokens.first_mut() {
        first.whitespace.remove(WhitespaceFlags::BEFORE);
    }
    if let Some(last) = tokens.last_mut() {
        last.whitespace.remove(WhitespaceFlags::AFTER);
    }
    for index in 0..tokens.len() {
        if tokens[index].kind != TokenKind::Comma {
            continue;
        }
        tokens[index].whitespace.remove(WhitespaceFlags::BEFORE);
        if index > 0 {
            tokens[index - 1].whitespace.remove(WhitespaceFlags::AFTER);
        }
        if minify_whitespace {
            tokens[index].whitespace.remove(WhitespaceFlags::AFTER);
            if index + 1 < tokens.len() {
                tokens[index + 1].whitespace.remove(WhitespaceFlags::BEFORE);
            }
        } else {
            tokens[index].whitespace |= WhitespaceFlags::AFTER;
            if index + 1 < tokens.len() {
                tokens[index + 1].whitespace |= WhitespaceFlags::BEFORE;
            }
        }
    }
}

fn minify_group_commas(tokens: &mut [Token]) {
    for token in tokens.iter_mut() {
        if let Some(children) = &mut token.children {
            minify_group_commas(children);
        }
    }
    for index in 0..tokens.len() {
        if tokens[index].kind != TokenKind::Comma {
            continue;
        }
        tokens[index]
            .whitespace
            .remove(WhitespaceFlags::BEFORE | WhitespaceFlags::AFTER);
        if index > 0 {
            tokens[index - 1].whitespace.remove(WhitespaceFlags::AFTER);
        }
        if index + 1 < tokens.len() {
            tokens[index + 1].whitespace.remove(WhitespaceFlags::BEFORE);
        }
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

fn mangle_empty_and_nested_rules(rules: &mut Vec<Rule>) {
    let mut index = 0;
    while index < rules.len() {
        let nested_replacement = match &mut rules[index].data {
            RuleData::Selector(selector)
                if selector.selectors.len() == 1
                    && selector.selectors[0].selectors.len() == 1
                    && selector.selectors[0].selectors[0].is_single_ampersand() =>
            {
                Some(std::mem::take(&mut selector.rules))
            }
            RuleData::Selector(selector) => {
                for complex in &mut selector.selectors {
                    if complex.selectors.len() > 1 && complex.selectors[0].is_single_ampersand() {
                        complex.selectors.remove(0);
                    }
                }
                None
            }
            _ => None,
        };
        if let Some(replacement) = nested_replacement {
            rules.splice(index..=index, replacement);
            continue;
        }
        let remove = match &mut rules[index].data {
            RuleData::Selector(selector) => selector.rules.is_empty(),
            RuleData::AtMedia(media) => media.rules.is_empty(),
            RuleData::KnownAt(rule) => {
                rule.rules.is_empty() && known_at_rule_can_be_removed_if_empty(&rule.at_token)
            }
            RuleData::AtLayer(layer) => {
                let inner = match layer.rules.as_slice() {
                    [
                        Rule {
                            data: RuleData::AtLayer(inner),
                            ..
                        },
                    ] => Some(inner.clone()),
                    _ => None,
                };
                if layer.names.len() == 1
                    && let Some(inner) = inner
                    && inner.names.len() == 1
                {
                    let mut name = layer.names[0].clone();
                    name.extend(inner.names[0].iter().cloned());
                    layer.names[0] = name;
                    layer.rules = inner.rules;
                }
                false
            }
            _ => false,
        };
        if remove {
            rules.remove(index);
        } else {
            index += 1;
        }
    }
}

fn unwrap_duplicate_media_rules(rules: &mut Vec<Rule>, parent_queries: &[MediaQuery]) {
    let mut index = 0;
    while index < rules.len() {
        let replacement = match &mut rules[index].data {
            RuleData::AtMedia(media)
                if media_queries_equal(&media.queries, parent_queries, None) =>
            {
                Some(std::mem::take(&mut media.rules))
            }
            _ => None,
        };
        if let Some(replacement) = replacement {
            rules.splice(index..=index, replacement);
        } else {
            index += 1;
        }
    }
}

fn known_at_rule_can_be_removed_if_empty(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "supports"
            | "font-face"
            | "page"
            | "font-palette-values"
            | "container"
            | "bottom-center"
            | "bottom-left-corner"
            | "bottom-left"
            | "bottom-right-corner"
            | "bottom-right"
            | "left-bottom"
            | "left-middle"
            | "left-top"
            | "right-bottom"
            | "right-middle"
            | "right-top"
            | "top-center"
            | "top-left-corner"
            | "top-left"
            | "top-right-corner"
            | "top-right"
            | "scope"
    )
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

fn lower_and_minify_single_color(
    tokens: &mut [Token],
    minify_syntax: bool,
    minify_whitespace: bool,
    unsupported_css_features: CssFeature,
) {
    let [token] = tokens else {
        return;
    };
    if token.kind == TokenKind::Hash
        && unsupported_css_features.contains(CssFeature::HEX_RGBA)
        && matches!(token.text.len(), 4 | 8)
    {
        if let Some((red, green, blue, alpha)) = parse_hex_color(&token.text) {
            generate_color_token(
                token,
                (red, green, blue, alpha),
                minify_syntax,
                minify_whitespace,
                unsupported_css_features,
            );
        }
        return;
    }
    if token.kind == TokenKind::Ident
        && unsupported_css_features.contains(CssFeature::REBECCA_PURPLE)
        && token.text.eq_ignore_ascii_case("rebeccapurple")
    {
        token.kind = TokenKind::Hash;
        token.text = "663399".into();
    }
    let lower_modern_rgb_hsl = token.kind == TokenKind::Function
        && unsupported_css_features.contains(CssFeature::MODERN_RGB_HSL)
        && matches!(
            token.text.to_ascii_lowercase().as_str(),
            "rgb" | "rgba" | "hsl" | "hsla"
        );
    if lower_modern_rgb_hsl {
        lower_modern_rgb_hsl_function(token, minify_whitespace);
        if !minify_syntax {
            return;
        }
    }
    let lower_hwb = token.kind == TokenKind::Function
        && unsupported_css_features.contains(CssFeature::HWB)
        && token.text.eq_ignore_ascii_case("hwb");
    if !(minify_syntax || lower_hwb) {
        return;
    }
    if token.kind == TokenKind::Ident {
        let Some(hex) = named_color_hex(&token.text.to_ascii_lowercase()) else {
            return;
        };
        let Some((red, green, blue, alpha)) = parse_hex_color(hex) else {
            return;
        };
        generate_color_token(
            token,
            (red, green, blue, alpha),
            minify_syntax,
            minify_whitespace,
            unsupported_css_features,
        );
        return;
    }
    if token.kind == TokenKind::Hash {
        if let Some((red, green, blue, alpha)) = parse_hex_color(&token.text) {
            generate_color_token(
                token,
                (red, green, blue, alpha),
                minify_syntax,
                minify_whitespace,
                unsupported_css_features,
            );
        }
        return;
    }
    if token.kind == TokenKind::Function {
        let color = if token.text.eq_ignore_ascii_case("rgb")
            || token.text.eq_ignore_ascii_case("rgba")
        {
            parse_rgb(token)
        } else if token.text.eq_ignore_ascii_case("hsl") || token.text.eq_ignore_ascii_case("hsla")
        {
            parse_hsl(token)
        } else if token.text.eq_ignore_ascii_case("hwb") {
            parse_hwb(token)
        } else {
            None
        };
        if let Some((red, green, blue, alpha)) = color {
            generate_color_token(
                token,
                (red, green, blue, alpha),
                minify_syntax,
                minify_whitespace,
                unsupported_css_features,
            );
        }
    }
}

fn lower_modern_rgb_hsl_function(token: &mut Token, minify_whitespace: bool) {
    let is_hsl = token.text.eq_ignore_ascii_case("hsl") || token.text.eq_ignore_ascii_case("hsla");
    let Some(args) = &mut token.children else {
        return;
    };

    // Upstream normalizes the first HSL argument before validating the rest of
    // the function. Keep this behavior for malformed functions too.
    if is_hsl
        && let Some(first) = args.first_mut()
        && let Some(degrees) = degrees_for_angle(first)
    {
        first.kind = TokenKind::Number;
        first.text = float_to_string_for_color(degrees);
        first.unit_offset = 0;
    }

    let mut remove_alpha = false;
    let mut add_alpha = false;
    match args.len() {
        3 if args.iter().all(|arg| arg.kind.is_numeric()) => {
            remove_alpha = true;
            let mut first = args[0].clone();
            let mut second = args[1].clone();
            let third = args[2].clone();
            first.whitespace = WhitespaceFlags::default();
            second.whitespace = WhitespaceFlags::default();
            let comma = gradient_comma(token.loc, minify_whitespace);
            *args = vec![first, comma.clone(), second, comma, third];
        }

        5 if args[0].kind.is_numeric()
            && args[1].kind == TokenKind::Comma
            && args[2].kind.is_numeric()
            && args[3].kind == TokenKind::Comma
            && args[4].kind.is_numeric() =>
        {
            remove_alpha = true;
        }

        5 if args[0].kind.is_numeric()
            && args[1].kind.is_numeric()
            && args[2].kind.is_numeric()
            && args[3].kind == TokenKind::DelimSlash
            && args[4].kind.is_numeric() =>
        {
            add_alpha = true;
            let mut first = args[0].clone();
            let mut second = args[1].clone();
            let mut third = args[2].clone();
            let alpha = lower_alpha_percentage_to_number(args[4].clone());
            first.whitespace = WhitespaceFlags::default();
            second.whitespace = WhitespaceFlags::default();
            third.whitespace = WhitespaceFlags::default();
            let comma = gradient_comma(token.loc, minify_whitespace);
            *args = vec![
                first,
                comma.clone(),
                second,
                comma.clone(),
                third,
                comma,
                alpha,
            ];
        }

        7 if args[0].kind.is_numeric()
            && args[1].kind == TokenKind::Comma
            && args[2].kind.is_numeric()
            && args[3].kind == TokenKind::Comma
            && args[4].kind.is_numeric()
            && args[5].kind == TokenKind::Comma
            && args[6].kind.is_numeric() =>
        {
            add_alpha = true;
            args[6] = lower_alpha_percentage_to_number(args[6].clone());
        }

        _ => {}
    }

    if remove_alpha {
        if token.text.eq_ignore_ascii_case("rgba") {
            token.text = "rgb".into();
        } else if token.text.eq_ignore_ascii_case("hsla") {
            token.text = "hsl".into();
        }
    } else if add_alpha {
        if token.text.eq_ignore_ascii_case("rgb") {
            token.text = "rgba".into();
        } else if token.text.eq_ignore_ascii_case("hsl") {
            token.text = "hsla".into();
        }
    }
}

fn lower_alpha_percentage_to_number(mut token: Token) -> Token {
    if token.kind == TokenKind::Percentage
        && let Ok(value) = token.percentage_value().parse::<f64>()
        && value.is_finite()
    {
        token.kind = TokenKind::Number;
        token.text = float_to_string_for_color(value / 100.0);
        token.unit_offset = 0;
    }
    token
}

fn float_to_string_for_color(value: f64) -> String {
    let mut text = format!("{value:.3}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
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
    let (red, green, blue, alpha) = match children {
        [red, green, blue] => (red, green, blue, None),
        [red, comma_one, green, comma_two, blue]
            if comma_one.kind == TokenKind::Comma && comma_two.kind == TokenKind::Comma =>
        {
            (red, green, blue, None)
        }
        [red, green, blue, slash, alpha] if slash.kind == TokenKind::DelimSlash => {
            (red, green, blue, Some(alpha))
        }
        [red, comma_one, green, comma_two, blue, comma_three, alpha]
            if comma_one.kind == TokenKind::Comma
                && comma_two.kind == TokenKind::Comma
                && comma_three.kind == TokenKind::Comma =>
        {
            (red, green, blue, Some(alpha))
        }
        _ => return None,
    };
    Some((
        parse_color_byte(red, 1.0)?,
        parse_color_byte(green, 1.0)?,
        parse_color_byte(blue, 1.0)?,
        parse_alpha_byte(alpha)?,
    ))
}

fn parse_hsl(token: &Token) -> Option<(u8, u8, u8, u8)> {
    let children = token.children.as_deref()?;
    let (hue, saturation, lightness, alpha) = parse_color_function_components(children, true)?;
    let hue = degrees_for_angle(hue)?;
    let saturation = saturation.clamped_fraction_for_percentage()?;
    let lightness = lightness.clamped_fraction_for_percentage()?;
    let (red, green, blue) = hsl_to_rgb(hue, saturation, lightness);
    Some((
        float_to_byte(red),
        float_to_byte(green),
        float_to_byte(blue),
        parse_alpha_byte(alpha)?,
    ))
}

fn parse_hwb(token: &Token) -> Option<(u8, u8, u8, u8)> {
    let children = token.children.as_deref()?;
    let (hue, white, black, alpha) = parse_color_function_components(children, false)?;
    let hue = degrees_for_angle(hue)?;
    let white = white.clamped_fraction_for_percentage()?;
    let black = black.clamped_fraction_for_percentage()?;
    let (red, green, blue) = hwb_to_rgb(hue, white, black);
    Some((
        float_to_byte(red),
        float_to_byte(green),
        float_to_byte(blue),
        parse_alpha_byte(alpha)?,
    ))
}

fn parse_color_function_components(
    children: &[Token],
    allow_legacy_commas: bool,
) -> Option<(&Token, &Token, &Token, Option<&Token>)> {
    Some(match children {
        [first, second, third] => (first, second, third, None),
        [first, comma_one, second, comma_two, third]
            if allow_legacy_commas
                && comma_one.kind == TokenKind::Comma
                && comma_two.kind == TokenKind::Comma =>
        {
            (first, second, third, None)
        }
        [first, second, third, slash, alpha] if slash.kind == TokenKind::DelimSlash => {
            (first, second, third, Some(alpha))
        }
        [
            first,
            comma_one,
            second,
            comma_two,
            third,
            comma_three,
            alpha,
        ] if allow_legacy_commas
            && comma_one.kind == TokenKind::Comma
            && comma_two.kind == TokenKind::Comma
            && comma_three.kind == TokenKind::Comma =>
        {
            (first, second, third, Some(alpha))
        }
        _ => return None,
    })
}

fn parse_color_byte(token: &Token, number_scale: f64) -> Option<u8> {
    let value = match token.kind {
        TokenKind::Number => token.text.parse::<f64>().ok()? * number_scale,
        TokenKind::Percentage => token.percentage_value().parse::<f64>().ok()? * (255.0 / 100.0),
        _ => return None,
    };
    rounded_byte(value)
}

fn parse_alpha_byte(token: Option<&Token>) -> Option<u8> {
    token.map_or(Some(255), |token| parse_color_byte(token, 255.0))
}

fn degrees_for_angle(token: &Token) -> Option<f64> {
    if token.kind == TokenKind::Number {
        return token
            .text
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite());
    }
    if token.kind != TokenKind::Dimension {
        return None;
    }
    let value = token
        .dimension_value()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())?;
    Some(
        value
            * match token.dimension_unit() {
                "deg" => 1.0,
                "grad" => 0.9,
                "rad" => 180.0 / std::f64::consts::PI,
                "turn" => 360.0,
                _ => return None,
            },
    )
}

fn hsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> (f64, f64, f64) {
    let hue = hue / 360.0;
    let t2 = if lightness <= 0.5 {
        (saturation + 1.0) * lightness
    } else {
        lightness + saturation - lightness * saturation
    };
    let t1 = lightness * 2.0 - t2;
    (
        hue_to_rgb(t1, t2, hue + 1.0 / 3.0),
        hue_to_rgb(t1, t2, hue),
        hue_to_rgb(t1, t2, hue - 1.0 / 3.0),
    )
}

fn hue_to_rgb(t1: f64, t2: f64, hue: f64) -> f64 {
    let hue = (hue - hue.floor()) * 6.0;
    if hue < 1.0 {
        t1 + (t2 - t1) * hue
    } else if hue < 3.0 {
        t2
    } else if hue < 4.0 {
        t1 + (t2 - t1) * (4.0 - hue)
    } else {
        t1
    }
}

fn hwb_to_rgb(hue: f64, white: f64, black: f64) -> (f64, f64, f64) {
    if white + black >= 1.0 {
        let gray = white / (white + black);
        return (gray, gray, gray);
    }
    let delta = 1.0 - white - black;
    let (red, green, blue) = hsl_to_rgb(hue, 1.0, 0.5);
    (
        delta * red + white,
        delta * green + white,
        delta * blue + white,
    )
}

fn float_to_byte(value: f64) -> u8 {
    rounded_byte(value * 255.0).expect("computed color channels are finite")
}

fn rounded_byte(value: f64) -> Option<u8> {
    value.round().clamp(0.0, 255.0).to_string().parse().ok()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GradientKind {
    Linear,
    Radial,
    Conic,
}

#[derive(Clone, Debug)]
struct GradientStop {
    color: Token,
    positions: Vec<Token>,
    midpoint: Option<Token>,
}

fn lower_and_minify_gradient(
    token: &mut Token,
    minify_syntax: bool,
    minify_whitespace: bool,
    unsupported_css_features: CssFeature,
) {
    let Some((kind, mut leading, mut stops)) = parse_gradient(token) else {
        return;
    };
    if minify_syntax
        || unsupported_css_features.contains(CssFeature::REBECCA_PURPLE)
        || unsupported_css_features.contains(CssFeature::HEX_RGBA)
        || unsupported_css_features.contains(CssFeature::HWB)
        || unsupported_css_features.contains(CssFeature::MODERN_RGB_HSL)
    {
        for stop in &mut stops {
            lower_and_minify_single_color(
                std::slice::from_mut(&mut stop.color),
                minify_syntax,
                minify_whitespace,
                unsupported_css_features,
            );
        }
    }
    if unsupported_css_features.contains(CssFeature::GRADIENT_DOUBLE_POSITION) {
        split_double_gradient_stops(&mut stops);
    } else if minify_syntax {
        merge_duplicate_gradient_stops(&mut stops);
    }
    if minify_syntax {
        remove_implied_gradient_positions(kind, &mut stops);
    }

    let mut children = Vec::new();
    children.append(&mut leading);
    for mut stop in stops {
        if !children.is_empty() {
            children.push(gradient_comma(token.loc, minify_whitespace));
        }
        if stop.positions.is_empty() && stop.midpoint.is_none() {
            stop.color.whitespace.remove(WhitespaceFlags::AFTER);
        }
        children.push(stop.color);
        children.append(&mut stop.positions);
        if let Some(midpoint) = stop.midpoint {
            children.push(gradient_comma(token.loc, minify_whitespace));
            children.push(midpoint);
        }
    }
    token.children = Some(children);
}

fn parse_gradient(token: &Token) -> Option<(GradientKind, Vec<Token>, Vec<GradientStop>)> {
    if token.kind != TokenKind::Function {
        return None;
    }
    let kind = match token.text.to_ascii_lowercase().as_str() {
        "linear-gradient" | "repeating-linear-gradient" => GradientKind::Linear,
        "radial-gradient" | "repeating-radial-gradient" => GradientKind::Radial,
        "conic-gradient" | "repeating-conic-gradient" => GradientKind::Conic,
        _ => return None,
    };
    let children = token.children.as_deref()?;
    if children
        .iter()
        .any(|child| child.kind == TokenKind::Function && child.text.eq_ignore_ascii_case("var"))
    {
        return None;
    }

    let mut position = 0;
    let mut leading = Vec::new();
    if children
        .first()
        .is_some_and(|child| !token_looks_like_color(child))
    {
        while position < children.len() && children[position].kind != TokenKind::Comma {
            leading.push(children[position].clone());
            position += 1;
        }
        if position == children.len() {
            return None;
        }
        position += 1;
    }

    let mut stops = Vec::new();
    while position < children.len() {
        if !token_looks_like_color(&children[position]) {
            return None;
        }
        let color = children[position].clone();
        position += 1;
        let mut positions = Vec::new();
        while positions.len() < 2
            && position < children.len()
            && (children[position].kind.is_numeric()
                || children[position].kind == TokenKind::Function
                    && children[position].text.eq_ignore_ascii_case("calc"))
        {
            positions.push(children[position].clone());
            position += 1;
        }

        let mut midpoint = None;
        if position < children.len() {
            if children[position].kind != TokenKind::Comma {
                return None;
            }
            position += 1;
            if position == children.len() {
                return None;
            }
            if children[position].kind.is_numeric() {
                midpoint = Some(children[position].clone());
                position += 1;
                if position == children.len() || children[position].kind != TokenKind::Comma {
                    return None;
                }
                position += 1;
            }
        }
        stops.push(GradientStop {
            color,
            positions,
            midpoint,
        });
    }
    Some((kind, leading, stops))
}

fn gradient_comma(loc: Loc, minify_whitespace: bool) -> Token {
    Token {
        kind: TokenKind::Comma,
        text: ",".into(),
        loc,
        whitespace: if minify_whitespace {
            WhitespaceFlags::default()
        } else {
            WhitespaceFlags::AFTER
        },
        ..Token::default()
    }
}

fn merge_duplicate_gradient_stops(stops: &mut Vec<GradientStop>) {
    let mut result = Vec::with_capacity(stops.len());
    let mut position = 0;
    while position < stops.len() {
        let stop = &stops[position];
        if position + 1 < stops.len()
            && stop.positions.len() == 1
            && stop.midpoint.is_none()
            && stops[position + 1].positions.len() == 1
            && stop
                .color
                .equal_ignoring_whitespace(&stops[position + 1].color)
        {
            result.push(GradientStop {
                color: stop.color.clone(),
                positions: vec![
                    stop.positions[0].clone(),
                    stops[position + 1].positions[0].clone(),
                ],
                midpoint: stops[position + 1].midpoint.clone(),
            });
            position += 2;
        } else {
            result.push(stop.clone());
            position += 1;
        }
    }
    *stops = result;
}

fn split_double_gradient_stops(stops: &mut Vec<GradientStop>) {
    let mut result = Vec::with_capacity(stops.len());
    for mut stop in std::mem::take(stops) {
        for position in &mut stop.positions {
            position.whitespace = WhitespaceFlags::BEFORE;
        }
        while stop.positions.len() > 1 {
            result.push(GradientStop {
                color: stop.color.clone(),
                positions: vec![stop.positions.remove(0)],
                midpoint: None,
            });
        }
        result.push(stop);
    }
    *stops = result;
}

fn remove_implied_gradient_positions(kind: GradientKind, stops: &mut [GradientStop]) {
    let positions = stops
        .iter()
        .map(|stop| {
            (stop.positions.len() == 1)
                .then(|| parse_gradient_position(&stop.positions[0], kind))
                .flatten()
        })
        .collect::<Vec<_>>();
    let mut start = 0;
    while start < stops.len() {
        if let Some((start_value, start_unit)) = &positions[start] {
            let mut end = start + 1;
            'run: while end < stops.len() && stops[end - 1].midpoint.is_none() {
                let Some((end_value, end_unit)) = &positions[end] else {
                    break;
                };
                if end_unit != start_unit {
                    break;
                }
                for (index, position) in positions.iter().enumerate().take(end).skip(start + 1) {
                    let Some((value, unit)) = position else {
                        break 'run;
                    };
                    if unit != start_unit {
                        break 'run;
                    }
                    let numerator = u32::try_from(index - start).ok().map(f64::from);
                    let denominator = u32::try_from(end - start).ok().map(f64::from);
                    let (Some(numerator), Some(denominator)) = (numerator, denominator) else {
                        break 'run;
                    };
                    let implied =
                        start_value + (end_value - start_value) * (numerator / denominator);
                    if (value - implied).abs() > 0.01 {
                        break 'run;
                    }
                }
                end += 1;
            }
            if end - start > 1 {
                for stop in stops.iter_mut().take(end - 1).skip(start + 1) {
                    stop.positions.clear();
                }
                start = end - 1;
                continue;
            }
        }
        start += 1;
    }
    if let Some(first) = stops.first_mut()
        && first.positions.len() == 1
        && ((first.positions[0].kind == TokenKind::Percentage
            && first.positions[0].percentage_value() == "0")
            || first.positions[0].kind == TokenKind::Dimension
                && first.positions[0].dimension_value() == "0")
    {
        first.positions.clear();
    }
    if let Some(last) = stops.last_mut()
        && last.positions.len() == 1
        && last.positions[0].kind == TokenKind::Percentage
        && last.positions[0].percentage_value() == "100"
    {
        last.positions.clear();
    }
}

fn parse_gradient_position(token: &Token, kind: GradientKind) -> Option<(f64, String)> {
    if kind == GradientKind::Conic {
        return match token.kind {
            TokenKind::Dimension => Some((degrees_for_angle(token)? * (100.0 / 360.0), "%".into())),
            TokenKind::Percentage => Some((token.percentage_value().parse().ok()?, "%".into())),
            _ => None,
        };
    }
    match token.kind {
        TokenKind::Number if token.text.parse::<f64>().ok()? == 0.0 => Some((0.0, "%".into())),
        TokenKind::Dimension => Some((
            token.dimension_value().parse().ok()?,
            token.dimension_unit().into(),
        )),
        TokenKind::Percentage => Some((token.percentage_value().parse().ok()?, "%".into())),
        _ => None,
    }
}

fn generate_color_token(
    token: &mut Token,
    color: (u8, u8, u8, u8),
    minify_syntax: bool,
    minify_whitespace: bool,
    unsupported_css_features: CssFeature,
) {
    let (red, green, blue, alpha) = color;
    token.children = None;
    if alpha == 255 {
        let hex = format!("{red:02x}{green:02x}{blue:02x}");
        if minify_syntax {
            if let Some(name) = short_color_name(&hex) {
                token.kind = TokenKind::Ident;
                token.text = name.into();
                return;
            }
            token.text = compact_hex(&hex);
        } else {
            token.text = hex;
        }
        token.kind = TokenKind::Hash;
    } else if !unsupported_css_features.contains(CssFeature::HEX_RGBA) {
        token.kind = TokenKind::Hash;
        let hex = format!("{red:02x}{green:02x}{blue:02x}{alpha:02x}");
        token.text = if minify_syntax {
            compact_hex(&hex)
        } else {
            hex
        };
    } else {
        let comma = gradient_comma(token.loc, minify_whitespace);
        token.kind = TokenKind::Function;
        token.text = "rgba".into();
        token.children = Some(vec![
            color_number_token(token.loc, red.to_string()),
            comma.clone(),
            color_number_token(token.loc, green.to_string()),
            comma.clone(),
            color_number_token(token.loc, blue.to_string()),
            comma,
            color_number_token(token.loc, alpha_fraction(alpha).into()),
        ]);
    }
}

fn color_number_token(loc: Loc, text: String) -> Token {
    Token {
        loc,
        kind: TokenKind::Number,
        text,
        ..Token::default()
    }
}

fn alpha_fraction(alpha: u8) -> &'static str {
    let index = usize::from(alpha) * 4;
    ALPHA_FRACTION_TABLE[index..index + 4].trim_end()
}

// Every four characters in this table are the shortest decimal fraction for
// the corresponding 8-bit alpha value. This is copied from pinned upstream.
const ALPHA_FRACTION_TABLE: &str = concat!(
    "0   .004.008.01 .016.02 .024.027.03 .035.04 .043.047.05 .055.06 ",
    ".063.067.07 .075.08 .082.086.09 .094.098.1  .106.11 .114.118.12 ",
    ".125.13 .133.137.14 .145.15 .153.157.16 .165.17 .173.176.18 .184",
    ".19 .192.196.2  .204.208.21 .216.22 .224.227.23 .235.24 .243.247",
    ".25 .255.26 .263.267.27 .275.28 .282.286.29 .294.298.3  .306.31 ",
    ".314.318.32 .325.33 .333.337.34 .345.35 .353.357.36 .365.37 .373",
    ".376.38 .384.39 .392.396.4  .404.408.41 .416.42 .424.427.43 .435",
    ".44 .443.447.45 .455.46 .463.467.47 .475.48 .482.486.49 .494.498",
    ".5  .506.51 .514.518.52 .525.53 .533.537.54 .545.55 .553.557.56 ",
    ".565.57 .573.576.58 .584.59 .592.596.6  .604.608.61 .616.62 .624",
    ".627.63 .635.64 .643.647.65 .655.66 .663.667.67 .675.68 .682.686",
    ".69 .694.698.7  .706.71 .714.718.72 .725.73 .733.737.74 .745.75 ",
    ".753.757.76 .765.77 .773.776.78 .784.79 .792.796.8  .804.808.81 ",
    ".816.82 .824.827.83 .835.84 .843.847.85 .855.86 .863.867.87 .875",
    ".88 .882.886.89 .894.898.9  .906.91 .914.918.92 .925.93 .933.937",
    ".94 .945.95 .953.957.96 .965.97 .973.976.98 .984.99 .992.9961   ",
);

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

fn process_declaration(
    key: &str,
    value: &mut Vec<Token>,
    minify_syntax: bool,
    minify_whitespace: bool,
    unsupported_css_features: CssFeature,
) {
    let key = key.to_ascii_lowercase();
    let lower_color_syntax = unsupported_css_features.contains(CssFeature::REBECCA_PURPLE)
        || unsupported_css_features.contains(CssFeature::HEX_RGBA)
        || unsupported_css_features.contains(CssFeature::HWB)
        || unsupported_css_features.contains(CssFeature::MODERN_RGB_HSL);
    if minify_syntax {
        minify_numeric_tokens(value);
    }
    if minify_syntax || lower_color_syntax {
        if is_single_color_property(&key) {
            lower_and_minify_single_color(
                value,
                minify_syntax,
                minify_whitespace,
                unsupported_css_features,
            );
        } else if key == "background" {
            for token in value.iter_mut() {
                lower_and_minify_single_color(
                    std::slice::from_mut(token),
                    minify_syntax,
                    minify_whitespace,
                    unsupported_css_features,
                );
            }
        }
    }
    if (minify_syntax
        || unsupported_css_features.contains(CssFeature::GRADIENT_DOUBLE_POSITION)
        || lower_color_syntax)
        && matches!(
            key.as_str(),
            "background" | "background-image" | "border-image" | "mask-image"
        )
    {
        for token in value.iter_mut() {
            lower_and_minify_gradient(
                token,
                minify_syntax,
                minify_whitespace,
                unsupported_css_features,
            );
        }
    }
    if key == "box-shadow" && (minify_syntax || lower_color_syntax) {
        lower_and_minify_box_shadows(
            value,
            minify_syntax,
            minify_whitespace,
            unsupported_css_features,
        );
    }
    if !minify_syntax {
        return;
    }
    if matches!(key.as_str(), "margin" | "inset") && is_box_quad(value, true)
        || key == "padding" && is_box_quad(value, false)
    {
        minify_four_side_shorthand(value);
    }
    if key == "border-radius" {
        minify_border_radius(value, minify_whitespace);
    } else if matches!(
        key.as_str(),
        "border-top-left-radius"
            | "border-top-right-radius"
            | "border-bottom-right-radius"
            | "border-bottom-left-radius"
    ) {
        minify_border_radius_corner(value);
    }
    if key == "font-weight"
        && let [token] = value.as_mut_slice()
    {
        minify_font_weight_token(token);
    }
    if key == "font-family"
        && let Some(family) = minify_font_family(value, minify_whitespace)
    {
        *value = family;
    }
    if key == "font" {
        minify_font(value, minify_whitespace);
    }
    if key == "transform" {
        minify_transforms(value);
    }
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
                let value = token.dimension_value().to_owned();
                let unit = token.dimension_unit().to_owned();
                if let Some((value, unit)) = minify_dimension(&value, &unit) {
                    let Ok(unit_offset) = u16::try_from(value.len()) else {
                        continue;
                    };
                    token.unit_offset = unit_offset;
                    token.text = format!("{value}{unit}");
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

fn minify_dimension(value: &str, unit: &str) -> Option<(String, &'static str)> {
    if unit.eq_ignore_ascii_case("ms")
        && let Some(shifted) = shift_decimal_point(value, -3)
        && shifted.len() + 1 < value.len() + 2
    {
        return Some((shifted, "s"));
    }
    if unit.eq_ignore_ascii_case("s")
        && let Some(shifted) = shift_decimal_point(value, 3)
        && shifted.len() + 2 < value.len() + 1
    {
        return Some((shifted, "ms"));
    }
    None
}

fn shift_decimal_point(text: &str, dot_offset: isize) -> Option<String> {
    if text.contains(['e', 'E']) {
        return None;
    }
    let (sign, unsigned) = if text.starts_with(['-', '+']) {
        (&text[..1], &text[1..])
    } else {
        ("", text)
    };
    let mut digits = unsigned.to_owned();
    let mut dot = match digits.find('.') {
        Some(index) => {
            digits.remove(index);
            isize::try_from(index).ok()?
        }
        None => isize::try_from(digits.len()).ok()?,
    };
    dot += dot_offset;

    while dot > 0 && digits.starts_with('0') {
        digits.remove(0);
        dot -= 1;
    }
    while isize::try_from(digits.len()).ok()? > dot && digits.ends_with('0') {
        digits.pop();
    }
    let digits_len = isize::try_from(digits.len()).ok()?;
    if dot >= digits_len {
        let zeros = usize::try_from(dot - digits_len).ok()?;
        return Some(format!("{sign}{digits}{}", "0".repeat(zeros)));
    }
    if dot < 0 {
        let zeros = usize::try_from(-dot).ok()?;
        digits = format!("{}{digits}", "0".repeat(zeros));
        dot = 0;
    }
    let dot = usize::try_from(dot).ok()?;
    Some(format!("{sign}{}.{}", &digits[..dot], &digits[dot..]))
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

fn minify_border_radius(tokens: &mut Vec<Token>, minify_whitespace: bool) {
    let mut slash_index = None;
    for (index, token) in tokens.iter().enumerate() {
        if token.kind == TokenKind::DelimSlash {
            if slash_index.is_some() {
                return;
            }
            slash_index = Some(index);
        }
    }

    let split = slash_index.unwrap_or(tokens.len());
    let second_start = split + usize::from(slash_index.is_some());
    let mut first = tokens[..split].to_vec();
    let mut second = tokens[second_start..].to_vec();
    if !is_numeric_quad(&first) || slash_index.is_some() && !is_numeric_quad(&second) {
        return;
    }

    minify_four_side_shorthand(&mut first);
    if slash_index.is_none() {
        *tokens = first;
        return;
    }

    minify_four_side_shorthand(&mut second);
    if tokens_equal_ignoring_whitespace(&first, &second) {
        *tokens = first;
        return;
    }

    let mut slash = tokens[split].clone();
    slash.whitespace = if minify_whitespace {
        WhitespaceFlags::default()
    } else {
        WhitespaceFlags::BEFORE | WhitespaceFlags::AFTER
    };
    first.push(slash);
    first.extend(second);
    *tokens = first;
}

fn minify_border_radius_corner(tokens: &mut Vec<Token>) {
    if tokens.len() == 2
        && tokens.iter().all(|token| token.kind.is_numeric())
        && tokens[0].equal_ignoring_whitespace(&tokens[1])
    {
        tokens.truncate(1);
        tokens[0].whitespace = WhitespaceFlags::default();
    }
}

fn is_numeric_quad(tokens: &[Token]) -> bool {
    (1..=4).contains(&tokens.len()) && tokens.iter().all(|token| token.kind.is_numeric())
}

fn tokens_equal_ignoring_whitespace(left: &[Token], right: &[Token]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.equal_ignoring_whitespace(right))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum UnitSafetyStatus {
    #[default]
    Safe,
    UnsafeSingle,
    UnsafeMixed,
}

#[derive(Clone, Debug, Default)]
struct UnitSafety {
    unit: String,
    status: UnitSafetyStatus,
}

impl UnitSafety {
    fn include_unit_of(&mut self, token: &Token) {
        match token.kind {
            TokenKind::Number if token.text == "0" => return,
            TokenKind::Percentage => return,
            TokenKind::Dimension if token.dimension_unit_is_safe_length() => return,
            TokenKind::Dimension => {
                let unit = token.dimension_unit();
                if self.status == UnitSafetyStatus::Safe {
                    self.status = UnitSafetyStatus::UnsafeSingle;
                    self.unit = unit.into();
                    return;
                }
                if self.status == UnitSafetyStatus::UnsafeSingle && self.unit == unit {
                    return;
                }
            }
            _ => {}
        }
        self.status = UnitSafetyStatus::UnsafeMixed;
    }

    fn is_safe_with(&self, other: &Self) -> bool {
        self.status == other.status
            && self.status != UnitSafetyStatus::UnsafeMixed
            && (self.status != UnitSafetyStatus::UnsafeSingle || self.unit == other.unit)
    }
}

#[derive(Clone, Debug)]
struct BorderRadiusCorner {
    first_token: Token,
    second_token: Token,
    unit_safety: UnitSafety,
    rule_index: usize,
    was_single_rule: bool,
}

#[derive(Debug, Default)]
struct BorderRadiusTracker {
    corners: [Option<BorderRadiusCorner>; 4],
    important: bool,
}

impl BorderRadiusTracker {
    fn reset_for_importance(&mut self, important: bool) {
        if self.important != important {
            self.corners = Default::default();
            self.important = important;
        }
    }

    fn update_corner(&mut self, removed: &mut [bool], corner: usize, new: BorderRadiusCorner) {
        if let Some(old) = &self.corners[corner]
            && (!new.was_single_rule || old.was_single_rule)
            && old.unit_safety.status == UnitSafetyStatus::Safe
            && new.unit_safety.status == UnitSafetyStatus::Safe
        {
            removed[old.rule_index] = true;
        }
        self.corners[corner] = Some(new);
    }

    fn mangle_shorthand(
        &mut self,
        rules: &mut [Rule],
        removed: &mut [bool],
        rule_index: usize,
        declaration: &DeclarationRule,
        minify_whitespace: bool,
    ) {
        self.reset_for_importance(declaration.important);
        let Some((mut first_radii, mut second_radii)) =
            expand_border_radius_tokens(&declaration.value)
        else {
            self.corners = Default::default();
            return;
        };

        let mut unit_safety = UnitSafety::default();
        for token in first_radii.iter().chain(&second_radii) {
            unit_safety.include_unit_of(token);
        }
        if unit_safety.status == UnitSafetyStatus::Safe {
            for token in first_radii.iter_mut().chain(&mut second_radii) {
                token.turn_length_into_number_if_zero();
            }
        }

        for corner in 0..4 {
            self.update_corner(
                removed,
                corner,
                BorderRadiusCorner {
                    first_token: first_radii[corner].clone(),
                    second_token: second_radii[corner].clone(),
                    unit_safety: unit_safety.clone(),
                    rule_index,
                    was_single_rule: false,
                },
            );
        }
        self.compact_rules(rules, removed, declaration.key_range, minify_whitespace);
    }

    fn mangle_corner(
        &mut self,
        rules: &mut [Rule],
        removed: &mut [bool],
        rule_index: usize,
        declaration: &DeclarationRule,
        minify_whitespace: bool,
        corner: usize,
    ) {
        self.reset_for_importance(declaration.important);
        if !(1..=2).contains(&declaration.value.len())
            || !declaration
                .value
                .iter()
                .all(|token| token.kind.is_numeric())
        {
            self.corners = Default::default();
            return;
        }

        let mut first_token = declaration.value[0].clone();
        let mut second_token = declaration
            .value
            .get(1)
            .cloned()
            .unwrap_or_else(|| first_token.clone());
        let mut unit_safety = UnitSafety::default();
        unit_safety.include_unit_of(&first_token);
        unit_safety.include_unit_of(&second_token);
        if unit_safety.status == UnitSafetyStatus::Safe {
            first_token.turn_length_into_number_if_zero();
            second_token.turn_length_into_number_if_zero();
        }

        let mut value = vec![first_token.clone(), second_token.clone()];
        if first_token.equal_ignoring_whitespace(&second_token) {
            value.truncate(1);
        }
        for (index, token) in value.iter_mut().enumerate() {
            token.whitespace = if index == 0 {
                WhitespaceFlags::default()
            } else {
                WhitespaceFlags::BEFORE
            };
        }
        if let RuleData::Declaration(current) = &mut rules[rule_index].data {
            current.value = value;
        }

        self.update_corner(
            removed,
            corner,
            BorderRadiusCorner {
                first_token,
                second_token,
                unit_safety,
                rule_index,
                was_single_rule: true,
            },
        );
        self.compact_rules(rules, removed, declaration.key_range, minify_whitespace);
    }

    fn compact_rules(
        &self,
        rules: &mut [Rule],
        removed: &mut [bool],
        key_range: Range,
        minify_whitespace: bool,
    ) {
        let Some(corners) = self
            .corners
            .iter()
            .map(Option::as_ref)
            .collect::<Option<Vec<_>>>()
        else {
            return;
        };
        if corners[1..]
            .iter()
            .any(|corner| !corner.unit_safety.is_safe_with(&corners[0].unit_safety))
        {
            return;
        }

        let mut tokens = corners
            .iter()
            .map(|corner| corner.first_token.clone())
            .collect::<Vec<_>>();
        minify_four_side_shorthand(&mut tokens);
        let mut second_tokens = corners
            .iter()
            .map(|corner| corner.second_token.clone())
            .collect::<Vec<_>>();
        minify_four_side_shorthand(&mut second_tokens);
        if !tokens_equal_ignoring_whitespace(&tokens, &second_tokens) {
            let mut slash = Token {
                kind: TokenKind::DelimSlash,
                text: "/".into(),
                loc: tokens.last().map(|token| token.loc).unwrap_or_default(),
                ..Token::default()
            };
            if !minify_whitespace {
                slash.whitespace = WhitespaceFlags::BEFORE | WhitespaceFlags::AFTER;
            }
            tokens.push(slash);
            tokens.extend(second_tokens);
        }

        let target = corners[3].rule_index;
        let min_loc = corners
            .iter()
            .map(|corner| rules[corner.rule_index].loc)
            .min_by_key(|loc| loc.start)
            .unwrap_or_default();
        for corner in &corners {
            removed[corner.rule_index] = true;
        }
        removed[target] = false;
        rules[target] = Rule {
            loc: min_loc,
            data: RuleData::Declaration(DeclarationRule {
                key_text: "border-radius".into(),
                value: tokens,
                key_range,
                key: Declaration::BorderRadius,
                important: self.important,
            }),
        };
    }
}

fn expand_border_radius_tokens(tokens: &[Token]) -> Option<(Vec<Token>, Vec<Token>)> {
    let mut slash_index = None;
    for (index, token) in tokens.iter().enumerate() {
        if token.kind == TokenKind::DelimSlash {
            if slash_index.is_some() {
                return None;
            }
            slash_index = Some(index);
        }
    }
    let split = slash_index.unwrap_or(tokens.len());
    let first = expand_numeric_quad(&tokens[..split])?;
    let second = if slash_index.is_some() {
        expand_numeric_quad(&tokens[split + 1..])?
    } else {
        first.clone()
    };
    Some((first, second))
}

fn expand_numeric_quad(tokens: &[Token]) -> Option<Vec<Token>> {
    if !is_numeric_quad(tokens) {
        return None;
    }
    Some(vec![
        tokens[0].clone(),
        tokens.get(1).unwrap_or(&tokens[0]).clone(),
        tokens.get(2).unwrap_or(&tokens[0]).clone(),
        tokens
            .get(3)
            .or_else(|| tokens.get(1))
            .unwrap_or(&tokens[0])
            .clone(),
    ])
}

fn mangle_border_radius_declarations(rules: &mut Vec<Rule>, minify_whitespace: bool) {
    let mut tracker = BorderRadiusTracker::default();
    let mut removed = vec![false; rules.len()];
    for rule_index in 0..rules.len() {
        let RuleData::Declaration(declaration) = &rules[rule_index].data else {
            continue;
        };
        let declaration = declaration.clone();
        match declaration.key {
            Declaration::BorderRadius => tracker.mangle_shorthand(
                rules,
                &mut removed,
                rule_index,
                &declaration,
                minify_whitespace,
            ),
            Declaration::BorderTopLeftRadius => tracker.mangle_corner(
                rules,
                &mut removed,
                rule_index,
                &declaration,
                minify_whitespace,
                0,
            ),
            Declaration::BorderTopRightRadius => tracker.mangle_corner(
                rules,
                &mut removed,
                rule_index,
                &declaration,
                minify_whitespace,
                1,
            ),
            Declaration::BorderBottomRightRadius => tracker.mangle_corner(
                rules,
                &mut removed,
                rule_index,
                &declaration,
                minify_whitespace,
                2,
            ),
            Declaration::BorderBottomLeftRadius => tracker.mangle_corner(
                rules,
                &mut removed,
                rule_index,
                &declaration,
                minify_whitespace,
                3,
            ),
            _ => {}
        }
    }
    let mut index = 0;
    rules.retain(|_| {
        let keep = !removed[index];
        index += 1;
        keep
    });
}

#[derive(Clone, Debug)]
struct BoxSide {
    token: Token,
    unit_safety: UnitSafety,
    rule_index: usize,
    was_single_rule: bool,
}

#[derive(Debug)]
struct BoxTracker {
    key: Declaration,
    key_text: &'static str,
    allow_auto: bool,
    sides: [Option<BoxSide>; 4],
    important: bool,
}

impl BoxTracker {
    fn new(key: Declaration, key_text: &'static str, allow_auto: bool) -> Self {
        Self {
            key,
            key_text,
            allow_auto,
            sides: Default::default(),
            important: false,
        }
    }

    fn reset_for_importance(&mut self, important: bool) {
        if self.important != important {
            self.sides = Default::default();
            self.important = important;
        }
    }

    fn update_side(&mut self, removed: &mut [bool], side: usize, new: BoxSide) {
        if let Some(old) = &self.sides[side]
            && (!new.was_single_rule || old.was_single_rule)
            && old.unit_safety.status == UnitSafetyStatus::Safe
            && new.unit_safety.status == UnitSafetyStatus::Safe
        {
            removed[old.rule_index] = true;
        }
        self.sides[side] = Some(new);
    }

    fn mangle_shorthand(
        &mut self,
        rules: &mut [Rule],
        removed: &mut [bool],
        rule_index: usize,
        declaration: &DeclarationRule,
    ) {
        self.reset_for_importance(declaration.important);
        let Some(mut quad) = expand_box_quad(&declaration.value, self.allow_auto) else {
            self.sides = Default::default();
            return;
        };
        let mut unit_safety = UnitSafety::default();
        for token in &quad {
            if token.kind.is_numeric() {
                unit_safety.include_unit_of(token);
            }
        }
        if unit_safety.status == UnitSafetyStatus::Safe {
            for token in &mut quad {
                token.turn_length_into_number_if_zero();
            }
        }
        for (side, token) in quad.into_iter().enumerate() {
            self.update_side(
                removed,
                side,
                BoxSide {
                    token,
                    unit_safety: unit_safety.clone(),
                    rule_index,
                    was_single_rule: false,
                },
            );
        }
        self.compact_rules(rules, removed, declaration.key_range);
    }

    fn mangle_side(
        &mut self,
        rules: &mut [Rule],
        removed: &mut [bool],
        rule_index: usize,
        declaration: &DeclarationRule,
        side: usize,
    ) {
        self.reset_for_importance(declaration.important);
        let [token] = declaration.value.as_slice() else {
            self.sides = Default::default();
            return;
        };
        if !is_box_value(token, self.allow_auto) {
            self.sides = Default::default();
            return;
        }

        let mut token = token.clone();
        let mut unit_safety = UnitSafety::default();
        if token.kind.is_numeric() {
            unit_safety.include_unit_of(&token);
        }
        if unit_safety.status == UnitSafetyStatus::Safe {
            token.turn_length_into_number_if_zero();
        }
        if let RuleData::Declaration(current) = &mut rules[rule_index].data {
            current.value = vec![token.clone()];
        }
        self.update_side(
            removed,
            side,
            BoxSide {
                token,
                unit_safety,
                rule_index,
                was_single_rule: true,
            },
        );
        self.compact_rules(rules, removed, declaration.key_range);
    }

    fn compact_rules(&self, rules: &mut [Rule], removed: &mut [bool], key_range: Range) {
        if self.key == Declaration::Unknown {
            return;
        }
        let Some(sides) = self
            .sides
            .iter()
            .map(Option::as_ref)
            .collect::<Option<Vec<_>>>()
        else {
            return;
        };
        if sides[1..]
            .iter()
            .any(|side| !side.unit_safety.is_safe_with(&sides[0].unit_safety))
        {
            return;
        }

        let mut value = sides
            .iter()
            .map(|side| side.token.clone())
            .collect::<Vec<_>>();
        minify_four_side_shorthand(&mut value);
        let target = sides[3].rule_index;
        let min_loc = sides
            .iter()
            .map(|side| rules[side.rule_index].loc)
            .min_by_key(|loc| loc.start)
            .unwrap_or_default();
        for side in &sides {
            removed[side.rule_index] = true;
        }
        removed[target] = false;
        rules[target] = Rule {
            loc: min_loc,
            data: RuleData::Declaration(DeclarationRule {
                key_text: self.key_text.into(),
                value,
                key_range,
                key: self.key,
                important: self.important,
            }),
        };
    }
}

fn is_box_value(token: &Token, allow_auto: bool) -> bool {
    token.kind.is_numeric()
        || allow_auto && token.kind == TokenKind::Ident && token.text.eq_ignore_ascii_case("auto")
}

fn is_box_quad(tokens: &[Token], allow_auto: bool) -> bool {
    (1..=4).contains(&tokens.len()) && tokens.iter().all(|token| is_box_value(token, allow_auto))
}

fn expand_box_quad(tokens: &[Token], allow_auto: bool) -> Option<Vec<Token>> {
    if !is_box_quad(tokens, allow_auto) {
        return None;
    }
    Some(vec![
        tokens[0].clone(),
        tokens.get(1).unwrap_or(&tokens[0]).clone(),
        tokens.get(2).unwrap_or(&tokens[0]).clone(),
        tokens
            .get(3)
            .or_else(|| tokens.get(1))
            .unwrap_or(&tokens[0])
            .clone(),
    ])
}

fn lower_inset_declarations(rules: &mut Vec<Rule>, minify_whitespace: bool) {
    let mut rewritten = Vec::with_capacity(rules.len());
    for rule in rules.drain(..) {
        let RuleData::Declaration(declaration) = &rule.data else {
            rewritten.push(rule);
            continue;
        };
        if declaration.key != Declaration::Inset {
            rewritten.push(rule);
            continue;
        }
        let Some(mut quad) = expand_box_quad(&declaration.value, false) else {
            rewritten.push(rule);
            continue;
        };
        for token in &mut quad {
            if minify_whitespace {
                token.whitespace = WhitespaceFlags::default();
            } else {
                token.whitespace.remove(WhitespaceFlags::AFTER);
            }
        }
        for (token, (key_text, key)) in quad.into_iter().zip([
            ("top", Declaration::Top),
            ("right", Declaration::Right),
            ("bottom", Declaration::Bottom),
            ("left", Declaration::Left),
        ]) {
            rewritten.push(Rule {
                loc: rule.loc,
                data: RuleData::Declaration(DeclarationRule {
                    key_text: key_text.into(),
                    value: vec![token],
                    key_range: declaration.key_range,
                    key,
                    important: declaration.important,
                }),
            });
        }
    }
    *rules = rewritten;
}

fn mangle_box_declarations(rules: &mut Vec<Rule>, allow_inset_shorthand: bool) {
    let mut margin = BoxTracker::new(Declaration::Margin, "margin", true);
    let mut padding = BoxTracker::new(Declaration::Padding, "padding", false);
    let mut inset = if allow_inset_shorthand {
        BoxTracker::new(Declaration::Inset, "inset", true)
    } else {
        BoxTracker::new(Declaration::Unknown, "", true)
    };
    let mut removed = vec![false; rules.len()];
    for rule_index in 0..rules.len() {
        let RuleData::Declaration(declaration) = &rules[rule_index].data else {
            continue;
        };
        let declaration = declaration.clone();
        let (tracker, side) = match declaration.key {
            Declaration::Margin => (&mut margin, None),
            Declaration::MarginTop => (&mut margin, Some(0)),
            Declaration::MarginRight => (&mut margin, Some(1)),
            Declaration::MarginBottom => (&mut margin, Some(2)),
            Declaration::MarginLeft => (&mut margin, Some(3)),
            Declaration::Padding => (&mut padding, None),
            Declaration::PaddingTop => (&mut padding, Some(0)),
            Declaration::PaddingRight => (&mut padding, Some(1)),
            Declaration::PaddingBottom => (&mut padding, Some(2)),
            Declaration::PaddingLeft => (&mut padding, Some(3)),
            Declaration::Inset => (&mut inset, None),
            Declaration::Top => (&mut inset, Some(0)),
            Declaration::Right => (&mut inset, Some(1)),
            Declaration::Bottom => (&mut inset, Some(2)),
            Declaration::Left => (&mut inset, Some(3)),
            _ => continue,
        };
        if let Some(side) = side {
            tracker.mangle_side(rules, &mut removed, rule_index, &declaration, side);
        } else {
            tracker.mangle_shorthand(rules, &mut removed, rule_index, &declaration);
        }
    }
    let mut index = 0;
    rules.retain(|_| {
        let keep = !removed[index];
        index += 1;
        keep
    });
}

const CSS_WIDE_AND_RESERVED_KEYWORDS: &[&str] = &[
    "initial",
    "inherit",
    "unset",
    "default",
    "revert",
    "revert-layer",
];

const GENERIC_FONT_FAMILY_NAMES: &[&str] = &[
    "serif",
    "sans-serif",
    "cursive",
    "fantasy",
    "monospace",
    "system-ui",
    "emoji",
    "math",
    "fangsong",
    "ui-serif",
    "ui-sans-serif",
    "ui-monospace",
    "ui-rounded",
];

fn is_predefined_counter_style(name: &str) -> bool {
    matches!(
        name,
        "arabic-indic"
            | "armenian"
            | "bengali"
            | "cambodian"
            | "cjk-decimal"
            | "decimal-leading-zero"
            | "decimal"
            | "devanagari"
            | "georgian"
            | "gujarati"
            | "gurmukhi"
            | "hebrew"
            | "kannada"
            | "khmer"
            | "lao"
            | "lower-armenian"
            | "lower-roman"
            | "malayalam"
            | "mongolian"
            | "myanmar"
            | "oriya"
            | "persian"
            | "tamil"
            | "telugu"
            | "thai"
            | "tibetan"
            | "upper-armenian"
            | "upper-roman"
            | "hiragana-iroha"
            | "hiragana"
            | "katakana-iroha"
            | "katakana"
            | "lower-alpha"
            | "lower-greek"
            | "lower-latin"
            | "upper-alpha"
            | "upper-latin"
            | "circle"
            | "disc"
            | "disclosure-closed"
            | "disclosure-open"
            | "square"
            | "cjk-earthly-branch"
            | "cjk-heavenly-stem"
            | "japanese-formal"
            | "japanese-informal"
            | "korean-hangul-formal"
            | "korean-hanja-formal"
            | "korean-hanja-informal"
            | "simp-chinese-formal"
            | "simp-chinese-informal"
            | "trad-chinese-formal"
            | "trad-chinese-informal"
            | "ethiopic-numeric"
    )
}

fn minify_font_family(tokens: &[Token], minify_whitespace: bool) -> Option<Vec<Token>> {
    let mut result = Vec::new();
    let mut position = minify_font_family_name(&mut result, tokens, 0, minify_whitespace)?;
    while position < tokens.len() && tokens[position].kind == TokenKind::Comma {
        result.push(tokens[position].clone());
        position = minify_font_family_name(&mut result, tokens, position + 1, minify_whitespace)?;
    }
    (position == tokens.len()).then_some(result)
}

fn minify_font_family_name(
    result: &mut Vec<Token>,
    tokens: &[Token],
    position: usize,
    minify_whitespace: bool,
) -> Option<usize> {
    let token = tokens.get(position)?;
    if token.kind == TokenKind::Ident && GENERIC_FONT_FAMILY_NAMES.contains(&token.text.as_str()) {
        result.push(token.clone());
        return Some(position + 1);
    }
    if token.kind == TokenKind::String {
        let names = token.text.split(' ').collect::<Vec<_>>();
        if names
            .iter()
            .all(|name| is_valid_custom_ident(name, GENERIC_FONT_FAMILY_NAMES))
        {
            for (index, name) in names.into_iter().enumerate() {
                let mut ident = Token {
                    kind: TokenKind::Ident,
                    text: name.into(),
                    loc: token.loc,
                    ..Token::default()
                };
                if index != 0 || !minify_whitespace {
                    ident.whitespace = WhitespaceFlags::BEFORE;
                }
                result.push(ident);
            }
        } else {
            result.push(token.clone());
        }
        return Some(position + 1);
    }
    if token.kind != TokenKind::Ident {
        return None;
    }

    let mut position = position;
    while let Some(token) = tokens.get(position) {
        if token.kind != TokenKind::Ident {
            break;
        }
        if !is_valid_custom_ident(&token.text, GENERIC_FONT_FAMILY_NAMES) {
            return None;
        }
        result.push(token.clone());
        position += 1;
    }
    Some(position)
}

fn is_valid_custom_ident(text: &str, predefined_keywords: &[&str]) -> bool {
    let lower = text.to_ascii_lowercase();
    !text.is_empty()
        && !predefined_keywords.contains(&lower.as_str())
        && !CSS_WIDE_AND_RESERVED_KEYWORDS.contains(&lower.as_str())
        && would_start_identifier_without_escapes(text.as_bytes())
        && text
            .chars()
            .all(|character| is_name_continue(character as i32))
}

fn minify_font(tokens: &mut Vec<Token>, minify_whitespace: bool) {
    let original = tokens.clone();
    let mut result = Vec::new();
    let mut position = 0;
    while position < original.len() && !is_font_size(&original[position]) {
        let mut token = original[position].clone();
        match token.kind {
            TokenKind::Ident => match token.text.to_ascii_lowercase().as_str() {
                "normal" => {
                    position += 1;
                    continue;
                }
                "italic" | "small-caps" | "ultra-condensed" | "extra-condensed" | "condensed"
                | "semi-condensed" | "semi-expanded" | "expanded" | "extra-expanded"
                | "ultra-expanded" => {}
                "oblique" => {
                    if original.get(position + 1).is_some_and(Token::is_angle) {
                        result.push(token);
                        result.push(original[position + 1].clone());
                        position += 2;
                        continue;
                    }
                }
                "bold" | "bolder" | "lighter" => {
                    minify_font_weight_token(&mut token);
                }
                _ => return,
            },
            TokenKind::Number => {
                let Ok(value) = token.text.parse::<f64>() else {
                    return;
                };
                if !(1.0..=1000.0).contains(&value) {
                    return;
                }
            }
            _ => return,
        }
        result.push(token);
        position += 1;
    }
    let Some(size) = original.get(position) else {
        return;
    };
    result.push(size.clone());
    position += 1;

    if original
        .get(position)
        .is_some_and(|token| token.kind == TokenKind::DelimSlash)
    {
        let Some(line_height) = original.get(position + 1) else {
            return;
        };
        let mut slash = original[position].clone();
        let mut line_height = line_height.clone();
        if minify_whitespace {
            if let Some(size) = result.last_mut() {
                size.whitespace.remove(WhitespaceFlags::AFTER);
            }
            slash.whitespace = WhitespaceFlags::default();
            line_height.whitespace.remove(WhitespaceFlags::BEFORE);
        }
        result.push(slash);
        result.push(line_height);
        position += 2;
    }

    let Some(mut family) = minify_font_family(&original[position..], minify_whitespace) else {
        return;
    };
    if !result.is_empty() && !family.is_empty() && family[0].kind != TokenKind::String {
        family[0].whitespace |= WhitespaceFlags::BEFORE;
    }
    result.extend(family);
    *tokens = result;
}

fn minify_font_weight_token(token: &mut Token) {
    if token.kind != TokenKind::Ident {
        return;
    }
    if token.text.eq_ignore_ascii_case("normal") {
        token.kind = TokenKind::Number;
        token.text = "400".into();
    } else if token.text.eq_ignore_ascii_case("bold") {
        token.kind = TokenKind::Number;
        token.text = "700".into();
    }
}

fn is_font_size(token: &Token) -> bool {
    matches!(token.kind, TokenKind::Dimension | TokenKind::Percentage)
        || token.kind == TokenKind::Ident
            && matches!(
                token.text.to_ascii_lowercase().as_str(),
                "xx-small"
                    | "x-small"
                    | "small"
                    | "medium"
                    | "large"
                    | "x-large"
                    | "xx-large"
                    | "xxx-large"
                    | "larger"
                    | "smaller"
            )
}

fn lower_and_minify_box_shadows(
    tokens: &mut Vec<Token>,
    minify_syntax: bool,
    minify_whitespace: bool,
    unsupported_css_features: CssFeature,
) {
    let original = std::mem::take(tokens);
    let mut start = 0;
    for index in 0..=original.len() {
        if index == original.len() || original[index].kind == TokenKind::Comma {
            let mut shadow = original[start..index].to_vec();
            lower_and_minify_box_shadow(
                &mut shadow,
                minify_syntax,
                minify_whitespace,
                unsupported_css_features,
            );
            tokens.extend(shadow);
            if index < original.len() {
                tokens.push(original[index].clone());
            }
            start = index + 1;
        }
    }
}

fn lower_and_minify_box_shadow(
    tokens: &mut Vec<Token>,
    minify_syntax: bool,
    minify_whitespace: bool,
    unsupported_css_features: CssFeature,
) {
    let mut inset_count = 0;
    let mut color_count = 0;
    let mut numbers_begin = 0;
    let mut numbers_count = 0;
    let mut numbers_done = false;
    let mut found_unexpected_token = false;
    for (index, token) in tokens.iter_mut().enumerate() {
        if matches!(token.kind, TokenKind::Number | TokenKind::Dimension) {
            if numbers_done {
                found_unexpected_token = true;
            }
            if minify_syntax {
                token.turn_length_into_number_if_zero();
            }
            if numbers_count == 0 {
                numbers_begin = index;
            }
            numbers_count += 1;
        } else {
            if numbers_count != 0 {
                numbers_done = true;
            }
            if token_looks_like_color(token) {
                color_count += 1;
                lower_and_minify_single_color(
                    std::slice::from_mut(token),
                    minify_syntax,
                    minify_whitespace,
                    unsupported_css_features,
                );
            } else if token.kind == TokenKind::Ident && token.text.eq_ignore_ascii_case("inset") {
                inset_count += 1;
            } else {
                found_unexpected_token = true;
            }
        }
    }
    if minify_syntax
        && inset_count <= 1
        && color_count <= 1
        && numbers_count > 2
        && numbers_count <= 4
        && !found_unexpected_token
    {
        let numbers_end = numbers_begin + numbers_count;
        while numbers_count > 2 && tokens[numbers_begin + numbers_count - 1].is_zero() {
            numbers_count -= 1;
        }
        tokens.drain(numbers_begin + numbers_count..numbers_end);
    }
    let token_count = tokens.len();
    for (index, token) in tokens.iter_mut().enumerate() {
        token.whitespace = WhitespaceFlags::default();
        if index > 0 || !minify_whitespace {
            token.whitespace |= WhitespaceFlags::BEFORE;
        }
        if index + 1 < token_count {
            token.whitespace |= WhitespaceFlags::AFTER;
        }
    }
}

fn token_looks_like_color(token: &Token) -> bool {
    match token.kind {
        TokenKind::Hash => true,
        TokenKind::Ident => named_color_hex(&token.text.to_ascii_lowercase()).is_some(),
        TokenKind::Function => matches!(
            token.text.to_ascii_lowercase().as_str(),
            "color"
                | "color-mix"
                | "hsl"
                | "hsla"
                | "hwb"
                | "lab"
                | "lch"
                | "light-dark"
                | "oklab"
                | "oklch"
                | "rgb"
                | "rgba"
        ),
        _ => false,
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

fn reduce_calc_expressions(tokens: &mut [Token], preserve_replacement_whitespace: bool) {
    for token in tokens.iter_mut() {
        if let Some(children) = &mut token.children {
            reduce_calc_expressions(children, preserve_replacement_whitespace);
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
            if preserve_replacement_whitespace {
                replacement.whitespace |= WhitespaceFlags::BEFORE | WhitespaceFlags::AFTER;
            }
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
        "counter-style"
            | "container"
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

    use super::{ALPHA_FRACTION_TABLE, Options, SymbolMode, make_dead_rule_mangler, parse};
    use crate::internal::{
        ast::{ImportKind, SymbolKind, SymbolMap},
        compat::CssFeature,
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
        parse_and_print_with_options(contents, false, minify_whitespace)
    }

    fn parse_and_print_with_options(
        contents: &str,
        minify_syntax: bool,
        minify_whitespace: bool,
    ) -> String {
        parse_and_print_with_parser_options(
            contents,
            Options {
                minify_syntax,
                minify_whitespace,
                ..Options::default()
            },
        )
    }

    fn parse_and_print_with_parser_options(contents: &str, options: Options) -> String {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let tree = parse(log.clone(), source(contents), options);
        assert!(log.done().is_empty());
        let mut symbols = SymbolMap::new(1);
        symbols.symbols_for_source[0] = tree.symbols.clone();
        String::from_utf8(
            css_printer::print(
                &tree,
                &symbols,
                css_printer::Options {
                    minify_whitespace: options.minify_whitespace,
                    ..css_printer::Options::default()
                },
            )
            .css,
        )
        .expect("CSS output is UTF-8")
    }

    #[test]
    fn lowers_unsupported_double_position_gradient_stops() {
        for gradient in [
            "linear-gradient",
            "repeating-linear-gradient",
            "radial-gradient",
            "repeating-radial-gradient",
            "conic-gradient",
            "repeating-conic-gradient",
        ] {
            let input = format!(
                "a {{ background: {gradient}(green, red 10%, red 20%, yellow 70% 80%, black) }}"
            );
            let output = parse_and_print_with_parser_options(
                &input,
                Options {
                    unsupported_css_features: CssFeature::GRADIENT_DOUBLE_POSITION,
                    ..Options::default()
                },
            );
            assert!(
                output.contains("yellow 70%,\n      yellow 80%"),
                "{gradient}: {output}"
            );
            assert!(!output.contains("yellow 70% 80%"), "{gradient}: {output}");
        }
    }

    #[test]
    fn lowers_gradient_positions_without_unsafe_minification() {
        let options = Options {
            unsupported_css_features: CssFeature::GRADIENT_DOUBLE_POSITION,
            ..Options::default()
        };
        assert_eq!(
            parse_and_print_with_parser_options(
                "a { background: linear-gradient(red calc(10%) calc(20%), blue) }\
                 b { background: linear-gradient(var(--stops)) }",
                options,
            ),
            "a {\n\
             \x20\x20background:\n\
             \x20\x20\x20\x20linear-gradient(\n\
             \x20\x20\x20\x20\x20\x20red calc(10%),\n\
             \x20\x20\x20\x20\x20\x20red calc(20%),\n\
             \x20\x20\x20\x20\x20\x20blue);\n\
             }\n\
             b {\n\
             \x20\x20background: linear-gradient(var(--stops));\n\
             }\n"
        );
    }

    #[test]
    fn minification_does_not_restore_unsupported_double_positions() {
        assert_eq!(
            parse_and_print_with_parser_options(
                "a { background: linear-gradient(green, red 10%, red 20%, yellow 70% 80%, black) }",
                Options {
                    minify_syntax: true,
                    minify_whitespace: true,
                    unsupported_css_features: CssFeature::GRADIENT_DOUBLE_POSITION,
                    ..Options::default()
                },
            ),
            "a{background:linear-gradient(green,red 10%,red 20%,#ff0 70%,#ff0 80%,#000)}"
        );
    }

    #[test]
    fn lowers_unsupported_rebecca_purple_colors() {
        let input = "a {\
                       color: ReBeCcApUrPlE;\
                       background: ReBeCcApUrPlE;\
                       box-shadow: 0px 0px 0px 0px ReBeCcApUrPlE, inset 1px 2px rebeccapurple;\
                       text-shadow: 0 0 rebeccapurple;\
                       --x: rebeccapurple\
                     }\
                     b { background-image: linear-gradient(ReBeCcApUrPlE, blue) }";
        let lowered = parse_and_print_with_parser_options(
            input,
            Options {
                unsupported_css_features: CssFeature::REBECCA_PURPLE,
                ..Options::default()
            },
        );
        assert_eq!(
            lowered,
            "a {\n\
             \x20\x20color: #663399;\n\
             \x20\x20background: #663399;\n\
             \x20\x20box-shadow: 0px 0px 0px 0px #663399, inset 1px 2px #663399;\n\
             \x20\x20text-shadow: 0 0 rebeccapurple;\n\
             \x20\x20--x: rebeccapurple;\n\
             }\n\
             b {\n\
             \x20\x20background-image: linear-gradient(#663399, blue);\n\
             }\n"
        );

        let supported = parse_and_print_with_parser_options(input, Options::default());
        assert!(!supported.contains("#663399"), "{supported}");
        assert!(supported.contains("color: ReBeCcApUrPlE"), "{supported}");

        assert_eq!(
            parse_and_print_with_parser_options(
                "a { color: ReBeCcApUrPlE; box-shadow: 0px 0px 0px 0px ReBeCcApUrPlE }\
                 b { background-image: linear-gradient(ReBeCcApUrPlE, blue) }",
                Options {
                    minify_syntax: true,
                    minify_whitespace: true,
                    unsupported_css_features: CssFeature::REBECCA_PURPLE,
                    ..Options::default()
                },
            ),
            "a{color:#639;box-shadow:0 0 #639}b{background-image:linear-gradient(#639,#00f)}"
        );
    }

    #[test]
    fn lowers_unsupported_hex_rgba_colors() {
        assert_eq!(ALPHA_FRACTION_TABLE.len(), 256 * 4);
        let options = Options {
            unsupported_css_features: CssFeature::HEX_RGBA,
            ..Options::default()
        };
        assert_eq!(
            parse_and_print_with_parser_options(
                "a { color: #0123 } b { color: #1230 } c { color: #1234 }\
                 d { color: #123f } e { color: #12345678 } f { color: #ff00007f }",
                options,
            ),
            "a {\n\
             \x20\x20color: rgba(0, 17, 34, .2);\n\
             }\n\
             b {\n\
             \x20\x20color: rgba(17, 34, 51, 0);\n\
             }\n\
             c {\n\
             \x20\x20color: rgba(17, 34, 51, .267);\n\
             }\n\
             d {\n\
             \x20\x20color: #112233;\n\
             }\n\
             e {\n\
             \x20\x20color: rgba(18, 52, 86, .47);\n\
             }\n\
             f {\n\
             \x20\x20color: rgba(255, 0, 0, .498);\n\
             }\n"
        );
        assert_eq!(
            parse_and_print_with_parser_options(
                "a{color:#00000001}b{color:#00000002}c{color:#00000003}\
                 d{color:#0000007f}e{color:#000000fe}",
                Options {
                    minify_whitespace: true,
                    ..options
                },
            ),
            "a{color:rgba(0,0,0,.004)}b{color:rgba(0,0,0,.008)}\
             c{color:rgba(0,0,0,.01)}d{color:rgba(0,0,0,.498)}\
             e{color:rgba(0,0,0,.996)}"
        );

        let supported =
            parse_and_print_with_parser_options("a { color: #11223344 }", Options::default());
        assert!(supported.contains("#11223344"), "{supported}");
        let compact = parse_and_print_with_parser_options(
            "a { color: #11223344 }",
            Options {
                minify_syntax: true,
                minify_whitespace: true,
                ..Options::default()
            },
        );
        assert_eq!(compact, "a{color:#1234}");
    }

    #[test]
    fn lowers_hex_rgba_across_color_declaration_contexts() {
        let options = Options {
            unsupported_css_features: CssFeature::HEX_RGBA,
            ..Options::default()
        };
        let output = parse_and_print_with_parser_options(
            "a {\
               background: border-box #11223344;\
               box-shadow: 0px 0px 0px 0px #1234, inset 1px 2px #12345678\
             }\
             b { background-image: linear-gradient(#11223344 10%, blue) }\
             c { text-shadow: 0 0 #1234; --x: #1234 }",
            options,
        );
        assert!(
            output.contains("background: border-box rgba(17, 34, 51, .267)"),
            "{output}"
        );
        assert!(
            output.contains(
                "box-shadow: 0px 0px 0px 0px rgba(17, 34, 51, .267), \
                 inset 1px 2px rgba(18, 52, 86, .47)"
            ),
            "{output}"
        );
        assert!(
            output.contains("linear-gradient(rgba(17, 34, 51, .267) 10%, blue)"),
            "{output}"
        );
        assert!(output.contains("text-shadow: 0 0 #1234"), "{output}");
        assert!(output.contains("--x: #1234"), "{output}");

        assert_eq!(
            parse_and_print_with_parser_options(
                "a { color: rgba(255, 0, 0, .5) }\
                 b { color: hsla(120, 100%, 25%, .25) }\
                 c { color: hwb(240 0% 0% / 75%) }",
                Options {
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..options
                },
            ),
            "a{color:rgba(255,0,0,.5)}b{color:rgba(0,128,0,.25)}\
            c{color:rgba(0,0,255,.75)}"
        );
    }

    #[test]
    fn lowers_unsupported_hwb_colors() {
        let options = Options {
            unsupported_css_features: CssFeature::HWB,
            ..Options::default()
        };
        assert_eq!(
            parse_and_print_with_parser_options(
                "a { color: HWB(90deg 20% 40%);\
                     outline-color: hwb(.75turn 20% 40% / .6667);\
                     fill: hwb(1deg 40% 80%);\
                     stroke: hwb(1deg 9000% 50%) }",
                options,
            ),
            "a {\n\
             \x20\x20color: #669933;\n\
             \x20\x20outline-color: #663399aa;\n\
             \x20\x20fill: #555555;\n\
             \x20\x20stroke: #aaaaaa;\n\
             }\n"
        );

        assert_eq!(
            parse_and_print_with_parser_options(
                "a { color: hwb(90deg 20% 40%);\
                     outline-color: hwb(.75turn 20% 40% / .6667) }",
                Options {
                    minify_whitespace: true,
                    ..options
                },
            ),
            "a{color:#669933;outline-color:#663399aa}"
        );
        assert_eq!(
            parse_and_print_with_parser_options(
                "a { color: hwb(90deg 20% 40%);\
                     outline-color: hwb(.75turn 20% 40% / .6667) }",
                Options {
                    minify_syntax: true,
                    ..options
                },
            ),
            "a {\n  color: #693;\n  outline-color: #639a;\n}\n"
        );

        let invalid = "a { color: hwb(90deg, 20%, 40%);\
                           fill: hwb(none 20% 40%);\
                           stroke: hwb(90deg 20% none) }";
        assert_eq!(
            parse_and_print_with_parser_options(
                invalid,
                Options {
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..options
                },
            ),
            "a{color:hwb(90deg,20%,40%);fill:hwb(none 20% 40%);stroke:hwb(90deg 20% none)}"
        );

        let supported = parse_and_print_with_parser_options(
            "a { color: HWB(90deg 20% 40%) }",
            Options::default(),
        );
        assert_eq!(supported, "a {\n  color: HWB(90deg 20% 40%);\n}\n");
    }

    #[test]
    fn lowers_hwb_alpha_for_hex_rgba_compatibility() {
        assert_eq!(
            parse_and_print_with_parser_options(
                "a { color: hwb(90 20% 40% / .2);\
                     outline-color: hwb(270 20% 40% / .6667) }",
                Options {
                    unsupported_css_features: CssFeature::HWB,
                    ..Options::default()
                },
            ),
            "a {\n  color: #66993333;\n  outline-color: #663399aa;\n}\n"
        );
        assert_eq!(
            parse_and_print_with_parser_options(
                "a { color: hwb(90 20% 40% / .2) }",
                Options {
                    unsupported_css_features: CssFeature::HWB | CssFeature::HEX_RGBA,
                    ..Options::default()
                },
            ),
            "a {\n  color: rgba(102, 153, 51, .2);\n}\n"
        );
        assert_eq!(
            parse_and_print_with_parser_options(
                "a { color: hwb(90 20% 40% / .2);\
                     outline-color: hwb(270 20% 40% / .6667) }",
                Options {
                    minify_syntax: true,
                    minify_whitespace: true,
                    unsupported_css_features: CssFeature::HWB | CssFeature::HEX_RGBA,
                    ..Options::default()
                },
            ),
            "a{color:rgba(102,153,51,.2);outline-color:rgba(102,51,153,.667)}"
        );
        assert_eq!(
            parse_and_print_with_parser_options(
                "a { color: hwb(90 20% 40% / .2) }",
                Options {
                    unsupported_css_features: CssFeature::HEX_RGBA,
                    ..Options::default()
                },
            ),
            "a {\n  color: hwb(90 20% 40% / .2);\n}\n"
        );
    }

    #[test]
    fn lowers_hwb_across_color_declaration_contexts() {
        let options = Options {
            unsupported_css_features: CssFeature::HWB,
            ..Options::default()
        };
        for property in [
            "background-color",
            "border-block-end-color",
            "border-block-start-color",
            "border-bottom-color",
            "border-color",
            "border-inline-end-color",
            "border-inline-start-color",
            "border-left-color",
            "border-right-color",
            "border-top-color",
            "caret-color",
            "color",
            "column-rule-color",
            "fill",
            "flood-color",
            "lighting-color",
            "outline-color",
            "stop-color",
            "stroke",
            "text-decoration-color",
            "text-emphasis-color",
        ] {
            assert_eq!(
                parse_and_print_with_parser_options(
                    &format!("a {{ {property}: hwb(90 20% 40%) }}"),
                    options,
                ),
                format!("a {{\n  {property}: #669933;\n}}\n"),
                "{property}"
            );
        }

        assert_eq!(
            parse_and_print_with_parser_options(
                "a {\
                   background: border-box hwb(90 20% 40%);\
                   box-shadow: 0 0 hwb(90 20% 40%), inset 1px 2px hwb(270 20% 40% / .75);\
                   text-shadow: 0 0 hwb(90 20% 40%);\
                   border-color: hwb(90 20% 40%) hwb(270 20% 40%);\
                   --x: hwb(90 20% 40%)\
                 }\
                 b {\
                   background: linear-gradient(hwb(90 20% 40%) 10%, hwb(270 20% 40% / .75));\
                   background-image: radial-gradient(hwb(90 20% 40%), blue);\
                   border-image: conic-gradient(hwb(90 20% 40%), blue);\
                   mask-image: linear-gradient(hwb(90 20% 40%), blue)\
                 }",
                Options {
                    minify_whitespace: true,
                    ..options
                },
            ),
            "a{background:border-box #669933;\
               box-shadow:0 0 #669933,inset 1px 2px #663399bf;\
               text-shadow:0 0 hwb(90 20% 40%);\
               border-color:hwb(90 20% 40%) hwb(270 20% 40%);\
               --x: hwb(90 20% 40%)}\
             b{background:linear-gradient(#669933 10%,#663399bf);\
               background-image:radial-gradient(#669933,blue);\
               border-image:conic-gradient(#669933,blue);\
               mask-image:linear-gradient(#669933,blue)}"
        );
    }

    #[test]
    fn lowers_unsupported_modern_rgb_hsl_branch_matrix() {
        let input = "a{color:rgb(1 2 3)}\
                     b{color:rgba(1% 2% 3%)}\
                     c{color:hsl(1deg 2% 3%)}\
                     d{color:hsla(200grad 2% 3%)}\
                     e{color:rgb(1 2 3/4)}\
                     f{color:rgba(1% 2% 3%/4%)}\
                     g{color:hsl(1 2% 3%/4)}\
                     h{color:hsla(1 2% 3%/4%)}\
                     i{color:rgb(1,2,3)}\
                     j{color:rgba(1,2,3)}\
                     k{color:rgb(1,2,3,4%)}\
                     l{color:hsla(1turn,2%,3%,.04%)}\
                     m{color:RGB(1 2 3)}\
                     n{color:RGBA(1 2 3)}\
                     o{color:RGB(1 2 3/4)}\
                     p{color:RGBA(1 2 3/4)}\
                     q{color:HSL(1 2% 3%)}\
                     r{color:HSLA(1 2% 3%)}\
                     s{color:HSL(1 2% 3%/4)}\
                     t{color:HSLA(1 2% 3%/4)}\
                     u{color:hsl(6.283185307rad 2% 3%)}\
                     v{color:hsl(.333333turn 2% 3%)}\
                     w{color:hsl(-200grad 2% 3%)}\
                     x{color:hsl(1DEG 2% 3%)}\
                     y{color:rgb(1 2 3/33.333%)}\
                     z{color:rgb(1 2 3/99.99%)}";
        let options = Options {
            minify_whitespace: true,
            unsupported_css_features: CssFeature::MODERN_RGB_HSL,
            ..Options::default()
        };
        assert_eq!(
            parse_and_print_with_parser_options(input, options),
            "a{color:rgb(1,2, 3)}\
             b{color:rgb(1%,2%, 3%)}\
             c{color:hsl(1,2%, 3%)}\
             d{color:hsl(180,2%, 3%)}\
             e{color:rgba(1,2,3,4)}\
             f{color:rgba(1%,2%,3%,0.04)}\
             g{color:hsla(1,2%,3%,4)}\
             h{color:hsla(1,2%,3%,0.04)}\
             i{color:rgb(1,2,3)}\
             j{color:rgb(1,2,3)}\
             k{color:rgba(1,2,3,0.04)}\
             l{color:hsla(360,2%,3%,0)}\
             m{color:RGB(1,2, 3)}\
             n{color:rgb(1,2, 3)}\
             o{color:rgba(1,2,3,4)}\
             p{color:RGBA(1,2,3,4)}\
             q{color:HSL(1,2%, 3%)}\
             r{color:hsl(1,2%, 3%)}\
             s{color:hsla(1,2%,3%,4)}\
             t{color:HSLA(1,2%,3%,4)}\
             u{color:hsl(360,2%, 3%)}\
             v{color:hsl(120,2%, 3%)}\
             w{color:hsl(-180,2%, 3%)}\
             x{color:hsl(1DEG,2%, 3%)}\
             y{color:rgba(1,2,3,0.333)}\
             z{color:rgba(1,2,3,1)}"
        );

        assert_eq!(
            parse_and_print_with_parser_options(
                input,
                Options {
                    minify_whitespace: true,
                    ..Options::default()
                }
            ),
            input
        );
    }

    #[test]
    fn lowers_modern_rgb_hsl_across_color_declaration_contexts() {
        let options = Options {
            minify_whitespace: true,
            unsupported_css_features: CssFeature::MODERN_RGB_HSL,
            ..Options::default()
        };
        for property in [
            "background-color",
            "border-block-end-color",
            "border-block-start-color",
            "border-bottom-color",
            "border-inline-end-color",
            "border-inline-start-color",
            "border-left-color",
            "border-right-color",
            "border-top-color",
            "caret-color",
            "color",
            "column-rule-color",
            "fill",
            "flood-color",
            "lighting-color",
            "outline-color",
            "stop-color",
            "stroke",
            "text-decoration-color",
            "text-emphasis-color",
        ] {
            assert_eq!(
                parse_and_print_with_parser_options(
                    &format!("a{{{property}:rgb(1 2 3/4%)}}"),
                    options,
                ),
                format!("a{{{property}:rgba(1,2,3,0.04)}}"),
                "{property}"
            );
        }

        for gradient in [
            "linear-gradient",
            "repeating-linear-gradient",
            "radial-gradient",
            "repeating-radial-gradient",
            "conic-gradient",
            "repeating-conic-gradient",
        ] {
            assert_eq!(
                parse_and_print_with_parser_options(
                    &format!("a{{background-image:{gradient}(rgb(1 2 3),hsl(.5turn 20% 30%))}}"),
                    options,
                ),
                format!("a{{background-image:{gradient}(rgb(1,2, 3),hsl(180,20%, 30%))}}"),
                "{gradient}"
            );
        }

        assert_eq!(
            parse_and_print_with_parser_options(
                "a{background:border-box rgb(1 2 3/4%);\
                   box-shadow:0 0 rgb(1 2 3/4%),inset 1px 2px hsl(.5turn 20% 30%);\
                   text-shadow:0 0 rgb(1 2 3/4%);\
                   border-color:rgb(1 2 3) hsl(.5turn 20% 30%);\
                   --x:rgb(1 2 3/4%);\
                   unknown:rgb(1 2 3/4%)}\
                 b{background-image:linear-gradient(var(--x),rgb(1 2 3))}",
                options,
            ),
            "a{background:border-box rgba(1,2,3,0.04);\
               box-shadow:0 0 rgba(1,2,3,0.04),inset 1px 2px hsl(180,20%, 30%);\
               text-shadow:0 0 rgb(1 2 3/4%);\
               border-color:rgb(1 2 3) hsl(.5turn 20% 30%);\
               --x:rgb(1 2 3/4%);\
               unknown:rgb(1 2 3/4%)}\
             b{background-image:linear-gradient(var(--x),rgb(1 2 3))}"
        );
    }

    #[test]
    fn lowers_modern_rgb_hsl_malformed_angles_and_minification() {
        let options = Options {
            minify_whitespace: true,
            unsupported_css_features: CssFeature::MODERN_RGB_HSL,
            ..Options::default()
        };
        assert_eq!(
            parse_and_print_with_parser_options(
                "a{color:hsl(.5turn var(--x));\
                   background:hsl(.5turn,foo);\
                   border-color:hsl(.5turn foo bar baz);\
                   outline-color:hsl(.5turn,var(--x),3%);\
                   fill:rgb(var(--x) var(--y) var(--z));\
                   stroke:rgb(1px 2 3)}",
                options,
            ),
            "a{color:hsl(180 var(--x));\
               background:hsl(180,foo);\
               border-color:hsl(180 foo bar baz);\
               outline-color:hsl(180,var(--x),3%);\
               fill:rgb(var(--x) var(--y) var(--z));\
               stroke:rgb(1px,2, 3)}"
        );

        assert_eq!(
            parse_and_print_with_parser_options(
                "a{color:rgb(1 2 3/50%);outline-color:hsl(30 25% 50%/50%)}",
                Options {
                    minify_syntax: true,
                    unsupported_css_features: CssFeature::MODERN_RGB_HSL,
                    ..Options::default()
                },
            ),
            "a {\n  color: #01020380;\n  outline-color: #9f806080;\n}\n"
        );
        assert_eq!(
            parse_and_print_with_parser_options(
                "a{color:rgb(1 2 3/50%)}",
                Options {
                    minify_syntax: true,
                    minify_whitespace: true,
                    unsupported_css_features: CssFeature::MODERN_RGB_HSL | CssFeature::HEX_RGBA,
                    ..Options::default()
                },
            ),
            "a{color:rgba(1,2,3,.5)}"
        );
    }

    #[test]
    fn lowers_unsupported_inset_property() {
        let options = Options {
            unsupported_css_features: CssFeature::INSET_PROPERTY,
            ..Options::default()
        };
        assert_eq!(
            parse_and_print_with_parser_options(
                "a{inset:1px}b{inset:1px 2%}c{inset:1px 2% 3em}\
                 d{inset:1px 2% 3em 4}e{inset:5px!important}",
                options,
            ),
            "a {\n\
             \x20\x20top: 1px;\n\
             \x20\x20right: 1px;\n\
             \x20\x20bottom: 1px;\n\
             \x20\x20left: 1px;\n\
             }\n\
             b {\n\
             \x20\x20top: 1px;\n\
             \x20\x20right: 2%;\n\
             \x20\x20bottom: 1px;\n\
             \x20\x20left: 2%;\n\
             }\n\
             c {\n\
             \x20\x20top: 1px;\n\
             \x20\x20right: 2%;\n\
             \x20\x20bottom: 3em;\n\
             \x20\x20left: 2%;\n\
             }\n\
             d {\n\
             \x20\x20top: 1px;\n\
             \x20\x20right: 2%;\n\
             \x20\x20bottom: 3em;\n\
             \x20\x20left: 4;\n\
             }\n\
             e {\n\
             \x20\x20top: 5px !important;\n\
             \x20\x20right: 5px !important;\n\
             \x20\x20bottom: 5px !important;\n\
             \x20\x20left: 5px !important;\n\
             }\n"
        );

        assert_eq!(
            parse_and_print_with_parser_options(
                "a{inset:auto}b{inset:var(--x)}c{inset:calc(1px + 2px)}\
                 d{inset:1px 2px 3px 4px 5px}",
                options,
            ),
            "a {\n\
             \x20\x20inset: auto;\n\
             }\n\
             b {\n\
             \x20\x20inset: var(--x);\n\
             }\n\
             c {\n\
             \x20\x20inset: calc(1px + 2px);\n\
             }\n\
             d {\n\
             \x20\x20inset: 1px 2px 3px 4px 5px;\n\
             }\n"
        );

        assert_eq!(
            parse_and_print_with_parser_options(
                "a{inset:0px 1px 2px 3px}\
                 b{top:1px;right:2px;bottom:3px;left:4px}\
                 c{inset:auto}d{inset:calc(1px + 2px)}",
                Options {
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..options
                },
            ),
            "a{top:0;right:1px;bottom:2px;left:3px}\
             b{top:1px;right:2px;bottom:3px;left:4px}\
             c{inset:auto}d{top:3px;right:3px;bottom:3px;left:3px}"
        );

        assert_eq!(
            parse_and_print_with_parser_options("a{inset:1px 2px}", Options::default()),
            "a {\n  inset: 1px 2px;\n}\n"
        );
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
            "@media screen{@supports (display: grid){a{display:grid}}}"
        );
    }

    #[test]
    fn minifies_redundant_css_nesting_selectors() {
        assert_eq!(
            parse_and_print_with_options(
                ".parent { & > .child { color: red } & { background: blue } }",
                true,
                true,
            ),
            ".parent{>.child{color:red}background:#00f}"
        );
    }

    #[test]
    fn preserves_custom_property_and_slash_whitespace() {
        assert_eq!(
            parse_and_print(
                ":root { --gap: 10px; } .x { border-radius: 10px / 5px; }",
                true,
            ),
            ":root{--gap: 10px}.x{border-radius:10px / 5px}"
        );
        assert_eq!(
            parse_and_print_with_options(":root { --value:calc(1 + 2) }", true, false),
            ":root {\n  --value: 3 ;\n}\n"
        );
        assert_eq!(
            parse_and_print_with_options(":root { --value:calc(1 + 2) }", true, true),
            ":root{--value: 3 }"
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

    #[test]
    fn removes_duplicate_rules_across_css_files_back_to_front() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let earlier = parse(
            log.clone(),
            source("a { color: red } b { color: blue }"),
            Options::default(),
        );
        let later = parse(log.clone(), source("a { color: red }"), Options::default());
        assert!(log.done().is_empty());
        let mut remover = make_dead_rule_mangler(SymbolMap::default());
        let later_rules = remover.remove_dead_rules_in_place(2, later.rules, &later.import_records);
        let earlier_rules =
            remover.remove_dead_rules_in_place(1, earlier.rules, &earlier.import_records);
        assert_eq!(later_rules.len(), 1);
        assert_eq!(earlier_rules.len(), 1);
        let crate::internal::css_ast::RuleData::Selector(selector) = &earlier_rules[0].data else {
            panic!("expected selector");
        };
        assert_eq!(
            selector.selectors[0].selectors[0]
                .type_selector
                .as_ref()
                .expect("type selector")
                .name
                .text,
            "b"
        );
    }

    #[test]
    fn removes_css_rules_whose_selectors_can_never_match() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut tree = parse(
            log.clone(),
            source("a { color: green }"),
            Options::default(),
        );
        assert!(log.done().is_empty());
        let dead_rule = |kind| {
            crate::internal::css_ast::Rule {
            loc: crate::internal::logger::Loc::default(),
            data: crate::internal::css_ast::RuleData::Selector(
                crate::internal::css_ast::SelectorRule {
                    selectors: vec![crate::internal::css_ast::ComplexSelector {
                        selectors: vec![crate::internal::css_ast::CompoundSelector {
                            subclass_selectors: vec![
                                crate::internal::css_ast::SubclassSelector {
                                    data: crate::internal::css_ast::SubclassData::PseudoWithSelectorList(
                                        crate::internal::css_ast::PseudoClassWithSelectorList {
                                            kind,
                                            ..crate::internal::css_ast::PseudoClassWithSelectorList::default()
                                        },
                                    ),
                                    range: crate::internal::logger::Range::default(),
                                },
                            ],
                            ..crate::internal::css_ast::CompoundSelector::default()
                        }],
                    }],
                    ..crate::internal::css_ast::SelectorRule::default()
                },
            ),
        }
        };
        tree.rules
            .insert(0, dead_rule(crate::internal::css_ast::PseudoClassKind::Is));
        tree.rules.insert(
            1,
            dead_rule(crate::internal::css_ast::PseudoClassKind::Where),
        );
        let mut remover = make_dead_rule_mangler(SymbolMap::default());
        let rules = remover.remove_dead_rules_in_place(1, tree.rules, &tree.import_records);
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn lowers_media_range_queries_for_old_targets() {
        assert_eq!(
            parse_and_print_with_parser_options(
                "@media (width >= 100px), (height <= 200px), (width > 3px),\
                 (4px < width), (5px >= width), (width = 6px),\
                 (1/1 < aspect-ratio <= 16/9) {a{color:red}}",
                Options {
                    unsupported_css_features: CssFeature::MEDIA_RANGE,
                    ..Options::default()
                },
            ),
            "@media (min-width: 100px), (max-height: 200px), not (max-width: 3px), \
             not (max-width: 4px), (max-width: 5px), (width: 6px), \
             (not (max-aspect-ratio: 1/1)) and (max-aspect-ratio: 16/9) {\n\
             \x20\x20a {\n\
             \x20\x20\x20\x20color: red;\n\
             \x20\x20}\n\
             }\n"
        );
    }

    #[test]
    fn parses_nested_media_conditions_and_minifies_like_upstream() {
        let options = Options {
            minify_syntax: true,
            minify_whitespace: true,
            unsupported_css_features: CssFeature::MEDIA_RANGE,
            ..Options::default()
        };
        assert_eq!(
            parse_and_print_with_parser_options(
                "@media not (width > 10px),\
                 ((width > 1px) and (height < 2px)),\
                 screen and ((width >= 3px) or (height <= 4px)) {a{color:red}}",
                options,
            ),
            "@media(max-width:10px),(not (max-width:1px))and (not (min-height:2px)),\
             screen and ((min-width:3px)or (max-height:4px)){a{color:red}}"
        );
        assert_eq!(
            parse_and_print_with_parser_options(
                "@media (width >= calc(1px + 2px)),\
                 (width >= calc(2px * 3)) {a{color:red}}",
                Options {
                    minify_syntax: true,
                    unsupported_css_features: CssFeature::MEDIA_RANGE,
                    ..Options::default()
                },
            ),
            "@media (min-width: 3px), (min-width: 6px) {\n\
             \x20\x20a {\n\
             \x20\x20\x20\x20color: red;\n\
             \x20\x20}\n\
             }\n"
        );
    }

    #[test]
    fn preserves_supported_and_malformed_media_ranges() {
        assert_eq!(
            parse_and_print_with_parser_options(
                "@media (width >= 100px), (1px < width <= 2px),\
                 not (height > 3px) {a{color:red}}",
                Options {
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..Options::default()
                },
            ),
            "@media(width>=100px),(1px<width<=2px),(height<=3px){a{color:red}}"
        );
        assert_eq!(
            parse_and_print_with_parser_options(
                "@media (width < = 1px), (width >= 50%),\
                 (1px < width > 2px), (foo bar) {a{color:red}}",
                Options {
                    minify_syntax: true,
                    minify_whitespace: true,
                    unsupported_css_features: CssFeature::MEDIA_RANGE,
                    ..Options::default()
                },
            ),
            "@media (width < = 1px),(width >= 50%),\
             (1px < width > 2px),(foo bar){a{color:red}}"
        );
        assert_eq!(
            parse_and_print_with_parser_options(
                "@media junk(a, b), (width >= 1px) {a{color:red}}",
                Options {
                    minify_syntax: true,
                    minify_whitespace: true,
                    unsupported_css_features: CssFeature::MEDIA_RANGE,
                    ..Options::default()
                },
            ),
            "@media junk(a,b),(min-width:1px){a{color:red}}"
        );
    }

    #[test]
    fn parses_import_layers_supports_and_media_ranges() {
        let input = "@import \"a.css\" (width >= 1px);\
                     @import \"b.css\" layer (height < 2px);\
                     @import \"c.css\" layer(foo, bar) supports(display: grid) (3px <= width);";
        let options = Options {
            unsupported_css_features: CssFeature::MEDIA_RANGE,
            ..Options::default()
        };
        assert_eq!(
            parse_and_print_with_parser_options(input, options),
            "@import \"a.css\" (min-width: 1px);\n\
             @import \"b.css\" layer not (min-height: 2px);\n\
             @import \"c.css\" layer(foo, bar) supports(display: grid) (min-width: 3px);\n"
        );
        assert_eq!(
            parse_and_print_with_parser_options(
                input,
                Options {
                    minify_syntax: true,
                    minify_whitespace: true,
                    ..options
                },
            ),
            "@import\"a.css\"(min-width:1px);\
             @import\"b.css\"layer not (min-height:2px);\
             @import\"c.css\"layer(foo,bar) supports(display: grid) (min-width:3px);"
        );
    }
}
