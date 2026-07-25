use std::ops::{BitOr, BitOrAssign};
use std::panic::panic_any;

use crate::internal::{
    ast::{AssertOrWithEntry, Index32},
    config::TsOptions,
    helpers::decode_wtf8_rune,
    js_ast,
    logger::{LineColumnTracker, Loc, Log, Msg, MsgData, MsgKind, Range, Source, Span},
};

use super::Token;

const END_OF_FILE: u32 = u32::MAX;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MaybeSubstring {
    pub string: Vec<u8>,
    pub start: Index32,
}

impl MaybeSubstring {
    #[must_use]
    pub fn from_allocated(string: Vec<u8>) -> Self {
        Self {
            string,
            start: Index32::default(),
        }
    }

    #[must_use]
    pub fn is_source_substring(&self) -> bool {
        self.start.is_valid()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum JsonFlavor {
    Json,
    TsConfigJson,
    #[default]
    NotJson,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CommentBefore(u8);

impl CommentBefore {
    pub const PURE: Self = Self(1);
    pub const KEY: Self = Self(2);
    pub const NO_SIDE_EFFECTS: Self = Self(4);

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for CommentBefore {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for CommentBefore {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum KeyOrValue {
    #[default]
    KeyRange,
    ValueRange,
    KeyAndValueRange,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IdentifierKind {
    Normal,
    Private,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LexerPanic;

#[allow(clippy::struct_excessive_bools)]
pub struct Lexer {
    pub legal_comments_before_token: Vec<Range>,
    pub comments_before_token: Vec<Range>,
    pub all_comments: Vec<Range>,
    pub identifier: MaybeSubstring,
    log: Log,
    pub source: Source,
    pub jsx_factory_pragma_comment: Span,
    pub jsx_fragment_pragma_comment: Span,
    pub jsx_runtime_pragma_comment: Span,
    pub jsx_import_source_pragma_comment: Span,
    pub source_mapping_url: Span,
    pub bad_arrow_in_tsx_suggestion: String,
    decoded_string_literal: Option<Vec<u16>>,
    encoded_string_literal_text: Vec<u8>,
    error_suffix: String,
    tracker: LineColumnTracker,
    encoded_string_literal_start: usize,
    pub number: f64,
    current: usize,
    start: usize,
    end: usize,
    pub approximate_newline_count: usize,
    pub could_be_bad_arrow_in_tsx: usize,
    pub bad_arrow_in_tsx_range: Range,
    pub legacy_octal_loc: Loc,
    pub await_keyword_loc: Loc,
    pub fn_or_arrow_start_loc: Loc,
    pub previous_backslash_quote_in_jsx: Range,
    pub legacy_html_comment_range: Range,
    code_point: u32,
    previous_error_loc: Loc,
    pub json: JsonFlavor,
    pub token: Token,
    pub ts: TsOptions,
    pub has_newline_before: bool,
    pub has_comment_before: CommentBefore,
    pub is_legacy_octal_literal: bool,
    pub previous_token_was_await_keyword: bool,
    rescan_close_brace_as_template_token: bool,
    for_global_name: bool,
    pub is_log_disabled: bool,
}

impl Lexer {
    fn uninitialized(
        log: Log,
        source: Source,
        ts: TsOptions,
        json: JsonFlavor,
        error_suffix: String,
        for_global_name: bool,
    ) -> Self {
        let tracker = LineColumnTracker::new(Some(&source));
        Self {
            legal_comments_before_token: Vec::new(),
            comments_before_token: Vec::new(),
            all_comments: Vec::new(),
            identifier: MaybeSubstring::default(),
            log,
            source,
            jsx_factory_pragma_comment: Span::default(),
            jsx_fragment_pragma_comment: Span::default(),
            jsx_runtime_pragma_comment: Span::default(),
            jsx_import_source_pragma_comment: Span::default(),
            source_mapping_url: Span::default(),
            bad_arrow_in_tsx_suggestion: String::new(),
            decoded_string_literal: None,
            encoded_string_literal_text: Vec::new(),
            error_suffix,
            tracker,
            encoded_string_literal_start: 0,
            number: 0.0,
            current: 0,
            start: 0,
            end: 0,
            approximate_newline_count: 0,
            could_be_bad_arrow_in_tsx: 0,
            bad_arrow_in_tsx_range: Range::default(),
            legacy_octal_loc: Loc::default(),
            await_keyword_loc: Loc::default(),
            fn_or_arrow_start_loc: Loc { start: -1 },
            previous_backslash_quote_in_jsx: Range::default(),
            legacy_html_comment_range: Range::default(),
            code_point: 0,
            previous_error_loc: Loc { start: -1 },
            json,
            token: Token::EndOfFile,
            ts,
            has_newline_before: false,
            has_comment_before: CommentBefore::default(),
            is_legacy_octal_literal: false,
            previous_token_was_await_keyword: false,
            rescan_close_brace_as_template_token: false,
            for_global_name,
            is_log_disabled: false,
        }
    }

    /// Create a JavaScript/TypeScript lexer and scan the first token.
    ///
    /// # Panics
    ///
    /// Panics with [`LexerPanic`] after logging malformed input.
    #[must_use]
    pub fn new(log: Log, source: Source, ts: TsOptions) -> Self {
        let mut lexer =
            Self::uninitialized(log, source, ts, JsonFlavor::NotJson, String::new(), false);
        lexer.step();
        lexer.next();
        lexer
    }

    /// Create the restricted lexer used for dotted global names.
    ///
    /// # Panics
    ///
    /// Panics with [`LexerPanic`] after logging malformed input.
    #[must_use]
    pub fn new_global_name(log: Log, source: Source) -> Self {
        let mut lexer = Self::uninitialized(
            log,
            source,
            TsOptions::default(),
            JsonFlavor::NotJson,
            String::new(),
            true,
        );
        lexer.step();
        lexer.next();
        lexer
    }

    /// Create a JSON or TypeScript configuration JSON lexer.
    ///
    /// # Panics
    ///
    /// Panics with [`LexerPanic`] after logging malformed input.
    #[must_use]
    pub fn new_json(
        log: Log,
        source: Source,
        json: JsonFlavor,
        error_suffix: impl Into<String>,
    ) -> Self {
        let mut lexer = Self::uninitialized(
            log,
            source,
            TsOptions::default(),
            json,
            error_suffix.into(),
            false,
        );
        lexer.step();
        lexer.next();
        lexer
    }

    /// # Panics
    ///
    /// Panics if the source offset does not fit in esbuild's 32-bit location.
    #[must_use]
    pub fn loc(&self) -> Loc {
        Loc {
            start: i32::try_from(self.start).expect("esbuild source offsets must fit in 32 bits"),
        }
    }

    /// # Panics
    ///
    /// Panics if the source range does not fit in esbuild's 32-bit locations.
    #[must_use]
    pub fn range(&self) -> Range {
        Range {
            loc: Loc {
                start: i32::try_from(self.start)
                    .expect("esbuild source offsets must fit in 32 bits"),
            },
            len: i32::try_from(self.end - self.start)
                .expect("esbuild source ranges must fit in 32 bits"),
        }
    }

    #[must_use]
    pub fn raw(&self) -> &[u8] {
        &self.source.contents[self.start..self.end]
    }

    fn raw_identifier(&self) -> MaybeSubstring {
        MaybeSubstring {
            string: self.raw().to_vec(),
            start: Index32::new(
                u32::try_from(self.start).expect("esbuild source offsets must fit in 32 bits"),
            ),
        }
    }

    #[must_use]
    pub fn is_identifier_or_keyword(&self) -> bool {
        self.token >= Token::Identifier
    }

    #[must_use]
    pub fn is_contextual_keyword(&self, text: &[u8]) -> bool {
        self.token == Token::Identifier && self.raw() == text
    }

    /// Consume a contextual keyword.
    ///
    /// # Panics
    ///
    /// Panics with [`LexerPanic`] after logging a mismatch.
    pub fn expect_contextual_keyword(&mut self, text: &[u8]) {
        if !self.is_contextual_keyword(text) {
            self.expected_string(&format!("{:?}", String::from_utf8_lossy(text)));
        }
        self.next();
    }

    /// Report that a particular description was expected.
    ///
    /// # Panics
    ///
    /// Always panics with [`LexerPanic`] after logging the diagnostic.
    pub fn expected_string(&mut self, text: &str) -> ! {
        if self.previous_token_was_await_keyword {
            let notes = if self.fn_or_arrow_start_loc.start == -1 {
                Vec::new()
            } else {
                let mut note = self.tracker.msg_data(
                    Range {
                        loc: self.fn_or_arrow_start_loc,
                        len: 0,
                    },
                    "Consider adding the \"async\" keyword here:",
                );
                if let Some(location) = &mut note.location {
                    "async".clone_into(&mut location.suggestion);
                }
                vec![note]
            };
            self.add_range_error_with_notes(
                range_of_identifier(&self.source, self.await_keyword_loc),
                "\"await\" can only be used inside an \"async\" function",
                notes,
            );
            panic_any(LexerPanic);
        }
        let found = if self.start == self.source.contents.len() {
            "end of file".to_owned()
        } else {
            format!("{:?}", String::from_utf8_lossy(self.raw()))
        };
        let suggestion = text
            .strip_prefix('"')
            .and_then(|text| text.strip_suffix('"'))
            .unwrap_or_default();
        self.add_range_error_with_suggestion(
            self.range(),
            format!("Expected {text}{} but found {found}", self.error_suffix),
            suggestion,
        );
        panic_any(LexerPanic);
    }

    /// Report that a token was expected.
    ///
    /// # Panics
    ///
    /// Always panics with [`LexerPanic`] after logging the diagnostic.
    pub fn expected(&mut self, token: Token) -> ! {
        self.expected_string(token.as_str())
    }

    /// Report the current token as unexpected.
    ///
    /// # Panics
    ///
    /// Always panics with [`LexerPanic`] after logging the diagnostic.
    pub fn unexpected(&mut self) -> ! {
        let found = if self.start == self.source.contents.len() {
            "end of file".to_owned()
        } else {
            format!("{:?}", String::from_utf8_lossy(self.raw()))
        };
        self.add_range_error(
            self.range(),
            format!("Unexpected {found}{}", self.error_suffix),
        );
        panic_any(LexerPanic);
    }

    /// Require and consume a token.
    ///
    /// # Panics
    ///
    /// Panics with [`LexerPanic`] after logging a mismatch.
    pub fn expect(&mut self, token: Token) {
        if self.token != token {
            self.expected(token);
        }
        self.next();
    }

    /// Consume a semicolon when automatic semicolon insertion does not apply.
    ///
    /// # Panics
    ///
    /// Panics with [`LexerPanic`] after logging a mismatch.
    pub fn expect_or_insert_semicolon(&mut self) {
        if self.token == Token::Semicolon
            || (!self.has_newline_before
                && self.token != Token::CloseBrace
                && self.token != Token::EndOfFile)
        {
            self.expect(Token::Semicolon);
        }
    }

    /// Consume one `<`, splitting a longer shift/comparison token if needed.
    ///
    /// # Panics
    ///
    /// Panics with [`LexerPanic`] after logging a mismatch.
    pub fn expect_less_than(&mut self, is_inside_jsx_element: bool) {
        match self.token {
            Token::LessThan => {
                if is_inside_jsx_element {
                    self.next_inside_jsx_element();
                } else {
                    self.next();
                }
            }
            Token::LessThanEquals => {
                self.token = Token::Equals;
                self.start += 1;
                self.maybe_expand_equals();
            }
            Token::LessThanLessThan => {
                self.token = Token::LessThan;
                self.start += 1;
            }
            Token::LessThanLessThanEquals => {
                self.token = Token::LessThanEquals;
                self.start += 1;
            }
            _ => self.expected(Token::LessThan),
        }
    }

    /// Consume one `>`, splitting a longer shift/comparison token if needed.
    ///
    /// # Panics
    ///
    /// Panics with [`LexerPanic`] after logging a mismatch.
    pub fn expect_greater_than(&mut self, is_inside_jsx_element: bool) {
        match self.token {
            Token::GreaterThan => {
                if is_inside_jsx_element {
                    self.next_inside_jsx_element();
                } else {
                    self.next();
                }
            }
            Token::GreaterThanEquals => {
                self.token = Token::Equals;
                self.start += 1;
                self.maybe_expand_equals();
            }
            Token::GreaterThanGreaterThan => {
                self.token = Token::GreaterThan;
                self.start += 1;
            }
            Token::GreaterThanGreaterThanEquals => {
                self.token = Token::GreaterThanEquals;
                self.start += 1;
            }
            Token::GreaterThanGreaterThanGreaterThan => {
                self.token = Token::GreaterThanGreaterThan;
                self.start += 1;
            }
            Token::GreaterThanGreaterThanGreaterThanEquals => {
                self.token = Token::GreaterThanGreaterThanEquals;
                self.start += 1;
            }
            _ => self.expected(Token::GreaterThan),
        }
    }

    fn maybe_expand_equals(&mut self) {
        if self.code_point == u32::from(b'>') {
            self.token = Token::EqualsGreaterThan;
            self.step();
        } else if self.code_point == u32::from(b'=') {
            self.token = Token::EqualsEquals;
            self.step();
            // This comparison intentionally mirrors upstream's token/rune
            // comparison, which currently makes this branch unreachable.
            if self.token as u8 == b'=' {
                self.token = Token::EqualsEqualsEquals;
                self.step();
            }
        }
    }

    /// Reinterpret the current `}` as a template middle or tail token.
    ///
    /// # Panics
    ///
    /// Panics with [`LexerPanic`] after logging a mismatch.
    pub fn rescan_close_brace_as_template_token(&mut self) {
        if self.token != Token::CloseBrace {
            self.expected(Token::CloseBrace);
        }
        self.rescan_close_brace_as_template_token = true;
        self.code_point = u32::from(b'`');
        self.current = self.end;
        self.end -= 1;
        self.next();
        self.rescan_close_brace_as_template_token = false;
    }

    /// Scan a regular expression after the parser has interpreted `/` as the
    /// start of a regular-expression literal.
    ///
    /// # Panics
    ///
    /// Panics with [`LexerPanic`] after logging malformed input.
    pub fn scan_reg_exp(&mut self) {
        loop {
            match char::from_u32(self.code_point) {
                Some('/') => {
                    self.step();
                    let mut bits = 0_u32;
                    while is_identifier_continue(self.code_point) {
                        let Some(flag) = char::from_u32(self.code_point) else {
                            self.syntax_error();
                        };
                        if matches!(flag, 'd' | 'g' | 'i' | 'm' | 's' | 'u' | 'v' | 'y') {
                            let bit = 1_u32 << (self.code_point - u32::from(b'a'));
                            if bit & bits != 0 {
                                self.add_duplicate_reg_exp_flag_error(flag);
                            } else {
                                bits |= bit;
                            }
                            self.step();
                        } else {
                            self.syntax_error();
                        }
                    }
                    return;
                }
                Some('[') => {
                    self.step();
                    while self.code_point != u32::from(b']') {
                        self.validate_and_step_reg_exp();
                    }
                    self.step();
                }
                _ => self.validate_and_step_reg_exp(),
            }
        }
    }

    /// Require a JSX child token and advance in JSX-child mode.
    ///
    /// # Panics
    ///
    /// Panics with [`LexerPanic`] after logging a mismatch.
    pub fn expect_jsx_element_child(&mut self, token: Token) {
        if self.token != token {
            self.expected(token);
        }
        self.next_jsx_element_child();
    }

    /// Advance while scanning JSX element children.
    #[allow(clippy::too_many_lines)]
    pub fn next_jsx_element_child(&mut self) {
        self.has_newline_before = false;
        let original_start = self.end;
        self.start = self.end;

        match char::from_u32(self.code_point) {
            None => self.token = Token::EndOfFile,
            Some('{') => self.single_character_token(Token::OpenBrace),
            Some('<') => self.single_character_token(Token::LessThan),
            Some(_) => {
                let mut needs_fixing = false;
                loop {
                    match char::from_u32(self.code_point) {
                        None | Some('{' | '<') => break,
                        Some('&' | '\r' | '\n' | '\u{2028}' | '\u{2029}') => {
                            needs_fixing = true;
                            self.step();
                        }
                        Some(character @ ('}' | '>')) => {
                            self.log_invalid_jsx_text_character(character);
                            self.step();
                        }
                        Some(character) => {
                            needs_fixing |= !character.is_ascii();
                            self.step();
                        }
                    }
                }
                self.token = Token::StringLiteral;
                let text = &self.source.contents[original_start..self.end];
                self.decoded_string_literal = Some(if needs_fixing {
                    fix_whitespace_and_decode_jsx_entities(text)
                } else {
                    text.iter().copied().map(u16::from).collect()
                });
            }
        }
    }

    fn log_invalid_jsx_text_character(&mut self, character: char) {
        let replacement = if character == '}' { "{'}'}" } else { "{'>'}" };
        let range = Range {
            loc: Loc {
                start: source_i32(self.end),
            },
            len: 1,
        };
        let mut data = self.tracker.msg_data(
            range,
            format!("The character \"{character}\" is not valid inside a JSX element"),
        );
        let (kind, notes) = if self.could_be_bad_arrow_in_tsx > 0
            && character == '>'
            && self.end > 0
            && self.source.contents[self.end - 1] == b'='
        {
            let mut note = self.tracker.msg_data(
                self.bad_arrow_in_tsx_range,
                "TypeScript's TSX syntax interprets arrow functions with a single generic type \
                 parameter as an opening JSX element. If you want it to be interpreted as an \
                 arrow function instead, you need to add a trailing comma after the type \
                 parameter to disambiguate:",
            );
            if let Some(location) = &mut note.location {
                location
                    .suggestion
                    .clone_from(&self.bad_arrow_in_tsx_suggestion);
            }
            (MsgKind::Error, vec![note])
        } else {
            if let Some(location) = &mut data.location {
                replacement.clone_into(&mut location.suggestion);
            }
            (
                if self.ts.parse {
                    MsgKind::Error
                } else {
                    MsgKind::Warning
                },
                vec![MsgData {
                    text: format!("Did you mean to escape it as {replacement:?} instead?"),
                    ..MsgData::default()
                }],
            )
        };
        self.log.add_msg(Msg {
            notes,
            data,
            ..Msg::new(kind, "")
        });
    }

    /// Require a token inside a JSX tag and advance in JSX-tag mode.
    ///
    /// # Panics
    ///
    /// Panics with [`LexerPanic`] after logging a mismatch.
    pub fn expect_inside_jsx_element(&mut self, token: Token) {
        if self.token != token {
            self.expected(token);
        }
        self.next_inside_jsx_element();
    }

    /// Advance while scanning inside a JSX opening or closing tag.
    ///
    /// # Panics
    ///
    /// Panics with [`LexerPanic`] after logging malformed input.
    #[allow(clippy::too_many_lines)]
    pub fn next_inside_jsx_element(&mut self) {
        self.has_newline_before = false;
        loop {
            self.start = self.end;
            match char::from_u32(self.code_point) {
                None => self.token = Token::EndOfFile,
                Some('\r' | '\n' | '\u{2028}' | '\u{2029}') => {
                    self.step();
                    self.has_newline_before = true;
                    continue;
                }
                Some('\t' | ' ') => {
                    self.step();
                    continue;
                }
                Some('.') => self.single_character_token(Token::Dot),
                Some(':') => self.single_character_token(Token::Colon),
                Some('=') => self.single_character_token(Token::Equals),
                Some('{') => self.single_character_token(Token::OpenBrace),
                Some('}') => self.single_character_token(Token::CloseBrace),
                Some('<') => self.single_character_token(Token::LessThan),
                Some('>') => self.single_character_token(Token::GreaterThan),
                Some('/') => {
                    self.step();
                    if self.code_point == u32::from(b'/') {
                        loop {
                            self.step();
                            if matches!(
                                char::from_u32(self.code_point),
                                None | Some('\r' | '\n' | '\u{2028}' | '\u{2029}')
                            ) {
                                break;
                            }
                        }
                        continue;
                    }
                    if self.code_point == u32::from(b'*') {
                        self.step();
                        loop {
                            match char::from_u32(self.code_point) {
                                Some('*') => {
                                    self.step();
                                    if self.code_point == u32::from(b'/') {
                                        self.step();
                                        break;
                                    }
                                }
                                Some('\r' | '\n' | '\u{2028}' | '\u{2029}') => {
                                    self.step();
                                    self.has_newline_before = true;
                                }
                                None => {
                                    self.add_range_error(
                                        Range {
                                            loc: self.loc(),
                                            len: 0,
                                        },
                                        "Expected \"*/\" to terminate multi-line comment",
                                    );
                                    panic_any(LexerPanic);
                                }
                                Some(_) => self.step(),
                            }
                        }
                        continue;
                    }
                    self.token = Token::Slash;
                }
                Some(quote @ ('\'' | '"')) => self.scan_jsx_attribute_string(quote),
                Some(character) if js_ast::is_whitespace(character) => {
                    self.step();
                    continue;
                }
                Some(character) if js_ast::is_identifier_start(character) => {
                    self.step();
                    while char::from_u32(self.code_point)
                        .is_some_and(|next| js_ast::is_identifier_continue(next) || next == '-')
                    {
                        self.step();
                    }
                    self.identifier = self.raw_identifier();
                    self.token = Token::Identifier;
                }
                Some(_) => {
                    self.end = self.current;
                    self.token = Token::SyntaxError;
                }
            }
            return;
        }
    }

    fn scan_jsx_attribute_string(&mut self, quote: char) {
        let mut backslash = Range::default();
        let mut needs_decode = false;
        self.step();
        loop {
            match char::from_u32(self.code_point) {
                None => self.syntax_error(),
                Some('&') => {
                    needs_decode = true;
                    self.step();
                }
                Some('\\') => {
                    backslash = Range {
                        loc: Loc {
                            start: source_i32(self.end),
                        },
                        len: 1,
                    };
                    self.step();
                    continue;
                }
                Some(character) if character == quote => {
                    if backslash.len > 0 {
                        backslash.len += 1;
                        self.previous_backslash_quote_in_jsx = backslash;
                    }
                    self.step();
                    break;
                }
                Some(character) => {
                    needs_decode |= !character.is_ascii();
                    self.step();
                }
            }
            backslash = Range::default();
        }
        self.token = Token::StringLiteral;
        let text = &self.source.contents[self.start + 1..self.end - 1];
        self.decoded_string_literal = Some(if needs_decode {
            decode_jsx_entities(text)
        } else {
            text.iter().copied().map(u16::from).collect()
        });
    }

    fn validate_and_step_reg_exp(&mut self) {
        if self.code_point == u32::from(b'\\') {
            self.step();
        }
        if matches!(
            char::from_u32(self.code_point),
            None | Some('\r' | '\n' | '\u{2028}' | '\u{2029}')
        ) {
            self.add_range_error(
                Range {
                    loc: Loc {
                        start: source_i32(self.end),
                    },
                    len: 0,
                },
                "Unterminated regular expression",
            );
            panic_any(LexerPanic);
        }
        self.step();
    }

    fn add_duplicate_reg_exp_flag_error(&mut self, flag: char) {
        let mut first = Range {
            loc: Loc {
                start: source_i32(self.start),
            },
            len: 1,
        };
        let duplicate = Range {
            loc: Loc {
                start: source_i32(self.end),
            },
            len: 1,
        };
        let duplicate_start =
            usize::try_from(duplicate.loc.start).expect("source locations are non-negative");
        while first.loc.start < duplicate.loc.start {
            let first_start =
                usize::try_from(first.loc.start).expect("source locations are non-negative");
            if self.source.contents[first_start]
                == u8::try_from(flag as u32).expect("regular expression flags are ASCII")
            {
                break;
            }
            first.loc.start += 1;
        }
        if usize::try_from(first.loc.start).expect("source locations are non-negative")
            >= duplicate_start
        {
            return;
        }
        let note = self
            .tracker
            .msg_data(first, format!("The first \"{flag}\" was here:"));
        self.log.add_error_with_notes(
            Some(&mut self.tracker),
            duplicate,
            format!("Duplicate flag \"{flag}\" in regular expression"),
            vec![note],
        );
    }

    /// Decode the current string literal to UTF-16.
    ///
    /// # Panics
    ///
    /// Panics with [`LexerPanic`] after logging an invalid escape sequence.
    #[must_use]
    pub fn string_literal(&mut self) -> &[u16] {
        if self.decoded_string_literal.is_none() {
            let text = self.encoded_string_literal_text.clone();
            match self.try_to_decode_escape_sequences(
                self.encoded_string_literal_start,
                &text,
                true,
            ) {
                Ok(decoded) => self.decoded_string_literal = Some(decoded),
                Err(end) => {
                    self.end = end;
                    self.syntax_error();
                }
            }
        }
        self.decoded_string_literal
            .as_deref()
            .expect("the decoded string was initialized")
    }

    /// Return the template's cooked UTF-16 value and normalized raw bytes.
    ///
    /// The cooked value is `None` for an invalid escape sequence, which maps
    /// to JavaScript's `undefined` value for tagged templates.
    #[must_use]
    pub fn cooked_and_raw_template_contents(&mut self) -> (Option<Vec<u16>>, Vec<u8>) {
        let (raw_start, raw_end) = match self.token {
            Token::NoSubstitutionTemplateLiteral | Token::TemplateTail => {
                (self.start + 1, self.end - 1)
            }
            Token::TemplateHead | Token::TemplateMiddle => (self.start + 1, self.end - 2),
            _ => (self.start, self.start),
        };
        let mut raw = self.source.contents[raw_start..raw_end].to_vec();
        if raw.contains(&b'\r') {
            let mut normalized = Vec::with_capacity(raw.len());
            let mut index = 0;
            while index < raw.len() {
                let mut byte = raw[index];
                index += 1;
                if byte == b'\r' {
                    if raw.get(index) == Some(&b'\n') {
                        index += 1;
                    }
                    byte = b'\n';
                }
                normalized.push(byte);
            }
            raw = normalized;
        }
        let cooked = self
            .try_to_decode_escape_sequences(self.start + 1, &raw, false)
            .ok();
        (cooked, raw)
    }

    /// Advance to the next context-free JavaScript token.
    ///
    /// Context-sensitive regular expressions, JSX children, and template
    /// rescanning are exposed by separate routines, matching upstream.
    ///
    /// # Panics
    ///
    /// Panics with [`LexerPanic`] after logging malformed input.
    #[allow(clippy::too_many_lines)]
    pub fn next(&mut self) {
        self.has_newline_before = self.end == 0;
        self.has_comment_before = CommentBefore::default();
        self.previous_token_was_await_keyword = false;
        self.legal_comments_before_token.clear();
        self.comments_before_token.clear();

        loop {
            self.start = self.end;
            self.token = Token::EndOfFile;

            let Some(character) = char::from_u32(self.code_point) else {
                if self.code_point == END_OF_FILE {
                    self.token = Token::EndOfFile;
                } else {
                    self.end = self.current;
                    self.token = Token::SyntaxError;
                }
                return;
            };

            match character {
                '#' => {
                    if self.start == 0 && self.source.contents.starts_with(b"#!") {
                        while !matches!(
                            char::from_u32(self.code_point),
                            Some('\r' | '\n' | '\u{2028}' | '\u{2029}') | None
                        ) {
                            self.step();
                        }
                        self.token = Token::Hashbang;
                        self.identifier = self.raw_identifier();
                    } else {
                        self.step();
                        if self.code_point == u32::from(b'\\') {
                            (self.identifier, _) =
                                self.scan_identifier_with_escapes(IdentifierKind::Private);
                        } else {
                            if !char::from_u32(self.code_point)
                                .is_some_and(js_ast::is_identifier_start)
                            {
                                self.syntax_error();
                            }
                            self.step();
                            while char::from_u32(self.code_point)
                                .is_some_and(js_ast::is_identifier_continue)
                            {
                                self.step();
                            }
                            if self.code_point == u32::from(b'\\') {
                                (self.identifier, _) =
                                    self.scan_identifier_with_escapes(IdentifierKind::Private);
                            } else {
                                self.identifier = self.raw_identifier();
                            }
                        }
                        self.token = Token::PrivateIdentifier;
                    }
                }
                '\r' | '\n' | '\u{2028}' | '\u{2029}' => {
                    self.step();
                    self.has_newline_before = true;
                    continue;
                }
                '\t' | ' ' => {
                    self.step();
                    continue;
                }
                '(' => self.single_character_token(Token::OpenParen),
                ')' => self.single_character_token(Token::CloseParen),
                '[' => self.single_character_token(Token::OpenBracket),
                ']' => self.single_character_token(Token::CloseBracket),
                '{' => self.single_character_token(Token::OpenBrace),
                '}' => self.single_character_token(Token::CloseBrace),
                ',' => self.single_character_token(Token::Comma),
                ':' => self.single_character_token(Token::Colon),
                ';' => self.single_character_token(Token::Semicolon),
                '@' => self.single_character_token(Token::At),
                '~' => self.single_character_token(Token::Tilde),
                '?' => {
                    self.step();
                    if self.code_point == u32::from(b'?') {
                        self.step();
                        if self.code_point == u32::from(b'=') {
                            self.step();
                            self.token = Token::QuestionQuestionEquals;
                        } else {
                            self.token = Token::QuestionQuestion;
                        }
                    } else if self.code_point == u32::from(b'.')
                        && self
                            .source
                            .contents
                            .get(self.current)
                            .is_some_and(|byte| !byte.is_ascii_digit())
                    {
                        self.step();
                        self.token = Token::QuestionDot;
                    } else {
                        self.token = Token::Question;
                    }
                }
                '%' => self.scan_one_or_equals(Token::Percent, Token::PercentEquals),
                '^' => self.scan_one_or_equals(Token::Caret, Token::CaretEquals),
                '&' => self.scan_pair_or_equals(
                    b'&',
                    Token::Ampersand,
                    Token::AmpersandEquals,
                    Token::AmpersandAmpersand,
                    Token::AmpersandAmpersandEquals,
                ),
                '|' => self.scan_pair_or_equals(
                    b'|',
                    Token::Bar,
                    Token::BarEquals,
                    Token::BarBar,
                    Token::BarBarEquals,
                ),
                '+' => self.scan_pair_or_equals(
                    b'+',
                    Token::Plus,
                    Token::PlusEquals,
                    Token::PlusPlus,
                    Token::PlusPlus,
                ),
                '-' => self.scan_pair_or_equals(
                    b'-',
                    Token::Minus,
                    Token::MinusEquals,
                    Token::MinusMinus,
                    Token::MinusMinus,
                ),
                '*' => self.scan_pair_or_equals(
                    b'*',
                    Token::Asterisk,
                    Token::AsteriskEquals,
                    Token::AsteriskAsterisk,
                    Token::AsteriskAsteriskEquals,
                ),
                '/' => {
                    self.step();
                    if self.for_global_name {
                        self.token = Token::Slash;
                    } else if self.code_point == u32::from(b'=') {
                        self.step();
                        self.token = Token::SlashEquals;
                    } else if self.code_point == u32::from(b'/') {
                        loop {
                            self.step();
                            if matches!(
                                char::from_u32(self.code_point),
                                Some('\r' | '\n' | '\u{2028}' | '\u{2029}') | None
                            ) {
                                break;
                            }
                        }
                        if self.json == JsonFlavor::Json {
                            self.add_range_error(self.range(), "JSON does not support comments");
                        }
                        self.scan_comment_text();
                        continue;
                    } else if self.code_point == u32::from(b'*') {
                        self.step();
                        loop {
                            match char::from_u32(self.code_point) {
                                Some('*') => {
                                    self.step();
                                    if self.code_point == u32::from(b'/') {
                                        self.step();
                                        break;
                                    }
                                }
                                Some('\r' | '\n' | '\u{2028}' | '\u{2029}') => {
                                    self.step();
                                    self.has_newline_before = true;
                                }
                                None => {
                                    self.add_range_error(
                                        Range {
                                            loc: self.loc(),
                                            len: 0,
                                        },
                                        "Expected \"*/\" to terminate multi-line comment",
                                    );
                                    panic_any(LexerPanic);
                                }
                                Some(_) => self.step(),
                            }
                        }
                        if self.json == JsonFlavor::Json {
                            self.add_range_error(self.range(), "JSON does not support comments");
                        }
                        self.scan_comment_text();
                        continue;
                    } else {
                        self.token = Token::Slash;
                    }
                }
                '=' => {
                    self.step();
                    if self.code_point == u32::from(b'>') {
                        self.step();
                        self.token = Token::EqualsGreaterThan;
                    } else if self.code_point == u32::from(b'=') {
                        self.step();
                        if self.code_point == u32::from(b'=') {
                            self.step();
                            self.token = Token::EqualsEqualsEquals;
                        } else {
                            self.token = Token::EqualsEquals;
                        }
                    } else {
                        self.token = Token::Equals;
                    }
                }
                '<' => self.scan_less_than(),
                '>' => self.scan_greater_than(),
                '!' => {
                    self.step();
                    if self.code_point == u32::from(b'=') {
                        self.step();
                        if self.code_point == u32::from(b'=') {
                            self.step();
                            self.token = Token::ExclamationEqualsEquals;
                        } else {
                            self.token = Token::ExclamationEquals;
                        }
                    } else {
                        self.token = Token::Exclamation;
                    }
                }
                '\'' | '"' | '`' => self.scan_string(character),
                '.' | '0'..='9' => self.parse_numeric_literal_or_dot(),
                '\\' => {
                    (self.identifier, self.token) =
                        self.scan_identifier_with_escapes(IdentifierKind::Normal);
                }
                _ if js_ast::is_whitespace(character) => {
                    self.step();
                    continue;
                }
                _ if js_ast::is_identifier_start(character) => self.scan_identifier(),
                _ => {
                    self.end = self.current;
                    self.token = Token::SyntaxError;
                }
            }
            return;
        }
    }

    fn single_character_token(&mut self, token: Token) {
        self.step();
        self.token = token;
    }

    fn scan_one_or_equals(&mut self, one: Token, equals: Token) {
        self.step();
        if self.code_point == u32::from(b'=') {
            self.step();
            self.token = equals;
        } else {
            self.token = one;
        }
    }

    fn scan_pair_or_equals(
        &mut self,
        pair: u8,
        one: Token,
        equals: Token,
        doubled: Token,
        doubled_equals: Token,
    ) {
        self.step();
        if self.code_point == u32::from(b'=') {
            self.step();
            self.token = equals;
        } else if self.code_point == u32::from(pair) {
            self.step();
            if self.code_point == u32::from(b'=')
                && doubled != Token::PlusPlus
                && doubled != Token::MinusMinus
            {
                self.step();
                self.token = doubled_equals;
            } else {
                self.token = doubled;
            }
        } else {
            self.token = one;
        }
    }

    fn scan_less_than(&mut self) {
        self.step();
        if self.code_point == u32::from(b'=') {
            self.step();
            self.token = Token::LessThanEquals;
        } else if self.code_point == u32::from(b'<') {
            self.step();
            if self.code_point == u32::from(b'=') {
                self.step();
                self.token = Token::LessThanLessThanEquals;
            } else {
                self.token = Token::LessThanLessThan;
            }
        } else {
            self.token = Token::LessThan;
        }
    }

    fn scan_greater_than(&mut self) {
        self.step();
        if self.code_point == u32::from(b'=') {
            self.step();
            self.token = Token::GreaterThanEquals;
        } else if self.code_point == u32::from(b'>') {
            self.step();
            if self.code_point == u32::from(b'=') {
                self.step();
                self.token = Token::GreaterThanGreaterThanEquals;
            } else if self.code_point == u32::from(b'>') {
                self.step();
                if self.code_point == u32::from(b'=') {
                    self.step();
                    self.token = Token::GreaterThanGreaterThanGreaterThanEquals;
                } else {
                    self.token = Token::GreaterThanGreaterThanGreaterThan;
                }
            } else {
                self.token = Token::GreaterThanGreaterThan;
            }
        } else {
            self.token = Token::GreaterThan;
        }
    }

    fn scan_string(&mut self, quote: char) {
        self.token = if quote == '`' {
            if self.rescan_close_brace_as_template_token {
                Token::TemplateTail
            } else {
                Token::NoSubstitutionTemplateLiteral
            }
        } else {
            Token::StringLiteral
        };
        self.step();
        let content_start = self.end;
        let mut needs_slow_path = false;
        let mut suffix_len = 1;
        loop {
            match char::from_u32(self.code_point) {
                Some('\\') => {
                    needs_slow_path = true;
                    self.step();
                    if self.code_point == END_OF_FILE {
                        self.unterminated_string();
                    }
                    self.step();
                }
                None => self.unterminated_string(),
                Some('\r' | '\n') if quote != '`' => self.unterminated_string(),
                Some('\r') => {
                    needs_slow_path = true;
                    self.step();
                }
                Some('$') if quote == '`' => {
                    self.step();
                    if self.code_point == u32::from(b'{') {
                        suffix_len = 2;
                        self.step();
                        self.token = if self.rescan_close_brace_as_template_token {
                            Token::TemplateMiddle
                        } else {
                            Token::TemplateHead
                        };
                        break;
                    }
                }
                Some(value) if value == quote => {
                    self.step();
                    break;
                }
                Some(value) => {
                    needs_slow_path |= !value.is_ascii();
                    if self.json == JsonFlavor::Json && value < '\u{20}' {
                        self.syntax_error();
                    }
                    self.step();
                }
            }
        }

        let content_end = self.end - suffix_len;
        let text = self.source.contents[content_start..content_end].to_vec();
        self.encoded_string_literal_start = content_start;
        self.encoded_string_literal_text.clone_from(&text);
        self.decoded_string_literal = if needs_slow_path {
            None
        } else {
            Some(text.into_iter().map(u16::from).collect())
        };
        if quote == '\'' && self.json != JsonFlavor::NotJson {
            self.add_range_error(self.range(), "JSON strings must use double quotes");
        }
    }

    fn unterminated_string(&mut self) -> ! {
        self.add_range_error(
            Range {
                loc: Loc {
                    start: i32::try_from(self.end)
                        .expect("esbuild source offsets must fit in 32 bits"),
                },
                len: 0,
            },
            "Unterminated string literal",
        );
        panic_any(LexerPanic);
    }

    fn scan_identifier(&mut self) {
        self.step();
        while char::from_u32(self.code_point).is_some_and(js_ast::is_identifier_continue) {
            self.step();
        }
        if self.code_point == u32::from(b'\\') {
            (self.identifier, self.token) =
                self.scan_identifier_with_escapes(IdentifierKind::Normal);
            return;
        }
        self.identifier = self.raw_identifier();
        self.token = super::keyword_token(
            std::str::from_utf8(self.raw()).expect("ASCII keywords are valid UTF-8"),
        )
        .unwrap_or(Token::Identifier);
    }

    fn scan_identifier_with_escapes(&mut self, kind: IdentifierKind) -> (MaybeSubstring, Token) {
        loop {
            if self.code_point == u32::from(b'\\') {
                self.step();
                if self.code_point != u32::from(b'u') {
                    self.syntax_error();
                }
                self.step();
                if self.code_point == u32::from(b'{') {
                    self.step();
                    while self.code_point != u32::from(b'}') {
                        if hex_value(self.code_point).is_none() {
                            self.syntax_error();
                        }
                        self.step();
                    }
                    self.step();
                } else {
                    for _ in 0..4 {
                        if hex_value(self.code_point).is_none() {
                            self.syntax_error();
                        }
                        self.step();
                    }
                }
                continue;
            }
            if !is_identifier_continue(self.code_point) {
                break;
            }
            self.step();
        }

        let raw = self.raw().to_vec();
        let decoded = match self.try_to_decode_escape_sequences(self.start, &raw, true) {
            Ok(decoded) => decoded,
            Err(end) => {
                self.end = end;
                self.syntax_error();
            }
        };
        let text = crate::internal::helpers::utf16_to_string(&decoded);
        let identifier = if kind == IdentifierKind::Private {
            text.get(1..).unwrap_or_default()
        } else {
            &text
        };
        if !is_identifier_bytes(identifier) {
            self.add_range_error(
                self.range(),
                format!("Invalid identifier: {:?}", String::from_utf8_lossy(&text)),
            );
        }
        let token = std::str::from_utf8(&text)
            .ok()
            .and_then(super::keyword_token)
            .map_or(Token::Identifier, |_| Token::EscapedKeyword);
        (MaybeSubstring::from_allocated(text), token)
    }

    #[allow(clippy::if_not_else, clippy::too_many_lines)]
    fn parse_numeric_literal_or_dot(&mut self) {
        let first = self.code_point;
        self.step();
        if first == u32::from(b'.') && !is_decimal_digit(self.code_point) {
            if self.code_point == u32::from(b'.')
                && self.source.contents.get(self.current) == Some(&b'.')
            {
                self.step();
                self.step();
                self.token = Token::DotDotDot;
            } else {
                self.token = Token::Dot;
            }
            return;
        }

        let mut underscore_count = 0;
        let mut last_underscore_end = None;
        let mut has_dot_or_exponent = first == u32::from(b'.');
        let mut is_missing_digit_after_dot = false;
        let mut base = 0_u32;
        self.is_legacy_octal_literal = false;
        self.token = Token::NumericLiteral;

        if first == u32::from(b'0') {
            match char::from_u32(self.code_point) {
                Some('b' | 'B') => base = 2,
                Some('o' | 'O') => base = 8,
                Some('x' | 'X') => base = 16,
                Some('0'..='7' | '_') => {
                    base = 8;
                    self.is_legacy_octal_literal = true;
                }
                Some('8' | '9') => self.is_legacy_octal_literal = true,
                _ => {}
            }
        }

        if base != 0 {
            let mut is_first = true;
            let mut is_invalid_legacy_octal_literal = false;
            self.number = 0.0;
            if !self.is_legacy_octal_literal {
                self.step();
            }
            loop {
                match char::from_u32(self.code_point) {
                    Some('_') => {
                        if last_underscore_end.is_some_and(|last| self.end == last + 1)
                            || is_first
                            || self.is_legacy_octal_literal
                        {
                            self.syntax_error();
                        }
                        last_underscore_end = Some(self.end);
                        underscore_count += 1;
                    }
                    Some('0' | '1') => {
                        self.number = self.number * f64::from(base)
                            + f64::from(self.code_point - u32::from(b'0'));
                    }
                    Some('2'..='7') => {
                        if base == 2 {
                            self.syntax_error();
                        }
                        self.number = self.number * f64::from(base)
                            + f64::from(self.code_point - u32::from(b'0'));
                    }
                    Some('8' | '9') => {
                        if self.is_legacy_octal_literal {
                            is_invalid_legacy_octal_literal = true;
                        } else if base < 10 {
                            self.syntax_error();
                        }
                        self.number = self.number * f64::from(base)
                            + f64::from(self.code_point - u32::from(b'0'));
                    }
                    Some('A'..='F') => {
                        if base != 16 {
                            self.syntax_error();
                        }
                        self.number = self.number * f64::from(base)
                            + f64::from(self.code_point + 10 - u32::from(b'A'));
                    }
                    Some('a'..='f') => {
                        if base != 16 {
                            self.syntax_error();
                        }
                        self.number = self.number * f64::from(base)
                            + f64::from(self.code_point + 10 - u32::from(b'a'));
                    }
                    _ => {
                        if is_first {
                            self.syntax_error();
                        }
                        break;
                    }
                }
                self.step();
                is_first = false;
            }

            let is_big_integer_literal = self.code_point == u32::from(b'n') && !has_dot_or_exponent;
            if is_big_integer_literal || is_invalid_legacy_octal_literal {
                let mut text = self.raw_identifier();
                if is_big_integer_literal && self.is_legacy_octal_literal {
                    self.syntax_error();
                }
                if underscore_count > 0 {
                    text = without_underscores(text);
                }
                if is_big_integer_literal {
                    self.identifier = text;
                } else {
                    self.number = parse_decimal_f64(&text.string);
                }
            }
        } else {
            let is_invalid_legacy_octal_literal = first == u32::from(b'0')
                && matches!(char::from_u32(self.code_point), Some('8' | '9'));

            loop {
                if !is_decimal_digit(self.code_point) {
                    if self.code_point != u32::from(b'_') {
                        break;
                    }
                    if last_underscore_end.is_some_and(|last| self.end == last + 1)
                        || is_invalid_legacy_octal_literal
                    {
                        self.syntax_error();
                    }
                    last_underscore_end = Some(self.end);
                    underscore_count += 1;
                }
                self.step();
            }

            if first != u32::from(b'.') && self.code_point == u32::from(b'.') {
                if last_underscore_end.is_some_and(|last| self.end == last + 1) {
                    self.end -= 1;
                    self.syntax_error();
                }
                has_dot_or_exponent = true;
                self.step();
                if self.code_point == u32::from(b'_') {
                    self.syntax_error();
                }
                is_missing_digit_after_dot = true;
                loop {
                    if is_decimal_digit(self.code_point) {
                        is_missing_digit_after_dot = false;
                    } else {
                        if self.code_point != u32::from(b'_') {
                            break;
                        }
                        if last_underscore_end.is_some_and(|last| self.end == last + 1) {
                            self.syntax_error();
                        }
                        last_underscore_end = Some(self.end);
                        underscore_count += 1;
                    }
                    self.step();
                }
            }

            if matches!(char::from_u32(self.code_point), Some('e' | 'E')) {
                if last_underscore_end.is_some_and(|last| self.end == last + 1) {
                    self.end -= 1;
                    self.syntax_error();
                }
                has_dot_or_exponent = true;
                self.step();
                if matches!(char::from_u32(self.code_point), Some('+' | '-')) {
                    self.step();
                }
                if !is_decimal_digit(self.code_point) {
                    self.syntax_error();
                }
                loop {
                    if !is_decimal_digit(self.code_point) {
                        if self.code_point != u32::from(b'_') {
                            break;
                        }
                        if last_underscore_end.is_some_and(|last| self.end == last + 1) {
                            self.syntax_error();
                        }
                        last_underscore_end = Some(self.end);
                        underscore_count += 1;
                    }
                    self.step();
                }
            }

            let mut text = self.raw_identifier();
            if underscore_count > 0 {
                text = without_underscores(text);
            }
            if self.code_point == u32::from(b'n') && !has_dot_or_exponent {
                if text.string.len() > 1 && first == u32::from(b'0') {
                    self.syntax_error();
                }
                self.identifier = text;
            } else if !has_dot_or_exponent && self.end - self.start < 10 {
                let mut number = 0_u32;
                for byte in text.string {
                    number = number * 10 + u32::from(byte - b'0');
                }
                self.number = f64::from(number);
            } else {
                self.number = parse_decimal_f64(&text.string);
            }
        }

        if last_underscore_end.is_some_and(|last| self.end == last + 1) {
            self.end -= 1;
            self.syntax_error();
        }
        if self.code_point == u32::from(b'n') && !has_dot_or_exponent {
            self.token = Token::BigIntegerLiteral;
            self.step();
        }
        if is_identifier_start(self.code_point) {
            self.syntax_error();
        }
        if self.json == JsonFlavor::Json
            && (first == u32::from(b'.')
                || base != 0
                || underscore_count > 0
                || is_missing_digit_after_dot)
        {
            self.unexpected();
        }
    }

    fn syntax_error(&mut self) -> ! {
        let (code_point, _) = decode_wtf8_rune(&self.source.contents[self.end..]);
        let character = char::from_u32(code_point);
        let message = match character {
            _ if self.end == self.source.contents.len() => "Unexpected end of file".to_owned(),
            None => format!("Syntax error \"\\u{{{code_point:x}}}\""),
            Some(character) if character < '\u{20}' => {
                format!("Syntax error \"\\x{:02X}\"", character as u32)
            }
            Some(character) if !character.is_ascii() => {
                format!("Syntax error \"\\u{{{:x}}}\"", character as u32)
            }
            Some('"') => "Syntax error '\"'".to_owned(),
            Some(character) => format!("Syntax error \"{character}\""),
        };
        self.add_range_error(
            Range {
                loc: Loc {
                    start: i32::try_from(self.end)
                        .expect("esbuild source offsets must fit in 32 bits"),
                },
                len: 0,
            },
            message,
        );
        panic_any(LexerPanic);
    }

    #[allow(clippy::too_many_lines)]
    fn try_to_decode_escape_sequences(
        &mut self,
        start: usize,
        text: &[u8],
        report_errors: bool,
    ) -> Result<Vec<u16>, usize> {
        let mut decoded = Vec::new();
        let mut index = 0;

        while index < text.len() {
            let (mut code_point, width) = decode_wtf8_rune(&text[index..]);
            index += width;

            if code_point == u32::from(b'\r') {
                if text.get(index) == Some(&b'\n') {
                    index += 1;
                }
                decoded.push(u16::from(b'\n'));
                continue;
            }

            if code_point == u32::from(b'\\') {
                let (escaped, escaped_width) = decode_wtf8_rune(&text[index..]);
                if escaped_width == 0 {
                    return Err(start + index);
                }
                index += escaped_width;
                match char::from_u32(escaped) {
                    Some('b') => {
                        decoded.push(u16::from(b'\x08'));
                        continue;
                    }
                    Some('f') => {
                        decoded.push(u16::from(b'\x0C'));
                        continue;
                    }
                    Some('n') => {
                        decoded.push(u16::from(b'\n'));
                        continue;
                    }
                    Some('r') => {
                        decoded.push(u16::from(b'\r'));
                        continue;
                    }
                    Some('t') => {
                        decoded.push(u16::from(b'\t'));
                        continue;
                    }
                    Some('v') => {
                        if self.json == JsonFlavor::Json {
                            return Err(start + index - escaped_width);
                        }
                        decoded.push(u16::from(b'\x0B'));
                        continue;
                    }
                    Some('0'..='7') => {
                        if self.json == JsonFlavor::Json {
                            return Err(start + index - escaped_width);
                        }
                        let octal_start = index - 2;
                        let mut value = escaped - u32::from(b'0');
                        let mut is_bad = false;
                        let (third, third_width) = decode_wtf8_rune(&text[index..]);
                        if (u32::from(b'0')..=u32::from(b'7')).contains(&third) {
                            value = value * 8 + third - u32::from(b'0');
                            index += third_width;
                            let (fourth, fourth_width) = decode_wtf8_rune(&text[index..]);
                            if (u32::from(b'0')..=u32::from(b'7')).contains(&fourth) {
                                let candidate = value * 8 + fourth - u32::from(b'0');
                                if candidate < 256 {
                                    value = candidate;
                                    index += fourth_width;
                                }
                            } else if matches!(char::from_u32(fourth), Some('8' | '9')) {
                                is_bad = true;
                            }
                        } else if matches!(char::from_u32(third), Some('8' | '9')) {
                            is_bad = true;
                        }
                        code_point = value;
                        if is_bad || &text[octal_start..index] != b"\\0" {
                            self.legacy_octal_loc = Loc {
                                start: source_i32(start + octal_start),
                            };
                        }
                    }
                    Some('8' | '9') => {
                        code_point = escaped;
                        self.legacy_octal_loc = Loc {
                            start: source_i32(start + index - 2),
                        };
                    }
                    Some('x') => {
                        if self.json == JsonFlavor::Json {
                            return Err(start + index - escaped_width);
                        }
                        code_point = 0;
                        for _ in 0..2 {
                            let (digit, digit_width) = decode_wtf8_rune(&text[index..]);
                            index += digit_width;
                            let Some(value) = hex_value(digit) else {
                                return Err(start + index - digit_width);
                            };
                            code_point = (code_point * 16) | value;
                        }
                    }
                    Some('u') => {
                        code_point = 0;
                        let (mut digit, mut digit_width) = decode_wtf8_rune(&text[index..]);
                        index += digit_width;
                        if digit == u32::from(b'{') {
                            if self.json == JsonFlavor::Json {
                                return Err(start + index - escaped_width);
                            }
                            let hex_start = index - width - escaped_width - digit_width;
                            let mut is_first = true;
                            let mut is_out_of_range = false;
                            loop {
                                (digit, digit_width) = decode_wtf8_rune(&text[index..]);
                                index += digit_width;
                                if digit == u32::from(b'}') {
                                    if is_first {
                                        return Err(start + index - digit_width);
                                    }
                                    break;
                                }
                                let Some(value) = hex_value(digit) else {
                                    return Err(start + index - digit_width);
                                };
                                code_point = code_point.saturating_mul(16) | value;
                                is_out_of_range |= code_point > 0x10_FFFF;
                                is_first = false;
                            }
                            if is_out_of_range && report_errors {
                                self.add_range_error(
                                    Range {
                                        loc: Loc {
                                            start: source_i32(start + hex_start),
                                        },
                                        len: source_i32(index - hex_start),
                                    },
                                    "Unicode escape sequence is out of range",
                                );
                                panic_any(LexerPanic);
                            }
                        } else {
                            for digit_index in 0..4 {
                                let Some(value) = hex_value(digit) else {
                                    return Err(start + index - digit_width);
                                };
                                code_point = (code_point * 16) | value;
                                if digit_index < 3 {
                                    (digit, digit_width) = decode_wtf8_rune(&text[index..]);
                                    index += digit_width;
                                }
                            }
                        }
                    }
                    Some('\r') => {
                        if self.json == JsonFlavor::Json {
                            return Err(start + index - escaped_width);
                        }
                        if text.get(index) == Some(&b'\n') {
                            index += 1;
                        }
                        continue;
                    }
                    Some('\n' | '\u{2028}' | '\u{2029}') => {
                        if self.json == JsonFlavor::Json {
                            return Err(start + index - escaped_width);
                        }
                        continue;
                    }
                    Some(value) => {
                        if self.json == JsonFlavor::Json && !matches!(value, '"' | '\\' | '/') {
                            return Err(start + index - escaped_width);
                        }
                        code_point = value as u32;
                    }
                    None => return Err(start + index - escaped_width),
                }
            }

            append_utf16(&mut decoded, code_point);
        }
        Ok(decoded)
    }

    fn add_range_error(&mut self, range: Range, text: impl Into<String>) {
        if range.loc == self.previous_error_loc {
            return;
        }
        self.previous_error_loc = range.loc;
        if !self.is_log_disabled {
            self.log
                .add_error(Some(&mut self.tracker), range, text.into());
        }
    }

    fn add_range_error_with_suggestion(
        &mut self,
        range: Range,
        text: impl Into<String>,
        suggestion: &str,
    ) {
        if range.loc == self.previous_error_loc {
            return;
        }
        self.previous_error_loc = range.loc;
        if !self.is_log_disabled {
            let mut data = self.tracker.msg_data(range, text);
            if let Some(location) = &mut data.location {
                suggestion.clone_into(&mut location.suggestion);
            }
            self.log.add_msg(Msg {
                data,
                ..Msg::new(MsgKind::Error, "")
            });
        }
    }

    /// Add one range error with attached notes, suppressing duplicates at the
    /// same source location.
    pub fn add_range_error_with_notes(
        &mut self,
        range: Range,
        text: impl Into<String>,
        notes: Vec<MsgData>,
    ) {
        if range.loc == self.previous_error_loc {
            return;
        }
        self.previous_error_loc = range.loc;
        if !self.is_log_disabled {
            self.log
                .add_error_with_notes(Some(&mut self.tracker), range, text.into(), notes);
        }
    }

    fn step(&mut self) {
        let (mut code_point, width) = decode_wtf8_rune(&self.source.contents[self.current..]);
        if width == 0 {
            code_point = END_OF_FILE;
        }
        if code_point == u32::from(b'\n') {
            self.approximate_newline_count += 1;
        }
        self.code_point = code_point;
        self.end = self.current;
        self.current += width;
    }

    fn scan_comment_text(&mut self) {
        let text = self.raw().to_vec();
        if text.len() < 2 {
            return;
        }
        let mut has_legal_annotation = text.len() > 2 && text[2] == b'!';
        let is_multi_line_comment = text[1] == b'*';
        let mut omit_from_general_comment_preservation = false;
        self.all_comments.push(self.range());
        let end_of_comment_text = text.len() - usize::from(is_multi_line_comment) * 2;

        for (index, byte) in text.iter().copied().enumerate() {
            if index + 1 > end_of_comment_text {
                break;
            }
            let rest = &text[index + 1..end_of_comment_text];
            match byte {
                b'#' => {
                    if has_prefix_with_word_boundary(rest, b"__PURE__") {
                        omit_from_general_comment_preservation = true;
                        self.has_comment_before |= CommentBefore::PURE;
                    } else if has_prefix_with_word_boundary(rest, b"__KEY__") {
                        omit_from_general_comment_preservation = true;
                        self.has_comment_before |= CommentBefore::KEY;
                    } else if has_prefix_with_word_boundary(rest, b"__NO_SIDE_EFFECTS__") {
                        omit_from_general_comment_preservation = true;
                        self.has_comment_before |= CommentBefore::NO_SIDE_EFFECTS;
                    } else if index == 2 {
                        self.scan_source_mapping_url(
                            index,
                            rest,
                            &mut omit_from_general_comment_preservation,
                        );
                    }
                }
                b'@' => {
                    if has_prefix_with_word_boundary(rest, b"__PURE__") {
                        omit_from_general_comment_preservation = true;
                        self.has_comment_before |= CommentBefore::PURE;
                    } else if has_prefix_with_word_boundary(rest, b"__KEY__") {
                        omit_from_general_comment_preservation = true;
                        self.has_comment_before |= CommentBefore::KEY;
                    } else if has_prefix_with_word_boundary(rest, b"__NO_SIDE_EFFECTS__") {
                        omit_from_general_comment_preservation = true;
                        self.has_comment_before |= CommentBefore::NO_SIDE_EFFECTS;
                    } else if has_prefix_with_word_boundary(rest, b"preserve")
                        || has_prefix_with_word_boundary(rest, b"license")
                    {
                        has_legal_annotation = true;
                    } else if let Some(span) = scan_for_pragma_arg(
                        PragmaArg::SkipSpaceFirst,
                        self.start + index + 1,
                        b"jsx",
                        rest,
                    ) {
                        self.jsx_factory_pragma_comment = span;
                    } else if let Some(span) = scan_for_pragma_arg(
                        PragmaArg::SkipSpaceFirst,
                        self.start + index + 1,
                        b"jsxFrag",
                        rest,
                    ) {
                        self.jsx_fragment_pragma_comment = span;
                    } else if let Some(span) = scan_for_pragma_arg(
                        PragmaArg::SkipSpaceFirst,
                        self.start + index + 1,
                        b"jsxRuntime",
                        rest,
                    ) {
                        self.jsx_runtime_pragma_comment = span;
                    } else if let Some(span) = scan_for_pragma_arg(
                        PragmaArg::SkipSpaceFirst,
                        self.start + index + 1,
                        b"jsxImportSource",
                        rest,
                    ) {
                        self.jsx_import_source_pragma_comment = span;
                    } else if index == 2 {
                        self.scan_source_mapping_url(
                            index,
                            rest,
                            &mut omit_from_general_comment_preservation,
                        );
                    }
                }
                _ => {}
            }
        }

        if has_legal_annotation {
            self.legal_comments_before_token.push(self.range());
        }
        if !omit_from_general_comment_preservation {
            self.comments_before_token.push(self.range());
        }
    }

    fn scan_source_mapping_url(&mut self, index: usize, rest: &[u8], omit: &mut bool) {
        let pragma = b" sourceMappingURL=";
        if let Some(span) = scan_for_pragma_arg(
            PragmaArg::NoSpaceFirst,
            self.start + index + 1,
            pragma,
            rest,
        ) {
            *omit = true;
            self.source_mapping_url = span;
        }
    }
}

/// Return the source range occupied by an identifier at `location`.
///
/// # Panics
///
/// Panics if `location` is negative or outside `source`.
#[must_use]
pub fn range_of_identifier(source: &Source, location: Loc) -> Range {
    let start = usize::try_from(location.start).expect("source locations are non-negative");
    let text = &source.contents[start..];
    if text.is_empty() {
        return Range {
            loc: location,
            len: 0,
        };
    }

    let mut index = 0;
    let (mut code_point, width) = decode_wtf8_rune(text);
    if code_point == u32::from(b'#') {
        index += width;
        (code_point, _) = decode_wtf8_rune(&text[index..]);
    }

    if is_identifier_start(code_point) || code_point == u32::from(b'\\') {
        while index < text.len() {
            let (next, next_width) = decode_wtf8_rune(&text[index..]);
            if next == u32::from(b'\\') {
                index += next_width;
                if index + 2 < text.len() && text[index] == b'u' && text[index + 1] == b'{' {
                    index += 2;
                    while index < text.len() {
                        let byte = text[index];
                        index += 1;
                        if byte == b'}' {
                            break;
                        }
                    }
                }
            } else if !is_identifier_continue(next) {
                return Range {
                    loc: location,
                    len: i32::try_from(index).expect("identifier lengths fit in 32 bits"),
                };
            } else {
                index += next_width;
            }
        }
    }

    source.range_of_string(location)
}

/// Return the key, value, or combined source range for an import attribute.
///
/// # Panics
///
/// Panics if either entry location is negative or outside `source`.
#[must_use]
pub fn range_of_import_assert_or_with(
    source: &Source,
    entry: &AssertOrWithEntry,
    which: KeyOrValue,
) -> Range {
    match which {
        KeyOrValue::KeyRange => range_of_identifier(source, entry.key_loc),
        KeyOrValue::ValueRange => source.range_of_string(entry.value_loc),
        KeyOrValue::KeyAndValueRange => {
            let key = range_of_identifier(source, entry.key_loc);
            let value = source.range_of_string(entry.value_loc);
            Range {
                loc: key.loc,
                len: value.end() - key.loc.start,
            }
        }
    }
}

fn is_identifier_start(code_point: u32) -> bool {
    char::from_u32(code_point).is_some_and(js_ast::is_identifier_start)
}

fn is_identifier_bytes(text: &[u8]) -> bool {
    let (first, first_width) = decode_wtf8_rune(text);
    if first_width == 0 || !is_identifier_start(first) {
        return false;
    }
    let mut index = first_width;
    while index < text.len() {
        let (code_point, width) = decode_wtf8_rune(&text[index..]);
        if width == 0 || !is_identifier_continue(code_point) {
            return false;
        }
        index += width;
    }
    true
}

fn is_decimal_digit(code_point: u32) -> bool {
    (u32::from(b'0')..=u32::from(b'9')).contains(&code_point)
}

fn without_underscores(mut text: MaybeSubstring) -> MaybeSubstring {
    text.string.retain(|byte| *byte != b'_');
    text.start = Index32::default();
    text
}

fn parse_decimal_f64(text: &[u8]) -> f64 {
    std::str::from_utf8(text)
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(f64::NAN)
}

fn source_i32(value: usize) -> i32 {
    i32::try_from(value).expect("esbuild source offsets must fit in 32 bits")
}

fn hex_value(code_point: u32) -> Option<u32> {
    match char::from_u32(code_point)? {
        '0'..='9' => Some(code_point - u32::from(b'0')),
        'a'..='f' => Some(code_point + 10 - u32::from(b'a')),
        'A'..='F' => Some(code_point + 10 - u32::from(b'A')),
        _ => None,
    }
}

fn append_utf16(decoded: &mut Vec<u16>, mut code_point: u32) {
    if code_point <= 0xFFFF {
        decoded.push(u16::try_from(code_point).expect("BMP code points fit in UTF-16"));
    } else {
        code_point -= 0x1_0000;
        decoded.push(
            u16::try_from((0xD800 + ((code_point >> 10) & 0x3FF)) & 0xFFFF)
                .expect("masked UTF-16 code units fit in u16"),
        );
        decoded.push(
            u16::try_from((0xDC00 + (code_point & 0x3FF)) & 0xFFFF)
                .expect("masked UTF-16 code units fit in u16"),
        );
    }
}

#[must_use]
pub fn decode_jsx_entities(text: &[u8]) -> Vec<u16> {
    let mut decoded = Vec::new();
    let mut index = 0;
    while index < text.len() {
        let (mut code_point, width) = decode_wtf8_rune(&text[index..]);
        index += width;
        if code_point == u32::from(b'&')
            && let Some(length) = text[index..].iter().position(|byte| *byte == b';')
            && length > 0
        {
            let entity = &text[index..index + length];
            let replacement = if let Some(number) = entity.strip_prefix(b"#") {
                let (number, radix) = number
                    .strip_prefix(b"x")
                    .map_or((number, 10), |number| (number, 16));
                std::str::from_utf8(number)
                    .ok()
                    .and_then(|number| u32::from_str_radix(number, radix).ok())
            } else {
                std::str::from_utf8(entity)
                    .ok()
                    .and_then(super::jsx_entity)
                    .map(u32::from)
            };
            if let Some(replacement) = replacement {
                code_point = replacement;
                index += length + 1;
            }
        }
        append_utf16(&mut decoded, code_point);
    }
    decoded
}

#[must_use]
pub fn fix_whitespace_and_decode_jsx_entities(text: &[u8]) -> Vec<u16> {
    let mut after_last_non_whitespace = None;
    let mut decoded = Vec::new();
    let mut index = 0;
    let mut first_non_whitespace = Some(0);

    while index < text.len() {
        let (code_point, width) = decode_wtf8_rune(&text[index..]);
        match char::from_u32(code_point) {
            Some('\r' | '\n' | '\u{2028}' | '\u{2029}') => {
                if let (Some(first), Some(after_last)) =
                    (first_non_whitespace, after_last_non_whitespace)
                {
                    if !decoded.is_empty() {
                        decoded.push(u16::from(b' '));
                    }
                    decoded.extend(decode_jsx_entities(&text[first..after_last]));
                }
                first_non_whitespace = None;
            }
            Some('\t' | ' ') => {}
            Some(character) if !js_ast::is_whitespace(character) => {
                after_last_non_whitespace = Some(index + width);
                first_non_whitespace.get_or_insert(index);
            }
            _ => {}
        }
        index += width;
    }

    if let Some(first) = first_non_whitespace {
        if !decoded.is_empty() {
            decoded.push(u16::from(b' '));
        }
        decoded.extend(decode_jsx_entities(&text[first..]));
    }
    decoded
}

fn is_identifier_continue(code_point: u32) -> bool {
    char::from_u32(code_point).is_some_and(js_ast::is_identifier_continue)
}

fn has_prefix_with_word_boundary(text: &[u8], prefix: &[u8]) -> bool {
    text.strip_prefix(prefix).is_some_and(|rest| {
        rest.is_empty() || {
            let (code_point, _) = decode_wtf8_rune(rest);
            !is_identifier_continue(code_point)
        }
    })
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PragmaArg {
    NoSpaceFirst,
    SkipSpaceFirst,
}

fn scan_for_pragma_arg(
    kind: PragmaArg,
    mut start: usize,
    pragma: &[u8],
    text: &[u8],
) -> Option<Span> {
    let mut text = text.strip_prefix(pragma)?;
    start += pragma.len();
    if text.is_empty() {
        return None;
    }

    let (mut code_point, mut width) = decode_wtf8_rune(text);
    if kind == PragmaArg::SkipSpaceFirst {
        if !is_whitespace(code_point) {
            return None;
        }
        while is_whitespace(code_point) {
            text = &text[width..];
            start += width;
            if text.is_empty() {
                return None;
            }
            (code_point, width) = decode_wtf8_rune(text);
        }
    }

    let mut index = 0;
    while !is_whitespace(code_point) {
        index += width;
        if index >= text.len() {
            break;
        }
        (code_point, width) = decode_wtf8_rune(&text[index..]);
    }

    Some(Span {
        text: String::from_utf8_lossy(&text[..index]).into_owned(),
        range: Range {
            loc: Loc {
                start: i32::try_from(start).expect("source offsets fit in 32 bits"),
            },
            len: i32::try_from(index).expect("pragma lengths fit in 32 bits"),
        },
    })
}

fn is_whitespace(code_point: u32) -> bool {
    char::from_u32(code_point).is_some_and(js_ast::is_whitespace)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::{
        CommentBefore, JsonFlavor, KeyOrValue, Lexer, MaybeSubstring, PragmaArg,
        decode_jsx_entities, fix_whitespace_and_decode_jsx_entities, has_prefix_with_word_boundary,
        range_of_identifier, scan_for_pragma_arg,
    };
    use crate::internal::js_lexer::Token;
    use crate::internal::{
        config::TsOptions,
        logger::{DeferLogKind, Loc, Log, Source},
    };

    fn source(text: &[u8]) -> Source {
        Source {
            contents: Arc::from(text),
            ..Source::default()
        }
    }

    #[test]
    fn maybe_substring_distinguishes_allocated_text() {
        let allocated = MaybeSubstring::from_allocated(b"name".to_vec());
        assert!(!allocated.is_source_substring());
    }

    #[test]
    fn comment_flags_compose() {
        let flags = CommentBefore::PURE | CommentBefore::NO_SIDE_EFFECTS;
        assert!(flags.contains(CommentBefore::PURE));
        assert!(!flags.contains(CommentBefore::KEY));
    }

    #[test]
    fn identifier_ranges_match_plain_private_and_escaped_names() {
        let source = source(b"hello.world #private x\\u{79}z;");
        assert_eq!(range_of_identifier(&source, Loc { start: 0 }).len, 5);
        assert_eq!(range_of_identifier(&source, Loc { start: 12 }).len, 8);
        assert_eq!(range_of_identifier(&source, Loc { start: 21 }).len, 8);
    }

    #[test]
    fn pragma_arguments_require_the_expected_boundary() {
        assert!(has_prefix_with_word_boundary(b"jsx x", b"jsx"));
        assert!(!has_prefix_with_word_boundary(b"jsxFactory", b"jsx"));
        let span = scan_for_pragma_arg(PragmaArg::SkipSpaceFirst, 10, b"jsx", b"jsx h next")
            .expect("pragma should scan");
        assert_eq!(span.text, "h");
        assert_eq!(span.range.loc.start, 14);
        assert_eq!(span.range.len, 1);
    }

    #[test]
    fn key_or_value_defaults_to_key() {
        assert_eq!(KeyOrValue::default(), KeyOrValue::KeyRange);
    }

    #[test]
    fn scans_common_context_free_tokens() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut lexer = Lexer::new(
            log,
            source(b"const answer = 40 + 2; // done\nanswer"),
            TsOptions::default(),
        );
        let mut tokens = Vec::new();
        loop {
            tokens.push(lexer.token);
            if lexer.token == Token::EndOfFile {
                break;
            }
            lexer.next();
        }
        assert_eq!(
            tokens,
            [
                Token::Const,
                Token::Identifier,
                Token::Equals,
                Token::NumericLiteral,
                Token::Plus,
                Token::NumericLiteral,
                Token::Semicolon,
                Token::Identifier,
                Token::EndOfFile,
            ]
        );
        assert_eq!(lexer.approximate_newline_count, 1);
    }

    #[test]
    fn scans_string_template_and_punctuation_tokens() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut lexer = Lexer::new(
            log,
            source(br#""hi" `head${ value }` ?. ??= === !== **="#),
            TsOptions::default(),
        );
        assert_eq!(lexer.token, Token::StringLiteral);
        assert_eq!(
            lexer.string_literal(),
            "hi".encode_utf16().collect::<Vec<_>>()
        );
        lexer.next();
        assert_eq!(lexer.token, Token::TemplateHead);
        lexer.next();
        assert_eq!(lexer.token, Token::Identifier);
        lexer.next();
        assert_eq!(lexer.token, Token::CloseBrace);
    }

    #[test]
    fn lazily_decodes_javascript_escape_sequences() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut lexer = Lexer::new(
            log,
            source(br#""a\n\x62\u0063\u{1F600}""#),
            TsOptions::default(),
        );
        assert_eq!(
            lexer.string_literal(),
            "a\nbc😀".encode_utf16().collect::<Vec<_>>()
        );
    }

    #[test]
    fn scans_escaped_identifiers_and_keywords() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut lexer = Lexer::new(
            log,
            source(br"\u0061bc v\u0061r #pr\u0069vate"),
            TsOptions::default(),
        );
        assert_eq!(lexer.token, Token::Identifier);
        assert_eq!(lexer.identifier.string, b"abc");
        assert!(!lexer.identifier.is_source_substring());
        lexer.next();
        assert_eq!(lexer.token, Token::EscapedKeyword);
        assert_eq!(lexer.identifier.string, b"var");
        lexer.next();
        assert_eq!(lexer.token, Token::PrivateIdentifier);
        assert_eq!(lexer.identifier.string, b"#private");
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn numeric_literals_match_upstream_forms() {
        let cases = [
            ("0", 0.0),
            ("000", 0.0),
            ("010", 8.0),
            ("0123", 83.0),
            ("0987.6543", 987.6543),
            ("0b00101", 5.0),
            ("0B101110", 46.0),
            ("0o12345", 5349.0),
            ("0x12345678", 305_419_896.0),
            ("123.", 123.0),
            (".0123", 0.0123),
            ("1e+1", 10.0),
            (".1e-1", 0.01),
            ("1_2.3_4e2", 1234.0),
            ("0b1_0", 2.0),
            ("0o1_2", 10.0),
            ("0x1_2", 18.0),
        ];
        for (text, expected) in cases {
            let log = Log::new_defer(DeferLogKind::All, HashMap::new());
            let lexer = Lexer::new(log, source(text.as_bytes()), TsOptions::default());
            assert_eq!(lexer.token, Token::NumericLiteral, "{text}");
            assert_eq!(lexer.number, expected, "{text}");
        }
    }

    #[test]
    fn bigint_literals_preserve_exact_text() {
        let cases = [
            ("0n", "0"),
            ("9007199254740993n", "9007199254740993"),
            ("0b1_0_1n", "0b101"),
            ("0o1_2_3n", "0o123"),
            ("0x1_2_3n", "0x123"),
        ];
        for (text, expected) in cases {
            let log = Log::new_defer(DeferLogKind::All, HashMap::new());
            let lexer = Lexer::new(log, source(text.as_bytes()), TsOptions::default());
            assert_eq!(lexer.token, Token::BigIntegerLiteral, "{text}");
            assert_eq!(lexer.identifier.string, expected.as_bytes(), "{text}");
        }
    }

    #[test]
    fn malformed_numeric_literals_raise_lexer_panic() {
        let cases = [
            "0b", "0b012", "0o018", "0xGF", "1e", "1e+", "1e+-1", "1z", "1__2", "1_", "1._",
            "0b_1", "0x1_", "1e2n", "1.0n", "000n",
        ];
        for text in cases {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let log = Log::new_defer(DeferLogKind::All, HashMap::new());
                Lexer::new(log, source(text.as_bytes()), TsOptions::default())
            }));
            assert!(result.is_err(), "{text}");
        }
    }

    #[test]
    fn scans_regular_expressions_and_flags() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut lexer = Lexer::new(
            log.clone(),
            source(br"/foo[\/\]]+/gim"),
            TsOptions::default(),
        );
        assert_eq!(lexer.token, Token::Slash);
        lexer.scan_reg_exp();
        assert_eq!(lexer.raw(), br"/foo[\/\]]+/gim");
        assert!(log.done().is_empty());
    }

    #[test]
    fn regular_expression_duplicate_flags_are_diagnostics() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut lexer = Lexer::new(log.clone(), source(b"/x/gg"), TsOptions::default());
        lexer.scan_reg_exp();
        let messages = log.done();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].data.text.contains("Duplicate flag \"g\""));
    }

    #[test]
    fn unterminated_regular_expressions_raise_lexer_panic() {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let log = Log::new_defer(DeferLogKind::All, HashMap::new());
            let mut lexer = Lexer::new(log, source(b"/unterminated"), TsOptions::default());
            lexer.scan_reg_exp();
        }));
        assert!(result.is_err());
    }

    #[test]
    fn scans_jsx_tag_tokens_attributes_and_children() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut lexer = Lexer::new(
            log,
            source(
                br#"<my-tag data-id="a&amp;b">  hello
              world  </my-tag>"#,
            ),
            TsOptions::default(),
        );
        assert_eq!(lexer.token, Token::LessThan);
        lexer.next_inside_jsx_element();
        assert_eq!(lexer.token, Token::Identifier);
        assert_eq!(lexer.raw(), b"my-tag");
        lexer.next_inside_jsx_element();
        assert_eq!(lexer.raw(), b"data-id");
        lexer.next_inside_jsx_element();
        assert_eq!(lexer.token, Token::Equals);
        lexer.next_inside_jsx_element();
        assert_eq!(lexer.token, Token::StringLiteral);
        assert_eq!(
            lexer.string_literal(),
            "a&b".encode_utf16().collect::<Vec<_>>()
        );
        lexer.next_inside_jsx_element();
        assert_eq!(lexer.token, Token::GreaterThan);
        lexer.next_jsx_element_child();
        assert_eq!(lexer.token, Token::StringLiteral);
        assert_eq!(
            lexer.string_literal(),
            "  hello world  ".encode_utf16().collect::<Vec<_>>()
        );
        lexer.next_jsx_element_child();
        assert_eq!(lexer.token, Token::LessThan);
    }

    #[test]
    fn invalid_jsx_text_delimiters_produce_diagnostics() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut lexer = Lexer::new(log.clone(), source(b"<x>bad > text<"), TsOptions::default());
        lexer.next_inside_jsx_element();
        lexer.next_inside_jsx_element();
        lexer.next_jsx_element_child();
        assert_eq!(lexer.token, Token::StringLiteral);
        assert_eq!(log.done().len(), 1);
    }

    #[test]
    fn splits_angle_bracket_tokens_for_the_parser() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut lexer = Lexer::new(log, source(b"<<= >>>= <=="), TsOptions::default());
        assert_eq!(lexer.token, Token::LessThanLessThanEquals);
        lexer.expect_less_than(false);
        assert_eq!(lexer.token, Token::LessThanEquals);
        lexer.expect_less_than(false);
        assert_eq!(lexer.token, Token::Equals);
        lexer.next();
        assert_eq!(lexer.token, Token::GreaterThanGreaterThanGreaterThanEquals);
        lexer.expect_greater_than(false);
        assert_eq!(lexer.token, Token::GreaterThanGreaterThanEquals);
        lexer.expect_greater_than(false);
        assert_eq!(lexer.token, Token::GreaterThanEquals);
        lexer.expect_greater_than(false);
        assert_eq!(lexer.token, Token::Equals);
        lexer.next();
        assert_eq!(lexer.token, Token::LessThanEquals);
        lexer.expect_less_than(false);
        assert_eq!(lexer.token, Token::EqualsEquals);
        assert_eq!(lexer.raw(), b"==");
    }

    #[test]
    fn rescans_template_middle_and_tail_tokens() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut lexer = Lexer::new(log, source(b"`head${value}tail`"), TsOptions::default());
        assert_eq!(lexer.token, Token::TemplateHead);
        lexer.next();
        assert_eq!(lexer.token, Token::Identifier);
        lexer.next();
        assert_eq!(lexer.token, Token::CloseBrace);
        lexer.rescan_close_brace_as_template_token();
        assert_eq!(lexer.token, Token::TemplateTail);
        assert_eq!(
            lexer.string_literal(),
            "tail".encode_utf16().collect::<Vec<_>>()
        );
    }

    #[test]
    fn json_rejects_non_json_escape_sequences_when_decoded() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let mut lexer = Lexer::new_json(log, source(br#""\x41""#), JsonFlavor::Json, "");
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = lexer.string_literal();
            }))
            .is_err()
        );
    }

    #[test]
    fn decodes_and_normalizes_jsx_text() {
        assert_eq!(
            decode_jsx_entities(b"&lt;x&#x3e;&#62;"),
            "<x>>".encode_utf16().collect::<Vec<_>>()
        );
        assert_eq!(
            fix_whitespace_and_decode_jsx_entities(b"  first  \n  second &amp; third  "),
            "  first second & third  "
                .encode_utf16()
                .collect::<Vec<_>>()
        );
    }
}
