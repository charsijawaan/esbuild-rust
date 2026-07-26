use std::{collections::HashMap, panic::panic_any};

use crate::internal::{
    helpers::string_to_utf16,
    js_ast::{
        DotExpr, Expr, ExprData, IdentifierExpr, JsxElementExpr, JsxTextExpr, Precedence, Property,
        PropertyFlags, PropertyKind, SpreadExpr, StringExpr,
    },
    js_lexer::{Lexer, LexerPanic, Token},
    logger::{Loc, Range},
};

use super::{parser_core::ParserCore, syntax_expression::parse_expression};

pub(crate) fn parse_jsx_element_prefix(core: &mut ParserCore, lexer: &mut Lexer) -> Option<Expr> {
    if lexer.token != Token::LessThan {
        return None;
    }
    if !core.options.jsx.parse {
        if core.options.ts.parse {
            return None;
        }
        core.add_error_range(
            lexer.range(),
            "The JSX syntax extension is not currently enabled",
        );
        core.options.jsx.parse = true;
    }
    let loc = lexer.loc();
    core.has_jsx_element = true;
    lexer.next_inside_jsx_element();
    let element = parse_jsx_element(core, lexer, loc);
    lexer.next();
    Some(element)
}

fn parse_jsx_namespaced_name(core: &mut ParserCore, lexer: &mut Lexer) -> (Range, String) {
    let mut name_range = lexer.range();
    let mut name = String::from_utf8_lossy(&lexer.identifier.string).into_owned();
    lexer.expect_inside_jsx_element(Token::Identifier);
    if lexer.token == Token::Colon {
        name.push(':');
        lexer.next_inside_jsx_element();
        if lexer.token != Token::Identifier {
            core.add_error_range(
                Range {
                    loc: Loc {
                        start: name_range.end(),
                    },
                    len: 0,
                },
                format!("Expected identifier after {name:?} in namespaced JSX name"),
            );
            panic_any(LexerPanic);
        }
        let second = lexer.range();
        name.push_str(&String::from_utf8_lossy(&lexer.identifier.string));
        lexer.next_inside_jsx_element();
        name_range.len = second.end() - name_range.loc.start;
    }
    (name_range, name)
}

fn parse_jsx_tag(core: &mut ParserCore, lexer: &mut Lexer) -> (Range, String, Expr) {
    let loc = lexer.loc();
    if lexer.token == Token::GreaterThan {
        return (Range { loc, len: 0 }, String::new(), Expr::default());
    }

    let (mut tag_range, tag_name) = parse_jsx_namespaced_name(core, lexer);
    let is_intrinsic = tag_name.contains(['-', ':'])
        || (lexer.token != Token::Dot
            && tag_name
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_lowercase));
    if is_intrinsic {
        return (
            tag_range,
            tag_name.clone(),
            Expr::new(
                loc,
                ExprData::String(StringExpr {
                    value: string_to_utf16(tag_name.as_bytes()),
                    ..StringExpr::default()
                }),
            ),
        );
    }

    let reference = core.store_name_in_ref(crate::internal::js_lexer::MaybeSubstring {
        string: tag_name.as_bytes().to_vec(),
        ..crate::internal::js_lexer::MaybeSubstring::default()
    });
    let mut tag = Expr::new(
        loc,
        ExprData::Identifier(IdentifierExpr {
            reference,
            ..IdentifierExpr::default()
        }),
    );
    let mut tag_text = tag_name;
    while lexer.token == Token::Dot {
        lexer.next_inside_jsx_element();
        let member_range = lexer.range();
        let member = String::from_utf8_lossy(&lexer.identifier.string).into_owned();
        lexer.expect_inside_jsx_element(Token::Identifier);
        if let Some(index) = member.find('-') {
            core.add_error_range(
                Range {
                    loc: Loc {
                        start: member_range.loc.start + i32::try_from(index).unwrap_or(i32::MAX),
                    },
                    len: 1,
                },
                "Unexpected \"-\"",
            );
            panic_any(LexerPanic);
        }
        tag_text.push('.');
        tag_text.push_str(&member);
        tag = Expr::new(
            loc,
            ExprData::Dot(DotExpr {
                target: tag,
                name: member,
                name_loc: member_range.loc,
                ..DotExpr::default()
            }),
        );
        tag_range.len = member_range.end() - tag_range.loc.start;
    }
    (tag_range, tag_text, tag)
}

#[allow(clippy::too_many_lines)]
fn parse_jsx_element(core: &mut ParserCore, lexer: &mut Lexer, loc: Loc) -> Expr {
    let (_start_range, start_text, start_tag_or_nil) = parse_jsx_tag(core, lexer);
    let mut properties = Vec::new();
    let mut attribute_locs = HashMap::new();
    let mut is_single_line = true;

    if start_tag_or_nil.data.is_some() {
        loop {
            is_single_line &= !lexer.has_newline_before;
            match lexer.token {
                Token::Identifier => {
                    let (key_range, key_name) = parse_jsx_namespaced_name(core, lexer);
                    if attribute_locs
                        .insert(key_name.clone(), key_range.loc)
                        .is_some()
                    {
                        core.add_warning_range(
                            key_range,
                            format!("Duplicate {key_name:?} attribute in JSX element"),
                        );
                    }
                    let key = Expr::new(
                        key_range.loc,
                        ExprData::String(StringExpr {
                            value: string_to_utf16(key_name.as_bytes()),
                            ..StringExpr::default()
                        }),
                    );
                    let (value_or_nil, flags) = if lexer.token == Token::Equals {
                        lexer.next_inside_jsx_element();
                        let value = match lexer.token {
                            Token::StringLiteral => {
                                let string_loc = lexer.loc();
                                let value = if core.options.jsx.preserve {
                                    Expr::new(
                                        string_loc,
                                        ExprData::JsxText(JsxTextExpr {
                                            raw: String::from_utf8_lossy(lexer.raw()).into_owned(),
                                        }),
                                    )
                                } else {
                                    let value = lexer.string_literal().to_vec();
                                    Expr::new(
                                        string_loc,
                                        ExprData::String(StringExpr {
                                            value,
                                            ..StringExpr::default()
                                        }),
                                    )
                                };
                                lexer.next_inside_jsx_element();
                                value
                            }
                            Token::LessThan => {
                                let child_loc = lexer.loc();
                                lexer.next_inside_jsx_element();
                                let value = parse_jsx_element(core, lexer, child_loc);
                                lexer.next_inside_jsx_element();
                                value
                            }
                            _ => {
                                lexer.expect(Token::OpenBrace);
                                let value = parse_expression(core, lexer, Precedence::Lowest, true);
                                lexer.expect_inside_jsx_element(Token::CloseBrace);
                                value
                            }
                        };
                        (value, PropertyFlags::NONE)
                    } else {
                        (
                            Expr::new(
                                Loc {
                                    start: key_range.end(),
                                },
                                ExprData::Boolean(true),
                            ),
                            PropertyFlags::WAS_SHORTHAND,
                        )
                    };
                    properties.push(Property {
                        key,
                        value_or_nil,
                        loc: key_range.loc,
                        flags,
                        ..Property::default()
                    });
                }
                Token::OpenBrace => {
                    lexer.next();
                    let spread_loc = lexer.loc();
                    lexer.expect(Token::DotDotDot);
                    let value = parse_expression(core, lexer, Precedence::Comma, true);
                    properties.push(Property {
                        value_or_nil: value,
                        loc: spread_loc,
                        kind: PropertyKind::Spread,
                        ..Property::default()
                    });
                    lexer.expect_inside_jsx_element(Token::CloseBrace);
                }
                _ => break,
            }
        }
    }

    if lexer.token == Token::Slash {
        let close_loc = lexer.loc();
        lexer.next_inside_jsx_element();
        if lexer.token != Token::GreaterThan {
            lexer.expected(Token::GreaterThan);
        }
        return Expr::new(
            loc,
            ExprData::JsxElement(JsxElementExpr {
                tag_or_nil: start_tag_or_nil,
                properties,
                close_loc,
                is_tag_single_line: is_single_line,
                ..JsxElementExpr::default()
            }),
        );
    }

    lexer.expect_jsx_element_child(Token::GreaterThan);
    let mut nullable_children = Vec::new();
    loop {
        match lexer.token {
            Token::StringLiteral => {
                let child_loc = lexer.loc();
                if core.options.jsx.preserve {
                    nullable_children.push(Expr::new(
                        child_loc,
                        ExprData::JsxText(JsxTextExpr {
                            raw: String::from_utf8_lossy(lexer.raw()).into_owned(),
                        }),
                    ));
                } else {
                    let value = lexer.string_literal().to_vec();
                    if !value.is_empty() {
                        nullable_children.push(Expr::new(
                            child_loc,
                            ExprData::String(StringExpr {
                                value,
                                ..StringExpr::default()
                            }),
                        ));
                    }
                }
                lexer.next_jsx_element_child();
            }
            Token::OpenBrace => {
                let child_loc = lexer.loc();
                lexer.next();
                if lexer.token == Token::CloseBrace {
                    nullable_children.push(Expr {
                        loc: child_loc,
                        ..Expr::default()
                    });
                } else if lexer.token == Token::DotDotDot {
                    let spread_loc = lexer.loc();
                    lexer.next();
                    nullable_children.push(Expr::new(
                        spread_loc,
                        ExprData::Spread(SpreadExpr {
                            value: parse_expression(core, lexer, Precedence::Lowest, true),
                        }),
                    ));
                } else {
                    nullable_children.push(parse_expression(core, lexer, Precedence::Lowest, true));
                }
                lexer.expect_jsx_element_child(Token::CloseBrace);
            }
            Token::LessThan => {
                let less_than_loc = lexer.loc();
                lexer.next_inside_jsx_element();
                if lexer.token != Token::Slash {
                    nullable_children.push(parse_jsx_element(core, lexer, less_than_loc));
                    lexer.next_jsx_element_child();
                    continue;
                }

                lexer.next_inside_jsx_element();
                let (end_range, end_text, _) = parse_jsx_tag(core, lexer);
                if start_text != end_text {
                    core.add_error_range(
                        end_range,
                        format!(
                            "Unexpected closing {} does not match opening {}",
                            tag_or_fragment_help_text(&end_text),
                            tag_or_fragment_help_text(&start_text)
                        ),
                    );
                }
                if lexer.token != Token::GreaterThan {
                    lexer.expected(Token::GreaterThan);
                }
                return Expr::new(
                    loc,
                    ExprData::JsxElement(JsxElementExpr {
                        tag_or_nil: start_tag_or_nil,
                        properties,
                        nullable_children,
                        close_loc: less_than_loc,
                        is_tag_single_line: is_single_line,
                    }),
                );
            }
            Token::EndOfFile => {
                core.add_error_range(
                    lexer.range(),
                    format!(
                        "Unexpected end of file before a closing {}",
                        tag_or_fragment_help_text(&start_text)
                    ),
                );
                panic_any(LexerPanic);
            }
            _ => lexer.unexpected(),
        }
    }
}

fn tag_or_fragment_help_text(tag: &str) -> String {
    if tag.is_empty() {
        "fragment tag".into()
    } else {
        format!("{tag:?} tag")
    }
}
