#![allow(dead_code)]

use crate::internal::{
    js_ast::{Expr, ExprData, NewExpr, NewTargetExpr},
    js_lexer::{Lexer, Token},
    logger::Range,
};

use super::{parser_core::ParserCore, syntax_suffix::parse_call_args};

pub(crate) fn parse_new_prefix(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    mut parse_target: impl FnMut(&mut ParserCore, &mut Lexer) -> Expr,
    mut parse_argument: impl FnMut(&mut ParserCore, &mut Lexer) -> Expr,
) -> Option<Expr> {
    if lexer.token != Token::New {
        return None;
    }
    let loc = lexer.loc();
    lexer.next();

    if lexer.token == Token::Dot {
        lexer.next();
        if lexer.token != Token::Identifier || lexer.raw() != b"target" {
            lexer.unexpected();
        }
        let range = Range {
            loc,
            len: lexer.range().end() - loc.start,
        };
        lexer.next();
        return Some(Expr::new(loc, ExprData::NewTarget(NewTargetExpr { range })));
    }

    let target = parse_target(core, lexer);
    if core.options.ts.parse {
        super::syntax_typescript::try_skip_type_arguments_in_expression(lexer);
    }
    let (args, close_paren_loc, is_multi_line) = if lexer.token == Token::OpenParen {
        parse_call_args(core, lexer, |core, lexer| parse_argument(core, lexer))
    } else {
        (Vec::new(), crate::internal::logger::Loc::default(), false)
    };
    Some(Expr::new(
        loc,
        ExprData::New(NewExpr {
            target,
            args,
            close_paren_loc,
            is_multi_line,
            ..NewExpr::default()
        }),
    ))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::parse_new_prefix;
    use crate::internal::{
        config::TsOptions,
        js_ast::{Expr, ExprData},
        js_lexer::{Lexer, Token},
        js_parser::Options,
        logger::{DeferLogKind, Log, Source},
    };

    fn simple(core: &mut super::ParserCore, lexer: &mut Lexer) -> Expr {
        if let Some(expr) =
            crate::internal::js_parser::syntax_literals::parse_simple_prefix(core, lexer)
        {
            expr
        } else {
            let loc = lexer.loc();
            let value = lexer.number;
            lexer.next();
            Expr::new(loc, ExprData::Number(value))
        }
    }

    #[test]
    fn parses_constructor_target_and_arguments() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"new Constructor(1)"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let expr = parse_new_prefix(&mut core, &mut lexer, simple, simple).expect("expected new");
        let Some(ExprData::New(new)) = expr.data.as_deref() else {
            panic!("expected new expression");
        };
        assert_eq!(new.args.len(), 1);
        assert_eq!(lexer.token, Token::EndOfFile);
    }

    #[test]
    fn parses_new_target() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"new.target"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let expr =
            parse_new_prefix(&mut core, &mut lexer, simple, simple).expect("expected new target");
        assert!(matches!(expr.data.as_deref(), Some(ExprData::NewTarget(_))));
        assert_eq!(lexer.token, Token::EndOfFile);
    }
}
