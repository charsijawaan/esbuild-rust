#![allow(dead_code)]

use crate::internal::{
    compat::JsFeature,
    js_ast::{Expr, ExprData, Precedence, PrivateIdentifierExpr},
    js_lexer::{Lexer, Token},
};

use super::parser_core::ParserCore;

pub(crate) fn parse_private_brand_check_prefix(
    core: &mut ParserCore,
    lexer: &mut Lexer,
    minimum_precedence: Precedence,
    allow_in: bool,
) -> Option<Expr> {
    if lexer.token != Token::PrivateIdentifier {
        return None;
    }
    if !allow_in || minimum_precedence >= Precedence::Compare {
        lexer.unexpected();
    }

    let loc = lexer.loc();
    let name = lexer.identifier.clone();
    lexer.next();
    if lexer.token != Token::In {
        lexer.expected(Token::In);
    }

    if core
        .options
        .unsupported_js_features
        .contains(JsFeature::CLASS_PRIVATE_BRAND_CHECK)
    {
        let text = String::from_utf8(name.string.clone())
            .expect("private identifiers must be valid UTF-8");
        core.lower_all_of_these_private_names.insert(text, true);
    }

    Some(Expr::new(
        loc,
        ExprData::PrivateIdentifier(PrivateIdentifierExpr {
            reference: core.store_name_in_ref(name),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::parse_private_brand_check_prefix;
    use crate::internal::{
        compat::JsFeature,
        config::TsOptions,
        js_ast::{ExprData, Precedence},
        js_lexer::{Lexer, Token},
        js_parser::Options,
        logger::{DeferLogKind, Log, Source},
    };

    #[test]
    fn parses_private_name_before_in_and_marks_it_for_lowering() {
        let log = Log::new_defer(DeferLogKind::All, HashMap::new());
        let source = Source {
            contents: Arc::from(&b"#field in object"[..]),
            ..Source::default()
        };
        let mut lexer = Lexer::new(log, source.clone(), TsOptions::default());
        let mut core = super::ParserCore::new(
            source,
            Options {
                unsupported_js_features: JsFeature::CLASS_PRIVATE_BRAND_CHECK,
                ..Options::default()
            },
        );
        let expr =
            parse_private_brand_check_prefix(&mut core, &mut lexer, Precedence::Lowest, true)
                .expect("expected private brand check");
        assert!(matches!(
            expr.data.as_deref(),
            Some(ExprData::PrivateIdentifier(_))
        ));
        assert_eq!(lexer.token, Token::In);
        assert_eq!(
            core.lower_all_of_these_private_names.get("#field"),
            Some(&true)
        );
    }
}
