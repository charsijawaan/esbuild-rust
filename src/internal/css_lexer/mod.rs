// Port of upstream internal/css_lexer.

use crate::internal::logger::{LineColumnTracker, Loc, Log, MsgId, MsgKind, Range, Source, Span};
use std::ops::{BitOr, BitOrAssign};
use std::sync::Arc;

const END_OF_FILE: i32 = -1;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum TokenKind {
    #[default]
    EndOfFile,
    AtKeyword,
    UnterminatedString,
    BadUrl,
    Cdc,
    Cdo,
    CloseBrace,
    CloseBracket,
    CloseParen,
    Colon,
    Comma,
    Delim,
    DelimAmpersand,
    DelimAsterisk,
    DelimBar,
    DelimCaret,
    DelimDollar,
    DelimDot,
    DelimEquals,
    DelimExclamation,
    DelimGreaterThan,
    DelimLessThan,
    DelimMinus,
    DelimPlus,
    DelimSlash,
    DelimTilde,
    Dimension,
    Function,
    Hash,
    Ident,
    Number,
    OpenBrace,
    OpenBracket,
    OpenParen,
    Percentage,
    Semicolon,
    String,
    Url,
    Whitespace,
    Symbol,
}

const TOKEN_TO_STRING: &[&str] = &[
    "end of file",
    "@-keyword",
    "bad string token",
    "bad URL token",
    "\"-->\"",
    "\"<!--\"",
    "\"}\"",
    "\"]\"",
    "\")\"",
    "\":\"",
    "\",\"",
    "delimiter",
    "\"&\"",
    "\"*\"",
    "\"|\"",
    "\"^\"",
    "\"$\"",
    "\".\"",
    "\"=\"",
    "\"!\"",
    "\">\"",
    "\"<\"",
    "\"-\"",
    "\"+\"",
    "\"/\"",
    "\"~\"",
    "dimension",
    "function token",
    "hash token",
    "identifier",
    "number",
    "\"{\"",
    "\"[\"",
    "\"(\"",
    "percentage",
    "\";\"",
    "string token",
    "URL token",
    "whitespace",
    "identifier",
];

impl TokenKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        TOKEN_TO_STRING[self as usize]
    }

    #[must_use]
    pub const fn is_numeric(self) -> bool {
        matches!(self, Self::Number | Self::Percentage | Self::Dimension)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TokenFlags(u8);

impl TokenFlags {
    pub const IS_ID: Self = Self(1 << 0);
    pub const DID_WARN_ABOUT_SINGLE_LINE_COMMENT: Self = Self(1 << 1);

    #[must_use]
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 != 0
    }
}

impl BitOr for TokenFlags {
    type Output = Self;

    fn bitor(self, right: Self) -> Self::Output {
        Self(self.0 | right.0)
    }
}

impl BitOrAssign for TokenFlags {
    fn bitor_assign(&mut self, right: Self) {
        self.0 |= right.0;
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Token {
    pub range: Range,
    pub unit_offset: u16,
    pub kind: TokenKind,
    pub flags: TokenFlags,
}

impl Token {
    /// # Panics
    ///
    /// Panics if this token's range is outside `contents`.
    #[must_use]
    pub fn decoded_text(self, contents: &[u8]) -> Vec<u8> {
        let raw = &contents[range_start(self.range)..range_end(self.range)];
        match self.kind {
            TokenKind::Ident | TokenKind::Dimension => decode_escapes_in_token(raw),
            TokenKind::AtKeyword | TokenKind::Hash => decode_escapes_in_token(&raw[1..]),
            TokenKind::Function => decode_escapes_in_token(&raw[..raw.len() - 1]),
            TokenKind::String => decode_escapes_in_token(&raw[1..raw.len() - 1]),
            TokenKind::Url => {
                let start = 4;
                let mut end = raw.len();
                if raw.last() == Some(&b')') {
                    end -= 1;
                }
                let mut start = start;
                while start < end && is_whitespace(i32::from(raw[start])) {
                    start += 1;
                }
                while start < end && is_whitespace(i32::from(raw[end - 1])) {
                    end -= 1;
                }
                decode_escapes_in_token(&raw[start..end])
            }
            _ => raw.to_vec(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Comment {
    pub text: Vec<u8>,
    pub loc: Loc,
    pub token_index_after: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TokenizeResult {
    pub tokens: Vec<Token>,
    pub all_comments: Vec<Range>,
    pub legal_comments: Vec<Comment>,
    pub source_map_comment: Span,
    pub approximate_line_count: i32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Options {
    pub record_all_comments: bool,
}

struct Lexer {
    options: Options,
    log: Log,
    source: Source,
    all_comments: Vec<Range>,
    legal_comments_before: Vec<Comment>,
    source_mapping_url: Span,
    tracker: LineColumnTracker,
    approximate_newline_count: i32,
    current: usize,
    old_single_line_comment_end: Loc,
    code_point: i32,
    token: Token,
}

/// # Panics
///
/// Panics if the source exceeds the signed 32-bit source-size limit.
#[must_use]
pub fn tokenize(log: Log, source: Source, options: Options) -> TokenizeResult {
    let tracker = LineColumnTracker::new(Some(&source));
    let mut lexer = Lexer {
        options,
        log,
        source,
        all_comments: Vec::new(),
        legal_comments_before: Vec::new(),
        source_mapping_url: Span::default(),
        tracker,
        approximate_newline_count: 0,
        current: 0,
        old_single_line_comment_end: Loc::default(),
        code_point: 0,
        token: Token::default(),
    };
    lexer.step();
    if lexer.code_point == 0xfeff {
        lexer.step();
    }

    lexer.next();
    let mut tokens = Vec::new();
    let mut legal_comments = Vec::new();
    while lexer.token.kind != TokenKind::EndOfFile {
        for mut comment in lexer.legal_comments_before.drain(..) {
            comment.token_index_after =
                u32::try_from(tokens.len()).expect("token count must fit in 32 bits");
            legal_comments.push(comment);
        }
        tokens.push(lexer.token);
        lexer.next();
    }
    for mut comment in lexer.legal_comments_before.drain(..) {
        comment.token_index_after =
            u32::try_from(tokens.len()).expect("token count must fit in 32 bits");
        legal_comments.push(comment);
    }

    TokenizeResult {
        tokens,
        all_comments: lexer.all_comments,
        legal_comments,
        source_map_comment: lexer.source_mapping_url,
        approximate_line_count: lexer.approximate_newline_count + 1,
    }
}

fn range_start(range: Range) -> usize {
    usize::try_from(range.loc.start).expect("source offsets must be non-negative")
}

fn range_end(range: Range) -> usize {
    usize::try_from(range.end()).expect("source offsets must be non-negative")
}

impl Lexer {
    fn step(&mut self) {
        let (mut code_point, width) = decode_rune(&self.source.contents[self.current..]);
        if width == 0 {
            code_point = END_OF_FILE;
        }
        if code_point == i32::from(b'\n') {
            self.approximate_newline_count += 1;
        }
        self.code_point = code_point;
        self.token.range.len = i32::try_from(self.current).expect("source must fit in 32 bits")
            - self.token.range.loc.start;
        self.current += width;
    }

    #[allow(clippy::too_many_lines)]
    fn next(&mut self) {
        loop {
            self.token = Token {
                range: Range {
                    loc: Loc {
                        start: self.token.range.end(),
                    },
                    len: 0,
                },
                ..Token::default()
            };

            match self.code_point {
                END_OF_FILE => self.token.kind = TokenKind::EndOfFile,

                value if value == i32::from(b'/') => {
                    self.step();
                    if self.code_point == i32::from(b'*') {
                        self.step();
                        self.consume_to_end_of_multi_line_comment(self.token.range);
                        continue;
                    }
                    if self.code_point == i32::from(b'/') {
                        let location = self.token.range.loc;
                        if location.start >= self.old_single_line_comment_end.start {
                            let contents = &self.source.contents;
                            let mut end = self.current;
                            while end < contents.len() && !is_newline(i32::from(contents[end])) {
                                end += 1;
                            }
                            self.log.add_id(
                                MsgId::CssJsCommentInCss,
                                MsgKind::Warning,
                                Some(&mut self.tracker),
                                Range {
                                    loc: location,
                                    len: 2,
                                },
                                "Comments in CSS use \"/* ... */\" instead of \"//\"",
                            );
                            self.old_single_line_comment_end.start =
                                i32::try_from(end).expect("source must fit in 32 bits");
                            self.token.flags |= TokenFlags::DID_WARN_ABOUT_SINGLE_LINE_COMMENT;
                        }
                    }
                    self.token.kind = TokenKind::DelimSlash;
                }

                value if is_whitespace(value) => {
                    self.step();
                    loop {
                        if is_whitespace(self.code_point) {
                            self.step();
                        } else if self.code_point == i32::from(b'/')
                            && self.current < self.source.contents.len()
                            && self.source.contents[self.current] == b'*'
                        {
                            let start_range = Range {
                                loc: Loc {
                                    start: self.token.range.end(),
                                },
                                len: 2,
                            };
                            self.step();
                            self.step();
                            self.consume_to_end_of_multi_line_comment(start_range);
                        } else {
                            break;
                        }
                    }
                    self.token.kind = TokenKind::Whitespace;
                }

                0x22 | 0x27 => {
                    self.token.kind = self.consume_string();
                }

                value if value == i32::from(b'#') => {
                    self.step();
                    if is_name_continue(self.code_point) || self.is_valid_escape() {
                        self.token.kind = TokenKind::Hash;
                        if self.would_start_identifier() {
                            self.token.flags |= TokenFlags::IS_ID;
                        }
                        self.consume_name();
                    } else {
                        self.token.kind = TokenKind::Delim;
                    }
                }

                value if value == i32::from(b'(') => {
                    self.step();
                    self.token.kind = TokenKind::OpenParen;
                }
                value if value == i32::from(b')') => {
                    self.step();
                    self.token.kind = TokenKind::CloseParen;
                }
                value if value == i32::from(b'[') => {
                    self.step();
                    self.token.kind = TokenKind::OpenBracket;
                }
                value if value == i32::from(b']') => {
                    self.step();
                    self.token.kind = TokenKind::CloseBracket;
                }
                value if value == i32::from(b'{') => {
                    self.step();
                    self.token.kind = TokenKind::OpenBrace;
                }
                value if value == i32::from(b'}') => {
                    self.step();
                    self.token.kind = TokenKind::CloseBrace;
                }
                value if value == i32::from(b',') => {
                    self.step();
                    self.token.kind = TokenKind::Comma;
                }
                value if value == i32::from(b':') => {
                    self.step();
                    self.token.kind = TokenKind::Colon;
                }
                value if value == i32::from(b';') => {
                    self.step();
                    self.token.kind = TokenKind::Semicolon;
                }

                value if value == i32::from(b'+') => {
                    if self.would_start_number() {
                        self.token.kind = self.consume_numeric();
                    } else {
                        self.step();
                        self.token.kind = TokenKind::DelimPlus;
                    }
                }
                value if value == i32::from(b'.') => {
                    if self.would_start_number() {
                        self.token.kind = self.consume_numeric();
                    } else {
                        self.step();
                        self.token.kind = TokenKind::DelimDot;
                    }
                }
                value if value == i32::from(b'-') => {
                    if self.would_start_number() {
                        self.token.kind = self.consume_numeric();
                    } else if self.current + 2 <= self.source.contents.len()
                        && &self.source.contents[self.current..self.current + 2] == b"->"
                    {
                        self.step();
                        self.step();
                        self.step();
                        self.token.kind = TokenKind::Cdc;
                    } else if self.would_start_identifier() {
                        self.token.kind = self.consume_ident_like();
                    } else {
                        self.step();
                        self.token.kind = TokenKind::DelimMinus;
                    }
                }
                value if value == i32::from(b'<') => {
                    if self.current + 3 <= self.source.contents.len()
                        && &self.source.contents[self.current..self.current + 3] == b"!--"
                    {
                        self.step();
                        self.step();
                        self.step();
                        self.step();
                        self.token.kind = TokenKind::Cdo;
                    } else {
                        self.step();
                        self.token.kind = TokenKind::DelimLessThan;
                    }
                }
                value if value == i32::from(b'@') => {
                    self.step();
                    if self.would_start_identifier() {
                        self.consume_name();
                        self.token.kind = TokenKind::AtKeyword;
                    } else {
                        self.token.kind = TokenKind::Delim;
                    }
                }
                value if value == i32::from(b'\\') => {
                    if self.is_valid_escape() {
                        self.token.kind = self.consume_ident_like();
                    } else {
                        self.step();
                        self.log.add_error(
                            Some(&mut self.tracker),
                            self.token.range,
                            "Invalid escape",
                        );
                        self.token.kind = TokenKind::Delim;
                    }
                }
                value if (i32::from(b'0')..=i32::from(b'9')).contains(&value) => {
                    self.token.kind = self.consume_numeric();
                }

                value if value == i32::from(b'>') => {
                    self.step();
                    self.token.kind = TokenKind::DelimGreaterThan;
                }
                value if value == i32::from(b'~') => {
                    self.step();
                    self.token.kind = TokenKind::DelimTilde;
                }
                value if value == i32::from(b'&') => {
                    self.step();
                    self.token.kind = TokenKind::DelimAmpersand;
                }
                value if value == i32::from(b'*') => {
                    self.step();
                    self.token.kind = TokenKind::DelimAsterisk;
                }
                value if value == i32::from(b'|') => {
                    self.step();
                    self.token.kind = TokenKind::DelimBar;
                }
                value if value == i32::from(b'!') => {
                    self.step();
                    self.token.kind = TokenKind::DelimExclamation;
                }
                value if value == i32::from(b'=') => {
                    self.step();
                    self.token.kind = TokenKind::DelimEquals;
                }
                value if value == i32::from(b'^') => {
                    self.step();
                    self.token.kind = TokenKind::DelimCaret;
                }
                value if value == i32::from(b'$') => {
                    self.step();
                    self.token.kind = TokenKind::DelimDollar;
                }
                _ => {
                    if is_name_start(self.code_point) {
                        self.token.kind = self.consume_ident_like();
                    } else {
                        self.step();
                        self.token.kind = TokenKind::Delim;
                    }
                }
            }
            return;
        }
    }

    fn consume_to_end_of_multi_line_comment(&mut self, start_range: Range) {
        let mut start_of_source_mapping_url = 0;
        let mut is_legal_comment = false;
        match self.code_point {
            0x23 | 0x40 => {
                if self.source.contents[self.current..].starts_with(b" sourceMappingURL=") {
                    start_of_source_mapping_url = self.current + b" sourceMappingURL=".len();
                }
            }
            value if value == i32::from(b'!') => is_legal_comment = true,
            _ => {}
        }

        loop {
            match self.code_point {
                value if value == i32::from(b'*') => {
                    let end_of_source_mapping_url = self.current - 1;
                    self.step();
                    if self.code_point == i32::from(b'/') {
                        let comment_end = self.current;
                        self.step();

                        if start_of_source_mapping_url != 0 {
                            let text = &self.source.contents
                                [start_of_source_mapping_url..end_of_source_mapping_url];
                            let mut range = Range {
                                loc: Loc {
                                    start: i32::try_from(start_of_source_mapping_url)
                                        .expect("source must fit in 32 bits"),
                                },
                                len: 0,
                            };
                            while usize::try_from(range.len)
                                .expect("range length must be non-negative")
                                < text.len()
                                && !is_whitespace(i32::from(
                                    text[usize::try_from(range.len)
                                        .expect("range length must be non-negative")],
                                ))
                            {
                                range.len += 1;
                            }
                            self.source_mapping_url = Span {
                                text: String::from_utf8_lossy(
                                    &text[..usize::try_from(range.len)
                                        .expect("range length must be non-negative")],
                                )
                                .into_owned(),
                                range,
                            };
                        }

                        let comment_range = Range {
                            loc: start_range.loc,
                            len: i32::try_from(comment_end).expect("source must fit in 32 bits")
                                - start_range.loc.start,
                        };
                        if self.options.record_all_comments {
                            self.all_comments.push(comment_range);
                        }

                        let raw = &self.source.contents[range_start(start_range)..comment_end];
                        if is_legal_comment || contains_at_preserve_or_at_license(raw) {
                            self.legal_comments_before.push(Comment {
                                loc: start_range.loc,
                                text: self.source.comment_text_without_indent(comment_range),
                                token_index_after: 0,
                            });
                        }
                        return;
                    }
                }
                END_OF_FILE => {
                    let note = self
                        .tracker
                        .msg_data(start_range, "The multi-line comment starts here:");
                    self.log.add_error_with_notes(
                        Some(&mut self.tracker),
                        Range {
                            loc: Loc {
                                start: self.token.range.end(),
                            },
                            len: 0,
                        },
                        "Expected \"*/\" to terminate multi-line comment",
                        vec![note],
                    );
                    return;
                }
                _ => self.step(),
            }
        }
    }

    fn is_valid_escape(&self) -> bool {
        if self.code_point != i32::from(b'\\') {
            return false;
        }
        let (next, _) = decode_rune(&self.source.contents[self.current..]);
        !is_newline(next)
    }

    fn would_start_identifier(&self) -> bool {
        if is_name_start(self.code_point) {
            return true;
        }
        if self.code_point == i32::from(b'-') {
            let (next, width) = decode_rune(&self.source.contents[self.current..]);
            if next == 0xfffd && width <= 1 {
                return false;
            }
            if is_name_start(next) || next == i32::from(b'-') {
                return true;
            }
            if next == i32::from(b'\\') {
                let (after, _) = decode_rune(&self.source.contents[self.current + width..]);
                return !is_newline(after);
            }
            return false;
        }
        self.is_valid_escape()
    }

    fn would_start_number(&self) -> bool {
        if (i32::from(b'0')..=i32::from(b'9')).contains(&self.code_point) {
            return true;
        }
        let contents = &self.source.contents;
        if self.code_point == i32::from(b'.') {
            return contents.get(self.current).is_some_and(u8::is_ascii_digit);
        }
        if matches!(self.code_point, 0x2b | 0x2d)
            && let Some(mut next) = contents.get(self.current).copied()
        {
            if next.is_ascii_digit() {
                return true;
            }
            if next == b'.' && self.current + 1 < contents.len() {
                next = contents[self.current + 1];
                return next.is_ascii_digit();
            }
        }
        false
    }

    fn consume_name(&mut self) -> Vec<u8> {
        let contents = Arc::clone(&self.source.contents);
        if is_name_continue(self.code_point) {
            let mut index = self.current;
            while index < contents.len() && is_name_continue(i32::from(contents[index])) {
                index += 1;
            }
            self.current = index;
            self.step();
        }
        let mut raw = contents[range_start(self.token.range)..range_end(self.token.range)].to_vec();
        if !self.is_valid_escape() {
            return raw;
        }

        push_code_point(&mut raw, self.consume_escape());
        loop {
            if is_name_continue(self.code_point) {
                push_code_point(&mut raw, self.code_point);
                self.step();
            } else if self.is_valid_escape() {
                push_code_point(&mut raw, self.consume_escape());
            } else {
                break;
            }
        }
        raw
    }

    fn consume_escape(&mut self) -> i32 {
        self.step();
        let code_point = self.code_point;
        if let Some(mut hex) = hex_value(code_point) {
            self.step();
            for _ in 0..5 {
                let Some(next) = hex_value(self.code_point) else {
                    break;
                };
                self.step();
                hex = hex * 16 + next;
            }
            if is_whitespace(self.code_point) {
                self.step();
            }
            if hex == 0 || (0xd800..=0xdfff).contains(&hex) || hex > 0x10_ffff {
                return 0xfffd;
            }
            return hex;
        }
        if code_point == END_OF_FILE {
            return 0xfffd;
        }
        self.step();
        code_point
    }

    fn consume_ident_like(&mut self) -> TokenKind {
        let name = self.consume_name();
        if self.code_point == i32::from(b'(') {
            let matching_location = Loc {
                start: self.token.range.end(),
            };
            self.step();
            if name.len() == 3
                && name[0].eq_ignore_ascii_case(&b'u')
                && name[1].eq_ignore_ascii_case(&b'r')
                && name[2].eq_ignore_ascii_case(&b'l')
            {
                let approximate_newline_count = self.approximate_newline_count;
                let code_point = self.code_point;
                let token_range_len = self.token.range.len;
                let current = self.current;
                while is_whitespace(self.code_point) {
                    self.step();
                }
                if !matches!(self.code_point, 0x22 | 0x27) {
                    return self.consume_url(matching_location);
                }
                self.approximate_newline_count = approximate_newline_count;
                self.code_point = code_point;
                self.token.range.len = token_range_len;
                self.current = current;
            }
            return TokenKind::Function;
        }
        TokenKind::Ident
    }

    #[allow(clippy::too_many_lines)]
    fn consume_url(&mut self, matching_location: Loc) -> TokenKind {
        let bad_url = loop {
            match self.code_point {
                value if value == i32::from(b')') => {
                    self.step();
                    return TokenKind::Url;
                }
                END_OF_FILE => {
                    let location = Loc {
                        start: self.token.range.end(),
                    };
                    let note = self.tracker.msg_data(
                        Range {
                            loc: matching_location,
                            len: 1,
                        },
                        "The unbalanced \"(\" is here:",
                    );
                    self.log.add_id_with_notes(
                        MsgId::CssSyntaxError,
                        MsgKind::Warning,
                        Some(&mut self.tracker),
                        Range {
                            loc: location,
                            len: 0,
                        },
                        "Expected \")\" to end URL token",
                        vec![note],
                    );
                    return TokenKind::Url;
                }
                value if is_whitespace(value) => {
                    self.step();
                    while is_whitespace(self.code_point) {
                        self.step();
                    }
                    if self.code_point != i32::from(b')') {
                        let location = Loc {
                            start: self.token.range.end(),
                        };
                        let note = self.tracker.msg_data(
                            Range {
                                loc: matching_location,
                                len: 1,
                            },
                            "The unbalanced \"(\" is here:",
                        );
                        self.log.add_id_with_notes(
                            MsgId::CssSyntaxError,
                            MsgKind::Warning,
                            Some(&mut self.tracker),
                            Range {
                                loc: location,
                                len: 0,
                            },
                            "Expected \")\" to end URL token",
                            vec![note],
                        );
                        if self.code_point == END_OF_FILE {
                            return TokenKind::Url;
                        }
                        break true;
                    }
                    self.step();
                    return TokenKind::Url;
                }
                0x22 | 0x27 | 0x28 => {
                    let range = Range {
                        loc: Loc {
                            start: self.token.range.end(),
                        },
                        len: 1,
                    };
                    let note = self.tracker.msg_data(
                        Range {
                            loc: matching_location,
                            len: 1,
                        },
                        "The unbalanced \"(\" is here:",
                    );
                    self.log.add_id_with_notes(
                        MsgId::CssSyntaxError,
                        MsgKind::Warning,
                        Some(&mut self.tracker),
                        range,
                        "Expected \")\" to end URL token",
                        vec![note],
                    );
                    break true;
                }
                value if value == i32::from(b'\\') => {
                    if !self.is_valid_escape() {
                        let range = Range {
                            loc: Loc {
                                start: self.token.range.end(),
                            },
                            len: 1,
                        };
                        self.log.add_id(
                            MsgId::CssSyntaxError,
                            MsgKind::Warning,
                            Some(&mut self.tracker),
                            range,
                            "Invalid escape",
                        );
                        break true;
                    }
                    self.consume_escape();
                }
                value if is_non_printable(value) => {
                    let range = Range {
                        loc: Loc {
                            start: self.token.range.end(),
                        },
                        len: 1,
                    };
                    self.log.add_id(
                        MsgId::CssSyntaxError,
                        MsgKind::Warning,
                        Some(&mut self.tracker),
                        range,
                        "Unexpected non-printable character in URL token",
                    );
                    break true;
                }
                _ => self.step(),
            }
        };
        debug_assert!(bad_url);

        loop {
            match self.code_point {
                value if value == i32::from(b')') || value == END_OF_FILE => {
                    self.step();
                    return TokenKind::BadUrl;
                }
                value if value == i32::from(b'\\') && self.is_valid_escape() => {
                    self.consume_escape();
                }
                _ => {}
            }
            self.step();
        }
    }

    fn consume_string(&mut self) -> TokenKind {
        let quote = self.code_point;
        self.step();
        loop {
            match self.code_point {
                value if value == i32::from(b'\\') => {
                    self.step();
                    if self.code_point == i32::from(b'\r') {
                        self.step();
                        if self.code_point == i32::from(b'\n') {
                            self.step();
                        }
                        continue;
                    }
                }
                END_OF_FILE | 0x0a | 0x0d | 0x0c => {
                    self.log.add_id(
                        MsgId::CssSyntaxError,
                        MsgKind::Warning,
                        Some(&mut self.tracker),
                        Range {
                            loc: Loc {
                                start: self.token.range.end(),
                            },
                            len: 0,
                        },
                        "Unterminated string token",
                    );
                    return TokenKind::UnterminatedString;
                }
                value if value == quote => {
                    self.step();
                    return TokenKind::String;
                }
                _ => {}
            }
            self.step();
        }
    }

    fn consume_numeric(&mut self) -> TokenKind {
        if matches!(self.code_point, 0x2b | 0x2d) {
            self.step();
        }
        while (i32::from(b'0')..=i32::from(b'9')).contains(&self.code_point) {
            self.step();
        }
        if self.code_point == i32::from(b'.') {
            self.step();
            while (i32::from(b'0')..=i32::from(b'9')).contains(&self.code_point) {
                self.step();
            }
        }
        if matches!(self.code_point, 0x45 | 0x65) {
            let contents = &self.source.contents;
            if self.current < contents.len() {
                let mut next = contents[self.current];
                if matches!(next, b'+' | b'-') && self.current + 1 < contents.len() {
                    next = contents[self.current + 1];
                }
                if next.is_ascii_digit() {
                    self.step();
                    if matches!(self.code_point, 0x2b | 0x2d) {
                        self.step();
                    }
                    while (i32::from(b'0')..=i32::from(b'9')).contains(&self.code_point) {
                        self.step();
                    }
                }
            }
        }

        if self.would_start_identifier() {
            let bytes = self.token.range.len.to_le_bytes();
            self.token.unit_offset = u16::from_le_bytes([bytes[0], bytes[1]]);
            self.consume_name();
            return TokenKind::Dimension;
        }
        if self.code_point == i32::from(b'%') {
            self.step();
            return TokenKind::Percentage;
        }
        TokenKind::Number
    }
}

fn contains_at_preserve_or_at_license(text: &[u8]) -> bool {
    text.iter().enumerate().any(|(index, byte)| {
        *byte == b'@'
            && (text[index + 1..].starts_with(b"preserve")
                || text[index + 1..].starts_with(b"license"))
    })
}

#[must_use]
pub fn would_start_identifier_without_escapes(text: &[u8]) -> bool {
    let (first, width) = decode_rune(text);
    if first == 0xfffd && width <= 1 {
        return false;
    }
    if is_name_start(first) {
        return true;
    }
    if first == i32::from(b'-') {
        let (second, second_width) = decode_rune(&text[width..]);
        if second == 0xfffd && second_width <= 1 {
            return false;
        }
        return is_name_start(second) || second == i32::from(b'-');
    }
    false
}

/// # Panics
///
/// Panics if `location` is outside the source or the source exceeds the signed
/// 32-bit source-size limit.
#[must_use]
pub fn range_of_identifier(source: &Source, location: Loc) -> Range {
    let start = usize::try_from(location.start).expect("source locations must be non-negative");
    let text = &source.contents[start..];
    if text.is_empty() {
        return Range {
            loc: location,
            len: 0,
        };
    }

    let mut index = 0;
    while index < text.len() {
        let (character, width) = decode_rune(&text[index..]);
        if is_name_continue(character) {
            index += width;
            continue;
        }

        if character == i32::from(b'\\')
            && index + 1 < text.len()
            && !is_newline(i32::from(text[index + 1]))
        {
            index += width;
            let (escaped, escaped_width) = decode_rune(&text[index..]);
            if hex_value(escaped).is_some() {
                index += escaped_width;
                for _ in 0..5 {
                    if index >= text.len() {
                        break;
                    }
                    let (next, next_width) = decode_rune(&text[index..]);
                    if hex_value(next).is_none() {
                        break;
                    }
                    index += next_width;
                }
                if index < text.len() {
                    let (next, next_width) = decode_rune(&text[index..]);
                    if is_whitespace(next) {
                        index += next_width;
                    }
                }
            }
            continue;
        }
        break;
    }

    if index > 0 && is_whitespace(i32::from(text[index - 1])) {
        index -= 1;
    }
    Range {
        loc: location,
        len: i32::try_from(index).expect("source must fit in 32 bits"),
    }
}

#[must_use]
pub const fn is_name_start(character: i32) -> bool {
    (character >= b'a' as i32 && character <= b'z' as i32)
        || (character >= b'A' as i32 && character <= b'Z' as i32)
        || character == b'_' as i32
        || character >= 0x80
        || character == 0
}

#[must_use]
pub const fn is_name_continue(character: i32) -> bool {
    is_name_start(character)
        || (character >= b'0' as i32 && character <= b'9' as i32)
        || character == b'-' as i32
}

const fn is_newline(character: i32) -> bool {
    matches!(character, 0x0a | 0x0d | 0x0c)
}

const fn is_whitespace(character: i32) -> bool {
    matches!(character, 0x20 | 0x09 | 0x0a | 0x0d | 0x0c)
}

const fn hex_value(character: i32) -> Option<i32> {
    if character >= b'0' as i32 && character <= b'9' as i32 {
        Some(character - b'0' as i32)
    } else if character >= b'a' as i32 && character <= b'f' as i32 {
        Some(character + (10 - b'a' as i32))
    } else if character >= b'A' as i32 && character <= b'F' as i32 {
        Some(character + (10 - b'A' as i32))
    } else {
        None
    }
}

const fn is_non_printable(character: i32) -> bool {
    character <= 0x08
        || character == 0x0b
        || (character >= 0x0e && character <= 0x1f)
        || character == 0x7f
}

fn decode_escapes_in_token(inner: &[u8]) -> Vec<u8> {
    let mut index = 0;
    while index < inner.len() {
        if matches!(inner[index], b'\\' | 0) {
            break;
        }
        index += 1;
    }
    if index == inner.len() {
        return inner.to_vec();
    }

    let mut output = inner[..index].to_vec();
    let mut remaining = &inner[index..];
    while !remaining.is_empty() {
        let (mut character, width) = decode_rune(remaining);
        remaining = &remaining[width..];
        if character != i32::from(b'\\') {
            if character == 0 {
                character = 0xfffd;
            }
            push_code_point(&mut output, character);
            continue;
        }

        if remaining.is_empty() {
            push_code_point(&mut output, 0xfffd);
            continue;
        }
        (character, index) = decode_rune(remaining);
        remaining = &remaining[index..];
        let Some(mut hex) = hex_value(character) else {
            if matches!(character, 0x0a | 0x0c) {
                continue;
            }
            if character == i32::from(b'\r') {
                if !remaining.is_empty() {
                    let (next, next_width) = decode_rune(remaining);
                    if next == i32::from(b'\n') {
                        remaining = &remaining[next_width..];
                    }
                }
                continue;
            }
            push_code_point(&mut output, character);
            continue;
        };

        for _ in 0..5 {
            if remaining.is_empty() {
                break;
            }
            let (next, next_width) = decode_rune(remaining);
            let Some(next_hex) = hex_value(next) else {
                break;
            };
            remaining = &remaining[next_width..];
            hex = hex * 16 + next_hex;
        }
        if !remaining.is_empty() {
            let (next, next_width) = decode_rune(remaining);
            if is_whitespace(next) {
                remaining = &remaining[next_width..];
            }
        }
        if hex == 0 || (0xd800..=0xdfff).contains(&hex) || hex > 0x10_ffff {
            push_code_point(&mut output, 0xfffd);
        } else {
            push_code_point(&mut output, hex);
        }
    }
    output
}

fn decode_rune(bytes: &[u8]) -> (i32, usize) {
    if bytes.is_empty() {
        return (END_OF_FILE, 0);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => {
            let character = text.chars().next().expect("input is non-empty");
            (
                i32::try_from(u32::from(character)).expect("Unicode scalar fits in i32"),
                character.len_utf8(),
            )
        }
        Err(error) if error.valid_up_to() > 0 => {
            let valid = std::str::from_utf8(&bytes[..error.valid_up_to()])
                .expect("prefix reported as valid UTF-8");
            let character = valid.chars().next().expect("valid prefix is non-empty");
            (
                i32::try_from(u32::from(character)).expect("Unicode scalar fits in i32"),
                character.len_utf8(),
            )
        }
        Err(_) => (0xfffd, 1),
    }
}

fn push_code_point(output: &mut Vec<u8>, code_point: i32) {
    let character = u32::try_from(code_point)
        .ok()
        .and_then(char::from_u32)
        .unwrap_or('\u{fffd}');
    let mut bytes = [0; 4];
    output.extend_from_slice(character.encode_utf8(&mut bytes).as_bytes());
}

#[cfg(test)]
mod tests {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::Value;

    use super::{Options, TokenKind, range_of_identifier, tokenize};
    use crate::internal::logger::{
        DeferLogKind, Loc, Log, OutputOptions, Path, PrettyPaths, Source, TerminalInfo,
    };
    use std::collections::HashMap;
    use std::sync::Arc;

    fn source(contents: &[u8]) -> Source {
        Source {
            pretty_paths: PrettyPaths {
                abs: "<stdin>".into(),
                rel: "<stdin>".into(),
            },
            identifier_name: "stdin".into(),
            contents: Arc::from(contents),
            key_path: Path {
                text: "<stdin>".into(),
                ..Path::default()
            },
            ..Source::default()
        }
    }

    fn base64_field(case: &Value, field: &str) -> Vec<u8> {
        STANDARD
            .decode(case[field].as_str().expect("base64 corpus field"))
            .expect("valid base64 corpus field")
    }

    #[test]
    fn matches_pinned_upstream_css_lexer_corpus() {
        let cases: Value =
            serde_json::from_str(include_str!("../../../tests/upstream/css_lexer.json"))
                .expect("valid pinned upstream css_lexer corpus");
        let cases = cases.as_array().expect("css_lexer corpus array");
        let kind_filter = std::env::var("UPSTREAM_TEST_FILTER").ok();
        let line_filter = std::env::var("UPSTREAM_LINE_FILTER")
            .ok()
            .and_then(|value| value.parse::<u64>().ok());
        if kind_filter.is_none() && line_filter.is_none() {
            assert_eq!(cases.len(), 69, "upstream css_lexer case count changed");
        }

        let mut failures = Vec::new();
        for case in cases {
            let kind = case["kind"].as_str().expect("case kind");
            let line = case["line"].as_u64().expect("case line");
            if kind_filter.as_deref().is_some_and(|filter| kind != filter)
                || line_filter.is_some_and(|filter| line != filter)
            {
                continue;
            }
            let input = base64_field(case, "input_base64");
            let log = Log::new_defer(DeferLogKind::NoVerboseOrDebug, HashMap::new());
            let result = tokenize(log.clone(), source(&input), Options::default());
            let diagnostics = log
                .done()
                .iter()
                .flat_map(|message| {
                    message.to_bytes(&OutputOptions::default(), TerminalInfo::default())
                })
                .collect::<Vec<_>>();

            if kind == "diagnostic" {
                let expected = base64_field(case, "expected_base64");
                if diagnostics != expected {
                    failures.push(format!(
                        "internal/css_lexer/css_lexer_test.go:{line}: input {input:?}\nexpected diagnostic: {:?}\nactual diagnostic:   {:?}",
                        String::from_utf8_lossy(&expected),
                        String::from_utf8_lossy(&diagnostics),
                    ));
                }
                continue;
            }
            let actual_token = result
                .tokens
                .first()
                .map_or(TokenKind::EndOfFile, |token| token.kind);
            let expected_token = case["expected_token"].as_str().expect("expected token");
            let actual_token_name = format!("{actual_token:?}");
            if actual_token_name != expected_token {
                failures.push(format!(
                    "internal/css_lexer/css_lexer_test.go:{line}: input {input:?}: expected {expected_token}, actual {actual_token_name}"
                ));
                continue;
            }
            if kind == "decoded" {
                let expected = base64_field(case, "expected_base64");
                let actual = result.tokens[0].decoded_text(&input);
                if actual != expected {
                    failures.push(format!(
                        "internal/css_lexer/css_lexer_test.go:{line}: input {input:?}\nexpected decoded: {expected:?}\nactual decoded:   {actual:?}"
                    ));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "pinned upstream css_lexer failures:\n{}",
            failures.join("\n\n")
        );
    }

    fn lex_token(contents: &str) -> (TokenKind, Vec<u8>) {
        let source = source(contents.as_bytes());
        let result = tokenize(
            Log::new_defer(DeferLogKind::NoVerboseOrDebug, HashMap::new()),
            source.clone(),
            Options::default(),
        );
        result
            .tokens
            .first()
            .map_or((TokenKind::EndOfFile, Vec::new()), |token| {
                (token.kind, token.decoded_text(&source.contents))
            })
    }

    fn lexer_error(contents: &str) -> String {
        let log = Log::new_defer(DeferLogKind::NoVerboseOrDebug, HashMap::new());
        let _ = tokenize(log.clone(), source(contents.as_bytes()), Options::default());
        log.done()
            .iter()
            .map(|message| {
                message.to_string_lossy(&OutputOptions::default(), TerminalInfo::default())
            })
            .collect()
    }

    #[test]
    fn token_kinds_match_upstream() {
        let cases = [
            ("", TokenKind::EndOfFile),
            ("@media", TokenKind::AtKeyword),
            ("url(x y", TokenKind::BadUrl),
            ("-->", TokenKind::Cdc),
            ("<!--", TokenKind::Cdo),
            ("}", TokenKind::CloseBrace),
            ("]", TokenKind::CloseBracket),
            (")", TokenKind::CloseParen),
            (":", TokenKind::Colon),
            (",", TokenKind::Comma),
            ("?", TokenKind::Delim),
            ("&", TokenKind::DelimAmpersand),
            ("*", TokenKind::DelimAsterisk),
            ("|", TokenKind::DelimBar),
            ("^", TokenKind::DelimCaret),
            ("$", TokenKind::DelimDollar),
            (".", TokenKind::DelimDot),
            ("=", TokenKind::DelimEquals),
            ("!", TokenKind::DelimExclamation),
            (">", TokenKind::DelimGreaterThan),
            ("+", TokenKind::DelimPlus),
            ("/", TokenKind::DelimSlash),
            ("~", TokenKind::DelimTilde),
            ("1px", TokenKind::Dimension),
            ("max(", TokenKind::Function),
            ("#name", TokenKind::Hash),
            ("name", TokenKind::Ident),
            ("123", TokenKind::Number),
            ("{", TokenKind::OpenBrace),
            ("[", TokenKind::OpenBracket),
            ("(", TokenKind::OpenParen),
            ("50%", TokenKind::Percentage),
            (";", TokenKind::Semicolon),
            ("'abc'", TokenKind::String),
            ("url(test)", TokenKind::Url),
            (" ", TokenKind::Whitespace),
        ];
        for (contents, expected) in cases {
            assert_eq!(lex_token(contents).0, expected, "{contents:?}");
        }
    }

    #[test]
    fn parses_string_escapes_like_upstream() {
        let decoded = |contents| {
            let (kind, text) = lex_token(contents);
            assert_eq!(kind, TokenKind::String);
            String::from_utf8(text).unwrap()
        };
        assert_eq!(decoded("\"foo\""), "foo");
        assert_eq!(decoded("\"f\\oo\""), "foo");
        assert_eq!(decoded("\"f\\\"o\""), "f\"o");
        assert_eq!(decoded("\"f\\\\o\""), "f\\o");
        assert_eq!(decoded("\"f\\\no\""), "fo");
        assert_eq!(decoded("\"f\\\ro\""), "fo");
        assert_eq!(decoded("\"f\\\r\no\""), "fo");
        assert_eq!(decoded("\"f\\\u{c}o\""), "fo");
        assert_eq!(decoded("\"f\\6fo\""), "foo");
        assert_eq!(decoded("\"f\\6f o\""), "foo");
        assert_eq!(decoded("\"f\\6f  o\""), "fo o");
        assert_eq!(decoded("\"f\\fffffffo\""), "f\u{fffd}fo");
        assert_eq!(decoded("\"f\\10abcdeo\""), "f\u{10abcd}eo");
    }

    #[test]
    fn parses_url_escapes_and_bad_urls_like_upstream() {
        let decoded = |expected, contents| {
            let (kind, text) = lex_token(contents);
            assert_eq!(kind, expected);
            String::from_utf8(text).unwrap()
        };
        assert_eq!(decoded(TokenKind::Url, "url(foo)"), "foo");
        assert_eq!(decoded(TokenKind::Url, "url(  foo\t\t)"), "foo");
        assert_eq!(decoded(TokenKind::Url, "url(f\\oo)"), "foo");
        assert_eq!(decoded(TokenKind::Url, "url(f\\\"o)"), "f\"o");
        assert_eq!(decoded(TokenKind::Url, "url(f\\'o)"), "f'o");
        assert_eq!(decoded(TokenKind::Url, "url(f\\)o)"), "f)o");
        assert_eq!(decoded(TokenKind::Url, "url(f\\6fo)"), "foo");
        assert_eq!(decoded(TokenKind::Url, "url(f\\6f o)"), "foo");
        assert_eq!(decoded(TokenKind::BadUrl, "url(f\\6f  o)"), "url(f\\6f  o)");
    }

    #[test]
    fn reports_comment_and_string_diagnostics_like_upstream() {
        assert_eq!(
            lexer_error("/*"),
            "<stdin>: ERROR: Expected \"*/\" to terminate multi-line comment\n<stdin>: NOTE: The multi-line comment starts here:\n"
        );
        assert_eq!(
            lexer_error("/*/"),
            "<stdin>: ERROR: Expected \"*/\" to terminate multi-line comment\n<stdin>: NOTE: The multi-line comment starts here:\n"
        );
        assert_eq!(lexer_error("/**/"), "");
        assert_eq!(
            lexer_error("//"),
            "<stdin>: WARNING: Comments in CSS use \"/* ... */\" instead of \"//\"\n"
        );
        for contents in ["'", "\"", "'\\'", "\"\\\""] {
            assert_eq!(
                lexer_error(contents),
                "<stdin>: WARNING: Unterminated string token\n"
            );
        }
        assert_eq!(lexer_error("''"), "");
        assert_eq!(lexer_error("\"\""), "");
    }

    #[test]
    fn skips_bom_and_preserves_raw_invalid_utf8() {
        assert_eq!(lex_token("\u{feff}.").0, TokenKind::DelimDot);
        let source = source(&[0xff, b'a']);
        let result = tokenize(
            Log::new_defer(DeferLogKind::All, HashMap::new()),
            source.clone(),
            Options::default(),
        );
        assert_eq!(result.tokens[0].kind, TokenKind::Ident);
        assert_eq!(
            result.tokens[0].decoded_text(&source.contents),
            [0xff, b'a']
        );
    }

    #[test]
    fn records_comments_source_maps_and_identifier_ranges() {
        let css_source =
            source(b"/*! first */a/* @license second */b/*# sourceMappingURL=out.css.map */");
        let result = tokenize(
            Log::new_defer(DeferLogKind::All, HashMap::new()),
            css_source,
            Options {
                record_all_comments: true,
            },
        );
        assert_eq!(result.all_comments.len(), 3);
        assert_eq!(result.legal_comments.len(), 2);
        assert_eq!(result.legal_comments[0].text, b"/*! first */");
        assert_eq!(result.source_map_comment.text, "out.css.map");

        let identifier_source = source(b"foo\\62 ar trailing");
        let range = range_of_identifier(&identifier_source, Loc { start: 0 });
        assert_eq!(
            &identifier_source.contents[..usize::try_from(range.len).unwrap()],
            b"foo\\62 ar"
        );
    }
}
