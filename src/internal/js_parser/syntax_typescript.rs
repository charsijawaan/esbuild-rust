use crate::internal::{
    ast::{INVALID_REF, LocRef, SymbolKind},
    helpers::string_to_utf16,
    js_ast::{
        EnumStmt, EnumValue, Expr, ExprData, ExprStmt, IdentifierExpr, Precedence, Stmt, StmtData,
        TsNamespaceScope, TypeScriptStmt,
    },
    js_lexer::{Lexer, Token},
};

use super::{
    parser_core::ParserCore,
    syntax_expression::{parse_expression, parse_expression_suffix},
};

pub(crate) fn skip_type_parameters(lexer: &mut Lexer) {
    if lexer.token != Token::LessThan {
        return;
    }
    let mut depth = 0_usize;
    loop {
        match lexer.token {
            Token::LessThan => depth += 1,
            Token::GreaterThan
            | Token::GreaterThanEquals
            | Token::GreaterThanGreaterThan
            | Token::GreaterThanGreaterThanEquals
            | Token::GreaterThanGreaterThanGreaterThan
            | Token::GreaterThanGreaterThanGreaterThanEquals => {
                depth -= 1;
                lexer.expect_greater_than(false);
                if depth == 0 {
                    return;
                }
                continue;
            }
            Token::EndOfFile => lexer.expected(Token::GreaterThan),
            _ => {}
        }
        lexer.next();
    }
}

pub(crate) fn skip_class_implements_clause(lexer: &mut Lexer) {
    if !lexer.is_contextual_keyword(b"implements") {
        return;
    }
    lexer.next();
    let mut delimiters = Vec::new();
    loop {
        if delimiters.is_empty() && lexer.token == Token::OpenBrace {
            return;
        }
        match lexer.token {
            Token::OpenParen => delimiters.push(Token::CloseParen),
            Token::OpenBracket => delimiters.push(Token::CloseBracket),
            Token::LessThan => delimiters.push(Token::GreaterThan),
            Token::GreaterThan
            | Token::GreaterThanEquals
            | Token::GreaterThanGreaterThan
            | Token::GreaterThanGreaterThanEquals
            | Token::GreaterThanGreaterThanGreaterThan
            | Token::GreaterThanGreaterThanGreaterThanEquals
                if delimiters.last() == Some(&Token::GreaterThan) =>
            {
                delimiters.pop();
                lexer.expect_greater_than(false);
                continue;
            }
            token if delimiters.last() == Some(&token) => {
                delimiters.pop();
            }
            Token::EndOfFile => lexer.expected(Token::OpenBrace),
            _ => {}
        }
        lexer.next();
    }
}

pub(crate) fn skip_type_script_method_signature(lexer: &mut Lexer) {
    lexer.expect(Token::OpenParen);
    let mut depth = 1_usize;
    while depth > 0 {
        match lexer.token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth -= 1,
            Token::EndOfFile => lexer.expected(Token::CloseParen),
            _ => {}
        }
        lexer.next();
    }
    skip_type_annotation(lexer, &[Token::Semicolon, Token::CloseBrace]);
    lexer.expect_or_insert_semicolon();
}

pub(crate) fn skip_type_annotation(lexer: &mut Lexer, stop_tokens: &[Token]) {
    if lexer.token != Token::Colon {
        return;
    }
    lexer.next();
    let mut delimiters = Vec::new();
    let mut has_type_token = false;
    loop {
        if delimiters.is_empty()
            && (stop_tokens.contains(&lexer.token)
                || (stop_tokens.contains(&Token::In) && lexer.is_contextual_keyword(b"of")))
        {
            return;
        }
        if has_type_token
            && delimiters.is_empty()
            && lexer.has_newline_before
            && is_statement_start(lexer)
        {
            return;
        }
        match lexer.token {
            Token::OpenParen => delimiters.push(Token::CloseParen),
            Token::OpenBracket => delimiters.push(Token::CloseBracket),
            Token::OpenBrace => delimiters.push(Token::CloseBrace),
            Token::LessThan => delimiters.push(Token::GreaterThan),
            token if delimiters.last() == Some(&token) => {
                delimiters.pop();
            }
            Token::EndOfFile => {
                if let Some(expected) = delimiters.last().copied() {
                    lexer.expected(expected);
                }
                return;
            }
            _ => {}
        }
        has_type_token = true;
        lexer.next();
    }
}

pub(crate) fn skip_type_assertion(lexer: &mut Lexer) {
    let mut delimiters = Vec::new();
    let mut has_type_token = false;
    loop {
        if delimiters.is_empty()
            && has_type_token
            && matches!(
                lexer.token,
                Token::Comma
                    | Token::Semicolon
                    | Token::CloseBrace
                    | Token::CloseBracket
                    | Token::CloseParen
                    | Token::Question
                    | Token::Colon
                    | Token::Equals
                    | Token::Plus
                    | Token::Minus
                    | Token::Asterisk
                    | Token::AsteriskAsterisk
                    | Token::Slash
                    | Token::Percent
                    | Token::AmpersandAmpersand
                    | Token::BarBar
                    | Token::QuestionQuestion
                    | Token::EqualsEquals
                    | Token::EqualsEqualsEquals
                    | Token::ExclamationEquals
                    | Token::ExclamationEqualsEquals
                    | Token::Instanceof
                    | Token::In
                    | Token::PlusPlus
                    | Token::MinusMinus
                    | Token::Exclamation
                    | Token::OpenParen
            )
        {
            return;
        }
        if delimiters.is_empty() && lexer.token.is_assign() {
            return;
        }
        match lexer.token {
            Token::OpenParen => delimiters.push(Token::CloseParen),
            Token::OpenBracket => delimiters.push(Token::CloseBracket),
            Token::OpenBrace => delimiters.push(Token::CloseBrace),
            Token::LessThan => delimiters.push(Token::GreaterThan),
            token if delimiters.last() == Some(&token) => {
                delimiters.pop();
            }
            Token::EndOfFile => {
                if let Some(expected) = delimiters.last().copied() {
                    lexer.expected(expected);
                }
                return;
            }
            _ => {}
        }
        has_type_token = true;
        lexer.next();
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn parse_type_script_statement(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    is_export: bool,
) -> Option<Stmt> {
    if !core.options.ts.parse {
        return None;
    }
    if lexer.token == Token::Enum {
        return Some(parse_enum_statement(core, lexer, is_export));
    }
    if lexer.token != Token::Identifier {
        return None;
    }
    let loc = lexer.loc();
    if lexer.is_contextual_keyword(b"interface") {
        lexer.next();
        lexer.expect(Token::Identifier);
        while lexer.token != Token::OpenBrace {
            if lexer.token == Token::EndOfFile {
                lexer.expected(Token::OpenBrace);
            }
            lexer.next();
        }
        skip_balanced_group(lexer, Token::OpenBrace, Token::CloseBrace);
        if lexer.token == Token::Semicolon {
            lexer.next();
        }
        return Some(Stmt::new(
            loc,
            StmtData::TypeScript(TypeScriptStmt::default()),
        ));
    }
    if lexer.is_contextual_keyword(b"declare") {
        return Some(parse_declare_statement(core, lexer, loc, is_export));
    }
    if lexer.is_contextual_keyword(b"namespace") || lexer.is_contextual_keyword(b"module") {
        let is_module = lexer.is_contextual_keyword(b"module");
        let reference = core.store_name_in_ref(lexer.identifier.clone());
        lexer.next();
        if lexer.token == Token::Identifier {
            return Some(parse_namespace_statement(core, lexer, loc, is_export));
        }
        if is_module && lexer.token == Token::StringLiteral {
            lexer.next();
            if lexer.token == Token::OpenBrace {
                skip_balanced_group(lexer, Token::OpenBrace, Token::CloseBrace);
            } else {
                lexer.expect_or_insert_semicolon();
            }
            return Some(Stmt::new(
                loc,
                StmtData::TypeScript(TypeScriptStmt::default()),
            ));
        }
        if !is_export {
            let value = parse_expression_suffix(
                core,
                lexer,
                Expr::new(
                    loc,
                    ExprData::Identifier(IdentifierExpr {
                        reference,
                        ..IdentifierExpr::default()
                    }),
                ),
                Precedence::Lowest,
                true,
            );
            lexer.expect_or_insert_semicolon();
            return Some(Stmt::new(
                loc,
                StmtData::Expr(ExprStmt {
                    value,
                    ..ExprStmt::default()
                }),
            ));
        }
        lexer.expected(Token::Identifier);
    }
    if lexer.is_contextual_keyword(b"type") {
        let reference = core.store_name_in_ref(lexer.identifier.clone());
        lexer.next();
        if is_export && matches!(lexer.token, Token::OpenBrace | Token::Asterisk) {
            if lexer.token == Token::OpenBrace {
                skip_balanced_group(lexer, Token::OpenBrace, Token::CloseBrace);
            } else {
                lexer.next();
            }
            if lexer.is_contextual_keyword(b"from") {
                lexer.next();
                lexer.expect(Token::StringLiteral);
            }
            lexer.expect_or_insert_semicolon();
            return Some(Stmt::new(
                loc,
                StmtData::TypeScript(TypeScriptStmt::default()),
            ));
        }
        if !is_export && lexer.token != Token::Identifier {
            let value = parse_expression_suffix(
                core,
                lexer,
                Expr::new(
                    loc,
                    ExprData::Identifier(IdentifierExpr {
                        reference,
                        ..IdentifierExpr::default()
                    }),
                ),
                Precedence::Lowest,
                true,
            );
            lexer.expect_or_insert_semicolon();
            return Some(Stmt::new(
                loc,
                StmtData::Expr(ExprStmt {
                    value,
                    ..ExprStmt::default()
                }),
            ));
        }
        lexer.expect(Token::Identifier);
        let mut depth = 0_usize;
        while lexer.token != Token::Equals || depth > 0 {
            match lexer.token {
                Token::OpenParen | Token::OpenBracket | Token::OpenBrace | Token::LessThan => {
                    depth += 1;
                }
                Token::CloseParen
                | Token::CloseBracket
                | Token::CloseBrace
                | Token::GreaterThan
                    if depth > 0 =>
                {
                    depth -= 1;
                }
                Token::EndOfFile => lexer.expected(Token::Equals),
                _ => {}
            }
            lexer.next();
        }
        lexer.next();
        skip_type_until_statement_end(lexer);
        return Some(Stmt::new(
            loc,
            StmtData::TypeScript(TypeScriptStmt::default()),
        ));
    }
    None
}

fn parse_namespace_statement(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    loc: crate::internal::logger::Loc,
    is_export: bool,
) -> Stmt {
    let name_loc = lexer.loc();
    let name_text = String::from_utf8_lossy(lexer.raw()).into_owned();
    let name = LocRef {
        loc: name_loc,
        reference: core.declare_symbol(SymbolKind::TsNamespace, name_loc, &name_text),
    };
    lexer.expect(Token::Identifier);
    let argument = core.new_symbol(SymbolKind::Hoisted, format!("_{name_text}"));

    core.push_scope_for_parse_pass(crate::internal::js_ast::ScopeKind::Entry, loc);
    core.current_scope
        .as_ref()
        .expect("namespace scope")
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .ts_namespace = Some(TsNamespaceScope {
        argument_ref: argument,
        ..TsNamespaceScope::default()
    });
    let old_context = core.fn_or_arrow_data_parse;
    core.fn_or_arrow_data_parse.is_this_disallowed = true;
    core.fn_or_arrow_data_parse.is_return_disallowed = true;
    let statements = if lexer.token == Token::Dot {
        let dot_loc = lexer.loc();
        lexer.next();
        vec![parse_namespace_statement(core, lexer, dot_loc, true)]
    } else {
        lexer.expect(Token::OpenBrace);
        let mut statements = Vec::new();
        while lexer.token != Token::CloseBrace {
            statements.push(super::syntax_statement::parse_statement(core, lexer));
        }
        lexer.expect(Token::CloseBrace);
        statements
    };
    core.fn_or_arrow_data_parse = old_context;
    core.pop_scope();

    Stmt::new(
        loc,
        StmtData::Namespace(crate::internal::js_ast::NamespaceStmt {
            statements,
            name,
            argument,
            is_export,
        }),
    )
}

fn parse_declare_statement(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    loc: crate::internal::logger::Loc,
    is_export: bool,
) -> Stmt {
    let reference = core.store_name_in_ref(lexer.identifier.clone());
    lexer.next();
    match lexer.token {
        Token::Class => {
            lexer.next();
            lexer.expect(Token::Identifier);
            while lexer.token != Token::OpenBrace {
                if lexer.token == Token::EndOfFile {
                    lexer.expected(Token::OpenBrace);
                }
                lexer.next();
            }
            skip_balanced_group(lexer, Token::OpenBrace, Token::CloseBrace);
        }
        Token::Function => {
            lexer.next();
            if lexer.token == Token::Asterisk {
                lexer.next();
            }
            lexer.expect(Token::Identifier);
            skip_type_parameters(lexer);
            skip_type_script_method_signature(lexer);
        }
        Token::Var | Token::Const => {
            lexer.next();
            skip_type_until_statement_end(lexer);
        }
        Token::Enum => {
            lexer.next();
            lexer.expect(Token::Identifier);
            skip_balanced_group(lexer, Token::OpenBrace, Token::CloseBrace);
        }
        Token::Identifier
            if lexer.is_contextual_keyword(b"namespace")
                || lexer.is_contextual_keyword(b"module") =>
        {
            lexer.next();
            if matches!(lexer.token, Token::Identifier | Token::StringLiteral) {
                lexer.next();
            } else {
                lexer.expected(Token::Identifier);
            }
            while lexer.token == Token::Dot {
                lexer.next();
                lexer.expect(Token::Identifier);
            }
            if lexer.token == Token::OpenBrace {
                skip_balanced_group(lexer, Token::OpenBrace, Token::CloseBrace);
            } else {
                lexer.expect_or_insert_semicolon();
            }
        }
        Token::Identifier if lexer.is_contextual_keyword(b"global") => {
            lexer.next();
            skip_balanced_group(lexer, Token::OpenBrace, Token::CloseBrace);
        }
        Token::Identifier if lexer.is_contextual_keyword(b"let") => {
            lexer.next();
            skip_type_until_statement_end(lexer);
        }
        _ if !is_export => {
            let value = parse_expression_suffix(
                core,
                lexer,
                Expr::new(
                    loc,
                    ExprData::Identifier(IdentifierExpr {
                        reference,
                        ..IdentifierExpr::default()
                    }),
                ),
                Precedence::Lowest,
                true,
            );
            lexer.expect_or_insert_semicolon();
            return Stmt::new(
                loc,
                StmtData::Expr(ExprStmt {
                    value,
                    ..ExprStmt::default()
                }),
            );
        }
        _ => lexer.unexpected(),
    }
    if lexer.token == Token::Semicolon {
        lexer.next();
    }
    Stmt::new(loc, StmtData::TypeScript(TypeScriptStmt::default()))
}

pub(crate) fn parse_enum_statement(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    is_export: bool,
) -> Stmt {
    let loc = lexer.loc();
    lexer.expect(Token::Enum);
    let name_loc = lexer.loc();
    let name_text = String::from_utf8_lossy(lexer.raw()).into_owned();
    let name = LocRef {
        loc: name_loc,
        reference: core.store_name_in_ref(lexer.identifier.clone()),
    };
    lexer.expect(Token::Identifier);
    let argument = core.new_symbol(SymbolKind::Hoisted, format!("_{name_text}"));
    lexer.expect(Token::OpenBrace);
    let mut values = Vec::new();
    while lexer.token != Token::CloseBrace {
        let value_loc = lexer.loc();
        let name = if lexer.token == Token::StringLiteral {
            let name = lexer.string_literal().to_vec();
            lexer.next();
            name
        } else if lexer.is_identifier_or_keyword() {
            let name = string_to_utf16(lexer.raw());
            lexer.next();
            name
        } else {
            lexer.expected(Token::Identifier);
        };
        let value_or_nil = if lexer.token == Token::Equals {
            lexer.next();
            parse_expression(core, lexer, Precedence::Comma, true)
        } else {
            Expr::default()
        };
        values.push(EnumValue {
            value_or_nil,
            name,
            reference: INVALID_REF,
            loc: value_loc,
        });
        if !matches!(lexer.token, Token::Comma | Token::Semicolon) {
            break;
        }
        lexer.next();
    }
    lexer.expect(Token::CloseBrace);
    if lexer.token == Token::Semicolon {
        lexer.next();
    }
    Stmt::new(
        loc,
        StmtData::Enum(EnumStmt {
            values,
            name,
            argument,
            is_export,
        }),
    )
}

fn skip_balanced_group(lexer: &mut Lexer, open: Token, close: Token) {
    lexer.expect(open);
    let mut depth = 1_usize;
    while depth > 0 {
        if lexer.token == open {
            depth += 1;
        } else if lexer.token == close {
            depth -= 1;
        } else if lexer.token == Token::EndOfFile {
            lexer.expected(close);
        }
        lexer.next();
    }
}

fn skip_type_until_statement_end(lexer: &mut Lexer) {
    let mut delimiters = Vec::new();
    let mut has_type_token = false;
    loop {
        if has_type_token
            && delimiters.is_empty()
            && lexer.has_newline_before
            && is_statement_start(lexer)
        {
            return;
        }
        match lexer.token {
            Token::Semicolon if delimiters.is_empty() => {
                lexer.next();
                return;
            }
            Token::EndOfFile if delimiters.is_empty() => return,
            Token::OpenParen => delimiters.push(Token::CloseParen),
            Token::OpenBracket => delimiters.push(Token::CloseBracket),
            Token::OpenBrace => delimiters.push(Token::CloseBrace),
            Token::LessThan => delimiters.push(Token::GreaterThan),
            token if delimiters.last() == Some(&token) => {
                delimiters.pop();
            }
            Token::EndOfFile => {
                lexer.expected(*delimiters.last().expect("delimiter stack is not empty"));
            }
            _ => {}
        }
        has_type_token = true;
        lexer.next();
    }
}

fn is_statement_start(lexer: &Lexer) -> bool {
    matches!(
        lexer.token,
        Token::Var
            | Token::Const
            | Token::Function
            | Token::Class
            | Token::Import
            | Token::Export
            | Token::If
            | Token::For
            | Token::While
            | Token::Do
            | Token::Switch
            | Token::Try
            | Token::Throw
            | Token::Return
    ) || lexer.is_contextual_keyword(b"let")
        || lexer.is_contextual_keyword(b"interface")
        || lexer.is_contextual_keyword(b"type")
        || lexer.is_contextual_keyword(b"async")
}
