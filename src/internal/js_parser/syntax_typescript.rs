use crate::internal::{
    js_ast::{
        Expr, ExprData, ExprStmt, IdentifierExpr, Precedence, Stmt, StmtData, TypeScriptStmt,
    },
    js_lexer::{Lexer, Token},
};

use super::{parser_core::ParserCore, syntax_expression::parse_expression_suffix};

pub(crate) fn skip_type_parameters(lexer: &mut Lexer) {
    if lexer.token != Token::LessThan {
        return;
    }
    let mut depth = 0_usize;
    loop {
        match lexer.token {
            Token::LessThan => depth += 1,
            Token::GreaterThan => {
                depth -= 1;
                lexer.next();
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

pub(crate) fn parse_type_script_statement(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    is_export: bool,
) -> Option<Stmt> {
    if !core.options.ts.parse || lexer.token != Token::Identifier {
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
