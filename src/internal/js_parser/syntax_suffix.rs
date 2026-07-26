#![allow(dead_code)]

use crate::internal::{
    js_ast::{Expr, ExprData, SpreadExpr},
    js_lexer::{Lexer, Token},
    logger::Loc,
};

pub(crate) fn parse_call_args(
    lexer: &mut Lexer,
    mut parse_arg: impl FnMut(&mut Lexer) -> Expr,
) -> (Vec<Expr>, Loc, bool) {
    lexer.expect(Token::OpenParen);
    let mut args = Vec::new();
    let mut is_multi_line = false;

    while lexer.token != Token::CloseParen {
        if lexer.has_newline_before {
            is_multi_line = true;
        }
        let loc = lexer.loc();
        let is_spread = lexer.token == Token::DotDotDot;
        if is_spread {
            lexer.next();
        }
        let mut argument = parse_arg(lexer);
        if is_spread {
            argument = Expr::new(loc, ExprData::Spread(SpreadExpr { value: argument }));
        }
        args.push(argument);
        if lexer.token != Token::Comma {
            break;
        }
        if lexer.has_newline_before {
            is_multi_line = true;
        }
        lexer.next();
    }

    if lexer.has_newline_before {
        is_multi_line = true;
    }
    let close_paren_loc = lexer.loc();
    lexer.expect(Token::CloseParen);
    (args, close_paren_loc, is_multi_line)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::parse_call_args;
    use crate::internal::{
        config::TsOptions,
        js_ast::{Expr, ExprData},
        js_lexer::{Lexer, Token},
        logger::{DeferLogKind, Log, Source},
    };

    #[test]
    fn parses_spread_trailing_and_multiline_call_arguments() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"(\n1, ...2,\n)"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source, TsOptions::default());
        let (args, close, multiline) = parse_call_args(&mut lexer, |lexer| {
            let loc = lexer.loc();
            let value = lexer.number;
            lexer.next();
            Expr::new(loc, ExprData::Number(value))
        });
        assert_eq!(args.len(), 2);
        assert!(matches!(args[1].data.as_deref(), Some(ExprData::Spread(_))));
        assert!(multiline);
        assert!(close.start > 0);
        assert_eq!(lexer.token, Token::EndOfFile);
    }
}
