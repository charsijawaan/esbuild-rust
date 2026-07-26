//! Port of upstream `internal/css_parser`.

use std::collections::HashMap;

use crate::internal::{
    ast::{CharFreq, ImportKind, ImportRecord, LocRef, Ref, Symbol, SymbolKind},
    css_ast::{
        Ast, AtCharsetRule, AtImportRule, AtKeyframesRule, AtLayerRule, AtMediaRule,
        BadDeclarationRule, ClassSelector, Combinator, ComplexSelector, Composes, CompoundSelector,
        DeclarationRule, HashSelector, ImportConditions, ImportedComposesName, KeyframeBlock,
        KnownAtRule, MediaArbitraryTokensQuery, MediaQuery, MediaQueryData, NameToken,
        NamespacedName, PseudoClassSelector, QualifiedRule, Rule, RuleData, SelectorRule,
        SubclassData, SubclassSelector, Token, UnknownAtRule, WhitespaceFlags,
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
    let rules = parser.parse_rule_list(false);
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
                    if let Some(rule) = self.parse_declaration() {
                        rules.push(rule);
                    }
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
                let rules = self.parse_rule_list(true);
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
                let rules = self.parse_rule_list(true);
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
                let rules = self.parse_rule_list(true);
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
            let rules = self.parse_rule_list(true);
            blocks.push(KeyframeBlock {
                selectors: keyframe_selectors(&selector_tokens),
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
        let rules = self.parse_rule_list(true);
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
