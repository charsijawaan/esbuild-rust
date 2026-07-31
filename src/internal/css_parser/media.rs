use crate::internal::{
    compat::CssFeature,
    css_ast::{
        MediaArbitraryTokensQuery, MediaBinaryOp, MediaBinaryQuery, MediaCmp, MediaNotQuery,
        MediaPlainOrBooleanQuery, MediaQuery, MediaQueryData, MediaRangeQuery, MediaTypeOp,
        MediaTypeQuery, Token, WhitespaceFlags,
    },
    css_lexer::TokenKind,
    logger::Loc,
};

/// Parse a CSS media query list from the already-balanced token tree used by
/// the rest of the Rust CSS parser. Invalid or currently-unrecognized queries
/// deliberately fall back to arbitrary tokens so printing remains lossless.
pub(super) fn parse_media_query_list(
    tokens: Vec<Token>,
    fallback_loc: Loc,
    unsupported_css_features: CssFeature,
    minify_syntax: bool,
) -> Vec<MediaQuery> {
    let mut queries = Vec::new();
    let mut current = Vec::new();
    for token in tokens {
        if token.kind == TokenKind::Comma {
            push_media_query(
                &mut queries,
                std::mem::take(&mut current),
                fallback_loc,
                unsupported_css_features,
                minify_syntax,
            );
        } else {
            current.push(token);
        }
    }
    if !current.is_empty() {
        push_media_query(
            &mut queries,
            current,
            fallback_loc,
            unsupported_css_features,
            minify_syntax,
        );
    }
    queries
}

fn push_media_query(
    queries: &mut Vec<MediaQuery>,
    mut tokens: Vec<Token>,
    fallback_loc: Loc,
    unsupported_css_features: CssFeature,
    minify_syntax: bool,
) {
    trim_boundary_whitespace(&mut tokens);
    let loc = tokens.first().map_or(fallback_loc, |token| token.loc);
    let mut parser = MediaParser {
        tokens: &tokens,
        index: 0,
        unsupported_css_features,
        minify_syntax,
    };
    let parsed = parser
        .parse_media_query()
        .filter(|_| parser.index == parser.tokens.len());
    queries.push(match parsed {
        Some(query) => query,
        None => MediaQuery {
            loc,
            data: MediaQueryData::ArbitraryTokens(MediaArbitraryTokensQuery { tokens }),
        },
    });
}

struct MediaParser<'a> {
    tokens: &'a [Token],
    index: usize,
    unsupported_css_features: CssFeature,
    minify_syntax: bool,
}

impl MediaParser<'_> {
    fn parse_media_query(&mut self) -> Option<MediaQuery> {
        let loc = self.current()?.loc;
        if self.looks_like_media_condition() {
            return self.parse_media_condition(true);
        }

        let mut media_type = self.current_ident()?.to_string();
        let mut op = MediaTypeOp::None;
        if media_type.eq_ignore_ascii_case("not") {
            op = MediaTypeOp::Not;
        } else if media_type.eq_ignore_ascii_case("only") {
            op = MediaTypeOp::Only;
        }
        if op != MediaTypeOp::None {
            self.index += 1;
            media_type = self.current_ident()?.to_string();
        }
        if matches!(
            media_type.to_ascii_lowercase().as_str(),
            "only" | "not" | "and" | "or" | "layer"
        ) {
            return None;
        }
        self.index += 1;

        let and_or_null = if self.current_is_ident("and") {
            self.index += 1;
            Some(Box::new(self.parse_media_condition(false)?))
        } else {
            None
        };
        Some(MediaQuery {
            loc,
            data: MediaQueryData::Type(MediaTypeQuery {
                op,
                media_type,
                and_or_null,
            }),
        })
    }

    fn parse_media_condition(&mut self, allow_or: bool) -> Option<MediaQuery> {
        let loc = self.current()?.loc;
        if self.current_is_ident("not") {
            self.index += 1;
            let inner = self.parse_media_in_parens()?;
            return Some(maybe_simplify_media_not(loc, inner, self.minify_syntax));
        }

        let first = self.parse_media_in_parens()?;
        let Some(keyword) = self.current_ident().map(str::to_string) else {
            return Some(first);
        };
        let op = if keyword.eq_ignore_ascii_case("and") {
            MediaBinaryOp::And
        } else if allow_or && keyword.eq_ignore_ascii_case("or") {
            MediaBinaryOp::Or
        } else {
            return Some(first);
        };

        let mut terms = Vec::new();
        append_media_term(&mut terms, first, op, self.minify_syntax);
        loop {
            self.index += 1;
            let next = self.parse_media_in_parens()?;
            append_media_term(&mut terms, next, op, self.minify_syntax);
            if !self
                .current_ident()
                .is_some_and(|current| current.eq_ignore_ascii_case(&keyword))
            {
                break;
            }
        }
        Some(MediaQuery {
            loc,
            data: MediaQueryData::Binary(MediaBinaryQuery { op, terms }),
        })
    }

    fn parse_media_in_parens(&mut self) -> Option<MediaQuery> {
        let token = self.current()?.clone();
        if !matches!(token.kind, TokenKind::OpenParen | TokenKind::Function) {
            return None;
        }
        self.index += 1;
        let loc = token.loc;

        if token.kind == TokenKind::OpenParen {
            let children = token.children.as_deref().unwrap_or_default();
            if looks_like_media_condition_at(children, 0) {
                let mut nested = MediaParser {
                    tokens: children,
                    index: 0,
                    unsupported_css_features: self.unsupported_css_features,
                    minify_syntax: self.minify_syntax,
                };
                let inner = nested.parse_media_condition(true)?;
                if nested.index != children.len() {
                    return None;
                }
                return Some(inner);
            }

            if let Some(data) = parse_plain_or_boolean_media_feature(children) {
                return Some(MediaQuery {
                    loc,
                    data: MediaQueryData::PlainOrBoolean(data),
                });
            }
            if let Some(range) = parse_range_media_feature(children) {
                if self
                    .unsupported_css_features
                    .contains(CssFeature::MEDIA_RANGE)
                {
                    return Some(lower_media_range_query(loc, range));
                }
                return Some(MediaQuery {
                    loc,
                    data: MediaQueryData::Range(range),
                });
            }
        }

        Some(MediaQuery {
            loc,
            data: MediaQueryData::ArbitraryTokens(MediaArbitraryTokensQuery {
                tokens: vec![token],
            }),
        })
    }

    fn looks_like_media_condition(&self) -> bool {
        looks_like_media_condition_at(self.tokens, self.index)
    }

    fn current(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }

    fn current_ident(&self) -> Option<&str> {
        let token = self.current()?;
        (token.kind == TokenKind::Ident).then_some(token.text.as_str())
    }

    fn current_is_ident(&self, expected: &str) -> bool {
        self.current_ident()
            .is_some_and(|text| text.eq_ignore_ascii_case(expected))
    }
}

fn looks_like_media_condition_at(tokens: &[Token], index: usize) -> bool {
    let Some(token) = tokens.get(index) else {
        return false;
    };
    if matches!(token.kind, TokenKind::OpenParen | TokenKind::Function) {
        return true;
    }
    token.kind == TokenKind::Ident
        && token.text.eq_ignore_ascii_case("not")
        && tokens.get(index + 1).is_some_and(|next| {
            next.kind == TokenKind::OpenParen && has_boundary_whitespace(token, next)
        })
}

fn has_boundary_whitespace(left: &Token, right: &Token) -> bool {
    left.whitespace.contains(WhitespaceFlags::AFTER)
        || right.whitespace.contains(WhitespaceFlags::BEFORE)
}

fn append_media_term(
    terms: &mut Vec<MediaQuery>,
    term: MediaQuery,
    op: MediaBinaryOp,
    minify_syntax: bool,
) {
    match term {
        MediaQuery {
            data: MediaQueryData::Binary(binary),
            ..
        } if minify_syntax && binary.op == op => terms.extend(binary.terms),
        term => terms.push(term),
    }
}

fn parse_plain_or_boolean_media_feature(tokens: &[Token]) -> Option<MediaPlainOrBooleanQuery> {
    if let [name] = tokens
        && name.kind == TokenKind::Ident
    {
        return Some(MediaPlainOrBooleanQuery {
            name: name.text.clone(),
            value_or_nil: Vec::new(),
        });
    }
    if tokens.len() >= 3 && tokens[0].kind == TokenKind::Ident && tokens[1].kind == TokenKind::Colon
    {
        let (value, next) = scan_media_value(tokens, 2)?;
        if next == tokens.len() {
            return Some(MediaPlainOrBooleanQuery {
                name: tokens[0].text.clone(),
                value_or_nil: value,
            });
        }
    }
    None
}

fn parse_range_media_feature(tokens: &[Token]) -> Option<MediaRangeQuery> {
    let (first, index) = scan_media_value(tokens, 0)?;
    let (first_cmp, index) = scan_media_comparison(tokens, index)?;
    let (second, index) = scan_media_value(tokens, index)?;

    if index == tokens.len() {
        if let Some((name, name_loc)) = is_single_ident(&first) {
            return Some(MediaRangeQuery {
                name,
                name_loc,
                after_cmp: first_cmp,
                after: second,
                ..MediaRangeQuery::default()
            });
        }
        if let Some((name, name_loc)) = is_single_ident(&second) {
            return Some(MediaRangeQuery {
                before: first,
                before_cmp: first_cmp,
                name,
                name_loc,
                ..MediaRangeQuery::default()
            });
        }
        return None;
    }

    let (name, name_loc) = is_single_ident(&second)?;
    let (second_cmp, index) = scan_media_comparison(tokens, index)?;
    let first_direction = first_cmp.direction();
    let second_direction = second_cmp.direction();
    if !((first_direction < 0 && second_direction < 0)
        || (first_direction > 0 && second_direction > 0))
    {
        return None;
    }
    let (third, index) = scan_media_value(tokens, index)?;
    (index == tokens.len()).then_some(MediaRangeQuery {
        before: first,
        before_cmp: first_cmp,
        name,
        name_loc,
        after_cmp: second_cmp,
        after: third,
    })
}

fn scan_media_value(tokens: &[Token], index: usize) -> Option<(Vec<Token>, usize)> {
    let first = tokens.get(index)?;
    let count = if first.kind == TokenKind::Number
        && tokens.get(index + 1).map(|token| token.kind) == Some(TokenKind::DelimSlash)
        && tokens.get(index + 2).map(|token| token.kind) == Some(TokenKind::Number)
    {
        3
    } else if matches!(
        first.kind,
        TokenKind::Dimension | TokenKind::Ident | TokenKind::Number
    ) {
        1
    } else {
        return None;
    };
    let end = index + count;
    let mut value = tokens.get(index..end)?.to_vec();
    trim_boundary_whitespace(&mut value);
    Some((value, end))
}

fn scan_media_comparison(tokens: &[Token], index: usize) -> Option<(MediaCmp, usize)> {
    let token = tokens.get(index)?;
    match token.kind {
        TokenKind::DelimEquals => Some((MediaCmp::Equal, index + 1)),
        TokenKind::DelimLessThan => {
            if let Some(equal) = tokens.get(index + 1)
                && equal.kind == TokenKind::DelimEquals
                && !has_boundary_whitespace(token, equal)
            {
                Some((MediaCmp::LessThanOrEqual, index + 2))
            } else {
                Some((MediaCmp::LessThan, index + 1))
            }
        }
        TokenKind::DelimGreaterThan => {
            if let Some(equal) = tokens.get(index + 1)
                && equal.kind == TokenKind::DelimEquals
                && !has_boundary_whitespace(token, equal)
            {
                Some((MediaCmp::GreaterThanOrEqual, index + 2))
            } else {
                Some((MediaCmp::GreaterThan, index + 1))
            }
        }
        _ => None,
    }
}

fn is_single_ident(tokens: &[Token]) -> Option<(String, Loc)> {
    let [token] = tokens else {
        return None;
    };
    (token.kind == TokenKind::Ident).then(|| (token.text.clone(), token.loc))
}

fn lower_media_range_query(loc: Loc, range: MediaRangeQuery) -> MediaQuery {
    let mut terms = Vec::new();
    if range.before_cmp != MediaCmp::None {
        terms.push(lower_media_range(
            range.name_loc,
            &range.name,
            range.before_cmp.reverse(),
            range.before,
        ));
    }
    if range.after_cmp != MediaCmp::None {
        terms.push(lower_media_range(
            range.name_loc,
            &range.name,
            range.after_cmp,
            range.after,
        ));
    }
    if terms.len() == 1 {
        terms.pop().expect("one media range term")
    } else {
        MediaQuery {
            loc,
            data: MediaQueryData::Binary(MediaBinaryQuery {
                op: MediaBinaryOp::And,
                terms,
            }),
        }
    }
}

fn lower_media_range(loc: Loc, name: &str, comparison: MediaCmp, value: Vec<Token>) -> MediaQuery {
    let plain = |name: String, value| MediaQuery {
        loc,
        data: MediaQueryData::PlainOrBoolean(MediaPlainOrBooleanQuery {
            name,
            value_or_nil: value,
        }),
    };
    match comparison {
        MediaCmp::LessThanOrEqual => plain(format!("max-{name}"), value),
        MediaCmp::GreaterThanOrEqual => plain(format!("min-{name}"), value),
        MediaCmp::LessThan => MediaQuery {
            loc,
            data: MediaQueryData::Not(MediaNotQuery {
                inner: Box::new(plain(format!("min-{name}"), value)),
            }),
        },
        MediaCmp::GreaterThan => MediaQuery {
            loc,
            data: MediaQueryData::Not(MediaNotQuery {
                inner: Box::new(plain(format!("max-{name}"), value)),
            }),
        },
        MediaCmp::None | MediaCmp::Equal => plain(name.to_string(), value),
    }
}

fn maybe_simplify_media_not(loc: Loc, inner: MediaQuery, minify_syntax: bool) -> MediaQuery {
    if minify_syntax {
        match inner {
            MediaQuery {
                data: MediaQueryData::Not(query),
                ..
            } => return *query.inner,
            MediaQuery {
                loc: inner_loc,
                data: MediaQueryData::Binary(mut query),
            } if query
                .terms
                .iter()
                .all(|term| matches!(term.data, MediaQueryData::Not(_))) =>
            {
                query.op = match query.op {
                    MediaBinaryOp::And => MediaBinaryOp::Or,
                    MediaBinaryOp::Or => MediaBinaryOp::And,
                };
                query.terms = query
                    .terms
                    .into_iter()
                    .map(|term| match term.data {
                        MediaQueryData::Not(query) => *query.inner,
                        _ => unreachable!("all media terms were checked above"),
                    })
                    .collect();
                return MediaQuery {
                    loc: inner_loc,
                    data: MediaQueryData::Binary(query),
                };
            }
            MediaQuery {
                loc: inner_loc,
                data: MediaQueryData::Range(mut query),
            } if (query.before_cmp == MediaCmp::None && query.after_cmp != MediaCmp::Equal)
                || (query.after_cmp == MediaCmp::None && query.before_cmp != MediaCmp::Equal) =>
            {
                query.before_cmp = query.before_cmp.flip();
                query.after_cmp = query.after_cmp.flip();
                return MediaQuery {
                    loc: inner_loc,
                    data: MediaQueryData::Range(query),
                };
            }
            inner => {
                return MediaQuery {
                    loc,
                    data: MediaQueryData::Not(MediaNotQuery {
                        inner: Box::new(inner),
                    }),
                };
            }
        }
    }
    MediaQuery {
        loc,
        data: MediaQueryData::Not(MediaNotQuery {
            inner: Box::new(inner),
        }),
    }
}

fn trim_boundary_whitespace(tokens: &mut [Token]) {
    if let Some(first) = tokens.first_mut() {
        first.whitespace.remove(WhitespaceFlags::BEFORE);
    }
    if let Some(last) = tokens.last_mut() {
        last.whitespace.remove(WhitespaceFlags::AFTER);
    }
}
