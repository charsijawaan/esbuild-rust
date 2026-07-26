#![allow(dead_code)]

use crate::internal::{
    ast::ImportPhase,
    js_ast::{Expr, ExprData, ImportCallExpr, ImportMetaExpr},
    js_lexer::{Lexer, Token},
    logger::Range,
};

use super::parser_core::ParserCore;

pub(crate) fn parse_import_prefix(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    parse_argument: impl FnMut(&mut ParserCore, &mut Lexer) -> Expr,
) -> Option<Expr> {
    if lexer.token != Token::Import {
        return None;
    }
    let loc = lexer.loc();
    lexer.next();
    Some(parse_import_after_keyword(core, lexer, loc, parse_argument))
}

pub(crate) fn parse_import_after_keyword(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    loc: crate::internal::logger::Loc,
    mut parse_argument: impl FnMut(&mut ParserCore, &mut Lexer) -> Expr,
) -> Expr {
    let mut phase = ImportPhase::Evaluation;

    if lexer.token == Token::Dot {
        lexer.next();
        let name = lexer.identifier.string.as_slice();
        if name == b"meta" {
            core.esm_import_meta = Range {
                loc,
                len: lexer.range().end() - loc.start,
            };
            lexer.next();
            return Expr::new(
                loc,
                ExprData::ImportMeta(ImportMetaExpr {
                    range_len: core.esm_import_meta.len,
                }),
            );
        }
        phase = if name == b"defer" {
            ImportPhase::Defer
        } else if name == b"source" {
            ImportPhase::Source
        } else {
            lexer.unexpected();
        };
        lexer.next();
    }

    lexer.expect(Token::OpenParen);
    let expr = parse_argument(core, lexer);
    let mut options_or_nil = Expr::default();
    if lexer.token == Token::Comma {
        lexer.next();
        if lexer.token != Token::CloseParen {
            options_or_nil = parse_argument(core, lexer);
            if lexer.token == Token::Comma {
                lexer.next();
            }
        }
    }
    let close_paren_loc = lexer.loc();
    lexer.expect(Token::CloseParen);
    Expr::new(
        loc,
        ExprData::ImportCall(ImportCallExpr {
            expr,
            options_or_nil,
            close_paren_loc,
            phase,
        }),
    )
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::parse_import_prefix;
    use crate::internal::{
        ast::ImportPhase,
        config::TsOptions,
        js_ast::{Expr, ExprData},
        js_lexer::{Lexer, Token},
        js_parser::Options,
        logger::{DeferLogKind, Log, Source},
    };

    fn number(lexer: &mut Lexer) -> Expr {
        let loc = lexer.loc();
        let value = lexer.number;
        lexer.next();
        Expr::new(loc, ExprData::Number(value))
    }

    #[test]
    fn parses_import_meta() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"import.meta.url"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let expr = parse_import_prefix(&mut core, &mut lexer, |_, lexer| number(lexer))
            .expect("expected import");
        assert!(matches!(
            expr.data.as_deref(),
            Some(ExprData::ImportMeta(_))
        ));
        assert!(core.esm_import_meta.len > 0);
        assert_eq!(lexer.token, Token::Dot);
    }

    #[test]
    fn parses_phased_dynamic_import_with_options_and_trailing_comma() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"import.source(1, 2,)"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(source, Options::default());
        let expr = parse_import_prefix(&mut core, &mut lexer, |_, lexer| number(lexer))
            .expect("expected import");
        let Some(ExprData::ImportCall(import)) = expr.data.as_deref() else {
            panic!("expected import call");
        };
        assert_eq!(import.phase, ImportPhase::Source);
        assert!(import.options_or_nil.data.is_some());
        assert_eq!(lexer.token, Token::EndOfFile);
    }
}
