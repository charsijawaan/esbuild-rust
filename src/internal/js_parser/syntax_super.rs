#![allow(dead_code)]

use crate::internal::{
    js_ast::{Expr, ExprData, Precedence},
    js_lexer::{Lexer, Token},
};

use super::parser_core::ParserCore;

pub(crate) fn parse_super_prefix(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    minimum_precedence: Precedence,
) -> Option<Expr> {
    if lexer.token != Token::Super {
        return None;
    }

    let loc = lexer.loc();
    let range = lexer.range();
    lexer.next();
    let is_allowed = match lexer.token {
        Token::OpenParen => {
            minimum_precedence < Precedence::Call && core.fn_or_arrow_data_parse.allow_super_call
        }
        Token::Dot | Token::OpenBracket => core.fn_or_arrow_data_parse.allow_super_property,
        _ => false,
    };
    if !is_allowed {
        core.add_error_range(range, "Unexpected \"super\"");
    }
    Some(Expr::new(loc, ExprData::Super))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::parse_super_prefix;
    use crate::internal::{
        config::TsOptions,
        js_ast::{ExprData, Precedence},
        js_lexer::{Lexer, Token},
        js_parser::Options,
        logger::{DeferLogKind, Log, Source},
    };

    fn setup(text: &[u8]) -> (Log, Lexer, super::ParserCore) {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(text),
            ..Source::default()
        };
        let lexer = Lexer::new(log.clone(), source.clone(), TsOptions::default());
        let core = super::ParserCore::new_with_log(source, Options::default(), log.clone());
        (log, lexer, core)
    }

    #[test]
    fn allows_super_calls_and_properties_in_their_contexts() {
        for (text, call) in [(&b"super()"[..], true), (&b"super.name"[..], false)] {
            let (log, mut lexer, mut core) = setup(text);
            core.fn_or_arrow_data_parse.allow_super_call = call;
            core.fn_or_arrow_data_parse.allow_super_property = !call;
            let expr =
                parse_super_prefix(&mut core, &mut lexer, Precedence::Lowest).expect("super");
            assert!(matches!(expr.data.as_deref(), Some(ExprData::Super)));
            assert!(log.peek().is_empty());
            assert_eq!(
                lexer.token,
                if call { Token::OpenParen } else { Token::Dot }
            );
        }
    }

    #[test]
    fn reports_super_outside_a_valid_context() {
        let (log, mut lexer, mut core) = setup(b"super.name");
        let _ = parse_super_prefix(&mut core, &mut lexer, Precedence::Lowest).expect("super");
        assert_eq!(log.peek().len(), 1);
    }
}
